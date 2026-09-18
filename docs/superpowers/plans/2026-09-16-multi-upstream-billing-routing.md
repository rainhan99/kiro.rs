# Multi-upstream Billing and Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver configurable Anthropic/OpenAI upstreams, independent credit/CNY/USD spending accounts, virtual models and a working Web sticky/weighted-random selector with safe failover.

**Architecture:** Introduce `gateway` between authentication and provider-specific conversion. Keep Kiro's existing provider and artifact pipeline behind an adapter. Store gateway configuration independently beside the main config and financial records in a separate SQLite database; route, price and settle from immutable per-request snapshots.

**Tech Stack:** Existing Rust/Axum/Reqwest/Tokio/Rusqlite/Serde, React/TypeScript/TanStack Query/Bun; no new service or live provider testing.

**Spec:** `docs/superpowers/specs/2026-09-16-multi-upstream-billing-routing-design.md` (approved by user: “开干”).

## Global Constraints

- 单实例服务与本地 SQLite；不承诺多副本共享粘性或分布式额度强一致。
- 计费单位采用显式类型：`KiroCredit`、`Money(CNY)`、`Money(USD)`。
- 限额 `null` 表示不限，`0` 表示不允许消耗。
- 同组有效权重 `W = upstream.weight × binding.weight`；两个权重范围 `0..10000`，任一个为 `0` 排除候选。
- 模式为 `sticky`（默认）或 `weighted_random`；模型可继承全局。TTL 默认 3600 秒，范围 60..86400 秒。
- 真实 usage 才能确认费用与缓存；`confirmed`、`estimated`、`missing` 分开，缺失不是零。
- 不混用积分、CNY、USD；不隐式兑换；不把未提交给客户的失败尝试自动转嫁客户。
- 流式有效响应提交后不跨供应商拼接、不自动重放有副作用工具。
- 不删上下文、不静默丢能力、不假设同别名模型质量等价。
- 无真实上游请求、压力测试、部署或自动重启；仅本地 mock 和 fixture。
- 保留当前工作区之前的改动。按用户要求先做本地检查点提交，再分任务开发；任何提交仅在本地，不推送远端。
- 全部新增金额用十进制字符串，原生证据与价表版本进入独立账本；Trace 清理与统计重置不得抹掉财务消耗。
- Write failing behavior tests, observe RED, implement, observe GREEN. Use `apply_patch` for edits.

## Execution and Files

Execute in the current task, task-scoped implementation and independent review. No deployment at handoff. The user requested a local checkpoint of existing work before development. After that checkpoint, use task-scoped local commits and review packages; never push. Continue in this working directory and preserve the checkpoint history.

Rust command prefix for this environment:

```sh
env PATH=/private/tmp/kiro-rust.4ZfiZ9/cargo/bin:$PATH RUSTUP_HOME=/private/tmp/kiro-rust.4ZfiZ9/rustup CARGO_HOME=/private/tmp/kiro-rust.4ZfiZ9/cargo cargo
```

New files by responsibility:

- `src/gateway/{mod,amount,config,usage}.rs`: public typed contracts and native metering normalization.
- `src/gateway/{ledger,ledger_tests}.rs`: transactional accounts, reservations, attempts and settlement.
- `src/gateway/{routing,config_store,routing_tests}.rs`: routing state, validated durable config and dry-run evidence.
- `src/gateway/{protocol,transport,sse,adapter_tests}.rs`: wire conversion, network safety and incremental events.
- `src/gateway/{service,handlers,integration_tests}.rs`: request coordinator, existing-route integration and offline end-to-end checks.
- `src/admin/gateway.rs`: authenticated control-plane endpoints.
- `admin-ui/src/{types,api,hooks}/gateway.ts`, `components/settings/gateway-*`, `components/client-key-budgets.tsx`: working Web controls, budget and evidence views.
- `docs/multi-upstream-routing.md`, `docs/multi-upstream-verification.md`: operator instructions and observed test evidence.

Keep files focused; split larger implementations by responsibility within `src/gateway/` if required, update the report with the exact public interface. Do not refactor unrelated large files.

## Shared JSON Contract

Use camelCase object properties and the exact enum strings below. Configuration GET redacts `apiKey`; a missing key on PUT preserves the existing secret, an explicit replacement changes it. Never use a masked placeholder as an actual key.

```json
{
  "revision": 0,
  "config": {
    "defaultRoutingMode": "sticky",
    "affinityTtlSecs": 3600,
    "maxAttempts": 3,
    "requestTimeoutSecs": 120,
    "upstreams": [],
    "models": []
  }
}
```

Upstream: `id`, `name`, `kind` (`kiro`, `anthropic`, `openai_chat`, `openai_responses`), `enabled`, `weight`, optional `baseUrl`, optional `apiKey`, `hasApiKey` read-only, `allowPrivateNetwork`, optional `kiroGroup`. Public model: `id`, optional `displayName`, optional `routingMode`, optional `affinityTtlSecs`, `bindings`. Binding: `id`, `upstreamId`, `upstreamModel`, `enabled`, `priorityTier`, `weight`, `contextWindow`, `maxOutputTokens`, `supportsTools`, `supportsImages`, `supportsReasoning`, `allowModelSubstitution`, `billingUnit`, optional `costPrices`, optional `sellPrices`. Prices: `currency` (`CNY` or `USD`), `input`, `output`, `cacheRead`, `cacheWrite`, optional `cacheWrite1h`; all numbers are decimal strings per million tokens. Unit strings: `kiro_credit`, `CNY`, `USD`.

Budget policy: `unit`, nullable decimal-string `limit`, `enforcement` (`hard`, `soft`), `maxInFlight`, `maxPending`, `allowedModels`, `allowedUpstreams`. Empty allowlists mean all models/upstreams in the configured account scope, not access without an account. Monetary bindings cannot debit a credit account; Kiro-credit bindings cannot reference direct API upstreams. The binding explicitly determines the downstream account and default sell tariff. This first release does not add per-Key price overrides; separate model aliases/bindings provide explicit tariffs without ambiguously selecting currencies by balance.

Upstream also carries `cacheUsagePolicy`: categories `cacheRead`, `cacheWrite`, `cacheWrite1h` are explicitly `reported` or `not_applicable`. Strict defaults never fabricate supported-but-missing counts; policy-aware normalization retains per-category evidence, and nonzero raw data contradicting N/A is rejected. Web exposes this distinction for compatible endpoints that genuinely do not implement a cache category.

## Task 1: Typed contracts, exact arithmetic and usage evidence

**Files:** Create `src/gateway/mod.rs`, `amount.rs`, `config.rs`, `usage.rs`; modify `src/main.rs` only to declare `mod gateway;`.

**Interfaces:**

```rust
pub struct Amount(i128); // nonnegative; 18 decimal places; string serde
impl Amount {
    pub const ZERO: Self;
    pub fn checked_add(self, rhs: Self) -> anyhow::Result<Self>;
    pub fn checked_sub(self, rhs: Self) -> anyhow::Result<Self>;
    pub fn checked_mul_tokens(self, tokens: u64) -> anyhow::Result<Self>;
}
// FromStr + Display; price values restricted to six decimal places so
// multiplying integer tokens and dividing by 1_000_000 is exact at scale 18.
pub enum BillingUnit { KiroCredit, Cny, Usd }
pub enum RoutingMode { Sticky, WeightedRandom }
pub enum UpstreamKind { Kiro, Anthropic, OpenaiChat, OpenaiResponses }
pub struct GatewayConfig { /* fields from Shared JSON Contract */ }
impl GatewayConfig { pub fn validate(&self) -> anyhow::Result<()>; }
pub struct NativeUsage {
    pub input: u64, pub output: u64, pub cache_read: u64,
    pub cache_write: u64, pub cache_write_1h: u64,
    pub credits: Option<Amount>, pub raw: serde_json::Value,
}
pub fn normalize_usage(kind: UpstreamKind, raw: &serde_json::Value)
    -> anyhow::Result<Option<NativeUsage>>;
pub fn token_cost(prices: &TokenPrices, usage: &NativeUsage)
    -> anyhow::Result<Amount>;
```

- [ ] RED: Write tests for exact cost, no cache double counting, unknown usage, bad prices and invalid bindings. Example independent expected result:

```rust
#[test]
fn cached_input_is_not_charged_as_ordinary_input() {
    let raw = serde_json::json!({"prompt_tokens":1000,"completion_tokens":100,
        "prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":100}});
    let usage = normalize_usage(UpstreamKind::OpenaiChat, &raw).unwrap().unwrap();
    assert_eq!((usage.input, usage.cache_read, usage.cache_write), (100, 800, 100));
}
```

- [ ] Run focused `cargo test gateway::` before implementation and capture missing-behavior failure (minimal stubs may compile but must not implement behavior first).
- [ ] GREEN: Implement Amount parsing/serialization with checked integer arithmetic; reject negative/nonfinite/exponent/overflow input and precision beyond eighteen decimals. Preserve the existing native credit fixture `0.0169543708291874` exactly. Price validation allows at most six fractional digits. Use checked sum, never `as` casts for externally sized arithmetic.
- [ ] Implement full shared JSON structs, serde defaults, duplicate/reference/URL-shape/weights/TTL/context/price/unit validation. Empty default configuration is backward compatible. Native usage parsers distinguish missing required totals from confirmed zero and validate disjoint cache subsets. Anthropic TTL write breakdown must sum to total; OpenAI input includes read/write subsets when reported. Keep raw evidence, not fabricated zero cache evidence.
- [ ] Run focused tests and full Rust suite. Report test commands, RED/GREEN and any fields refined from the contract. Do not commit earlier work.

## Task 2: Durable independent spending accounts

**Files:** Create `src/gateway/ledger.rs`, `ledger_tests.rs`; register modules in `gateway/mod.rs`.

**Interfaces:** Consume Task 1 types. Produce `Ledger::open(path)`, `Ledger::open_in_memory()`, `set_account(key_id, policy)`, `accounts(key_id)`, `reserve(ReservationInput)`, `settle(SettlementInput)`, `record_attempt(AttemptInput)`, `release(attempt_id)`, `import_legacy(key_id, used, limit)`, `recover_inflight()`, `list_requests(key_id, limit)` and explicit input/view structs, all returning `anyhow::Result`. Reservation includes request/attempt IDs, unit, public model/upstream IDs, amount upper bound (optional for soft mode), config/price snapshot. Settlement includes evidence status, confirmed downstream amount (optional), attempt cost separately, committed flag and outcome. Publish exact definitions in report for later tasks.

- [ ] RED: Tests create an in-memory real SQLite DB, configure CNY limit `1`, reserve `0.7`, then assert another `0.4` reservation fails while an independent USD account remains usable. Settle same attempt twice and assert used remains `0.5`; conflicting repeat must fail, not overwrite.
- [ ] Run focused `cargo test gateway::ledger` and capture failure.
- [ ] GREEN: Use transactions and unique request/attempt/settlement keys; canonical Amount decimal strings stored as TEXT, not REAL, and calculate under the transaction in Rust rather than SQLite floating-point SUM. Account update cannot relabel units or erase usage. Reject missing accounts, exhausted limits, unsupported hard bounds, over-concurrency/pending; quota zero denies even if a caller supplies zero reservation. Exact upper bound reservation decrements availability atomically. Price/config snapshots contain only typed nonsecret version, route and tariff fields, never a serialized secret-bearing GatewayConfig.
- [ ] Persist pending evidence and in-flight attempts. Recovery changes unresolved reservations to pending; never silently zero-settle them. Release hidden failed-attempt customer reservations while still keeping upstream cost (possibly pending). Confirmed committed interruptions may settle; partial unknown usage stays pending. Support audited adjustment/new-cycle operations with reason, never a raw counter reset.
- [ ] Add idempotent legacy opening-balance import and full file-reopen tests. No trace cleanup dependence or in-memory fallback on financial DB failure. Migration must not overwrite existing accounts on restart.
- [ ] Run focused and full tests; report exact interfaces, RED/GREEN and durable behavior.

## Task 3: Validated configuration, sticky routing and weighted random

**Files:** Create `src/gateway/config_store.rs`, `routing.rs`, `routing_tests.rs`; register modules.

**Interfaces:** Consume config/amount types. Produce `ConfigStore::open(path)`, `snapshot()`, `update(expected_revision, config)` with secret-safe views; `RoutingEngine` with `preview`, `select`, `bind_success`, `mark_unavailable`; public `RouteContext`, `Candidate`, `RouteSelection`, `RouteEvidence`. Engine takes already budget-eligible candidate IDs and request capabilities; production random tickets use `fastrand`, tests inject tickets/time. Configuration snapshots are owned/Arc immutable values.

- [ ] RED: For a model with two same-tier candidates weighted `2` and `8`, supplied tickets `0,1` select the first, `2..9` the second. A valid sticky binding survives weight changes but not disabling/zero weight/budget exclusion/mode generation change. Higher-priority group always beats lower group when no valid sticky binding exists.
- [ ] Run focused `cargo test gateway::routing` and `gateway::config_store` and capture failures.
- [ ] GREEN: Exact weight multiplication and bounded summation, stable highest-score tie-breaks, scope Key+alias+session, TTL/capacity, same-session provisional lease and generation-checked success publication. Missing reliable session ID produces explicit evidence rather than Key-wide affinity. Persist no conversation body or auth key in route keys.
- [ ] Config storage uses separate `gateway.json` alongside main config, defaults only if absent, validates before atomic replace, fsync/rename with 0600 secrets. Optimistic revision checked under a lock; failed persistence does not mutate runtime. GET redacts secrets; update preserves unchanged keys and increments routing generation only for mode/security changes that invalidate bindings. Unknown/invalid config fails startup rather than falling back to empty.
- [ ] Preview is read-only: never reserves budget, renews affinity, samples production RNG or sends HTTP. Show candidates, filter reasons, weights, group and conditional probability. Selection rechecks runtime validity; don't trust preview as admission.
- [ ] Run focused/full tests and report interfaces plus redacted sample JSON.

## Task 4: Direct protocol adapters and real streaming transport

**Files:** Create `src/gateway/protocol.rs`, `transport.rs`, `sse.rs`, `adapter_tests.rs`; register modules. Existing `src/anthropic/{openai,responses}.rs` may expose conversion helpers only when semantics are lossless and tested.

**Interfaces:** `WireProtocol` enum `Anthropic`, `ChatCompletions`, `Responses`; `convert_request(from, to, body, actual_model) -> Result<Value>`; `convert_response(from, to, body, public_model) -> Result<Value>`; incremental `StreamTranslator` consumes one complete SSE event and emits destination frames plus usage/completion evidence. `DirectTransport::send(upstream, protocol, body, deadline)` returns status/headers and an incremental byte stream, or a typed upstream error. Endpoint, auth and network decisions are owned here, never by callers.

- [ ] RED: Local fixtures verify text, tool arguments split across chunks, tool result IDs and images across supported protocols; exact same-protocol unknown legitimate fields preserved; unsupported cross-protocol signed reasoning/provider state rejected, not dropped. Example:

```rust
let converted = convert_request(WireProtocol::Anthropic,
    WireProtocol::ChatCompletions,
    &json!({"model":"opus5","max_tokens":32,"messages":[
        {"role":"user","content":"hello"}]}), "real-model")?;
assert_eq!(converted["model"], "real-model");
assert_eq!(converted["messages"][0]["content"], "hello");
```

- [ ] Run focused tests to observe missing behavior.
- [ ] GREEN: Implement same-protocol pass-through and validated cross-protocol text/tools/base64-or-URL images. Capability gating rejects unsupported fields, modalities, private response references and semantic transformations. Preserve max output and reasoning constraints; never reduce them silently. No fake streaming from a buffered nonstream response.
- [ ] Stream parser handles UTF-8 boundaries, LF/CRLF, comments, multiple frames per chunk, bounded frame size, final usage and malformed/truncated streams. Chat requests ask for usage; parser does not manufacture it if missing. Responses terminal events and Anthropic start/delta cumulative usage must normalize correctly without adding cumulative counters twice.
- [ ] Transport constructs paths once; validates HTTPS except explicitly permitted private/local destinations, prevents URL credentials/fragments/query abuse, resolves and checks actual target addresses and prevents redirect/proxy bypass. Auth header comes only from the configured upstream secret. Disable redirects. Pass only protocol-required allowed headers. Timeout and cancellation bound requests; redact all error snippets.
- [ ] Typed errors distinguish invalid request, context limit, quota, throttle, authentication, transient and stream interruption. Use status plus structured body, not any occurrence of “quota” in arbitrary text. Respect Retry-After.
- [ ] Verify local HTTP/SSE server cases without real provider traffic, run focused/full suite, document unsupported conversions explicitly.

## Task 5: Runtime integration, Kiro adapter, admission and settlement

**Files:** Create `src/gateway/service.rs`, `handlers.rs`, `integration_tests.rs`; modify `src/main.rs`, `src/anthropic/{router,middleware,handlers,openai,responses,stream,websearch_loop}.rs`, `src/kiro/{provider,token_manager}.rs`, `src/admin/client_keys.rs` only at required integration points.

**Interfaces:** `GatewayService` holds configuration, routing, ledger, transport and existing Kiro provider. Public entry dispatch receives the original JSON, headers, protocol, KeyContext and existing AppState; returns `axum::response::Response`. If alias not managed, preserve legacy routing while applying the correct legacy credit ledger admission. Managed requests are coordinated before Kiro conversion. Add request-scoped Kiro overrides, never temporary global switches.

- [x] RED: Build local route fixtures: Key has exhausted credit account plus funded CNY account; Kiro quota fails, compatible Anthropic fallback succeeds, alias stays `opus5`, only CNY charged, Kiro failure cost separate. A credit-only Key cannot access money fallback. Missing credentials must not trigger any real HTTP in tests.
- [x] Run focused integration tests to observe failure.
- [x] GREEN: Identity verification no longer globally rejects a credit-exhausted Key. Keep account checks in the exact selected route. Per-request snapshots freeze model mapping, tariff, mode and retry deadline; fresh request sees saved config. `/v1/models` exposes configured public aliases with honest capabilities.
- [x] Reserve before sending; filter permissions/health/capabilities/known quotas. Monetary hard upper bounds must include configured input/output maxima and bounded internal rounds; unbounded requests reject unless account explicitly soft. Never use heuristic token count as a hard guarantee.
- [x] Kiro adapter preserves request pipeline and internal artifact loop. Propagate request-scoped actual model/group/affinity scope and disable Kiro sticky for random mode. Reuse eligible Kiro credentials within shared total retry budget. Native metering observer records raw usage independent of client estimates or `allowSimulatedCache`; aggregate internal rounds once. Credit balance/legacy JSON stats cannot be charged a second time by the new ledger.
- [x] Coordinate error retry before first effective downstream event only, with bounded total attempts/deadline across Kiro and direct requests. Exclude unsafe replay and incompatible state; after commitment report protocol-correct interruption, never concatenate another supplier. Drop/cancel leaves durable pending or confirmed settlement, not a lost charge. Success publishes affinity only if generation still valid.
- [x] Startup opens `gateway.json`/`billing.db` beside config/cache locations with fail-closed ledger errors; imports legacy Key opening balances idempotently. Legacy stats remain analytics; reset stats no longer clears quota expenditure. Deletion/rotation of Key does not delete historical ledger. Old enabled/disabled affinity choices stay respected until explicit migration.
- [x] Verify all incoming endpoints, no simulated billing, existing Kiro tests and CLI offline checks. Report exact coverage and any explicit unsupported protocol capabilities.

### Task 5 verification (takeover session)

**Endpoints.** The gateway layer sits inside auth and outside the handlers on
`/v1/messages`, `/v1/chat/completions`, `/v1/responses` and `/cc/v1/messages`.
`/v1/models` lists managed aliases and survives Kiro being unavailable.
`/v1/messages/count_tokens` is deliberately not intercepted — it bills nothing.
An unmanaged alias is rebuilt byte for byte and handed to the existing handler;
a mutation dropping the body on that path fails the pass-through test.

**No simulated billing.** `credits` is only ever accumulated from the native
`meteringEvent.usage`. The local `CacheMeter`, which `allowSimulatedCache`
controls, only splits token counts and never touches it. A test feeds enormous
cache counts with a native credit of zero and asserts the ledger charges zero.
For direct upstreams, a cost that cannot be computed settles as pending and
never as zero.

**Suites.** 1103 unit tests and 3 CLI integration tests pass in both feature
modes. Binary clippy stands at the pre-existing 120 warnings; `src/gateway/`
contributes none. The release binary's offline `--check-config` and
`--inspect-request` return `networkRequests: 0`, `localBudgetAccepted: true`,
`nativeCacheEvidence: null`.

**Unsupported protocol capabilities, stated rather than approximated.**

- Streaming across protocols is not implemented: the stream translator extracts
  usage and completion but does not convert frames, so a route whose wire
  protocol differs from the client's is skipped in favour of a matching route
  rather than forwarding frames the client cannot read.
- Request conversion exists only from Anthropic to Chat Completions and to
  Responses. The other directions are refused, not approximated.
- Provider-signed state — thinking signatures, `previous_response_id` — cannot
  be re-signed by a different upstream, so a conversion carrying it is refused
  rather than silently dropped.
- Kiro routes are executed by the existing channel, not by the gateway's own
  transport, so gateway-level cross-route retry does not apply to them; Kiro's
  own credential failover is the equivalent and already bounded.

## Task 6: Admin endpoints and working Web controls

**Files:** Create `src/admin/gateway.rs`; modify `src/admin/{mod,middleware,router,handlers}.rs`; create `admin-ui/src/{types,api,hooks}/gateway.ts`, `components/settings/gateway-section.tsx`, `gateway-form.ts`, `gateway-form.test.js`, `components/client-key-budgets.tsx`; modify settings page/sidebar and client-keys page. Update misleading profile-cache assertions in the existing Kiro settings/dashboard/trace copy and `session_affinity.rs` comments.

**Interfaces:** Authenticated `GET/PUT /api/admin/gateway/config`, `POST /gateway/preview`, `GET/PUT /client-keys/{id}/budgets`, `GET /gateway/requests`, and audited adjustment/new-cycle endpoints. Return structured `invalid_configuration`, `configuration_conflict`, `quota_exceeded`, `persistence_error`; redact secrets and raw sensitive payloads. Views match Shared JSON Contract and Task 2/3 produced types.

- [x] RED: Frontend tests submit sticky/random settings and model overrides, preserve decimal strings, reject wrong units/missing prices, and preserve edited drafts on refresh. Render real controls and separate currency labels. Backend handler tests prove unauthorized calls cannot read/modify config, update then GET reads the same effective revision and stale revision returns conflict.
- [x] Run `bun test` and focused Rust admin tests for expected failure.
- [x] GREEN: Real form inputs for upstream kind/URL/secret/weight, alias bindings/priority/weight/actual model/capabilities/cost and sell tariffs, global mode/model override/TTL, accounts/limits/allowlists. Support create/edit/remove with validation; deleting referenced upstreams fails clearly. No raw JSON editor as the only means to configure common fields.
- [x] Hot-save via backend; display saved/effective version and pending form state. Config mode changes demonstrably change mock route results. Budget UI separates credit/CNY/USD used/reserved/pending, soft-limit warnings and read-only audit evidence; legacy credit control cannot overwrite ledger usage or make money look like credits.
- [x] Route preview and request detail show actual route, evidence source, price/config versions, upstream cost vs downstream debit, typed units and failures. Masked secrets never resubmit as credentials. Update help: stickiness does not prove cache hits; unavailable native counts are unknown.
- [x] Verify backend handlers, Bun tests, `bun run build`; visual inspection if local browser access is permitted, otherwise report that limitation accurately. Do not bypass browser blocks.

### Task 6 verification (takeover session)

**Backend.** `GET/PUT /gateway/config`, `POST /gateway/preview`,
`GET /gateway/requests`, `GET/PUT /client-keys/{id}/budgets`,
`POST /client-keys/{id}/{adjustments,cycles}` and
`GET /client-keys/{id}/ledger-audit`, all behind the existing admin auth.
Errors are typed as `gateway_not_configured`, `invalid_configuration`,
`configuration_conflict`, `quota_exceeded` and `persistence_error`. 13 handler
tests run over real loopback HTTP; four mutations (conflict reported as
invalid, lazy gateway served as configured, endpoints moved outside auth,
amounts routed through f64) each fail one.

**Frontend.** Settings → 多上游网关 holds the structured editor; a key's ledger
budgets open from a wallet action on the client-keys page. `bun test` passes 42
tests across 5 files, `tsc -b` and `bun run build` are clean. Six mutations
against the form logic each fail a test.

**Not claimed.** No browser was opened, so no visual inspection or interaction
pass is asserted. Route preview is verified through its API, not through the
rendered page.

## Task 7: End-to-end regression, documentation and release artifact

**Files:** Extend `src/gateway/integration_tests.rs` and frontend behavior tests; create `docs/multi-upstream-routing.md`, `docs/multi-upstream-verification.md`; update README links. Do not alter live configuration or credentials.

- [ ] RED/GREEN for any newly uncovered bug; offline scenarios include both routing modes, TTL/concurrency, Kiro-pool-to-direct failover, exhausted accounts, currency isolation, pending usage, price snapshot, config persistence failure, restart recovery, stream cancellation and migration repeated twice.
- [ ] Run `cargo test`, `cargo test --no-default-features`, `bun test`, `bun run build`, `cargo build --release`, relevant existing offline CLI fixtures; capture exact counts, ignored tests and artifact SHA-256. No upstream test calls.
- [ ] Review final changed scope independently for security, financial invariants, integration and spec coverage. Resolve material findings and rerun covering tests; list any requirements that cannot honestly be claimed implemented.
- [ ] Write operator steps for `opus5`, main/backup groups, two weights, mode inheritance, account setup, zero vs unlimited, missing usage, price assumptions, known protocol limits and safe rollback. Cite current official usage/streaming docs used during adapter implementation.
- [ ] Deliver actual Web location, verified behavior, artifact path/hash, test evidence and no-deployment/no-live-cache-evidence statement. Leave user changes and review records intact; do not claim native cache savings from mocks.
