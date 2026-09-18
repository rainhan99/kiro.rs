use super::*;
use crate::gateway::service::GatewayService;
use crate::gateway::transport::UpstreamFailure;
use crate::gateway::{
    BudgetEnforcement, BudgetPolicy, GatewayConfig, PublicModel, RoutingMode, TokenPrices,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("kiro-dispatch-{name}-{}", uuid::Uuid::new_v4()))
}

/// 一次被记录的发送。
#[derive(Debug, Clone, PartialEq)]
struct Call {
    upstream_id: String,
    protocol: WireProtocol,
    body: Value,
}

type Script = Box<dyn Fn() -> std::result::Result<Value, SendError> + Send + Sync>;

/// 不碰网络的执行器：脚本化应答，并记下每一次发送。
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Call>>,
    scripts: Mutex<HashMap<String, Arc<Script>>>,
}

impl Fake {
    fn on(&self, upstream_id: &str, script: Script) -> &Self {
        self.scripts
            .lock()
            .insert(upstream_id.into(), Arc::new(script));
        self
    }
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().clone()
    }
}

impl RouteExecutor for Fake {
    fn execute<'a>(
        &'a self,
        upstream: &'a Upstream,
        protocol: WireProtocol,
        body: Vec<u8>,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<Value, SendError>> + Send + 'a>> {
        self.calls.lock().push(Call {
            upstream_id: upstream.id.clone(),
            protocol,
            body: serde_json::from_slice(&body).unwrap(),
        });
        let script = self.scripts.lock().get(&upstream.id).cloned();
        Box::pin(async move {
            match script {
                Some(script) => script(),
                None => Err(SendError::Upstream(UpstreamFailure::Transient {
                    status: 503,
                    snippet: "no script".into(),
                })),
            }
        })
    }
}

fn answers(usage_input: u64, usage_output: u64) -> Script {
    Box::new(move || {
        Ok(serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "real-upstream-model",
            "content": [{"type": "text", "text": "hi"}],
            // 真实的 Anthropic 响应带齐 cache 计数。strict 策略下"字段缺失"
            // 不等于 0——见下面 an_incomplete_cache_report_is_not_priced_as_zero。
            "usage": {
                "input_tokens": usage_input,
                "output_tokens": usage_output,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0
            }
        }))
    })
}

fn fails(status: u16) -> Script {
    Box::new(move || {
        Err(SendError::Upstream(UpstreamFailure::Transient {
            status,
            snippet: "upstream down".into(),
        }))
    })
}

fn price(v: &str) -> TokenPrices {
    TokenPrices {
        currency: BillingUnit::Cny,
        input: v.parse().unwrap(),
        output: v.parse().unwrap(),
        cache_read: "0".parse().unwrap(),
        cache_write: "0".parse().unwrap(),
        cache_write_1h: None,
    }
}

fn kiro_upstream() -> Upstream {
    // Kiro 上游不带 baseUrl/apiKey——凭据来自既有的凭据池（Task 1 的校验也这么要求）。
    Upstream {
        id: "kiro".into(),
        name: "kiro".into(),
        kind: UpstreamKind::Kiro,
        enabled: true,
        weight: 10,
        base_url: None,
        api_key: None,
        has_api_key: false,
        allow_private_network: false,
        kiro_group: None,
        cache_usage_policy: None,
    }
}

fn direct_upstream(id: &str) -> Upstream {
    Upstream {
        id: id.into(),
        name: id.into(),
        kind: UpstreamKind::Anthropic,
        enabled: true,
        weight: 10,
        base_url: Some("https://api.example.test".into()),
        api_key: Some("k".into()),
        has_api_key: true,
        allow_private_network: false,
        kiro_group: None,
        cache_usage_policy: None,
    }
}

fn credit_binding(id: &str, upstream: &str, tier: u32) -> ModelBinding {
    ModelBinding {
        id: id.into(),
        upstream_id: upstream.into(),
        upstream_model: format!("real-{id}"),
        enabled: true,
        priority_tier: tier,
        weight: 10,
        context_window: 200_000,
        max_output_tokens: 8_000,
        supports_tools: true,
        supports_images: true,
        supports_reasoning: true,
        allow_model_substitution: false,
        billing_unit: BillingUnit::KiroCredit,
        cost_prices: None,
        sell_prices: None,
    }
}

fn money_binding(id: &str, upstream: &str, tier: u32) -> ModelBinding {
    ModelBinding {
        billing_unit: BillingUnit::Cny,
        cost_prices: Some(price("1")),
        sell_prices: Some(price("2")),
        ..credit_binding(id, upstream, tier)
    }
}

struct Fixture {
    service: Arc<GatewayService>,
    config_path: PathBuf,
    ledger_path: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.config_path);
        let _ = std::fs::remove_file(&self.ledger_path);
    }
}

fn fixture(bindings: Vec<ModelBinding>, upstreams: Vec<Upstream>) -> Fixture {
    let config_path = temp("config.json");
    let ledger_path = temp("billing.db");
    let config = GatewayConfig {
        models: vec![PublicModel {
            id: "opus5".into(),
            display_name: None,
            routing_mode: Some(RoutingMode::Sticky),
            affinity_ttl_secs: None,
            bindings,
        }],
        upstreams,
        ..GatewayConfig::default()
    };
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let service = Arc::new(GatewayService::open(&config_path, &ledger_path).unwrap());
    Fixture {
        service,
        config_path,
        ledger_path,
    }
}

fn money_account(f: &Fixture, key_id: u64, limit: &str) {
    f.service
        .ledger()
        .unwrap()
        .set_account(
            key_id,
            BudgetPolicy {
                unit: BillingUnit::Cny,
                limit: Some(limit.parse().unwrap()),
                enforcement: BudgetEnforcement::Soft,
                max_in_flight: 8,
                max_pending: 16,
                allowed_models: vec![],
                allowed_upstreams: vec![],
            },
        )
        .unwrap();
}

/// 积分账户：已用 = 上限即为耗尽。
fn credit_account(f: &Fixture, key_id: u64, used: f64, limit: Option<f64>) {
    crate::gateway::import::import_opening_balances(
        f.service.ledger().unwrap(),
        &[crate::gateway::import::LegacyKeyBalance {
            key_id,
            used,
            limit,
        }],
    )
    .unwrap();
}

fn ctx() -> RouteContext {
    RouteContext {
        key_id: 7,
        public_model: "opus5".into(),
        session_id: Some("session-0123456789".into()),
        needs_tools: false,
        needs_images: false,
        needs_reasoning: false,
    }
}

fn request() -> Value {
    serde_json::json!({
        "model": "opus5",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

fn account(f: &Fixture, key_id: u64, unit: BillingUnit) -> super::super::ledger_types::AccountView {
    f.service
        .ledger()
        .unwrap()
        .accounts(key_id)
        .unwrap()
        .into_iter()
        .find(|a| a.policy.unit == unit)
        .unwrap_or_else(|| panic!("没有 {unit:?} 账户"))
}

/// 不接管的别名一个字节都不该被碰：调用方原样走既有路径。
#[tokio::test]
async fn an_unmanaged_alias_is_left_to_the_legacy_path() {
    let f = fixture(
        vec![money_binding("m", "d1", 0)],
        vec![direct_upstream("d1")],
    );
    let fake = Fake::default();
    let mut other = ctx();
    other.public_model = "some-other-model".into();

    let result = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &request(),
        &other,
        "r1",
    )
    .await;

    assert!(matches!(result, Dispatched::NotManaged));
    assert!(fake.calls().is_empty(), "不接管就不该发出任何请求");
}

/// 规格点名的那一条：积分账户已耗尽、人民币账户有余额的 Key，
/// 走按钱计费的备路完全应该放行——别名不变，且**只扣人民币**。
#[tokio::test]
async fn an_exhausted_credit_account_does_not_block_a_funded_money_route() {
    let f = fixture(
        vec![credit_binding("k", "kiro", 0), money_binding("m", "d1", 1)],
        vec![kiro_upstream(), direct_upstream("d1")],
    );
    credit_account(&f, 7, 10.0, Some(10.0)); // 耗尽
    money_account(&f, 7, "100");

    let fake = Fake::default();
    fake.on("d1", answers(1_000, 500));

    let result = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &request(),
        &ctx(),
        "r1",
    )
    .await;

    let Dispatched::Answered(answer) = result else {
        panic!("应走通备路，实得 {result:?}");
    };
    assert_eq!(answer["model"], "opus5", "对外别名不得变成上游真实模型名");

    // 只碰了按钱计费那条路，积分那条连发都没发。
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].upstream_id, "d1");
    assert_eq!(calls[0].body["model"], "real-m", "上游收到的是真实模型名");

    // 积分账户分文未动；人民币按售价 2/百万 token 扣了 1500 token。
    assert_eq!(
        account(&f, 7, BillingUnit::KiroCredit).used,
        "10".parse().unwrap()
    );
    assert_eq!(
        account(&f, 7, BillingUnit::Cny).used,
        "0.003".parse().unwrap(),
        "1500 token × 2/百万 = 0.003"
    );
}

/// 反过来也必须成立：只有积分账户的 Key **够不到**按钱计费的备路——
/// 它在那个币种上根本没有账户。
#[tokio::test]
async fn a_credit_only_key_cannot_reach_a_money_route() {
    let f = fixture(
        vec![credit_binding("k", "kiro", 0), money_binding("m", "d1", 1)],
        vec![kiro_upstream(), direct_upstream("d1")],
    );
    credit_account(&f, 7, 10.0, Some(10.0)); // 耗尽，且没有人民币账户

    let fake = Fake::default();
    fake.on("d1", answers(10, 10));

    let result = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &request(),
        &ctx(),
        "r1",
    )
    .await;

    let Dispatched::Refused { status, reason } = result else {
        panic!("没有对应币种的账户就不该放行，实得 {result:?}");
    };
    assert_eq!(status, 402, "积分耗尽是可补救的，优先报这条");
    assert!(reason.contains("exhausted"), "实得：{reason}");
    assert!(fake.calls().is_empty(), "一次都不该发出去");
}

/// 提交前失败换路重试，两条路的账各记各的。
#[tokio::test]
async fn a_failure_before_commitment_moves_to_another_route() {
    let f = fixture(
        vec![money_binding("a", "d1", 0), money_binding("b", "d2", 1)],
        vec![direct_upstream("d1"), direct_upstream("d2")],
    );
    money_account(&f, 7, "100");

    let fake = Fake::default();
    fake.on("d1", fails(503));
    fake.on("d2", answers(1_000, 0));

    let result = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &request(),
        &ctx(),
        "r1",
    )
    .await;

    assert!(matches!(result, Dispatched::Answered(_)));
    let calls = fake.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].upstream_id, "d1");
    assert_eq!(calls[1].upstream_id, "d2");

    // 失败那次释放掉了，只有成功那次计费。
    let cny = account(&f, 7, BillingUnit::Cny);
    assert_eq!(cny.used, "0.002".parse().unwrap(), "1000 token × 2/百万");
    assert_eq!(cny.in_flight, 0);
    assert_eq!(cny.customer_pending, 0);
}

/// 上游算过钱了但响应转不回来：**如实结算、绝不重试**。
/// 重试等于为同一个请求付两次。
#[tokio::test]
async fn a_response_that_cannot_be_converted_is_still_charged_and_never_retried() {
    let mut openai = direct_upstream("d1");
    openai.kind = UpstreamKind::OpenaiChat;
    let f = fixture(
        vec![money_binding("a", "d1", 0), money_binding("b", "d2", 1)],
        vec![openai, direct_upstream("d2")],
    );
    money_account(&f, 7, "100");

    let fake = Fake::default();
    // 用量报得齐全（所以算得出钱），但主体缺 choices，转回 Anthropic 会失败。
    fake.on(
        "d1",
        Box::new(|| {
            Ok(serde_json::json!({
                "id": "chatcmpl-1",
                "usage": {
                    "prompt_tokens": 1_000,
                    "completion_tokens": 500,
                    "prompt_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0}
                }
            }))
        }),
    );
    fake.on("d2", answers(10, 10));

    let result = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &request(),
        &ctx(),
        "r1",
    )
    .await;

    let Dispatched::Refused { status, reason } = result else {
        panic!("转不回来就不该报成功，实得 {result:?}");
    };
    assert_eq!(status, 502);
    assert!(reason.contains("charged"), "必须说明钱已经花了：{reason}");
    assert_eq!(fake.calls().len(), 1, "绝不因为转换失败而换路重试");
    // 关键：客户端收到的是错误，但账**必须**记上——上游确实算过这笔钱。
    let cny = account(&f, 7, BillingUnit::Cny);
    assert_eq!(
        cny.used,
        "0.003".parse().unwrap(),
        "转换失败不改变上游已经发生的消耗"
    );
    assert_eq!(cny.in_flight, 0, "不得把这笔留在在飞状态");
}

/// 本次这条路的协议接不住请求时，换一条——别的路可能原生说客户端的协议。
#[tokio::test]
async fn a_request_this_route_cannot_express_moves_to_another() {
    let mut responses = direct_upstream("d1");
    responses.kind = UpstreamKind::OpenaiResponses;
    let f = fixture(
        vec![money_binding("a", "d1", 0), money_binding("b", "d2", 1)],
        vec![responses, direct_upstream("d2")],
    );
    money_account(&f, 7, "100");

    let fake = Fake::default();
    fake.on("d2", answers(10, 10));

    // 带签名的推理块无法在另一个厂商重新签名，转换会被拒绝。
    let mut body = request();
    body["messages"][0]["content"] = serde_json::json!([
        {"type": "thinking", "thinking": "...", "signature": "sig-abc"}
    ]);

    let result = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &body,
        &ctx(),
        "r1",
    )
    .await;

    assert!(matches!(result, Dispatched::Answered(_)), "实得 {result:?}");
    let calls = fake.calls();
    assert_eq!(calls.len(), 1, "转换失败的那条路不该发出请求");
    assert_eq!(calls[0].upstream_id, "d2");
}

/// 上游没报用量时记为待结算，**不得用 0 顶替**。
#[tokio::test]
async fn a_missing_usage_settles_as_pending_not_as_zero() {
    let f = fixture(
        vec![money_binding("a", "d1", 0)],
        vec![direct_upstream("d1")],
    );
    money_account(&f, 7, "100");

    let fake = Fake::default();
    fake.on(
        "d1",
        Box::new(|| {
            Ok(serde_json::json!({
                "id": "msg_1", "type": "message", "role": "assistant",
                "model": "x", "content": [{"type": "text", "text": "hi"}]
            }))
        }),
    );

    let result = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &request(),
        &ctx(),
        "r1",
    )
    .await;

    assert!(matches!(result, Dispatched::Answered(_)));
    let cny = account(&f, 7, BillingUnit::Cny);
    assert_eq!(cny.used, Amount::ZERO, "没有证据就不该记为已用");
    assert_eq!(cny.customer_pending, 1, "必须留下待结算的义务");
}

/// 缓存用量报得不全时**不得按 0 计价**：少报一个缓存写入就是少收一笔钱，
/// 而且是静悄悄地少收。这种情况记为待结算，等证据齐了再说。
#[tokio::test]
async fn an_incomplete_cache_report_is_not_priced_as_zero() {
    let f = fixture(
        vec![money_binding("a", "d1", 0)],
        vec![direct_upstream("d1")],
    );
    money_account(&f, 7, "100");

    let fake = Fake::default();
    fake.on(
        "d1",
        Box::new(|| {
            Ok(serde_json::json!({
                "id": "msg_1", "type": "message", "role": "assistant",
                "model": "x", "content": [{"type": "text", "text": "hi"}],
                // 有 token 数，但一个缓存计数都没报。
                "usage": {"input_tokens": 1_000, "output_tokens": 500}
            }))
        }),
    );

    let result = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &request(),
        &ctx(),
        "r1",
    )
    .await;

    assert!(
        matches!(result, Dispatched::Answered(_)),
        "请求本身是成功的"
    );
    let cny = account(&f, 7, BillingUnit::Cny);
    assert_eq!(cny.used, Amount::ZERO, "证据不全不得当成 0 计价");
    assert_eq!(cny.customer_pending, 1, "必须留下待结算的义务");
}

/// 上游用了 1 小时缓存写入，而运维压根没给这一项定价：算不出金额，
/// 记为待结算。按 0 计价等于静悄悄少收一笔钱。
///
/// 这条打的是"用量有效、但定价失败"这个分支——缓存报不全的那种在
/// `normalize_usage` 就返回 None 了，够不到这里。
#[tokio::test]
async fn an_unpriced_cache_category_is_not_charged_as_zero() {
    let f = fixture(
        vec![money_binding("a", "d1", 0)],
        vec![direct_upstream("d1")],
    );
    money_account(&f, 7, "100");
    // money_binding 的售价里没有 cacheWrite1h。
    assert!(
        f.service
            .plan_for("opus5")
            .unwrap()
            .binding("a")
            .unwrap()
            .sell_prices
            .as_ref()
            .unwrap()
            .cache_write_1h
            .is_none(),
        "前提：这条绑定没给 1 小时缓存写入定价"
    );

    let fake = Fake::default();
    fake.on(
        "d1",
        Box::new(|| {
            Ok(serde_json::json!({
                "id": "msg_1", "type": "message", "role": "assistant",
                "model": "x", "content": [{"type": "text", "text": "hi"}],
                "usage": {
                    "input_tokens": 1_000,
                    "output_tokens": 500,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 100,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 0,
                        "ephemeral_1h_input_tokens": 100
                    }
                }
            }))
        }),
    );

    let result = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &request(),
        &ctx(),
        "r1",
    )
    .await;

    assert!(
        matches!(result, Dispatched::Answered(_)),
        "请求本身是成功的"
    );
    let cny = account(&f, 7, BillingUnit::Cny);
    assert_eq!(cny.used, Amount::ZERO, "算不出价不得按 0 记为已确认");
    assert_eq!(cny.customer_pending, 1, "必须留下待结算的义务");
}

/// 所有路都失败时报 502，并说清是"都试过了"。
#[tokio::test]
async fn every_route_failing_is_reported_as_such() {
    let f = fixture(
        vec![money_binding("a", "d1", 0), money_binding("b", "d2", 1)],
        vec![direct_upstream("d1"), direct_upstream("d2")],
    );
    money_account(&f, 7, "100");

    let fake = Fake::default();
    fake.on("d1", fails(503));
    fake.on("d2", fails(500));

    let result = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &request(),
        &ctx(),
        "r1",
    )
    .await;

    let Dispatched::Refused { status, .. } = result else {
        panic!("实得 {result:?}");
    };
    assert_eq!(status, 502);
    assert_eq!(fake.calls().len(), 2);
    // 全部释放，不留在飞。
    assert_eq!(account(&f, 7, BillingUnit::Cny).in_flight, 0);
}

/// Kiro 路由**交回**既有通道执行，并带上请求级覆盖。
///
/// 网关不替 Kiro 发请求：用量真相只有原生 `metadataEvent.tokenUsage` 一个来源，
/// 既有通道已经在正确提取它，网关另起一套解析迟早与它对不上。
#[tokio::test]
async fn a_kiro_route_is_handed_back_with_request_scoped_overrides() {
    let mut kiro = kiro_upstream();
    kiro.kiro_group = Some("team-a".into());
    let f = fixture(vec![credit_binding("k", "kiro", 0)], vec![kiro]);
    credit_account(&f, 7, 0.0, Some(100.0));

    let fake = Fake::default();
    let result = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &request(),
        &ctx(),
        "r1",
    )
    .await;

    let Dispatched::UseKiro(route) = result else {
        panic!("Kiro 路该交回既有通道，实得 {result:?}");
    };
    assert_eq!(route.upstream_model, "real-k", "覆盖成这一路的真实模型名");
    assert_eq!(
        route.group.as_deref(),
        Some("team-a"),
        "覆盖成这一路的凭据分组"
    );
    assert!(fake.calls().is_empty(), "网关不该替 Kiro 发请求");

    // 预留已经记在账上：先记账再执行，崩溃也不会丢掉这笔。
    let reserved = account(&f, 7, BillingUnit::KiroCredit);
    assert_eq!(reserved.in_flight, 1, "交回之前必须已经预留");

    // 结算句柄指向的正是这条预留。
    route
        .settlement
        .succeeded(Some("2.5".parse().unwrap()), None)
        .unwrap();
    let settled = account(&f, 7, BillingUnit::KiroCredit);
    assert_eq!(settled.in_flight, 0);
    assert_eq!(settled.used, "2.5".parse().unwrap(), "按原生积分结算");
}

/// 拿不到原生积分时记为待结算，**绝不用 0 顶替**——0 的意思是"确认没花钱"。
#[tokio::test]
async fn a_kiro_call_without_native_credits_settles_as_pending() {
    let f = fixture(vec![credit_binding("k", "kiro", 0)], vec![kiro_upstream()]);
    credit_account(&f, 7, 0.0, Some(100.0));

    let fake = Fake::default();
    let Dispatched::UseKiro(route) = dispatch(
        &f.service,
        &fake,
        WireProtocol::Anthropic,
        &request(),
        &ctx(),
        "r1",
    )
    .await
    else {
        panic!("应交回 Kiro 通道");
    };

    route.settlement.succeeded(None, None).unwrap();
    let settled = account(&f, 7, BillingUnit::KiroCredit);
    assert_eq!(settled.used, Amount::ZERO, "没有证据不得记为已用");
    assert_eq!(settled.customer_pending, 1, "必须留下待结算义务");
}

/// 未提交的失败释放预留；已提交的失败保留义务。
#[tokio::test]
async fn a_kiro_failure_releases_only_when_nothing_reached_the_client() {
    for (committed, expect_pending) in [(false, 0), (true, 1)] {
        let f = fixture(vec![credit_binding("k", "kiro", 0)], vec![kiro_upstream()]);
        credit_account(&f, 7, 0.0, Some(100.0));
        let fake = Fake::default();
        let Dispatched::UseKiro(route) = dispatch(
            &f.service,
            &fake,
            WireProtocol::Anthropic,
            &request(),
            &ctx(),
            "r1",
        )
        .await
        else {
            panic!("应交回 Kiro 通道");
        };

        route.settlement.failed(committed).unwrap();
        let settled = account(&f, 7, BillingUnit::KiroCredit);
        assert_eq!(settled.in_flight, 0, "committed={committed}");
        assert_eq!(
            settled.customer_pending, expect_pending,
            "已向下游发过内容才保留义务；committed={committed}"
        );
    }
}
