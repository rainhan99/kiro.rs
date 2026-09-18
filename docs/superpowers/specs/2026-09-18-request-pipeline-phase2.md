# Request pipeline phase 2 — observe, aggregate, admit, recover

Phase 1 measured and reported. Phase 2 acts, but only on evidence it has actually
collected. The four items are again one chain: retain what the upstream already tells us and
we currently discard; aggregate those observations across traffic that happens anyway; only
then let a ceiling refuse a request; and only then let a classified refusal be answered by a
single lossless correction. Taken in any other order the chain promotes a guess into a gate —
refusing real traffic on a number nothing has ever checked.

The ordering is the substance of this phase, not a preference about sequencing.

## Observation

`contextUsageEvent` is deserialized into a single percentage field with no
`deny_unknown_fields`, so anything else the upstream sends is silently discarded and the
payload's real shape is unknown. Discovering it by sending probe traffic is not authorized.
The payload is therefore retained as bounded evidence, so the shape becomes knowable from
requests that were going to happen regardless. Retention carries counts and structure, never
prompt text.

The input token count currently reported to clients is the usage percentage multiplied by a
hardcoded per-model-name window table. That table is exactly the kind of guess that phase 1's
ceiling reporting refuses to make, and it is load-bearing: every percentage-derived
`input_tokens` inherits its error. It is not swapped for a different guess here. The declared
`maxInputTokens`, the hardcoded window and the native usage that arrived in the same response
are recorded beside each other and their disagreement is reported, not silently resolved.

## Calibration

When one response yields both a complete native `metadataEvent.tokenUsage` and a
`contextUsagePercentage`, the pair implies the denominator the upstream divided by. That
derivation consumes only traffic that would have occurred anyway. It is not a probe: no
threshold bisection, no replay, no synthetic load, no forced account or profile switching, and
no request is made in order to produce a sample.

Samples are aggregated per model and endpoint and always carry their count. An implied
denominator is an observation about one provider's arithmetic over the samples seen; it is
never presented as a measured or published upstream limit, and a handful of samples is
reported as a handful of samples. A zero percentage, a missing native field, an interrupted
stream or a partial usage snapshot produces no sample at all — missing is not zero, and an
absent sample is never counted as agreement.

Calibration observes. It does not edit configuration, does not widen or narrow a limit on its
own, and does not feed a learned number into admission without an operator enabling that.

## Admission

Off by default. When enabled, a request whose estimated input tokens exceed the known ceiling
is refused before transmission, with a structured error carrying the estimate, the ceiling,
where the ceiling came from, and the plain statement that the estimate is a local heuristic.

The ceiling is the upstream-declared `maxInputTokens` for that credential's model. The
hardcoded window table is not a ceiling and must never be used as one. Where no ceiling is
known, admission does not refuse — an unknown limit is not an infinite limit and not a zero
limit; it is a reason not to gate.

Refusing on an estimate can refuse a request the upstream would have accepted. That is a real
cost, it is stated in the error and in the documentation, and it is why the feature is off by
default rather than a silent improvement. Admission never substitutes a model, never rotates
credentials, and never truncates or summarizes to fit.

## Recovery

Off by default. At most one additional attempt, on the same model, under the same
credential-selection rules as any other request.

Recovery triggers only on a classified rejection whose class names a specific budget line, and
only when a lossless remedy for that line is enabled. When no remedy is enabled the rejection
is returned exactly as it is today. This phase adds no new remedy: the only lossless
correction available is the tool-result chunking delivered in phase 1, and enabling recovery
does not enable it.

The remedy is applied to the payload and the request is rebuilt from it. Nothing is truncated,
summarized, dropped, reordered or downgraded, and the model is not changed. If the second
attempt also fails, it fails: there is no third attempt, no escalation to a different remedy,
and no repetition with a different account in the hope of a different answer.

A remedy whose upstream acceptance is unverified remains unverified when recovery applies it.
Recovery succeeding once is not proof that the shape is accepted in general, and the
documentation says so rather than implying the retry validates it.

## Boundaries carried forward

No truncation, summarization, history deletion, model downgrade, blind resend, online
threshold probing, or account switching to evade a length rejection. Only complete native
`tokenUsage` is provider truth; a corrected or calibrated estimate is still an estimate and is
never relabeled as native. A local threshold remains operator policy and never becomes a claim
about a measured upstream limit. Evidence never contains prompt text, credentials or header
values.

## Verification

Offline only. Observation retention, sample derivation and rejection, aggregation arithmetic,
admission decisions and the single-retry state machine are verified with synthetic fixtures
through the existing configuration check and request dry run, which stop before credentials and
background network initialization. Both build modes and the existing suite stay green. No
upstream traffic is sent for acceptance, and no calibration figure obtained from synthetic
fixtures is presented as an observation about the real upstream.
