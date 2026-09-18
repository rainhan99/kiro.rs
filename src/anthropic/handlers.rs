//! Anthropic API Handler 函数

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Instant;

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::{
    SharedTraceStore, TraceAttempt, TraceKeySource, TraceRecord, TraceRoute, TraceSink, outcome,
    usage_source,
};
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder, UsageRecord};
use crate::kiro::model::available_models::{TokenLimits, UpstreamModel};
use crate::kiro::model::events::{Event, TokenUsage};
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::token_manager::ModelDiscoveryError;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::{Extension, State},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use chrono::Utc;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{ConversionError, convert_request_with_pipeline, get_context_window_size};
use super::middleware::{AppState, KeyContext};
use super::stream::{BufferedStreamContext, SseEvent, StreamContext};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking,
};
use super::websearch;

/// 请求结束时记录用量的钩子
///
/// 在 handler 入口构造，调用 [`Self::record`] 时把当次请求的 input/output token、
/// 命中的上游凭据 ID、状态写入：
/// - `usage_log.YYYY-MM-DD.jsonl`（持久化历史）
/// - 内存聚合器（仪表盘趋势）
/// - 客户端 Key 计数（按 Key 累计）
#[derive(Clone)]
pub(crate) struct UsageRecordHook {
    pub recorder: Option<SharedRecorder>,
    pub aggregator: Option<SharedAggregator>,
    pub client_keys: Option<SharedClientKeyManager>,
    pub key_id: u64,
    pub model: String,
    pub started_at: Instant,
}

impl UsageRecordHook {
    pub fn from_state(state: &AppState, key_id: u64, model: String) -> Self {
        Self {
            recorder: state.usage_recorder.clone(),
            aggregator: state.usage_aggregator.clone(),
            client_keys: state.client_keys.clone(),
            key_id,
            model,
            started_at: Instant::now(),
        }
    }

    pub fn record(
        &self,
        credential_id: u64,
        input_tokens: i32,
        output_tokens: i32,
        cache_creation_tokens: i32,
        cache_read_tokens: i32,
        credits: f64,
        status: &str,
    ) {
        let rec = UsageRecord {
            ts: Utc::now().to_rfc3339(),
            key_id: self.key_id,
            credential_id,
            model: self.model.clone(),
            input_tokens: input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            cache_creation_tokens: cache_creation_tokens.max(0) as u64,
            cache_read_tokens: cache_read_tokens.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            status: status.to_string(),
        };
        if let Some(r) = &self.recorder {
            r.record(&rec);
        }
        if let Some(a) = &self.aggregator {
            a.ingest(&rec);
        }
        if status == "success" && self.key_id != 0 {
            if let Some(m) = &self.client_keys {
                m.record_usage(
                    self.key_id,
                    rec.input_tokens,
                    rec.output_tokens,
                    rec.cache_creation_tokens,
                    rec.cache_read_tokens,
                    rec.credits,
                );
            }
        }
    }
}

/// 单次请求的链路追踪器
///
/// 在 handler 入口构造，作为 [`TraceSink`] 传入 provider；provider 在重试循环里
/// 每跳调用 [`on_attempt`](TraceSink::on_attempt) 累积一条 [`TraceAttempt`]。
/// 请求结束时调用 [`Self::finalize`] 组装 [`TraceRecord`] 并写入 SQLite。
///
/// `store` 为 None（未启用 Admin / trace）时所有方法都是空操作，零开销。
pub(crate) struct RequestTracer {
    pipeline_evidence: parking_lot::Mutex<Vec<(&'static str, serde_json::Value)>>,
    store: Option<SharedTraceStore>,
    trace_id: String,
    ts: String,
    key_id: u64,
    key_source: TraceKeySource,
    client_ip: Option<String>,
    model: String,
    is_stream: bool,
    started_at: Instant,
    /// 首个上游 chunk 到达时刻（仅流式标记；取第一次）
    first_token_at: parking_lot::Mutex<Option<Instant>>,
    attempts: parking_lot::Mutex<Vec<TraceAttempt>>,
    /// 首次选号的路由决策。web_search 一条 trace 内多次 provider 调用，
    /// 「是否沿用了上一轮账号」只看第一次。
    route: parking_lot::Mutex<Option<TraceRoute>>,
}

/// usage 三项的来源，落到 trace 行便于区分「上游真值」与「本地估算」。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum UsageSource {
    /// 错误早退等无用量场景
    #[default]
    Unknown,
    /// 上游 metadataEvent.tokenUsage
    Provider,
    /// 本地 CacheMeter 按断点估算
    Simulated,
    /// 无断点 / 计量关闭
    None,
}

impl UsageSource {
    fn as_db(self) -> Option<&'static str> {
        match self {
            Self::Unknown => Option::None,
            Self::Provider => Some(usage_source::PROVIDER),
            Self::Simulated => Some(usage_source::SIMULATED),
            Self::None => Some(usage_source::NONE),
        }
    }

    /// 由「上游是否给了精确用量」与「本地模拟是否覆盖到前缀」推断来源。
    pub fn resolve(
        has_provider_usage: bool,
        cache_usage: &super::cache_metering::CacheUsage,
    ) -> Self {
        if has_provider_usage {
            Self::Provider
        } else if cache_usage.cache_covered_est > 0 {
            Self::Simulated
        } else {
            Self::None
        }
    }
}

/// 本次请求的用量快照（落入 trace 行，与 usage_log 同源）
#[derive(Clone, Copy, Default)]
pub(crate) struct TraceUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub credits: f64,
    pub source: UsageSource,
}

impl TraceUsage {
    /// 错误早退等无用量场景
    pub fn zero() -> Self {
        Self::default()
    }
}

struct RequestTraceOptions {
    key_ctx: KeyContext,
    model: String,
    is_stream: bool,
}

impl RequestTracer {
    fn new(state: &AppState, options: RequestTraceOptions) -> Self {
        Self {
            pipeline_evidence: parking_lot::Mutex::new(Vec::new()),
            store: state.trace_store.clone(),
            trace_id: Uuid::new_v4().to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: options.key_ctx.key_id,
            key_source: options.key_ctx.key_source,
            client_ip: options.key_ctx.client_ip,
            model: options.model,
            is_stream: options.is_stream,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            attempts: parking_lot::Mutex::new(Vec::new()),
            route: parking_lot::Mutex::new(None),
        }
    }

    /// 标记首个上游 chunk 到达（幂等，仅记录第一次）
    pub fn mark_first_token(&self) {
        if !self.is_stream {
            return;
        }
        let mut slot = self.first_token_at.lock();
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
    }

    /// 组装并落库一条完整链路。store 为 None 时不做任何事。
    pub fn finalize(
        &self,
        final_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
        interrupted_after_bytes: Option<u64>,
        usage: TraceUsage,
    ) {
        let Some(store) = &self.store else { return };
        let attempts = std::mem::take(&mut *self.attempts.lock());
        // 最终凭据：最后一跳的命中凭据（成功跳即命中凭据，失败跳即最后尝试的凭据）
        let final_credential_id = attempts.last().map(|a| a.credential_id).unwrap_or(0);
        let final_endpoint = attempts
            .last()
            .map(|a| a.endpoint.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let first_token_ms = self
            .first_token_at
            .lock()
            .map(|t| t.duration_since(self.started_at).as_millis() as u64);
        let route = self.route.lock().take();
        let rec = TraceRecord {
            trace_id: self.trace_id.clone(),
            ts: self.ts.clone(),
            key_id: self.key_id,
            key_source: self.key_source,
            model: self.model.clone(),
            is_stream: self.is_stream,
            final_status: final_status.to_string(),
            final_credential_id,
            error_type: error_type.map(|s| s.to_string()),
            error_message: error_message.map(|s| s.to_string()),
            total_attempts: attempts.len() as u32,
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            interrupted_after_bytes,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            credits: usage.credits,
            first_token_ms,
            session_id: route.as_ref().and_then(|r| r.session_id.clone()),
            sticky_outcome: route.as_ref().map(|r| r.sticky_outcome.to_string()),
            previous_credential_id: route.as_ref().and_then(|r| r.previous_credential_id),
            usage_source: usage.source.as_db().map(|s| s.to_string()),
            client_ip: self.client_ip.clone(),
            attempts,
        };
        store.insert(&rec);
        let mut evidence = self.pipeline_evidence.lock();
        // Direct one-round handlers already resolve these four fields exclusively
        // from metadataEvent. Internal loops report individual rounds themselves.
        if usage.source == UsageSource::Provider
            && !evidence.iter().any(|(kind, _)| *kind == "native_usage")
        {
            evidence.push((
                "native_usage",
                json!({
                    "uncachedInputTokens": usage.input_tokens,
                    "outputTokens": usage.output_tokens,
                    "cacheReadInputTokens": usage.cache_read_tokens,
                    "cacheWriteInputTokens": usage.cache_creation_tokens
                }),
            ));
        }
        // 被动分母校准：只有当本次响应同时给出了完整的原生用量与一个可用的上下文
        // 百分比时才产出样本。样本来自本来就会发生的请求，没有任何探测流量。
        // 校准只观察：它不改配置，也不会自己流入准入判定。
        if let Some(sample) = crate::pipeline::calibration::derive_sample(
            &self.model,
            &final_endpoint,
            evidence.iter().find_map(|(kind, value)| {
                (*kind == "context_observation")
                    .then(|| value.get("percentage").and_then(serde_json::Value::as_f64))
                    .flatten()
            }),
            // 估算值不能用来反推上游算术；只认 metadataEvent 的真值。
            (usage.source == UsageSource::Provider).then(|| {
                usage.input_tokens + usage.cache_read_tokens + usage.cache_creation_tokens
            }),
            final_status == "success",
        ) && let Err(error) = store.record_calibration_sample(&sample)
        {
            tracing::warn!(%error, "could not persist passive calibration sample");
        }
        for (kind, value) in evidence.drain(..) {
            if let Err(error) = store.record_pipeline_evidence(&self.trace_id, kind, &value) {
                tracing::warn!(%error, "could not persist redacted pipeline evidence");
            }
        }
    }
}

impl TraceSink for RequestTracer {
    fn on_wire_audit(&self, audit: serde_json::Value) {
        let mut evidence = self.pipeline_evidence.lock();
        if evidence.len() < 255 {
            evidence.push(("wire_audit", audit));
        }
    }
    fn on_native_usage(&self, usage: TokenUsage) {
        let mut evidence = self.pipeline_evidence.lock();
        if evidence.len() < 255 {
            evidence.push(("native_usage", serde_json::to_value(usage).unwrap()));
        }
    }
    fn on_context_observation(&self, observation: serde_json::Value) {
        let mut evidence = self.pipeline_evidence.lock();
        if evidence.len() < 255 {
            evidence.push(("context_observation", observation));
        }
    }
    fn on_attempt(&self, mut attempt: TraceAttempt) {
        let mut attempts = self.attempts.lock();
        // Each provider call numbers retries from zero. A web-search request can make
        // several provider calls under one trace, so assign a request-wide sequence
        // before persisting to the (trace_id, attempt) primary key.
        attempt.attempt = attempts.len() as u32;
        attempts.push(attempt);
    }

    fn on_route(&self, route: TraceRoute) {
        let mut slot = self.route.lock();
        if slot.is_none() {
            *slot = Some(route);
        }
    }
}

/// 取追踪器里最后一跳的 outcome（用于把 provider 的失败分类提升到 record.error_type）。
/// 返回 'static str（outcome 常量），无 attempt 时返回 None。
pub(crate) fn last_attempt_outcome(tracer: &RequestTracer) -> Option<&'static str> {
    let last = tracer.attempts.lock().last()?.outcome.clone();
    Some(canonical_attempt_outcome(&last))
}

fn canonical_attempt_outcome(value: &str) -> &'static str {
    match value {
        outcome::QUOTA_EXHAUSTED => outcome::QUOTA_EXHAUSTED,
        outcome::ACCOUNT_THROTTLED => outcome::ACCOUNT_THROTTLED,
        outcome::ACCOUNT_SUSPENDED => outcome::ACCOUNT_SUSPENDED,
        outcome::AUTH_FAILED => outcome::AUTH_FAILED,
        outcome::TRANSIENT => outcome::TRANSIENT,
        outcome::NETWORK_ERROR => outcome::NETWORK_ERROR,
        outcome::BAD_REQUEST => outcome::BAD_REQUEST,
        _ => outcome::UNKNOWN,
    }
}

/// Image-budget warning threshold (in raw base64 chars, not decoded bytes).
/// Emits a warning when the total base64 char count of all image content in one request exceeds this threshold.
/// The threshold does not reject the request (the upstream makes the final call); it only gives operators more precise diagnostics.
const IMAGE_BUDGET_WARN_BYTES: usize = 800 * 1024;

/// Budget statistics for the image content in one inbound request.
struct ImageBudget {
    count: usize,
    total_b64_bytes: usize,
    largest_b64_bytes: usize,
}

/// Counts the total number of images in the payload and their base64 byte size.
/// Looks only at inline base64 (image source.type == "base64"), skipping url-mode images (which do not
/// go directly into a Bedrock single message body). This is a lightweight O(N) scan that does not decode base64.
fn count_image_budget(payload: &super::types::MessagesRequest) -> ImageBudget {
    let mut count = 0usize;
    let mut total = 0usize;
    let mut largest = 0usize;
    for msg in &payload.messages {
        if let serde_json::Value::Array(arr) = &msg.content {
            for item in arr {
                if item.get("type").and_then(|v| v.as_str()) != Some("image") {
                    continue;
                }
                let Some(src) = item.get("source") else {
                    continue;
                };
                if src.get("type").and_then(|v| v.as_str()) != Some("base64") {
                    continue;
                }
                let n = src
                    .get("data")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len())
                    .unwrap_or(0);
                count += 1;
                total += n;
                if n > largest {
                    largest = n;
                }
            }
        }
    }
    ImageBudget {
        count,
        total_b64_bytes: total,
        largest_b64_bytes: largest,
    }
}

/// 将 KiroProvider 错误映射为 HTTP 响应
pub(super) fn map_provider_error(err: Error) -> Response {
    if let Some(rate_limit) = err.downcast_ref::<crate::kiro::error::UpstreamRateLimitError>() {
        tracing::warn!(error = %err, "上游限流（映射为 429）");
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::new(
                "rate_limit_error",
                "Upstream rate limit exceeded. Retry later.",
            )),
        )
            .into_response();
        if let Some(value) = rate_limit
            .retry_after()
            .and_then(|value| value.parse::<header::HeaderValue>().ok())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }

    let err_str = err.to_string();

    if err
        .downcast_ref::<crate::pipeline::LocalPayloadLimit>()
        .is_some()
    {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse::new("local_payload_limit", err_str)),
        )
            .into_response();
    }

    // 本地按声明上限拒绝：与字节预算同为 413，但用独立的错误码，运维才能分清
    // 拒绝来自运维自配的字节预算还是来自「估算 vs 上游声明上限」的比较。
    if err
        .downcast_ref::<crate::pipeline::LocalTokenAdmission>()
        .is_some()
    {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse::new("local_token_admission", err_str)),
        )
            .into_response();
    }

    // Upstream request-level rejections arrive typed. Classification already happened once,
    // against the raw upstream body, where the JSON field confirmation actually works.
    // Re-deriving it here from the formatted error string is what previously bypassed that
    // confirmation: the formatted string is not valid JSON, so the endpoint layer's parse
    // branch always failed and silently degraded to a bare substring match.
    if let Some(rejection) = err.downcast_ref::<crate::kiro::error::UpstreamRequestError>() {
        use crate::kiro::error::UpstreamRejectionKind;
        match rejection.kind() {
            // This undocumented error does not identify body, field or token-window limits.
            UpstreamRejectionKind::ContentLengthThreshold => {
                tracing::warn!(
                    status = %rejection.status(),
                    "Kiro input length threshold rejection; no retry or model downgrade"
                );
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(
                        "invalid_request_error",
                        "Kiro rejected input length (CONTENT_LENGTH_EXCEEDS_THRESHOLD). The upstream did not specify whether the limit concerns total body, a text/tool-result/image field, or model context. Inspect pipeline evidence; this request was not retried, truncated or downgraded.",
                    )),
                )
                    .into_response();
            }
            // 单次输入太长（请求体本身超出上游限制）
            UpstreamRejectionKind::InputTooLong => {
                tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(
                        "invalid_request_error",
                        "Input is too long. Reduce the size of your messages.",
                    )),
                )
                    .into_response();
            }
            // Bedrock client-side validation errors (tool_use <-> tool_result mismatch, invalid
            // message sequence, etc.). The root cause is the client's own messages array, not an
            // upstream failure, so it must not map to 5xx — otherwise it triggers an upstream
            // cooldown that amplifies one client error into a 30+ burst of 503s. The provider
            // already bails out without retry on these; this mapping is the client-facing net.
            UpstreamRejectionKind::ClientValidation => {
                tracing::warn!(
                    error = %err,
                    "client messages array violates the protocol (Bedrock validation; mapped to 400 to avoid a false cooldown)"
                );
                // Return a stable, client-facing message and avoid echoing the raw upstream
                // body (which can carry request IDs or internal validation details).
                // The full error is already logged above for diagnostics.
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(
                        "invalid_request_error",
                        "Invalid message sequence: tool_use and tool_result blocks must be correctly paired and ordered.".to_string(),
                    )),
                )
                    .into_response();
            }
            // The upstream refused for a reason it did not label. Do not guess one.
            UpstreamRejectionKind::Unclassified => {}
        }
    }

    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            "Upstream API request failed.",
        )),
    )
        .into_response()
}

/// 解析普通非流式响应的最终 Anthropic usage。
///
/// 返回 `(uncached_input, output, cache_write, cache_read)`。精确 provider 快照优先；
/// 缺失时才使用 contextUsage/输入估算和本地 CacheMeter 分摊。
fn resolve_non_stream_usage(
    fallback_total_input_tokens: i32,
    context_total_input_tokens: Option<i32>,
    fallback_output_tokens: i32,
    cache_usage: super::cache_metering::CacheUsage,
    provider_usage: Option<TokenUsage>,
) -> (i32, i32, i32, i32) {
    if let Some(usage) = provider_usage {
        let usage = usage.sanitized();
        return (
            usage.uncached_input_tokens,
            usage.output_tokens,
            usage.cache_write_input_tokens,
            usage.cache_read_input_tokens,
        );
    }

    let total_input = context_total_input_tokens.unwrap_or(fallback_total_input_tokens);
    let (input, cache_write, cache_read) = cache_usage.split_against_total(total_input);
    (
        input,
        fallback_output_tokens.max(0),
        cache_write,
        cache_read,
    )
}

fn validate_max_tokens(max_tokens: i32) -> Result<(), ErrorResponse> {
    if max_tokens <= 0 {
        Err(ErrorResponse::new(
            "invalid_request_error",
            "max_tokens must be greater than 0",
        ))
    } else {
        Ok(())
    }
}

fn merge_token_limits(target: &mut Option<TokenLimits>, incoming: Option<TokenLimits>) {
    let Some(incoming) = incoming else {
        return;
    };
    match target {
        Some(target) => {
            target.max_input_tokens = target.max_input_tokens.max(incoming.max_input_tokens);
            target.max_output_tokens = target.max_output_tokens.max(incoming.max_output_tokens);
        }
        None => *target = Some(incoming),
    }
}

fn infer_model_owner(model_id: &str) -> &'static str {
    let id = model_id.to_ascii_lowercase();
    if id.starts_with("claude-") {
        "anthropic"
    } else if id.starts_with("gpt-")
        || id.starts_with("chatgpt-")
        || id.starts_with("o1-")
        || id.starts_with("o3-")
        || id.starts_with("o4-")
    {
        "openai"
    } else {
        "kiro"
    }
}

fn context_window_from_upstream(model_id: &str, token_limits: Option<&TokenLimits>) -> i32 {
    token_limits
        .and_then(|limits| limits.max_input_tokens)
        .and_then(|limit| i32::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or_else(|| get_context_window_size(model_id))
}

fn model_from_upstream(upstream: UpstreamModel) -> Model {
    let max_tokens = upstream
        .token_limits
        .as_ref()
        .and_then(|limits| limits.max_output_tokens)
        .and_then(|limit| i32::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(64_000);
    let context_window =
        context_window_from_upstream(&upstream.model_id, upstream.token_limits.as_ref());
    Model {
        display_name: upstream
            .model_name
            .clone()
            .unwrap_or_else(|| upstream.model_id.clone()),
        owned_by: infer_model_owner(&upstream.model_id).to_string(),
        id: upstream.model_id,
        object: "model".to_string(),
        created: 0,
        model_type: "chat".to_string(),
        context_window,
        max_tokens,
    }
}

fn aggregate_available_models_with_custom(
    upstream_models: Vec<UpstreamModel>,
    custom_models: &[crate::model::config::CustomModel],
) -> Vec<Model> {
    let mut merged_upstream: BTreeMap<String, UpstreamModel> = BTreeMap::new();
    for incoming in upstream_models {
        match merged_upstream.get_mut(&incoming.model_id) {
            Some(existing) => {
                if existing.model_name.is_none() {
                    existing.model_name = incoming.model_name;
                }
                if existing.description.is_none() {
                    existing.description = incoming.description;
                }
                merge_token_limits(&mut existing.token_limits, incoming.token_limits);
            }
            None => {
                merged_upstream.insert(incoming.model_id.clone(), incoming);
            }
        }
    }

    let mut models: BTreeMap<String, Model> = BTreeMap::new();
    for upstream in merged_upstream.into_values() {
        let model = model_from_upstream(upstream);
        models.insert(model.id.clone(), model);
    }

    // 自定义别名最后写入，同名时其展示元数据优先于动态条目。
    for custom in custom_models {
        let model = Model {
            id: custom.id.clone(),
            object: "model".to_string(),
            created: 0,
            owned_by: custom
                .owned_by
                .clone()
                .unwrap_or_else(|| "custom".to_string()),
            display_name: custom
                .display_name
                .clone()
                .unwrap_or_else(|| custom.id.clone()),
            model_type: "chat".to_string(),
            context_window: custom
                .context_window
                .unwrap_or_else(|| get_context_window_size(&custom.id)),
            max_tokens: custom.max_tokens.unwrap_or(64_000),
        };
        models.insert(model.id.clone(), model);
    }

    models.into_values().collect()
}

fn aggregate_available_models(upstream_models: Vec<UpstreamModel>) -> Vec<Model> {
    aggregate_available_models_with_custom(upstream_models, &crate::model::custom_models::all())
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
) -> Response {
    tracing::info!("Received GET /v1/models request");

    // 网关接管的别名先取出来：它们与 Kiro 凭据无关，Kiro 那边失败也不该让它们消失。
    let managed: Vec<Model> = state
        .gateway
        .as_ref()
        .map(|entry| entry.public_models().into_iter().map(gateway_model).collect())
        .unwrap_or_default();

    let Some(provider) = &state.kiro_provider else {
        if !managed.is_empty() {
            // 没有 Kiro 也照样有东西可用——只列网关接管的。
            return Json(ModelsResponse {
                object: "list".to_string(),
                data: managed,
            })
            .into_response();
        }
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "Kiro API provider not configured",
            )),
        )
            .into_response();
    };

    let upstream = match provider
        .token_manager()
        .discover_models_for_group(key_ctx.group.as_deref())
        .await
    {
        Ok(models) => models,
        Err(ModelDiscoveryError::NoAvailableCredentials) => {
            if !managed.is_empty() {
                return Json(ModelsResponse {
                    object: "list".to_string(),
                    data: managed,
                })
                .into_response();
            }
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "No available credentials for this API key",
                )),
            )
                .into_response();
        }
        Err(error @ ModelDiscoveryError::ColdStartFailed { .. }) => {
            tracing::warn!("动态模型列表加载失败: {}", error);
            if !managed.is_empty() {
                return Json(ModelsResponse {
                    object: "list".to_string(),
                    data: managed,
                })
                .into_response();
            }
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    "Unable to load available models from upstream",
                )),
            )
                .into_response();
        }
    };

    // 网关接管的别名排在前面并**覆盖**同名的 Kiro 条目：同一个名字，
    // 实际服务它的是网关，列表就该按网关声明的能力来说。
    let mut models = managed;
    for model in aggregate_available_models(upstream) {
        if !models.iter().any(|m| m.id == model.id) {
            models.push(model);
        }
    }

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
    .into_response()
}

/// 把网关声明的能力如实翻成对外的模型条目。
///
/// `created` 用 0 而不是编一个时间戳：网关不知道这个别名"何时创建"，
/// 那个字段在这里没有真实来源。
fn gateway_model(model: crate::gateway::service::PublicModelCapabilities) -> Model {
    Model {
        display_name: model.display_name.unwrap_or_else(|| model.id.clone()),
        id: model.id,
        object: "model".to_string(),
        created: 0,
        owned_by: "gateway".to_string(),
        model_type: "model".to_string(),
        context_window: i32::try_from(model.context_window).unwrap_or(i32::MAX),
        max_tokens: i32::try_from(model.max_output_tokens).unwrap_or(i32::MAX),
    }
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    // Count the image budget on inbound to provide precise diagnostics for later context-window-full errors
    let img_stats = count_image_budget(&payload);
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        image_count = %img_stats.count,
        image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
        image_largest_b64_kb = %(img_stats.largest_b64_bytes / 1024),
        "Received POST /v1/messages request"
    );
    if let Err(error) = validate_max_tokens(payload.max_tokens) {
        return (StatusCode::BAD_REQUEST, Json(error)).into_response();
    }
    if img_stats.total_b64_bytes > IMAGE_BUDGET_WARN_BYTES {
        tracing::warn!(
            image_count = %img_stats.count,
            image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
            "incoming image payload is large; if upstream rejects with CONTENT_LENGTH_EXCEEDS_THRESHOLD, reduce image count or use lower-resolution screenshots"
        );
    }
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    let context = match provider.pipeline().prepare(&mut payload, key_ctx.key_id) {
        Ok(context) => context,
        Err(error) => {
            let tracer = RequestTracer::new(
                &state,
                RequestTraceOptions {
                    key_ctx: key_ctx.clone(),
                    model: payload.model.clone(),
                    is_stream: payload.stream,
                },
            );
            tracer.finalize(
                "error",
                Some("pipeline_preparation"),
                Some(&error.to_string()),
                None,
                TraceUsage::zero(),
            );
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "pipeline_preparation_error",
                    error.to_string(),
                )),
            )
                .into_response();
        }
    };
    // 按需工具发现：声明的 schema 体积超预算时，改为提供分页目录 + 揭示接口。
    // 没有任何工具被移除；未揭示的工具仍可随时列目录索取，只是需要多花轮次。
    let catalog = take_tool_catalog(&mut payload, &provider.pipeline().config);
    // 分块处理工具只在有原文会话时才有意义，且必须排在目录替换之后——否则它会被
    // 当成客户端工具塞进目录里。默认关闭。
    offer_chunked_map(&mut payload, &provider.pipeline().config, context.is_some());
    if context.is_some() || catalog.is_some() {
        let stream = payload.stream;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: stream,
            },
        ));
        return super::websearch_loop::run_context_loop(
            provider,
            payload,
            hook,
            tracer,
            stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
            context,
            catalog,
        )
        .await;
    }

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        let resp = websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            key_ctx.group.as_deref(),
        )
        .await;
        // WebSearch 路径走 MCP 端点，没有 credential_id 上下文，统一记 0
        let status = if resp.status().is_success() {
            "success"
        } else {
            "error"
        };
        hook.record(0, input_tokens, 0, 0, 0, 0.0, status);
        return resp;
    }

    let payload_stream = payload.stream;
    // Mixed-tools (web_search + exec...) case: web_search coexists with other tools and falls onto the normal chat path,
    // where the upstream may return a tool_use with name=web_search. Take the internal agentic loop: search internally and feed the results back.
    if websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected mixed tools containing web_search, entering the web_search agentic loop"
        );
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: payload_stream,
            },
        ));
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            tracer,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
        )
        .await;
    }

    // 转换请求
    let conversion_result = match convert_request_with_pipeline(
        &payload,
        state.tool_compatibility_mode,
        &provider.pipeline().config,
    ) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::InvalidModel(reason) => {
                    ("invalid_request_error", format!("无效模型 ID: {}", reason))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
                ConversionError::InvalidMessageSequence(reason) => (
                    "invalid_request_error",
                    format!("消息序列无效: {}", reason),
                ),
                ConversionError::UnsupportedToolMapping(reason) => (
                    "invalid_request_error",
                    format!("工具映射不支持: {}", reason),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match crate::pipeline::serialize_request(
        &payload,
        &kiro_request,
        &provider.pipeline().config,
    ) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!(
        body_bytes = request_body.len(),
        "Kiro request prepared (content redacted)"
    );

    // 预先构造「一次修改后重试」用的修正体（策略关闭时为 None，不产生任何开销差异
    // 之外的行为变化）。只有上游按长度拒绝时才会用到它；若修正没改变任何字节，
    // 这里就是 None，从而不可能发生原样重发。
    let recovery_body = crate::pipeline::recovery_body(
        &payload,
        state.tool_compatibility_mode,
        &provider.pipeline().config,
        &request_body,
    );

    // 估算输入 tokens
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;

    // CacheMeter：根据 cache_control 断点查 / 写中转层提示词缓存。
    // 返回 estimate 口径的覆盖量；真实 input/cache 互斥分摊在拿到 total 真值时进行。
    let cache_usage = match state
        .cache_meter
        .as_ref()
        .filter(|_| provider.pipeline().config.allow_simulated_cache)
    {
        Some(cache) => {
            super::cache_metering::compute_cache_usage(cache, &payload, key_ctx.key_id).await
        }
        None => super::cache_metering::CacheUsage::default(),
    };

    if payload.stream {
        // 流式响应
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: true,
            },
        ));
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            recovery_body,
        )
        .await
    } else {
        // Responses reasoning requests must expose native reasoning even when the
        // global Anthropic compatibility flag is disabled; the Responses adapter
        // explicitly opted into it via `thinking`/`output_config`.
        let extract_thinking =
            (state.extract_thinking || payload.output_config.is_some()) && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: false,
            },
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            extract_thinking,
            tool_name_map,
            known_tool_names,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            recovery_body,
        )
        .await
    }
}

/// 在有原文会话时提供分块处理工具（默认关闭）。
///
/// 只是**提供**：网关不会自动套用分块，模型必须显式调用。这样模型才不会在不知情的
/// 情况下拿到由碎片推出的结论——工具描述里也写明了它不是无损机制。
fn offer_chunked_map(
    payload: &mut crate::anthropic::types::MessagesRequest,
    config: &crate::pipeline::config::PipelineConfig,
    has_context: bool,
) {
    if !has_context
        || config.chunked_map.strategy != crate::pipeline::config::ChunkedMapStrategy::ModelInvoked
    {
        return;
    }
    payload
        .tools
        .get_or_insert_with(Vec::new)
        .push(crate::pipeline::chunked_map::map_tool(
            config.chunked_map.chunk_bytes,
            config.chunked_map.max_chunks,
        ));
}

/// 按需工具发现：超预算时把客户端工具换成目录接口，并把原始声明交给会话保管。
///
/// 返回 `None` 表示未启用、无工具或未超预算——此时 `payload` 一字未动，行为与改造前
/// 完全一致。工具**没有被丢弃**：它们被会话持有，模型可随时列目录并索取。
fn take_tool_catalog(
    payload: &mut super::types::MessagesRequest,
    config: &crate::pipeline::config::PipelineConfig,
) -> Option<crate::pipeline::tool_catalog::CatalogSession> {
    use crate::pipeline::tool_catalog;
    if config.tool_catalog.strategy != crate::pipeline::config::ToolCatalogStrategy::OnDemand {
        return None;
    }
    let declared = payload.tools.as_ref()?;
    if declared.is_empty()
        || declared.iter().any(|tool| tool_catalog::is_catalog_tool(&tool.name))
        || !tool_catalog::should_paginate(declared, config.tool_catalog.budget_bytes)
    {
        return None;
    }
    let session = tool_catalog::CatalogSession::new(declared.clone());
    tracing::info!(
        declared = session.total(),
        declared_bytes = tool_catalog::declared_bytes(declared),
        "工具声明体积超预算，改为按需发现（工具未被移除，需多轮索取）"
    );
    payload.tools = Some(session.active_tools());
    Some(session)
}

/// 分类明确的长度拒绝 + 已构造出的修正体 → 允许再发一次。
///
/// 其它任何情形都不重试：协议配对错误改尺寸无用，未分类的拒绝更不该被当作长度问题；
/// 修正体为 `None` 时说明策略关闭或修正没改变任何字节（见 `pipeline::recovery_body`）。
fn recoverable_body<'a>(error: &anyhow::Error, recovery: Option<&'a str>) -> Option<&'a str> {
    let rejection = error.downcast_ref::<crate::kiro::error::UpstreamRequestError>()?;
    rejection
        .kind()
        .names_a_length_budget()
        .then_some(recovery)
        .flatten()
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    recovery_body: Option<String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let first = provider
        .call_api_stream(request_body, Some(tracer.as_ref()), group.as_deref())
        .await;
    // 一次修改后重试：仅在拒绝分类指向长度预算、且修正确实改变了 payload 时发生。
    // 同模型、最多一次；第二次失败就是失败，没有第三次、不换补救手段、不换账号碰运气。
    let outcome = match first {
        Ok(resp) => Ok(resp),
        Err(first_error) => match recoverable_body(&first_error, recovery_body.as_deref()) {
            Some(corrected) => {
                tracing::info!(
                    "上游按长度拒绝；对 payload 施加一次无损修正后重发（同模型，仅此一次）"
                );
                provider
                    .call_api_stream(corrected, Some(tracer.as_ref()), group.as_deref())
                    .await
            }
            None => Err(first_error),
        },
    };
    let call_result = match outcome {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
            // 重试链路全部失败、未开始返回内容：error_type 取最后一跳分类
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    let response = call_result.response;
    let credential_id = call_result.credential_id;

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_thinking(
        model,
        input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
    );
    ctx.cache_usage = cache_usage;

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(response, ctx, initial_events, hook, credential_id, tracer);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();
    let settlement = StreamSettlement::new(hook, credential_id, tracer, &ctx);

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS)), settlement, 0u64),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, mut settlement, mut sent_bytes)| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            settlement.tracer.mark_first_token();
                            sent_bytes += chunk.len() as u64;
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            let sse_events = ctx.process_kiro_event(&event);
                                            events.extend(sse_events);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            settlement.update(&ctx, sent_bytes);

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, settlement, sent_bytes)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 流已开始后无法修改 HTTP 状态码。关闭已打开的内容块并发送
                            // Anthropic error 终态，不能用正常 message_stop 掩盖上游断流。
                            let final_events = ctx.generate_error_events(
                                "upstream_error",
                                "Upstream response stream was interrupted",
                            );
                            settlement.update(&ctx, sent_bytes);
                            // 已开始返回内容后上游断流：标记为 interrupted，带已发送字节数
                            settlement.finish(
                                "error",
                                "interrupted",
                                Some(outcome::STREAM_INTERRUPTED),
                                Some(&e.to_string()),
                                Some(sent_bytes),
                            );
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement, sent_bytes)))
                        }
                        None => {
                            // 流结束，发送最终事件（generate_final_events 内部会 finish()
                            // 累积器，据此判定是否有半截 / 非法工具调用 JSON）。
                            let final_events = ctx.generate_final_events();
                            settlement.update(&ctx, sent_bytes);
                            if let Some(message) = ctx.tool_json_error_message() {
                                // 工具调用 JSON 半截 / 非法：实时流已回 200，无法改状态码，
                                // 只能记 error 并让 generate_final_events 补发的 `error` 事件透传给客户端。
                                settlement.finish(
                                    "error",
                                    "error",
                                    Some(outcome::BAD_REQUEST),
                                    Some(&message),
                                    None,
                                );
                            } else {
                                settlement.finish("success", "success", None, None, None);
                            }
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement, sent_bytes)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, settlement, sent_bytes)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

/// Exactly-once settlement for a live Messages stream.
///
/// Responses consumes this stream as an inner body. If the outer client goes
/// away, dropping that body also drops the in-flight unfold future; `Drop`
/// records the latest usage snapshot and closes the trace instead of losing
/// already incurred provider usage.
struct StreamSettlement {
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
    usage: TraceUsage,
    /// 最近一次从 StreamContext 取得的上下文观测；流结束时落一次，不逐事件重复落。
    observation: Option<serde_json::Value>,
    sent_bytes: u64,
    settled: bool,
}

impl StreamSettlement {
    fn new(
        hook: UsageRecordHook,
        credential_id: u64,
        tracer: std::sync::Arc<RequestTracer>,
        ctx: &StreamContext,
    ) -> Self {
        Self {
            hook,
            credential_id,
            tracer,
            usage: stream_trace_usage(ctx),
            observation: context_observation(ctx),
            sent_bytes: 0,
            settled: false,
        }
    }

    fn update(&mut self, ctx: &StreamContext, sent_bytes: u64) {
        self.usage = stream_trace_usage(ctx);
        self.observation = context_observation(ctx);
        self.sent_bytes = sent_bytes;
    }

    fn finish(
        &mut self,
        usage_status: &str,
        trace_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
        interrupted_after_bytes: Option<u64>,
    ) {
        if self.settled {
            return;
        }
        self.record_usage(usage_status);
        if let Some(observation) = self.observation.take() {
            self.tracer.on_context_observation(observation);
        }
        self.tracer.finalize(
            trace_status,
            error_type,
            error_message,
            interrupted_after_bytes,
            self.usage,
        );
        self.settled = true;
    }

    fn record_usage(&self, status: &str) {
        self.hook.record(
            self.credential_id,
            self.usage.input_tokens.min(i32::MAX as u64) as i32,
            self.usage.output_tokens.min(i32::MAX as u64) as i32,
            self.usage.cache_creation_tokens.min(i32::MAX as u64) as i32,
            self.usage.cache_read_tokens.min(i32::MAX as u64) as i32,
            self.usage.credits,
            status,
        );
    }
}

impl Drop for StreamSettlement {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.record_usage("error");
        self.tracer.finalize(
            "interrupted",
            Some(outcome::STREAM_INTERRUPTED),
            Some("response stream was cancelled before completion"),
            Some(self.sent_bytes),
            self.usage,
        );
        self.settled = true;
    }
}

/// 构造一次被动的上下文观测记录。
///
/// 只有上游真的发来 contextUsageEvent 才产生记录——没有观测就是没有观测，不补零。
/// 记录里刻意**并列**三样互不替代的东西，而不是把它们合成一个结论：
/// - `percentage`：上游原样下发的使用率
/// - `guessedWindowTokens`：当前换算用的写死窗口表取值（`get_context_window_size`）
/// - `derivedInputTokens`：二者相乘的结果，也就是当前上报给客户端的数字
///
/// 声明上限 `maxInputTokens` 与原生 `tokenUsage` 已由同一条 trace 的 `wire_audit`
/// 与 `native_usage` 证据行承载，这里不重复存；聚合时在 trace 内 join 即可。
///
/// `shape` 是脱敏后的事件结构（字符串只留长度），用于让 breakdown 的真实形状能从
/// 正常业务流量中被观察到——本项目不允许为此发探测流量。
fn context_observation(ctx: &StreamContext) -> Option<serde_json::Value> {
    let percentage = ctx.context_usage_percentage?;
    Some(json!({
        "model": ctx.model,
        "percentage": percentage,
        "guessedWindowTokens": get_context_window_size(&ctx.model),
        "derivedInputTokens": ctx.context_input_tokens,
        "shape": ctx.context_usage_shape,
        "note": "passive observation of one ordinary request; no probe traffic",
    }))
}

/// 从 StreamContext 提取用量，转成 trace 行用量（与 record_stream_usage 同源）
fn stream_trace_usage(ctx: &StreamContext) -> TraceUsage {
    let (input, cache_creation, cache_read) = ctx.resolved_usage();
    TraceUsage {
        input_tokens: input.max(0) as u64,
        output_tokens: ctx.resolved_output_tokens() as u64,
        cache_creation_tokens: cache_creation.max(0) as u64,
        cache_read_tokens: cache_read.max(0) as u64,
        source: UsageSource::resolve(ctx.provider_token_usage.is_some(), &ctx.cache_usage),
        credits: if ctx.credits.is_finite() && ctx.credits > 0.0 {
            ctx.credits
        } else {
            0.0
        },
    }
}

pub(crate) enum NonStreamExecutionError {
    Provider(Error),
    Response(Response),
}

pub(crate) fn new_non_stream_request_tracer(
    state: &AppState,
    key_ctx: KeyContext,
    model: String,
) -> std::sync::Arc<RequestTracer> {
    std::sync::Arc::new(RequestTracer::new(
        state,
        RequestTraceOptions {
            key_ctx,
            model,
            is_stream: false,
        },
    ))
}

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    // 非流式路径直接处理结构化 Event::ToolUse，不经过 <invoke> 文本嗅探，
    // 因此这里不需要工具表校验；保留参数以对齐调用方签名。
    _known_tool_names: std::collections::HashSet<String>,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    recovery_body: Option<String>,
) -> Response {
    match execute_non_stream_request(
        provider,
        request_body,
        model,
        input_tokens,
        thinking_enabled,
        tool_name_map,
        hook,
        cache_usage,
        tracer,
        group,
        recovery_body,
    )
    .await
    {
        Ok(response_body) => (StatusCode::OK, Json(response_body)).into_response(),
        Err(NonStreamExecutionError::Provider(error)) => map_provider_error(error),
        Err(NonStreamExecutionError::Response(response)) => response,
    }
}

pub(crate) async fn execute_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    // 一次修改后重试用的修正体。compaction 通道传 None：它有自己的溢出处理，
    // 不应再叠加一次无损修正重试。
    recovery_body: Option<String>,
) -> Result<serde_json::Value, NonStreamExecutionError> {
    // 调用 Kiro API（支持多凭据故障转移）
    let first = provider
        .call_api(request_body, Some(tracer.as_ref()), group.as_deref())
        .await;
    // 与流式路径同一条规则：分类指向长度预算、且修正确实改了 payload 才重发一次。
    let outcome = match first {
        Ok(resp) => Ok(resp),
        Err(first_error) => match recoverable_body(&first_error, recovery_body.as_deref()) {
            Some(corrected) => {
                tracing::info!(
                    "上游按长度拒绝；对 payload 施加一次无损修正后重发（同模型，仅此一次）"
                );
                provider
                    .call_api(corrected, Some(tracer.as_ref()), group.as_deref())
                    .await
            }
            None => Err(first_error),
        },
    };
    let call_result = match outcome {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return Err(NonStreamExecutionError::Provider(e));
        }
    };
    let response = call_result.response;
    let credential_id = call_result.credential_id;

    // 读取响应体
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "interrupted",
                Some(outcome::STREAM_INTERRUPTED),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return Err(NonStreamExecutionError::Response(
                (
                    StatusCode::BAD_GATEWAY,
                    Json(ErrorResponse::new(
                        "api_error",
                        format!("读取响应失败: {}", e),
                    )),
                )
                    .into_response(),
            ));
        }
    };

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut native_thinking = String::new();
    let mut native_thinking_signature: Option<String> = None;
    let mut native_redacted_thinking: Vec<String> = Vec::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    // metadataEvent.tokenUsage 是本次 provider 调用的精确最终快照。
    let mut provider_token_usage: Option<TokenUsage> = None;
    // meteringEvent 上报的 credit 计费量（上游真实下发）；
    // input/cache_* 的互斥分摊在拿到 total 真值后由 cache_usage 完成。
    let mut credits: f64 = 0.0;
    // 最近一次 meteringEvent 的完整 payload，用于在响应体 usage 中透传
    // credit_usage / credit_unit / credit_unit_plural 字段，与 /v1/messages
    // 流式（message_delta）行为一致；如果上游多次下发则取最后一次。
    let mut metering: Option<crate::kiro::model::events::MeteringEvent> = None;

    // 工具调用参数 JSON 累积器：按 tool_use_id 缓冲分片，stop 时整体解析。
    // 半截 / 非法 JSON 显式暴露为错误（返回 502），不再静默回退 {} 或丢弃。
    let mut tool_accumulator = super::stream::ToolJsonAccumulator::new();
    let mut tool_json_error: Option<super::stream::ToolJsonAccumulatorError> = None;

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ReasoningContent(reasoning) => {
                            if let Some(text) = reasoning.text
                                && !text.is_empty()
                            {
                                native_thinking.push_str(&text);
                            }
                            if let Some(signature) = reasoning.signature
                                && !signature.is_empty()
                            {
                                native_thinking_signature = Some(signature);
                            }
                            if let Some(redacted) = reasoning.redacted_content
                                && !redacted.is_empty()
                            {
                                native_redacted_thinking.push(redacted);
                            }
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;
                            match tool_accumulator.push(&tool_use, &tool_name_map) {
                                Ok(Some(completed)) => {
                                    tool_uses.push(completed.to_anthropic_block());
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::error!("{}", e);
                                    tool_json_error = Some(e);
                                }
                            }
                        }
                        Event::Metadata(metadata) => {
                            if let Some(usage) = metadata.token_usage {
                                let usage = usage.sanitized();
                                tracing::debug!(
                                    uncached_input_tokens = usage.uncached_input_tokens,
                                    cache_write_input_tokens = usage.cache_write_input_tokens,
                                    cache_read_input_tokens = usage.cache_read_input_tokens,
                                    output_tokens = usage.output_tokens,
                                    "收到 metadataEvent.tokenUsage 精确用量"
                                );
                                // 单条 provider 流内是最终快照，重复事件取最后一份。
                                provider_token_usage = Some(usage);
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比计算实际的 input_tokens
                            let window_size = get_context_window_size(model);
                            let actual_input_tokens =
                                (context_usage.context_usage_percentage * (window_size as f64)
                                    / 100.0) as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Metering(event_metering) => {
                            // 上游只下发 credit；token / cache 字段不存在
                            credits += event_metering.usage;
                            tracing::debug!(
                                usage = event_metering.usage,
                                unit = %event_metering.unit,
                                unit_plural = %event_metering.unit_plural,
                                "metering credits +{:.6}", event_metering.usage
                            );
                            metering = Some(event_metering);
                        }
                        Event::Exception { exception_type, .. } => {
                            if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 收尾：若仍有未收到 stop=true 的工具调用缓冲（上游在参数写到一半时截断），
    // finish() 返回 IncompleteJson。已有错误则保持不变。
    if tool_json_error.is_none()
        && let Err(e) = tool_accumulator.finish()
    {
        tracing::error!("{}", e);
        tool_json_error = Some(e);
    }

    // 工具调用 JSON 半截 / 非法：非流式路径尚未发送任何字节，直接回 502，
    // 明确暴露上游问题，而不是把无法解析的参数当成完整调用返回。
    if let Some(err) = tool_json_error {
        let message = err.message();
        if let Some(usage) = provider_token_usage {
            let usage = usage.sanitized();
            let trace_usage = TraceUsage {
                input_tokens: usage.uncached_input_tokens as u64,
                output_tokens: usage.output_tokens as u64,
                cache_creation_tokens: usage.cache_write_input_tokens as u64,
                cache_read_tokens: usage.cache_read_input_tokens as u64,
                credits: if credits.is_finite() && credits > 0.0 {
                    credits
                } else {
                    0.0
                },
                source: UsageSource::Provider,
            };
            hook.record(
                credential_id,
                usage.uncached_input_tokens,
                usage.output_tokens,
                usage.cache_write_input_tokens,
                usage.cache_read_input_tokens,
                credits,
                "error",
            );
            tracer.finalize(
                "error",
                Some(outcome::BAD_REQUEST),
                Some(&message),
                None,
                trace_usage,
            );
        } else {
            // metadata 缺失时保留原有错误口径，不把不完整工具输出估算成已消费量。
            hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                Some(outcome::BAD_REQUEST),
                Some(&message),
                None,
                TraceUsage::zero(),
            );
        }
        return Err(NonStreamExecutionError::Response(
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new("upstream_tool_json_error", message)),
            )
                .into_response(),
        ));
    }

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 剥离混入文本的字面 <tool_use> XML 泄漏（非流式：整段文本已就绪，一次性剥离）。
    let text_content = crate::kiro::model::events::strip_tool_use_xml_leaks(&text_content);

    // 构建响应内容
    let mut content = build_non_stream_content(
        thinking_enabled,
        text_content,
        native_thinking,
        native_thinking_signature,
        native_redacted_thinking,
    );
    content.extend(tool_uses);

    // provider 未下发 metadataEvent 时才使用本地输出估算。
    let fallback_output_tokens = token::estimate_output_tokens(&content);
    let (final_input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens) =
        resolve_non_stream_usage(
            input_tokens,
            context_input_tokens,
            fallback_output_tokens,
            cache_usage,
            provider_token_usage,
        );

    // 构建 Anthropic 响应
    let mut usage_json = json!({
        "input_tokens": final_input_tokens,
        "output_tokens": output_tokens,
        "cache_creation_input_tokens": cache_creation_tokens,
        "cache_read_input_tokens": cache_read_tokens
    });
    // 透传上游 meteringEvent 的 credit_* 字段，让客户端拿到与 Kiro
    // 后端口径一致的计费元数据；只在收到过 meteringEvent 时才追加。
    if let Some(m) = &metering {
        usage_json["credit_usage"] = json!(m.usage);
        usage_json["credit_unit"] = json!(m.unit);
        usage_json["credit_unit_plural"] = json!(m.unit_plural);
    }
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage_json
    });

    hook.record(
        credential_id,
        final_input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        credits,
        "success",
    );
    tracer.finalize(
        "success",
        None,
        None,
        None,
        TraceUsage {
            input_tokens: final_input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            cache_creation_tokens: cache_creation_tokens.max(0) as u64,
            cache_read_tokens: cache_read_tokens.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
            source: UsageSource::resolve(provider_token_usage.is_some(), &cache_usage),
        },
    );
    Ok(response_body)
}

fn build_non_stream_content(
    thinking_enabled: bool,
    text_content: String,
    native_thinking: String,
    native_thinking_signature: Option<String>,
    native_redacted_thinking: Vec<String>,
) -> Vec<serde_json::Value> {
    let mut content = Vec::new();
    let has_native_thinking = !native_thinking.is_empty();

    if thinking_enabled {
        if has_native_thinking {
            content.push(json!({
                "type": "thinking",
                "thinking": native_thinking.clone(),
                "signature": native_thinking_signature
                    .unwrap_or_else(|| super::stream::THINKING_SIGNATURE_PLACEHOLDER.to_string()),
            }));
        } else {
            // 从完整文本中提取 thinking 块，兼容旧的 <thinking> 文本路径。
            let (thinking, remaining_text) =
                super::stream::extract_thinking_from_complete_text(&text_content);

            if let Some(thinking_text) = thinking {
                content.push(json!({
                    "type": "thinking",
                    "thinking": thinking_text,
                    "signature": super::stream::THINKING_SIGNATURE_PLACEHOLDER,
                }));
            }

            if !remaining_text.is_empty() {
                content.push(json!({
                    "type": "text",
                    "text": remaining_text
                }));
            }
        }

        for redacted in native_redacted_thinking {
            content.push(json!({
                "type": "redacted_thinking",
                "data": redacted
            }));
        }

        if has_native_thinking && !text_content.is_empty() {
            content.push(json!({
                "type": "text",
                "text": text_content
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    } else if has_native_thinking {
        content.push(json!({
            "type": "text",
            "text": native_thinking
        }));
    }
    content
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Opus 4.6：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - 仅在客户端未提供 thinking 时补默认值；绝不覆盖显式预算/effort。
pub(crate) fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") || payload.thinking.is_some() {
        return;
    }

    let is_opus_4_6 = model_lower.contains("opus")
        && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 { "adaptive" } else { "enabled" };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });

    if is_opus_4_6 && payload.output_config.is_none() {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    Extension(_key_ctx): Extension<KeyContext>,
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );
    if let Err(error) = validate_max_tokens(payload.max_tokens) {
        return (StatusCode::BAD_REQUEST, Json(error)).into_response();
    }
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求

    let context = match provider.pipeline().prepare(&mut payload, key_ctx.key_id) {
        Ok(context) => context,
        Err(error) => {
            let tracer = RequestTracer::new(
                &state,
                RequestTraceOptions {
                    key_ctx: key_ctx.clone(),
                    model: payload.model.clone(),
                    is_stream: payload.stream,
                },
            );
            tracer.finalize(
                "error",
                Some("pipeline_preparation"),
                Some(&error.to_string()),
                None,
                TraceUsage::zero(),
            );
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "pipeline_preparation_error",
                    error.to_string(),
                )),
            )
                .into_response();
        }
    };
    // 按需工具发现：声明的 schema 体积超预算时，改为提供分页目录 + 揭示接口。
    // 没有任何工具被移除；未揭示的工具仍可随时列目录索取，只是需要多花轮次。
    let catalog = take_tool_catalog(&mut payload, &provider.pipeline().config);
    // 分块处理工具只在有原文会话时才有意义，且必须排在目录替换之后——否则它会被
    // 当成客户端工具塞进目录里。默认关闭。
    offer_chunked_map(&mut payload, &provider.pipeline().config, context.is_some());
    if context.is_some() || catalog.is_some() {
        let stream = payload.stream;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: stream,
            },
        ));
        return super::websearch_loop::run_context_loop(
            provider,
            payload,
            hook,
            tracer,
            stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
            context,
            catalog,
        )
        .await;
    }
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        let resp = websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            key_ctx.group.as_deref(),
        )
        .await;
        let status = if resp.status().is_success() {
            "success"
        } else {
            "error"
        };
        hook.record(0, input_tokens, 0, 0, 0, 0.0, status);
        return resp;
    }

    let payload_stream = payload.stream;
    // Mixed-tools (web_search + exec...) case: web_search coexists with other tools and falls onto the normal chat path,
    // where the upstream may return a tool_use with name=web_search. Take the internal agentic loop: search internally and feed the results back.
    if websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected mixed tools containing web_search, entering the web_search agentic loop"
        );
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: payload_stream,
            },
        ));
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            tracer,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
        )
        .await;
    }

    // 转换请求
    let conversion_result = match convert_request_with_pipeline(
        &payload,
        state.tool_compatibility_mode,
        &provider.pipeline().config,
    ) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::InvalidModel(reason) => {
                    ("invalid_request_error", format!("无效模型 ID: {}", reason))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
                ConversionError::InvalidMessageSequence(reason) => (
                    "invalid_request_error",
                    format!("消息序列无效: {}", reason),
                ),
                ConversionError::UnsupportedToolMapping(reason) => (
                    "invalid_request_error",
                    format!("工具映射不支持: {}", reason),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match crate::pipeline::serialize_request(
        &payload,
        &kiro_request,
        &provider.pipeline().config,
    ) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!(
        body_bytes = request_body.len(),
        "Kiro request prepared (content redacted)"
    );

    // 预先构造「一次修改后重试」用的修正体（策略关闭时为 None，不产生任何开销差异
    // 之外的行为变化）。只有上游按长度拒绝时才会用到它；若修正没改变任何字节，
    // 这里就是 None，从而不可能发生原样重发。
    let recovery_body = crate::pipeline::recovery_body(
        &payload,
        state.tool_compatibility_mode,
        &provider.pipeline().config,
        &request_body,
    );

    // 计算总 input tokens
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;

    // CacheMeter：根据 cache_control 断点查 / 写中转层提示词缓存（estimate 口径）。
    let cache_usage = match state
        .cache_meter
        .as_ref()
        .filter(|_| provider.pipeline().config.allow_simulated_cache)
    {
        Some(cache) => {
            super::cache_metering::compute_cache_usage(cache, &payload, key_ctx.key_id).await
        }
        None => super::cache_metering::CacheUsage::default(),
    };

    if payload.stream {
        // 流式响应（缓冲模式）
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: true,
            },
        ));
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            hook,
            total_input_tokens,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: false,
            },
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            extract_thinking,
            tool_name_map,
            known_tool_names,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            recovery_body,
        )
        .await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    hook: UsageRecordHook,
    fallback_input_tokens: i32,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider
        .call_api_stream(request_body, Some(tracer.as_ref()), group.as_deref())
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, fallback_input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    let response = call_result.response;
    let credential_id = call_result.credential_id;

    // 创建缓冲流处理上下文
    let mut ctx = BufferedStreamContext::new(
        model,
        fallback_input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
    );
    ctx.set_cache_usage(cache_usage);

    // 创建缓冲 SSE 流
    let stream = create_buffered_sse_stream(response, ctx, hook, credential_id, tracer);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 创建缓冲 SSE 事件流
///
/// 工作流程：
/// 1. 等待上游流完成，期间只发送 ping 保活信号
/// 2. 使用 StreamContext 的事件处理逻辑处理所有 Kiro 事件，结果缓存
/// 3. 流结束后，用正确的 input_tokens 更正 message_start 事件
/// 4. 一次性发送所有事件
fn create_buffered_sse_stream(
    response: reqwest::Response,
    ctx: BufferedStreamContext,
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            hook,
            credential_id,
            tracer,
            0u64,
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, hook, credential_id, tracer, mut sent_bytes)| async move {
            if finished {
                return None;
            }

            loop {
                tokio::select! {
                    // 使用 biased 模式，优先检查 ping 定时器
                    // 避免在上游 chunk 密集时 ping 被"饿死"
                    biased;

                    // 优先检查 ping 保活（等待期间唯一发送的数据）
                    _ = ping_interval.tick() => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, hook, credential_id, tracer, sent_bytes)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                tracer.mark_first_token();
                                sent_bytes += chunk.len() as u64;
                                // 解码事件
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!("缓冲区溢出: {}", e);
                                }

                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            if let Ok(event) = Event::from_frame(frame) {
                                                // 缓冲事件（复用 StreamContext 的处理逻辑）
                                                ctx.process_and_buffer(&event);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                // 发生错误，完成处理并返回所有事件
                                let all_events = ctx.finish_and_get_all_events();
                                let (i, o, cc, cr, credits) = ctx.final_usage();
                                hook.record(credential_id, i, o, cc, cr, credits, "error");
                                // 缓冲模式 chunk 读取失败：上游中途断流
                                tracer.finalize(
                                    "interrupted",
                                    Some(outcome::STREAM_INTERRUPTED),
                                    Some(&e.to_string()),
                                    Some(sent_bytes),
                                    TraceUsage {
                                        input_tokens: i.max(0) as u64,
                                        output_tokens: o.max(0) as u64,
                                        cache_creation_tokens: cc.max(0) as u64,
                                        cache_read_tokens: cr.max(0) as u64,
                                        credits: if credits.is_finite() && credits > 0.0 { credits } else { 0.0 },
                                        source: UsageSource::resolve(ctx.has_provider_usage(), ctx.cache_usage()),
                                    },
                                );
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）。
                                // finish_and_get_all_events 内部会 finish() 累积器；若有半截 /
                                // 非法工具调用 JSON，error 事件已随缓冲发出，这里据此记 error。
                                let all_events = ctx.finish_and_get_all_events();
                                let (i, o, cc, cr, credits) = ctx.final_usage();
                                let trace_usage = TraceUsage {
                                    input_tokens: i.max(0) as u64,
                                    output_tokens: o.max(0) as u64,
                                    cache_creation_tokens: cc.max(0) as u64,
                                    cache_read_tokens: cr.max(0) as u64,
                                    credits: if credits.is_finite() && credits > 0.0 { credits } else { 0.0 },
                                    source: UsageSource::resolve(ctx.has_provider_usage(), ctx.cache_usage()),
                                };
                                if let Some(message) = ctx.tool_json_error_message() {
                                    hook.record(credential_id, i, o, cc, cr, credits, "error");
                                    tracer.finalize(
                                        "error",
                                        Some(outcome::BAD_REQUEST),
                                        Some(&message),
                                        None,
                                        trace_usage,
                                    );
                                } else {
                                    hook.record(credential_id, i, o, cc, cr, credits, "success");
                                    tracer.finalize("success", None, None, None, trace_usage);
                                }
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::ToolCompatibilityMode;

    #[test]
    fn dropped_stream_settles_latest_usage_exactly_once() {
        let aggregator = std::sync::Arc::new(crate::admin::usage_stats::UsageAggregator::new());
        let state = AppState::new(false, ToolCompatibilityMode::Raw).with_usage(
            None,
            None,
            Some(aggregator.clone()),
        );
        let hook = UsageRecordHook::from_state(&state, 0, "test-model".to_string());
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: KeyContext {
                    key_id: 0,
                    group: None,
                    key_source: TraceKeySource::MasterApiKey,
                    client_ip: None,
                },
                model: "test-model".to_string(),
                is_stream: true,
            },
        ));
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            11,
            false,
            std::collections::HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.output_tokens = 7;
        ctx.credits = 0.5;

        let mut settlement = StreamSettlement::new(hook, 42, tracer, &ctx);
        settlement.update(&ctx, 123);
        drop(settlement);

        let overview = aggregator.overview();
        assert_eq!(overview.today_calls, 1);
        assert_eq!(overview.today_errors, 1);
        assert_eq!(overview.today_input_tokens, 11);
        assert_eq!(overview.today_output_tokens, 7);
        assert_eq!(overview.today_credits, 0.5);
    }

    #[test]
    fn tracer_renumbers_attempts_across_provider_rounds_before_persisting() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        let tracer = RequestTracer {
            pipeline_evidence: parking_lot::Mutex::new(Vec::new()),
            store: Some(store.clone()),
            trace_id: "gpt-websearch-trace".to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 7,
            key_source: TraceKeySource::ClientKey,
            client_ip: None,
            model: "gpt-5.6-luna".to_string(),
            is_stream: false,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            attempts: parking_lot::Mutex::new(Vec::new()),
            route: parking_lot::Mutex::new(None),
        };

        let attempt = |attempt, credential_id, outcome: &str| TraceAttempt {
            attempt,
            credential_id,
            endpoint: "ide".to_string(),
            http_status: Some(200),
            outcome: outcome.to_string(),
            error_snippet: None,
            duration_ms: 10,
        };

        // First provider round reports local attempts 0,1; the next round starts at 0 again.
        tracer.on_attempt(attempt(0, 11, outcome::TRANSIENT));
        tracer.on_attempt(attempt(1, 12, outcome::SUCCESS));
        tracer.on_attempt(attempt(0, 13, outcome::SUCCESS));
        tracer.finalize(
            "success",
            None,
            None,
            None,
            TraceUsage {
                input_tokens: 101,
                output_tokens: 23,
                cache_creation_tokens: 7,
                cache_read_tokens: 89,
                credits: 0.25,
                source: UsageSource::Simulated,
            },
        );

        let (records, total) = store.query_paged(&TraceQuery {
            model: Some("gpt-5.6-luna".to_string()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.final_credential_id, 13);
        assert_eq!(record.total_attempts, 3);
        assert_eq!(
            record
                .attempts
                .iter()
                .map(|a| a.attempt)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(record.input_tokens, 101);
        assert_eq!(record.output_tokens, 23);
        assert_eq!(record.cache_creation_tokens, 7);
        assert_eq!(record.cache_read_tokens, 89);
        assert_eq!(record.credits, 0.25);
    }

    #[test]
    fn tracer_only_marks_first_token_for_streaming_requests() {
        let mut tracer = RequestTracer {
            pipeline_evidence: parking_lot::Mutex::new(Vec::new()),
            store: None,
            trace_id: "first-token-trace".to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 0,
            key_source: TraceKeySource::MasterApiKey,
            client_ip: None,
            model: "claude-sonnet-4".to_string(),
            is_stream: false,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            attempts: parking_lot::Mutex::new(Vec::new()),
            route: parking_lot::Mutex::new(None),
        };

        tracer.mark_first_token();
        assert!(tracer.first_token_at.lock().is_none());

        tracer.is_stream = true;
        tracer.mark_first_token();
        let first = *tracer.first_token_at.lock();
        assert!(first.is_some());

        tracer.mark_first_token();
        assert_eq!(*tracer.first_token_at.lock(), first);
    }

    #[test]
    fn tracer_uses_terminal_mcp_attempt_for_failure_fields() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        let tracer = RequestTracer {
            store: Some(store.clone()),
            pipeline_evidence: parking_lot::Mutex::new(Vec::new()),
            trace_id: "mcp-failure-trace".to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 0,
            key_source: TraceKeySource::MasterApiKey,
            client_ip: None,
            model: "claude-sonnet-4".to_string(),
            is_stream: true,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            attempts: parking_lot::Mutex::new(Vec::new()),
            route: parking_lot::Mutex::new(None),
        };
        let attempt = |credential_id, endpoint: &str, status, attempt_outcome: &str| TraceAttempt {
            attempt: 0,
            credential_id,
            endpoint: endpoint.to_string(),
            http_status: Some(status),
            outcome: attempt_outcome.to_string(),
            error_snippet: None,
            duration_ms: 10,
        };

        tracer.on_attempt(attempt(11, "ide", 200, outcome::SUCCESS));
        tracer.on_attempt(attempt(29, "cli", 503, outcome::TRANSIENT));
        tracer.finalize(
            "error",
            last_attempt_outcome(&tracer),
            Some("MCP request failed"),
            None,
            TraceUsage::zero(),
        );

        let (records, total) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1);
        let record = &records[0];
        assert_eq!(record.final_credential_id, 29);
        assert_eq!(record.error_type.as_deref(), Some(outcome::TRANSIENT));
        assert_eq!(record.total_attempts, 2);
        assert_eq!(record.attempts[0].endpoint, "ide");
        assert_eq!(record.attempts[1].endpoint, "cli");
    }

    #[test]
    fn account_suspended_attempt_is_preserved_as_request_error_type() {
        assert_eq!(
            canonical_attempt_outcome(outcome::ACCOUNT_SUSPENDED),
            outcome::ACCOUNT_SUSPENDED
        );
    }

    fn request_with_tools(count: usize) -> crate::anthropic::types::MessagesRequest {
        use crate::anthropic::types::{Message, MessagesRequest, Tool};
        let tools = (0..count)
            .map(|i| Tool {
                tool_type: None,
                name: format!("client_tool_{i}"),
                description: "x".repeat(400),
                input_schema: Default::default(),
                max_uses: None,
                cache_control: None,
            })
            .collect();
        MessagesRequest {
            model: "claude-sonnet-4.5".into(),
            max_tokens: 64,
            messages: vec![Message { role: "user".into(), content: serde_json::json!("hi") }],
            stream: false,
            system: None,
            tools: Some(tools),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        }
    }

    fn on_demand_config(budget: usize) -> crate::pipeline::config::PipelineConfig {
        let mut config = crate::pipeline::config::PipelineConfig::default();
        config.tool_catalog.strategy = crate::pipeline::config::ToolCatalogStrategy::OnDemand;
        config.tool_catalog.budget_bytes = budget;
        config
    }

    /// 超预算时替换为目录接口，但**原始声明一个不少**地交给会话保管。
    #[test]
    fn oversized_tool_declarations_become_a_catalog_without_losing_any_tool() {
        let mut payload = request_with_tools(50);
        let session = take_tool_catalog(&mut payload, &on_demand_config(4096))
            .expect("超预算应启用按需发现");
        assert_eq!(session.total(), 50, "全部工具仍在会话中，未被丢弃");
        let sent = payload.tools.as_ref().unwrap();
        assert_eq!(sent.len(), 2, "本轮只声明两个目录工具");
        assert!(
            sent.iter()
                .all(|t| crate::pipeline::tool_catalog::is_catalog_tool(&t.name))
        );
    }

    /// 关闭、未超预算、无工具时一字不动——行为与改造前完全一致。
    #[test]
    fn tool_catalog_is_inert_when_disabled_or_under_budget() {
        let mut payload = request_with_tools(50);
        let before = serde_json::to_value(&payload.tools).unwrap();
        assert!(
            take_tool_catalog(&mut payload, &crate::pipeline::config::PipelineConfig::default())
                .is_none()
        );
        assert_eq!(
            serde_json::to_value(&payload.tools).unwrap(),
            before,
            "关闭时不得改动 payload"
        );

        let mut payload = request_with_tools(2);
        let before = serde_json::to_value(&payload.tools).unwrap();
        assert!(take_tool_catalog(&mut payload, &on_demand_config(10_000_000)).is_none());
        assert_eq!(
            serde_json::to_value(&payload.tools).unwrap(),
            before,
            "未超预算不得改动 payload"
        );

        let mut payload = request_with_tools(0);
        assert!(take_tool_catalog(&mut payload, &on_demand_config(1024)).is_none());
    }

    fn model_invoked_map() -> crate::pipeline::config::PipelineConfig {
        let mut config = crate::pipeline::config::PipelineConfig::default();
        config.chunked_map.strategy = crate::pipeline::config::ChunkedMapStrategy::ModelInvoked;
        config
    }

    /// 只在有原文会话且策略开启时提供；默认关闭，且无会话时不提供。
    #[test]
    fn chunked_map_is_offered_only_with_a_context_session_and_when_enabled() {
        let has_map = |payload: &crate::anthropic::types::MessagesRequest| {
            payload.tools.as_ref().is_some_and(|tools| {
                tools
                    .iter()
                    .any(|t| crate::pipeline::chunked_map::is_map_tool(&t.name))
            })
        };

        let mut payload = request_with_tools(1);
        offer_chunked_map(&mut payload, &model_invoked_map(), true);
        assert!(has_map(&payload), "开启且有会话时应提供");

        let mut payload = request_with_tools(1);
        offer_chunked_map(&mut payload, &model_invoked_map(), false);
        assert!(!has_map(&payload), "没有原文会话时分块无对象可切");

        let mut payload = request_with_tools(1);
        offer_chunked_map(
            &mut payload,
            &crate::pipeline::config::PipelineConfig::default(),
            true,
        );
        assert!(!has_map(&payload), "默认必须关闭");
    }

    fn rejection(body: &str) -> anyhow::Error {
        crate::kiro::error::UpstreamRequestError::api("非流式", StatusCode::BAD_REQUEST, body).into()
    }

    const LENGTH_REJECTION: &str = r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#;

    /// 长度拒绝 + 有修正体 → 允许重发一次。
    #[test]
    fn length_rejection_with_a_correction_is_recoverable() {
        assert_eq!(
            recoverable_body(&rejection(LENGTH_REJECTION), Some("corrected")),
            Some("corrected")
        );
        assert_eq!(
            recoverable_body(
                &rejection(r#"{"message":"Input is too long for requested model."}"#),
                Some("corrected")
            ),
            Some("corrected")
        );
    }

    /// 分类没有指向长度预算时一律不重试：协议配对错误改尺寸不会通过，
    /// 未分类的拒绝更不该被当作长度问题去乱动 payload。
    #[test]
    fn non_length_rejections_are_never_recoverable() {
        assert!(
            recoverable_body(
                &rejection(r#"{"reason":"TOOL_USE_RESULT_MISMATCH"}"#),
                Some("corrected")
            )
            .is_none()
        );
        assert!(
            recoverable_body(
                &rejection(r#"{"reason":"INTERNAL_SERVER_ERROR"}"#),
                Some("corrected")
            )
            .is_none()
        );
    }

    /// 非上游拒绝（网络错误等）不进入恢复路径。
    #[test]
    fn non_upstream_errors_are_never_recoverable() {
        assert!(
            recoverable_body(&anyhow::anyhow!("connection reset"), Some("corrected")).is_none()
        );
    }

    /// 没有修正体就不重发——策略关闭，或修正后字节毫无变化。
    /// 后者若放行就是被禁止的盲目原样重发。
    #[test]
    fn without_a_correction_there_is_no_retry() {
        assert!(recoverable_body(&rejection(LENGTH_REJECTION), None).is_none());
    }

    fn stream_ctx(model: &str) -> StreamContext {
        StreamContext::new_with_thinking(
            model,
            0,
            false,
            std::collections::HashMap::new(),
            std::collections::HashSet::new(),
        )
    }

    /// 上游没发 contextUsageEvent 就是没有观测：不补零、不造样本。
    #[test]
    fn no_context_event_yields_no_observation() {
        assert!(context_observation(&stream_ctx("claude-sonnet-4")).is_none());
    }

    /// 观测必须把上游原值、猜测窗口和换算结果**并列**保留，
    /// 否则事后分不清误差来自上游还是来自那张写死的窗口表。
    #[test]
    fn observation_keeps_upstream_value_and_guess_side_by_side() {
        let mut ctx = stream_ctx("claude-sonnet-4");
        ctx.context_usage_percentage = Some(12.5);
        ctx.context_input_tokens = Some(25_000);
        let observation = context_observation(&ctx).expect("有百分比就应有观测");
        assert_eq!(observation["percentage"], 12.5);
        assert_eq!(observation["derivedInputTokens"], 25_000);
        assert_eq!(
            observation["guessedWindowTokens"],
            get_context_window_size("claude-sonnet-4")
        );
        assert_eq!(observation["model"], "claude-sonnet-4");
    }

    /// 脱敏结构原样带过来，且不得携带任何文本内容。
    #[test]
    fn observation_carries_redacted_shape_without_text() {
        let event = crate::kiro::model::events::ContextUsageEvent::from_payload(
            br#"{"contextUsagePercentage":5.0,"breakdown":{"historyTokens":900},"note":"PRIVATE"}"#,
        )
        .unwrap();
        let mut ctx = stream_ctx("claude-sonnet-4");
        ctx.context_usage_percentage = Some(event.context_usage_percentage);
        ctx.context_usage_shape = event.shape.clone();
        let observation = context_observation(&ctx).unwrap();
        assert_eq!(observation["shape"]["breakdown"]["historyTokens"], 900);
        assert!(!observation.to_string().contains("PRIVATE"));
    }

    fn upstream_rejection(status: u16, body: &str) -> anyhow::Error {
        crate::kiro::error::UpstreamRequestError::api(
            "非流式",
            StatusCode::from_u16(status).unwrap(),
            body,
        )
        .into()
    }

    #[test]
    fn bedrock_client_validation_errors_map_to_400() {
        // 客户端校验错误必须映射为 400（而非 5xx），否则会被 provider 当作上游
        // 瞬态错误触发冷却，放大成 503 风暴。
        //
        // 分类现在由 provider 在读取**原始报文**时一次完成并随类型携带下来；
        // 本测试因此构造 typed error，而不是像以前那样构造一条拼接字符串——
        // 在拼接串上分类正是被修掉的缺陷（串不是合法 JSON，字段确认必然失败）。
        for body in [
            // 精确 reason
            r#"{"reason":"TOOL_USE_RESULT_MISMATCH"}"#,
            // message 级特异短语（纯文本报文）
            "Expected toolResult blocks but found none",
        ] {
            let resp = map_provider_error(upstream_rejection(500, body));
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "报文 `{body}` 应映射为 400"
            );
        }
    }

    /// 回归：关键词只是被报文的其它字段回显时，不得再判成客户端校验错误。
    /// 旧实现在拼接字符串上做裸 `contains`，会把这种响应误杀成 400。
    #[test]
    fn echoed_validation_keyword_is_not_a_client_error() {
        let resp = map_provider_error(upstream_rejection(
            500,
            r#"{"reason":"INTERNAL_SERVER_ERROR","message":"tool output mentioned TOOL_USE_RESULT_MISMATCH"}"#,
        ));
        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "偶然提及不等于该 reason 成立，应按上游错误走 502 而非误杀为 400"
        );
    }

    #[test]
    fn content_length_rejection_maps_to_400_without_attributing_a_limit() {
        let resp = map_provider_error(upstream_rejection(
            400,
            r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#,
        ));
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn generic_upstream_error_still_maps_to_502() {
        // 回归：普通上游错误不应被新分支误伤，仍应是 502 BAD_GATEWAY。
        let resp = map_provider_error(anyhow::anyhow!("connection reset by peer"));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        // 回归：宽泛的 ValidationException 不再被当作客户端校验错误而误判为 400，
        // 仍按上游错误走 502（避免把可重试故障误杀）。
        let resp = map_provider_error(anyhow::anyhow!(
            "ValidationException: transient backend issue".to_string()
        ));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn upstream_rate_limit_maps_to_429_with_retry_after() {
        let err = crate::kiro::error::UpstreamRateLimitError::new(Some("1800".to_string()));
        let resp = map_provider_error(err.into());

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "1800");
    }

    #[test]
    fn upstream_rate_limit_drops_invalid_retry_after() {
        let err =
            crate::kiro::error::UpstreamRateLimitError::new(Some("not-a-retry-delay".to_string()));
        let resp = map_provider_error(err.into());

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get(header::RETRY_AFTER).is_none());
    }

    /// 长度拒绝稳定映射为 400，但**不得替上游断言是哪条限制**。
    ///
    /// 上游 0.9.0 的同名测试断言文案包含 "Context window is full"。本仓库不这么说：
    /// `CONTENT_LENGTH_EXCEEDS_THRESHOLD` 没有指明是总 body、单个字段、图片还是模型
    /// 上下文窗口，把它渲染成"上下文窗口已满"是替上游做了它没做的判断，会把排查引向
    /// 错误方向。合并 0.9.0 时改的是断言，不是实现。
    #[tokio::test]
    async fn typed_context_overflow_maps_to_stable_400() {
        let resp = map_provider_error(upstream_rejection(
            400,
            r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#,
        ));
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD"));
        assert!(
            message.contains("did not specify"),
            "必须如实说明上游没有指明是哪条限制：{message}"
        );
        assert!(
            !message.contains("Context window is full"),
            "不得替上游断言是上下文窗口：{message}"
        );
    }

    #[tokio::test]
    async fn generic_upstream_error_does_not_expose_raw_body() {
        let secret = "aws-account=123456789012 request-id=private-request";
        let resp = map_provider_error(anyhow::anyhow!(secret));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains(secret));
        assert!(body.contains("Upstream API request failed"));
    }

    #[test]
    fn non_stream_native_thinking_precedes_redacted_and_text() {
        let content = build_non_stream_content(
            true,
            "final answer".to_string(),
            "native thinking".to_string(),
            Some("real-signature".to_string()),
            vec!["encrypted-thinking".to_string()],
        );

        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "native thinking");
        assert_eq!(content[0]["signature"], "real-signature");
        assert_eq!(content[1]["type"], "redacted_thinking");
        assert_eq!(content[1]["data"], "encrypted-thinking");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "final answer");
    }

    #[test]
    fn non_stream_legacy_thinking_extraction_still_works_without_native_reasoning() {
        let content = build_non_stream_content(
            true,
            "<thinking>legacy thinking</thinking>\n\nfinal answer".to_string(),
            String::new(),
            None,
            Vec::new(),
        );

        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "legacy thinking");
        assert_eq!(
            content[0]["signature"],
            crate::anthropic::stream::THINKING_SIGNATURE_PLACEHOLDER
        );
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "final answer");
    }

    #[test]
    fn non_stream_native_thinking_downgrades_to_text_when_thinking_disabled() {
        let content = build_non_stream_content(
            false,
            String::new(),
            "native thinking fallback".to_string(),
            Some("ignored-signature".to_string()),
            vec!["ignored-redacted".to_string()],
        );

        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "native thinking fallback");
    }

    #[test]
    fn dynamic_models_do_not_synthesize_claude_thinking_alias() {
        let models = aggregate_available_models(vec![UpstreamModel {
            model_id: "claude-opus-5".to_string(),
            model_name: Some("Claude Opus 5".to_string()),
            description: None,
            token_limits: None,
        }]);
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();

        assert!(ids.contains(&"claude-opus-5"));
        assert!(!ids.contains(&"claude-opus-5-thinking"));
    }

    #[test]
    fn count_image_budget_handles_empty() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": []
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.total_b64_bytes, 0);
        assert_eq!(stats.largest_b64_bytes, 0);
    }

    #[test]
    fn count_image_budget_counts_inline_base64() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA1111"}},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBBBBBBBB"}},
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#).unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_b64_bytes, 18);
        assert_eq!(stats.largest_b64_bytes, 10);
    }

    #[test]
    fn count_image_budget_skips_url_only_images() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
    }

    #[test]
    fn dynamic_models_merge_metadata_and_do_not_use_input_limit_as_output_limit() {
        let models = aggregate_available_models(vec![
            UpstreamModel {
                model_id: "glm-5".to_string(),
                model_name: None,
                description: Some("first".to_string()),
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(200_000),
                    max_output_tokens: None,
                }),
            },
            UpstreamModel {
                model_id: "glm-5".to_string(),
                model_name: Some("GLM 5".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(1_000_000),
                    max_output_tokens: Some(32_000),
                }),
            },
        ]);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "GLM 5");
        assert_eq!(models[0].owned_by, "kiro");
        assert_eq!(models[0].context_window, 1_000_000);
        assert_eq!(models[0].max_tokens, 32_000);
    }

    #[test]
    fn custom_model_metadata_overrides_dynamic_collision() {
        let custom = crate::model::config::CustomModel {
            id: "gpt-next".to_string(),
            backend_id: "gpt-next".to_string(),
            display_name: Some("Configured GPT".to_string()),
            context_window: Some(500_000),
            max_tokens: Some(12_345),
            supports_reasoning: Some(true),
            owned_by: Some("configured-owner".to_string()),
        };
        let models = aggregate_available_models_with_custom(
            vec![UpstreamModel {
                model_id: "gpt-next".to_string(),
                model_name: Some("Upstream GPT".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(300_000),
                    max_output_tokens: Some(64_000),
                }),
            }],
            &[custom],
        );

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "Configured GPT");
        assert_eq!(models[0].owned_by, "configured-owner");
        assert_eq!(models[0].context_window, 500_000);
        assert_eq!(models[0].max_tokens, 12_345);
    }

    #[test]
    fn non_stream_usage_prefers_sanitized_provider_snapshot() {
        let fallback_cache = super::super::cache_metering::CacheUsage {
            cache_read: 25,
            cache_covered_est: 50,
            prompt_total_est: 100,
        };
        let provider = TokenUsage {
            uncached_input_tokens: 3,
            output_tokens: 11,
            cache_read_input_tokens: 7,
            cache_write_input_tokens: 4,
        };

        assert_eq!(
            resolve_non_stream_usage(100, Some(80), 9, fallback_cache, Some(provider)),
            (3, 11, 4, 7)
        );
    }

    #[test]
    fn non_stream_usage_falls_back_to_context_and_cache_split() {
        let cache_usage = super::super::cache_metering::CacheUsage {
            cache_read: 25,
            cache_covered_est: 50,
            prompt_total_est: 100,
        };

        assert_eq!(
            resolve_non_stream_usage(100, Some(80), 9, cache_usage, None),
            (40, 9, 20, 20)
        );
        assert_eq!(
            resolve_non_stream_usage(100, None, -9, Default::default(), None),
            (100, 0, 0, 0)
        );
    }

    #[test]
    fn max_tokens_must_be_positive() {
        assert!(validate_max_tokens(1).is_ok());
        assert!(validate_max_tokens(0).is_err());
        assert!(validate_max_tokens(-1).is_err());
    }
}
