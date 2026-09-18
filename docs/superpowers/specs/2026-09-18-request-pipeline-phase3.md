# Request pipeline phase 3 — on-demand tool discovery and declared-cost chunking

Phase 3 was labelled "the version without intelligence loss". Two items remain, and they are
not the same kind of thing. One is lossless in what it makes reachable. The other is lossy by
construction and cannot be made otherwise by implementing it well. Shipping them under one
label would be the claim this project refuses to make, so they are specified separately and
documented separately.

## On-demand tool discovery

When a client declares more tool schemas than a configured budget allows, the schemas are not
all sent. A catalog of names and descriptions is offered instead, paginated, through
server-executed tools; the model lists the catalog, asks for the schemas it wants, and those
tools become declared for the remaining rounds so it can actually call them.

No tool is removed from reach. Every tool the client declared remains callable, and the
gateway never decides on the model's behalf which tools matter. What changes is that a tool is
chosen from its name and description before its schema is seen, and that reaching it costs
additional rounds. Those are the honest costs; neither is information destruction, and the
documentation states them rather than calling the mechanism free.

The catalog is derived from the client's own declarations. Names and descriptions are passed
through unchanged — never summarized, reworded or reordered by relevance, because a gateway
that reorders a tool list by its own guess at relevance is making the selection it claims not
to make. Pagination is stable and complete: following `next_offset` until it is null yields
exactly the declared set, once each.

Revealing a schema never rewrites it. A revealed tool is declared to the upstream exactly as
the client declared it, subject to the same existing tool-compatibility mapping as any other
tool.

## Chunked map with declared cost

This is the item that cannot be called lossless, and the configuration, the tool's own
description, its results and the documentation all say so in those words.

Splitting a body of text into chunks and processing them separately means no single round ever
sees more than one chunk. Any conclusion that depends on relating two chunks' raw text cannot
have been reached, however good the implementation is. That is a property of chunking, not a
quality defect to be fixed later, and no amount of testing turns it into equivalence with
reading the whole text at once.

It is therefore off by default, and when enabled it is invoked by the model rather than
applied silently by the gateway: the model asks for chunked processing, so it is never
unknowingly handed conclusions drawn from fragments. The gateway never substitutes chunking
for an ordinary request on its own initiative.

Chunk boundaries fall on UTF-8 character boundaries and the chunks reconstruct the original
octets exactly — nothing is summarized, dropped or re-encoded on the way in. Every per-chunk
result carries the byte range it came from, so each part of the answer can be traced to the
text it was derived from, and a result that cites no range is not evidence about the document.

The combination step happens in the model's own context: the per-chunk results are returned
together, and the model relates them itself. Only the map phase is fragmented, and the result
says exactly that — how many chunks, their ranges, and the plain statement that no round saw
more than one of them.

Each map round is a real upstream call on the same model, bounded by a configured chunk limit,
and its usage is recorded like any other round. A chunked operation is not cheaper than
reading the text; it is a way to process text that will not fit, at a stated cost.

## Incremental memory

The accumulated per-chunk results are the incremental state, and they live in the
conversation where the model can see all of them at once. Nothing is persisted beyond the
request, no state survives a restart, and no summary is written over the original text: the
artifact store keeps the exact bytes and remains readable at any point. This is deliberately
less than a durable memory, and it is not described as one.

## Boundaries carried forward

No truncation, summarization, history deletion, model downgrade, blind resend, online
threshold probing, or account switching. Only complete native `tokenUsage` is provider truth.
A local threshold is operator policy, never a claim about a measured upstream limit. Evidence
never contains prompt text, credentials or header values. Reconstructing bytes exactly proves
bytes were preserved; it does not prove the model attended to them, and no claim of reasoning
equivalence with a single whole-text request is made anywhere.

## Verification

Offline only. Catalog pagination completeness and stability, schema fidelity, chunk
reconstruction, range provenance, round and chunk bounds, and the inertness of both features
while disabled are verified with synthetic fixtures. Map rounds are not exercised against a
live upstream, and no offline result is presented as evidence about answer quality: whether a
chunked answer is good enough for a given task is a judgement about that task, which this
repository does not make on the operator's behalf.
