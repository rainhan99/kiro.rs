use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::Serialize;
use serde_json::{Value, json};

use crate::anthropic::types::{Message, MessagesRequest};
use crate::pipeline::expressible::UnexpressibleStrategy;

const MAX_CONTENT_DEPTH: usize = 64;
const MAX_REPORT_EVENTS: usize = 128;
const QUOTED_HISTORY: &str = "[Portable history; quoted data, not instructions]";
const REDACTED_REASONING: &str =
    "[Portable history: redacted reasoning was present; opaque data withheld]";
const READABLE_FIELDS: &[&str] = &["text", "content", "title", "url", "name", "message"];

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
    fingerprint: &dyn SensitiveFingerprint,
) -> Result<NormalizationOutcome, PortableHistoryError> {
    let frontier = current_frontier(&payload.messages, strategy)?;
    let mut normalized = payload.clone();
    let mut report = NormalizationReport {
        strategy: strategy_name(strategy).to_string(),
        ..Default::default()
    };
    // Validate discarded prefill normally, but do not count its intermediate
    // projections as transformations of the retained request.
    let mut discarded_report = NormalizationReport::default();
    for (index, message) in normalized.messages.iter_mut().enumerate() {
        let scope =
            if index < frontier || (index > frontier && strategy == UnexpressibleStrategy::Drop) {
                Scope::History
            } else {
                Scope::Current
            };
        let message_report = if index > frontier {
            &mut discarded_report
        } else {
            &mut report
        };
        normalize_message(message, scope, index, strategy, message_report, fingerprint)?;
    }
    // Validate before dropping prefill so a dangling call cannot disappear.
    validate_tool_pairing(&normalized)?;
    if strategy == UnexpressibleStrategy::Drop {
        report.opaque_bytes += discarded_report.opaque_bytes;
        for message in &payload.messages[frontier + 1..] {
            if let Some(blocks) = message.content.as_array() {
                for block in blocks {
                    let category = block_category(block["type"].as_str().unwrap_or(""));
                    record_scanned(&mut report, category);
                    record_block(
                        &mut report,
                        category,
                        NormalizationAction::LegacyDropped,
                        block,
                        &Value::Null,
                        fingerprint,
                    );
                }
            } else if !content_is_empty(&message.content) {
                record_scanned(&mut report, NormalizationBlockCategory::Text);
                record_block(
                    &mut report,
                    NormalizationBlockCategory::Text,
                    NormalizationAction::LegacyDropped,
                    &message.content,
                    &Value::Null,
                    fingerprint,
                );
            }
        }
    }
    // The effective frontier must also be the physical wire tail. Empty trailing
    // messages carry no content and are not an intentional-loss event.
    normalized.messages.truncate(frontier + 1);
    validate_tool_pairing(&normalized)?;
    Ok(NormalizationOutcome {
        payload: normalized,
        report,
    })
}

fn current_frontier(
    messages: &[Message],
    strategy: UnexpressibleStrategy,
) -> Result<usize, PortableHistoryError> {
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
    if strategy != UnexpressibleStrategy::Drop
        && messages[index + 1..]
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
    strategy: UnexpressibleStrategy,
    report: &mut NormalizationReport,
    fingerprint: &dyn SensitiveFingerprint,
) -> Result<(), PortableHistoryError> {
    let path = format!("messages[{index}].content");
    normalize_content(
        &mut message.content,
        &message.role,
        scope,
        &path,
        0,
        strategy,
        report,
        fingerprint,
    )
}

fn normalize_content(
    value: &mut Value,
    role: &str,
    scope: Scope,
    path: &str,
    depth: usize,
    strategy: UnexpressibleStrategy,
    report: &mut NormalizationReport,
    fingerprint: &dyn SensitiveFingerprint,
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
    let projected_searches = if scope == Scope::History && role == "assistant" {
        normalize_server_searches(blocks, path, report, fingerprint)?
    } else {
        BTreeSet::new()
    };
    for (index, block) in blocks.iter_mut().enumerate() {
        if projected_searches.contains(&index) {
            continue;
        }
        let block_path = format!("{path}[{index}]");
        normalize_block(
            block,
            role,
            scope,
            &block_path,
            depth,
            strategy,
            report,
            fingerprint,
        )?;
    }
    // Only explicit legacy removals create null here; malformed input nulls fail above.
    blocks.retain(|block| !block.is_null());
    Ok(())
}

fn normalize_block(
    block: &mut Value,
    role: &str,
    scope: Scope,
    path: &str,
    depth: usize,
    strategy: UnexpressibleStrategy,
    report: &mut NormalizationReport,
    fingerprint: &dyn SensitiveFingerprint,
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
                if let Some(legacy_action) = legacy_unexpressible(block, strategy, path)? {
                    record_block(
                        report,
                        category,
                        legacy_action,
                        &original,
                        block,
                        fingerprint,
                    );
                    return Ok(());
                }
                if scope == Scope::Current {
                    return unsupported_current(scope, path);
                }
                *block = image_projection(&original);
                action = NormalizationAction::PortableText;
            }
        }
        "tool_use" => {
            if role != "assistant" {
                return invalid_block_role(scope, path);
            }
            require_string(block, "id", path, "tool_use requires an id string")?;
            require_string(block, "name", path, "tool_use requires a name string")?;
            if block.get("input").is_some_and(|input| !input.is_object()) {
                return Err(malformed(
                    path,
                    "tool_use input must be an object when supplied",
                ));
            }
        }
        "tool_result" => {
            if role != "user" {
                return invalid_block_role(scope, path);
            }
            require_string(
                block,
                "tool_use_id",
                path,
                "tool_result requires a tool_use_id string",
            )?;
            if block
                .get("is_error")
                .is_some_and(|value| !value.is_boolean())
            {
                return Err(malformed(
                    path,
                    "tool_result is_error must be boolean when supplied",
                ));
            }
            if let Some(content) = block.get_mut("content") {
                normalize_content(
                    content,
                    role,
                    scope,
                    &format!("{path}.content"),
                    depth + 1,
                    strategy,
                    report,
                    fingerprint,
                )?;
            }
        }
        "thinking" => {
            if role != "assistant" {
                return invalid_block_role(scope, path);
            }
            require_string(
                block,
                "thinking",
                path,
                "thinking block requires a thinking string",
            )?;
            if scope == Scope::History && block.get("signature").is_some() {
                report.opaque_bytes += block
                    .get("signature")
                    .and_then(Value::as_str)
                    .map_or(0, str::len);
                block
                    .as_object_mut()
                    .expect("content block was read as an object")
                    .remove("signature");
                action = NormalizationAction::OpaqueRedacted;
            }
        }
        "redacted_thinking" => {
            if role != "assistant" {
                return invalid_block_role(scope, path);
            }
            if scope == Scope::Current {
                return unsupported_current(scope, path);
            }
            let data = require_string(
                block,
                "data",
                path,
                "redacted thinking requires opaque data",
            )?;
            report.opaque_bytes += data.len();
            *block = text_block(REDACTED_REASONING.to_string());
            action = NormalizationAction::OpaqueRedacted;
        }
        "document" => {
            if let Some(legacy_action) = legacy_unexpressible(block, strategy, path)? {
                record_block(
                    report,
                    category,
                    legacy_action,
                    &original,
                    block,
                    fingerprint,
                );
                return Ok(());
            }
            if scope == Scope::Current {
                return unsupported_current(scope, path);
            }
            *block = document_projection(&original);
            action = NormalizationAction::PortableText;
        }
        "server_tool_use" | "web_search_tool_result" => {
            return match scope {
                Scope::Current => unsupported_current(scope, path),
                Scope::History => Err(PortableHistoryError::Malformed {
                    path: path.into(),
                    reason: "server history records must be consumed in an assistant block array"
                        .into(),
                }),
            };
        }
        _ => {
            if let Some(legacy_action) = legacy_unexpressible(block, strategy, path)? {
                action = legacy_action;
            } else if scope == Scope::History {
                *block = unknown_projection(&original, &block_type);
                action = NormalizationAction::PortableText;
            } else {
                return unsupported_current(scope, path);
            }
        }
    }

    record_block(report, category, action, &original, block, fingerprint);
    Ok(())
}

fn legacy_unexpressible(
    block: &mut Value,
    strategy: UnexpressibleStrategy,
    path: &str,
) -> Result<Option<NormalizationAction>, PortableHistoryError> {
    match strategy {
        UnexpressibleStrategy::PortableText => Ok(None),
        UnexpressibleStrategy::Refuse => Err(PortableHistoryError::CurrentUnexpressible {
            path: path.into(),
            reason: "legacy refuse strategy rejects unexpressible content".into(),
        }),
        UnexpressibleStrategy::Drop => {
            *block = Value::Null;
            Ok(Some(NormalizationAction::LegacyDropped))
        }
    }
}

fn text_block(text: String) -> Value {
    json!({"type": "text", "text": text})
}

fn quoted_text(text: String) -> Value {
    text_block(format!("{QUOTED_HISTORY}\n{text}"))
}

fn sanitize_metadata(value: Option<&str>) -> Option<String> {
    let text: String = value?
        .chars()
        .take(256)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    (!text.is_empty()).then_some(text)
}

fn attachment_text(kind: &str, title: Option<&str>, media_type: Option<&str>) -> Value {
    let title = sanitize_metadata(title);
    let media_type = sanitize_metadata(media_type);
    let mut text = String::from("[Portable history attachment");
    match (kind, title.as_deref()) {
        (_, Some(title)) => text.push_str(&format!(": {title}")),
        ("image", None) => text.push_str(": image"),
        _ => text.push_str(": document"),
    }
    if let Some(media_type) = media_type {
        text.push_str(&format!(" ({media_type})"));
    }
    text.push_str("; binary content unavailable to Kiro]");
    text_block(text)
}

fn document_projection(block: &Value) -> Value {
    let title = block.get("title").and_then(Value::as_str);
    let source = block.get("source").and_then(Value::as_object);
    let media_type = source
        .and_then(|source| source.get("media_type"))
        .and_then(Value::as_str);
    match source
        .and_then(|source| source.get("type"))
        .and_then(Value::as_str)
    {
        Some("text") => source
            .and_then(|source| {
                source
                    .get("data")
                    .or_else(|| source.get("text"))
                    .and_then(Value::as_str)
            })
            .map(|text| quoted_text(text.to_string()))
            .unwrap_or_else(|| attachment_text("document", title, media_type)),
        Some("base64") if media_type.is_some_and(|media| media.starts_with("text/")) => source
            .and_then(|source| source.get("data"))
            .and_then(Value::as_str)
            .and_then(|data| BASE64.decode(data).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(quoted_text)
            .unwrap_or_else(|| attachment_text("document", title, media_type)),
        _ => attachment_text("document", title, media_type),
    }
}

fn image_projection(block: &Value) -> Value {
    let media_type = block
        .get("source")
        .and_then(Value::as_object)
        .and_then(|source| source.get("media_type"))
        .and_then(Value::as_str);
    attachment_text("image", None, media_type)
}

fn unknown_projection(block: &Value, block_type: &str) -> Value {
    let readable = READABLE_FIELDS
        .iter()
        .filter_map(|field| {
            block
                .get(*field)
                .and_then(Value::as_str)
                .map(|value| format!("{field}: {value}"))
        })
        .collect::<Vec<_>>();
    if readable.is_empty() {
        let block_type = sanitize_metadata(Some(block_type)).unwrap_or_else(|| "unknown".into());
        text_block(format!(
            "[Portable history: unsupported block type \"{block_type}\" was present; no readable content]"
        ))
    } else {
        quoted_text(readable.join("\n"))
    }
}

fn normalize_server_searches(
    blocks: &mut [Value],
    path: &str,
    report: &mut NormalizationReport,
    fingerprint: &dyn SensitiveFingerprint,
) -> Result<BTreeSet<usize>, PortableHistoryError> {
    let mut seen_uses = BTreeSet::new();
    let mut pending = BTreeMap::new();
    let mut seen_results = BTreeSet::new();
    let mut pairs = Vec::new();

    for (index, block) in blocks.iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("server_tool_use") => {
                if block.get("name").and_then(Value::as_str) != Some("web_search") {
                    return Err(tool_pairing_error(
                        &format!("{path}[{index}]"),
                        "unsupported server tool history",
                    ));
                }
                let id = block.get("id").and_then(Value::as_str).ok_or_else(|| {
                    tool_pairing_error(&format!("{path}[{index}]"), "server search requires an id")
                })?;
                if !seen_uses.insert(id.to_string()) {
                    return Err(tool_pairing_error(
                        &format!("{path}[{index}]"),
                        "duplicate server search use",
                    ));
                }
                let block_path = format!("{path}[{index}]");
                let input = block
                    .get("input")
                    .filter(|input| input.is_object())
                    .ok_or_else(|| {
                        malformed(&block_path, "server search requires an input object")
                    })?;
                let query = require_string(
                    input,
                    "query",
                    &block_path,
                    "server search requires a query string",
                )?
                .to_owned();
                pending.insert(id.to_string(), (index, query));
            }
            Some("web_search_tool_result") => {
                let result_text = search_result_text(block, &format!("{path}[{index}]"))?;
                let id = match block.get("tool_use_id") {
                    Some(Value::String(id)) => id.to_string(),
                    Some(_) => {
                        return Err(tool_pairing_error(
                            &format!("{path}[{index}]"),
                            "server search result has a malformed tool_use_id",
                        ));
                    }
                    None if pending.len() == 1 => {
                        pending.keys().next().cloned().expect("one pending id")
                    }
                    None => {
                        return Err(tool_pairing_error(
                            &format!("{path}[{index}]"),
                            "ambiguous server search result",
                        ));
                    }
                };
                let Some((use_index, query)) = pending.remove(&id) else {
                    return Err(tool_pairing_error(
                        &format!("{path}[{index}]"),
                        "orphan server search result",
                    ));
                };
                if !seen_results.insert(id) {
                    return Err(tool_pairing_error(
                        &format!("{path}[{index}]"),
                        "duplicate server search result",
                    ));
                }
                pairs.push((use_index, index, query, result_text));
            }
            _ => {}
        }
    }
    if !pending.is_empty() {
        return Err(tool_pairing_error(
            path,
            "server search is missing its result",
        ));
    }

    let mut projected = BTreeSet::new();
    for (use_index, result_index, query, result_text) in pairs {
        let original_use = blocks[use_index].clone();
        let original_result = blocks[result_index].clone();
        let use_text = format!("Web search query: {query}");
        blocks[use_index] = quoted_text(use_text);
        blocks[result_index] = quoted_text(result_text);
        // Completed public search records are expressible after projection under
        // every strategy. Legacy policy applies only to unsupported content.
        let action = NormalizationAction::PortableText;
        report.opaque_bytes += search_opaque_bytes(&original_result);
        record_scanned(report, NormalizationBlockCategory::ServerToolUse);
        record_scanned(report, NormalizationBlockCategory::WebSearchToolResult);
        record_block(
            report,
            NormalizationBlockCategory::ServerToolUse,
            action,
            &original_use,
            &blocks[use_index],
            fingerprint,
        );
        record_block(
            report,
            NormalizationBlockCategory::WebSearchToolResult,
            action,
            &original_result,
            &blocks[result_index],
            fingerprint,
        );
        projected.insert(use_index);
        projected.insert(result_index);
    }
    Ok(projected)
}

fn search_result_text(block: &Value, path: &str) -> Result<String, PortableHistoryError> {
    const PUBLIC_SEARCH_FIELDS: &[&str] =
        &["title", "url", "content", "snippet", "error", "message"];

    fn collect_public_fields(
        value: &Value,
        fields: &mut Vec<String>,
        path: &str,
    ) -> Result<(), PortableHistoryError> {
        for field in PUBLIC_SEARCH_FIELDS {
            if let Some(value) = value.get(*field) {
                let value = value
                    .as_str()
                    .ok_or_else(|| malformed(path, "public search fields must be strings"))?;
                fields.push(format!("{field}: {value}"));
            }
        }
        Ok(())
    }

    let mut fields = Vec::new();
    match block.get("content") {
        // Older gateway replies also used plain readable result content.
        Some(Value::String(content)) => fields.push(format!("content: {content}")),
        Some(Value::Array(content)) => {
            for result in content {
                if result.get("type").and_then(Value::as_str) != Some("web_search_result") {
                    return Err(malformed(
                        path,
                        "search result entries must be web_search_result objects",
                    ));
                }
                require_string(
                    result,
                    "title",
                    path,
                    "search result requires a title string",
                )?;
                require_string(result, "url", path, "search result requires a URL string")?;
                if result
                    .get("encrypted_content")
                    .is_some_and(|value| !value.is_string())
                {
                    return Err(malformed(path, "encrypted search content must be a string"));
                }
                collect_public_fields(result, &mut fields, path)?;
            }
        }
        Some(content @ Value::Object(_)) => {
            if content.get("type").and_then(Value::as_str) != Some("web_search_tool_result_error") {
                return Err(malformed(
                    path,
                    "search error content must be a public search error object",
                ));
            }
            let code = require_string(
                content,
                "error_code",
                path,
                "search error requires an error code",
            )?;
            if !matches!(
                code,
                "invalid_tool_input"
                    | "unavailable"
                    | "max_uses_exceeded"
                    | "too_many_requests"
                    | "query_too_long"
                    | "request_too_large"
            ) {
                return Err(malformed(path, "unsupported public search error code"));
            }
            // The public error code is a closed vocabulary; arbitrary message or
            // provider-private fields on the error object never enter the text.
            return Ok(format!("Web search failed: {code}"));
        }
        _ => {
            return Err(malformed(
                path,
                "search result content must be a string, result array or public error object",
            ));
        }
    }
    Ok(if fields.is_empty() {
        "Web search result record".into()
    } else {
        fields.join("\n")
    })
}

fn search_opaque_bytes(block: &Value) -> usize {
    fn encrypted_bytes(value: &Value) -> usize {
        value
            .get("encrypted_content")
            .and_then(Value::as_str)
            .map_or(0, str::len)
    }
    encrypted_bytes(block)
        + match block.get("content") {
            Some(Value::Array(results)) => results.iter().map(encrypted_bytes).sum(),
            Some(content @ Value::Object(_)) => encrypted_bytes(content),
            _ => 0,
        }
}

fn malformed(path: &str, reason: &str) -> PortableHistoryError {
    PortableHistoryError::Malformed {
        path: path.into(),
        reason: reason.into(),
    }
}

fn tool_pairing_error(path: &str, reason: &str) -> PortableHistoryError {
    PortableHistoryError::ToolPairing {
        path: path.into(),
        reason: reason.into(),
    }
}

fn invalid_block_role(scope: Scope, path: &str) -> Result<(), PortableHistoryError> {
    match scope {
        Scope::History => Err(PortableHistoryError::Malformed {
            path: path.into(),
            reason: "content block is invalid for the message role".into(),
        }),
        Scope::Current => unsupported_current(scope, path),
    }
}

/// Validates client-visible top-level tool calls after server-search records have
/// been converted to text. Nested tool-result content is deliberately not a
/// separate client tool stream.
pub fn validate_tool_pairing(payload: &MessagesRequest) -> Result<(), PortableHistoryError> {
    let mut seen_uses = BTreeSet::new();
    let mut pending = BTreeSet::new();
    let mut seen_results = BTreeSet::new();

    for (message_index, message) in payload.messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            if message.content.is_string() {
                continue;
            }
            return Err(PortableHistoryError::Malformed {
                path: format!("messages[{message_index}].content"),
                reason: "content must be a string or content-block array".into(),
            });
        };
        for (block_index, block) in blocks.iter().enumerate() {
            let path = format!("messages[{message_index}].content[{block_index}]");
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let id = require_string(block, "id", &path, "tool_use requires an id string")?;
                    if !seen_uses.insert(id.to_string()) {
                        return Err(tool_pairing_error(&path, "duplicate tool_use id"));
                    }
                    pending.insert(id.to_string());
                }
                Some("tool_result") => {
                    let id = require_string(
                        block,
                        "tool_use_id",
                        &path,
                        "tool_result requires a tool_use_id string",
                    )?;
                    if !seen_results.insert(id.to_string()) || !pending.remove(id) {
                        return Err(tool_pairing_error(&path, "orphan or duplicate tool_result"));
                    }
                }
                _ => {}
            }
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(tool_pairing_error(
            "messages",
            "tool_use is missing its result",
        ))
    }
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
    fingerprint: &dyn SensitiveFingerprint,
) {
    *report
        .by_action
        .entry(action_name(action).to_string())
        .or_default() += 1;
    if action == NormalizationAction::Preserved {
        return;
    }
    report.transformed_blocks += 1;
    record_event(report, original_type, action, input, output, fingerprint);
}

fn record_event(
    report: &mut NormalizationReport,
    original_type: NormalizationBlockCategory,
    action: NormalizationAction,
    input: &Value,
    output: &Value,
    fingerprint: &dyn SensitiveFingerprint,
) {
    let input = serde_json::to_vec(input).unwrap_or_default();
    // Fingerprint every transformed original even after detailed event storage
    // reaches its cap. The digest is intentionally not retained in the report.
    let _ = fingerprint.fingerprint(b"portable-history-block", &input);
    if report.events.len() == MAX_REPORT_EVENTS {
        report.events_truncated = true;
        return;
    }
    let input_bytes = input.len();
    let output_bytes = serde_json::to_vec(output).map_or(0, |bytes| bytes.len());
    report.events.push(NormalizationEvent {
        original_type,
        action,
        input_bytes,
        output_bytes,
    });
}

fn record_scanned(report: &mut NormalizationReport, category: NormalizationBlockCategory) {
    report.scanned_blocks += 1;
    *report.by_original_type.entry(category).or_default() += 1;
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
pub(crate) mod tests {
    use super::*;
    use crate::anthropic::types::MessagesRequest;
    use crate::pipeline::expressible::UnexpressibleStrategy;
    use base64::Engine;
    use serde_json::json;
    use std::cell::Cell;

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
    fn final_review_empty_tail_is_trimmed_without_loss_events() {
        for strategy in [
            UnexpressibleStrategy::PortableText,
            UnexpressibleStrategy::Refuse,
            UnexpressibleStrategy::Drop,
        ] {
            for tail in [json!(""), json!([])] {
                let input = request(json!([
                    {"role":"user","content":"actual frontier"},
                    {"role":"assistant","content":tail},
                    {"role":"user","content":tail}
                ]));
                let before = serde_json::to_value(&input).unwrap();
                let normalized = normalize(&input, strategy, &TestFingerprint).unwrap();
                assert_eq!(normalized.payload.messages.len(), 1);
                assert_eq!(normalized.report.transformed_blocks, 0);
                assert!(normalized.report.events.is_empty());
                assert_eq!(serde_json::to_value(input).unwrap(), before);
            }
        }
    }

    // Shared invalid client shapes exercise the normalizer, prepare atomicity and
    // the real HTTP error mapper against the same input classes.
    pub(crate) fn final_review_malformed_requests() -> Vec<MessagesRequest> {
        let mut cases = Vec::new();
        for bad in [
            json!(null),
            json!(7),
            json!([]),
            json!("PRIVATE_SHAPE"),
            json!(false),
        ] {
            cases.push(request(json!([
                {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"read","input":bad}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"ok"}]}
            ])));
        }
        for bad in [
            json!(null),
            json!(7),
            json!([]),
            json!("PRIVATE_SHAPE"),
            json!({}),
        ] {
            cases.push(request(json!([
                {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"read"}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","is_error":bad,"content":"ok"}]}
            ])));
        }
        for (field, bad) in [
            ("input", json!(null)),
            ("input", json!([])),
            ("input", json!(7)),
            ("input", json!({"query":7})),
            ("input", json!({"query":null})),
            ("input", json!({"query":[]})),
            ("input", json!({})),
            ("content", json!(null)),
            ("content", json!(7)),
            ("content", json!(false)),
            ("content", json!({})),
            ("content", json!([7])),
            (
                "content",
                json!([{"type":"web_search_result","title":7,"url":"https://example.invalid"}]),
            ),
            (
                "content",
                json!([{"type":"web_search_result","title":"Source","url":null}]),
            ),
            (
                "content",
                json!([{"type":"web_search_result","title":"Source","url":"https://example.invalid","snippet":{}}]),
            ),
            (
                "content",
                json!({"type":"web_search_tool_result_error","error_code":7}),
            ),
            (
                "content",
                json!({"type":"web_search_tool_result_error","error_code":"PRIVATE_SHAPE"}),
            ),
        ] {
            let mut blocks = json!([
                {"type":"server_tool_use","id":"search-1","name":"web_search","input":{"query":"query"}},
                {"type":"web_search_tool_result","tool_use_id":"search-1","content":[]}
            ]);
            blocks[if field == "input" { 0 } else { 1 }][field] = bad;
            cases.push(request(json!([
                {"role":"assistant","content":blocks}, {"role":"user","content":"continue"}
            ])));
        }
        cases
    }

    #[test]
    fn final_review_known_malformed_shapes_fail_atomically_in_every_strategy() {
        let mut accepted_or_misclassified = Vec::new();
        for strategy in [
            UnexpressibleStrategy::PortableText,
            UnexpressibleStrategy::Refuse,
            UnexpressibleStrategy::Drop,
        ] {
            for (index, input) in final_review_malformed_requests().into_iter().enumerate() {
                let before = serde_json::to_value(&input).unwrap();
                let result = normalize(&input, strategy, &TestFingerprint);
                if !matches!(result, Err(PortableHistoryError::Malformed { .. })) {
                    accepted_or_misclassified.push((index, strategy));
                }
                assert_eq!(serde_json::to_value(&input).unwrap(), before);
            }
        }
        assert!(
            accepted_or_misclassified.is_empty(),
            "malformed cases accepted or misclassified: {accepted_or_misclassified:?}"
        );
    }

    #[test]
    fn final_review_public_search_errors_remain_distinct_from_empty_success() {
        let mut texts = Vec::new();
        for content in [
            json!([]),
            json!({
                "type":"web_search_tool_result_error", "error_code":"max_uses_exceeded",
                "signature":"PRIVATE_SIGNATURE", "encrypted_content":"PRIVATE_CIPHERTEXT",
                "arbitrary":"PRIVATE_EXTRA"
            }),
        ] {
            let input = request(json!([
                {"role":"assistant","content":[
                    {"type":"server_tool_use","id":"search-1","name":"web_search","input":{"query":"query"}},
                    {"type":"web_search_tool_result","tool_use_id":"search-1","content":content}
                ]}, {"role":"user","content":"continue"}
            ]));
            let outcome = normalize(
                &input,
                UnexpressibleStrategy::PortableText,
                &TestFingerprint,
            )
            .unwrap();
            let text = outcome.payload.messages[0].content[1]["text"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(!text.contains("PRIVATE_"));
            let again = normalize(
                &outcome.payload,
                UnexpressibleStrategy::PortableText,
                &TestFingerprint,
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(&again.payload).unwrap(),
                serde_json::to_value(&outcome.payload).unwrap()
            );
            texts.push(text);
        }
        assert_ne!(texts[0], texts[1]);
        assert!(texts[1].contains("Web search failed: max_uses_exceeded"));
    }

    #[test]
    fn final_review_drop_prefill_records_each_original_block_once() {
        let blocks = json!([
            {"type":"text","text":"partial"},
            {"type":"thinking","thinking":"reasoning","signature":"sig"},
            {"type":"document","source":{"type":"text","data":"doc"}},
            {"type":"FUTURE_PRIVATE_TYPE","text":"other"},
            {"type":"server_tool_use","id":"s","name":"web_search","input":{"query":"q"}},
            {"type":"web_search_tool_result","tool_use_id":"s","content":[]}
        ]);
        let input = request(json!([
            {"role":"user","content":"continue"}, {"role":"assistant","content":blocks}
        ]));
        let outcome = normalize(&input, UnexpressibleStrategy::Drop, &TestFingerprint).unwrap();
        assert_eq!(outcome.payload.messages.len(), 1);
        assert_eq!(outcome.report.scanned_blocks, 6);
        assert_eq!(outcome.report.transformed_blocks, 6);
        assert_eq!(
            outcome.report.by_action,
            BTreeMap::from([("legacy_dropped".into(), 6)])
        );
        for (index, category) in [
            NormalizationBlockCategory::Text,
            NormalizationBlockCategory::Thinking,
            NormalizationBlockCategory::Document,
            NormalizationBlockCategory::Unknown,
            NormalizationBlockCategory::ServerToolUse,
            NormalizationBlockCategory::WebSearchToolResult,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(outcome.report.by_original_type[&category], 1);
            let event = &outcome.report.events[index];
            assert_eq!(event.original_type, category);
            assert_eq!(event.action, NormalizationAction::LegacyDropped);
            assert_eq!(
                event.input_bytes,
                serde_json::to_vec(&blocks[index]).unwrap().len()
            );
        }
        let dangling = request(json!([
            {"role":"user","content":"continue"},
            {"role":"assistant","content":[{"type":"tool_use","id":"dangling","name":"read","input":{}}]}
        ]));
        assert!(matches!(
            normalize(&dangling, UnexpressibleStrategy::Drop, &TestFingerprint),
            Err(PortableHistoryError::ToolPairing { .. })
        ));
    }

    #[test]
    fn final_review_opaque_bytes_count_all_removed_values_exactly() {
        let input = request(json!([
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"kept","signature":"签名"},
                {"type":"redacted_thinking","data":"hidden"},
                {"type":"server_tool_use","id":"s","name":"web_search","input":{"query":"q"}},
                {"type":"web_search_tool_result","tool_use_id":"s","content":[
                    {"type":"web_search_result","title":"one","url":"https://example.invalid/1","encrypted_content":"cipher"},
                    {"type":"web_search_result","title":"two","url":"https://example.invalid/2","encrypted_content":"密"}
                ]}
            ]}, {"role":"user","content":"continue"}
        ]));
        let outcome = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap();
        assert_eq!(outcome.report.opaque_bytes, 21); // 6 + 6 + 6 + 3 UTF-8 bytes.
        let serialized = serde_json::to_string(&outcome.payload).unwrap();
        let report = serde_json::to_string(&outcome.report).unwrap();
        for private in ["签名", "hidden", "cipher", "密"] {
            assert!(!serialized.contains(private));
            assert!(!report.contains(private));
        }
        assert_eq!(
            normalize(
                &outcome.payload,
                UnexpressibleStrategy::PortableText,
                &TestFingerprint
            )
            .unwrap()
            .report
            .opaque_bytes,
            0
        );
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
    fn provider_private_fields_never_reach_the_normalized_request() {
        let input = request(json!([
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"readable reasoning","signature":"SIGNATURE_SENTINEL"},
                {"type":"redacted_thinking","data":"REDACTED_SENTINEL"},
                {"type":"server_tool_use","id":"search-1","name":"web_search","input":{"query":"rust release"}},
                {"type":"web_search_tool_result","tool_use_id":"search-1","content":[{
                    "type":"web_search_result","title":"Release notes","url":"https://example.invalid/release",
                    "encrypted_content":"ENCRYPTED_SENTINEL"
                }]}
            ]},
            {"role":"user","content":"continue"}
        ]));
        let outcome = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap();
        let serialized = serde_json::to_string(&outcome.payload).unwrap();
        assert!(serialized.contains("readable reasoning"));
        for forbidden in [
            "SIGNATURE_SENTINEL",
            "REDACTED_SENTINEL",
            "ENCRYPTED_SENTINEL",
        ] {
            assert!(!serialized.contains(forbidden), "leaked {forbidden}");
        }
        assert_eq!(outcome.report.transformed_blocks, 4);
        assert_eq!(outcome.report.scanned_blocks, 4);
    }

    #[test]
    fn historical_nested_document_becomes_quoted_text_but_binary_stays_out() {
        let input = request(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[
                {"type":"document","title":"notes.txt","source":{"type":"text","media_type":"text/plain","data":"PORTABLE_TEXT"}},
                {"type":"document","title":"report.pdf","source":{"type":"base64","media_type":"application/pdf","data":"BINARY_SENTINEL"}},
                {"type":"future_result","text":"FUTURE_READABLE","signature":"FUTURE_SECRET"}
            ]}]},
            {"role":"assistant","content":"done"},
            {"role":"user","content":"continue"}
        ]));
        let outcome = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap();
        let serialized = serde_json::to_string(&outcome.payload).unwrap();
        assert!(serialized.contains("PORTABLE_TEXT"));
        assert!(serialized.contains("report.pdf"));
        assert!(serialized.contains("application/pdf"));
        assert!(serialized.contains("FUTURE_READABLE"));
        assert!(!serialized.contains("BINARY_SENTINEL"));
        assert!(!serialized.contains("FUTURE_SECRET"));
    }

    #[test]
    fn historical_text_base64_decodes_but_invalid_or_binary_text_is_an_attachment() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("decoded text");
        let invalid_utf8 = base64::engine::general_purpose::STANDARD.encode([0xff, 0xfe]);
        let input = request(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[
                {"type":"document","title":"ok.txt","source":{"type":"base64","media_type":"text/plain","data":encoded}},
                {"type":"document","title":"broken.txt","source":{"type":"base64","media_type":"text/plain","data":"%%%"}},
                {"type":"document","title":"bytes.txt","source":{"type":"base64","media_type":"text/plain","data":invalid_utf8}}
            ]}]},
            {"role":"assistant","content":"done"},
            {"role":"user","content":"continue"}
        ]));
        let outcome = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap();
        let serialized = serde_json::to_string(&outcome.payload).unwrap();
        assert!(serialized.contains("decoded text"));
        assert!(serialized.contains("broken.txt (text/plain); binary content unavailable to Kiro"));
        assert!(serialized.contains("bytes.txt (text/plain); binary content unavailable to Kiro"));

        let current = request(json!([
            {"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"text/plain","data":"%%%"}}]}
        ]));
        assert_eq!(
            normalize(
                &current,
                UnexpressibleStrategy::PortableText,
                &TestFingerprint,
            )
            .unwrap_err()
            .code(),
            "portable_history.current_unexpressible"
        );
    }

    #[test]
    fn ambiguous_server_search_result_without_id_is_rejected() {
        let input = request(json!([
            {"role":"assistant","content":[
                {"type":"server_tool_use","id":"search-1","name":"web_search","input":{"query":"one"}},
                {"type":"server_tool_use","id":"search-2","name":"web_search","input":{"query":"two"}},
                {"type":"web_search_tool_result","content":[]}
            ]},
            {"role":"user","content":"continue"}
        ]));
        assert!(matches!(
            normalize(
                &input,
                UnexpressibleStrategy::PortableText,
                &TestFingerprint
            ),
            Err(PortableHistoryError::ToolPairing { .. })
        ));
    }

    #[test]
    fn non_web_server_tool_history_is_rejected_instead_of_projected_as_generic_text() {
        let input = request(json!([
            {"role":"assistant","content":[
                {"type":"server_tool_use","id":"server-1","name":"computer_use","input":{}}
            ]},
            {"role":"user","content":"continue"}
        ]));
        assert!(matches!(
            normalize(
                &input,
                UnexpressibleStrategy::PortableText,
                &TestFingerprint
            ),
            Err(PortableHistoryError::ToolPairing { .. })
        ));
    }

    #[test]
    fn client_tool_pairing_rejects_duplicate_or_orphan_ids_and_accepts_a_complete_pair() {
        let duplicate = request(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"one","input":{}}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"two","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"ok"}]}
        ]));
        assert!(matches!(
            validate_tool_pairing(&duplicate),
            Err(PortableHistoryError::ToolPairing { .. })
        ));

        let orphan = request(json!([
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"no use"}]}
        ]));
        assert!(matches!(
            validate_tool_pairing(&orphan),
            Err(PortableHistoryError::ToolPairing { .. })
        ));

        let complete = request(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","is_error":true,"content":"ok"}]}
        ]));
        assert!(validate_tool_pairing(&complete).is_ok());
    }

    fn complex_history_request() -> MessagesRequest {
        request(json!([
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"reasoning","signature":"provider-signature"},
                {"type":"server_tool_use","id":"search-1","name":"web_search","input":{"query":"query"}},
                {"type":"web_search_tool_result","tool_use_id":"search-1","content":[
                    {"type":"web_search_result","title":"Result","url":"https://example.invalid/result"}
                ]}
            ]},
            {"role":"user","content":"continue"}
        ]))
    }

    #[test]
    fn portable_normalization_is_idempotent() {
        let first = normalize(
            &complex_history_request(),
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap();
        let second = normalize(
            &first.payload,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&first.payload).unwrap(),
            serde_json::to_value(&second.payload).unwrap()
        );
        assert_eq!(second.report.transformed_blocks, 0);
    }

    #[test]
    fn every_replacement_is_fingerprinted_even_after_event_reporting_is_capped() {
        struct CountingFingerprint(Cell<usize>);
        impl SensitiveFingerprint for CountingFingerprint {
            fn fingerprint(&self, _: &[u8], _: &[u8]) -> String {
                self.0.set(self.0.get() + 1);
                "counted".into()
            }
        }

        let input = request(json!([
            {"role":"assistant","content":(0..=MAX_REPORT_EVENTS).map(|_| json!({"type":"future_type"})).collect::<Vec<_>>()},
            {"role":"user","content":"continue"}
        ]));
        let fingerprint = CountingFingerprint(Cell::new(0));
        let outcome = normalize(&input, UnexpressibleStrategy::PortableText, &fingerprint).unwrap();
        assert_eq!(fingerprint.0.get(), MAX_REPORT_EVENTS + 1);
        assert_eq!(outcome.report.events.len(), MAX_REPORT_EVENTS);
        assert!(outcome.report.events_truncated);
    }

    #[test]
    fn invalid_historical_roles_are_rejected_without_leaking_private_block_content() {
        let input = request(json!([
            {"role":"user","content":[
                {"type":"thinking","thinking":"TOP_LEVEL_REASONING_SENTINEL","signature":"TOP_LEVEL_SIGNATURE_SENTINEL"},
                {"type":"redacted_thinking","data":"TOP_LEVEL_REDACTED_SENTINEL"}
            ]},
            {"role":"assistant","content":"previous answer"},
            {"role":"user","content":"continue"}
        ]));
        let before = serde_json::to_value(&input).unwrap();
        let error = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap_err();
        assert_eq!(error.code(), "portable_history.malformed");
        for sentinel in [
            "TOP_LEVEL_REASONING_SENTINEL",
            "TOP_LEVEL_SIGNATURE_SENTINEL",
            "TOP_LEVEL_REDACTED_SENTINEL",
        ] {
            assert!(!error.safe_message().contains(sentinel));
        }
        assert_eq!(serde_json::to_value(&input).unwrap(), before);

        let nested = request(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[
                {"type":"thinking","thinking":"NESTED_REASONING_SENTINEL","signature":"NESTED_SIGNATURE_SENTINEL"}
            ]}]},
            {"role":"assistant","content":"previous answer"},
            {"role":"user","content":"continue"}
        ]));
        let error = normalize(
            &nested,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap_err();
        assert_eq!(error.code(), "portable_history.malformed");
        assert!(!error.safe_message().contains("NESTED_REASONING_SENTINEL"));

        let assistant_result = request(json!([
            {"role":"assistant","content":[{"type":"tool_result","tool_use_id":"call-1","content":"ASSISTANT_RESULT_SENTINEL"}]},
            {"role":"user","content":"continue"}
        ]));
        let error = normalize(
            &assistant_result,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap_err();
        assert_eq!(error.code(), "portable_history.malformed");
        assert!(!error.safe_message().contains("ASSISTANT_RESULT_SENTINEL"));

        let current = request(json!([
            {"role":"user","content":[{"type":"thinking","thinking":"CURRENT_REASONING_SENTINEL"}]}
        ]));
        assert_eq!(
            normalize(
                &current,
                UnexpressibleStrategy::PortableText,
                &TestFingerprint,
            )
            .unwrap_err()
            .code(),
            "portable_history.current_unexpressible"
        );
    }

    #[test]
    fn server_records_outside_assistant_prescan_are_rejected_without_projection() {
        let user_server_record = request(json!([
            {"role":"user","content":[{"type":"server_tool_use","id":"server-1","name":"web_search","input":{"query":"USER_SERVER_SENTINEL"}}]},
            {"role":"assistant","content":"previous answer"},
            {"role":"user","content":"continue"}
        ]));
        let error = normalize(
            &user_server_record,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap_err();
        assert_eq!(error.code(), "portable_history.malformed");
        assert!(!error.safe_message().contains("USER_SERVER_SENTINEL"));

        let nested_server_record = request(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[
                {"type":"web_search_tool_result","tool_use_id":"server-1","content":"NESTED_SERVER_SENTINEL"}
            ]}]},
            {"role":"assistant","content":"previous answer"},
            {"role":"user","content":"continue"}
        ]));
        let error = normalize(
            &nested_server_record,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap_err();
        assert_eq!(error.code(), "portable_history.malformed");
        assert!(!error.safe_message().contains("NESTED_SERVER_SENTINEL"));
    }

    #[test]
    fn current_server_records_are_current_unexpressible_at_top_level_and_nested() {
        let top_level = request(json!([
            {"role":"user","content":[{"type":"server_tool_use","id":"server-1","name":"web_search","input":{}}]}
        ]));
        assert_eq!(
            normalize(
                &top_level,
                UnexpressibleStrategy::PortableText,
                &TestFingerprint,
            )
            .unwrap_err()
            .code(),
            "portable_history.current_unexpressible"
        );

        let nested = request(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[
                {"type":"web_search_tool_result","tool_use_id":"server-1","content":"CURRENT_NESTED_SERVER_SENTINEL"}
            ]}]}
        ]));
        assert_eq!(
            normalize(
                &nested,
                UnexpressibleStrategy::PortableText,
                &TestFingerprint,
            )
            .unwrap_err()
            .code(),
            "portable_history.current_unexpressible"
        );
    }

    #[test]
    fn historical_top_level_user_redacted_thinking_is_rejected_without_opaque_leakage() {
        let input = request(json!([
            {"role":"user","content":[{"type":"redacted_thinking","data":"TOP_LEVEL_OPAQUE_SENTINEL"}]},
            {"role":"assistant","content":"previous answer"},
            {"role":"user","content":"continue"}
        ]));
        let before = serde_json::to_value(&input).unwrap();
        let error = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap_err();
        assert_eq!(error.code(), "portable_history.malformed");
        assert!(!error.safe_message().contains("TOP_LEVEL_OPAQUE_SENTINEL"));
        assert_eq!(serde_json::to_value(&input).unwrap(), before);
    }

    #[test]
    fn historical_nested_user_redacted_thinking_is_rejected_without_opaque_leakage() {
        let input = request(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[
                {"type":"redacted_thinking","data":"NESTED_OPAQUE_SENTINEL"}
            ]}]},
            {"role":"assistant","content":"previous answer"},
            {"role":"user","content":"continue"}
        ]));
        let error = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap_err();
        assert_eq!(error.code(), "portable_history.malformed");
        assert!(!error.safe_message().contains("NESTED_OPAQUE_SENTINEL"));
    }

    #[test]
    fn server_result_ids_must_be_absent_or_exact_strings() {
        for malformed_id in [json!(null), json!(7), json!({"id":"search-1"}), json!([])] {
            let input = request(json!([
                {"role":"assistant","content":[
                    {"type":"server_tool_use","id":"search-1","name":"web_search","input":{"query":"query"}},
                    {"type":"web_search_tool_result","tool_use_id":malformed_id,"content":"RESULT_SENTINEL"}
                ]},
                {"role":"user","content":"continue"}
            ]));
            assert!(matches!(
                normalize(
                    &input,
                    UnexpressibleStrategy::PortableText,
                    &TestFingerprint
                ),
                Err(PortableHistoryError::ToolPairing { .. })
            ));
        }

        let mismatched = request(json!([
            {"role":"assistant","content":[
                {"type":"server_tool_use","id":"search-1","name":"web_search","input":{"query":"query"}},
                {"type":"web_search_tool_result","tool_use_id":"search-2","content":"RESULT_SENTINEL"}
            ]},
            {"role":"user","content":"continue"}
        ]));
        assert!(matches!(
            normalize(
                &mismatched,
                UnexpressibleStrategy::PortableText,
                &TestFingerprint
            ),
            Err(PortableHistoryError::ToolPairing { .. })
        ));
    }

    #[test]
    fn string_server_result_content_is_projected_once() {
        let input = request(json!([
            {"role":"assistant","content":[
                {"type":"server_tool_use","id":"search-1","name":"web_search","input":{"query":"query"}},
                {"type":"web_search_tool_result","tool_use_id":"search-1","content":"STRING_CONTENT_SENTINEL"}
            ]},
            {"role":"user","content":"continue"}
        ]));
        let outcome = normalize(
            &input,
            UnexpressibleStrategy::PortableText,
            &TestFingerprint,
        )
        .unwrap();
        let serialized = serde_json::to_string(&outcome.payload).unwrap();
        assert_eq!(serialized.matches("STRING_CONTENT_SENTINEL").count(), 1);
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
