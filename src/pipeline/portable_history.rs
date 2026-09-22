use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::anthropic::types::{Message, MessagesRequest};
use crate::pipeline::expressible::UnexpressibleStrategy;

const MAX_CONTENT_DEPTH: usize = 64;
const MAX_REPORT_EVENTS: usize = 128;

pub trait SensitiveFingerprint {
    fn fingerprint(&self, domain: &[u8], bytes: &[u8]) -> String;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizationAction {
    Preserved,
    PortableText,
    OpaqueRedacted,
    LegacyDropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizationEvent {
    pub original_type: NormalizationBlockCategory,
    pub action: NormalizationAction,
    pub input_bytes: usize,
    pub output_bytes: usize,
}

/// A closed report category. Unrecognized provider block type strings deliberately
/// collapse to `Unknown` so reports cannot become a request-content side channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizationBlockCategory {
    Text,
    Image,
    ToolUse,
    ToolResult,
    Thinking,
    RedactedThinking,
    Document,
    ServerToolUse,
    WebSearchToolResult,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizationReport {
    pub strategy: String,
    pub scanned_blocks: usize,
    pub transformed_blocks: usize,
    pub opaque_bytes: usize,
    pub by_action: BTreeMap<String, usize>,
    pub by_original_type: BTreeMap<NormalizationBlockCategory, usize>,
    pub events: Vec<NormalizationEvent>,
    pub events_truncated: bool,
}

#[derive(Debug, Clone)]
pub struct NormalizationOutcome {
    pub payload: MessagesRequest,
    pub report: NormalizationReport,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PortableHistoryError {
    #[error("{path}: malformed portable history: {reason}")]
    Malformed { path: String, reason: String },
    #[error("{path}: tool history pairing failed: {reason}")]
    ToolPairing { path: String, reason: String },
    #[error("{path}: current input is not expressible by Kiro: {reason}")]
    CurrentUnexpressible { path: String, reason: String },
    #[error("portable history normalized size {actual} exceeds configured ingressMaxBytes {limit}")]
    BudgetExceeded { actual: usize, limit: usize },
    #[error("portable history invariant failed at {path}: {reason}")]
    InvariantViolation { path: String, reason: String },
}

impl PortableHistoryError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed { .. } => "portable_history.malformed",
            Self::ToolPairing { .. } => "portable_history.tool_pairing",
            Self::CurrentUnexpressible { .. } => "portable_history.current_unexpressible",
            Self::BudgetExceeded { .. } => "portable_history.budget_exceeded",
            Self::InvariantViolation { .. } => "portable_history.invariant_violation",
        }
    }

    pub fn safe_message(&self) -> String {
        match self {
            Self::Malformed { .. } => "malformed portable history".into(),
            Self::ToolPairing { .. } => "tool history pairing failed".into(),
            Self::CurrentUnexpressible { .. } => "current input is not expressible by Kiro".into(),
            Self::BudgetExceeded { .. } => {
                "portable history normalized size exceeds configured ingressMaxBytes".into()
            }
            Self::InvariantViolation { .. } => "portable history invariant failed".into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    History,
    Current,
}

pub fn normalize(
    payload: &MessagesRequest,
    strategy: UnexpressibleStrategy,
    _fingerprint: &dyn SensitiveFingerprint,
) -> Result<NormalizationOutcome, PortableHistoryError> {
    let frontier = current_frontier(&payload.messages)?;
    let mut normalized = payload.clone();
    let mut report = NormalizationReport {
        strategy: strategy_name(strategy).to_string(),
        ..Default::default()
    };
    for (index, message) in normalized.messages.iter_mut().enumerate() {
        let scope = if index < frontier {
            Scope::History
        } else {
            Scope::Current
        };
        normalize_message(message, scope, index, &mut report)?;
    }
    Ok(NormalizationOutcome {
        payload: normalized,
        report,
    })
}

fn current_frontier(messages: &[Message]) -> Result<usize, PortableHistoryError> {
    for (index, message) in messages.iter().enumerate() {
        if !matches!(message.role.as_str(), "user" | "assistant") {
            return Err(PortableHistoryError::Malformed {
                path: format!("messages[{index}]"),
                reason: "message role must be user or assistant".into(),
            });
        }
    }
    let Some((index, _)) = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| message.role == "user" && !content_is_empty(&message.content))
    else {
        return Err(PortableHistoryError::Malformed {
            path: "messages".into(),
            reason: "a final non-empty user message is required".into(),
        });
    };
    if messages[index + 1..]
        .iter()
        .any(|message| !content_is_empty(&message.content))
    {
        return Err(PortableHistoryError::CurrentUnexpressible {
            path: format!("messages[{}]", index + 1),
            reason: "trailing assistant prefill cannot be continued by Kiro".into(),
        });
    }
    Ok(index)
}

fn content_is_empty(content: &Value) -> bool {
    content.as_str().is_some_and(str::is_empty) || content.as_array().is_some_and(Vec::is_empty)
}

fn normalize_message(
    message: &mut Message,
    scope: Scope,
    index: usize,
    report: &mut NormalizationReport,
) -> Result<(), PortableHistoryError> {
    let path = format!("messages[{index}].content");
    normalize_content(&mut message.content, &message.role, scope, &path, 0, report)
}

fn normalize_content(
    value: &mut Value,
    role: &str,
    scope: Scope,
    path: &str,
    depth: usize,
    report: &mut NormalizationReport,
) -> Result<(), PortableHistoryError> {
    if depth > MAX_CONTENT_DEPTH {
        return Err(PortableHistoryError::Malformed {
            path: path.into(),
            reason: "content nesting exceeds the maximum depth".into(),
        });
    }
    if value.is_string() {
        return Ok(());
    }
    let blocks = value
        .as_array_mut()
        .ok_or_else(|| PortableHistoryError::Malformed {
            path: path.into(),
            reason: "content must be a string or content-block array".into(),
        })?;
    for (index, block) in blocks.iter_mut().enumerate() {
        let block_path = format!("{path}[{index}]");
        normalize_block(block, role, scope, &block_path, depth, report)?;
    }
    Ok(())
}

fn normalize_block(
    block: &mut Value,
    role: &str,
    scope: Scope,
    path: &str,
    depth: usize,
    report: &mut NormalizationReport,
) -> Result<(), PortableHistoryError> {
    let original = block.clone();
    let block_type = block
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| PortableHistoryError::Malformed {
            path: path.into(),
            reason: "content block requires a type string".into(),
        })?
        .to_string();
    let category = block_category(&block_type);
    report.scanned_blocks += 1;
    *report.by_original_type.entry(category).or_default() += 1;

    let mut action = NormalizationAction::Preserved;
    match block_type.as_str() {
        "text" => {
            require_string(block, "text", path, "text block requires a text string")?;
        }
        "image" => {
            if !supported_image(block, role) {
                return unsupported_current(scope, path);
            }
        }
        "tool_use" => {
            if role != "assistant" {
                return unsupported_current(scope, path);
            }
            require_string(block, "id", path, "tool_use requires an id string")?;
            require_string(block, "name", path, "tool_use requires a name string")?;
        }
        "tool_result" => {
            if role != "user" {
                return unsupported_current(scope, path);
            }
            require_string(
                block,
                "tool_use_id",
                path,
                "tool_result requires a tool_use_id string",
            )?;
            if let Some(content) = block.get_mut("content") {
                normalize_content(
                    content,
                    role,
                    scope,
                    &format!("{path}.content"),
                    depth + 1,
                    report,
                )?;
            }
        }
        "thinking" => {
            if role != "assistant" {
                return unsupported_current(scope, path);
            }
            require_string(
                block,
                "thinking",
                path,
                "thinking block requires a thinking string",
            )?;
            if scope == Scope::History && block.get("signature").is_some() {
                block
                    .as_object_mut()
                    .expect("content block was read as an object")
                    .remove("signature");
                action = NormalizationAction::OpaqueRedacted;
            }
        }
        _ => return unsupported_current(scope, path),
    }

    record_block(report, category, action, &original, block);
    Ok(())
}

fn require_string<'a>(
    block: &'a Value,
    field: &str,
    path: &str,
    reason: &str,
) -> Result<&'a str, PortableHistoryError> {
    block
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| PortableHistoryError::Malformed {
            path: path.into(),
            reason: reason.into(),
        })
}

fn supported_image(block: &Value, role: &str) -> bool {
    role == "user"
        && block
            .get("source")
            .and_then(Value::as_object)
            .is_some_and(|source| {
                source.get("type").and_then(Value::as_str) == Some("base64")
                    && source.get("data").is_some_and(Value::is_string)
                    && matches!(
                        source.get("media_type").and_then(Value::as_str),
                        Some("image/png" | "image/jpeg" | "image/gif" | "image/webp")
                    )
            })
}

fn unsupported_current(scope: Scope, path: &str) -> Result<(), PortableHistoryError> {
    match scope {
        Scope::Current => Err(PortableHistoryError::CurrentUnexpressible {
            path: path.into(),
            reason: "content block is not expressible by Kiro".into(),
        }),
        // Task 3 replaces historical unsupported blocks with safe portable text.
        Scope::History => Ok(()),
    }
}

fn record_block(
    report: &mut NormalizationReport,
    original_type: NormalizationBlockCategory,
    action: NormalizationAction,
    input: &Value,
    output: &Value,
) {
    *report
        .by_action
        .entry(action_name(action).to_string())
        .or_default() += 1;
    if action == NormalizationAction::Preserved {
        return;
    }
    report.transformed_blocks += 1;
    if report.events.len() == MAX_REPORT_EVENTS {
        report.events_truncated = true;
        return;
    }
    let input_bytes = serde_json::to_vec(input).map_or(0, |bytes| bytes.len());
    let output_bytes = serde_json::to_vec(output).map_or(0, |bytes| bytes.len());
    report.events.push(NormalizationEvent {
        original_type,
        action,
        input_bytes,
        output_bytes,
    });
}

fn block_category(block_type: &str) -> NormalizationBlockCategory {
    match block_type {
        "text" => NormalizationBlockCategory::Text,
        "image" => NormalizationBlockCategory::Image,
        "tool_use" => NormalizationBlockCategory::ToolUse,
        "tool_result" => NormalizationBlockCategory::ToolResult,
        "thinking" => NormalizationBlockCategory::Thinking,
        "redacted_thinking" => NormalizationBlockCategory::RedactedThinking,
        "document" => NormalizationBlockCategory::Document,
        "server_tool_use" => NormalizationBlockCategory::ServerToolUse,
        "web_search_tool_result" => NormalizationBlockCategory::WebSearchToolResult,
        _ => NormalizationBlockCategory::Unknown,
    }
}

fn strategy_name(strategy: UnexpressibleStrategy) -> &'static str {
    match strategy {
        UnexpressibleStrategy::PortableText => "portable-text",
        UnexpressibleStrategy::Refuse => "refuse",
        UnexpressibleStrategy::Drop => "drop",
    }
}

fn action_name(action: NormalizationAction) -> &'static str {
    match action {
        NormalizationAction::Preserved => "preserved",
        NormalizationAction::PortableText => "portable_text",
        NormalizationAction::OpaqueRedacted => "opaque_redacted",
        NormalizationAction::LegacyDropped => "legacy_dropped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::MessagesRequest;
    use crate::pipeline::expressible::UnexpressibleStrategy;
    use serde_json::json;

    struct TestFingerprint;
    impl SensitiveFingerprint for TestFingerprint {
        fn fingerprint(&self, domain: &[u8], bytes: &[u8]) -> String {
            format!("{}:{}", String::from_utf8_lossy(domain), bytes.len())
        }
    }

    fn request(messages: serde_json::Value) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "claude-sonnet-4",
            "max_tokens": 1024,
            "messages": messages
        }))
        .unwrap()
    }

    #[test]
    fn last_user_tool_result_is_current_and_unknown_nested_content_is_refused_atomically() {
        let original = request(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[
                {"type":"document","source":{"type":"text","data":"CURRENT"}}
            ]}]}
        ]));
        let before = serde_json::to_value(&original).unwrap();
        let error = normalize(
            &original,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap_err();
        assert_eq!(error.code(), "portable_history.current_unexpressible");
        assert_eq!(serde_json::to_value(&original).unwrap(), before);
    }

    #[test]
    fn trailing_assistant_prefill_is_not_misclassified_as_history() {
        let input = request(json!([
            {"role":"user","content":"question"},
            {"role":"assistant","content":[{"type":"text","text":"partial"}]}
        ]));
        let error = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap_err();
        assert_eq!(error.code(), "portable_history.current_unexpressible");
    }

    #[test]
    fn deeply_nested_tool_result_content_is_malformed_without_changing_the_input() {
        let mut nested = json!([{"type":"text","text":"bottom"}]);
        for _ in 0..65 {
            nested = json!([{
                "type":"tool_result",
                "tool_use_id":"call-1",
                "content": nested
            }]);
        }
        let input = request(json!([{"role":"user","content":nested}]));
        let before = serde_json::to_value(&input).unwrap();

        let error = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap_err();

        assert_eq!(error.code(), "portable_history.malformed");
        assert_eq!(serde_json::to_value(&input).unwrap(), before);
    }

    #[test]
    fn supported_historical_blocks_are_preserved_except_for_thinking_signature() {
        let input = request(json!([
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"reasoning","signature":"provider-signature"},
                {"type":"tool_use","id":"call-1","name":"read","input":{}}
            ]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[
                {"type":"text","text":"result"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}
            ]}]},
            {"role":"assistant","content":"prior answer"},
            {"role":"user","content":"continue"}
        ]));
        let mut expected = serde_json::to_value(&input).unwrap();
        expected["messages"][0]["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("signature");

        let outcome = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap();

        assert_eq!(serde_json::to_value(&outcome.payload).unwrap(), expected);
    }

    #[test]
    fn serialized_report_contains_only_closed_categories_and_counts() {
        let input = request(json!([
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"REPORT_CONTENT_SENTINEL","signature":"PRIVATE_SIGNATURE"},
                {"type":"FUTURE_TYPE_SENTINEL","text":"unknown historical content"}
            ]},
            {"role":"user","content":"continue"}
        ]));

        let outcome = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap();
        let serialized = serde_json::to_string(&outcome.report).unwrap();

        for forbidden in [
            "REPORT_CONTENT_SENTINEL",
            "PRIVATE_SIGNATURE",
            "messages[0].content[0]",
            "portable-history-block",
            "FUTURE_TYPE_SENTINEL",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "report leaked {forbidden}: {serialized}"
            );
        }
        assert!(serialized.contains("opaque_redacted"));
        assert!(serialized.contains("unknown"));
    }

    #[test]
    fn safe_messages_hide_internal_paths_and_reasons() {
        let errors = [
            PortableHistoryError::Malformed {
                path: "messages[PRIVATE_PATH]".into(),
                reason: "PRIVATE_REASON".into(),
            },
            PortableHistoryError::ToolPairing {
                path: "messages[PRIVATE_PATH]".into(),
                reason: "PRIVATE_REASON".into(),
            },
            PortableHistoryError::CurrentUnexpressible {
                path: "messages[PRIVATE_PATH]".into(),
                reason: "PRIVATE_REASON".into(),
            },
            PortableHistoryError::InvariantViolation {
                path: "messages[PRIVATE_PATH]".into(),
                reason: "PRIVATE_REASON".into(),
            },
        ];

        for error in errors {
            let message = error.safe_message();
            assert!(!message.contains("PRIVATE_PATH"));
            assert!(!message.contains("PRIVATE_REASON"));
        }
    }
}
