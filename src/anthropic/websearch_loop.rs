//! Local web-search and private context-retrieval agentic loop.
//!
//! Server-only rounds execute web search or scoped artifact retrieval, append
//! paired history, and reconvert through the configured request pipeline.
//! Client tools terminate the local loop and are returned once. Private calls
//! remain local, while web search uses Anthropic server-tool presentation.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use axum::{
    body::{Body, to_bytes},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{FutureExt, StreamExt, stream};
use serde_json::{Value, json};
use tokio::{
    sync::mpsc,
    time::{Duration, Instant, interval_at},
};
use uuid::Uuid;

use crate::admin::trace_db::{TraceSink, outcome};
use crate::kiro::model::events::{Event, MeteringEvent, TokenUsage};
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::KiroProvider;
use crate::pipeline::artifacts::{ContextSession, is_internal_tool};
use crate::token;

use super::converter::{ConversionError, convert_request_with_pipeline, get_context_window_size};
use super::handlers::{
    KiroRouting, RequestTracer, TraceUsage, UsageRecordHook, UsageSource, last_attempt_outcome,
    map_provider_error,
};
use super::stream::{CompletedToolUse, SseEvent, ToolJsonAccumulator, ToolJsonAccumulatorError};
use super::types::{ErrorResponse, Message, MessagesRequest};
use super::websearch::{self, WebSearchResults};
use crate::model::config::ToolCompatibilityMode;

/// Maximum number of search rounds, to prevent an infinite loop if the upstream keeps asking to search
const MAX_WEB_SEARCH_ROUNDS: usize = 5;

/// A valid assistant turn after a tool result must contain either visible text or
/// another client tool call. Kiro occasionally closes a successful upstream stream
/// without either, which used to be serialized as `end_turn` and made Codex mark an
/// unfinished task complete. Retry once before surfacing an upstream error.
const MAX_EMPTY_TOOL_RESULT_RETRIES: usize = 1;

/// Bounded progress queue for the streamed agentic loop. Search result blocks
/// are small, so this is enough to provide backpressure without buffering an
/// unbounded response when the client is slow.
const WEB_SEARCH_PROGRESS_CAPACITY: usize = 32;

/// Keep a streamed response alive while an upstream model round or MCP call is
/// still running. The first `message_start` is emitted immediately; pings cover
/// any subsequent long-running operation.
const WEB_SEARCH_PING_INTERVAL_SECS: u64 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyToolResultDisposition {
    Accept,
    Retry,
    Fail,
}

/// Result of buffer-decoding one round of the upstream response
struct RoundOutcome {
    /// Accumulated assistant text
    text: String,
    /// Accumulated thinking / reasoning text (Kiro reasoningContentEvent).
    /// Surfaced out-of-band via render_json's `kiro_thinking` so Anthropic
    /// clients never see (and never replay) an unsigned thinking block.
    thinking: String,
    /// The complete tool_use for this round (name already restored via tool_name_map)
    tool_uses: Vec<CompletedToolUse>,
    /// Actual input tokens computed from contextUsageEvent
    context_input_tokens: Option<i32>,
    /// metadataEvent.tokenUsage 的单轮精确最终快照。
    provider_token_usage: Option<TokenUsage>,
    /// Cumulative credits from meteringEvent (sum of usage across rounds)
    credits: f64,
    /// 最近一次 meteringEvent 完整 payload（含 unit / unit_plural / usage）。
    /// 在 run_web_search_loop 出口处透传到响应 usage 字段；如果上游多次下发
    /// 则取最后一次（与 /v1/messages 非流 / 流式路径一致）。
    last_metering: Option<MeteringEvent>,
    /// stop_reason override (max_tokens / model_context_window_exceeded)
    stop_reason_override: Option<String>,
    /// Upstream read or tool-JSON error. This round cannot execute or forward
    /// any tool calls, including successfully decoded calls in a mixed round.
    stream_error: Option<String>,
    /// Preserve the strict accumulator's structured error classification.
    tool_json_error: Option<ToolJsonAccumulatorError>,
    /// Tool names declared to the upstream this round (original + shortened),
    /// taken from `ConversionResult::known_tool_names`. Used by the shared
    /// `<invoke>` text-leak fault tolerance so a leaked `<invoke name=...>` is only
    /// reclaimed when its name is a real declared tool.
    known_tool_names: std::collections::HashSet<String>,
    /// Short-name -> original-name map for this round, taken from
    /// `ConversionResult::tool_name_map`. Used to restore the original tool name when a
    /// leaked `<invoke>` carries a shortened (>63 char) tool name.
    tool_name_map: std::collections::HashMap<String, String>,
}

impl RoundOutcome {
    /// 解析本次 provider 调用的 token 用量；精确 metadata 缺失时只回退本轮。
    fn resolved_token_usage(&self, fallback_input_tokens: i32) -> TokenUsage {
        if let Some(usage) = self.provider_token_usage {
            return usage.sanitized();
        }

        let mut output = Vec::new();
        if !self.thinking.is_empty() {
            output.push(json!({"type": "thinking", "thinking": self.thinking}));
        }
        if !self.text.is_empty() {
            output.push(json!({"type": "text", "text": self.text}));
        }
        output.extend(
            self.tool_uses
                .iter()
                .map(CompletedToolUse::to_anthropic_block),
        );

        TokenUsage {
            uncached_input_tokens: self
                .context_input_tokens
                .unwrap_or(fallback_input_tokens)
                .max(0),
            output_tokens: token::estimate_output_tokens(&output),
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
        }
    }
}

/// Normalize model-produced Web Search input into one non-empty query.
///
/// Codex-compatible providers can emit `query`, `search_query`, `q`, a
/// `queries` array, or wrap the text in `text`/`value`. Kiro's MCP
/// endpoint accepts only one string in `arguments.query`.
fn normalized_query_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let query = s.trim();
            (!query.is_empty()).then(|| query.to_string())
        }
        Value::Array(values) => values.iter().find_map(normalized_query_value),
        Value::Object(object) => ["query", "search_query", "q", "text", "value"]
            .iter()
            .find_map(|key| object.get(*key).and_then(normalized_query_value)),
        _ => None,
    }
}

/// Extract a usable Web Search query from a model tool-use input.
fn tool_query(tu: &CompletedToolUse) -> Option<String> {
    ["query", "search_query", "q", "queries"]
        .iter()
        .find_map(|key| tu.input.get(*key).and_then(normalized_query_value))
        .or_else(|| normalized_query_value(&tu.input))
}

fn log_invalid_web_search_input(tu: &CompletedToolUse) {
    let (input_kind, input_details) = match &tu.input {
        Value::Object(object) => {
            let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
            keys.sort_unstable();
            ("object", keys.join(","))
        }
        Value::Array(values) => ("array", format!("len={}", values.len())),
        Value::String(value) => ("string", format!("len={}", value.chars().count())),
        Value::Number(_) => ("number", String::new()),
        Value::Bool(_) => ("bool", String::new()),
        Value::Null => ("null", String::new()),
    };
    tracing::warn!(
        tool_use_id = %tu.id,
        input_kind,
        input_details = %input_details,
        "web_search tool input has no usable non-empty query; returning an empty result without calling MCP"
    );
}

fn log_normalized_web_search_query(tu: &CompletedToolUse, query: &str) {
    tracing::info!(
        tool_use_id = %tu.id,
        query_chars = query.chars().count(),
        "web_search normalized a non-empty query before calling Kiro MCP"
    );
}

/// Decides whether this round should keep searching (enter the next loop round)
///
/// Continue condition: every tool_use this round is web_search (at least one) and the round limit has not been reached.
/// As soon as a client tool such as exec is mixed in, there is no tool_use at all, or the limit is reached, it stops and flushes (exec is never swallowed).
fn should_search_round(round_idx: usize, tool_uses: &[CompletedToolUse]) -> bool {
    let only_web_search = !tool_uses.is_empty() && tool_uses.iter().all(|t| t.name == "web_search");
    only_web_search && round_idx < MAX_WEB_SEARCH_ROUNDS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolRoundDisposition {
    Continue,
    Flush,
    ContextUnavailable,
    ContextLimitExceeded,
    SearchLimitExceeded,
}

impl ToolRoundDisposition {
    fn error(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::ContextUnavailable => Some((
                "context_unavailable",
                "Private context retrieval is unavailable for this request.",
            )),
            Self::ContextLimitExceeded => Some((
                "context_round_limit_exceeded",
                "Private context retrieval reached its configured round limit before the model completed its response.",
            )),
            Self::SearchLimitExceeded => Some((
                "server_tool_round_limit_exceeded",
                "Web search reached its round limit while private context retrieval was still pending.",
            )),
            Self::Continue | Self::Flush => None,
        }
    }
}

fn is_server_tool(name: &str) -> bool {
    name == "web_search"
        || is_internal_tool(name)
        || crate::pipeline::tool_catalog::is_catalog_tool(name)
        || crate::pipeline::chunked_map::is_map_tool(name)
}

/// 目录轮次上限。模型可以反复列目录与索取 schema，但不能无限循环下去。
const MAX_CATALOG_ROUNDS: usize = 8;

/// 每块从属调用的输出上限。分块处理是抽取式的，不需要长输出。
const CHUNK_MAP_MAX_TOKENS: i32 = 2048;

/// Client calls must be handed back before another model round: their results
/// are not available locally, so continuing would create unpaired history.
fn tool_round_disposition(
    tool_uses: &[CompletedToolUse],
    context_limit: Option<usize>,
    context_rounds: usize,
    search_rounds: usize,
    catalog_rounds: usize,
) -> ToolRoundDisposition {
    if tool_uses.is_empty() || tool_uses.iter().any(|tool| !is_server_tool(&tool.name)) {
        return ToolRoundDisposition::Flush;
    }
    // 目录调用由网关本地执行，与检索同样需要继续下一轮；但轮次有上限。
    if tool_uses
        .iter()
        .any(|tool| crate::pipeline::tool_catalog::is_catalog_tool(&tool.name))
    {
        return if catalog_rounds < MAX_CATALOG_ROUNDS {
            ToolRoundDisposition::Continue
        } else {
            ToolRoundDisposition::ContextLimitExceeded
        };
    }
    let has_context = tool_uses.iter().any(|tool| {
        is_internal_tool(&tool.name) || crate::pipeline::chunked_map::is_map_tool(&tool.name)
    });
    if has_context {
        let Some(limit) = context_limit else {
            return ToolRoundDisposition::ContextUnavailable;
        };
        if context_rounds >= limit {
            return ToolRoundDisposition::ContextLimitExceeded;
        }
        if search_rounds >= MAX_WEB_SEARCH_ROUNDS
            && tool_uses.iter().any(|tool| tool.name == "web_search")
        {
            return ToolRoundDisposition::SearchLimitExceeded;
        }
        ToolRoundDisposition::Continue
    } else if should_search_round(search_rounds, tool_uses) {
        ToolRoundDisposition::Continue
    } else {
        ToolRoundDisposition::Flush
    }
}

/// Recover declared calls before deciding whether to continue. In particular,
/// a client call leaked into text alongside a private call must not be lost by
/// entering another server-only round.
fn reclaim_round_tool_uses(round: &mut RoundOutcome) {
    if round.text.is_empty() {
        return;
    }
    let structured_keys: std::collections::HashSet<(String, String)> = round
        .tool_uses
        .iter()
        .map(|tool| (tool.name.clone(), canonical_input_key(&tool.input)))
        .collect();
    let mut text = String::new();
    for block in super::stream::extract_invoke_content_blocks(
        &round.text,
        &round.known_tool_names,
        &round.tool_name_map,
    ) {
        if block["type"] == "tool_use" {
            let Some(name) = block["name"].as_str() else {
                continue;
            };
            let Some(id) = block["id"].as_str() else {
                continue;
            };
            let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
            if !structured_keys.contains(&(name.to_string(), canonical_input_key(&input))) {
                round.tool_uses.push(CompletedToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input,
                });
            }
        } else if let Some(part) = block["text"].as_str() {
            text.push_str(part);
        }
    }
    round.text = text;
}

/// Whether the request is the continuation immediately following a tool result.
fn last_message_has_tool_result(payload: &MessagesRequest) -> bool {
    let Some(last) = payload.messages.last() else {
        return false;
    };
    if last.role != "user" {
        return false;
    }
    last.content.as_array().is_some_and(|blocks| {
        blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
    })
}

/// Decide how to handle a successful upstream round after tool output. Reasoning
/// by itself is intentionally not enough: Codex needs either assistant text (a
/// real final answer) or a client tool call to keep the task lifecycle sound.
fn empty_tool_result_disposition(
    payload: &MessagesRequest,
    round: &RoundOutcome,
    retries: usize,
) -> EmptyToolResultDisposition {
    let is_invalid_empty_continuation = last_message_has_tool_result(payload)
        && round.text.trim().is_empty()
        && round.tool_uses.is_empty()
        && round.stop_reason_override.is_none();
    if !is_invalid_empty_continuation {
        EmptyToolResultDisposition::Accept
    } else if retries < MAX_EMPTY_TOOL_RESULT_RETRIES {
        EmptyToolResultDisposition::Retry
    } else {
        EmptyToolResultDisposition::Fail
    }
}

/// Buffer-decode one round of the upstream streaming response
async fn decode_round(
    response: reqwest::Response,
    model: &str,
    tool_name_map: &std::collections::HashMap<String, String>,
    mark_first_token: impl Fn(),
) -> RoundOutcome {
    let mut body_stream = response.bytes_stream();
    let mut decoder = EventStreamDecoder::new();

    let mut text = String::new();
    let mut thinking = String::new();
    let mut tool_accumulator = ToolJsonAccumulator::new();
    let mut tool_json_error = None;
    // Retain first-seen order even when interleaved calls complete out of order.
    let mut completed: std::collections::HashMap<String, Option<CompletedToolUse>> =
        std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut context_input_tokens: Option<i32> = None;
    let mut provider_token_usage: Option<TokenUsage> = None;
    let mut credits = 0.0;
    let mut last_metering: Option<MeteringEvent> = None;
    let mut stop_reason_override: Option<String> = None;
    let mut stream_error = None;

    while let Some(chunk) = body_stream.next().await {
        let chunk = match chunk {
            Ok(c) => {
                mark_first_token();
                c
            }
            Err(e) => {
                tracing::error!("web_search loop failed to read the response stream: {}", e);
                stream_error = Some(e.to_string());
                break;
            }
        };
        if let Err(e) = decoder.feed(&chunk) {
            tracing::warn!("buffer overflow: {}", e);
        }
        for result in decoder.decode_iter() {
            let frame = match result {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("failed to decode event: {}", e);
                    continue;
                }
            };
            let event = match Event::from_frame(frame) {
                Ok(ev) => ev,
                Err(_) => continue,
            };
            match event {
                Event::AssistantResponse(resp) => text.push_str(&resp.content),
                Event::ReasoningContent(r) => {
                    if let Some(t) = &r.text {
                        thinking.push_str(t);
                    }
                }
                Event::ToolUse(tu) => {
                    let entry = completed.entry(tu.tool_use_id.clone()).or_insert_with(|| {
                        order.push(tu.tool_use_id.clone());
                        None
                    });
                    match tool_accumulator.push(&tu, tool_name_map) {
                        Ok(Some(tool)) => *entry = Some(tool),
                        Ok(None) => {}
                        Err(error) => {
                            if tool_json_error.is_none() {
                                tool_json_error = Some(error);
                            }
                        }
                    }
                }
                Event::Metadata(metadata) => {
                    if let Some(usage) = metadata.token_usage {
                        // 单条流内重复 metadata 是快照，取最后一份。
                        provider_token_usage = Some(usage.sanitized());
                    }
                }
                Event::ContextUsage(cu) => {
                    let window = get_context_window_size(model);
                    let actual = (cu.context_usage_percentage * (window as f64) / 100.0) as i32;
                    context_input_tokens = Some(actual);
                    if cu.context_usage_percentage >= 100.0 {
                        stop_reason_override = Some("model_context_window_exceeded".to_string());
                    }
                }
                Event::Metering(m) => {
                    credits += m.usage;
                    last_metering = Some(m.clone());
                }
                Event::Exception { exception_type, .. } => {
                    if exception_type == "ContentLengthExceededException" {
                        stop_reason_override = Some("max_tokens".to_string());
                    }
                }
                _ => {}
            }
        }
    }

    if tool_json_error.is_none()
        && let Err(error) = tool_accumulator.finish()
    {
        tool_json_error = Some(error);
    }
    if let Some(error) = &tool_json_error {
        stream_error = Some(error.message());
    }
    let tool_uses = order
        .into_iter()
        .filter_map(|id| completed.remove(&id).flatten())
        .collect();

    // 剥离混入文本的字面 <tool_use> XML 泄漏（与非流式同口径）。
    let text = crate::kiro::model::events::strip_tool_use_xml_leaks(&text);

    RoundOutcome {
        text,
        thinking,
        tool_uses,
        context_input_tokens,
        provider_token_usage,
        credits,
        last_metering,
        stop_reason_override,
        stream_error,
        tool_json_error,
        // Populated by the caller (run_round), which holds ConversionResult::known_tool_names.
        known_tool_names: std::collections::HashSet::new(),
        // Populated by the caller (run_round), which holds ConversionResult::tool_name_map.
        tool_name_map: std::collections::HashMap::new(),
    }
}

/// A failed round plus any usage that can still be attributed to its provider call.
struct RoundFailure {
    response: Response,
    error_type: &'static str,
    error_message: String,
    credential_id: u64,
    token_usage: Option<TokenUsage>,
    usage_from_provider: bool,
    credits: f64,
}

/// Run one upstream round (convert + streaming request + buffer decode).
///
/// Usage recording belongs to the outer loop so every terminal path writes exactly one
/// aggregate. A failure after a provider call carries the usage already observed in that call.
async fn run_round(
    provider: &Arc<KiroProvider>,
    payload: &MessagesRequest,
    fallback_input_tokens: i32,
    tracer: &RequestTracer,
    routing: &KiroRouting,
    tool_compatibility_mode: ToolCompatibilityMode,
) -> Result<(RoundOutcome, u64), RoundFailure> {
    let config = &provider.token_manager().config().request_pipeline;
    let conversion = match convert_request_with_pipeline(payload, tool_compatibility_mode, config) {
        Ok(c) => c,
        Err(e) => {
            let (et, msg) = match &e {
                ConversionError::InvalidModel(reason) => (
                    "invalid_request_error",
                    format!("invalid model id: {}", reason),
                ),
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "message list is empty".to_string())
                }
                ConversionError::InvalidMessageSequence(reason) => (
                    "invalid_request_error",
                    format!("invalid message sequence: {}", reason),
                ),
                ConversionError::UnsupportedToolMapping(reason) => (
                    "invalid_request_error",
                    format!("unsupported tool mapping: {}", reason),
                ),
            };
            let error_message = msg.clone();
            return Err(RoundFailure {
                response: (StatusCode::BAD_REQUEST, Json(ErrorResponse::new(et, msg)))
                    .into_response(),
                error_type: outcome::BAD_REQUEST,
                error_message,
                credential_id: 0,
                token_usage: None,
                usage_from_provider: false,
                credits: 0.0,
            });
        }
    };

    let kiro_request = KiroRequest {
        conversation_state: conversion.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion.additional_model_request_fields,
    };
    let request_body = match crate::pipeline::serialize_request(payload, &kiro_request, config) {
        Ok(b) => b,
        Err(e) => {
            let error_message = format!("failed to serialize request: {}", e);
            return Err(RoundFailure {
                response: (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::new("internal_error", error_message.clone())),
                )
                    .into_response(),
                error_type: outcome::UNKNOWN,
                error_message,
                credential_id: 0,
                token_usage: None,
                usage_from_provider: false,
                credits: 0.0,
            });
        }
    };

    let call_result = match provider
        .call_api_stream(
            &request_body,
            Some(tracer),
            routing.group.as_deref(),
            routing.sticky,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let local_limit = e
                .downcast_ref::<crate::pipeline::LocalPayloadLimit>()
                .is_some();
            let error_type = if local_limit {
                outcome::BAD_REQUEST
            } else {
                last_attempt_outcome(tracer).unwrap_or(outcome::UNKNOWN)
            };
            let error_message = e.to_string();
            let token_usage = if local_limit {
                None
            } else {
                Some(TokenUsage {
                    uncached_input_tokens: fallback_input_tokens.max(0),
                    ..TokenUsage::default()
                })
            };
            return Err(RoundFailure {
                response: map_provider_error(e),
                error_type,
                error_message,
                credential_id: 0,
                token_usage,
                usage_from_provider: false,
                credits: 0.0,
            });
        }
    };
    let credential_id = call_result.credential_id;
    let mut outcome = decode_round(
        call_result.response,
        &payload.model,
        &conversion.tool_name_map,
        || tracer.mark_first_token(),
    )
    .await;
    // Carry the declared tool names (original + shortened) so the flush step can run the
    // shared `<invoke>` text-leak fault tolerance with a correct tool-table guard.
    outcome.known_tool_names = conversion.known_tool_names;
    // Carry the short->original tool name map so reclaimed <invoke> names get restored.
    outcome.tool_name_map = conversion.tool_name_map;
    if let Some(usage) = outcome.provider_token_usage {
        tracer.on_native_usage(usage);
    }
    finish_decoded_round(outcome, credential_id, fallback_input_tokens)
}

/// This gate runs before any local execution or client presentation. A bad call
/// invalidates the entire buffered round, while retaining observed usage.
fn finish_decoded_round(
    mut outcome: RoundOutcome,
    credential_id: u64,
    fallback_input_tokens: i32,
) -> Result<(RoundOutcome, u64), RoundFailure> {
    if let Some(error_message) = outcome.stream_error.take() {
        // The stream is partial and cannot re-enter the search loop, but any final metadata
        // snapshot/credits observed before the cut still belong to this real provider call.
        let token_usage = outcome.resolved_token_usage(fallback_input_tokens);
        let (response_error_type, response_message) = match &outcome.tool_json_error {
            Some(error) => (error.error_type(), error.message()),
            None => (
                "upstream_error",
                "Upstream response stream ended unexpectedly during the server-tool loop."
                    .to_string(),
            ),
        };
        return Err(RoundFailure {
            response: (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(response_error_type, response_message)),
            )
                .into_response(),
            error_type: outcome::STREAM_INTERRUPTED,
            error_message,
            credential_id,
            token_usage: Some(token_usage),
            usage_from_provider: outcome.provider_token_usage.is_some(),
            credits: outcome.credits,
        });
    }
    Ok((outcome, credential_id))
}

/// Continue a completed server-only round with exact tool-use/result pairing,
/// including its reasoning. Only web-search presentation is client-visible.
/// `searched` has one entry per tool call; private entries are `None`.
fn append_server_round(
    payload: &mut MessagesRequest,
    round: &RoundOutcome,
    tool_results: Vec<Value>,
    searched: &[Option<WebSearchResults>],
    presentation: &mut Vec<Value>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        round
            .tool_uses
            .iter()
            .all(|tool| is_server_tool(&tool.name)),
        "cannot continue a server round with pending client tool calls"
    );
    anyhow::ensure!(
        tool_results.len() == round.tool_uses.len()
            && round
                .tool_uses
                .iter()
                .zip(&tool_results)
                .all(|(tool, result)| {
                    result["type"] == "tool_result" && result["tool_use_id"] == tool.id
                }),
        "server tool calls and results must be paired before continuing"
    );
    // Kiro history requires every assistant tool_use to have a user tool_result.
    let mut assistant_content: Vec<Value> = Vec::new();
    if !round.thinking.is_empty() {
        assistant_content.push(json!({"type": "thinking", "thinking": round.thinking}));
    }
    if !round.text.is_empty() {
        assistant_content.push(json!({"type": "text", "text": round.text}));
    }
    for tu in &round.tool_uses {
        assistant_content.push(tu.to_anthropic_block());
    }
    payload.messages.push(Message {
        role: "assistant".to_string(),
        content: Value::Array(assistant_content),
    });

    for (tu, results) in round.tool_uses.iter().zip(searched.iter()) {
        if tu.name != "web_search" {
            continue;
        }
        let query = tool_query(tu).unwrap_or_default();

        // Client presentation: server_tool_use + web_search_tool_result (Contract A)
        let (srv_id, _mcp) = websearch::create_mcp_request(&query);
        presentation.push(json!({
            "type": "server_tool_use", "id": srv_id, "name": "web_search",
            "input": {"query": query}
        }));
        // Contract A: web_search_tool_result has only type + content (no tool_use_id), consistent with generate_websearch_events
        presentation.push(json!({
            "type": "web_search_tool_result",
            "content": build_result_block(results)
        }));
    }
    payload.messages.push(Message {
        role: "user".to_string(),
        content: Value::Array(tool_results),
    });
    Ok(())
}

/// 执行分块处理：切分 → 每块一次真实上游轮次（同模型）→ 带区间返回。
///
/// **合并不在这里发生**：各块结果一起返回给模型，由模型在自己的上下文里关联，所以只有
/// map 阶段是碎片化的。每块都是一次计费轮次，用量与普通轮次同样经 tracer 记录。
async fn execute_chunked_map(
    provider: &Arc<KiroProvider>,
    context: Option<&ContextSession>,
    tool: &CompletedToolUse,
    model: &str,
    tracer: &RequestTracer,
    routing: &KiroRouting,
    tool_compatibility_mode: ToolCompatibilityMode,
) -> Value {
    use crate::pipeline::chunked_map;
    let error = |message: String| {
        json!({"type":"tool_result","tool_use_id":tool.id,"is_error":true,
            "content": json!({"error":"chunked_map_error","message":message}).to_string()})
    };
    let config = &provider
        .token_manager()
        .config()
        .request_pipeline
        .chunked_map;
    let Some(context) = context else {
        return error("chunked map requires an active context artifact session".into());
    };
    let (Some(artifact_id), Some(instruction)) = (
        tool.input.get("artifact_id").and_then(Value::as_str),
        tool.input.get("instruction").and_then(Value::as_str),
    ) else {
        return error("artifact_id and instruction are required".into());
    };
    let text = match context.artifact_text(artifact_id) {
        Ok(text) => text,
        Err(e) => return error(e.to_string()),
    };
    let ranges = match chunked_map::plan_chunks(&text, config.chunk_bytes, config.max_chunks) {
        Ok(ranges) => ranges,
        Err(e) => return error(e.to_string()),
    };

    let mut outputs = Vec::with_capacity(ranges.len());
    for range in ranges {
        // 让出执行权：断开的 SSE 接收端应能在下一块开始前取消。
        tokio::task::yield_now().await;
        let request = chunked_map::chunk_request(
            model,
            instruction,
            range,
            text.len(),
            &text[range.0..range.1],
            CHUNK_MAP_MAX_TOKENS,
        );
        let fallback = token::count_all_tokens(
            request.model.clone(),
            request.system.clone(),
            request.messages.clone(),
            None,
        ) as i32;
        match run_round(
            provider,
            &request,
            fallback,
            tracer,
            routing,
            tool_compatibility_mode,
        )
        .await
        {
            Ok((round, _)) => outputs.push(chunked_map::ChunkOutput {
                range,
                output: round.text,
            }),
            // 中途失败不静默丢块：直接报错，否则模型会以为它看过全部区间。
            Err(failure) => {
                return error(format!(
                    "chunk {}-{} failed: {}",
                    range.0, range.1, failure.error_message
                ));
            }
        }
    }
    json!({"type":"tool_result","tool_use_id":tool.id,
        "content": chunked_map::build_result(artifact_id, text.len(), &outputs).to_string()})
}

fn execute_catalog_tool(
    catalog: Option<&mut crate::pipeline::tool_catalog::CatalogSession>,
    tool: &CompletedToolUse,
) -> Value {
    let result = match catalog {
        Some(session) => session.execute(&tool.name, &tool.input),
        None => Err(anyhow::anyhow!("tool catalog session is unavailable")),
    };
    match result {
        Ok(value) => json!({
            "type": "tool_result", "tool_use_id": tool.id, "content": value.to_string()
        }),
        Err(error) => json!({
            "type": "tool_result", "tool_use_id": tool.id, "is_error": true,
            "content": json!({"error": "tool_catalog_error", "message": error.to_string()}).to_string()
        }),
    }
}

fn execute_context_tool(
    context: Option<&ContextSession>,
    tool: &CompletedToolUse,
    context_rounds: usize,
) -> Value {
    let result = match context {
        Some(context) if context_rounds < context.max_rounds() => {
            context.execute(&tool.name, &tool.input)
        }
        Some(_) => Err(anyhow::anyhow!(
            "private context retrieval round limit reached"
        )),
        None => Err(anyhow::anyhow!(
            "private context retrieval session is unavailable"
        )),
    };
    match result {
        Ok(value) => json!({
            "type": "tool_result", "tool_use_id": tool.id, "content": value.to_string()
        }),
        Err(error) => json!({
            "type": "tool_result", "tool_use_id": tool.id, "is_error": true,
            "content": json!({"error": "context_retrieval_error", "message": error.to_string()}).to_string()
        }),
    }
}

/// Converts search results into an array of web_search_result blocks (Contract A fields)
fn build_result_block(results: &Option<WebSearchResults>) -> Vec<Value> {
    match results {
        Some(r) => r
            .results
            .iter()
            .map(|item| {
                let page_age = item.published_date.and_then(|ms| {
                    chrono::DateTime::from_timestamp_millis(ms)
                        .map(|dt| dt.format("%B %-d, %Y").to_string())
                });
                json!({
                    "type": "web_search_result",
                    "title": item.title,
                    "url": item.url,
                    "encrypted_content": item.snippet.clone().unwrap_or_default(),
                    "page_age": page_age
                })
            })
            .collect(),
        None => vec![],
    }
}

/// Classify visible search/client calls while excluding private context calls.
/// Order is preserved within each group.
fn partition_tool_uses(
    tool_uses: &[CompletedToolUse],
) -> (Vec<&CompletedToolUse>, Vec<&CompletedToolUse>) {
    let mut web = Vec::new();
    let mut client = Vec::new();
    for tu in tool_uses {
        if tu.name == "web_search" {
            web.push(tu);
        } else if !is_internal_tool(&tu.name) {
            client.push(tu);
        }
    }
    (web, client)
}

/// Resolves the final `stop_reason` for a flushed web_search-loop response.
///
/// Inputs:
/// - `override_reason`: an upstream-forced terminal reason (max_tokens /
///   model_context_window_exceeded). When present it always wins.
/// - `client_uses_empty`: whether the round had NO structured client tool_use.
/// - `content`: the FINAL flushed content (after the `<invoke>` fault tolerance may have
///   reclaimed a structured tool_use out of the assistant text).
///
/// Rules:
/// 1. An upstream override always wins (verbatim).
/// 2. Otherwise, if the final content contains a real (non-web_search) `tool_use` block,
///    the reason MUST be `tool_use` — this covers BOTH the structured case and the
///    reclaimed-from-text case (the common leak: model emits the call as text, so
///    `client_uses_empty` is true but a tool_use was reclaimed into `content`).
/// 3. Otherwise fall back to the structured signal: `tool_use` if the round had a client
///    tool_use, else `end_turn` (web_search-only rounds end as end_turn).
fn resolve_flush_stop_reason(
    override_reason: Option<&str>,
    client_uses_empty: bool,
    content: &[Value],
) -> String {
    if let Some(r) = override_reason {
        return r.to_string();
    }
    let has_client_tool_use = content.iter().any(|c| {
        c["type"] == "tool_use" && c["name"].as_str().is_some_and(|name| !is_server_tool(name))
    });
    if has_client_tool_use || !client_uses_empty {
        "tool_use".to_string()
    } else {
        "end_turn".to_string()
    }
}

/// Builds the final flush content with the web_search invariant baked in:
/// - any web_search tool_use becomes a `server_tool_use` + `web_search_tool_result`
///   presentation pair (NEVER a raw `tool_use`, which the Codex host rejects);
/// - client tools (exec, get_time, ...) are returned verbatim as raw `tool_use`.
///
/// `searched` corresponds one-to-one (same order) to `tool_uses`; entries for
/// web_search carry the already-completed search results, client-tool entries
/// are ignored (typically None).
///
/// `known_tool_names` is the set of tool names declared by the current request
/// (client short/long names). It is used to run the SAME `<invoke>` text-leak fault
/// tolerance as the streaming path (`stream.rs`): when the upstream model degrades
/// and emits a literal `<invoke name="...">...</invoke>` inside its assistant TEXT,
/// we reclaim it into a structured `tool_use` instead of passing the raw XML through.
/// The web_search loop builds its own SSE/content and historically bypassed that
/// fault tolerance entirely — this is the fix.
/// Canonical, order-independent key for a tool_use `input` JSON value, used to
/// detect that a reclaimed-from-text tool_use is identical to a structured one.
/// `serde_json::Value`'s `Map` is a BTreeMap (or preserves order when the
/// `preserve_order` feature is on); to be robust we serialize via a BTreeMap so
/// key order never affects equality.
fn canonical_input_key(input: &Value) -> String {
    match input {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            serde_json::to_string(&sorted).unwrap_or_else(|_| input.to_string())
        }
        _ => input.to_string(),
    }
}

fn build_flush_content(
    presentation: Vec<Value>,
    text: &str,
    tool_uses: &[CompletedToolUse],
    searched: &[Option<WebSearchResults>],
    known_tool_names: &std::collections::HashSet<String>,
    tool_name_map: &std::collections::HashMap<String, String>,
) -> Vec<Value> {
    let mut content: Vec<Value> = presentation;
    if !text.is_empty() {
        // Run the shared one-shot `<invoke>` sniffer: splits `text` into a sequence of
        // text blocks + reclaimed structured tool_use blocks (same safety gates as the
        // streaming fault tolerance). For clean text with no leaked `<invoke>`, this
        // returns a single text block identical to the old behavior.
        //
        // Server tools are reclaimed before routing, never as client calls at
        // flush time. Check original names too, so aliases cannot bypass this.
        let reclaim_tools: std::collections::HashSet<String> = known_tool_names
            .iter()
            .filter(|name| {
                let original = tool_name_map.get(*name).unwrap_or(*name);
                !is_server_tool(original)
            })
            .cloned()
            .collect();
        // DEDUP GUARD: a degraded model can emit BOTH a leaked literal `<invoke>` in the
        // text AND the matching structured tool_use in `tool_uses`. Emitting both would
        // make the host execute the same command twice. Suppress any reclaimed-from-text
        // tool_use whose (name + canonical input) already appears in the structured
        // `tool_uses` for this round. Text blocks (and distinct tool_uses) are kept as-is.
        let structured_keys: std::collections::HashSet<(String, String)> = tool_uses
            .iter()
            .filter(|t| !is_server_tool(&t.name))
            .map(|t| (t.name.clone(), canonical_input_key(&t.input)))
            .collect();
        for block in
            super::stream::extract_invoke_content_blocks(text, &reclaim_tools, tool_name_map)
        {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let key = (
                    name.to_string(),
                    block
                        .get("input")
                        .map(canonical_input_key)
                        .unwrap_or_default(),
                );
                if structured_keys.contains(&key) {
                    // identical to a structured tool_use already emitted below -> drop the
                    // reclaimed duplicate (avoid double execution).
                    continue;
                }
            }
            content.push(block);
        }
    }
    for (idx, tu) in tool_uses.iter().enumerate() {
        if tu.name == "web_search" {
            // INVARIANT: present as server_tool_use + web_search_tool_result,
            // never as a raw tool_use.
            let query = tool_query(tu).unwrap_or_default();
            let (srv_id, _mcp) = websearch::create_mcp_request(&query);
            content.push(json!({
                "type": "server_tool_use", "id": srv_id, "name": "web_search",
                "input": {"query": query}
            }));
            let results: &Option<WebSearchResults> = searched.get(idx).unwrap_or(&None);
            content.push(json!({
                "type": "web_search_tool_result",
                "content": build_result_block(results)
            }));
        } else if !is_internal_tool(&tu.name) {
            // Client tool (exec, get_time, ...): returned to the client verbatim.
            content.push(tu.to_anthropic_block());
        }
    }
    content
}

fn record_aggregated_usage(
    hook: &UsageRecordHook,
    credential_id: u64,
    usage: TokenUsage,
    credits: f64,
    status: &str,
) {
    let usage = usage.sanitized();
    hook.record(
        credential_id,
        usage.uncached_input_tokens,
        usage.output_tokens,
        usage.cache_write_input_tokens,
        usage.cache_read_input_tokens,
        credits,
        status,
    );
}

/// Cancellation-safe, exactly-once accounting for the multi-round search loop.
/// The snapshot is updated after every completed provider round, so cancelling
/// a later MCP/provider await still records all usage and trace attempts already
/// observed.
struct WebSearchUsageSettlement {
    hook: UsageRecordHook,
    tracer: Option<Arc<RequestTracer>>,
    credential_id: u64,
    usage: TokenUsage,
    credits: f64,
    settled: bool,
    /// 多轮聚合后的 usage 来源：全部轮次都有上游精确值才算 provider，
    /// 任一轮回退到本地估算即降级为 none（web_search 回退不走 CacheMeter，缓存恒为 0）。
    source: UsageSource,
}

impl WebSearchUsageSettlement {
    fn new(hook: UsageRecordHook, tracer: Arc<RequestTracer>) -> Self {
        Self {
            hook,
            tracer: Some(tracer),
            credential_id: 0,
            usage: TokenUsage::default(),
            credits: 0.0,
            settled: false,
            source: UsageSource::Unknown,
        }
    }

    #[cfg(test)]
    fn without_trace(hook: UsageRecordHook) -> Self {
        Self {
            hook,
            tracer: None,
            credential_id: 0,
            usage: TokenUsage::default(),
            credits: 0.0,
            settled: false,
            source: UsageSource::Unknown,
        }
    }

    fn add(&mut self, credential_id: u64, usage: TokenUsage, credits: f64) {
        if credential_id != 0 {
            self.credential_id = credential_id;
        }
        self.usage = self.usage.saturating_add(usage);
        if credits.is_finite() && credits > 0.0 {
            self.credits += credits;
        }
    }

    /// 记录本轮 usage 是否来自上游精确值。
    fn note_source(&mut self, from_provider: bool) {
        self.source = match (self.source, from_provider) {
            (UsageSource::Unknown, true) => UsageSource::Provider,
            (UsageSource::Provider, true) => UsageSource::Provider,
            _ => UsageSource::None,
        };
    }

    fn add_failure(&mut self, failure: &RoundFailure) {
        self.add(
            failure.credential_id,
            failure.token_usage.unwrap_or_default(),
            failure.credits,
        );
        // A preflight failure has no model round to classify. An attempted or
        // interrupted provider call with estimates invalidates native-only
        // aggregate attribution even if all earlier rounds were native.
        if failure.token_usage.is_some() {
            self.note_source(failure.usage_from_provider);
        }
    }

    fn usage(&self) -> TokenUsage {
        self.usage.sanitized()
    }

    fn finish(
        &mut self,
        usage_status: &str,
        trace_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
    ) {
        if self.settled {
            return;
        }
        record_aggregated_usage(
            &self.hook,
            self.credential_id,
            self.usage,
            self.credits,
            usage_status,
        );
        if let Some(tracer) = &self.tracer {
            finalize_aggregated_trace(
                tracer,
                trace_status,
                error_type,
                error_message,
                self.usage,
                self.credits,
                self.source,
            );
        }
        self.settled = true;
    }
}

impl Drop for WebSearchUsageSettlement {
    fn drop(&mut self) {
        if !self.settled {
            self.finish(
                "error",
                "interrupted",
                Some(outcome::STREAM_INTERRUPTED),
                Some("web_search loop was cancelled before completion"),
            );
        }
    }
}

#[derive(Debug)]
struct PendingWebSearch {
    block_index: i32,
}

/// Emits one coherent Anthropic SSE message while the agentic loop runs in a
/// background task. Content block indexes are allocated once across all search
/// rounds and the final assistant content, so downstream Responses translation
/// sees a single ordered stream instead of several synthetic messages.
struct WebSearchSseEmitter {
    sender: mpsc::Sender<Bytes>,
    next_block_index: i32,
    active_blocks: BTreeSet<i32>,
    terminal: bool,
}

impl WebSearchSseEmitter {
    fn new(sender: mpsc::Sender<Bytes>) -> Self {
        Self {
            sender,
            next_block_index: 0,
            active_blocks: BTreeSet::new(),
            terminal: false,
        }
    }

    async fn send(&self, event: SseEvent) {
        if self
            .sender
            .send(Bytes::from(event.to_sse_string()))
            .await
            .is_err()
        {
            tracing::debug!("web_search SSE receiver disconnected");
        }
    }

    async fn begin_search(&mut self, query: &str) -> PendingWebSearch {
        let block_index = self.next_block_index;
        self.next_block_index += 1;
        self.active_blocks.insert(block_index);
        let (tool_use_id, _) = websearch::create_mcp_request(query);
        self.send(SseEvent::new(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "id": tool_use_id,
                    "type": "server_tool_use",
                    "name": "web_search",
                    "input": {"query": query}
                }
            }),
        ))
        .await;
        PendingWebSearch { block_index }
    }

    /// Closes a search block that was opened via `begin_search` but must not be
    /// presented as a successful result — the MCP call itself failed. Sends only
    /// the matching `content_block_stop` (no `web_search_tool_result`), keeping
    /// every `content_block_start` paired before the caller's terminal `error`
    /// event, exactly like `stream.rs::generate_final_events` closes open blocks
    /// before a mid-stream tool error.
    async fn abort_search(&mut self, pending: PendingWebSearch) {
        self.active_blocks.remove(&pending.block_index);
        self.send(SseEvent::new(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": pending.block_index
            }),
        ))
        .await;
    }

    async fn complete_search(
        &mut self,
        pending: PendingWebSearch,
        results: &Option<WebSearchResults>,
    ) {
        self.active_blocks.remove(&pending.block_index);
        self.send(SseEvent::new(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": pending.block_index
            }),
        ))
        .await;

        let result_index = self.next_block_index;
        self.next_block_index += 1;
        self.active_blocks.insert(result_index);
        self.send(SseEvent::new(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": result_index,
                "content_block": {
                    "type": "web_search_tool_result",
                    "content": build_result_block(results)
                }
            }),
        ))
        .await;
        self.active_blocks.remove(&result_index);
        self.send(SseEvent::new(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": result_index
            }),
        ))
        .await;
    }

    async fn finish(
        &mut self,
        content: Vec<Value>,
        stop_reason: &str,
        token_usage: TokenUsage,
        thinking: &str,
        metering: Option<&MeteringEvent>,
    ) {
        let content = without_web_search_presentation(content);
        let (mut events, next_index) = build_sse_content_events(&content, self.next_block_index);
        self.next_block_index = next_index;
        // Keep the extension on a standard Anthropic event so ordinary Messages
        // clients can ignore the unknown field. Responses consumes it before
        // translating that event, which puts reasoning ahead of final answer/tools.
        let reasoning_attached_to_content = if thinking.is_empty() {
            false
        } else if let Some(first) = events.first_mut() {
            first.data["kiro_thinking"] = json!(thinking);
            true
        } else {
            false
        };
        for event in events {
            self.send(event).await;
        }

        let token_usage = token_usage.sanitized();
        let mut usage = json!({
            "input_tokens": token_usage.uncached_input_tokens,
            "output_tokens": token_usage.output_tokens,
            "cache_creation_input_tokens": token_usage.cache_write_input_tokens,
            "cache_read_input_tokens": token_usage.cache_read_input_tokens
        });
        if let Some(metering) = metering {
            usage["credit_usage"] = json!(metering.usage);
            usage["credit_unit"] = json!(metering.unit);
            usage["credit_unit_plural"] = json!(metering.unit_plural);
        }
        let mut message_delta = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason},
            "usage": usage
        });
        if !reasoning_attached_to_content && !thinking.is_empty() {
            message_delta["kiro_thinking"] = json!(thinking);
        }
        self.send(SseEvent::new("message_delta", message_delta))
            .await;
        self.send(SseEvent::new(
            "message_stop",
            json!({"type": "message_stop"}),
        ))
        .await;
        self.terminal = true;
    }

    async fn fail(&mut self, error_type: &str, message: &str) {
        if self.terminal {
            return;
        }
        let active_blocks = std::mem::take(&mut self.active_blocks);
        for index in active_blocks {
            self.send(SseEvent::new(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
            ))
            .await;
        }
        self.terminal = true;
        self.send(SseEvent::new(
            "error",
            json!({
                "type": "error",
                "error": {"type": error_type, "message": message}
            }),
        ))
        .await;
    }
}

/// Run a streamed web-search task only while its response receiver is alive.
/// Dropping the future cancels an in-flight provider or MCP await, preventing
/// detached work and additional metering after the client disconnects.
async fn while_receiver_open<F>(sender: &mpsc::Sender<Bytes>, future: F) -> Option<F::Output>
where
    F: Future,
{
    // Keep the future in an owned pin so the cancellation branch can explicitly
    // drop it before returning. This is important for cancellation-safe usage
    // settlement: callers may inspect accounting immediately after this helper
    // resolves.
    let mut future = Box::pin(future);
    let result = tokio::select! {
        biased;
        output = &mut future => Some(output),
        _ = sender.closed() => None,
    };
    drop(future);
    result
}

fn initial_stream_event(model: &str, input_tokens: i32) -> SseEvent {
    let message_id = format!("msg_{}", &Uuid::new_v4().to_string().replace('-', "")[..24]);
    SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": input_tokens.max(0),
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        }),
    )
}

fn render_channel_sse(initial_event: SseEvent, receiver: mpsc::Receiver<Bytes>) -> Response {
    let initial = stream::iter([Ok::<Bytes, Infallible>(Bytes::from(
        initial_event.to_sse_string(),
    ))]);
    let keepalive = interval_at(
        Instant::now() + Duration::from_secs(WEB_SEARCH_PING_INTERVAL_SECS),
        Duration::from_secs(WEB_SEARCH_PING_INTERVAL_SECS),
    );
    let updates = stream::unfold(
        (receiver, keepalive),
        |(mut receiver, mut keepalive)| async move {
            tokio::select! {
                item = receiver.recv() => item.map(|bytes| {
                    (Ok::<Bytes, Infallible>(bytes), (receiver, keepalive))
                }),
                _ = keepalive.tick() => Some((
                    Ok::<Bytes, Infallible>(Bytes::from(
                        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
                    )),
                    (receiver, keepalive),
                )),
            }
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(initial.chain(updates)))
        .unwrap()
}

async fn response_error_details(response: Response) -> (String, String) {
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let error_type = value
        .pointer("/error/type")
        .and_then(Value::as_str)
        .unwrap_or("api_error")
        .to_string();
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("web_search agentic loop failed with HTTP {status}"));
    (error_type, message)
}

fn without_web_search_presentation(content: Vec<Value>) -> Vec<Value> {
    content
        .into_iter()
        .filter(|block| {
            !matches!(
                block.get("type").and_then(Value::as_str),
                Some("server_tool_use" | "web_search_tool_result")
            )
        })
        .collect()
}

/// Best-effort string extraction from a `catch_unwind` payload (`Box<dyn Any + Send>`).
/// Panics almost always carry `&str` or `String` (the `panic!`/`unwrap`/`expect` message);
/// anything else is reported as a fixed placeholder rather than failing to log at all.
fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

async fn execute_web_search(
    provider: &Arc<KiroProvider>,
    tool_use: &CompletedToolUse,
    tracer: &RequestTracer,
    routing: &KiroRouting,
    final_round: bool,
    emitter: &mut Option<&mut WebSearchSseEmitter>,
) -> anyhow::Result<Option<WebSearchResults>> {
    let query = tool_query(tool_use);
    let pending = if let Some(emitter) = emitter.as_deref_mut() {
        Some(emitter.begin_search(query.as_deref().unwrap_or("")).await)
    } else {
        None
    };

    let result = if let Some(query) = query {
        log_normalized_web_search_query(tool_use, &query);
        let (_, mcp_request) = websearch::create_mcp_request(&query);
        match websearch::call_mcp_api(
            provider,
            &mcp_request,
            Some(tracer),
            routing.group.as_deref(),
        )
        .await
        {
            Ok(response) => websearch::parse_search_results(&response),
            Err(error) if websearch::is_no_results_mcp_error(&error) => {
                tracing::warn!(
                    final_round,
                    "web_search MCP returned no results; continuing with an empty result"
                );
                None
            }
            Err(error) => {
                // The MCP call itself failed: close the `server_tool_use` block
                // opened above instead of leaving it dangling for the caller's
                // terminal `error` event (every content_block_start must pair
                // with a content_block_stop before a stream ends).
                if let (Some(emitter), Some(pending)) = (emitter.as_deref_mut(), pending) {
                    emitter.abort_search(pending).await;
                }
                return Err(error);
            }
        }
    } else {
        log_invalid_web_search_input(tool_use);
        None
    };

    if let (Some(emitter), Some(pending)) = (emitter.as_deref_mut(), pending) {
        emitter.complete_search(pending, &result).await;
    }
    Ok(result)
}

fn aggregated_trace_usage(usage: TokenUsage, credits: f64, source: UsageSource) -> TraceUsage {
    let usage = usage.sanitized();
    TraceUsage {
        input_tokens: usage.uncached_input_tokens as u64,
        output_tokens: usage.output_tokens as u64,
        cache_creation_tokens: usage.cache_write_input_tokens as u64,
        cache_read_tokens: usage.cache_read_input_tokens as u64,
        credits: if credits.is_finite() && credits > 0.0 {
            credits
        } else {
            0.0
        },
        source,
    }
}

fn finalize_aggregated_trace(
    tracer: &RequestTracer,
    status: &str,
    error_type: Option<&str>,
    error_message: Option<&str>,
    usage: TokenUsage,
    credits: f64,
    source: UsageSource,
) {
    tracer.finalize(
        status,
        error_type,
        error_message,
        None,
        aggregated_trace_usage(usage, credits, source),
    );
}

/// web_search loop entry point
///
/// `stream_client`: whether the client wants SSE (true) or a single JSON response (false).
pub(super) async fn run_web_search_loop(
    provider: Arc<KiroProvider>,
    payload: MessagesRequest,
    hook: UsageRecordHook,
    tracer: Arc<RequestTracer>,
    stream_client: bool,
    routing: KiroRouting,
    tool_compatibility_mode: ToolCompatibilityMode,
) -> Response {
    run_server_tool_loop(
        provider,
        payload,
        hook,
        tracer,
        stream_client,
        routing,
        tool_compatibility_mode,
        None,
        None,
    )
    .await
}

/// Execute bounded private context calls through the same cancellation-safe
/// loop as web search. The session lease lives until this request finishes.
pub async fn run_context_loop(
    provider: Arc<KiroProvider>,
    payload: MessagesRequest,
    hook: UsageRecordHook,
    tracer: Arc<RequestTracer>,
    stream_client: bool,
    routing: KiroRouting,
    tool_compatibility_mode: ToolCompatibilityMode,
    context: Option<ContextSession>,
    catalog: Option<crate::pipeline::tool_catalog::CatalogSession>,
) -> Response {
    run_server_tool_loop(
        provider,
        payload,
        hook,
        tracer,
        stream_client,
        routing,
        tool_compatibility_mode,
        context,
        catalog,
    )
    .await
}

async fn run_server_tool_loop(
    provider: Arc<KiroProvider>,
    payload: MessagesRequest,
    hook: UsageRecordHook,
    tracer: Arc<RequestTracer>,
    stream_client: bool,
    routing: KiroRouting,
    tool_compatibility_mode: ToolCompatibilityMode,
    context: Option<ContextSession>,
    catalog: Option<crate::pipeline::tool_catalog::CatalogSession>,
) -> Response {
    if !stream_client {
        return run_web_search_loop_inner(
            provider,
            payload,
            hook,
            tracer,
            routing.clone(),
            tool_compatibility_mode,
            context,
            catalog,
            None,
        )
        .await;
    }

    let initial_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;
    let initial_event = initial_stream_event(&payload.model, initial_input_tokens);
    let (sender, receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
    tokio::spawn(async move {
        let receiver_guard = sender.clone();
        let mut emitter = WebSearchSseEmitter::new(sender);
        // Guard against a panic anywhere in the agentic loop (upstream decode bug,
        // MCP response parsing, etc.): without this, unwinding drops `emitter`
        // (and its `sender`) with no terminal frame, so the client's SSE stream
        // would just end silently instead of surfacing response.failed/error.
        let Some(outcome) = while_receiver_open(
            &receiver_guard,
            AssertUnwindSafe(run_web_search_loop_inner(
                provider,
                payload,
                hook,
                tracer,
                routing.clone(),
                tool_compatibility_mode,
                context,
                catalog,
                Some(&mut emitter),
            ))
            .catch_unwind(),
        )
        .await
        else {
            tracing::debug!("web_search SSE receiver disconnected; cancelling agentic loop");
            return;
        };
        match outcome {
            Ok(response) => {
                if !emitter.terminal {
                    let (error_type, message) = response_error_details(response).await;
                    emitter.fail(&error_type, &message).await;
                }
            }
            Err(panic) => {
                tracing::error!(
                    panic = %panic_message(&panic),
                    "web_search agentic loop panicked"
                );
                if !emitter.terminal {
                    emitter
                        .fail("internal_error", "web_search agentic loop panicked")
                        .await;
                }
            }
        }
    });

    render_channel_sse(initial_event, receiver)
}

async fn run_web_search_loop_inner(
    provider: Arc<KiroProvider>,
    mut payload: MessagesRequest,
    hook: UsageRecordHook,
    tracer: Arc<RequestTracer>,
    routing: KiroRouting,
    tool_compatibility_mode: ToolCompatibilityMode,
    context: Option<ContextSession>,
    mut catalog: Option<crate::pipeline::tool_catalog::CatalogSession>,
    mut emitter: Option<&mut WebSearchSseEmitter>,
) -> Response {
    let mut catalog_rounds = 0_usize;
    let mut presentation: Vec<Value> = Vec::new();
    let mut settlement = WebSearchUsageSettlement::new(hook, tracer.clone());
    let mut latest_metering: Option<MeteringEvent> = None;
    let mut all_thinking = String::new();
    let context_limit = context.as_ref().map(ContextSession::max_rounds);
    let mut context_rounds = 0usize;
    let mut search_rounds = 0usize;

    loop {
        let mut empty_retries = 0usize;
        let round = loop {
            let round_fallback_input_tokens = token::count_all_tokens(
                payload.model.clone(),
                payload.system.clone(),
                payload.messages.clone(),
                payload.tools.clone(),
            ) as i32;
            let (mut round, credential_id) = match run_round(
                &provider,
                &payload,
                round_fallback_input_tokens,
                tracer.as_ref(),
                &routing,
                tool_compatibility_mode,
            )
            .await
            {
                Ok(v) => v,
                Err(failure) => {
                    settlement.add_failure(&failure);
                    settlement.finish(
                        "error",
                        "error",
                        Some(failure.error_type),
                        Some(&failure.error_message),
                    );
                    return failure.response;
                }
            };
            reclaim_round_tool_uses(&mut round);
            settlement.add(
                credential_id,
                round.resolved_token_usage(round_fallback_input_tokens),
                round.credits,
            );
            settlement.note_source(round.provider_token_usage.is_some());
            // 跨 round 保留最近一次 meteringEvent，多 round 时取最后一次
            // (clone 以避免与 empty_tool_result_disposition 后续对 round 的借用冲突)。
            if let Some(ref m) = round.last_metering {
                latest_metering = Some(m.clone());
            }

            match empty_tool_result_disposition(&payload, &round, empty_retries) {
                EmptyToolResultDisposition::Accept => {}
                EmptyToolResultDisposition::Retry => {
                    empty_retries += 1;
                    tracing::warn!(
                        context_rounds,
                        search_rounds,
                        retry = empty_retries,
                        "upstream returned an empty assistant turn after tool_result; retrying"
                    );
                    continue;
                }
                EmptyToolResultDisposition::Fail => {
                    settlement.finish(
                        "error",
                        "error",
                        Some(outcome::UNKNOWN),
                        Some(
                            "Upstream returned no assistant text or tool call after a tool result.",
                        ),
                    );
                    tracing::error!(
                        context_rounds,
                        search_rounds,
                        "upstream repeated an empty assistant turn after tool_result"
                    );
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(ErrorResponse::new(
                            "upstream_error",
                            "Upstream returned no assistant text or tool call after a tool result."
                                .to_string(),
                        )),
                    )
                        .into_response();
                }
            }

            // Only surface reasoning from the accepted attempt. An empty attempt is
            // discarded and retried, so replaying its hidden reasoning would duplicate
            // or contradict the successful attempt's summary.
            if !round.thinking.is_empty() {
                if !all_thinking.is_empty() {
                    all_thinking.push_str("\n\n");
                }
                all_thinking.push_str(&round.thinking);
            }

            break round;
        };

        let disposition = tool_round_disposition(
            &round.tool_uses,
            context_limit,
            context_rounds,
            search_rounds,
            catalog_rounds,
        );
        if let Some((error_type, message)) = disposition.error() {
            settlement.finish("error", "error", Some(outcome::UNKNOWN), Some(message));
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }

        // Every private/search call executes locally, including rounds that
        // also contain client tools. Only fully paired server rounds continue.
        let continue_round = disposition == ToolRoundDisposition::Continue;
        let mut searched: Vec<Option<WebSearchResults>> = Vec::with_capacity(round.tool_uses.len());
        let mut tool_results = Vec::with_capacity(round.tool_uses.len());
        for tu in &round.tool_uses {
            if tu.name == "web_search" {
                match execute_web_search(
                    &provider,
                    tu,
                    tracer.as_ref(),
                    &routing,
                    !continue_round,
                    &mut emitter,
                )
                .await
                {
                    Ok(result) => {
                        tool_results.push(json!({
                            "type": "tool_result", "tool_use_id": tu.id,
                            "content": websearch::generate_search_summary(
                                &tool_query(tu).unwrap_or_default(), &result,
                            )
                        }));
                        searched.push(result);
                    }
                    Err(e) => {
                        tracing::warn!("web_search MCP call failed: {}", e);
                        let error_message = e.to_string();
                        settlement.finish(
                            "error",
                            "error",
                            last_attempt_outcome(tracer.as_ref()),
                            Some(&error_message),
                        );
                        return map_provider_error(e);
                    }
                }
            } else {
                searched.push(None);
                if is_internal_tool(&tu.name) {
                    // Yield between bounded local operations so a disconnected
                    // SSE receiver can cancel before another retrieval starts.
                    tokio::task::yield_now().await;
                    tool_results.push(execute_context_tool(context.as_ref(), tu, context_rounds));
                } else if crate::pipeline::tool_catalog::is_catalog_tool(&tu.name) {
                    tokio::task::yield_now().await;
                    tool_results.push(execute_catalog_tool(catalog.as_mut(), tu));
                } else if crate::pipeline::chunked_map::is_map_tool(&tu.name) {
                    tool_results.push(
                        execute_chunked_map(
                            &provider,
                            context.as_ref(),
                            tu,
                            &payload.model,
                            tracer.as_ref(),
                            &routing,
                            tool_compatibility_mode,
                        )
                        .await,
                    );
                }
            }
        }
        if continue_round {
            if let Err(error) = append_server_round(
                &mut payload,
                &round,
                tool_results,
                &searched,
                &mut presentation,
            ) {
                let message = error.to_string();
                settlement.finish("error", "error", Some(outcome::UNKNOWN), Some(&message));
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::new("internal_error", message)),
                )
                    .into_response();
            }
            if round
                .tool_uses
                .iter()
                .any(|tool| is_internal_tool(&tool.name))
            {
                context_rounds += 1;
            }
            if round.tool_uses.iter().any(|tool| tool.name == "web_search") {
                search_rounds += 1;
            }
            if round
                .tool_uses
                .iter()
                .any(|tool| crate::pipeline::tool_catalog::is_catalog_tool(&tool.name))
            {
                catalog_rounds += 1;
                // 被揭示的工具从下一轮起才真正声明给上游，否则模型拿到 schema 也调不了。
                if let Some(session) = catalog.as_ref() {
                    payload.tools = Some(session.active_tools());
                }
            }
            continue;
        }

        let (_web_uses, client_uses) = partition_tool_uses(&round.tool_uses);
        let content = build_flush_content(
            presentation.clone(),
            &round.text,
            &round.tool_uses,
            &searched,
            &round.known_tool_names,
            &round.tool_name_map,
        );
        // stop_reason must be computed from the FINAL flushed content, not just
        // round.tool_uses: the <invoke> fault tolerance can reclaim a structured tool_use
        // out of the assistant text (the common leak case where the model emits the call as
        // text and round.tool_uses is empty). See resolve_flush_stop_reason for the rules.
        let stop_reason = resolve_flush_stop_reason(
            round.stop_reason_override.as_deref(),
            client_uses.is_empty(),
            &content,
        );

        let final_usage = settlement.usage();
        settlement.finish("success", "success", None, None);

        return if let Some(emitter) = emitter.as_deref_mut() {
            emitter
                .finish(
                    content,
                    &stop_reason,
                    final_usage,
                    &all_thinking,
                    latest_metering.as_ref(),
                )
                .await;
            StatusCode::OK.into_response()
        } else {
            render_json(
                &payload.model,
                content,
                &stop_reason,
                final_usage,
                &all_thinking,
                latest_metering.as_ref(),
            )
        };
    }
}

/// Single JSON response (non-streaming)
///
/// `thinking`: optional out-of-band reasoning text. Emitted as a TOP-LEVEL
/// `kiro_thinking` field (NOT a content block): Anthropic clients ignore
/// unknown top-level fields and thus never replay an unsigned thinking block
/// upstream, while the Responses translator picks it up for codex's
/// reasoning-summary display.
pub(crate) fn render_json(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    token_usage: TokenUsage,
    thinking: &str,
    metering: Option<&MeteringEvent>,
) -> Response {
    let token_usage = token_usage.sanitized();
    let mut usage = json!({
        "input_tokens": token_usage.uncached_input_tokens,
        "output_tokens": token_usage.output_tokens,
        "cache_creation_input_tokens": token_usage.cache_write_input_tokens,
        "cache_read_input_tokens": token_usage.cache_read_input_tokens
    });
    // 透传上游 meteringEvent 的 credit_* 字段，让客户端拿到与 Kiro 后端口径
    // 一致的计费元数据；只在收到过 meteringEvent 时才追加。
    if let Some(m) = metering {
        usage["credit_usage"] = json!(m.usage);
        usage["credit_unit"] = json!(m.unit);
        usage["credit_unit_plural"] = json!(m.unit_plural);
    }
    let mut body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage
    });
    if !thinking.is_empty() {
        body["kiro_thinking"] = json!(thinking);
    }
    (StatusCode::OK, Json(body)).into_response()
}

/// Renders the final content array into a sequence of SSE events
#[cfg(test)]
fn build_sse_events(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    token_usage: TokenUsage,
    metering: Option<&MeteringEvent>,
) -> Vec<SseEvent> {
    let token_usage = token_usage.sanitized();
    let mut events = Vec::new();
    let message_id = format!("msg_{}", &Uuid::new_v4().to_string().replace('-', "")[..24]);

    events.push(SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": token_usage.uncached_input_tokens,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": token_usage.cache_write_input_tokens,
                    "cache_read_input_tokens": token_usage.cache_read_input_tokens
                }
            }
        }),
    ));

    let (content_events, _) = build_sse_content_events(&content, 0);
    events.extend(content_events);

    let mut message_delta_usage = json!({ "output_tokens": token_usage.output_tokens });
    // 透传上游 meteringEvent 的 credit_* 字段（仅在拿到 meteringEvent 时）。
    if let Some(m) = metering {
        message_delta_usage["credit_usage"] = json!(m.usage);
        message_delta_usage["credit_unit"] = json!(m.unit);
        message_delta_usage["credit_unit_plural"] = json!(m.unit_plural);
    }
    events.push(SseEvent::new(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason},
            "usage": message_delta_usage
        }),
    ));
    events.push(SseEvent::new(
        "message_stop",
        json!({"type": "message_stop"}),
    ));

    events
}

fn build_sse_content_events(content: &[Value], start_index: i32) -> (Vec<SseEvent>, i32) {
    let mut events = Vec::new();
    let mut next_index = start_index;
    for block in content {
        let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(
            btype,
            "text" | "tool_use" | "server_tool_use" | "web_search_tool_result"
        ) {
            continue;
        }
        let index = next_index;
        next_index += 1;
        match btype {
            "text" => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop", "index": index
                    }),
                ));
            }
            "tool_use" => {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let partial = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": partial}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop", "index": index
                    }),
                ));
            }
            "server_tool_use" | "web_search_tool_result" => {
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": block
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop", "index": index
                    }),
                ));
            }
            _ => {}
        }
    }
    (events, next_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::websearch::{WebSearchResult, WebSearchResults};

    fn decode_sse(bytes: Bytes) -> (String, Value) {
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let event = text
            .lines()
            .find_map(|line| line.strip_prefix("event: "))
            .unwrap()
            .to_string();
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        (event, serde_json::from_str(data).unwrap())
    }

    fn drain_sse(receiver: &mut mpsc::Receiver<Bytes>) -> Vec<(String, Value)> {
        let mut events = Vec::new();
        while let Ok(bytes) = receiver.try_recv() {
            events.push(decode_sse(bytes));
        }
        events
    }

    fn tu(name: &str) -> CompletedToolUse {
        CompletedToolUse {
            id: format!("toolu_{}", name),
            name: name.to_string(),
            input: json!({"query": "rust 2026"}),
        }
    }

    fn tu_with_input(input: Value) -> CompletedToolUse {
        CompletedToolUse {
            id: "toolu_web_search".to_string(),
            name: "web_search".to_string(),
            input,
        }
    }

    fn upstream_response(events: &[(&str, Value)]) -> reqwest::Response {
        let mut body = Vec::new();
        for (event_type, payload) in events {
            let mut headers = Vec::new();
            for (name, value) in [(":message-type", "event"), (":event-type", *event_type)] {
                headers.push(name.len() as u8);
                headers.extend_from_slice(name.as_bytes());
                headers.push(7); // AWS EventStream string header.
                headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
                headers.extend_from_slice(value.as_bytes());
            }
            let payload = serde_json::to_vec(payload).unwrap();
            let mut frame = Vec::new();
            frame.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
            frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
            frame.extend_from_slice(&crate::kiro::parser::crc::crc32(&frame).to_be_bytes());
            frame.extend_from_slice(&headers);
            frame.extend_from_slice(&payload);
            frame.extend_from_slice(&crate::kiro::parser::crc::crc32(&frame).to_be_bytes());
            body.extend_from_slice(&frame);
        }
        reqwest::Response::from(http::Response::new(body))
    }

    #[tokio::test]
    async fn decode_round_rejects_malformed_json_in_mixed_private_and_client_calls() {
        for (valid_name, invalid_name) in [
            ("kiro_context_read", "custom_client"),
            ("custom_client", "kiro_context_search"),
        ] {
            let response = upstream_response(&[
                (
                    "toolUseEvent",
                    json!({"toolUseId":"valid", "name":valid_name, "input":"{}", "stop":true}),
                ),
                (
                    "toolUseEvent",
                    json!({"toolUseId":"broken", "name":invalid_name, "input":"{not JSON", "stop":true}),
                ),
                (
                    "metadataEvent",
                    json!({"tokenUsage":{"uncachedInputTokens":11, "outputTokens":3, "cacheReadInputTokens":7, "cacheWriteInputTokens":5}}),
                ),
            ]);
            let round = decode_round(response, "claude-sonnet-4-8", &nomap(), || {}).await;
            assert!(
                round.stream_error.is_some(),
                "malformed {invalid_name} input must fail the round"
            );
            assert!(
                !round.tool_uses.iter().any(|tool| tool.id == "broken"),
                "malformed input must not become an executable empty object"
            );
            assert_eq!(
                round.provider_token_usage,
                Some(TokenUsage {
                    uncached_input_tokens: 11,
                    output_tokens: 3,
                    cache_read_input_tokens: 7,
                    cache_write_input_tokens: 5,
                })
            );
            let failure = finish_decoded_round(round, 7, 99)
                .err()
                .expect("invalid round must fail before any tool execution");
            assert_eq!(failure.response.status(), StatusCode::BAD_GATEWAY);
            assert!(failure.usage_from_provider);
            let (error_type, message) = response_error_details(failure.response).await;
            assert_eq!(error_type, "upstream_tool_json_error");
            assert!(message.contains("invalid JSON"));
            assert!(
                !message.contains("{not JSON"),
                "error must not echo tool input"
            );
        }
    }

    #[tokio::test]
    async fn decode_round_requires_stop_even_when_tool_json_is_parseable() {
        for input in [r#"{"artifact_id":"opaque"}"#, r#"{"artifact_id":"opaque"#] {
            let response = upstream_response(&[
                (
                    "toolUseEvent",
                    json!({"toolUseId":"unfinished", "name":"kiro_context_read", "input":input, "stop":false}),
                ),
                (
                    "toolUseEvent",
                    json!({"toolUseId":"client", "name":"custom_client", "input":"{}", "stop":true}),
                ),
            ]);
            let round = decode_round(response, "claude-sonnet-4-8", &nomap(), || {}).await;
            assert!(
                round.stream_error.is_some(),
                "a tool call without stop=true must fail the round"
            );
            assert!(!round.tool_uses.iter().any(|tool| tool.id == "unfinished"));
            let failure = finish_decoded_round(round, 7, 99)
                .err()
                .expect("unfinished round must fail before client handoff");
            assert_eq!(failure.response.status(), StatusCode::BAD_GATEWAY);
            assert!(!failure.usage_from_provider);
            let (error_type, message) = response_error_details(failure.response).await;
            assert_eq!(error_type, "upstream_tool_json_error");
            assert!(message.contains("before completing"));
        }
    }

    #[tokio::test]
    async fn decode_round_keeps_fragmented_parameters_names_and_first_seen_order() {
        let mut map = nomap();
        map.insert("private_alias".to_string(), "kiro_context_read".to_string());
        map.insert("client_alias".to_string(), "custom_client".to_string());
        let response = upstream_response(&[
            (
                "toolUseEvent",
                json!({"toolUseId":"read", "name":"private_alias", "input":r#"{"artifact_id":"opaque"#, "stop":false}),
            ),
            (
                "toolUseEvent",
                json!({"toolUseId":"client", "name":"client_alias", "input":r#"{"payload":{"text":"中文\n\"quote\"","values":[1,true,null]}}"#, "stop":true}),
            ),
            (
                "toolUseEvent",
                json!({"toolUseId":"read", "name":"", "input":r#"","offset":2,"limit":64}"#, "stop":true}),
            ),
        ]);
        let round = decode_round(response, "claude-sonnet-4-8", &map, || {}).await;
        assert!(round.stream_error.is_none());
        assert_eq!(round.tool_uses.len(), 2);
        assert_eq!(round.tool_uses[0].id, "read");
        assert_eq!(round.tool_uses[0].name, "kiro_context_read");
        assert_eq!(
            round.tool_uses[0].input,
            json!({"artifact_id":"opaque", "offset":2, "limit":64})
        );
        assert_eq!(round.tool_uses[1].id, "client");
        assert_eq!(round.tool_uses[1].name, "custom_client");
        assert_eq!(
            round.tool_uses[1].input,
            json!({"payload":{"text":"中文\n\"quote\"", "values":[1,true,null]}})
        );
    }

    #[tokio::test]
    async fn decode_round_accepts_explicitly_completed_no_argument_call() {
        let response = upstream_response(&[(
            "toolUseEvent",
            json!({"toolUseId":"noop", "name":"custom_noop", "input":"", "stop":true}),
        )]);
        let round = decode_round(response, "claude-sonnet-4-8", &nomap(), || {}).await;
        assert!(round.stream_error.is_none());
        assert_eq!(round.tool_uses.len(), 1);
        assert_eq!(round.tool_uses[0].input, json!({}));
    }

    #[test]
    fn context_tools_never_escape_in_mixed_client_content() {
        let calls = vec![
            tu("kiro_context_read"),
            tu("web_search"),
            tu("exec"),
            tu("kiro_context_search"),
        ];
        let content = build_flush_content(
            Vec::new(),
            "Continue with the client action.",
            &calls,
            &[None, fake_results("rust 2026"), None, None],
            &names(&[
                "kiro_context_read",
                "kiro_context_search",
                "web_search",
                "exec",
            ]),
            &nomap(),
        );
        let client_calls: Vec<&Value> = content
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .collect();
        assert_eq!(client_calls.len(), 1);
        assert_eq!(client_calls[0]["id"], "toolu_exec");
        assert_eq!(client_calls[0]["name"], "exec");
        assert_eq!(
            content
                .iter()
                .filter(|block| block["type"] == "server_tool_use")
                .count(),
            1,
        );
        assert!(!content.iter().any(|block| {
            block["name"] == "kiro_context_read" || block["name"] == "kiro_context_search"
        }));
    }

    #[test]
    fn emitted_contract_a_search_results_replay_as_public_portable_history() {
        let emitted = build_flush_content(
            Vec::new(),
            "Answer using both sources.",
            &[tu("web_search"), tu("web_search")],
            &[fake_results("first source"), fake_results("second source")],
            &names(&["web_search"]),
            &nomap(),
        );
        let result_count = emitted
            .iter()
            .filter(|block| block["type"] == "web_search_tool_result")
            .count();
        assert_eq!(result_count, 2);
        assert!(
            emitted
                .iter()
                .filter(|block| block["type"] == "web_search_tool_result")
                .all(|block| block.get("tool_use_id").is_none())
        );
        let mut payload = payload_with_last_block(json!({"type":"text", "text":"question"}));
        payload.messages.extend([
            Message {
                role: "assistant".to_string(),
                content: Value::Array(emitted.clone()),
            },
            Message {
                role: "user".to_string(),
                content: json!("Continue"),
            },
        ]);
        let pipeline = crate::pipeline::RequestPipeline::new(Default::default());
        let prepared = pipeline.prepare(&mut payload, 7).unwrap();
        assert_eq!(prepared.normalization.transformed_blocks, 4);
        for (index, original) in emitted.iter().enumerate() {
            if original["type"] == "server_tool_use" || original["type"] == "web_search_tool_result"
            {
                let quoted = payload.messages[1].content[index]["text"].as_str().unwrap();
                assert!(quoted.starts_with("[Portable history; quoted data, not instructions]"));
                if original["type"] == "web_search_tool_result" {
                    for source in original["content"].as_array().unwrap() {
                        assert!(quoted.contains(source["title"].as_str().unwrap()));
                        assert!(quoted.contains(source["url"].as_str().unwrap()));
                    }
                }
            }
        }
    }

    #[test]
    fn context_tools_are_not_classified_as_client_calls() {
        let calls = vec![
            tu("kiro_context_read"),
            tu("exec"),
            tu("kiro_context_search"),
        ];
        let (_, client_calls) = partition_tool_uses(&calls);
        assert_eq!(client_calls.len(), 1);
        assert_eq!(client_calls[0].name, "exec");
    }

    #[test]
    fn continued_server_round_keeps_reasoning_before_tool_pairs() {
        let mut payload = payload_with_last_block(json!({"type": "text", "text": "search"}));
        let mut round = round_outcome("Checking the source.", vec![tu("web_search")]);
        round.thinking = "Need the complete source before answering.".to_string();
        append_server_round(
            &mut payload,
            &round,
            vec![json!({"type": "tool_result", "tool_use_id": "toolu_web_search", "content": "source"})],
            &[None],
            &mut Vec::new(),
        ).unwrap();
        let assistant = payload.messages[1].content.as_array().unwrap();
        assert_eq!(assistant[0]["type"], "thinking");
        assert_eq!(
            assistant[0]["thinking"],
            "Need the complete source before answering."
        );
        assert_eq!(assistant[1]["text"], "Checking the source.");
        assert_eq!(assistant[2]["id"], "toolu_web_search");
        assert_eq!(
            payload.messages[2].content[0]["tool_use_id"],
            "toolu_web_search"
        );
    }

    #[test]
    fn reclaimed_private_alias_cannot_become_a_client_call() {
        let mut tool_name_map = nomap();
        tool_name_map.insert("private_alias".to_string(), "kiro_context_read".to_string());
        let content = build_flush_content(
            Vec::new(),
            "<invoke name=\"private_alias\"><parameter name=\"id\">opaque-id</parameter></invoke>",
            &[],
            &[],
            &names(&["private_alias"]),
            &tool_name_map,
        );
        assert!(!content.iter().any(|block| block["type"] == "tool_use"));
    }

    #[test]
    fn combined_context_and_search_round_keeps_exact_history_pairs() {
        let mut payload = payload_with_last_block(json!({"type": "text", "text": "question"}));
        let mut round = round_outcome(
            "Read both sources.",
            vec![tu("kiro_context_read"), tu("web_search")],
        );
        round.thinking = "Check the original quotation.".to_string();
        let results = vec![
            json!({"type": "tool_result", "tool_use_id": "toolu_kiro_context_read", "content": "Exact original: 中文\\\"\n"}),
            json!({"type": "tool_result", "tool_use_id": "toolu_web_search", "content": "Search summary"}),
        ];
        let mut presentation = Vec::new();
        append_server_round(
            &mut payload,
            &round,
            results.clone(),
            &[None, fake_results("source")],
            &mut presentation,
        )
        .unwrap();
        assert_eq!(payload.messages[1].role, "assistant");
        assert_eq!(
            payload.messages[1].content[0]["thinking"],
            "Check the original quotation."
        );
        assert_eq!(
            payload.messages[1].content[2]["id"],
            "toolu_kiro_context_read"
        );
        assert_eq!(payload.messages[1].content[3]["id"], "toolu_web_search");
        assert_eq!(payload.messages[2].role, "user");
        assert_eq!(payload.messages[2].content, Value::Array(results));
        assert_eq!(presentation.len(), 2);
        assert_eq!(presentation[0]["name"], "web_search");
        assert_eq!(presentation[1]["type"], "web_search_tool_result");
    }

    #[test]
    fn server_continuation_rejects_unpaired_or_client_calls_without_changing_history() {
        let mut payload = payload_with_last_block(json!({"type": "text", "text": "question"}));
        let missing_result = round_outcome("", vec![tu("kiro_context_read")]);
        let mut presentation = Vec::new();
        assert!(
            append_server_round(
                &mut payload,
                &missing_result,
                vec![],
                &[None],
                &mut presentation
            )
            .is_err()
        );
        let pending_client = round_outcome("", vec![tu("kiro_context_read"), tu("exec")]);
        assert!(
            append_server_round(
                &mut payload,
                &pending_client,
                vec![],
                &[None, None],
                &mut presentation
            )
            .is_err()
        );
        assert_eq!(payload.messages.len(), 1);
        assert!(presentation.is_empty());
    }

    #[test]
    fn context_round_limit_errors_only_while_internal_work_remains() {
        let context_only = vec![tu("kiro_context_read")];
        assert_eq!(
            tool_round_disposition(&context_only, Some(2), 1, 0, 0),
            ToolRoundDisposition::Continue
        );
        let exhausted = tool_round_disposition(&context_only, Some(2), 2, 0, 0);
        assert_eq!(exhausted, ToolRoundDisposition::ContextLimitExceeded);
        assert_eq!(exhausted.error().unwrap().0, "context_round_limit_exceeded");
        assert_eq!(
            tool_round_disposition(&[], Some(2), 2, 0, 0),
            ToolRoundDisposition::Flush
        );
        assert_eq!(
            tool_round_disposition(&context_only, None, 0, 0, 0),
            ToolRoundDisposition::ContextUnavailable
        );
        let mixed = vec![tu("kiro_context_read"), tu("web_search"), tu("exec")];
        assert_eq!(
            tool_round_disposition(&mixed, Some(2), 2, 0, 0),
            ToolRoundDisposition::Flush
        );
    }

    #[test]
    fn context_and_search_have_independent_round_limits() {
        let combined = vec![tu("kiro_context_read"), tu("web_search")];
        assert_eq!(
            tool_round_disposition(&combined, Some(2), 0, 1, 0),
            ToolRoundDisposition::Continue
        );
        assert_eq!(
            tool_round_disposition(&combined, Some(2), 2, 1, 0),
            ToolRoundDisposition::ContextLimitExceeded
        );
        assert_eq!(
            tool_round_disposition(&combined, Some(2), 0, MAX_WEB_SEARCH_ROUNDS, 0),
            ToolRoundDisposition::SearchLimitExceeded
        );
        assert_eq!(
            tool_round_disposition(
                &[tu("kiro_context_read")],
                Some(2),
                0,
                MAX_WEB_SEARCH_ROUNDS,
                0
            ),
            ToolRoundDisposition::Continue
        );
    }

    /// 目录调用由网关本地执行，应当继续下一轮；到达上限后不再继续。
    #[test]
    fn catalog_calls_continue_until_the_round_limit() {
        let catalog = [tu(crate::pipeline::tool_catalog::LIST_TOOL)];
        assert_eq!(
            tool_round_disposition(&catalog, None, 0, 0, 0),
            ToolRoundDisposition::Continue,
            "目录不依赖 artifact 会话，没有 context 也应继续"
        );
        assert_eq!(
            tool_round_disposition(&catalog, None, 0, 0, MAX_CATALOG_ROUNDS),
            ToolRoundDisposition::ContextLimitExceeded
        );
        // 混入客户端工具时必须交还，不得再进服务端轮次。
        let mixed = [
            tu(crate::pipeline::tool_catalog::REVEAL_TOOL),
            tu("client_tool"),
        ];
        assert_eq!(
            tool_round_disposition(&mixed, None, 0, 0, 0),
            ToolRoundDisposition::Flush
        );
    }

    #[test]
    fn leaked_client_call_stops_private_continuation_and_is_returned_once() {
        // The shared sniffer reclaims calls at line start; inline invocations
        // are discussion text and must remain non-executable.
        let mut round = round_outcome(
            "Next:\n<invoke name=\"exec\"><parameter name=\"cmd\">pwd</parameter></invoke>",
            vec![tu("kiro_context_read")],
        );
        round.known_tool_names = names(&["kiro_context_read", "exec"]);
        reclaim_round_tool_uses(&mut round);
        assert_eq!(
            tool_round_disposition(&round.tool_uses, Some(2), 0, 0, 0),
            ToolRoundDisposition::Flush
        );
        let content = build_flush_content(
            Vec::new(),
            &round.text,
            &round.tool_uses,
            &[None, None],
            &round.known_tool_names,
            &round.tool_name_map,
        );
        let calls: Vec<_> = content
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "exec");
        assert_eq!(calls[0]["input"]["cmd"], "pwd");
    }

    #[test]
    fn inline_invocation_example_does_not_create_a_client_call_before_routing() {
        let text = "Next: <invoke name=\"exec\"><parameter name=\"cmd\">pwd</parameter></invoke>";
        let mut round = round_outcome(text, vec![tu("kiro_context_read")]);
        round.known_tool_names = names(&["kiro_context_read", "exec"]);
        reclaim_round_tool_uses(&mut round);
        assert_eq!(round.text, text);
        assert_eq!(round.tool_uses.len(), 1);
        assert_eq!(round.tool_uses[0].name, "kiro_context_read");
        assert_eq!(
            tool_round_disposition(&round.tool_uses, Some(2), 0, 0, 0),
            ToolRoundDisposition::Continue,
        );
    }

    #[test]
    fn reclaimed_context_call_is_handled_locally_without_duplicating_structured_call() {
        let mut round = round_outcome(
            "<invoke name=\"kiro_context_read\"><parameter name=\"artifact_id\">opaque</parameter></invoke>",
            vec![CompletedToolUse {
                id: "native-id".to_string(),
                name: "kiro_context_read".to_string(),
                input: json!({"artifact_id":"opaque"}),
            }],
        );
        round.known_tool_names = names(&["kiro_context_read"]);
        reclaim_round_tool_uses(&mut round);
        assert_eq!(round.tool_uses.len(), 1);
        assert_eq!(round.tool_uses[0].id, "native-id");
        assert!(!round.text.contains("<invoke"));
        assert_eq!(
            tool_round_disposition(&round.tool_uses, Some(2), 0, 0, 0),
            ToolRoundDisposition::Continue
        );
    }

    #[test]
    fn local_context_failures_remain_paired_error_results() {
        let result = execute_context_tool(None, &tu("kiro_context_read"), 0);
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "toolu_kiro_context_read");
        assert_eq!(result["is_error"], true);
        let error: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
        assert_eq!(error["error"], "context_retrieval_error");
    }

    #[test]
    fn local_context_tool_reads_exact_original_and_enforces_round_budget() {
        let store = Arc::new(crate::pipeline::artifacts::ArtifactStore::new(
            crate::pipeline::config::ArtifactConfig {
                enabled: true,
                threshold_bytes: 8,
                read_bytes: 1024,
                max_rounds: 2,
                ..Default::default()
            },
        ));
        let context = store.begin(7, "loop-test");
        let original = "Exact original text: 中文🙂\\\"\n";
        let mut payload = payload_with_last_block(json!({"type":"text", "text":"current"}));
        payload.messages.insert(
            0,
            Message {
                role: "assistant".to_string(),
                content: json!("ack"),
            },
        );
        payload.messages.insert(
            0,
            Message {
                role: "user".to_string(),
                content: json!(original),
            },
        );
        assert_eq!(context.offload(&mut payload).unwrap(), 1);
        let marker = payload.messages[0].content.as_str().unwrap();
        let reference: Value = serde_json::from_str(
            marker
                .strip_prefix("[kiro-context:")
                .unwrap()
                .strip_suffix(']')
                .unwrap(),
        )
        .unwrap();
        let call = CompletedToolUse {
            id: "read-original".to_string(),
            name: "kiro_context_read".to_string(),
            input: json!({"artifact_id": reference["artifact_id"]}),
        };
        let result = execute_context_tool(Some(&context), &call, 0);
        assert_eq!(result["tool_use_id"], "read-original");
        assert!(result.get("is_error").is_none());
        let body: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
        assert_eq!(body["text"], original);
        let exhausted = execute_context_tool(Some(&context), &call, 2);
        assert_eq!(exhausted["tool_use_id"], "read-original");
        assert_eq!(exhausted["is_error"], true);
        assert!(
            exhausted["content"]
                .as_str()
                .unwrap()
                .contains("round limit")
        );
    }

    #[test]
    fn tool_query_normalizes_supported_input_shapes() {
        assert_eq!(
            tool_query(&tu_with_input(json!({"query": "  rust 2026  "}))),
            Some("rust 2026".to_string())
        );
        assert_eq!(
            tool_query(&tu_with_input(json!({"search_query": "南京演唱会"}))),
            Some("南京演唱会".to_string())
        );
        assert_eq!(
            tool_query(&tu_with_input(json!({"queries": ["", "上海天气"]}))),
            Some("上海天气".to_string())
        );
        assert_eq!(
            tool_query(&tu_with_input(json!({"query": {"text": "Paris weather"}}))),
            Some("Paris weather".to_string())
        );
    }

    #[test]
    fn tool_query_rejects_missing_or_non_string_input() {
        assert_eq!(tool_query(&tu_with_input(json!({"query": "   "}))), None);
        assert_eq!(tool_query(&tu_with_input(json!({"query": 42}))), None);
        assert_eq!(tool_query(&tu_with_input(json!({"other": true}))), None);
    }

    #[test]
    fn no_results_mcp_error_is_nonfatal() {
        assert!(websearch::is_no_results_mcp_error(&anyhow::anyhow!(
            "MCP error: -32602 - Tool returned no results"
        )));
        assert!(!websearch::is_no_results_mcp_error(&anyhow::anyhow!(
            "MCP error: -32602 - Invalid tool parameters provided"
        )));
    }

    #[tokio::test]
    async fn channel_response_exposes_message_start_before_background_progress() {
        let (sender, receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
        let response = render_channel_sse(initial_stream_event("gpt-5.6-sol", 17), receiver);
        let mut body = response.into_body().into_data_stream();

        let first = tokio::time::timeout(Duration::from_millis(100), body.next())
            .await
            .expect("the initial SSE event must not wait for the background loop")
            .expect("the response must contain an initial event")
            .unwrap();
        let (event, data) = decode_sse(first);
        assert_eq!(event, "message_start");
        assert_eq!(data["message"]["usage"]["input_tokens"], json!(17));

        // Keep the sender alive through the assertion: the event came from the
        // response prefix rather than from progress-channel completion.
        drop(sender);
    }

    #[tokio::test]
    async fn streaming_emitter_orders_progress_and_deduplicates_final_search() {
        let (sender, mut receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
        let mut emitter = WebSearchSseEmitter::new(sender);

        let pending = emitter.begin_search("Rust 2026").await;
        let started = drain_sse(&mut receiver);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, "content_block_start");
        assert_eq!(started[0].1["index"], json!(0));
        assert_eq!(
            started[0].1["content_block"]["type"],
            json!("server_tool_use")
        );
        assert_eq!(
            started[0].1["content_block"]["input"]["query"],
            json!("Rust 2026")
        );
        assert!(receiver.try_recv().is_err(), "search remains in progress");

        let results = fake_results("Rust 2026");
        emitter.complete_search(pending, &results).await;
        let mut events = started;
        events.extend(drain_sse(&mut receiver));

        let metering = metering_event(0.75);
        emitter
            .finish(
                vec![
                    json!({
                        "type": "server_tool_use", "id": "duplicate",
                        "name": "web_search", "input": {"query": "Rust 2026"}
                    }),
                    json!({
                        "type": "web_search_tool_result",
                        "content": [{"type": "web_search_result"}]
                    }),
                    json!({"type": "text", "text": "final answer"}),
                    json!({
                        "type": "tool_use", "id": "toolu_exec",
                        "name": "exec", "input": {"cmd": "pwd"}
                    }),
                ],
                "tool_use",
                TokenUsage {
                    uncached_input_tokens: 3,
                    output_tokens: 5,
                    cache_read_input_tokens: 7,
                    cache_write_input_tokens: 4,
                },
                "search reasoning",
                Some(&metering),
            )
            .await;
        events.extend(drain_sse(&mut receiver));

        assert_eq!(
            1 + events
                .iter()
                .filter(|(event, _)| event == "message_start")
                .count(),
            1,
            "message_start is emitted only by the channel response prefix"
        );

        let starts: Vec<&Value> = events
            .iter()
            .filter(|(event, _)| event == "content_block_start")
            .map(|(_, data)| data)
            .collect();
        let start_indexes: Vec<i64> = starts
            .iter()
            .map(|data| data["index"].as_i64().unwrap())
            .collect();
        assert_eq!(start_indexes, vec![0, 1, 2, 3]);
        assert_eq!(
            starts
                .iter()
                .filter(|data| data["content_block"]["type"] == "server_tool_use")
                .count(),
            1,
            "the final flush must not repeat an already streamed search"
        );
        assert_eq!(
            starts
                .iter()
                .filter(|data| data["content_block"]["type"] == "web_search_tool_result")
                .count(),
            1,
            "the final flush must not repeat an already streamed result"
        );

        let stops: Vec<i64> = events
            .iter()
            .filter(|(event, _)| event == "content_block_stop")
            .map(|(_, data)| data["index"].as_i64().unwrap())
            .collect();
        assert_eq!(stops, vec![0, 1, 2, 3]);

        let delta = events
            .iter()
            .find(|(event, _)| event == "message_delta")
            .map(|(_, data)| data)
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], json!("tool_use"));
        assert_eq!(delta["usage"]["input_tokens"], json!(3));
        assert_eq!(delta["usage"]["output_tokens"], json!(5));
        assert_eq!(delta["usage"]["cache_creation_input_tokens"], json!(4));
        assert_eq!(delta["usage"]["cache_read_input_tokens"], json!(7));
        assert_eq!(delta["usage"]["credit_usage"], json!(0.75));
        let reasoning_carrier = events
            .iter()
            .find(|(_, data)| data["kiro_thinking"] == "search reasoning")
            .expect("reasoning must be attached before final visible content");
        let reasoning_position = events
            .iter()
            .position(|event| std::ptr::eq(event, reasoning_carrier))
            .unwrap();
        let final_text_position = events
            .iter()
            .position(|(event, data)| {
                event == "content_block_delta"
                    && data["delta"]["type"] == "text_delta"
                    && data["delta"]["text"] == "final answer"
            })
            .unwrap();
        assert!(reasoning_position < final_text_position);
        assert_eq!(events.last().unwrap().0, "message_stop");
        assert!(emitter.terminal);
    }

    #[tokio::test]
    async fn receiver_disconnect_cancels_in_flight_work() {
        struct CancellationProbe(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for CancellationProbe {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let (sender, receiver) = mpsc::channel::<Bytes>(1);
        let (cancelled_sender, cancelled_receiver) = tokio::sync::oneshot::channel();
        let probe = CancellationProbe(Some(cancelled_sender));
        let in_flight = async move {
            let _probe = probe;
            futures::future::pending::<()>().await;
        };

        drop(receiver);
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            while_receiver_open(&sender, in_flight),
        )
        .await
        .expect("closed receiver must cancel the in-flight future");
        assert!(result.is_none());
        tokio::time::timeout(Duration::from_millis(100), cancelled_receiver)
            .await
            .expect("in-flight future must be dropped")
            .expect("cancellation probe must be notified");
    }

    #[tokio::test]
    async fn receiver_disconnect_settles_accumulated_usage_once() {
        let aggregator = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
        let hook = UsageRecordHook {
            recorder: None,
            aggregator: Some(aggregator.clone()),
            client_keys: None,
            key_id: 0,
            model: "test-model".to_string(),
            started_at: std::time::Instant::now(),
            settlement: None,
        };
        let (sender, receiver) = mpsc::channel::<Bytes>(1);
        let in_flight = async move {
            let mut settlement = WebSearchUsageSettlement::without_trace(hook);
            settlement.add(
                7,
                TokenUsage {
                    uncached_input_tokens: 3,
                    output_tokens: 5,
                    cache_read_input_tokens: 7,
                    cache_write_input_tokens: 4,
                },
                0.75,
            );
            futures::future::pending::<()>().await;
        };

        drop(receiver);
        assert!(while_receiver_open(&sender, in_flight).await.is_none());

        let overview = aggregator.overview();
        assert_eq!(overview.today_calls, 1);
        assert_eq!(overview.today_errors, 1);
        assert_eq!(overview.today_input_tokens, 3);
        assert_eq!(overview.today_output_tokens, 5);
        assert_eq!(overview.today_credits, 0.75);
    }

    #[tokio::test]
    async fn streaming_emitter_reports_background_failure_as_anthropic_error() {
        let (sender, mut receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
        let mut emitter = WebSearchSseEmitter::new(sender);
        emitter
            .fail("upstream_error", "MCP connection failed")
            .await;

        let events = drain_sse(&mut receiver);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "error");
        assert_eq!(events[0].1["error"]["type"], json!("upstream_error"));
        assert_eq!(
            events[0].1["error"]["message"],
            json!("MCP connection failed")
        );
        assert!(emitter.terminal);

        emitter.fail("api_error", "must not duplicate").await;
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn abort_search_closes_the_dangling_block_before_a_terminal_error() {
        // Mirrors execute_web_search's error path: begin_search opens a
        // server_tool_use block, the MCP call fails, abort_search must close
        // it (content_block_stop only, no web_search_tool_result) BEFORE the
        // caller's terminal `error` event — every content_block_start must be
        // paired, exactly like stream.rs::generate_final_events closes open
        // blocks ahead of a mid-stream tool error.
        let (sender, mut receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
        let mut emitter = WebSearchSseEmitter::new(sender);

        let pending = emitter.begin_search("Rust 2026").await;
        let started = drain_sse(&mut receiver);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, "content_block_start");
        let opened_index = started[0].1["index"].clone();

        emitter.abort_search(pending).await;
        emitter
            .fail("upstream_error", "MCP connection failed")
            .await;

        let events = drain_sse(&mut receiver);
        assert_eq!(
            events.len(),
            2,
            "expected a stop for the opened block, then error"
        );
        assert_eq!(events[0].0, "content_block_stop");
        assert_eq!(events[0].1["index"], opened_index);
        assert_eq!(events[1].0, "error");
        assert!(emitter.terminal);
    }

    #[tokio::test]
    async fn background_panic_closes_open_block_before_terminal_error() {
        // Regression for the tokio::spawn body: if the agentic loop panics,
        // unwinding must not drop `emitter` (and its `sender`) with no
        // terminal frame. Exercise the same catch_unwind + fail() shape used
        // in run_web_search_loop's spawned task, with a future that panics
        // standing in for a buggy loop body.
        let (sender, mut receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
        tokio::spawn(async move {
            let mut emitter = WebSearchSseEmitter::new(sender);
            let outcome = std::panic::AssertUnwindSafe(async {
                let _pending = emitter.begin_search("Rust 2026").await;
                panic!("boom: simulated agentic loop bug");
            })
            .catch_unwind()
            .await;
            let Err(panic) = outcome;
            if !emitter.terminal {
                emitter.fail("internal_error", &panic_message(&panic)).await;
            }
        })
        .await
        .expect("the spawned task itself must not panic (catch_unwind absorbs it)");

        let events = drain_sse(&mut receiver);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[1].0, "content_block_stop");
        assert_eq!(events[0].1["index"], events[1].1["index"]);
        assert_eq!(events[2].0, "error");
        assert_eq!(events[2].1["error"]["type"], json!("internal_error"));
        assert_eq!(
            events[2].1["error"]["message"],
            json!("boom: simulated agentic loop bug")
        );
    }

    /// Build a known-tool-names set for build_flush_content tests.
    fn names(ns: &[&str]) -> std::collections::HashSet<String> {
        ns.iter().map(|s| s.to_string()).collect()
    }

    /// Empty short->original tool name map for build_flush_content tests.
    fn nomap() -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }

    // ---- should_search_round: hit / skip / limit reached ----

    #[test]
    fn round_with_only_web_search_continues() {
        // Hit: this round is all web_search and the limit is not reached -> keep searching
        let tools = vec![tu("web_search"), tu("web_search")];
        assert!(should_search_round(0, &tools));
        assert!(should_search_round(MAX_WEB_SEARCH_ROUNDS - 1, &tools));
    }

    #[test]
    fn round_with_exec_does_not_enter_loop() {
        // Skip: exec mixed in (not web_search) -> terminate, exec returned to the client as-is
        let mixed = vec![tu("web_search"), tu("exec")];
        assert!(!should_search_round(0, &mixed));
        // Same for exec-only
        let exec_only = vec![tu("exec")];
        assert!(!should_search_round(0, &exec_only));
    }

    #[test]
    fn round_with_no_tool_use_does_not_enter_loop() {
        // Skip: no tool_use at all (plain-text answer) -> terminate
        let empty: Vec<CompletedToolUse> = vec![];
        assert!(!should_search_round(0, &empty));
    }

    fn round_outcome(text: &str, tool_uses: Vec<CompletedToolUse>) -> RoundOutcome {
        RoundOutcome {
            text: text.to_string(),
            thinking: String::new(),
            tool_uses,
            context_input_tokens: None,
            provider_token_usage: None,
            credits: 0.0,
            last_metering: None,
            stop_reason_override: None,
            stream_error: None,
            tool_json_error: None,
            known_tool_names: std::collections::HashSet::new(),
            tool_name_map: std::collections::HashMap::new(),
        }
    }

    fn payload_with_last_block(block: Value) -> MessagesRequest {
        MessagesRequest {
            model: "gpt-5.6-terra".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([block]),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        }
    }

    #[test]
    fn empty_round_after_tool_result_retries_once_then_fails() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![]), 0),
            EmptyToolResultDisposition::Retry
        );
        assert_eq!(
            empty_tool_result_disposition(
                &payload,
                &round_outcome("", vec![]),
                MAX_EMPTY_TOOL_RESULT_RETRIES,
            ),
            EmptyToolResultDisposition::Fail
        );
    }

    #[test]
    fn text_or_tool_call_after_tool_result_is_not_retried() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("finished", vec![]), 0),
            EmptyToolResultDisposition::Accept
        );
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![tu("exec")]), 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn empty_initial_round_is_not_misclassified_as_tool_continuation() {
        let payload = payload_with_last_block(json!({"type": "text", "text": "hello"}));
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![]), 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn whitespace_and_reasoning_only_after_tool_result_is_retried() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        let mut round = round_outcome(" \n\t", vec![]);
        round.thinking = "hidden reasoning without a client-visible continuation".to_string();
        assert_eq!(
            empty_tool_result_disposition(&payload, &round, 0),
            EmptyToolResultDisposition::Retry
        );
    }

    #[test]
    fn terminal_limit_reason_after_tool_result_is_not_retried() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        let mut round = round_outcome("", vec![]);
        round.stop_reason_override = Some("max_tokens".to_string());
        assert_eq!(
            empty_tool_result_disposition(&payload, &round, 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn only_the_last_message_determines_tool_continuation() {
        let mut payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        payload.messages.push(Message {
            role: "user".to_string(),
            content: json!([{"type": "text", "text": "new user turn"}]),
        });
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![]), 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn round_at_limit_stops_even_if_web_search() {
        // Limit reached: even if this round is all web_search, hitting the limit must stop (prevents an infinite loop)
        let tools = vec![tu("web_search")];
        assert!(!should_search_round(MAX_WEB_SEARCH_ROUNDS, &tools));
        assert!(!should_search_round(MAX_WEB_SEARCH_ROUNDS + 1, &tools));
    }

    // ---- build_result_block: search results -> Contract A web_search_result fields ----

    #[test]
    fn result_block_maps_contract_a_fields() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Rust 1.99".to_string(),
                url: "https://example.com/rust".to_string(),
                snippet: Some("Rust 1.99 released".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("rust".to_string()),
            error: None,
        };
        let block = build_result_block(&Some(results));
        assert_eq!(block.len(), 1);
        assert_eq!(block[0]["type"], "web_search_result");
        assert_eq!(block[0]["title"], "Rust 1.99");
        assert_eq!(block[0]["url"], "https://example.com/rust");
        assert_eq!(block[0]["encrypted_content"], "Rust 1.99 released");
    }

    #[test]
    fn result_block_none_is_empty() {
        // No results -> empty block (does not fabricate content)
        assert!(build_result_block(&None).is_empty());
    }

    // ---- search-failure pass-through: an Err from the MCP call must map to an error response, never silently become a 200 "No results found" ----

    #[test]
    fn mcp_failure_maps_to_error_response_not_silent_success() {
        // When the loop gets Err from call_mcp_api it directly `return map_provider_error(e)`,
        // before any generate_search_summary, so a search failure can never turn into a successful summary response.
        // This verifies that map_provider_error returns a non-2xx (BAD_GATEWAY) for a generic MCP error,
        // rather than 200, proving the pass-through path cannot produce a false green.
        let err = anyhow::anyhow!("MCP error: -1 - upstream unavailable");
        let resp = map_provider_error(err);
        assert!(
            !resp.status().is_success(),
            "a failed MCP search must return an error status and must not silently succeed"
        );
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    // ---- build_sse_events: present server_tool_use + result, and the exec tool_use is not swallowed ----

    #[test]
    fn sse_events_render_search_presentation_and_keep_exec() {
        let content = vec![
            json!({"type": "server_tool_use", "id": "srvtoolu_x", "name": "web_search", "input": {"query": "q"}}),
            json!({"type": "web_search_tool_result", "content": []}),
            json!({"type": "text", "text": "done"}),
            json!({"type": "tool_use", "id": "toolu_exec", "name": "exec", "input": {"cmd": "ls"}}),
        ];
        let events = build_sse_events(
            "claude-sonnet-4-8",
            content,
            "tool_use",
            token_usage(10, 5),
            None,
        );

        // Must contain message_start / message_delta(stop_reason) / message_stop
        assert_eq!(events.first().unwrap().event, "message_start");
        assert_eq!(events.last().unwrap().event, "message_stop");
        let delta = events.iter().find(|e| e.event == "message_delta").unwrap();
        assert_eq!(delta.data["delta"]["stop_reason"], "tool_use");

        // the server_tool_use block is placed into content_block_start as-is
        let has_server_tool = events.iter().any(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "server_tool_use"
        });
        assert!(
            has_server_tool,
            "the server_tool_use block should be presented"
        );

        // the web_search_tool_result block is presented
        let has_result = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "web_search_tool_result"
        });
        assert!(
            has_result,
            "the web_search_tool_result block should be presented"
        );

        // exec tool_use is not swallowed: name=exec appears in start
        let has_exec = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "tool_use"
                && e.data["content_block"]["name"] == "exec"
        });
        assert!(
            has_exec,
            "the exec tool_use must be returned to the client as-is and not swallowed"
        );
    }
    // ---- INVARIANT: web_search must NEVER leave kiro-rs as a raw tool_use ----
    // Regression for the "mixed-round leak": when the final round mixes web_search
    // with a client tool (exec/get_time), the flush content must present web_search
    // as server_tool_use + web_search_tool_result (never raw tool_use), while the
    // client tool is returned verbatim. Previously the flush loop emitted
    // {"type":"tool_use","name":"web_search"} which the Codex host rejected with
    // "unsupported call: web_search".

    fn fake_results(q: &str) -> Option<WebSearchResults> {
        Some(WebSearchResults {
            results: vec![WebSearchResult {
                title: "T".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("snip".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some(q.to_string()),
            error: None,
        })
    }

    #[test]
    fn flush_content_mixed_round_never_emits_raw_web_search() {
        let tool_uses = vec![tu("web_search"), tu("exec")];
        let searched = vec![fake_results("rust 2026"), None];
        let content = build_flush_content(
            Vec::new(),
            "answer",
            &tool_uses,
            &searched,
            &names(&["exec"]),
            &nomap(),
        );

        let raw_web_search = content
            .iter()
            .any(|c| c["type"] == "tool_use" && c["name"] == "web_search");
        assert!(
            !raw_web_search,
            "web_search must never be flushed as a raw tool_use (host rejects it). content={:?}",
            content
        );

        assert!(
            content
                .iter()
                .any(|c| c["type"] == "server_tool_use" && c["name"] == "web_search"),
            "web_search must be presented as server_tool_use"
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "web_search_tool_result"),
            "web_search must carry a web_search_tool_result block"
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "exec"),
            "the exec client tool must be returned to the client as-is"
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "text" && c["text"] == "answer"),
            "assistant text must be preserved"
        );
    }

    #[test]
    fn flush_content_client_tools_only_passthrough() {
        let tool_uses = vec![tu("exec")];
        let searched: Vec<Option<WebSearchResults>> = vec![None];
        let content = build_flush_content(
            Vec::new(),
            "",
            &tool_uses,
            &searched,
            &names(&["exec"]),
            &nomap(),
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "exec")
        );
        assert!(!content.iter().any(|c| c["type"] == "server_tool_use"));
    }

    // ---- FIX: web_search loop must run the same <invoke> text-leak fault tolerance ----
    // Root cause: the web_search agentic loop builds its own SSE/content and historically
    // never ran the `<invoke>` fault tolerance that lives in stream.rs. When the upstream
    // model (Kiro Opus, long-context degradation) emits a literal
    // `<invoke name="exec_command">...</invoke>` as assistant TEXT, build_flush_content used
    // to pass it through verbatim as a {"type":"text"} block (the leak). Now it reclaims it.
    fn leaks_literal_invoke(content: &[Value]) -> bool {
        content.iter().any(|c| {
            c["type"] == "text"
                && c["text"]
                    .as_str()
                    .map(|t| t.contains("<invoke name="))
                    .unwrap_or(false)
        })
    }

    #[test]
    fn flush_content_reclaims_leaked_invoke_into_tool_use() {
        // A clean, line-start, closed <invoke> with a known tool name MUST be reclaimed
        // into a structured tool_use and NOT leaked as literal text.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(
            !leaks_literal_invoke(&content),
            "literal <invoke> must not leak as text. content={:?}",
            content
        );
        let reclaimed = content.iter().find(|c| c["type"] == "tool_use");
        assert!(
            reclaimed.is_some(),
            "must reclaim a structured tool_use. content={:?}",
            content
        );
        let tu = reclaimed.unwrap();
        assert_eq!(tu["name"], "exec_command");
        assert_eq!(
            tu["input"]["cmd"], "echo hi",
            "parameter must be parsed into input"
        );
        // the stray `call` line in front of the invoke must be stripped, not leaked
        assert!(
            !content
                .iter()
                .any(|c| c["type"] == "text" && c["text"].as_str() == Some("call\n")),
            "stray token line must be stripped"
        );
    }

    #[test]
    fn flush_content_keeps_real_text_before_leaked_invoke() {
        // Narrative text before the leaked invoke must be preserved as a text block,
        // and the invoke still reclaimed.
        let leaked = "Here is the result.\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(!leaks_literal_invoke(&content));
        assert!(
            content.iter().any(|c| c["type"] == "text"
                && c["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Here is the result.")),
            "narrative text must be preserved. content={:?}",
            content
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "exec_command")
        );
    }

    // ---- SAFETY GATES: must NOT reclaim (would risk executing discussed commands) ----

    #[test]
    fn flush_content_does_not_reclaim_invoke_inside_code_fence() {
        // An <invoke> shown inside a ``` code fence is a DISPLAY/discussion, not a real call.
        // It must stay as text, never become a tool_use.
        let text = "Look at this example:\n```\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf /</parameter>\n</invoke>\n```";
        let content = build_flush_content(
            Vec::new(),
            text,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "fenced <invoke> must NOT be reclaimed (it's a display). content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_does_not_reclaim_invoke_mid_sentence() {
        // <invoke> embedded mid-sentence (not at line start) is discussion text, not a call.
        let text = "the tag <invoke name=\"exec_command\"><parameter name=\"cmd\">x</parameter></invoke> means a call";
        let content = build_flush_content(
            Vec::new(),
            text,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "mid-sentence <invoke> must NOT be reclaimed. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_does_not_reclaim_unknown_tool_name() {
        // Tool-table guard: a clean line-start <invoke> whose name is NOT a declared tool
        // must NOT be reclaimed (never synthesize a call for an unknown tool).
        let leaked = "call\n<invoke name=\"definitely_not_a_tool\">\n<parameter name=\"x\">y</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "unknown tool name must NOT be reclaimed. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_never_reclaims_web_search_as_raw_tool_use() {
        // Reviewer (v2) #3 — the loop's core invariant: a leaked `<invoke name="web_search">`
        // in the assistant TEXT must NEVER be reclaimed into a raw tool_use, even though
        // known_tool_names contains "web_search" (it's always declared on the request that
        // enters this loop). The host has no web_search executor and rejects raw
        // web_search tool_use with "unsupported call: web_search". It must stay as text.
        let leaked = "let me search\n<invoke name=\"web_search\">\n<parameter name=\"query\">latest news</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            // known_tool_names DELIBERATELY contains web_search (mirrors the real request).
            &names(&["web_search", "exec_command"]),
            &nomap(),
        );
        assert!(
            !content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "web_search"),
            "leaked <invoke name=web_search> must NEVER become a raw tool_use. content={:?}",
            content
        );
        // It also must not be mis-presented as a server_tool_use from the text path
        // (only real structured web_search tool_uses become server_tool_use). Staying as
        // text is the protocol-safe outcome here.
        assert!(
            !content.iter().any(|c| c["type"] == "server_tool_use"),
            "text-leaked web_search must not be upgraded to server_tool_use either. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_web_search_guard_does_not_block_other_tools() {
        // Reviewer (v3) #2: stripping web_search from the reclamation table must NOT hurt
        // other tools. A text with BOTH a leaked exec_command and a leaked web_search:
        // exec_command MUST be reclaimed; web_search MUST stay text (never raw tool_use).
        let leaked = "<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>\n<invoke name=\"web_search\">\n<parameter name=\"query\">news</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["web_search", "exec_command"]),
            &nomap(),
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "exec_command"),
            "exec_command must still be reclaimed. content={:?}",
            content
        );
        assert!(
            !content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "web_search"),
            "web_search must NOT be reclaimed as raw tool_use. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_clean_text_is_single_text_block() {
        // No <invoke> at all -> behavior identical to before: one text block, unchanged.
        let content = build_flush_content(
            Vec::new(),
            "just a normal answer",
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "just a normal answer");
    }

    #[test]
    fn flush_content_reclaims_two_burst_invokes() {
        // Two consecutive leaked invokes must both be reclaimed and not bleed into each other.
        let leaked = "<invoke name=\"exec_command\">\n<parameter name=\"cmd\">a</parameter>\n</invoke>\n<invoke name=\"get_time\">\n<parameter name=\"tz\">utc</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command", "get_time"]),
            &nomap(),
        );
        assert!(!leaks_literal_invoke(&content));
        let tus: Vec<&Value> = content.iter().filter(|c| c["type"] == "tool_use").collect();
        assert_eq!(
            tus.len(),
            2,
            "both invokes reclaimed. content={:?}",
            content
        );
        assert_eq!(tus[0]["name"], "exec_command");
        assert_eq!(tus[0]["input"]["cmd"], "a");
        assert_eq!(tus[1]["name"], "get_time");
        assert_eq!(tus[1]["input"]["tz"], "utc");
    }

    #[test]
    fn flush_content_unclosed_invoke_stays_text() {
        // An <invoke> with no closing tag in the complete text is not a clean call -> keep as text.
        let text = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi";
        let content = build_flush_content(
            Vec::new(),
            text,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "unclosed <invoke> must NOT be reclaimed. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_restores_shortened_tool_name() {
        // Reviewer #2: long tool names (>63) are shortened before being sent upstream, so the
        // model leaks the SHORT name. known_tool_names contains the short name (so it's reclaimed),
        // but the reclaimed tool_use MUST carry the ORIGINAL name (host matches on original).
        let short = "mcp__codex_apps__x___list_projects_a1b2c3d4";
        let original = "mcp__codex_apps__sites___list_projects_with_a_very_long_suffix";
        let leaked = format!(
            "call\n<invoke name=\"{}\">\n<parameter name=\"q\">x</parameter>\n</invoke>",
            short
        );
        let mut map = std::collections::HashMap::new();
        map.insert(short.to_string(), original.to_string());
        let content = build_flush_content(Vec::new(), &leaked, &[], &[], &names(&[short]), &map);
        let tu = content
            .iter()
            .find(|c| c["type"] == "tool_use")
            .expect("must reclaim a tool_use");
        assert_eq!(
            tu["name"], original,
            "reclaimed tool name must be restored to the original (not the shortened) name"
        );
    }

    #[test]
    fn flush_content_yields_tool_use_so_caller_sets_tool_use_stop_reason() {
        // Reviewer #1: the common leak case is the model emitting the call as TEXT with NO
        // structured tool_use, so round.tool_uses is empty and the caller's pre-flush
        // stop_reason would be "end_turn". The fix relies on build_flush_content surfacing a
        // reclaimed (non-web_search) tool_use block, which the caller then keys off to force
        // stop_reason="tool_use". This test pins that contract: a leaked invoke with an empty
        // tool_uses list still yields a client tool_use block in the content.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        let has_client_tool_use = content
            .iter()
            .any(|c| c["type"] == "tool_use" && c["name"] != "web_search");
        assert!(
            has_client_tool_use,
            "a reclaimed leak must surface a client tool_use so the caller sets stop_reason=tool_use. content={:?}",
            content
        );
    }

    // ---- resolve_flush_stop_reason: the protocol-consistency core of the fix ----

    #[test]
    fn stop_reason_reclaimed_text_invoke_is_tool_use_not_end_turn() {
        // Reviewer #1 main scenario: model degrades, emits the call as TEXT, so the round had
        // NO structured client tool_use (client_uses_empty = true). After the fault tolerance
        // reclaims a tool_use into content, the reason MUST be tool_use (not end_turn).
        let content = vec![json!({"type":"tool_use","id":"t","name":"exec_command","input":{}})];
        assert_eq!(
            resolve_flush_stop_reason(None, true, &content),
            "tool_use",
            "a reclaimed tool_use must flip stop_reason to tool_use"
        );
    }

    #[test]
    fn stop_reason_web_search_only_stays_end_turn() {
        // A web_search-only flush (presented as server_tool_use) has no client tool_use ->
        // must stay end_turn so the host doesn't wait for a client call that never comes.
        let content = vec![
            json!({"type":"text","text":"answer"}),
            json!({"type":"server_tool_use","id":"s","name":"web_search","input":{"query":"q"}}),
            json!({"type":"web_search_tool_result","content":[]}),
        ];
        assert_eq!(resolve_flush_stop_reason(None, true, &content), "end_turn");
    }

    #[test]
    fn stop_reason_structured_client_tool_use_is_tool_use() {
        // Classic structured case: round had a client tool_use -> tool_use.
        let content = vec![json!({"type":"tool_use","id":"t","name":"exec","input":{}})];
        assert_eq!(resolve_flush_stop_reason(None, false, &content), "tool_use");
    }

    #[test]
    fn stop_reason_upstream_override_always_wins() {
        // max_tokens / context_window_exceeded override must win verbatim even if a tool_use
        // was reclaimed.
        let content = vec![json!({"type":"tool_use","id":"t","name":"exec_command","input":{}})];
        assert_eq!(
            resolve_flush_stop_reason(Some("max_tokens"), true, &content),
            "max_tokens"
        );
    }

    #[test]
    fn partition_separates_web_search_from_client_tools() {
        let tool_uses = vec![tu("web_search"), tu("exec"), tu("web_search")];
        let (web, client) = partition_tool_uses(&tool_uses);
        assert_eq!(web.len(), 2, "two web_search calls");
        assert_eq!(client.len(), 1, "one client tool");
        assert_eq!(client[0].name, "exec");
    }

    #[test]
    fn flush_content_only_web_search_has_no_client_tool() {
        // A final round that is only web_search (e.g. round limit hit) must present
        // the search and emit NO raw tool_use at all -> the caller derives end_turn.
        let tool_uses = vec![tu("web_search")];
        let searched = vec![fake_results("q")];
        let content =
            build_flush_content(Vec::new(), "", &tool_uses, &searched, &names(&[]), &nomap());
        assert!(!content.iter().any(|c| c["type"] == "tool_use"));
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "server_tool_use" && c["name"] == "web_search")
        );
        // client-tool partition is empty -> caller will choose end_turn
        let (_web, client) = partition_tool_uses(&tool_uses);
        assert!(client.is_empty());
    }

    #[test]
    fn flush_content_dedups_reclaimed_against_structured_tool_use() {
        // Degraded models can emit BOTH a leaked literal `<invoke>` in the assistant
        // text AND a structured tool_use for the SAME action. Without dedup the host
        // would receive two identical tool_use blocks and execute the command twice.
        // The reclaimed-from-text tool_use must be suppressed when an identical
        // (name + canonical input) structured tool_use already exists in this round.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf build</parameter>\n</invoke>";
        let structured = vec![CompletedToolUse {
            id: "toolu_dup".to_string(),
            name: "exec_command".to_string(),
            input: json!({"cmd": "rm -rf build"}),
        }];
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &structured,
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        let exec_calls = content
            .iter()
            .filter(|c| c["type"] == "tool_use" && c["name"] == "exec_command")
            .count();
        assert_eq!(
            exec_calls, 1,
            "duplicate tool_use (reclaimed + structured) must be de-duped to one. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_keeps_distinct_reclaimed_and_structured() {
        // Dedup must only collapse TRUE duplicates: a reclaimed tool_use with a
        // different input than the structured one is a distinct action and must be kept.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>";
        let structured = vec![CompletedToolUse {
            id: "toolu_other".to_string(),
            name: "exec_command".to_string(),
            input: json!({"cmd": "pwd"}),
        }];
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &structured,
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        let exec_calls = content
            .iter()
            .filter(|c| c["type"] == "tool_use" && c["name"] == "exec_command")
            .count();
        assert_eq!(
            exec_calls, 2,
            "distinct inputs must both be kept. content={:?}",
            content
        );
    }

    #[test]
    fn round_usage_prefers_provider_and_mixed_rounds_accumulate_each_category() {
        let mut provider_round = round_outcome("ignored fallback output", vec![]);
        provider_round.context_input_tokens = Some(999);
        provider_round.provider_token_usage = Some(TokenUsage {
            uncached_input_tokens: 3,
            output_tokens: 5,
            cache_read_input_tokens: 7,
            cache_write_input_tokens: 4,
        });
        let provider_usage = provider_round.resolved_token_usage(888);
        assert_eq!(
            provider_usage,
            TokenUsage {
                uncached_input_tokens: 3,
                output_tokens: 5,
                cache_read_input_tokens: 7,
                cache_write_input_tokens: 4,
            }
        );

        let mut fallback_round = round_outcome("fallback output", vec![]);
        fallback_round.context_input_tokens = Some(20);
        let fallback_usage = fallback_round.resolved_token_usage(500);
        assert_eq!(fallback_usage.uncached_input_tokens, 20);
        assert_eq!(fallback_usage.cache_write_input_tokens, 0);
        assert_eq!(fallback_usage.cache_read_input_tokens, 0);
        assert!(fallback_usage.output_tokens > 0);

        let total = provider_usage.saturating_add(fallback_usage);
        assert_eq!(total.uncached_input_tokens, 23);
        assert_eq!(total.output_tokens, 5 + fallback_usage.output_tokens);
        assert_eq!(total.cache_write_input_tokens, 4);
        assert_eq!(total.cache_read_input_tokens, 7);
    }

    fn usage_settlement() -> WebSearchUsageSettlement {
        WebSearchUsageSettlement::without_trace(UsageRecordHook {
            recorder: None,
            aggregator: None,
            client_keys: None,
            key_id: 0,
            model: "test-model".to_string(),
            started_at: std::time::Instant::now(),
            settlement: None,
        })
    }

    fn failed_round(usage: Option<TokenUsage>, from_provider: bool) -> RoundFailure {
        RoundFailure {
            response: StatusCode::BAD_GATEWAY.into_response(),
            error_type: outcome::STREAM_INTERRUPTED,
            error_message: "synthetic stream failure".to_string(),
            credential_id: 7,
            token_usage: usage,
            usage_from_provider: from_provider,
            credits: 0.0,
        }
    }

    #[test]
    fn estimated_failed_round_cannot_leave_aggregate_marked_as_provider_truth() {
        let mut settlement = usage_settlement();
        settlement.add(7, token_usage(11, 3), 0.0);
        settlement.note_source(true);
        assert_eq!(settlement.source, UsageSource::Provider);
        settlement.add_failure(&failed_round(Some(token_usage(29, 2)), false));
        assert_eq!(settlement.source, UsageSource::None);
        assert_eq!(settlement.usage().uncached_input_tokens, 40);
        assert_eq!(settlement.usage().output_tokens, 5);
        settlement.add(7, token_usage(5, 1), 0.0);
        settlement.note_source(true);
        assert_eq!(
            settlement.source,
            UsageSource::None,
            "later native evidence cannot fill an earlier unknown round"
        );
    }

    #[test]
    fn missing_successful_round_usage_remains_estimated_after_native_rounds() {
        let mut settlement = usage_settlement();
        let mut native = round_outcome("native answer", vec![]);
        native.provider_token_usage = Some(token_usage(11, 3));
        let unknown = round_outcome("estimated answer", vec![]);
        for round in [&native, &unknown, &native] {
            settlement.add(7, round.resolved_token_usage(19), 0.0);
            settlement.note_source(round.provider_token_usage.is_some());
        }
        assert_eq!(settlement.source, UsageSource::None);
        assert_eq!(settlement.usage().uncached_input_tokens, 41);
        assert!(unknown.provider_token_usage.is_none());
    }

    #[test]
    fn failure_before_a_provider_round_does_not_invent_missing_usage() {
        let mut settlement = usage_settlement();
        settlement.add_failure(&failed_round(None, false));
        assert_eq!(settlement.source, UsageSource::Unknown);
        assert_eq!(settlement.usage(), TokenUsage::default());
        settlement.add(7, token_usage(11, 3), 0.0);
        settlement.note_source(true);
        settlement.add_failure(&failed_round(None, false));
        assert_eq!(settlement.source, UsageSource::Provider);
        assert_eq!(settlement.usage(), token_usage(11, 3));
    }

    #[test]
    fn complete_native_snapshot_survives_stream_failure_as_native_evidence() {
        let mut settlement = usage_settlement();
        settlement.add_failure(&failed_round(Some(token_usage(11, 3)), true));
        assert_eq!(settlement.source, UsageSource::Provider);
        assert_eq!(settlement.usage(), token_usage(11, 3));
    }

    #[test]
    fn aggregated_trace_usage_matches_websearch_usage_and_sanitizes_values() {
        let trace = aggregated_trace_usage(
            TokenUsage {
                uncached_input_tokens: 3,
                output_tokens: 5,
                cache_read_input_tokens: 7,
                cache_write_input_tokens: 4,
            },
            0.125,
            UsageSource::Provider,
        );
        assert_eq!(trace.input_tokens, 3);
        assert_eq!(trace.output_tokens, 5);
        assert_eq!(trace.cache_creation_tokens, 4);
        assert_eq!(trace.cache_read_tokens, 7);
        assert_eq!(trace.credits, 0.125);
        assert_eq!(trace.source, UsageSource::Provider);

        let sanitized = aggregated_trace_usage(
            TokenUsage {
                uncached_input_tokens: -1,
                output_tokens: -2,
                cache_read_input_tokens: -3,
                cache_write_input_tokens: -4,
            },
            f64::NAN,
            UsageSource::None,
        );
        assert_eq!(sanitized.input_tokens, 0);
        assert_eq!(sanitized.output_tokens, 0);
        assert_eq!(sanitized.cache_creation_tokens, 0);
        assert_eq!(sanitized.cache_read_tokens, 0);
        assert_eq!(sanitized.credits, 0.0);
    }

    #[test]
    fn json_and_sse_render_the_same_four_part_usage() {
        let expected = TokenUsage {
            uncached_input_tokens: 3,
            output_tokens: 5,
            cache_read_input_tokens: 7,
            cache_write_input_tokens: 4,
        };
        let content = vec![json!({"type": "text", "text": "ok"})];
        let response = render_json(
            "claude-opus-4-7",
            content.clone(),
            "end_turn",
            expected,
            "",
            None,
        );
        let bytes = futures::executor::block_on(async {
            axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap()
        });
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["usage"]["input_tokens"], json!(3));
        assert_eq!(body["usage"]["output_tokens"], json!(5));
        assert_eq!(body["usage"]["cache_creation_input_tokens"], json!(4));
        assert_eq!(body["usage"]["cache_read_input_tokens"], json!(7));

        let events = build_sse_events("claude-opus-4-7", content, "end_turn", expected, None);
        let start_usage = &events
            .iter()
            .find(|event| event.event == "message_start")
            .unwrap()
            .data["message"]["usage"];
        assert_eq!(start_usage["input_tokens"], json!(3));
        assert_eq!(start_usage["cache_creation_input_tokens"], json!(4));
        assert_eq!(start_usage["cache_read_input_tokens"], json!(7));
        let delta_usage = &events
            .iter()
            .find(|event| event.event == "message_delta")
            .unwrap()
            .data["usage"];
        assert_eq!(delta_usage["output_tokens"], json!(5));
    }

    // ---- credit_usage 透传：run_web_search_loop 路径 ----

    fn metering_event(usage: f64) -> MeteringEvent {
        MeteringEvent {
            unit: "credit".to_string(),
            unit_plural: "credits".to_string(),
            usage,
        }
    }

    fn token_usage(input_tokens: i32, output_tokens: i32) -> TokenUsage {
        TokenUsage {
            uncached_input_tokens: input_tokens,
            output_tokens,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
        }
    }

    #[test]
    fn render_json_carries_credit_fields_when_metering_present() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let metering = metering_event(0.42);
        let resp = render_json(
            "claude-opus-4-7",
            content,
            "end_turn",
            token_usage(10, 5),
            "",
            Some(&metering),
        );
        // 把 Response 的 body 序列化为 JSON 再断言。
        let body = resp.into_body();
        let bytes = futures::executor::block_on(async {
            axum::body::to_bytes(body, 64 * 1024).await.unwrap()
        });
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let usage = &v["usage"];
        assert_eq!(usage["credit_usage"], json!(0.42));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
        // 原有字段保持原样
        assert_eq!(usage["input_tokens"], json!(10));
        assert_eq!(usage["output_tokens"], json!(5));
    }

    #[test]
    fn render_json_omits_credit_fields_without_metering() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let resp = render_json(
            "claude-opus-4-7",
            content,
            "end_turn",
            token_usage(10, 5),
            "",
            None,
        );
        let body = resp.into_body();
        let bytes = futures::executor::block_on(async {
            axum::body::to_bytes(body, 64 * 1024).await.unwrap()
        });
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let usage = &v["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
    }

    #[test]
    fn build_sse_events_carries_credit_fields_in_message_delta() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let metering = metering_event(0.99);
        let events = build_sse_events(
            "claude-opus-4-7",
            content,
            "end_turn",
            token_usage(10, 5),
            Some(&metering),
        );
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert_eq!(usage["credit_usage"], json!(0.99));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
        // 原有字段保持原样
        assert_eq!(usage["output_tokens"], json!(5));
    }

    #[test]
    fn build_sse_events_omits_credit_fields_without_metering() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let events = build_sse_events(
            "claude-opus-4-7",
            content,
            "end_turn",
            token_usage(10, 5),
            None,
        );
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
    }
}
