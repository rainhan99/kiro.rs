# Kiro request pipeline — implementation contract

All inference stays on Kiro. No automatic downgrade, summarization, history deletion,
budget reduction, online threshold probing or account-switch experiments. A local
limit is an operator policy, not a claim about an undocumented upstream limit.

The pipeline has independently configured normalization, image handling, context
offloading, cache marking, final-wire budgets and metadata-only audit. `off` retains
legacy conversion; `audit` measures without transformations; `enforce` applies the
configured transformations and refuses payloads that exceed local limits. Native
usage truth and removal of secret-bearing debug logs apply in all modes.

Default: enforce, strip only an exact leading `x-anthropic-billing-header:` system
line, preserve images, no cachePoint injection, no context offloading, no guessed
wire threshold, no simulated cache reporting. Explicit opt-in example enables
static-prefix cachePoint and artifact retrieval. CachePoint is an experimental
wire feature, not a cache-hit guarantee; do not relocate dynamic dialogue into it.

Context offloading is opt-in and replaces only oversized historical user text and
tool-result text with immutable references. Never offload system instructions,
current user instructions, schemas, reasoning or tool-use input. Exact originals
remain available through tenant/session-scoped, bounded read/search tools. Internal
tool calls are executed by the gateway, never exposed as client-executable calls.
Their round limit, byte caps and expiry errors are explicit; no silent truncation.
Retrieval preserves information availability, not mathematical reasoning equivalence.

Only complete, nonnegative native `metadataEvent.tokenUsage` snapshots are provider
truth. Missing fields are unknown, not zero. Per-round native evidence remains
separate from aggregates/estimates. No `simulated` value proves a cache hit.

Audit captures final-wire measurements, cache points, configuration fingerprint,
scope fingerprints and header names/bytes (never header values, raw prompts,
credentials, profiles or artifact content). Full-wire hashes and static-prefix
hashes serve different comparisons. HTTP invocation IDs remain fresh.

Verification is offline by default: configuration check and request dry run stop
before credentials/background network initialization; synthetic fixtures test A–G
construction invariants and local boundaries. Online acceptance uses only normal,
authorized business traffic; no scripted threshold search, pressure or forced
account/profile switching. Unknown native evidence stays inconclusive.
