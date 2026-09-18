# Request pipeline phase 3 Implementation Plan

> **For agentic workers:** Use test-driven development, bounded independent-module implementation, and final code review. Track verification evidence below.

**Goal:** Close the two remaining phase-3 gaps in `docs/request-pipeline-coverage.md`, keeping them separate because one is lossless in reach and the other is lossy by construction.

**Architecture:** Both ride the existing server-executed tool loop, which already injects internal tools, executes them in the gateway and appends paired results. Tool discovery adds a paginated catalog over the client's own declarations plus a reveal step that declares the requested tools for later rounds. Chunked map reuses the artifact store's exact original bytes and the phase-1 UTF-8-safe splitter, runs one bounded upstream round per chunk on the same model, and returns per-chunk results tagged with their byte ranges so the model performs the combination in its own context.

**Tech Stack:** Rust/Axum/Serde, existing SQLite tracing, React/TypeScript admin UI.

**Spec:** `docs/superpowers/specs/2026-09-18-request-pipeline-phase3.md`

**Baseline:** local commit `bad19ec`, clean tree, branch `codex/request-pipeline`. Suite at baseline: 895 unit + 3 CLI passing, 1 ignored, both feature modes; admin UI 25 tests.

## Global Constraints

- Approved at phase start: both features configuration-gated and off by default; chunked map additionally model-invoked, never applied by the gateway on its own initiative.
- Chunking is never described as lossless or as "without intelligence loss", in configuration, tool descriptions, results, docs or commit messages.
- The catalog never summarizes, rewords or relevance-orders the client's tool names and descriptions.
- A revealed tool is declared exactly as the client declared it, under the existing compatibility mapping.
- Every per-chunk result carries its byte range; a result without a range is not evidence.
- Map rounds are real upstream calls on the same model, bounded by configuration, and their usage is recorded like any other round.
- No probe traffic, no model substitution, no truncation, no summarization of original text.

### Task 1: Paginated tool catalog

Files: `src/pipeline/config.rs`, new catalog module, `src/anthropic/websearch_loop.rs` wiring.

- [ ] RED: tests asserting pagination is complete and stable (following `next_offset` yields exactly the declared set, once each), that names and descriptions pass through unchanged and unordered by relevance, and that the feature is inert below the budget and while disabled.
- [ ] GREEN: when the declared tool schemas exceed the configured budget, offer `kiro_tool_catalog_list` and `kiro_tool_schema_read` instead of the full list.
- [ ] Revealing a schema declares that tool for subsequent rounds, byte-identical to the client's declaration.
- [ ] Bound the reveal set and the catalog page size; an unknown tool name is an explicit error, never a silent omission.

### Task 2: Chunked map with declared cost

Files: `src/pipeline/config.rs`, `src/pipeline/artifacts.rs` or a new module, `src/anthropic/websearch_loop.rs` wiring.

- [ ] RED: tests asserting chunks reconstruct the original octets exactly and split on UTF-8 boundaries, that every result carries its byte range, that the chunk limit is enforced, and that the tool is absent while disabled.
- [ ] GREEN: a model-invoked server tool that splits an artifact, runs one bounded upstream round per chunk on the same model, and returns per-chunk results tagged with ranges.
- [ ] The result states the chunk count, the ranges, and that no round saw more than one chunk. The combination happens in the model's own context.
- [ ] Map-round usage is recorded like any other round; the operation is never presented as cheaper than reading the text.

### Task 3: Integrate, verify, document

- [ ] Surface both switches in the console, with the chunking one carrying its cost statement rather than a neutral label.
- [ ] Update `docs/request-pipeline.md` and `docs/request-pipeline-coverage.md` to the post-phase-3 truth, including that chunking is not the "no intelligence loss" version of anything.
- [ ] Run focused regressions, both feature modes, clippy, the admin UI build and the offline fixtures.
- [ ] Review the complete diff for security, compatibility and information loss.

## Execution record

- No live Kiro requests or account experiments authorized or performed.
