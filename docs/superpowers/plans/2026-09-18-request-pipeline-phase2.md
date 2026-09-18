# Request pipeline phase 2 Implementation Plan

> **For agentic workers:** Use test-driven development, bounded independent-module implementation, and final code review. Track verification evidence below.

**Goal:** Close the four phase-2 gaps in `docs/request-pipeline-coverage.md` in the order the evidence requires — retain what is discarded, aggregate it passively, then gate, then correct.

**Architecture:** Keep the raw `contextUsageEvent` payload instead of dropping every field but one, and record it beside the declared ceiling, the hardcoded window and the native usage from the same response. Derive an implied denominator only when a response carried both a complete native usage snapshot and a percentage, aggregate those samples per model and endpoint in the existing SQLite evidence store, and surface them with their counts. Add admission against the declared ceiling and a single modify-then-retry over the phase-1 chunking remedy, both configuration-gated and off by default.

**Tech Stack:** Rust/Axum/Serde, existing SQLite tracing, React/TypeScript admin UI.

**Spec:** `docs/superpowers/specs/2026-09-18-request-pipeline-phase2.md`

**Baseline:** local commit `36415f8`, clean tree, branch `codex/request-pipeline`. Suite at baseline: 867 unit + 3 CLI passing, 1 ignored, both feature modes; admin UI 19 tests.

## Global Constraints

- All inference stays on Kiro. No probe traffic, threshold bisection, replay, synthetic load or forced account switching — a calibration sample comes only from a request that was going to happen anyway.
- Admission and recovery are off by default. Approved at phase start: observation and calibration take effect by default because they only make existing numbers more honest; anything that can refuse or re-send requires an operator to enable it.
- The hardcoded per-model-name window table is never treated as a ceiling.
- An unknown ceiling is not infinite and not zero; it is a reason not to gate.
- Missing native fields stay unknown. An absent sample is never counted as agreement.
- Recovery adds no new remedy and never proves an unverified one.
- Evidence never contains prompt text, credentials or header values.

### Task 1: Retain the discarded context-usage payload

Files: `src/kiro/model/events/context_usage.rs`, `src/anthropic/stream.rs`, `src/anthropic/handlers.rs`.

- [ ] RED: fixtures whose `contextUsageEvent` carries fields beyond the percentage assert those fields survive parsing today; assert a percentage-only payload still parses unchanged.
- [ ] GREEN: retain the raw payload alongside the typed percentage, bounded in size, without changing how the percentage itself is consumed.
- [ ] Record the declared `maxInputTokens`, the hardcoded window and the native usage from the same response beside the retained payload, so their disagreement is visible rather than resolved.
- [ ] Confirm no prompt text, credential or header value can reach the retained evidence.

### Task 2: Passive denominator calibration

Files: `src/admin/trace_db.rs`, new calibration module, `src/anthropic/stream.rs` wiring.

- [ ] RED: tests asserting a sample is produced only when a complete native usage snapshot and a nonzero percentage both arrived; asserting zero percentage, missing native fields, partial snapshots and interrupted streams each produce no sample.
- [ ] GREEN: derive the implied denominator per response, aggregate per model and endpoint with sample counts, persist in the existing evidence store.
- [ ] Expose the aggregate with its count through the admin API, labelled as an observation over N samples rather than a measured upstream limit.
- [ ] Verify calibration mutates no configuration and feeds no value into admission on its own.

### Task 3: Ceiling admission

Files: `src/pipeline/config.rs`, `src/pipeline/mod.rs`, `src/kiro/provider.rs` or `src/anthropic/handlers.rs`.

- [ ] RED: tests asserting a refusal above the declared ceiling when enabled, no refusal when the ceiling is unknown, no refusal when disabled, and that the hardcoded window never acts as a ceiling.
- [ ] GREEN: refuse before transmission with a structured error carrying estimate, ceiling, ceiling source and the heuristic caveat.
- [ ] Gate behind configuration, default off. Document that refusing on an estimate can refuse a request the upstream would have accepted.
- [ ] Verify admission changes no model, rotates no credential and truncates nothing.

### Task 4: Single modify-then-retry

Files: `src/anthropic/handlers.rs`, `src/pipeline/config.rs`.

- [ ] RED: tests asserting exactly one extra attempt; no retry when the rejection class names no budget line; no retry when the matching remedy is disabled; no third attempt when the second fails.
- [ ] GREEN: on a classified rejection with an enabled lossless remedy, apply it to the payload, rebuild the request and send once more on the same model.
- [ ] Gate behind configuration, default off. Enabling recovery must not enable a remedy.
- [ ] Verify nothing is truncated, summarized, dropped or downgraded, and that an unverified remedy stays labelled unverified.

### Task 5: Integrate, verify, document

- [ ] Surface calibration samples and admission/recovery state in the console, with sample counts and the estimate caveat.
- [ ] Update `docs/request-pipeline.md` and `docs/request-pipeline-coverage.md` to the post-phase-2 truth, including what remains unimplemented in phase 3.
- [ ] Run focused regressions, both feature modes, clippy on touched files, the admin UI build and the offline `--check-config` / `--inspect-request` fixtures.
- [ ] Review the complete diff for security, compatibility and information loss; fix important findings and rerun covering tests.

## Execution record

- No live Kiro requests or account experiments authorized or performed.
