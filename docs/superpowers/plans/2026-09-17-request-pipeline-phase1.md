# Request pipeline phase 1 Implementation Plan

> **For agentic workers:** Use test-driven development, bounded independent-module implementation, and final code review. Track verification evidence below.

**Goal:** Close the four phase-1 gaps recorded in `docs/request-pipeline-coverage.md` as one chain — a correct instrument, the account it produces, the classification that matches a refusal against that account, and the only lossless remedy phase 1 authorizes.

**Architecture:** Correct the recursive counter first so every later number rests on it. Type the upstream rejection at the boundary where the response body is read, replacing substring classification in the handler. Extend the existing final-wire audit with token dimensions beside its byte dimensions, reporting the model's declared input ceiling where known. Emit oversized tool results as multiple content entries in the array the upstream tool-result field already is, gated off by default because upstream acceptance is unverified.

**Tech Stack:** Rust/Axum/Serde, existing SQLite tracing, React/TypeScript admin UI.

**Spec:** `docs/superpowers/specs/2026-09-17-request-pipeline-phase1.md`

**Baseline:** local commit `51b64d7`, clean tree, branch `codex/request-pipeline`. Rust 1.98.1 at `~/.rustup`, plain `cargo` on PATH. Suite at baseline: 843 unit + 3 CLI passing, 1 ignored, both feature modes.

## Global Constraints

- All inference stays on Kiro; no real upstream traffic in this work, including to test chunk acceptance.
- Phase 1 reports and never enforces. `maxInputTokens` admission, `ContextUsage` breakdown, modify-then-retry and threshold calibration belong to phase 2 and stay out.
- No truncation, summarization, history deletion, model downgrade, blind resend or account switching to evade a length rejection.
- Only complete native `tokenUsage` is provider truth; a corrected estimate is still an estimate and is never relabeled as native.
- Local thresholds are operator policy, not measured Kiro limits.
- Logs and evidence never contain prompt text, credentials or header values.
- Approved at takeover: the corrected counter replaces the old figure outright. No compatibility switch reproduces the miscount, and already-written usage logs are not recomputed.

### Task 1: Recursive token counting

Files: `src/token.rs`.

- [x] RED: fixtures whose tokens live only in nested positions — `tool_result.content` as string and as array, `tool_use.input`, `thinking`, image and document blocks — assert the current counter under-reports each, and assert a mixed agentic fixture where nested content dominates the total.
- [x] GREEN: walk the whole content tree for input counting, reaching parity with the output estimator, which already counts `thinking` and `tool_use.input`.
- [x] Add final-wire counting over the constructed request so later tasks can report what is sent rather than what arrived.
- [x] Update the existing assertions that move to the corrected values; record the before/after figure for the mixed fixture. Do not bend the counter to preserve an old assertion.
- [x] Mutation-check that the new nested-position tests fail when each traversal branch is removed.

### Task 2: Structured upstream rejection

Files: `src/kiro/error.rs`, `src/kiro/provider.rs`, `src/anthropic/handlers.rs`.

- [x] RED: tests asserting that an upstream rejection is classified from typed fields, including a body whose *prompt text* contains `CONTENT_LENGTH_EXCEEDS_THRESHOLD` or `Input is too long` and must not be misclassified by the current substring match.
- [x] GREEN: typed error carrying status, upstream code, message and retained raw body, in the shape of the existing typed rate-limit error; classify once where the body is read; select handler responses by downcast.
- [x] Preserve existing policy exactly: `CONTENT_LENGTH_EXCEEDS_THRESHOLD` stays unattributed between body, field, image and context window; client validation errors still terminate without rotation whatever status carried them; no retry, truncation or downgrade is introduced.
- [x] Confirm the error carries enough to state which local budget line a rejection is consistent with, and nothing beyond that.

### Task 3: Token budget report

Files: `src/pipeline/mod.rs`, `src/kiro/provider.rs`, `src/kiro/model/available_models.rs` plumbing.

- [x] RED: tests asserting per-section token attribution over a fixture, an unknown model ceiling reported as unknown rather than guessed, and byte and token dimensions remaining separately labelled.
- [x] GREEN: extend the final-wire metrics with token dimensions per section — system, tool declarations, history, current turn, tool results, images — plus measured total, declared `maxInputTokens` where the model list supplies it, and resulting headroom.
- [x] Emit through the existing wire-audit sink before the local budget check and transmission, inheriting its redaction rules; keep the existing stage marker on entries rejected before sending.
- [x] Verify no enforcement path reads the new fields in this phase.

### Task 4: Lossless tool-result chunking

Files: `src/anthropic/converter.rs`, `src/pipeline/config.rs`.

- [ ] RED: tests asserting that concatenating emitted parts reproduces the original octets exactly, that boundaries fall on UTF-8 character boundaries, that pairing with `tool_use_id` survives, and that the feature is inert while disabled.
- [ ] GREEN: emit multiple content entries within one tool result when the text exceeds the configured per-field budget, instead of joining every part into one string.
- [ ] Gate behind configuration, default off, in the same revocable posture as the static-prefix cache strategy. Document the unverified upstream acceptance plainly; add no automatic reshaping or retry on rejection.
- [ ] Confirm chunking neither offloads nor summarizes, and that artifact retrieval remains a separate opt-in mechanism.

### Task 5: Integrate, verify, document

- [ ] Surface the new token dimensions in the admin request-pipeline evidence view beside the existing byte measurements, labelled as estimate.
- [ ] Update `docs/request-pipeline.md` and `docs/request-pipeline-coverage.md` to the post-phase-1 truth, including what remains unimplemented in phases 2 and 3.
- [ ] Run focused regressions, both feature modes, clippy on touched files, the admin UI build, and the offline `--check-config` / `--inspect-request` fixtures.
- [ ] Review the complete diff for security, compatibility and information loss; fix important findings and rerun covering tests.

## Execution record

- Takeover session: the prior Codex session's temporary toolchain is gone; Rust 1.98.1 installed at `~/.rustup`, so the `contracts.md` environment-prefix command form no longer applies.
- No live Kiro requests or account experiments authorized or performed.
