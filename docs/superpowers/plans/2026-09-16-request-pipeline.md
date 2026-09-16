# Kiro request pipeline Implementation Plan

> **For agentic workers:** Use test-driven development, bounded independent-module implementation, and final code review. Track verification evidence below.

**Goal:** Deliver configurable 400 prevention, stable cache construction and verifiable native cache evidence without leaving Kiro.

**Architecture:** Normalize Anthropic-compatible input once, optionally attach a scoped artifact session, convert deterministically, decorate cache points, then measure the actual endpoint-transformed wire body. The existing internal tool loop handles retrieval; trace evidence is recorded independently of estimated usage.

**Tech Stack:** Rust/Axum/Serde, existing SQLite tracing, React/TypeScript admin UI.

**Spec:** `docs/superpowers/specs/2026-09-16-request-pipeline.md`

## Global Constraints

- All inference stays on Kiro; no real upstream tests in this work.
- No automatic downgrade, summarization, history deletion or budget reduction.
- Only complete native tokenUsage is provider truth; no simulated hit claims.
- Local thresholds are operator policy, not official Kiro limits.
- Logs and evidence never contain prompt text, credentials or header values.

### Task 1: Configuration and deterministic wire pipeline

Files: `src/pipeline/{mod,config}.rs`, `src/model/config.rs`, converter, provider, handlers, router, main, CLI args.

- [x] Add tests for strict config parsing, invalid limits, exact billing-line removal and idempotence; cache-off/static-prefix isolation; recursive byte metrics on escaped UTF-8, tool results, images; local limit errors.
- [x] Implement typed configuration with `validate() -> anyhow::Result<()>`; final-wire budget check before HTTP send; native-only metering default.
- [x] Route both Anthropic paths and adapters/internal rounds through shared serialization and provider preflight. Correct length errors without mapping every 400 to a token-window error.
- [x] Add `--check-config` and `--inspect-request PATH` before any credentials/network initialization. Output sanitized JSON evidence only.

### Task 2: Bounded context and image preservation

Files: `src/pipeline/{artifacts,images}.rs`.

- [x] Test exact read reconstruction, UTF-8 pagination, tenant/session isolation, unknown IDs, caps, TTL/active leases, collisions and idempotent offloading.
- [x] Implement `ArtifactStore::new(ArtifactConfig)`, `begin(tenant_id, session_id) -> ContextSession`, `ContextSession::offload(&mut MessagesRequest) -> anyhow::Result<usize>`, `execute(name, input) -> anyhow::Result<Value>` and `is_internal_tool(name) -> bool`.
- [x] Implement optional lossless PNG tiling with decoded-pixel, output-byte and tile-count caps; reject unsupported animation instead of silently discarding frames. Preserve mode must bypass the legacy lossy resizer.

### Task 3: Internal retrieval loop

File: `src/anthropic/websearch_loop.rs`.

- [x] Test private tools do not leak, client tools are preserved, combined search/retrieval works, round cap errors and missing usage remains unknown.
- [x] Add `run_context_loop(...existing run_web_search_loop arguments..., ContextSession)` while retaining the existing public function; call shared pipeline serialization every round.
- [x] Include internal tool-use/result pairing in history, preserve reasoning, cancel work on disconnect, settle native evidence per actual round.

### Task 4: Native usage and audit surfaces

Files: metadata event parser, `src/admin/trace_db.rs`, admin handlers/router, admin trace UI.

- [x] Test incomplete/negative native counters are not provider truth; duplicate metadata snapshots are not summed; SQLite migration and audit round-trip preserve unknown.
- [x] Add default TraceSink hooks for wire audits and native per-round snapshots; expose authenticated trace evidence and effective startup configuration.
- [x] Display provider evidence separately from estimates; label missing upstream measurements as unavailable.

### Task 5: Integrate, verify, document

- [x] Add safe sample configuration and offline fixture verification; document A–G as passive observation, never a probe script.
- [x] Run focused regressions, all Rust tests, formatting and UI build; compile release.
- [x] Review the complete diff for security, compatibility and information loss; fix important findings and rerun covering tests.

## Execution record

- Started from clean HEAD `22d2c2d0695ba350890072c19990f54782827ae5` on dedicated branch `codex/request-pipeline`.
- Rust toolchain was absent from host PATH; a task-scoped official Rust 1.98.1 toolchain was installed under `/private/tmp/kiro-rust.4ZfiZ9`, without changing the user's shell configuration. Downloaded toolchain archives were checked against official SHA256 values; locked dependency checksums remained unchanged.
- No live Kiro requests or account experiments authorized/performed.
- Review fixes: defer total-body admission until endpoint transformation; keep arbitrary tool-input keys out of image/cache metrics; restrict billing cleanup to the first generated leading line; keep CLI stdout JSON-only and apply real offline endpoint transforms; preserve gateway-emitted search/opaque-thinking history; reject malformed or unfinished tool JSON before execution.
- Initial red-test execution was blocked by the absent Rust toolchain and is not claimed as observed. Strict decoder regression tests subsequently demonstrated behavioral red (invalid/incomplete calls accepted by the old decoder) then green using synthetic EventStream frames without HTTP traffic.
- Full suites passed in both feature modes: 795 unit tests and 3 command-line integration tests per run, 0 failures. All touched Rust files passed rustfmt checks; the admin UI production build passed (one upstream Node module.register deprecation warning).
- The final default-feature release build passed. Running that binary with the example config and offline request fixture returned valid JSON, `networkRequests:0`, `localBudgetAccepted:true`, `nativeCacheEvidence:null` and `evidenceType:construction-only`. Build identity and boundaries are recorded in `docs/request-pipeline-verification.md`.

## Follow-up: Web configuration and full-requirements audit

- Added the explicitly requested Web editor under Settings → Request pipeline, including GET/PUT, shared disk write lock, revision conflict detection, validation, secret/unknown-key preservation and effective/saved comparison. Settings persist for restart; no hot apply or automatic restart.
- New backend tests observed the absent API/status red before implementation; then passed with real loopback HTTP and temporary files. A separate post-rename sync fault test observed red → green and now reports persistence uncertainty rather than false success.
- Fresh follow-up totals: 806 unit tests + 3 CLI tests passing in both feature modes; one manual browser fixture is ignored by default. Frontend helper suite: 6 tests/23 assertions; production build passed.
- Browser access to the isolated loopback fixture was blocked by the in-app browser. No visual/browser interaction pass is claimed; fixture server stopped and test files removed.
- The original broader three-stage roadmap is NOT complete. See `docs/request-pipeline-coverage.md`; existing byte audit is not a complete token budget, and artifact pagination is not inline fragmentation or same-model memory/Map-Reduce.
