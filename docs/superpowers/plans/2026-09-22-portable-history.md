# Portable Cross-Provider History Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Claude/Anthropic conversation history safely portable to Kiro so a cc-switch provider change can continue, while current input remains faithful and provider-private data never reaches Kiro.

**Architecture:** Add a deterministic `PortableHistoryNormalizer` before every Kiro conversion, independent of pipeline mode. It returns a new normalized request plus a content-free report; the converter then becomes fail-closed, and the report is attached only to local trace evidence.

**Tech Stack:** Rust 2024, serde/serde_json, thiserror, base64 0.22, Axum, Bun, React 19, TypeScript 6.

**Spec:** `docs/superpowers/specs/2026-09-22-portable-history-design.md`

## Global Constraints

- Historical content may become portable quoted text; the final effective user message must never be silently degraded under `portable-text`.
- `signature`, `encrypted_content`, `redacted_thinking.data`, binary base64, raw unknown JSON, local paths and audit fingerprints must never enter the Kiro request.
- Tool-use IDs, tool-result IDs, `is_error`, ordering and pairing are preserved; ambiguous, orphaned or duplicate pairs fail.
- Normalization is deterministic, idempotent, atomic and runs for `off`, `audit` and `enforce` pipeline modes.
- No URL fetches, DNS lookups, OCR, PDF parsing, provider probing, account switching or automatic retry are introduced.
- Explicit legacy `drop` and `refuse` configurations continue to parse; `drop` is deprecated and intentionally lossy.
- New and field-absent configurations default to `portable-text`; no existing explicit value is rewritten on disk.
- No new dependency is needed: text base64 decoding uses the existing `base64 = "0.22"` dependency.
- All reports are bounded and content-free; sensitive correlation uses the existing process-epoch keyed fingerprint.

## Review Focus

- A final user message that is itself a tool-result round must remain “current”; Task 2 tests that a nested unknown block there is refused rather than treated as old history.
- A deeply nested or cyclic-looking JSON shape must terminate without mutating the input; Task 2 pins the maximum-depth error and atomicity.
- Two pending server searches followed by a result without `tool_use_id` must fail rather than guess; Task 3 includes the ambiguous-pair test.
- A human-readable-looking string placed under `encrypted_content`, `signature` or `data` must not leak through generic string extraction; Task 3 uses distinct sentinels and searches the complete normalized JSON.
- `portable-text`, `refuse` and deprecated `drop` must keep their behavior under all three pipeline modes; Task 4 exercises the full 3×3 matrix.

---

## File Structure

- Create `src/pipeline/portable_history.rs`: frontier detection, recursive normalization, safe content projections, tool/server-search validation, typed errors and redacted reports.
- Modify `src/pipeline/expressible.rs`: retain the public strategy enum and legacy semantics, add `portable-text`, and remove normalization responsibilities migrated to the new module.
- Modify `src/pipeline/mod.rs`: invoke compatibility normalization before the mode gate, expose `PrepareOutcome`, remove the old server-history normalizer/pairing implementation, and merge report data into audits.
- Modify `src/pipeline/config.rs`: make `portable-text` the missing-field default.
- Modify `src/anthropic/types.rs`: make the request model cloneable/serializable so normalization can be atomic.
- Modify `src/anthropic/converter.rs`: reject malformed or unknown normalized blocks instead of silently ignoring them.
- Modify `src/anthropic/handlers.rs`: consume `PrepareOutcome`, map typed preparation errors, and attach normalization summaries to local traces.
- Modify `src/pipeline/inspect.rs`: expose only the redacted normalization summary in offline inspection.
- Modify `admin-ui/src/types/request-pipeline.ts`, `admin-ui/src/components/settings/request-pipeline-form.ts`, and `admin-ui/src/components/settings/request-pipeline-section.tsx`: add the strategy and safe default.
- Create `admin-ui/src/components/trace-normalization.ts` and `admin-ui/src/components/trace-normalization.test.js`: validate and format redacted normalization evidence.
- Modify `admin-ui/src/components/trace-pipeline-panel.tsx`: show local-only conversion counts and the privacy boundary.
- Create `tests/fixtures/portable-history-cc-switch.json`: sanitized reproduction of the cross-provider nested-block failure.
- Modify `tests/pipeline_cli.rs` and `src/pipeline/tests.rs`: offline CLI and final-wire regressions.
- Modify `config.pipeline.example.json`, `docs/request-pipeline.md`, and `docs/request-pipeline-verification.md`: operator guidance, rollback and verified commands.

### Task 1: Establish the strategy and atomic request-model contract

**Files:**
- Modify: `src/pipeline/expressible.rs:25-78`
- Modify: `src/pipeline/config.rs:208-255`
- Modify: `src/anthropic/types.rs:60-160`
- Modify: `src/pipeline/tests.rs:1-35`
- Modify: `src/anthropic/types.rs:305-355` (tests)

**Interfaces:**
- Produces: `UnexpressibleStrategy::{PortableText, Refuse, Drop}` with `PortableText` as `Default`.
- Produces: `MessagesRequest: Clone + Serialize + Deserialize` and the same traits on its nested request-only types.
- Consumes: no interfaces from later tasks.

- [ ] **Step 1: Add failing Rust tests for the new default and explicit legacy round trips**

Add to `src/pipeline/tests.rs`:

```rust
#[test]
fn portable_text_is_the_missing_field_default_but_explicit_legacy_values_survive() {
    let default: config::PipelineConfig = serde_json::from_value(json!({})).unwrap();
    assert_eq!(
        default.unexpressible,
        expressible::UnexpressibleStrategy::PortableText
    );

    for (wire, expected) in [
        ("refuse", expressible::UnexpressibleStrategy::Refuse),
        ("drop", expressible::UnexpressibleStrategy::Drop),
        (
            "portable-text",
            expressible::UnexpressibleStrategy::PortableText,
        ),
    ] {
        let parsed: config::PipelineConfig = serde_json::from_value(json!({
            "unexpressible": wire
        }))
        .unwrap();
        assert_eq!(parsed.unexpressible, expected);
        assert_eq!(serde_json::to_value(parsed).unwrap()["unexpressible"], wire);
    }
}
```

Add to the tests in `src/anthropic/types.rs`:

```rust
#[test]
fn messages_request_can_be_cloned_and_serialized_for_atomic_normalization() {
    let request: MessagesRequest = serde_json::from_value(request_json(Some(4096))).unwrap();
    let cloned = request.clone();
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        serde_json::to_value(&cloned).unwrap()
    );
}
```

- [ ] **Step 2: Run the focused tests and verify they fail for the intended reasons**

Run:

```bash
cargo test -p kiro-rs portable_text_is_the_missing_field_default_but_explicit_legacy_values_survive --lib
cargo test -p kiro-rs messages_request_can_be_cloned_and_serialized_for_atomic_normalization --lib
```

Expected: the first command fails because `PortableText` does not exist; the second fails because `MessagesRequest` is not cloneable/serializable.

- [ ] **Step 3: Add the enum variant and default**

Change `UnexpressibleStrategy` in `src/pipeline/expressible.rs` to:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnexpressibleStrategy {
    #[default]
    PortableText,
    Refuse,
    Drop,
}
```

Update its documentation so `PortableText` states: historical incompatibilities become quoted portable text, while current incompatibilities fail. Mark `Drop` as deprecated/intentional loss without adding a Rust `#[deprecated]` attribute, because that attribute would create noise at every compatibility call site.

- [ ] **Step 4: Switch the backend missing-field default**

In `PipelineConfig::default()` use:

```rust
unexpressible: super::expressible::UnexpressibleStrategy::PortableText,
```

Keep `#[serde(default, alias = "prefill")]` unchanged so older configuration keys still parse.

- [ ] **Step 5: Make the complete request tree cloneable and serializable**

Add `Serialize` where missing and `Clone` to `MessagesRequest` and its nested request-only structs:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessagesRequest { /* existing fields unchanged */ }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Thinking { /* existing fields unchanged */ }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutputConfig { /* existing fields unchanged */ }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Metadata { /* existing fields unchanged */ }
```

Do not change field names, serde defaults or the custom system deserializer.

- [ ] **Step 6: Run the focused tests and the existing configuration tests**

Run:

```bash
cargo test -p kiro-rs portable_text_is_the_missing_field_default_but_explicit_legacy_values_survive --lib
cargo test -p kiro-rs messages_request_can_be_cloned_and_serialized_for_atomic_normalization --lib
cargo test -p kiro-rs strict_configuration_rejects_typos_and_zero_limits --lib
```

Expected: all pass.

- [ ] **Step 7: Commit the contract**

```bash
git add src/pipeline/expressible.rs src/pipeline/config.rs src/anthropic/types.rs src/pipeline/tests.rs
git commit -m "feat(pipeline): add portable history strategy"
```

### Task 2: Build the normalizer boundary, recursion guard and atomic outcome

**Files:**
- Create: `src/pipeline/portable_history.rs`
- Modify: `src/pipeline/mod.rs:1-12`

**Interfaces:**
- Consumes: `MessagesRequest: Clone + Serialize`, `UnexpressibleStrategy` from Task 1.
- Produces: `SensitiveFingerprint`, `NormalizationAction`, `NormalizationEvent`, `NormalizationReport`, `NormalizationOutcome`, `PortableHistoryError`, and `normalize()`.
- Produces exact signature:

```rust
pub fn normalize(
    payload: &MessagesRequest,
    strategy: UnexpressibleStrategy,
    fingerprint: &dyn SensitiveFingerprint,
) -> Result<NormalizationOutcome, PortableHistoryError>;
```

- [ ] **Step 1: Register the module and write failing boundary tests in the new file**

Add `pub mod portable_history;` to `src/pipeline/mod.rs`. In the new module, start with these tests and a deterministic fingerprint stub:

```rust
#[cfg(test)]
mod tests {
    use super::*;
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
}
```

- [ ] **Step 2: Run the new module tests and verify the missing-interface failure**

Run:

```bash
cargo test -p kiro-rs pipeline::portable_history::tests --lib
```

Expected: compile failure naming the undefined normalizer types.

- [ ] **Step 3: Define the typed public contract**

Add these production types above the tests:

```rust
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
    pub path: String,
    pub original_type: String,
    pub action: NormalizationAction,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizationReport {
    pub strategy: String,
    pub scanned_blocks: usize,
    pub transformed_blocks: usize,
    pub opaque_bytes: usize,
    pub by_action: std::collections::BTreeMap<String, usize>,
    pub by_original_type: std::collections::BTreeMap<String, usize>,
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
```

Implement `code()` as an exhaustive match returning the five stable strings from the spec. Add `safe_message()` that includes path/type reasons for client-caused variants but returns only `portable history invariant failed` for `InvariantViolation`.

- [ ] **Step 4: Implement final-user frontier detection without mutation**

Implement:

```rust
fn current_frontier(messages: &[Message]) -> Result<usize, PortableHistoryError> {
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
    if messages[index + 1..].iter().any(|message| !content_is_empty(&message.content)) {
        return Err(PortableHistoryError::CurrentUnexpressible {
            path: format!("messages[{}]", index + 1),
            reason: "trailing assistant prefill cannot be continued by Kiro".into(),
        });
    }
    Ok(index)
}
```

`content_is_empty` treats an empty string or empty array as empty; other scalars are malformed later, not silently empty.

- [ ] **Step 5: Implement the recursive walker with a hard depth bound**

Use a private scope enum:

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope { History, Current }
```

The walker accepts `(value, role, scope, path, depth, report, fingerprint)`. It handles strings and content-block arrays, increments `scanned_blocks`, recursively enters only `tool_result.content`, and returns `Malformed` before descending when `depth > MAX_CONTENT_DEPTH`. It preserves well-formed `text`, supported user base64 `image`, assistant `tool_use`, user `tool_result`, and assistant `thinking`; historical thinking has its `signature` key removed.

Under `portable-text`, any unsupported current block returns `CurrentUnexpressible`. Unknown roles return `Malformed`; they are never mapped to another role.

- [ ] **Step 6: Make `normalize()` clone first and publish only a complete outcome**

Implement the top-level shape:

```rust
pub fn normalize(
    payload: &MessagesRequest,
    strategy: UnexpressibleStrategy,
    fingerprint: &dyn SensitiveFingerprint,
) -> Result<NormalizationOutcome, PortableHistoryError> {
    let frontier = current_frontier(&payload.messages)?;
    let mut normalized = payload.clone();
    let mut report = NormalizationReport {
        strategy: strategy_name(strategy).to_string(),
        ..Default::default()
    };
    for (index, message) in normalized.messages.iter_mut().enumerate() {
        let scope = if index < frontier { Scope::History } else { Scope::Current };
        normalize_message(message, scope, index, &mut report, fingerprint)?;
    }
    Ok(NormalizationOutcome { payload: normalized, report })
}
```

Do not mutate the borrowed input and do not return a partial payload with an error.

- [ ] **Step 7: Add and run depth, supported-block and atomicity tests**

Add tests that build 65 nested `tool_result.content` arrays and assert `portable_history.malformed`; verify the original serialized request is unchanged. Add one well-formed historical tool pair containing text and a supported base64 PNG and assert the normalized request is byte-for-byte equal except for an assistant thinking `signature` key.

Run:

```bash
cargo test -p kiro-rs pipeline::portable_history::tests --lib
```

Expected: all Task 2 tests pass.

- [ ] **Step 8: Commit the normalizer boundary**

```bash
git add src/pipeline/mod.rs src/pipeline/portable_history.rs
git commit -m "feat(pipeline): add atomic portable history normalizer"
```

### Task 3: Implement safe historical projections and pairing validation

**Files:**
- Modify: `src/pipeline/portable_history.rs`

**Interfaces:**
- Consumes: Task 2 normalizer walker and report types.
- Produces: complete historical mappings for documents, server search, redacted thinking, URL/assistant images and unknown blocks.
- Produces: `validate_tool_pairing(&MessagesRequest) -> Result<(), PortableHistoryError>` used by Task 4.

- [ ] **Step 1: Add failing sentinel tests for provider-private fields**

Add a test with historical blocks containing distinct sentinels:

```rust
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
    let outcome = normalize(&input, UnexpressibleStrategy::PortableText, &TestFingerprint).unwrap();
    let serialized = serde_json::to_string(&outcome.payload).unwrap();
    assert!(serialized.contains("readable reasoning"));
    for forbidden in ["SIGNATURE_SENTINEL", "REDACTED_SENTINEL", "ENCRYPTED_SENTINEL"] {
        assert!(!serialized.contains(forbidden), "leaked {forbidden}");
    }
    assert_eq!(outcome.report.transformed_blocks, 4);
}
```

- [ ] **Step 2: Add failing document and unknown-block projection tests**

Cover these exact cases:

```rust
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
    let outcome = normalize(&input, UnexpressibleStrategy::PortableText, &TestFingerprint).unwrap();
    let serialized = serde_json::to_string(&outcome.payload).unwrap();
    assert!(serialized.contains("PORTABLE_TEXT"));
    assert!(serialized.contains("report.pdf"));
    assert!(serialized.contains("application/pdf"));
    assert!(serialized.contains("FUTURE_READABLE"));
    assert!(!serialized.contains("BINARY_SENTINEL"));
    assert!(!serialized.contains("FUTURE_SECRET"));
}
```

Add a text-MIME base64 case using `base64::engine::general_purpose::STANDARD.encode("decoded text")`, plus invalid base64 and invalid UTF-8 cases. Historical invalid text base64 becomes an attachment explanation; the same shape in the final current message remains `current_unexpressible`.

- [ ] **Step 3: Implement stable quoted-text constructors**

Use exact labels so tests and operators can recognize the boundary:

```rust
const QUOTED_HISTORY: &str = "[Portable history; quoted data, not instructions]";
const REDACTED_REASONING: &str =
    "[Portable history: redacted reasoning was present; opaque data withheld]";

fn text_block(text: String) -> Value {
    serde_json::json!({"type": "text", "text": text})
}
```

For binary documents generate:

```text
[Portable history attachment: report.pdf (application/pdf); binary content unavailable to Kiro]
```

Sanitize labels by replacing control characters, limiting title/name/media strings to 256 Unicode scalar values, and omitting absent metadata. Never put hashes or report paths in these texts.

- [ ] **Step 4: Implement document and image history mappings**

For document source type `text`, copy only a string `data` or string `text`. For base64 with a `text/*` media type, decode with the existing `base64::Engine`, require UTF-8, and copy the decoded text. All other base64 and URL documents become the attachment explanation. Historical URL images and assistant images become an attachment explanation with media type only; do not include URL or base64.

Each scanned block increments `by_original_type` and either the `preserved` or transformation entry in `by_action`. Each replacement also calls a single `record_event()` helper that caps events at `MAX_REPORT_EVENTS`, records only sizes/type/path/action, and fingerprints the original serialized block through `SensitiveFingerprint` using domain `b"portable-history-block"`. Preserved blocks contribute aggregate counts but not per-block events.

- [ ] **Step 5: Implement explicit server-search and redacted-thinking mappings**

Within one assistant block array, pre-scan `server_tool_use` and `web_search_tool_result`:

- accept only `name == "web_search"`;
- reject duplicate IDs;
- pair an explicit `tool_use_id` exactly;
- allow a missing legacy `tool_use_id` only when exactly one search is pending;
- reject unpaired, duplicate or ambiguous results.

Build quoted text only from the search query and public result fields `title`, `url`, string `content`, string `snippet`, string `error` or string `message`. Never copy `encrypted_content`. Replace `redacted_thinking` with `REDACTED_REASONING` and add its `data` byte length only to `report.opaque_bytes`.

- [ ] **Step 6: Implement generic unknown-block projection with a fixed whitelist**

Use this exact whitelist and no recursive arbitrary-string search:

```rust
const READABLE_FIELDS: &[&str] = &["text", "content", "title", "url", "name", "message"];
```

Only direct string values are copied, in whitelist order. If none exist, emit:

```text
[Portable history: unsupported block type "future_type" was present; no readable content]
```

Fields named `signature`, `encrypted_content`, `data`, `source` and `input` are never read by the generic projector.

- [ ] **Step 7: Move and strengthen client tool-pair validation**

Move the current pairing logic from `src/pipeline/mod.rs` into:

```rust
pub fn validate_tool_pairing(
    payload: &MessagesRequest,
) -> Result<(), PortableHistoryError>
```

Walk normalized top-level blocks, preserve existing valid cross-message pairing, and return `ToolPairing` for orphan result, duplicate use/result, unfinished use or mismatched ID. Server-search IDs are already consumed before text replacement and are not inserted into the client tool ID set.

- [ ] **Step 8: Add ambiguity, pairing and idempotence tests**

Add tests for two pending searches plus an id-less result, duplicate client tool ID, orphan result, and one complete pair. Add:

```rust
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
    let first = normalize(&complex_history_request(), UnexpressibleStrategy::PortableText, &TestFingerprint).unwrap();
    let second = normalize(&first.payload, UnexpressibleStrategy::PortableText, &TestFingerprint).unwrap();
    assert_eq!(
        serde_json::to_value(&first.payload).unwrap(),
        serde_json::to_value(&second.payload).unwrap()
    );
    assert_eq!(second.report.transformed_blocks, 0);
}
```

- [ ] **Step 9: Run all normalizer tests and inspect the negative sentinels**

Run:

```bash
cargo test -p kiro-rs pipeline::portable_history::tests --lib
```

Expected: all pass; no test output or assertion contains a leaked sentinel in normalized JSON.

- [ ] **Step 10: Commit the safe mappings**

```bash
git add src/pipeline/portable_history.rs
git commit -m "feat(pipeline): normalize cross-provider history safely"
```

### Task 4: Integrate normalization before the mode gate and into local audit

**Files:**
- Modify: `src/pipeline/mod.rs:1-135,205-285,306-470`
- Modify: `src/pipeline/inspect.rs:35-85`
- Modify: `src/anthropic/handlers.rs:275-320,970-1260,2280-2580`
- Modify: `src/pipeline/tests.rs:90-450`
- Modify: `src/anthropic/websearch_loop.rs:2480-2520` (prepare result adaptation in tests/callers)

**Interfaces:**
- Consumes: `normalize()`, `validate_tool_pairing()`, `NormalizationReport` from Tasks 2-3.
- Produces:

```rust
pub struct PrepareOutcome {
    pub context: Option<artifacts::ContextSession>,
    pub normalization: portable_history::NormalizationReport,
}

pub enum PipelinePrepareError {
    Portable(PortableHistoryError),
    Other(anyhow::Error),
}
```

- Changes: `RequestPipeline::prepare(...) -> Result<PrepareOutcome, PipelinePrepareError>`.

- [ ] **Step 1: Rewrite the existing pipeline tests as failing 3×3 behavior tests**

Replace assertions that assume default drop/full opaque JSON preservation. Add a helper producing a historical nested document followed by a final user message, then test:

```rust
fn historical_nested_document_fixture() -> MessagesRequest {
    serde_json::from_value(json!({
        "model": "claude-sonnet-4",
        "max_tokens": 1024,
        "messages": [
            {"role":"assistant","content":[{"type":"tool_use","id":"read-1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"read-1","content":[
                {"type":"document","source":{"type":"text","media_type":"text/plain","data":"portable body"}}
            ]}]},
            {"role":"assistant","content":"previous answer"},
            {"role":"user","content":"continue"}
        ]
    }))
    .unwrap()
}

#[test]
fn compatibility_strategy_is_orthogonal_to_pipeline_mode() {
    for mode in [config::PipelineMode::Off, config::PipelineMode::Audit, config::PipelineMode::Enforce] {
        for strategy in [
            expressible::UnexpressibleStrategy::PortableText,
            expressible::UnexpressibleStrategy::Refuse,
            expressible::UnexpressibleStrategy::Drop,
        ] {
            let pipeline = RequestPipeline::new(config::PipelineConfig {
                mode,
                unexpressible: strategy,
                ..config::PipelineConfig::default()
            });
            let mut payload = historical_nested_document_fixture();
            let result = pipeline.prepare(&mut payload, 1);
            match strategy {
                expressible::UnexpressibleStrategy::PortableText => {
                    let outcome = result.unwrap();
                    assert!(outcome.normalization.transformed_blocks > 0);
                    assert!(payload.messages[1].content.to_string().contains("portable body"));
                }
                expressible::UnexpressibleStrategy::Refuse => assert!(result.is_err()),
                expressible::UnexpressibleStrategy::Drop => {
                    let outcome = result.unwrap();
                    assert!(outcome.normalization.transformed_blocks > 0);
                    assert!(!payload.messages[1].content.to_string().contains("portable body"));
                }
            }
        }
    }
}
```

Add a budget test that sets `ingress_max_bytes` one byte below the serialized normalized request and expects code `portable_history.budget_exceeded` without changing the original payload.

- [ ] **Step 2: Run the focused pipeline tests and confirm mode-off currently bypasses normalization**

Run:

```bash
cargo test -p kiro-rs compatibility_strategy_is_orthogonal_to_pipeline_mode --lib
```

Expected: failure because `prepare()` returns before compatibility work and still returns `Option<ContextSession>`.

- [ ] **Step 3: Add `PrepareOutcome` and `PipelinePrepareError`**

Implement `From<PortableHistoryError>` and `From<anyhow::Error>` for `PipelinePrepareError` via `thiserror`. Add methods:

```rust
impl PipelinePrepareError {
    pub fn code(&self) -> &'static str { /* portable code or "pipeline_preparation" */ }
    pub fn safe_message(&self) -> String { /* no invariant internals */ }
    pub fn status(&self) -> http::StatusCode { /* 413 only for BudgetExceeded, 500 for invariant, else 400 */ }
}
```

The `Other` message remains the existing safe preparation error string. `InvariantViolation` returns HTTP 500 and `internal_error`; all client-caused portable errors use `invalid_request_error` at the handler layer.

- [ ] **Step 4: Implement the process-epoch fingerprint adapter**

Implement `SensitiveFingerprint for RequestPipeline` in `src/pipeline/mod.rs`:

```rust
impl portable_history::SensitiveFingerprint for RequestPipeline {
    fn fingerprint(&self, domain: &[u8], bytes: &[u8]) -> String {
        self.fingerprint(domain, bytes)
    }
}
```

If method-name resolution recurses, rename the existing private helper to `keyed_fingerprint` and update its existing audit call sites before implementing the trait.

- [ ] **Step 5: Move normalization before `PipelineMode` and swap only on success**

Run the entire preparation on a candidate clone so an error never leaks a partial mutation back to the handler. Use this ordering in `prepare()`:

```rust
let mut candidate = payload.clone();
crate::anthropic::handlers::override_thinking_from_model_name(&mut candidate);
let normalized = portable_history::normalize(&candidate, self.config.unexpressible, self)?;
let normalized_bytes = serde_json::to_vec(&normalized.payload)?.len();
if normalized_bytes > self.config.ingress_max_bytes {
    return Err(portable_history::PortableHistoryError::BudgetExceeded {
        actual: normalized_bytes,
        limit: self.config.ingress_max_bytes,
    }.into());
}
portable_history::validate_tool_pairing(&normalized.payload)?;
candidate = normalized.payload;
let report = normalized.report;

if self.config.mode != PipelineMode::Enforce {
    *payload = candidate;
    return Ok(PrepareOutcome { context: None, normalization: report });
}
```

Continue existing Enforce-only billing/image/artifact work against `candidate`, not `payload`. Swap `candidate` into `*payload` only after all enabled preparation succeeds, then return the same report with either `Some(session)` or `None`. Delete `normalize_server_history()` and the old `validate_tool_pairing()` from `mod.rs` after all tests move.

- [ ] **Step 6: Implement legacy strategy behavior inside the normalizer**

For `Refuse`, safely redact known provider-private blocks first, then refuse any otherwise unexpressible historical/current block. For `Drop`, safely redact known provider-private blocks, recursively remove other unexpressible blocks, and emit `LegacyDropped` events. Never preserve the old full-JSON leak from `normalize_server_history`; compatibility covers intentional loss, not sensitive forwarding.

Keep unknown roles, malformed arrays and broken tool pairs as errors under every strategy.

- [ ] **Step 7: Adapt all `prepare()` callers to the new outcome**

Use:

```rust
let prepared = provider.pipeline().prepare(&mut payload, key_ctx.key_id)?;
let context = prepared.context;
let normalization = prepared.normalization;
```

In tests that only need mutation, bind the result to `_outcome`. In `pipeline::inspect`, set:

```rust
result["normalization"] = serde_json::to_value(&prepared.normalization)?;
result["contextOffloaded"] = json!(prepared.context.is_some());
```

Do not put the report into `payload.metadata` or any serialized request field.

- [ ] **Step 8: Attach the report to `wire_audit`, not to the Kiro body**

Add `normalization: Option<Value>` to `RequestTraceOptions` and a private field on `RequestTracer`. Initializers associated with a prepared request pass `Some(serde_json::to_value(&normalization).unwrap())`; setup/error-only/internal test initializers pass `None`.

Change `TraceSink::on_wire_audit` implementation to:

```rust
fn on_wire_audit(&self, mut audit: serde_json::Value) {
    if let Some(normalization) = &self.normalization {
        audit["normalization"] = normalization.clone();
    }
    let mut evidence = self.pipeline_evidence.lock();
    if evidence.len() < 255 {
        evidence.push(("wire_audit", audit));
    }
}
```

The normalization value is already redacted. No report is added to `serialize_request()`.

- [ ] **Step 9: Add audit privacy and mode tests**

Assert that audit JSON contains counts, actions and fingerprints but not the input text, URL, tool ID or opaque sentinels. Assert final Kiro wire contains no `normalization`, `events`, local block path or fingerprint string.

Run:

```bash
cargo test -p kiro-rs compatibility_strategy_is_orthogonal_to_pipeline_mode --lib
cargo test -p kiro-rs pipeline::tests --lib
cargo test -p kiro-rs anthropic::handlers::tests --lib
```

Expected: all pass.

- [ ] **Step 10: Commit pipeline and audit integration**

```bash
git add src/pipeline/mod.rs src/pipeline/inspect.rs src/pipeline/tests.rs src/anthropic/handlers.rs src/anthropic/websearch_loop.rs src/pipeline/portable_history.rs
git commit -m "feat(pipeline): run portable history before Kiro conversion"
```

### Task 5: Make the converter fail closed and expose safe typed errors

**Files:**
- Modify: `src/anthropic/converter.rs:616-810,990-1175`
- Modify: `src/anthropic/handlers.rs:960-1030,2270-2340`

**Interfaces:**
- Consumes: normalized payload and `PipelinePrepareError` from Task 4.
- Produces: `ConversionError::InvariantViolation(String)` and `extract_tool_result_content(...) -> Result<String, ConversionError>`.
- Produces: one shared `pipeline_prepare_error_response()` used by both `/v1/messages` and `/cc/v1/messages`.

- [ ] **Step 1: Add failing converter tests for top-level and nested unknown blocks**

In the converter test module add:

```rust
fn converter_request(messages: serde_json::Value) -> MessagesRequest {
    serde_json::from_value(serde_json::json!({
        "model": "claude-sonnet-4",
        "max_tokens": 1024,
        "messages": messages
    }))
    .unwrap()
}

#[test]
fn converter_refuses_unknown_top_level_content_instead_of_omitting_it() {
    let request = converter_request(serde_json::json!([
        {"role":"user","content":[{"type":"future_block","text":"MUST_NOT_VANISH"}]}
    ]));
    let error = convert_request(&request).unwrap_err();
    assert!(matches!(error, ConversionError::InvariantViolation(_)));
}

#[test]
fn converter_refuses_unknown_nested_tool_result_content() {
    let request = converter_request(serde_json::json!([
        {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"read","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[
            {"type":"document","source":{"type":"text","data":"MUST_NOT_VANISH"}}
        ]}]}
    ]));
    let error = convert_request(&request).unwrap_err();
    assert!(matches!(error, ConversionError::InvariantViolation(_)));
}
```

- [ ] **Step 2: Run the focused tests and verify current silent success**

Run:

```bash
cargo test -p kiro-rs converter_refuses_unknown --lib
```

Expected: tests fail because the converter currently ignores the blocks.

- [ ] **Step 3: Add the invariant conversion error**

Extend `ConversionError` and its `Display` implementation:

```rust
InvariantViolation(String),
```

Display it as `Kiro converter invariant violation: {reason}`. The reason may contain a block type and structural path but no content text.

- [ ] **Step 4: Replace permissive top-level parsing/matches**

Every converter loop over content blocks must:

1. require an object with a string `type`;
2. deserialize the recognized block shape;
3. return `InvariantViolation` on malformed shape;
4. return `InvariantViolation` for an unrecognized type.

Replace `if let Ok(block) = ...` plus `_ => {}` with a `Result`-returning match. Preserve the deliberate `tool_use` handling split between assistant and user branches, but make a role/type mismatch an invariant error after normalization.

- [ ] **Step 5: Make nested tool-result extraction return `Result`**

Change the signature to:

```rust
fn extract_tool_result_content(
    content: &Option<Value>,
    dedup: &mut Option<&mut HashSet<String>>,
    images: &mut Vec<KiroImage>,
    preserve: bool,
) -> Result<String, ConversionError>
```

Accept only string content or arrays of valid `text`/supported `image` blocks. Return `InvariantViolation` for another scalar, malformed text/image or unknown nested type. Update the caller to use `?`.

- [ ] **Step 6: Add a single safe handler mapper for preparation errors**

Implement near the existing error helpers:

```rust
fn pipeline_prepare_error_response(error: &crate::pipeline::PipelinePrepareError) -> Response {
    let status = error.status();
    let error_type = if status == StatusCode::INTERNAL_SERVER_ERROR {
        "internal_error"
    } else {
        "invalid_request_error"
    };
    (
        status,
        Json(ErrorResponse::new(
            error_type,
            format!("{}: {}", error.code(), error.safe_message()),
        )),
    )
        .into_response()
}
```

Both message endpoints use the helper after writing the existing local warning/trace. They must not return `pipeline_preparation_error` for portable client errors. Invariant detail remains in the local `tracing::error!`; the client gets the generic safe message.

- [ ] **Step 7: Add handler mapping tests**

Construct `CurrentUnexpressible`, `BudgetExceeded` and `InvariantViolation { path, reason }` errors, call the helper, and assert HTTP 400, 413 and 500 respectively. Read the response body and assert the current error includes its stable code while the invariant body excludes an injected internal-detail sentinel.

- [ ] **Step 8: Run converter and handler tests**

Run:

```bash
cargo test -p kiro-rs anthropic::converter::tests --lib
cargo test -p kiro-rs anthropic::handlers::tests --lib
```

Expected: all pass and no converter test can produce a successful request after encountering an unknown content block.

- [ ] **Step 9: Commit fail-closed conversion and HTTP errors**

```bash
git add src/anthropic/converter.rs src/anthropic/handlers.rs
git commit -m "fix(adapter): reject unnormalized content without omission"
```

### Task 6: Complete the operator UI, sanitized regression fixture and documentation

**Files:**
- Modify: `admin-ui/src/types/request-pipeline.ts`
- Modify: `admin-ui/src/components/settings/request-pipeline-form.ts`
- Modify: `admin-ui/src/components/settings/request-pipeline-form.test.js`
- Modify: `admin-ui/src/components/settings/request-pipeline-section.tsx`
- Create: `admin-ui/src/components/trace-normalization.ts`
- Create: `admin-ui/src/components/trace-normalization.test.js`
- Modify: `admin-ui/src/components/trace-pipeline-panel.tsx`
- Create: `tests/fixtures/portable-history-cc-switch.json`
- Modify: `tests/pipeline_cli.rs`
- Modify: `src/pipeline/tests.rs`
- Modify: `config.pipeline.example.json`
- Modify: `docs/request-pipeline.md`

**Interfaces:**
- Consumes: redacted `wire_audit.normalization` from Task 4 and strategy contract from Task 1.
- Produces: TypeScript strategy union, `normalizationSummary(value)` parser, visible local-only trace summary, sanitized fixture and offline acceptance test.

- [ ] **Step 1: Add failing frontend tests for the strategy default and evidence parser**

Update the test fixture config to include `unexpressible: 'portable-text'`. Add to `request-pipeline-form.test.js`:

```javascript
test('portable history is preserved through a save and missing old drafts use the safe default', () => {
  const editor = createEditor(snapshot())
  expect(validateDraft(editor.draft).config?.unexpressible).toBe('portable-text')
  delete editor.draft.unexpressible
  expect(validateDraft(editor.draft).config?.unexpressible).toBe('portable-text')
  editor.draft.unexpressible = 'drop'
  expect(validateDraft(editor.draft).config?.unexpressible).toBe('drop')
})
```

Create `trace-normalization.test.js`:

```javascript
import { expect, test } from 'bun:test'
import { normalizationSummary } from './trace-normalization'

test('normalization evidence exposes counts but no arbitrary content', () => {
  const summary = normalizationSummary({
    strategy: 'portable-text', scannedBlocks: 9, transformedBlocks: 3,
    opaqueBytes: 40, eventsTruncated: false,
    events: [{ path: 'messages[1]', originalType: 'document', fingerprint: 'abc', text: 'SECRET' }],
  })
  expect(summary).toEqual({ strategy: 'portable-text', scannedBlocks: 9, transformedBlocks: 3, opaqueBytes: 40, eventsTruncated: false })
  expect(JSON.stringify(summary)).not.toContain('SECRET')
})

test('malformed evidence is not rendered as trusted numbers', () => {
  expect(normalizationSummary({ transformedBlocks: -1 })).toBeNull()
  expect(normalizationSummary('bad')).toBeNull()
})
```

- [ ] **Step 2: Run the frontend tests and verify the new parser/default are absent**

Run:

```bash
cd admin-ui && bun test src/components/settings/request-pipeline-form.test.js src/components/trace-normalization.test.js
```

Expected: failure because `portable-text` and `normalizationSummary` are not implemented.

- [ ] **Step 3: Update TypeScript configuration and form copy**

Use:

```typescript
unexpressible: 'portable-text' | 'refuse' | 'drop'
```

Change the form fallback from `'drop'` to `'portable-text'`. Order choices as:

```typescript
unexpressible: [
  { value: 'portable-text', label: '历史转为可移植文本（推荐）' },
  { value: 'refuse', label: '遇到不兼容内容就拒绝' },
  { value: 'drop', label: '丢弃并记录（旧版，有损）' },
]
```

The description must state that current input is refused rather than degraded, conversion audit stays local, and `drop` can lose content.

- [ ] **Step 4: Implement a defensive normalization evidence parser**

`normalizationSummary(value)` returns exactly these fields after checking non-negative safe integers and a known strategy string:

```typescript
export interface NormalizationSummary {
  strategy: 'portable-text' | 'refuse' | 'drop'
  scannedBlocks: number
  transformedBlocks: number
  opaqueBytes: number
  eventsTruncated: boolean
}
```

Do not return or render event paths/fingerprints in the first UI iteration. They remain available in raw admin evidence for expert diagnosis, but normal operators see aggregate counts only.

- [ ] **Step 5: Render the local-only trace summary**

In `WireAudit`, parse `value.normalization`. When present, render:

```text
跨上游历史：扫描 9 块，转换 3 块，隔离不透明数据 40 字节
仅本平台审计；该告警、路径与指纹未发送给 Kiro。
```

Show `eventsTruncated` as “详细事件已达到本地上限” without implying content loss from the request.

- [ ] **Step 6: Create the sanitized cc-switch fixture**

Create `tests/fixtures/portable-history-cc-switch.json` with:

- one assistant `tool_use` (`read-1`);
- one historical user `tool_result` whose content contains a text document and a future readable block;
- one assistant thinking block with `SIGNATURE_MUST_NOT_REACH_KIRO`;
- one completed server search with `ENCRYPTED_MUST_NOT_REACH_KIRO`;
- one `redacted_thinking` with `REDACTED_MUST_NOT_REACH_KIRO`;
- one final user message `Continue after switching providers`.

Use `.invalid` URLs and synthetic content only. Include no real prompt, token, account, host or credential.

- [ ] **Step 7: Add the offline CLI privacy regression**

Add to `tests/pipeline_cli.rs`:

```rust
#[test]
fn cc_switch_history_is_inspected_without_network_or_opaque_leakage() {
    let fixture = Fixture::new();
    let request: Value = serde_json::from_str(include_str!(
        "fixtures/portable-history-cc-switch.json"
    )).unwrap();
    let output = fixture.run(
        json!({"requestPipeline":{"unexpressible":"portable-text"}}),
        Some(request),
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["networkRequests"], 0);
    assert!(result["normalization"]["transformedBlocks"].as_u64().unwrap() >= 3);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for sentinel in ["SIGNATURE_MUST", "ENCRYPTED_MUST", "REDACTED_MUST"] {
        assert!(!stdout.contains(sentinel));
    }
}
```

- [ ] **Step 8: Add an in-memory fake-upstream final-wire regression**

In `src/pipeline/tests.rs`, load the same fixture, run `prepare`, converter and `serialize_request`, then pass the string to:

```rust
#[derive(Default)]
struct FakeKiroUpstream { received: Vec<String> }
impl FakeKiroUpstream {
    fn send(&mut self, body: String) { self.received.push(body); }
}

#[test]
fn cc_switch_history_reaches_fake_kiro_without_private_fields() {
    let mut payload: MessagesRequest = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/portable-history-cc-switch.json"
    )))
    .unwrap();
    let pipeline = RequestPipeline::new(config::PipelineConfig::default());
    let _prepared = pipeline.prepare(&mut payload, 1).unwrap();
    let converted = crate::anthropic::converter::convert_request_with_pipeline(
        &payload,
        crate::model::config::ToolCompatibilityMode::Raw,
        &pipeline.config,
    )
    .unwrap();
    let body = serialize_request(
        &payload,
        &KiroRequest {
            conversation_state: converted.conversation_state,
            profile_arn: None,
            additional_model_request_fields: converted.additional_model_request_fields,
        },
        &pipeline.config,
    )
    .unwrap();
    let mut upstream = FakeKiroUpstream::default();
    upstream.send(body);
    assert_eq!(upstream.received.len(), 1);
    let received = &upstream.received[0];
    assert!(received.contains("Continue after switching providers"));
    assert!(received.contains("read-1"));
    for forbidden in [
        "SIGNATURE_MUST_NOT_REACH_KIRO",
        "ENCRYPTED_MUST_NOT_REACH_KIRO",
        "REDACTED_MUST_NOT_REACH_KIRO",
        "normalization",
        "portable_history.",
    ] {
        assert!(!received.contains(forbidden));
    }
}
```

Assert exactly one body is received; it contains `Continue after switching providers`, the portable text document, public search title/URL and the original tool ID relationship; it excludes every sentinel, `normalization`, `messages[`, `portable_history.` and raw document base64. This is an offline capture boundary, not a claim about live Kiro acceptance.

- [ ] **Step 9: Update example configuration and operator docs**

Add explicit `"unexpressible": "portable-text"` after `"mode"` in `config.pipeline.example.json`.

In `docs/request-pipeline.md` replace the old statements that server history becomes full JSON and unsupported input is simply dropped/rejected. Document:

- historical vs current frontier;
- provider-private field removal;
- local-only report visibility;
- no URL/binary parsing;
- three strategies and deprecated `drop`;
- mode orthogonality;
- rollback by changing `portable-text` to `refuse` before an old binary.

- [ ] **Step 10: Run focused frontend, CLI and final-wire tests**

Run:

```bash
cd admin-ui && bun test src/components/settings/request-pipeline-form.test.js src/components/trace-normalization.test.js
cd .. && cargo test --test pipeline_cli cc_switch_history_is_inspected_without_network_or_opaque_leakage
cargo test -p kiro-rs cc_switch_history_reaches_fake_kiro_without_private_fields --lib
```

Expected: all pass.

- [ ] **Step 11: Commit the operator surface and regression fixture**

```bash
git add admin-ui/src/types/request-pipeline.ts admin-ui/src/components/settings/request-pipeline-form.ts admin-ui/src/components/settings/request-pipeline-form.test.js admin-ui/src/components/settings/request-pipeline-section.tsx admin-ui/src/components/trace-normalization.ts admin-ui/src/components/trace-normalization.test.js admin-ui/src/components/trace-pipeline-panel.tsx tests/fixtures/portable-history-cc-switch.json tests/pipeline_cli.rs src/pipeline/tests.rs config.pipeline.example.json docs/request-pipeline.md
git commit -m "feat(admin): expose local portable history audit"
```

### Task 7: Run complete verification and record the evidence

**Files:**
- Modify: `docs/request-pipeline-verification.md`
- Modify only if verification finds a defect: files owned by Tasks 1-6, with a new failing regression test committed alongside the fix.

**Interfaces:**
- Consumes: the complete feature.
- Produces: a clean branch, recorded offline evidence and no uncommitted generated artifacts.

- [ ] **Step 1: Format and reject whitespace errors**

Run:

```bash
cargo fmt --all
cargo fmt --all --check
git diff --check
```

Expected: both checks exit 0. Review `git diff --stat`; formatting must not touch unrelated files outside this plan.

- [ ] **Step 2: Run the complete Rust workspace suite outside restrictive loopback sandboxes**

Run:

```bash
cargo test --workspace --locked
```

Expected: all unit, integration, desktop and doc tests pass. The two existing dead-code warnings in pipeline tests may remain unless the implementation naturally removes those helpers; no new warnings are accepted.

- [ ] **Step 3: Run the no-default-features suite**

Run:

```bash
cargo test --locked -p kiro-rs --no-default-features
```

Expected: all library, CLI and doc tests pass without native-tls defaults.

- [ ] **Step 4: Run the complete frontend suite and production build**

Run:

```bash
cd admin-ui
bun test
bun run build
cd ..
```

Expected: every Bun test passes and Vite completes a production build. The existing Node `module.register()` deprecation warning is non-blocking; no new warning is accepted.

- [ ] **Step 5: Run offline diagnostics against the sanitized fixture**

Run:

```bash
cargo run --locked -- --config config.pipeline.example.json --check-config
cargo run --locked -- --config config.pipeline.example.json --inspect-request tests/fixtures/portable-history-cc-switch.json
```

Expected: exit 0, `networkRequests: 0`, a non-zero normalization count, and no sentinel or private fixture content in stdout/stderr.

- [ ] **Step 6: Record exact verification results**

Append a dated “Portable cross-provider history” section to `docs/request-pipeline-verification.md` listing the commands above, their exit status, test counts reported by the tools, the fixture name, and the explicit statement that this is offline construction evidence rather than live Kiro acceptance.

- [ ] **Step 7: Commit verification documentation and any test-driven corrections**

```bash
git add docs/request-pipeline-verification.md
git commit -m "docs: record portable history verification"
```

If a verification failure required a code change, first add a focused failing regression test, make the smallest correction, rerun its owning task tests, and commit that correction separately before the documentation commit.

- [ ] **Step 8: Confirm final branch state**

Run:

```bash
git status --short --branch
git log --oneline --decorate -8
git diff master...HEAD --check
```

Expected: `feat/portable-history` is clean, all implementation and documentation commits are present, the retained `feat/desktop-app-and-chat` branch is untouched, and nothing has been pushed or merged without a separate user request.
