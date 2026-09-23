//! Configurable request preparation and final-wire inspection. No upstream probes.
pub mod artifacts;
pub mod calibration;
pub mod capture;
pub mod chunked_map;
pub mod config;
pub mod expressible;
pub mod images;
pub mod inspect;
pub mod portable_history;
pub mod tool_catalog;

use crate::anthropic::types::MessagesRequest;
use crate::kiro::model::requests::kiro::KiroRequest;
use config::{CacheStrategy, PipelineConfig, PipelineMode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub struct RequestPipeline {
    pub config: PipelineConfig,
    artifacts: Arc<artifacts::ArtifactStore>,
    fingerprint_key: [u8; 32],
    epoch: String,
    /// 入站请求抓取（默认关）。排查"这个请求为什么被改/被拒"时打开。
    capture: capture::CaptureStore,
}

pub struct PrepareOutcome {
    pub context: Option<artifacts::ContextSession>,
    pub normalization: portable_history::NormalizationReport,
}

#[derive(Debug, thiserror::Error)]
pub enum PipelinePrepareError {
    #[error("{}: {}", .0.code(), .0.safe_message())]
    Portable(#[from] portable_history::PortableHistoryError),
    #[error("pipeline preparation failed")]
    Other(#[from] anyhow::Error),
}

impl PipelinePrepareError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Portable(error) => error.code(),
            Self::Other(_) => "pipeline_preparation",
        }
    }

    pub fn safe_message(&self) -> String {
        match self {
            Self::Portable(error) => error.safe_message(),
            Self::Other(_) => "pipeline preparation failed".into(),
        }
    }

    pub fn status(&self) -> http::StatusCode {
        match self {
            Self::Portable(portable_history::PortableHistoryError::BudgetExceeded { .. }) => {
                http::StatusCode::PAYLOAD_TOO_LARGE
            }
            Self::Portable(portable_history::PortableHistoryError::InvariantViolation {
                ..
            }) => http::StatusCode::INTERNAL_SERVER_ERROR,
            _ => http::StatusCode::BAD_REQUEST,
        }
    }
}

impl portable_history::SensitiveFingerprint for RequestPipeline {
    fn fingerprint(&self, domain: &[u8], bytes: &[u8]) -> String {
        self.keyed_fingerprint(domain, bytes)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireMetrics {
    pub body_bytes: usize,
    pub text_bytes: usize,
    pub largest_text_bytes: usize,
    pub largest_text_wire_bytes: usize,
    pub tool_result_count: usize,
    /// 所有工具结果的 text 条目总数。等于 `tool_result_count` 表示每个结果都是单
    /// 条目（默认形状）；大于它说明无损分片已生效，可据此在发送前肉眼验收线上形状。
    pub tool_result_entry_count: usize,
    pub largest_tool_result_bytes: usize,
    pub image_count: usize,
    pub image_base64_bytes: usize,
    pub largest_image_base64_bytes: usize,
    pub cache_point_count: usize,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "local_payload_limit: {0}. This is a configured gateway byte limit, not a measured Kiro context limit; no request was sent and no content was truncated"
)]
pub struct LocalPayloadLimit(pub String);

/// 发送前按声明上限拒绝。
///
/// 与 [`LocalPayloadLimit`] 分开成两个类型，因为两者的判据性质不同：字节预算是运维
/// 自己配置的策略，而这里比的是**本地估算**与**上游声明的上限**。错误文案必须把
/// 「估算」说清楚——它可能拒掉上游本来会接受的请求。
#[derive(Debug, thiserror::Error)]
#[error(
    "local_token_admission: estimated {estimated} input tokens exceed the model's declared maxInputTokens {ceiling}. The estimate is a local heuristic, not the upstream's own count, so this can refuse a request the upstream would have accepted; no request was sent and no content was truncated"
)]
pub struct LocalTokenAdmission {
    pub estimated: u64,
    pub ceiling: i64,
}

impl RequestPipeline {
    pub fn new(config: PipelineConfig) -> Self {
        let mut fingerprint_key = [0u8; 32];
        fingerprint_key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        fingerprint_key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self {
            artifacts: Arc::new(artifacts::ArtifactStore::new(config.artifacts.clone())),
            config,
            fingerprint_key,
            epoch: uuid::Uuid::new_v4().to_string(),
            capture: capture::CaptureStore::new(),
        }
    }

    /// 抓包缓冲，供管理端读取与清空。
    pub fn capture(&self) -> &capture::CaptureStore {
        &self.capture
    }

    pub fn prepare(
        &self,
        payload: &mut MessagesRequest,
        tenant_id: u64,
    ) -> Result<PrepareOutcome, PipelinePrepareError> {
        if payload.max_tokens <= 0 {
            return Err(anyhow::anyhow!("max_tokens must be greater than 0").into());
        }
        let mut candidate = payload.clone();
        crate::anthropic::handlers::override_thinking_from_model_name(&mut candidate);
        let normalized = portable_history::normalize(&candidate, self.config.unexpressible, self)?;
        let normalized_bytes = serde_json::to_vec(&normalized.payload)
            .map_err(anyhow::Error::from)?
            .len();
        if normalized_bytes > self.config.ingress_max_bytes {
            return Err(portable_history::PortableHistoryError::BudgetExceeded {
                actual: normalized_bytes,
                limit: self.config.ingress_max_bytes,
            }
            .into());
        }
        portable_history::validate_tool_pairing(&normalized.payload)?;
        candidate = normalized.payload;
        let normalization = normalized.report;
        let mut context = None;

        if self.config.mode == PipelineMode::Enforce {
            if self.config.strip_billing_header {
                if let Some(first) = candidate.system.as_mut().and_then(|s| s.first_mut()) {
                    first.text = strip_billing_line(&first.text).to_string();
                }
            }
            // An empty generated header must not bypass the converter's thinking prefix.
            if candidate
                .system
                .as_ref()
                .is_some_and(|s| s.iter().all(|m| m.text.is_empty()))
            {
                candidate.system = None;
            }
            images::prepare_images(&mut candidate, &self.config.images)?;
            if self.config.artifacts.enabled {
                let session_id = candidate
                    .metadata
                    .as_ref()
                    .and_then(|m| m.user_id.as_deref())
                    .and_then(crate::anthropic::converter::extract_session_id)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let session = self.artifacts.begin(tenant_id, &session_id);
                if session.offload(&mut candidate)? > 0 {
                    candidate.metadata = Some(crate::anthropic::types::Metadata {
                        user_id: Some(format!("pipeline_session_{session_id}")),
                    });
                    context = Some(session);
                }
            }
        }
        // Commit only after every enabled preparation step has succeeded.
        *payload = candidate;
        if normalization.transformed_blocks > 0 {
            tracing::warn!(
                strategy = normalization.strategy.as_str(),
                scanned_blocks = normalization.scanned_blocks,
                transformed_blocks = normalization.transformed_blocks,
                opaque_bytes = normalization.opaque_bytes,
                "portable history transformed"
            );
        }
        Ok(PrepareOutcome {
            context,
            normalization,
        })
    }

    /// 发送前的 token 准入检查。
    ///
    /// `max_input_tokens` 必须来自**上游声明**（凭据缓存里的模型列表）。上限未知时
    /// 不拦截：未知不是无限，也不是零，它只是「没有依据去拦」。写死的窗口表不是上限，
    /// 不得传进来。
    ///
    /// 策略关闭（默认）时本函数永不拒绝，行为与改造前完全一致。
    pub fn admit(&self, body: &str, max_input_tokens: Option<i64>) -> anyhow::Result<()> {
        if self.config.admission != config::AdmissionStrategy::DeclaredCeiling {
            return Ok(());
        }
        let Some(ceiling) = max_input_tokens.filter(|value| *value > 0) else {
            return Ok(());
        };
        let estimated = measure_wire_tokens(body)?.total;
        anyhow::ensure!(
            estimated <= ceiling as u64,
            LocalTokenAdmission { estimated, ceiling }
        );
        Ok(())
    }

    pub fn preflight(&self, body: &str) -> anyhow::Result<WireMetrics> {
        self.preflight_with_config(body, &self.config)
    }

    pub fn preflight_before_endpoint(&self, body: &str) -> anyhow::Result<WireMetrics> {
        // CLI removes fields; an intermediate body can exceed a limit even when
        // the actual body fits. Only endpoint-invariant field budgets apply here.
        let mut config = self.config.clone();
        config.limits.body_bytes = None;
        self.preflight_with_config(body, &config)
    }

    fn preflight_with_config(
        &self,
        body: &str,
        config: &PipelineConfig,
    ) -> anyhow::Result<WireMetrics> {
        let metrics = measure_wire(body)?;
        let violations = violations(&metrics, config);
        if self.config.mode == PipelineMode::Enforce && !violations.is_empty() {
            return Err(LocalPayloadLimit(violations.join("; ")).into());
        }
        Ok(metrics)
    }

    fn keyed_fingerprint(&self, domain: &[u8], bytes: &[u8]) -> String {
        // HMAC-SHA256 with a per-process secret. No raw prompt or profile hashes that
        // could be tested with an offline dictionary. Epoch bounds valid comparisons.
        let mut inner_pad = [0x36u8; 64];
        let mut outer_pad = [0x5cu8; 64];
        for (i, key) in self.fingerprint_key.iter().enumerate() {
            inner_pad[i] ^= key;
            outer_pad[i] ^= key;
        }
        let mut inner = Sha256::new();
        inner.update(inner_pad);
        inner.update((domain.len() as u64).to_be_bytes());
        inner.update(domain);
        inner.update(bytes);
        let mut outer = Sha256::new();
        outer.update(outer_pad);
        outer.update(inner.finalize());
        hex::encode(outer.finalize())
    }

    /// 构造最终线上证据。
    ///
    /// `max_input_tokens` 是该模型声明的输入上限；调用方拿不到时传 `None`，报告如实
    /// 记为未知。审计发生在本地预算检查与 HTTP 发送**之前**，因此它证明的是构造，
    /// 不是已经发出。
    pub fn audit(
        &self,
        body: &str,
        endpoint: &str,
        credential_id: u64,
        headers: &http::HeaderMap,
        max_input_tokens: Option<i64>,
    ) -> anyhow::Result<Value> {
        let metrics = measure_wire(body)?;
        let token_metrics = measure_wire_tokens(body)?.with_ceiling(max_input_tokens);
        let wire: Value = serde_json::from_str(body)?;
        let mut semantic = wire.clone();
        if let Some(state) = semantic
            .get_mut("conversationState")
            .and_then(Value::as_object_mut)
        {
            state.remove("conversationId");
            state.remove("agentContinuationId");
        }
        // Exact marked prefix, not all history and not a hash of HTTP headers.
        let history = wire
            .pointer("/conversationState/history")
            .and_then(Value::as_array);
        let last_mark = history.and_then(|h| {
            h.iter()
                .rposition(|v| v.pointer("/userInputMessage/cachePoint").is_some())
        });
        let prefix = last_mark.map(|i| json!({
            "history": &history.unwrap()[..=i],
            "tools": wire.pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools"),
            "modelId": wire.pointer("/conversationState/currentMessage/userInputMessage/modelId"),
            "agentTaskType": wire.pointer("/conversationState/agentTaskType"),
            "additionalModelRequestFields": wire.get("additionalModelRequestFields")
        }));
        let mut header_names: Vec<_> = headers.keys().map(|k| k.as_str()).collect();
        header_names.sort_unstable();
        let header_bytes: usize = headers
            .iter()
            .map(|(k, v)| k.as_str().len() + v.as_bytes().len() + 4)
            .sum();
        let config_bytes = serde_json::to_vec(&self.config)?;
        Ok(json!({
            "schemaVersion": 1, "fingerprintEpoch": self.epoch,
            "configFingerprint": hex::encode(Sha256::digest(&config_bytes)),
            "mode": self.config.mode, "cacheStrategy": self.config.cache_strategy,
            "endpoint": endpoint, "credentialId": credential_id,
            "profileFingerprint": wire.get("profileArn").and_then(Value::as_str).map(|s| self.keyed_fingerprint(b"profile",s.as_bytes())),
            "scopeFingerprint": self.keyed_fingerprint(b"diagnostic-scope", &serde_json::to_vec(&json!({
                "endpoint":endpoint, "profile":wire.get("profileArn"),
                "model":wire.pointer("/conversationState/currentMessage/userInputMessage/modelId"),
                "agentMode":wire.pointer("/conversationState/agentTaskType")
            }))?),
            "modelId": wire.pointer("/conversationState/currentMessage/userInputMessage/modelId"),
            "agentMode": wire.pointer("/conversationState/agentTaskType"),
            "wireFingerprint": self.keyed_fingerprint(b"wire",body.as_bytes()),
            "semanticFingerprint": self.keyed_fingerprint(b"semantic",&serde_json::to_vec(&semantic)?),
            "staticPrefixFingerprint": prefix.map(|v| self.keyed_fingerprint(b"prefix",&serde_json::to_vec(&v).unwrap())),
            "metrics": metrics, "violations": violations(&metrics,&self.config),
            // token 与字节是两个并列口径，互不替代。`source` 固定为 estimate：
            // 这些数字永远不是原生 tokenUsage，不构成缓存或计费证据。
            "tokenMetrics": {
                "source": "estimate",
                "total": token_metrics.total,
                "current": token_metrics.current,
                "history": token_metrics.history,
                "tools": token_metrics.tools,
                "toolResults": token_metrics.tool_results,
                "images": token_metrics.images,
                "other": token_metrics.other,
                "maxInputTokens": token_metrics.max_input_tokens,
                "headroom": token_metrics.headroom
            },
            "headerNames": header_names, "headerBytes": header_bytes,
            "cacheHitProven": false, "evidenceType": "construction-only"
        }))
    }
}

/// Only this known client-generated leading system line is removed. Quoted,
/// indented or later occurrences are user content and remain byte-for-byte intact.
fn strip_billing_line(text: &str) -> &str {
    let first_end = text.find('\n').unwrap_or(text.len());
    let first = &text[..first_end];
    if first
        .get(..27)
        .is_some_and(|p| p.eq_ignore_ascii_case("x-anthropic-billing-header:"))
    {
        &text[(first_end + usize::from(first_end < text.len()))..]
    } else {
        text
    }
}

/// 为「一次修改后重试」构造修正后的请求体。
///
/// 在稳态配置之上**仅**强制启用工具结果无损分片，其余一律不变——这是本项目目前唯一
/// 的无损修正手段。正常请求的形状完全不受影响：修正只作用于这一次重试。
///
/// 返回 `None` 表示不该重试：策略关闭、转换/序列化失败，或者**修正后字节毫无变化**。
/// 最后一条是硬约束——原样重发一个刚被拒绝的 payload 只是在碰运气，本项目不做。
pub fn recovery_body(
    payload: &MessagesRequest,
    tool_compatibility_mode: crate::model::config::ToolCompatibilityMode,
    config: &PipelineConfig,
    original_body: &str,
) -> Option<String> {
    if config.recovery != config::RecoveryStrategy::LosslessRetry {
        return None;
    }
    let mut corrected = config.clone();
    corrected.tool_results.strategy = config::ToolResultStrategy::LosslessChunks;
    let converted = crate::anthropic::converter::convert_request_with_pipeline(
        payload,
        tool_compatibility_mode,
        &corrected,
    )
    .ok()?;
    let request = KiroRequest {
        conversation_state: converted.conversation_state,
        profile_arn: None,
        additional_model_request_fields: converted.additional_model_request_fields,
    };
    let body = serialize_request(payload, &request, &corrected).ok()?;
    // 必须比较**语义**而非整串：`conversationId` / `agentContinuationId` 每次转换都会
    // 重新生成，整串比较两次必然不同，"没变化就不重发"的约束会形同虚设——守卫永远
    // 不触发，于是每个长度拒绝都会被原样重发一次。与 audit 的 semanticFingerprint 同源。
    // 任一侧无法解析时保守地不重发。
    (semantic_key(&body)? != semantic_key(original_body)?).then_some(body)
}

/// 去掉每次转换都会重新生成的会话标识，只留语义部分用于比较。
fn semantic_key(body: &str) -> Option<String> {
    let mut value: Value = serde_json::from_str(body).ok()?;
    if let Some(state) = value
        .get_mut("conversationState")
        .and_then(Value::as_object_mut)
    {
        state.remove("conversationId");
        state.remove("agentContinuationId");
    }
    serde_json::to_string(&value).ok()
}

/// 按字节上限把正文切成多个分片，切点落在 UTF-8 字符边界上。
///
/// **字节完全保留**：按序拼接所有分片必须逐字节还原原文——不插入分隔符、不丢弃、
/// 不重排、不重新编码。这与 artifact 卸载是两回事：卸载要模型主动来读，分片则是
/// 把全文一次性发出去，只是换了个线上形状。
///
/// 单个字符本身超过上限时整体成为一个分片：宁可该分片超限，也不切断字符产生非法
/// UTF-8。调用方据此不能假设每片都 ≤ 上限。
pub fn split_lossless(text: &str, max_bytes: usize) -> Vec<&str> {
    if max_bytes == 0 || text.len() <= max_bytes {
        return vec![text];
    }
    let mut parts = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let remaining = &text[start..];
        if remaining.len() <= max_bytes {
            parts.push(remaining);
            break;
        }
        // 从上限处向前退到最近的字符边界。
        let mut cut = max_bytes;
        while cut > 0 && !remaining.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut == 0 {
            // 首字符本身就超过上限。
            cut = remaining
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(remaining.len());
        }
        parts.push(&remaining[..cut]);
        start += cut;
    }
    parts
}

pub fn serialize_request(
    payload: &MessagesRequest,
    request: &KiroRequest,
    config: &PipelineConfig,
) -> anyhow::Result<String> {
    if config.mode != PipelineMode::Enforce {
        return Ok(serde_json::to_string(request)?);
    }
    let mut wire = serde_json::to_value(request)?;
    wire["conversationState"]["agentTaskType"] = json!(config.agent_mode);
    // One stable system boundary. Never mark the growing current message or silently
    // equate Anthropic cache_control/TTL to undocumented Kiro behavior.
    if config.cache_strategy == CacheStrategy::StaticPrefix
        && payload
            .system
            .as_ref()
            .is_some_and(|s| s.iter().any(|m| !m.text.is_empty()))
    {
        if let Some(system) = wire.pointer_mut("/conversationState/history/0/userInputMessage") {
            system["cachePoint"] = json!({"type":"default"});
        }
    }
    Ok(serde_json::to_string(&wire)?)
}

fn violations(m: &WireMetrics, c: &PipelineConfig) -> Vec<String> {
    [
        ("bodyBytes", m.body_bytes, c.limits.body_bytes),
        (
            "textFieldBytes",
            m.largest_text_bytes,
            c.limits.text_field_bytes,
        ),
        (
            "toolResultBytes",
            m.largest_tool_result_bytes,
            c.limits.tool_result_bytes,
        ),
        (
            "imageBase64Bytes",
            m.largest_image_base64_bytes,
            c.limits.image_base64_bytes,
        ),
    ]
    .into_iter()
    .filter_map(|(name, actual, limit)| {
        limit
            .filter(|limit| actual > *limit)
            .map(|limit| format!("{name}={actual} exceeds {limit}"))
    })
    .collect()
}

/// 最终线上请求的 token 分项。
///
/// 与 [`WireMetrics`] 的字节口径**并列而非替代**：字节预算不是 token 预算，两者都不能
/// 互相推导。本结构全部为**估算**（见 [`crate::token::count_tokens`]），不是原生
/// `metadataEvent.tokenUsage`，也不会被当作缓存或计费证据。
///
/// 分项互不重叠：`tools` / `toolResults` / `images` 虽然嵌在 `currentMessage` 或
/// `history` 之下，但归属会在进入这些子树时改判，因此 `total` 恰等于各分项之和。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireTokenMetrics {
    pub total: u64,
    /// 当前这一轮的用户消息文本（不含其下的工具/结果/图片）。
    pub current: u64,
    /// 历史消息文本（不含其下的工具/结果/图片）。
    pub history: u64,
    /// 工具声明（名称、描述、schema）。
    pub tools: u64,
    /// 工具结果正文。agentic 会话里通常是最大的一项。
    pub tool_results: u64,
    /// 图片，按共享估算器的 `(w×h)/750` 口径，不按 base64 串长度。
    pub images: u64,
    /// 未归入上述分项的线上字段。
    pub other: u64,
    /// 模型声明的输入上限。未知即为 `None`——不猜测、不按模型名推断、
    /// 不从上游拒绝反推。本阶段只展示，不参与准入。
    pub max_input_tokens: Option<i64>,
    /// `max_input_tokens - total`。上限未知时为 `None`；可为负值表示已越界，
    /// 不夹到 0，否则会掩盖越界的程度。
    pub headroom: Option<i64>,
}

/// 线上 JSON 的分项归属。
#[derive(Clone, Copy, PartialEq, Eq)]
enum WireSection {
    Current,
    History,
    Tools,
    ToolResults,
    Images,
    Other,
}

impl WireTokenMetrics {
    fn add(&mut self, section: WireSection, tokens: u64) {
        let slot = match section {
            WireSection::Current => &mut self.current,
            WireSection::History => &mut self.history,
            WireSection::Tools => &mut self.tools,
            WireSection::ToolResults => &mut self.tool_results,
            WireSection::Images => &mut self.images,
            WireSection::Other => &mut self.other,
        };
        *slot = slot.saturating_add(tokens);
        self.total = self.total.saturating_add(tokens);
    }

    fn with_ceiling(mut self, max_input_tokens: Option<i64>) -> Self {
        self.max_input_tokens = max_input_tokens;
        self.headroom = max_input_tokens.map(|limit| limit - self.total as i64);
        self
    }
}

/// 统计最终线上请求的 token 分项。
///
/// 描述的是**实际发送的形状**（endpoint 转换之后），不是入站的 Anthropic 请求。
pub fn measure_wire_tokens(body: &str) -> anyhow::Result<WireTokenMetrics> {
    let value: Value = serde_json::from_str(body)?;
    let mut m = WireTokenMetrics::default();

    fn visit(value: &Value, key: Option<&str>, section: WireSection, m: &mut WireTokenMetrics) {
        // 进入被识别的子树时改判归属，从而保证分项互不重叠。
        let section = match key {
            Some("history") => WireSection::History,
            Some("currentMessage") => WireSection::Current,
            Some("tools") => WireSection::Tools,
            Some("toolResults") => WireSection::ToolResults,
            Some("images") => WireSection::Images,
            _ => section,
        };

        // 图片整体按估算器计一次，不下探——否则 base64 会被当作文本严重高估。
        if section == WireSection::Images
            && let Some(data) = value.pointer("/source/bytes").and_then(Value::as_str)
        {
            let format = value.get("format").and_then(Value::as_str).unwrap_or("png");
            let media_type = format!("image/{format}");
            m.add(
                WireSection::Images,
                crate::image_resize::estimate_image_tokens(&media_type, data) as u64,
            );
            return;
        }

        match value {
            Value::Object(object) => {
                for (k, v) in object {
                    visit(v, Some(k), section, m);
                }
            }
            Value::Array(items) => {
                for v in items {
                    visit(v, None, section, m);
                }
            }
            Value::String(text) => m.add(section, crate::token::count_tokens(text)),
            _ => {}
        }
    }

    visit(&value, None, WireSection::Other, &mut m);
    Ok(m)
}

pub fn measure_wire(body: &str) -> anyhow::Result<WireMetrics> {
    let value: Value = serde_json::from_str(body)?;
    let mut m = WireMetrics {
        body_bytes: body.len(),
        ..Default::default()
    };
    fn user_tail<'a>(path: &'a [&'a str]) -> Option<&'a [&'a str]> {
        match path {
            [
                "conversationState",
                "currentMessage",
                "userInputMessage",
                tail @ ..,
            ]
            | [
                "conversationState",
                "history",
                "*",
                "userInputMessage",
                tail @ ..,
            ] => Some(tail),
            _ => None,
        }
    }
    fn visit<'a>(value: &'a Value, path: &mut Vec<&'a str>, m: &mut WireMetrics) {
        if let Some(tail) = user_tail(path) {
            match tail {
                ["images", "*", "source", "bytes"] if value.is_string() => {
                    let s = value.as_str().unwrap();
                    m.image_count += 1;
                    m.image_base64_bytes += s.len();
                    m.largest_image_base64_bytes = m.largest_image_base64_bytes.max(s.len());
                    return;
                }
                ["userInputMessageContext", "toolResults", "*"] => {
                    m.tool_result_count += 1;
                    m.largest_tool_result_bytes =
                        m.largest_tool_result_bytes.max(value.to_string().len());
                }
                [
                    "userInputMessageContext",
                    "toolResults",
                    "*",
                    "content",
                    "*",
                ] => {
                    m.tool_result_entry_count += 1;
                }
                ["cachePoint"] | ["userInputMessageContext", "tools", "*", "cachePoint"]
                    if value.is_object() =>
                {
                    m.cache_point_count += 1;
                }
                _ => {}
            }
        }
        match value {
            Value::Object(object) => {
                for (k, v) in object {
                    path.push(k);
                    visit(v, path, m);
                    path.pop();
                }
            }
            Value::Array(a) => {
                for v in a {
                    path.push("*");
                    visit(v, path, m);
                    path.pop();
                }
            }
            Value::String(s) => {
                m.text_bytes += s.len();
                m.largest_text_bytes = m.largest_text_bytes.max(s.len());
                m.largest_text_wire_bytes = m
                    .largest_text_wire_bytes
                    .max(serde_json::to_string(s).unwrap().len());
            }
            _ => {}
        }
    }
    visit(&value, &mut Vec::new(), &mut m);
    Ok(m)
}

#[cfg(test)]
mod tests;
