# Request pipeline phase 1 — measure, budget, classify, remediate

These four items are one chain, not four patches. Recursive counting is the instrument;
the budget report is the account it produces; the structured upstream rejection is how a
refusal is matched against that account; lossless tool-result chunking is the only remedy
phase 1 may apply, and it is applicable only once the report has named the offending field.
Implemented separately they each pass in isolation and fail together: a report built on a
counter that omits tool results is a false report, a rejection classified by substring match
cannot be attributed to any line of that report, and chunking without a report is blind
cutting. Phase 1 delivers the chain or it delivers nothing.

Phase 1 measures and reports. It does not admit, retry, recover or calibrate. Admission on
`maxInputTokens`, `ContextUsage` breakdown parsing, single modify-then-retry and per
model/endpoint threshold calibration remain phase 2 and must not be smuggled in as a
side effect of reporting.

## Counting

Token counting walks the entire content tree of the request it is given: nested
`tool_result.content` in both string and array form, `tool_use.input`, `thinking`, image
blocks, system blocks and tool declarations. The present counter reads only a top-level
`text` field and therefore omits, in agentic conversations, the largest component of the
input — measured on a fixture, a full agentic round carrying a large file body counts exactly
as much as the one-line question that preceded it. Output estimation already counts
`thinking` and `tool_use.input`; the asymmetry between the two sides is a defect, not a
convention to preserve.

Images are counted through the existing shared estimator, which already follows the
`(w×h)/750` formula and keeps a nonzero floor; no second formula is introduced. No formula is
invented for opaque binary payloads: document blocks are refused by the pipeline in enforce
mode rather than transmitted, so counting them would describe a request that is never sent.
Recursion is depth-bounded so that malformed deep nesting terminates instead of exhausting
the stack; that bound affects the estimate only and never what is transmitted.

Counting remains a heuristic estimate. It is never presented as, substituted for, or merged
with native `metadataEvent.tokenUsage`. Native usage continues to win wherever it exists, and
a missing native field stays unknown rather than being filled from an estimate.

Correcting the omission raises estimated input tokens for requests whose usage the upstream
did not report natively, and those estimates are what the usage log and the admin credit
display consume. The increase is approved and intended: it moves a known-wrong fallback
toward truth. The old figure is not retained behind a switch, because a configuration option
whose only purpose is to reproduce a miscount institutionalizes the defect. Already-written
usage logs are not recomputed; only subsequent requests are affected.

The counter is additionally applied to the final constructed wire, not only to the inbound
Anthropic request, so that the report describes what is actually sent after endpoint
transformation rather than what arrived.

## Budget report

The final-payload report carries token dimensions beside the existing byte dimensions, and
labels them as distinct: a byte budget is not a token budget and neither substitutes for the
other. Tokens are attributed per section — system, tool declarations, history, current turn,
tool results, images — so that a rejection can be matched against a specific contributor.

Where the model's `maxInputTokens` is known from the upstream model list it is reported
alongside the measured total and the resulting headroom. Where it is unknown it is reported
as unknown. It is never guessed, inferred from a model-name pattern, or back-derived from a
rejection. In phase 1 this value is displayed and never enforced.

The report is emitted through the existing wire-audit sink, before the local budget check and
before transmission, and inherits that sink's redaction rules unchanged: measurements,
counts, fingerprints and header names only — never header values, prompt text, credentials,
profile strings or artifact content. Emission proves construction, not transmission; an
entry rejected before sending keeps its existing stage marker.

## Structured upstream rejection

An upstream non-2xx response becomes a typed error carrying status, upstream error code,
upstream message and the retained raw body, in the shape already established by the typed
rate-limit error. Classification is performed once, at the boundary where the response is
read, by inspecting those fields. Downstream handlers select their response by matching the
type, not by searching a formatted string for substrings.

Structuring changes attribution, not policy. `CONTENT_LENGTH_EXCEEDS_THRESHOLD` remains
undetermined between total body, an individual field, an image and the model context window;
it must not be rendered as "context window is full" and must not be retried, truncated or
downgraded. Client validation errors continue to terminate without credential rotation
regardless of the status the upstream chose to return them under. The typed error carries
the evidence needed to say which local budget line, if any, the rejection is consistent with,
and says nothing beyond that.

## Lossless tool-result chunking

A tool result whose text exceeds the configured per-field budget is emitted as multiple
content entries within one tool result, preserving the existing pairing with its
`tool_use_id`. The upstream tool-result content field is an array; the present converter
collapses every part into a single joined string, which is a converter choice and not a
schema limit.

Chunking is byte-exact. Concatenating the emitted parts in order reproduces the original
octet sequence with nothing inserted, dropped, reordered or re-encoded, and chunk boundaries
fall on UTF-8 character boundaries. No part is summarized, elided or replaced by a reference.
Chunking is not offloading: artifact retrieval remains a separate, opt-in mechanism in which
the model must ask for content, whereas a chunked tool result is transmitted in full.

Whether the upstream accepts more than one content entry per tool result is unverified. The
repository has never sent such a payload and phase 1 does not authorize probing traffic to
find out. The capability is therefore configuration-gated and off by default, in the same
revocable posture as the static-prefix cache strategy, and the documentation states the
unverified status plainly rather than implying acceptance. If enabling it produces upstream
rejections, the configuration is returned to off; no automatic reshaping or retry is added to
work around a rejection.

## Boundaries carried forward

No truncation, no summarization, no history deletion, no model downgrade, no blind resend, no
online threshold probing, and no account switching to evade a length rejection. A local budget
remains an operator policy and never a claim about a measured upstream limit. An estimate is
never reported as native evidence. Reassembly being exact proves bytes were preserved; it does
not prove the model attended to every part, and no claim of reasoning equivalence with a
single inline field is made.

## Verification

Offline only. Counting, chunk-reassembly, report composition and rejection classification are
verified with synthetic fixtures through the existing configuration check and request dry run,
which stop before credentials and background network initialization. Both build modes and the
existing suite must stay green; a changed estimate is expected to move existing counting
assertions, and those assertions are updated to the corrected values rather than the counter
being bent to keep them passing. No upstream traffic is sent for acceptance.
