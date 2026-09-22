use super::*;
use crate::gateway::config_store::ConfigStore;
use crate::gateway::service::GatewayService;
use crate::gateway::{
    BudgetPolicy, GatewayConfig, PublicModel, RoutingMode, Upstream, UpstreamKind,
};
use std::path::PathBuf;

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("kiro-adm-{name}-{}", uuid::Uuid::new_v4()))
}

fn prices(unit: BillingUnit, input: &str, output: &str) -> TokenPrices {
    TokenPrices {
        currency: unit,
        input: input.parse().unwrap(),
        output: output.parse().unwrap(),
        cache_read: "0".parse().unwrap(),
        cache_write: "0".parse().unwrap(),
        cache_write_1h: None,
    }
}

fn binding(id: &str, upstream: &str, unit: BillingUnit, priced: bool) -> ModelBinding {
    ModelBinding {
        id: id.into(),
        upstream_id: upstream.into(),
        upstream_model: format!("m-{id}"),
        enabled: true,
        priority_tier: 0,
        weight: 10,
        context_window: 1_000_000,
        max_output_tokens: 1_000_000,
        supports_tools: true,
        supports_images: true,
        supports_reasoning: true,
        allow_model_substitution: false,
        billing_unit: unit,
        cost_prices: (unit != BillingUnit::KiroCredit).then(|| prices(unit, "1", "2")),
        sell_prices: priced.then(|| prices(unit, "3", "6")),
    }
}

fn upstream(id: &str, kind: UpstreamKind) -> Upstream {
    Upstream {
        id: id.into(),
        name: id.into(),
        kind,
        enabled: true,
        weight: 10,
        base_url: (kind != UpstreamKind::Kiro).then(|| "https://api.example.test".into()),
        api_key: (kind != UpstreamKind::Kiro).then(|| "k".into()),
        has_api_key: kind != UpstreamKind::Kiro,
        allow_private_network: false,
        kiro_group: None,
        cache_usage_policy: None,
    }
}

fn policy(unit: BillingUnit, limit: Option<&str>, enforcement: BudgetEnforcement) -> BudgetPolicy {
    BudgetPolicy {
        unit,
        limit: limit.map(|l| l.parse().unwrap()),
        enforcement,
        max_in_flight: 4,
        max_pending: 8,
        allowed_models: vec![],
        allowed_upstreams: vec![],
    }
}

/// 建一个带 (积分路, 人民币路) 两条备选的计划。
fn plan_with(
    bindings: Vec<ModelBinding>,
    upstreams: Vec<Upstream>,
) -> (GatewayService, RequestPlan, PathBuf, PathBuf) {
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
    let service = GatewayService::open(&config_path, &ledger_path).unwrap();
    let plan = service.plan_for("opus5").unwrap();
    (service, plan, config_path, ledger_path)
}

fn verdicts(ledger: &Ledger, key: u64, plan: &RequestPlan) -> Vec<(String, CandidateVerdict)> {
    evaluate(ledger, key, plan, &plan.candidates).unwrap()
}

fn verdict_for<'a>(list: &'a [(String, CandidateVerdict)], id: &str) -> &'a CandidateVerdict {
    &list.iter().find(|(b, _)| b == id).unwrap().1
}

/// 积分耗尽但人民币有余额的 Key：按钱计费的那条路仍应放行。
/// 旧行为是「积分用尽 → 整个 Key 被拒」，多上游之后那是错的。
#[test]
fn an_exhausted_credit_account_does_not_block_a_funded_money_route() {
    let (service, plan, config_path, ledger_path) = plan_with(
        vec![
            binding("credit", "kiro", BillingUnit::KiroCredit, false),
            binding("money", "direct", BillingUnit::Cny, true),
        ],
        vec![
            upstream("kiro", UpstreamKind::Kiro),
            upstream("direct", UpstreamKind::Anthropic),
        ],
    );
    let ledger = service.ledger().unwrap();

    // 积分账户额度为 0（耗尽），人民币账户有额度且为软策略。
    ledger
        .set_account(
            7,
            policy(BillingUnit::KiroCredit, Some("0"), BudgetEnforcement::Soft),
        )
        .unwrap();
    ledger
        .set_account(
            7,
            policy(BillingUnit::Cny, Some("100"), BudgetEnforcement::Soft),
        )
        .unwrap();

    let list = verdicts(ledger, 7, &plan);
    assert_eq!(
        verdict_for(&list, "credit"),
        &CandidateVerdict::Refused(Refusal::Exhausted {
            unit: BillingUnit::KiroCredit
        })
    );
    assert!(
        matches!(verdict_for(&list, "money"), CandidateVerdict::Eligible(_)),
        "人民币账户有余额，这条路必须放行"
    );
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 只有积分账户的 Key **够不到**按钱计费的备路——它在那个币种上根本没有账户。
#[test]
fn a_credit_only_key_cannot_reach_a_money_route() {
    let (service, plan, config_path, ledger_path) = plan_with(
        vec![
            binding("credit", "kiro", BillingUnit::KiroCredit, false),
            binding("money", "direct", BillingUnit::Cny, true),
        ],
        vec![
            upstream("kiro", UpstreamKind::Kiro),
            upstream("direct", UpstreamKind::Anthropic),
        ],
    );
    let ledger = service.ledger().unwrap();
    ledger
        .set_account(
            7,
            policy(BillingUnit::KiroCredit, Some("50"), BudgetEnforcement::Soft),
        )
        .unwrap();

    let list = verdicts(ledger, 7, &plan);
    assert!(matches!(
        verdict_for(&list, "credit"),
        CandidateVerdict::Eligible(_)
    ));
    assert_eq!(
        verdict_for(&list, "money"),
        &CandidateVerdict::Refused(Refusal::NoAccountForUnit {
            unit: BillingUnit::Cny
        }),
        "没有该币种账户和额度用尽是两回事，必须分开报"
    );
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 硬额度账户必须拿到**由配置推出**的保证上界，且覆盖配置允许的全部轮次。
#[test]
fn a_hard_account_gets_a_bound_derived_from_declared_maxima() {
    let (service, plan, config_path, ledger_path) = plan_with(
        vec![binding("money", "direct", BillingUnit::Cny, true)],
        vec![upstream("direct", UpstreamKind::Anthropic)],
    );
    let ledger = service.ledger().unwrap();
    ledger
        .set_account(
            7,
            policy(BillingUnit::Cny, Some("1000000"), BudgetEnforcement::Hard),
        )
        .unwrap();

    let list = verdicts(ledger, 7, &plan);
    let CandidateVerdict::Eligible(admission) = verdict_for(&list, "money") else {
        panic!("硬额度且价格齐全时应放行");
    };
    assert_eq!(admission.bound_kind, BoundKind::Guaranteed);
    // 输入 100 万 token × 单价 3/百万 = 3；输出 100 万 × 6/百万 = 6；单轮 9，3 轮 27。
    assert_eq!(admission.bound, Some("27".parse::<Amount>().unwrap()));
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 硬额度账户在算不出保证上界时必须**拒绝**，而不是拿估算冒充保证。
///
/// 直接单测 `guaranteed_bound`：按钱计费却缺售价的配置根本加载不进来
/// （校验器第 439 行 `monetary billing requires sellPrices`），
/// 所以这条分支是防御性的，不绕过校验去伪造一个能加载的坏配置。
#[test]
fn no_guarantee_can_be_derived_without_prices_or_declared_maxima() {
    let mut unpriced = binding("money", "direct", BillingUnit::Cny, false);
    unpriced.sell_prices = None;
    let error = guaranteed_bound(&unpriced, 1).unwrap_err();
    assert!(format!("{error:#}").contains("no sell prices"));

    // 币种与售价币种不一致：算出来的数字没有意义。
    let mut mismatched = binding("money", "direct", BillingUnit::Cny, true);
    mismatched.sell_prices = Some(prices(BillingUnit::Usd, "1", "1"));
    let error = guaranteed_bound(&mismatched, 1).unwrap_err();
    assert!(format!("{error:#}").contains("does not match its billing unit"));

    // 未声明完整的输入/输出上限：没有上限就没有保证。
    for (window, output) in [(0_u64, 100_u64), (100, 0)] {
        let mut incomplete = binding("money", "direct", BillingUnit::Cny, true);
        incomplete.context_window = window;
        incomplete.max_output_tokens = output;
        let error = guaranteed_bound(&incomplete, 1).unwrap_err();
        assert!(format!("{error:#}").contains("no guaranteed upper bound exists"));
    }
}

/// 软额度账户允许无保证上界——原生积分本就没有可证的调用前上界。
#[test]
fn a_soft_account_may_proceed_without_a_bound() {
    let (service, plan, config_path, ledger_path) = plan_with(
        vec![binding("credit", "kiro", BillingUnit::KiroCredit, false)],
        vec![upstream("kiro", UpstreamKind::Kiro)],
    );
    let ledger = service.ledger().unwrap();
    ledger
        .set_account(
            7,
            policy(BillingUnit::KiroCredit, None, BudgetEnforcement::Soft),
        )
        .unwrap();
    let list = verdicts(ledger, 7, &plan);
    let CandidateVerdict::Eligible(admission) = verdict_for(&list, "credit") else {
        panic!("软额度应放行");
    };
    assert_eq!(admission.bound, None);
    assert_eq!(admission.bound_kind, BoundKind::Unknown);
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 账户的授权列表限制到具体模型/上游时，不在列表里的路要拒。
#[test]
fn an_account_authorization_list_is_enforced_per_route() {
    let (service, plan, config_path, ledger_path) = plan_with(
        vec![
            binding("a", "kiro", BillingUnit::KiroCredit, false),
            binding("b", "other", BillingUnit::KiroCredit, false),
        ],
        vec![
            upstream("kiro", UpstreamKind::Kiro),
            upstream("other", UpstreamKind::Kiro),
        ],
    );
    let ledger = service.ledger().unwrap();
    let mut restricted = policy(BillingUnit::KiroCredit, Some("50"), BudgetEnforcement::Soft);
    restricted.allowed_upstreams = vec!["kiro".into()];
    ledger.set_account(7, restricted).unwrap();

    let list = verdicts(ledger, 7, &plan);
    assert!(matches!(
        verdict_for(&list, "a"),
        CandidateVerdict::Eligible(_)
    ));
    assert_eq!(
        verdict_for(&list, "b"),
        &CandidateVerdict::Refused(Refusal::NotAuthorized {
            unit: BillingUnit::KiroCredit
        })
    );
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 上界计算取输入侧最贵的单价：无法预知这次输入落在普通输入还是缓存读写，
/// 取最大值才是"保证不被突破"。
#[test]
fn the_bound_uses_the_dearest_input_price() {
    let mut binding = binding("money", "direct", BillingUnit::Cny, true);
    binding.context_window = 1_000_000;
    binding.max_output_tokens = 0;
    // 缓存写入比普通输入贵得多。
    let mut dear = prices(BillingUnit::Cny, "1", "0");
    dear.cache_write = "10".parse().unwrap();
    binding.sell_prices = Some(dear);

    binding.max_output_tokens = 1;
    let bound = guaranteed_bound(&binding, 1).unwrap();
    // 输入 100 万 token 按 cache_write 的 10/百万 计 = 10；
    // 输出单价为 0 故不贡献。若误按 input 的 1 计，结果会是 1。
    assert_eq!(bound, "10".parse::<Amount>().unwrap());
}

/// 并发/待结算达到上限时拒绝，且与"额度用尽"分开报。
#[test]
fn concurrency_limits_are_reported_separately_from_exhaustion() {
    let (service, plan, config_path, ledger_path) = plan_with(
        vec![binding("credit", "kiro", BillingUnit::KiroCredit, false)],
        vec![upstream("kiro", UpstreamKind::Kiro)],
    );
    let ledger = service.ledger().unwrap();
    let mut tight = policy(BillingUnit::KiroCredit, Some("50"), BudgetEnforcement::Soft);
    tight.max_in_flight = 1;
    ledger.set_account(7, tight).unwrap();

    // 先占住唯一的并发名额。
    ledger
        .reserve(crate::gateway::ledger_types::ReservationInput {
            request_id: "r1".into(),
            attempt_id: "a1".into(),
            key_id: 7,
            unit: BillingUnit::KiroCredit,
            public_model: "opus5".into(),
            upstream_id: "kiro".into(),
            upper_bound: None,
            bound_kind: BoundKind::Unknown,
            snapshot: crate::gateway::ledger_types::PriceSnapshot {
                config_revision: 1,
                price_revision: 1,
                binding_id: "credit".into(),
                upstream_kind: UpstreamKind::Kiro,
                upstream_model: "m-credit".into(),
                cost_prices: None,
                sell_prices: None,
            },
        })
        .unwrap();

    let list = verdicts(ledger, 7, &plan);
    assert_eq!(
        verdict_for(&list, "credit"),
        &CandidateVerdict::Refused(Refusal::TooManyInFlight {
            unit: BillingUnit::KiroCredit
        })
    );
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 没有任何账户的 Key：每条路都报"没有该币种账户"，而不是笼统地不可用。
#[test]
fn a_key_without_accounts_is_refused_per_unit() {
    let (service, plan, config_path, ledger_path) = plan_with(
        vec![binding("credit", "kiro", BillingUnit::KiroCredit, false)],
        vec![upstream("kiro", UpstreamKind::Kiro)],
    );
    let list = verdicts(service.ledger().unwrap(), 999, &plan);
    assert_eq!(
        verdict_for(&list, "credit"),
        &CandidateVerdict::Refused(Refusal::NoAccountForUnit {
            unit: BillingUnit::KiroCredit
        })
    );
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
    let _ = ConfigStore::open(temp("unused"));
}
