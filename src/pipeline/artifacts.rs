//! Tenant/session-scoped immutable context artifacts and bounded internal tools.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::config::ArtifactConfig;
use crate::anthropic::types::{MessagesRequest, Tool};

type ArtifactId = [u8; 32];
const REFERENCE_PREFIX: &str = "[kiro-context:";
// Account for the entry, Arc allocation, hash table bucket and immutable key as
// well as the content. No unbounded tombstone or tenant/session registry is kept.
const ENTRY_OVERHEAD: usize = 256;
const READ_TOOL: &str = "kiro_context_read";
const SEARCH_TOOL: &str = "kiro_context_search";

pub struct ArtifactStore {
    config: ArtifactConfig,
    state: Mutex<StoreState>,
}

#[derive(Default)]
struct StoreState {
    entries: HashMap<ArtifactId, Arc<StoredArtifact>>,
    used_bytes: usize,
}

struct StoredArtifact {
    id: ArtifactId,
    scope: [u8; 32],
    text: Box<str>,
    expires_at: Instant,
}

impl StoredArtifact {
    fn charge(&self) -> usize {
        self.text.len().saturating_add(ENTRY_OVERHEAD)
    }

    fn reference(&self) -> String {
        format!(
            "{REFERENCE_PREFIX}{}]",
            json!({"artifact_id":hex::encode(self.id),"bytes":self.text.len()})
        )
    }
}

impl StoreState {
    fn expire_unused(&mut self, now: Instant) {
        let mut released = 0;
        self.entries.retain(|_, entry| {
            let keep = entry.expires_at > now || Arc::strong_count(entry) > 1;
            if !keep {
                released += entry.charge();
            }
            keep
        });
        self.used_bytes -= released;
    }
}

#[derive(Clone)]
pub struct ContextSession {
    lease: Arc<SessionLease>,
}

struct SessionLease {
    store: Arc<ArtifactStore>,
    scope: [u8; 32],
    state: Mutex<SessionState>,
}

#[derive(Default)]
struct SessionState {
    // These Arc leases are shared by ContextSession clones and prevent eviction
    // until the complete request (including its internal rounds) is finished.
    pins: HashMap<ArtifactId, Arc<StoredArtifact>>,
    tools_injected: bool,
}

impl ArtifactStore {
    pub fn new(config: ArtifactConfig) -> Self {
        Self {
            config,
            state: Mutex::new(StoreState::default()),
        }
    }

    /// Scope comes exclusively from the authenticated handler, never tool input.
    pub fn begin(self: &Arc<Self>, tenant_id: u64, session_id: &str) -> ContextSession {
        let mut hash = Sha256::new();
        hash.update(b"kiro-context-scope-v1\0");
        hash.update(tenant_id.to_be_bytes());
        hash.update((session_id.len() as u64).to_be_bytes());
        hash.update(session_id.as_bytes());
        ContextSession {
            lease: Arc::new(SessionLease {
                store: Arc::clone(self),
                scope: hash.finalize().into(),
                state: Mutex::new(SessionState::default()),
            }),
        }
    }
}

impl ContextSession {
    pub fn max_rounds(&self) -> usize {
        self.lease.store.config.max_rounds
    }

    /// Returns the number of fields represented by validated references,
    /// including existing references. Repeating this operation is idempotent.
    /// Both storage admission and request mutation are transactional.
    pub fn offload(&self, payload: &mut MessagesRequest) -> Result<usize> {
        let config = &self.lease.store.config;
        if !config.enabled {
            return Ok(0);
        }

        let mut lease = self.lease.state.lock();
        let existing_tools: Vec<_> = payload
            .tools
            .iter()
            .flatten()
            .filter(|tool| is_internal_tool(&tool.name))
            .collect();
        if !existing_tools.is_empty() {
            let expected = internal_tools();
            ensure!(
                lease.tools_injected
                    && existing_tools.len() == expected.len()
                    && expected.iter().all(|tool| existing_tools
                        .iter()
                        .filter(|candidate| {
                            candidate.name == tool.name
                                && candidate.description == tool.description
                                && candidate.input_schema == tool.input_schema
                                && candidate.tool_type == tool.tool_type
                                && candidate.max_uses == tool.max_uses
                        })
                        .count()
                        == 1),
                "incoming tool name collides with a reserved context tool"
            );
        }

        let now = Instant::now();
        let mut state = self.lease.store.state.lock();
        state.expire_unused(now);
        let mut pending = HashMap::<ArtifactId, Arc<StoredArtifact>>::new();
        let mut pins = HashMap::new();
        let mut messages = payload.messages.clone();
        let last_user = messages.iter().rposition(|message| message.role == "user");
        let mut references = 0;
        let mut visit = |text: &mut String, eligible: bool| -> Result<()> {
            if let Some((id, _)) = parse_reference(text)? {
                let entry = lease
                    .pins
                    .get(&id)
                    .or_else(|| {
                        state
                            .entries
                            .get(&id)
                            .filter(|entry| entry.expires_at > now)
                    })
                    .cloned()
                    .context("context artifact is unknown or expired in this tenant/session")?;
                ensure!(
                    entry.scope == self.lease.scope,
                    "context artifact is unknown or expired in this tenant/session"
                );
                ensure!(
                    entry.reference() == *text,
                    "context reference is malformed or has been modified"
                );
                pins.insert(id, entry);
                references += 1;
            } else if eligible && text.len() > config.threshold_bytes {
                ensure!(
                    text.len() <= config.max_artifact_bytes,
                    "context artifact exceeds max_artifact_bytes"
                );
                let mut hash = Sha256::new();
                hash.update(b"kiro-context-artifact-v1\0");
                hash.update(self.lease.scope);
                hash.update(text.as_bytes());
                let id: ArtifactId = hash.finalize().into();
                let entry = if let Some(entry) = pending.get(&id).or_else(|| state.entries.get(&id))
                {
                    ensure!(
                        entry.scope == self.lease.scope && entry.text.as_ref() == text.as_str(),
                        "context artifact digest collision"
                    );
                    Arc::clone(entry)
                } else {
                    let expires_at = now
                        .checked_add(Duration::from_secs(config.ttl_secs))
                        .context("context artifact TTL is invalid")?;
                    let entry = Arc::new(StoredArtifact {
                        id,
                        scope: self.lease.scope,
                        text: text.clone().into_boxed_str(),
                        expires_at,
                    });
                    pending.insert(id, Arc::clone(&entry));
                    entry
                };
                *text = entry.reference();
                pins.insert(id, entry);
                references += 1;
            }
            Ok(())
        };
        for (index, message) in messages.iter_mut().enumerate() {
            if message.role == "user" {
                visit_user_content(&mut message.content, Some(index) != last_user, &mut visit)?;
            }
        }
        let added = pending
            .values()
            .try_fold(0usize, |sum, entry| sum.checked_add(entry.charge()))
            .context("context artifact store capacity exceeded")?;
        let total = state
            .used_bytes
            .checked_add(added)
            .context("context artifact store capacity exceeded")?;
        ensure!(
            total <= config.max_store_bytes,
            "context artifact store capacity exceeded; live entries are never evicted"
        );
        state.entries.extend(pending);
        state.used_bytes = total;
        lease.pins.extend(pins);
        payload.messages = messages;
        if references > 0 {
            if existing_tools.is_empty() {
                payload
                    .tools
                    .get_or_insert_with(Vec::new)
                    .extend(internal_tools());
            }
            lease.tools_injected = true;
        }
        Ok(references)
    }

    pub fn execute(&self, name: &str, input: &Value) -> Result<Value> {
        ensure!(
            self.lease.store.config.enabled,
            "context artifacts are disabled"
        );
        let keys: &[&str] = match name {
            READ_TOOL => &["artifact_id", "offset", "limit"],
            SEARCH_TOOL => &["artifact_id", "query", "offset", "limit"],
            _ => bail!("unknown internal context tool"),
        };
        let input = input
            .as_object()
            .context("context tool input must be an object")?;
        ensure!(
            input.keys().all(|key| keys.contains(&key.as_str())),
            "unsupported context tool input field; scope is supplied by the handler"
        );
        let id = parse_id(
            input
                .get("artifact_id")
                .and_then(Value::as_str)
                .context("artifact_id must be a string")?,
        )?;
        let entry = self.resolve(id)?;
        let offset = unsigned(input, "offset", 0)?;
        ensure!(
            offset <= entry.text.len() && entry.text.is_char_boundary(offset),
            "offset must be a UTF-8 byte boundary within the artifact"
        );
        let cap = self.lease.store.config.read_bytes;
        match name {
            READ_TOOL => read_page(&entry, input, offset, cap),
            SEARCH_TOOL => search_page(&entry, input, offset, cap),
            _ => unreachable!(),
        }
    }

    fn resolve(&self, id: ArtifactId) -> Result<Arc<StoredArtifact>> {
        let mut lease = self.lease.state.lock();
        if let Some(entry) = lease.pins.get(&id) {
            return Ok(Arc::clone(entry));
        }
        let now = Instant::now();
        let mut state = self.lease.store.state.lock();
        state.expire_unused(now);
        let entry = state
            .entries
            .get(&id)
            .filter(|entry| entry.scope == self.lease.scope && entry.expires_at > now)
            .cloned()
            .context("context artifact is unknown or expired in this tenant/session")?;
        lease.pins.insert(id, Arc::clone(&entry));
        Ok(entry)
    }
}

pub fn is_internal_tool(name: &str) -> bool {
    matches!(name, READ_TOOL | SEARCH_TOOL)
}

fn visit_user_content(
    content: &mut Value,
    historical: bool,
    visit: &mut impl FnMut(&mut String, bool) -> Result<()>,
) -> Result<()> {
    match content {
        Value::String(text) => visit(text, historical)?,
        Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(Value::String(text)) = block.get_mut("text") {
                            visit(text, historical)?;
                        }
                    }
                    Some("tool_result") => match block.get_mut("content") {
                        Some(Value::String(text)) => visit(text, true)?,
                        Some(Value::Array(parts)) => {
                            for part in parts {
                                if part.get("type").and_then(Value::as_str) == Some("text")
                                    && let Some(Value::String(text)) = part.get_mut("text")
                                {
                                    visit(text, true)?;
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_id(value: &str) -> Result<ArtifactId> {
    let mut id = [0; 32];
    ensure!(
        value.len() == 64,
        "artifact_id must be a 64-character hexadecimal identifier"
    );
    hex::decode_to_slice(value, &mut id).context("artifact_id is not hexadecimal")?;
    Ok(id)
}

fn parse_reference(text: &str) -> Result<Option<(ArtifactId, usize)>> {
    let Some(encoded) = text.strip_prefix(REFERENCE_PREFIX) else {
        return Ok(None);
    };
    let encoded = encoded
        .strip_suffix(']')
        .context("context reference is malformed")?;
    let value: Value = serde_json::from_str(encoded).context("context reference is malformed")?;
    let object = value
        .as_object()
        .context("context reference is malformed")?;
    ensure!(object.len() == 2, "context reference is malformed");
    let id = parse_id(
        object
            .get("artifact_id")
            .and_then(Value::as_str)
            .context("context reference is malformed")?,
    )?;
    let bytes = unsigned(object, "bytes", usize::MAX)?;
    ensure!(bytes != usize::MAX, "context reference is malformed");
    Ok(Some((id, bytes)))
}

fn unsigned(input: &Map<String, Value>, name: &str, default: usize) -> Result<usize> {
    input
        .get(name)
        .map(|value| {
            value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .with_context(|| format!("{name} must be a nonnegative integer"))
        })
        .unwrap_or(Ok(default))
}

fn floor_boundary(text: &str, mut offset: usize) -> usize {
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn read_response(entry: &StoredArtifact, start: usize, end: usize) -> Value {
    json!({"artifact_id":hex::encode(entry.id),"offset":start,"text":&entry.text[start..end],
        "total_bytes":entry.text.len(),"next_offset":if end < entry.text.len() { Some(end) } else { None }})
}

fn read_page(
    entry: &StoredArtifact,
    input: &Map<String, Value>,
    offset: usize,
    cap: usize,
) -> Result<Value> {
    let limit = unsigned(input, "limit", cap)?;
    ensure!(
        limit > 0 && limit <= cap,
        "read limit must be positive and <= read_bytes"
    );
    let end = floor_boundary(
        &entry.text,
        offset.saturating_add(limit).min(entry.text.len()),
    );
    ensure!(
        end > offset || offset == entry.text.len(),
        "read limit cannot fit the next UTF-8 character"
    );
    let mut low = 0;
    let mut high = end - offset;
    let mut best = None;
    while low <= high {
        let middle = low + (high - low) / 2;
        let candidate_end = floor_boundary(&entry.text, offset + middle);
        let value = read_response(entry, offset, candidate_end);
        if serde_json::to_vec(&value)?.len() <= cap {
            best = Some((candidate_end, value));
            low = middle + 1;
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    let (end, value) = best.context("read_bytes cannot fit context response metadata")?;
    ensure!(
        end > offset || offset == entry.text.len(),
        "read_bytes cannot fit the next UTF-8 character and metadata"
    );
    Ok(value)
}

fn search_response(entry: &StoredArtifact, matches: &[Value], next: usize) -> Value {
    json!({"artifact_id":hex::encode(entry.id),"matches":matches,"total_bytes":entry.text.len(),
        "next_offset":if next < entry.text.len() { Some(next) } else { None }})
}

fn search_page(
    entry: &StoredArtifact,
    input: &Map<String, Value>,
    offset: usize,
    cap: usize,
) -> Result<Value> {
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .context("search query must be a string")?;
    ensure!(
        !query.is_empty() && query.len() <= 256,
        "search query must contain 1..=256 UTF-8 bytes"
    );
    let limit = unsigned(input, "limit", 8)?;
    ensure!(
        (1..=32).contains(&limit),
        "search limit must be between 1 and 32 matches"
    );
    // Bound scanning as well as output. Include enough lookahead to find a
    // literal whose first byte lies in this page and last byte in the next one.
    let scan_end = floor_boundary(
        &entry.text,
        offset
            .saturating_add(cap.saturating_mul(64).max(256))
            .min(entry.text.len()),
    );
    let lookahead = floor_boundary(
        &entry.text,
        scan_end.saturating_add(query.len()).min(entry.text.len()),
    );
    let mut cursor = offset;
    let mut matches = Vec::new();
    while cursor < scan_end && matches.len() < limit {
        let Some(relative) = entry.text[cursor..lookahead].find(query) else {
            cursor = scan_end;
            break;
        };
        let start = cursor + relative;
        if start >= scan_end {
            cursor = scan_end;
            break;
        }
        let end = start + query.len();
        matches.push(json!({"start":start,"end":end}));
        if serde_json::to_vec(&search_response(entry, &matches, end))?.len() > cap {
            matches.pop();
            ensure!(
                !matches.is_empty(),
                "read_bytes cannot fit a context search match and metadata"
            );
            break;
        }
        cursor = end;
    }
    let value = search_response(entry, &matches, cursor);
    ensure!(
        serde_json::to_vec(&value)?.len() <= cap,
        "read_bytes cannot fit context search metadata"
    );
    Ok(value)
}

fn internal_tools() -> Vec<Tool> {
    let id = json!({"type":"string","pattern":"^[0-9a-f]{64}$","description":"artifact_id from a kiro-context reference in this conversation"});
    let offset = json!({"type":"integer","minimum":0,"description":"UTF-8 byte offset; continue with next_offset returned by the previous page"});
    let read = json!({"type":"object","additionalProperties":false,"required":["artifact_id"],
        "properties":{"artifact_id":id,"offset":offset,"limit":{"type":"integer","minimum":1,"description":"Maximum raw text bytes; response metadata also counts toward the server's read_bytes limit"}}});
    let search = json!({"type":"object","additionalProperties":false,"required":["artifact_id","query"],
        "properties":{"artifact_id":id,"offset":offset,"query":{"type":"string","minLength":1,"maxLength":256,"description":"Literal, non-regex text, at most 256 UTF-8 bytes"},"limit":{"type":"integer","minimum":1,"maximum":32}}});
    [(READ_TOOL,"Read exact original context from a kiro-context reference. Continue at next_offset until null to reconstruct the original UTF-8 text. This server tool is restricted to the authenticated tenant and session.",read),
     (SEARCH_TOOL,"Search original context for a literal string. Returns bounded, non-overlapping match byte offsets; use kiro_context_read to read surrounding text. Continue at next_offset until null, including after an empty page. Scope is fixed by the server.",search)]
        .into_iter().map(|(name, description, schema)| Tool {
            tool_type: None, name: name.into(), description: description.into(),
            input_schema: serde_json::from_value::<BTreeMap<String, Value>>(schema).expect("static context tool schema"),
            max_uses: None, cache_control: None,
        }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> ArtifactConfig {
        ArtifactConfig {
            enabled: true,
            threshold_bytes: 8,
            max_store_bytes: 16_384,
            max_artifact_bytes: 4096,
            ttl_secs: 60,
            read_bytes: 256,
            max_rounds: 4,
        }
    }

    fn request(messages: Value) -> MessagesRequest {
        serde_json::from_value(json!({"model":"test", "messages":messages})).unwrap()
    }

    fn saved(session: &ContextSession, text: &str) -> (MessagesRequest, String) {
        let mut req = request(json!([{"role":"user", "content":text},
            {"role":"assistant", "content":"ack"}, {"role":"user", "content":"current"}]));
        assert_eq!(session.offload(&mut req).unwrap(), 1);
        let marker = req.messages[0].content.as_str().unwrap();
        let object: Value = serde_json::from_str(
            marker
                .strip_prefix("[kiro-context:")
                .unwrap()
                .strip_suffix(']')
                .unwrap(),
        )
        .unwrap();
        (req, object["artifact_id"].as_str().unwrap().to_owned())
    }

    #[test]
    fn reads_reconstruct_utf8_and_json_escapes_with_a_total_response_cap() {
        let store = Arc::new(ArtifactStore::new(config()));
        let session = store.begin(7, "session");
        let original = "中文🙂\n\"\\\t".repeat(25);
        let (_, id) = saved(&session, &original);
        let mut rebuilt = String::new();
        let mut offset = 0;
        loop {
            let page = session
                .execute(
                    "kiro_context_read",
                    &json!({"artifact_id":id,"offset":offset}),
                )
                .unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() <= 256);
            rebuilt.push_str(page["text"].as_str().unwrap());
            match page["next_offset"].as_u64() {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(rebuilt, original);
        assert!(
            session
                .execute("kiro_context_read", &json!({"artifact_id":id,"offset":1}))
                .is_err()
        );
        assert!(
            session
                .execute("kiro_context_read", &json!({"artifact_id":id,"limit":257}))
                .is_err()
        );
        assert!(
            session
                .execute("kiro_context_read", &json!({"artifact_id":id,"limit":1}))
                .is_err()
        );
    }

    #[test]
    fn only_historical_user_text_and_tool_results_are_replaced() {
        let session = Arc::new(ArtifactStore::new(config())).begin(7, "scope");
        let mut req = request(json!([
            {"role":"user","content":[{"type":"text","text":"historical original","cache_control":{"type":"ephemeral"}}]},
            {"role":"assistant","content":[{"type":"thinking","thinking":"private reasoning"},{"type":"tool_use","id":"tool-1","name":"external","input":{"text":"unchanged input"}}]},
            {"role":"user","content":[{"type":"text","text":"current user original"},{"type":"tool_result","tool_use_id":"tool-1","is_error":false,"content":[{"type":"text","text":"large result original"},{"type":"image","source":{"data":"untouched"}}]}]}
        ]));
        req.system = Some(vec![
            serde_json::from_value(json!({"text":"system unchanged"})).unwrap(),
        ]);
        let assistant = req.messages[1].clone();
        assert_eq!(session.offload(&mut req).unwrap(), 2);
        assert_eq!(req.messages[1].content, assistant.content);
        assert_eq!(req.messages[2].content[0]["text"], "current user original");
        assert_eq!(req.messages[2].content[1]["tool_use_id"], "tool-1");
        assert_eq!(req.messages[2].content[1]["is_error"], false);
        assert_eq!(
            req.messages[2].content[1]["content"][1]["source"]["data"],
            "untouched"
        );
        assert_eq!(
            req.messages[0].content[0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(req.system.as_ref().unwrap()[0].text, "system unchanged");
    }

    #[test]
    fn identifiers_are_deterministic_and_isolated_by_tenant_and_session() {
        let store = Arc::new(ArtifactStore::new(config()));
        let a = store.begin(7, "a");
        let b = store.begin(8, "a");
        let c = store.begin(7, "b");
        let (_, aid) = saved(&a, "same original text");
        let (_, aid2) = saved(&store.begin(7, "a"), "same original text");
        let (_, bid) = saved(&b, "same original text");
        let (_, cid) = saved(&c, "same original text");
        assert_eq!(aid, aid2);
        assert_ne!(aid, bid);
        assert_ne!(aid, cid);
        for other in [&b, &c] {
            assert!(
                other
                    .execute("kiro_context_read", &json!({"artifact_id":aid}))
                    .is_err()
            );
        }
        assert!(
            a.execute("kiro_context_read", &json!({"artifact_id":"0".repeat(64)}))
                .is_err()
        );
        assert!(
            a.execute(
                "kiro_context_read",
                &json!({"artifact_id":aid,"tenant_id":8})
            )
            .is_err()
        );
        assert!(
            a.execute(
                "kiro_context_read",
                &json!({"artifact_id":aid,"path":"/etc/passwd"})
            )
            .is_err()
        );
    }

    #[test]
    fn repeated_offload_is_idempotent_and_tampered_references_are_rejected() {
        let store = Arc::new(ArtifactStore::new(config()));
        let session = store.begin(7, "a");
        let (mut req, _) = saved(&session, "immutable original");
        let before = serde_json::to_value(&req.messages).unwrap();
        assert_eq!(session.offload(&mut req).unwrap(), 1);
        assert_eq!(serde_json::to_value(&req.messages).unwrap(), before);
        assert_eq!(
            req.tools
                .as_ref()
                .unwrap()
                .iter()
                .filter(|t| is_internal_tool(&t.name))
                .count(),
            2
        );
        req.messages[0].content =
            Value::String("[kiro-context:{\"artifact_id\":\"unknown\",\"bytes\":18}]".into());
        assert!(session.offload(&mut req).is_err());
    }

    #[test]
    fn client_tool_name_collisions_are_rejected_before_content_changes() {
        let session = Arc::new(ArtifactStore::new(config())).begin(1, "a");
        for name in ["kiro_context_read", "kiro_context_search"] {
            let mut req = request(
                json!([{"role":"user","content":"oversized history"},{"role":"user","content":"now"}]),
            );
            req.tools = Some(vec![
                serde_json::from_value(json!({"name":name,"input_schema":{"type":"object"}}))
                    .unwrap(),
            ]);
            assert!(session.offload(&mut req).is_err());
            assert_eq!(req.messages[0].content, "oversized history");
        }
    }

    #[test]
    fn tools_are_injected_only_for_actual_references() {
        let session = Arc::new(ArtifactStore::new(config())).begin(1, "a");
        let mut req = request(json!([{"role":"user","content":"current user is long"}]));
        assert_eq!(session.offload(&mut req).unwrap(), 0);
        assert!(req.tools.is_none());
        let mut disabled = config();
        disabled.enabled = false;
        let inactive = Arc::new(ArtifactStore::new(disabled)).begin(1, "a");
        let mut req = request(
            json!([{"role":"user","content":"historical original"},{"role":"user","content":"now"}]),
        );
        assert_eq!(inactive.offload(&mut req).unwrap(), 0);
        assert_eq!(req.messages[0].content, "historical original");
    }

    #[test]
    fn search_is_literal_paginated_and_response_bounded() {
        let session = Arc::new(ArtifactStore::new(config())).begin(1, "a");
        let (_, id) = saved(&session, "x.*中 x.*中 x.*中 x.*中");
        let mut offsets = Vec::new();
        let mut offset = 0;
        loop {
            let page = session
                .execute(
                    "kiro_context_search",
                    &json!({"artifact_id":id,"query":".*","offset":offset,"limit":2}),
                )
                .unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() <= 256);
            offsets.extend(
                page["matches"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m["start"].as_u64().unwrap()),
            );
            match page["next_offset"].as_u64() {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(offsets, vec![1, 8, 15, 22]);
        assert!(
            session
                .execute("kiro_context_search", &json!({"artifact_id":id,"query":""}))
                .is_err()
        );
        assert!(
            session
                .execute(
                    "kiro_context_search",
                    &json!({"artifact_id":id,"query":"x","limit":33})
                )
                .is_err()
        );
    }

    #[test]
    fn capacity_failure_is_atomic_and_never_evicts_live_artifacts() {
        let mut limits = config();
        limits.max_store_bytes = 1024;
        limits.max_artifact_bytes = 512;
        let store = Arc::new(ArtifactStore::new(limits));
        let first = store.begin(1, "a");
        let (_, id) = saved(&first, &"A".repeat(400));
        let second = store.begin(1, "b");
        let mut req = request(
            json!([{"role":"user","content":"B".repeat(400)},{"role":"user","content":"now"}]),
        );
        let error = second.offload(&mut req).unwrap_err();
        assert!(error.to_string().contains("capacity"));
        assert_eq!(req.messages[0].content, "B".repeat(400));
        assert!(
            first
                .execute("kiro_context_read", &json!({"artifact_id":id}))
                .is_ok()
        );
        let mut too_large = request(
            json!([{"role":"user","content":"C".repeat(513)},{"role":"user","content":"now"}]),
        );
        assert!(second.offload(&mut too_large).is_err());
    }

    #[test]
    fn active_request_clones_pin_content_beyond_ttl_then_expire_on_release() {
        let mut limits = config();
        limits.ttl_secs = 0;
        let store = Arc::new(ArtifactStore::new(limits));
        let first = store.begin(1, "a");
        let (_, id) = saved(&first, "original survives active request");
        let clone = first.clone();
        drop(first);
        assert!(
            clone
                .execute("kiro_context_read", &json!({"artifact_id":id}))
                .is_ok()
        );
        assert!(
            store
                .begin(1, "a")
                .execute("kiro_context_read", &json!({"artifact_id":id}))
                .is_err()
        );
        drop(clone);
        let error = store
            .begin(1, "a")
            .execute("kiro_context_read", &json!({"artifact_id":id}))
            .unwrap_err();
        assert!(error.to_string().contains("expired"));
    }

    #[test]
    fn expired_storage_capacity_is_recovered_only_after_active_leases_end() {
        let mut limits = config();
        limits.ttl_secs = 0;
        limits.max_store_bytes = 1024;
        limits.max_artifact_bytes = 512;
        let store = Arc::new(ArtifactStore::new(limits));
        let first = store.begin(1, "a");
        let (_, id) = saved(&first, &"A".repeat(400));
        let second = store.begin(1, "b");
        let mut req = request(
            json!([{"role":"user","content":"B".repeat(400)},{"role":"user","content":"now"}]),
        );
        assert!(second.offload(&mut req).is_err());
        assert!(
            first
                .execute("kiro_context_read", &json!({"artifact_id":id}))
                .is_ok()
        );
        drop(first);
        assert_eq!(second.offload(&mut req).unwrap(), 1);
    }

    #[test]
    fn search_finds_literals_spanning_scan_pages_without_skipping_bytes() {
        let mut limits = config();
        limits.max_store_bytes = 100_000;
        limits.max_artifact_bytes = 50_000;
        let session = Arc::new(ArtifactStore::new(limits)).begin(1, "a");
        let original = format!("{}needle{}needle", "a".repeat(16_383), "中".repeat(5500));
        let (_, id) = saved(&session, &original);
        let mut offset = 0;
        let mut starts = Vec::new();
        loop {
            let page = session
                .execute(
                    "kiro_context_search",
                    &json!({"artifact_id":id,"query":"needle","offset":offset}),
                )
                .unwrap();
            starts.extend(
                page["matches"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m["start"].as_u64().unwrap()),
            );
            match page["next_offset"].as_u64() {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(starts, [16_383, 32_889]);
    }

    #[test]
    fn current_tool_result_string_is_offloaded_and_cross_scope_reference_replay_fails() {
        let store = Arc::new(ArtifactStore::new(config()));
        let session = store.begin(1, "a");
        let mut req = request(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"one","name":"tool","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"one","content":"exact current tool output"}]}
        ]));
        assert_eq!(session.offload(&mut req).unwrap(), 1);
        assert_eq!(req.messages[1].content[0]["tool_use_id"], "one");
        req.tools = None;
        let other = store.begin(2, "a");
        assert!(other.offload(&mut req).is_err());
    }
}
