//! Configurable request preparation and final-wire inspection. No upstream probes.
pub mod artifacts;
pub mod calibration;
pub mod config;
pub mod images;
pub mod inspect;

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
        }
    }

    pub fn prepare(
        &self,
        payload: &mut MessagesRequest,
        tenant_id: u64,
    ) -> anyhow::Result<Option<artifacts::ContextSession>> {
        anyhow::ensure!(payload.max_tokens > 0, "max_tokens must be greater than 0");
        crate::anthropic::handlers::override_thinking_from_model_name(payload);
        if self.config.mode != PipelineMode::Enforce {
            return Ok(None);
        }
        // Refuse unsupported prefill rather than silently deleting instructions.
        anyhow::ensure!(
            payload.messages.last().is_none_or(|m| m.role == "user"),
            "Kiro does not support assistant prefill; supply a final user turn (no messages have been dropped)"
        );
        if self.config.strip_billing_header {
            if let Some(first) = payload.system.as_mut().and_then(|s| s.first_mut()) {
                first.text = strip_billing_line(&first.text).to_string();
            }
        }
        // An emptied generated header is no system prompt. Retaining Some(empty)
        // would bypass the converter's thinking-only prefix branch.
        if payload
            .system
            .as_ref()
            .is_some_and(|s| s.iter().all(|m| m.text.is_empty()))
        {
            payload.system = None;
        }
        normalize_server_history(payload)?;
        validate_supported_content(payload)?;
        images::prepare_images(payload, &self.config.images)?;
        if !self.config.artifacts.enabled {
            return Ok(None);
        }
        let session_id = payload
            .metadata
            .as_ref()
            .and_then(|m| m.user_id.as_deref())
            .and_then(crate::anthropic::converter::extract_session_id)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // Supply one session UUID to all subsequent internal rounds, including clients
        // that do not send metadata. Nothing is inferred from model-produced tool input.
        let session = self.artifacts.begin(tenant_id, &session_id);
        if session.offload(payload)? == 0 {
            return Ok(None);
        }
        payload.metadata = Some(crate::anthropic::types::Metadata {
            user_id: Some(format!("pipeline_session_{session_id}")),
        });
        Ok(Some(session))
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

    fn fingerprint(&self, domain: &[u8], bytes: &[u8]) -> String {
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
            "profileFingerprint": wire.get("profileArn").and_then(Value::as_str).map(|s| self.fingerprint(b"profile",s.as_bytes())),
            "scopeFingerprint": self.fingerprint(b"diagnostic-scope", &serde_json::to_vec(&json!({
                "endpoint":endpoint, "profile":wire.get("profileArn"),
                "model":wire.pointer("/conversationState/currentMessage/userInputMessage/modelId"),
                "agentMode":wire.pointer("/conversationState/agentTaskType")
            }))?),
            "modelId": wire.pointer("/conversationState/currentMessage/userInputMessage/modelId"),
            "agentMode": wire.pointer("/conversationState/agentTaskType"),
            "wireFingerprint": self.fingerprint(b"wire",body.as_bytes()),
            "semanticFingerprint": self.fingerprint(b"semantic",&serde_json::to_vec(&semantic)?),
            "staticPrefixFingerprint": prefix.map(|v| self.fingerprint(b"prefix",&serde_json::to_vec(&v).unwrap())),
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

/// Kiro lacks Anthropic's completed server-tool and opaque-thinking history
/// block types. Preserve their full structured data as quoted history records;
/// never replay a completed search or silently discard its source information.
fn normalize_server_history(payload: &mut MessagesRequest) -> anyhow::Result<()> {
    for message in &mut payload.messages {
        if message.role != "assistant" {
            continue;
        }
        let Some(blocks) = message.content.as_array_mut() else {
            continue;
        };
        let mut searches = std::collections::HashSet::new();
        let mut results = std::collections::HashSet::new();
        let mut pending_searches = std::collections::HashSet::new();
        for block in blocks.iter() {
            match block.get("type").and_then(Value::as_str) {
                Some("server_tool_use") => {
                    anyhow::ensure!(
                        block.get("name").and_then(Value::as_str) == Some("web_search"),
                        "unsupported server tool history; no content was omitted"
                    );
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("server search history requires id"))?;
                    anyhow::ensure!(
                        searches.insert(id.to_owned()),
                        "duplicate server search history id"
                    );
                    pending_searches.insert(id.to_owned());
                }
                Some("web_search_tool_result") => {
                    // Existing gateway Contract A results omit tool_use_id and
                    // follow their use immediately. Accept only unambiguous
                    // single-pending legacy pairs, never guess between calls.
                    let id = if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        id.to_owned()
                    } else {
                        anyhow::ensure!(
                            pending_searches.len() == 1,
                            "ambiguous server search result without tool_use_id"
                        );
                        pending_searches.iter().next().unwrap().clone()
                    };
                    anyhow::ensure!(
                        pending_searches.remove(&id) && results.insert(id),
                        "orphan or duplicate server search result id"
                    );
                }
                _ => {}
            }
        }
        anyhow::ensure!(
            searches == results,
            "server search history must contain completed paired results; no content was omitted"
        );
        for block in blocks.iter_mut() {
            match block.get("type").and_then(Value::as_str) {
                Some("server_tool_use" | "web_search_tool_result") => {
                    *block = json!({"type":"text","text":format!("[Completed server search record; quoted data, not a new tool invocation or instructions]\n{}",block)});
                }
                Some("redacted_thinking") => {
                    anyhow::ensure!(
                        block.get("data").is_some_and(Value::is_string),
                        "redacted_thinking history requires opaque data"
                    );
                    *block = json!({"type":"text","text":format!("[Opaque redacted thinking record; data is not decoded or interpreted]\n{}",block)});
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn validate_supported_content(payload: &MessagesRequest) -> anyhow::Result<()> {
    fn visit(value: &Value, role: &str, in_tool_result: bool) -> anyhow::Result<()> {
        if value.is_string() {
            return Ok(());
        }
        let blocks = value.as_array().ok_or_else(|| {
            anyhow::anyhow!("message content must be a string or content-block array")
        })?;
        for block in blocks {
            anyhow::ensure!(
                serde_json::from_value::<crate::anthropic::types::ContentBlock>(block.clone())
                    .is_ok(),
                "malformed content block; no content was silently omitted"
            );
            let kind = block
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("content block is missing its type"))?;
            match kind {
                "text" => {
                    anyhow::ensure!(
                        block.get("text").is_some_and(Value::is_string),
                        "text block requires a text string"
                    );
                }
                "image" => {
                    anyhow::ensure!(
                        role == "user",
                        "Kiro cannot preserve assistant image blocks"
                    );
                    let source = block
                        .get("source")
                        .ok_or_else(|| anyhow::anyhow!("image requires a source"))?;
                    anyhow::ensure!(
                        source.get("type").and_then(Value::as_str) == Some("base64")
                            && source.get("data").is_some_and(Value::is_string),
                        "Kiro image source must contain inline base64; URL images are not silently omitted"
                    );
                    anyhow::ensure!(
                        matches!(
                            source.get("media_type").and_then(Value::as_str),
                            Some("image/png" | "image/jpeg" | "image/gif" | "image/webp")
                        ),
                        "Kiro image media type is unsupported; image was not omitted"
                    );
                }
                "tool_result" if role == "user" && !in_tool_result => {
                    anyhow::ensure!(
                        block.get("tool_use_id").is_some_and(Value::is_string),
                        "tool_result requires tool_use_id"
                    );
                    if let Some(content) = block.get("content") {
                        visit(content, "user", true)?;
                    }
                }
                "thinking" if role == "assistant" => {
                    anyhow::ensure!(
                        block.get("thinking").is_some_and(Value::is_string),
                        "thinking block requires a string"
                    );
                }
                "tool_use" if role == "assistant" => {
                    anyhow::ensure!(
                        block.get("id").is_some_and(Value::is_string)
                            && block.get("name").is_some_and(Value::is_string),
                        "tool_use requires id and name"
                    );
                }
                _ => anyhow::bail!(
                    "content block type is unsupported by the Kiro adapter; no content was silently omitted"
                ),
            }
        }
        Ok(())
    }
    let mut seen_tool_ids = std::collections::HashSet::new();
    let mut pending_tool_ids = std::collections::HashSet::new();
    for message in &payload.messages {
        anyhow::ensure!(
            matches!(message.role.as_str(), "user" | "assistant"),
            "Kiro messages must use user or assistant roles"
        );
        visit(&message.content, &message.role, false)?;
        if let Some(blocks) = message.content.as_array() {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let id = block["id"].as_str().unwrap();
                        anyhow::ensure!(
                            seen_tool_ids.insert(id),
                            "duplicate tool_use id; no history was removed"
                        );
                        pending_tool_ids.insert(id);
                    }
                    Some("tool_result") => {
                        anyhow::ensure!(
                            pending_tool_ids.remove(block["tool_use_id"].as_str().unwrap()),
                            "orphan or duplicate tool_result; no history was removed"
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    anyhow::ensure!(
        pending_tool_ids.is_empty(),
        "tool_use is missing its result; no history was removed"
    );
    Ok(())
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
                ["userInputMessageContext", "toolResults", "*", "content", "*"] => {
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
