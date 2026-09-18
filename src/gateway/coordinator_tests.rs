use super::*;
use crate::gateway::service::GatewayService;
use crate::gateway::{
    BudgetEnforcement, BudgetPolicy, GatewayConfig, ModelBinding, PublicModel, RoutingMode,
    TokenPrices, Upstream, UpstreamKind,
};
use std::path::PathBuf;

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("kiro-coord-{name}-{}", uuid::Uuid::new_v4()))
}

fn binding(id: &str, upstream: &str, tier: u32) -> ModelBinding {
    ModelBinding {
        id: id.into(),
        upstream_id: upstream.into(),
        upstream_model: format!("m-{id}"),
        enabled: true,
        priority_tier: tier,
        weight: 10,
        context_window: 100_000,
        max_output_tokens: 4_000,
        supports_tools: true,
        supports_images: true,
        supports_reasoning: true,
        allow_model_substitution: false,
        billing_unit: BillingUnit::Cny,
        cost_prices: Some(price("1")),
        sell_prices: Some(price("2")),
    }
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

fn upstream(id: &str) -> Upstream {
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

struct Fixture {
    service: GatewayService,
    plan: RequestPlan,
    config_path: PathBuf,
    ledger_path: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.config_path);
        let _ = std::fs::remove_file(&self.ledger_path);
    }
}

fn fixture(bindings: Vec<ModelBinding>, limit: Option<&str>) -> Fixture {
    let config_path = temp("config.json");
    let ledger_path = temp("billing.db");
    let upstream_ids: Vec<String> = bindings.iter().map(|b| b.upstream_id.clone()).collect();
    let mut upstreams: Vec<Upstream> = Vec::new();
    for id in upstream_ids {
        if !upstreams.iter().any(|u| u.id == id) {
            upstreams.push(upstream(&id));
        }
    }
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
    service
        .ledger()
        .unwrap()
        .set_account(
            7,
            BudgetPolicy {
                unit: BillingUnit::Cny,
                limit: limit.map(|l| l.parse().unwrap()),
                enforcement: BudgetEnforcement::Soft,
                max_in_flight: 8,
                max_pending: 16,
                allowed_models: vec![],
                allowed_upstreams: vec![],
            },
        )
        .unwrap();
    let plan = service.plan_for("opus5").unwrap();
    Fixture {
        service,
        plan,
        config_path,
        ledger_path,
    }
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

fn coordinator<'a>(f: &'a Fixture, request: &str) -> Coordinator<'a> {
    Coordinator::new(
        f.service.ledger().unwrap(),
        f.service.routing(),
        &f.plan,
        7,
        request,
    )
}

/// 预留必须先于发送：`begin_attempt` 返回时账本上已经有这笔在飞记录。
/// 先发后记账意味着一次崩溃就丢掉一笔费用。
#[test]
fn a_reservation_exists_before_the_request_is_sent() {
    let f = fixture(vec![binding("a", "u1", 0)], Some("100"));
    let mut c = coordinator(&f, "r1");
    let attempt = c.begin_attempt(&ctx()).unwrap().expect("应有可用路线");

    let account = &f.service.ledger().unwrap().accounts(7).unwrap()[0];
    assert_eq!(account.in_flight, 1, "发送之前账本上就该有在飞记录");
    assert_eq!(attempt.binding_id, "a");
    assert_eq!(attempt.upstream_model, "m-a");
}

/// 已提交之后**绝不换供应商**：换供应商等于把两个模型的输出拼成一个响应。
#[test]
fn after_commitment_no_further_attempt_is_allowed() {
    let f = fixture(
        vec![binding("a", "u1", 0), binding("b", "u2", 1)],
        Some("100"),
    );
    let mut c = coordinator(&f, "r1");
    let attempt = c.begin_attempt(&ctx()).unwrap().unwrap();

    let next = c
        .finish_attempt(
            &ctx(),
            &attempt,
            AttemptResult::InterruptedAfterCommitment {
                cost: None,
                usage: None,
            },
        )
        .unwrap();
    assert_eq!(next, Next::Stop(StopReason::InterruptedAfterCommitment));
    assert!(c.is_committed());
    // 即便还有一条备路，也不许再开一次尝试。
    //
    // 断言必须钉住**协调器自己**那道守卫的措辞：只断言 is_err 的话，删掉协调器守卫
    // 后账本的 "request already committed" 会接住，测试照过——变异测试实测暴露过
    // 这一点。两道守卫都要，但每道都得被它自己的测试证明。
    let Err(error) = c.begin_attempt(&ctx()) else {
        panic!("已提交后再开尝试必须报错，而不是悄悄换供应商");
    };
    assert!(
        format!("{error:#}").contains("coordinator refuses another attempt"),
        "必须是协调器在碰账本之前就拒绝，实得：{error:#}"
    );
}

/// 已提交后的中断**不得 release**：下游已经收到内容，这笔消耗真实发生过。
#[test]
fn an_interrupted_committed_attempt_keeps_its_obligation() {
    let f = fixture(vec![binding("a", "u1", 0)], Some("100"));
    let mut c = coordinator(&f, "r1");
    let attempt = c.begin_attempt(&ctx()).unwrap().unwrap();
    c.finish_attempt(
        &ctx(),
        &attempt,
        AttemptResult::InterruptedAfterCommitment {
            cost: None,
            usage: None,
        },
    )
    .unwrap();

    let account = &f.service.ledger().unwrap().accounts(7).unwrap()[0];
    assert_eq!(account.in_flight, 0);
    assert!(
        account.customer_pending >= 1,
        "已提交的中断应留下待结算义务，而不是凭空消失"
    );
}

/// 提交前的失败可以换路重试，且失败过的那条路不再被选中。
#[test]
fn a_failure_before_commitment_releases_and_moves_to_another_route() {
    let f = fixture(
        vec![binding("a", "u1", 0), binding("b", "u2", 0)],
        Some("100"),
    );
    let mut c = coordinator(&f, "r1");
    let first = c.begin_attempt(&ctx()).unwrap().unwrap();

    let next = c
        .finish_attempt(
            &ctx(),
            &first,
            AttemptResult::FailedBeforeCommitment { retryable: true },
        )
        .unwrap();
    assert_eq!(next, Next::Retry);

    // 释放掉了，不占用额度。
    let account = &f.service.ledger().unwrap().accounts(7).unwrap()[0];
    assert_eq!(account.in_flight, 0);
    assert_eq!(account.customer_pending, 0, "确认未提交的失败应释放");

    let second = c.begin_attempt(&ctx()).unwrap().unwrap();
    assert_ne!(
        second.binding_id, first.binding_id,
        "不得重复选中失败过的路"
    );
}

/// 不可重试的失败立刻结束——请求本身有问题，换路也不会成功。
#[test]
fn a_non_retryable_failure_stops_immediately() {
    let f = fixture(
        vec![binding("a", "u1", 0), binding("b", "u2", 0)],
        Some("100"),
    );
    let mut c = coordinator(&f, "r1");
    let attempt = c.begin_attempt(&ctx()).unwrap().unwrap();
    let next = c
        .finish_attempt(
            &ctx(),
            &attempt,
            AttemptResult::FailedBeforeCommitment { retryable: false },
        )
        .unwrap();
    assert_eq!(next, Next::Stop(StopReason::NotRetryable));
}

/// 尝试次数有上限，跨全部路线共享。
#[test]
fn attempts_are_bounded_across_all_routes() {
    let f = fixture(
        vec![
            binding("a", "u1", 0),
            binding("b", "u2", 0),
            binding("c", "u3", 0),
            binding("d", "u4", 0),
        ],
        Some("100"),
    );
    let mut c = coordinator(&f, "r1");
    let mut last = Next::Retry;
    while last == Next::Retry {
        let attempt = c.begin_attempt(&ctx()).unwrap().unwrap();
        last = c
            .finish_attempt(
                &ctx(),
                &attempt,
                AttemptResult::FailedBeforeCommitment { retryable: true },
            )
            .unwrap();
    }
    assert_eq!(last, Next::Stop(StopReason::AttemptsExhausted));
    assert_eq!(c.attempts_used(), f.plan.max_attempts, "上限是 3");
}

/// 期限到了就停，即便次数还没用完。
#[test]
fn the_deadline_stops_retrying_even_with_attempts_left() {
    let f = fixture(
        vec![binding("a", "u1", 0), binding("b", "u2", 0)],
        Some("100"),
    );
    let mut c = coordinator(&f, "r1");
    let attempt = c.begin_attempt(&ctx()).unwrap().unwrap();
    c.age_by(f.plan.deadline + Duration::from_secs(1));
    let next = c
        .finish_attempt(
            &ctx(),
            &attempt,
            AttemptResult::FailedBeforeCommitment { retryable: true },
        )
        .unwrap();
    assert_eq!(next, Next::Stop(StopReason::DeadlineReached));
}

/// 成功才发布粘性绑定；失败绝不发布，否则会把坏上游钉死在会话上。
#[test]
fn only_success_publishes_affinity() {
    let f = fixture(
        vec![binding("a", "u1", 0), binding("b", "u2", 0)],
        Some("100"),
    );
    let mut c = coordinator(&f, "r1");
    let failed = c.begin_attempt(&ctx()).unwrap().unwrap();
    c.finish_attempt(
        &ctx(),
        &failed,
        AttemptResult::FailedBeforeCommitment { retryable: true },
    )
    .unwrap();
    assert_eq!(
        f.service
            .routing()
            .preview(&ctx(), &f.plan.candidates, RoutingMode::Sticky)
            .sticky,
        crate::gateway::routing::StickyOutcome::NoBinding,
        "失败不得发布绑定"
    );

    let ok = c.begin_attempt(&ctx()).unwrap().unwrap();
    c.finish_attempt(
        &ctx(),
        &ok,
        AttemptResult::Succeeded {
            cost: Some("1".parse().unwrap()),
            usage: None,
        },
    )
    .unwrap();
    assert_eq!(
        f.service
            .routing()
            .preview(&ctx(), &f.plan.candidates, RoutingMode::Sticky)
            .sticky,
        crate::gateway::routing::StickyOutcome::Hit,
        "成功才发布"
    );
}

/// 代际在请求期间失效时不发布绑定——它代表的语义已经不是当前配置了。
#[test]
fn a_stale_generation_does_not_publish_affinity() {
    let f = fixture(vec![binding("a", "u1", 0)], Some("100"));
    let mut c = coordinator(&f, "r1");
    let attempt = c.begin_attempt(&ctx()).unwrap().unwrap();
    f.service.routing().invalidate_bindings();
    c.finish_attempt(
        &ctx(),
        &attempt,
        AttemptResult::Succeeded {
            cost: Some("1".parse().unwrap()),
            usage: None,
        },
    )
    .unwrap();
    assert_eq!(
        f.service
            .routing()
            .preview(&ctx(), &f.plan.candidates, RoutingMode::Sticky)
            .sticky,
        crate::gateway::routing::StickyOutcome::NoBinding
    );
}

/// 被丢弃的尝试不得让费用凭空消失：它保持在飞，由启动期恢复转成待结算。
#[test]
fn an_abandoned_attempt_survives_as_a_durable_obligation() {
    let f = fixture(vec![binding("a", "u1", 0)], Some("100"));
    {
        let mut c = coordinator(&f, "r1");
        let _attempt = c.begin_attempt(&ctx()).unwrap().unwrap();
        // 协调器在这里被丢弃，从不结算——模拟进程崩溃或请求取消。
    }
    let ledger = f.service.ledger().unwrap();
    assert_eq!(ledger.accounts(7).unwrap()[0].in_flight, 1);

    // 启动期恢复把未决的在飞转成待结算，而不是当作没发生。
    assert_eq!(ledger.recover_inflight().unwrap(), 1);
    let account = &ledger.accounts(7).unwrap()[0];
    assert_eq!(account.in_flight, 0);
    assert!(account.customer_pending >= 1, "费用不得凭空消失");
}

/// 没有确认金额时保持 pending，绝不用 0 顶替。
#[test]
fn a_success_without_confirmed_cost_stays_pending() {
    let f = fixture(vec![binding("a", "u1", 0)], Some("100"));
    let mut c = coordinator(&f, "r1");
    let attempt = c.begin_attempt(&ctx()).unwrap().unwrap();
    c.finish_attempt(
        &ctx(),
        &attempt,
        AttemptResult::Succeeded {
            cost: None,
            usage: None,
        },
    )
    .unwrap();
    let account = &f.service.ledger().unwrap().accounts(7).unwrap()[0];
    assert_eq!(account.used, Amount::ZERO, "没有确认金额不得计入已用");
    assert!(account.customer_pending >= 1, "应保持待结算而不是按 0 结清");
}

/// 全部候选都被准入拒绝时给出"无可用路线"，而不是硬选一条。
#[test]
fn no_eligible_route_yields_no_attempt() {
    let f = fixture(vec![binding("a", "u1", 0)], Some("0"));
    let mut c = coordinator(&f, "r1");
    assert!(
        c.begin_attempt(&ctx()).unwrap().is_none(),
        "额度为 0 时不应硬选一条路"
    );
}
