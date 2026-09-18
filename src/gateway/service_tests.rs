use super::*;
use crate::gateway::import::LegacyKeyBalance;
use crate::gateway::ledger_types::{BoundKind, PriceSnapshot, ReservationInput};
use crate::gateway::{BillingUnit, Upstream, UpstreamKind};
use std::path::PathBuf;

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("kiro-gw-{name}-{}", uuid::Uuid::new_v4()))
}

fn upstream(id: &str, enabled: bool) -> Upstream {
    Upstream {
        id: id.into(),
        name: id.into(),
        kind: UpstreamKind::Kiro,
        enabled,
        weight: 10,
        base_url: None,
        api_key: None,
        has_api_key: false,
        allow_private_network: false,
        kiro_group: None,
        cache_usage_policy: None,
    }
}

/// 每个绑定用不同的 upstream_model：Task 1 的校验禁止同一 (上游, 模型, 层级)
/// 出现两次——两条完全相同的路没有意义。
fn binding(id: &str, upstream_id: &str, enabled: bool) -> ModelBinding {
    ModelBinding {
        id: id.into(),
        upstream_id: upstream_id.into(),
        upstream_model: format!("real-model-{id}"),
        enabled,
        priority_tier: 0,
        weight: 10,
        context_window: 200_000,
        max_output_tokens: 8_000,
        supports_tools: true,
        supports_images: false,
        supports_reasoning: false,
        allow_model_substitution: false,
        billing_unit: BillingUnit::KiroCredit,
        cost_prices: None,
        sell_prices: None,
    }
}

fn configured(models: Vec<PublicModel>, upstreams: Vec<Upstream>) -> GatewayConfig {
    GatewayConfig {
        models,
        upstreams,
        ..GatewayConfig::default()
    }
}

fn model(id: &str, bindings: Vec<ModelBinding>) -> PublicModel {
    PublicModel {
        id: id.into(),
        display_name: None,
        routing_mode: None,
        affinity_ttl_secs: None,
        bindings,
    }
}

fn service_with(config: Option<GatewayConfig>) -> (GatewayService, PathBuf, PathBuf) {
    let config_path = temp("config.json");
    let ledger_path = temp("billing.db");
    if let Some(config) = config {
        std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    }
    let service = GatewayService::open(&config_path, &ledger_path).unwrap();
    (service, config_path, ledger_path)
}

/// 最重要的一条：没有配置时网关完全惰性，任何别名都不接管，
/// 调用方据此原样走既有路径。一个"顺便改变了既有行为"的集成不可接受。
#[test]
fn an_unconfigured_gateway_manages_nothing() {
    let (service, config_path, ledger_path) = service_with(None);
    for alias in ["opus5", "claude-sonnet-4", "anything"] {
        assert!(!service.is_managed(alias));
        assert!(service.plan_for(alias).is_none());
    }
    assert!(service.public_models().is_empty());
    // 惰性网关不需要账本，也就不该因为账本打不开而拒绝启动。
    assert!(service.ledger().is_none());
    assert!(!ledger_path.exists(), "惰性状态下不该创建账本文件");
    let _ = std::fs::remove_file(config_path);
}

/// 配置里声明了东西，账本就必须能开——记不了账就不能做收费的事。
#[test]
fn a_configured_gateway_requires_a_usable_ledger() {
    let config_path = temp("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&configured(
            vec![model("opus5", vec![binding("b1", "u1", true)])],
            vec![upstream("u1", true)],
        ))
        .unwrap(),
    )
    .unwrap();

    // 把账本路径指向一个目录，保证打不开。
    let bad_ledger = temp("ledger-dir");
    std::fs::create_dir_all(&bad_ledger).unwrap();
    let Err(error) = GatewayService::open(&config_path, &bad_ledger) else {
        panic!("账本不可用时必须拒绝启动");
    };
    assert!(format!("{error:#}").contains("记不了账就不能收费"));

    // 正常路径下账本存在。
    let good = temp("billing.db");
    let service = GatewayService::open(&config_path, &good).unwrap();
    assert!(service.ledger().is_some());
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(good);
    let _ = std::fs::remove_dir_all(bad_ledger);
}

/// 定义了但全部停用的模型**不算接管**：把请求从既有路径劫走再让它失败，
/// 比回到原来能用的路更糟。
#[test]
fn a_model_with_no_usable_binding_is_not_managed() {
    for (name, config) in [
        (
            "绑定全停用",
            configured(
                vec![model("opus5", vec![binding("b1", "u1", false)])],
                vec![upstream("u1", true)],
            ),
        ),
        (
            "上游停用",
            configured(
                vec![model("opus5", vec![binding("b1", "u1", true)])],
                vec![upstream("u1", false)],
            ),
        ),
        // 「绑定指向不存在的上游」不在此列：Task 1 的校验已经保证引用完整性，
        // 这样的配置根本加载不进来。service 里那条检查是防御性的，不假装它可测。
        (
            "没有任何绑定",
            configured(vec![model("opus5", vec![])], vec![upstream("u1", true)]),
        ),
    ] {
        let (service, config_path, ledger_path) = service_with(Some(config));
        assert!(!service.is_managed("opus5"), "{name}：不应接管");
        assert!(service.plan_for("opus5").is_none(), "{name}");
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_file(ledger_path);
    }
}

/// 接管的别名要给出冻结好的计划：模式、代际、期限、候选都在里面。
#[test]
fn a_managed_alias_produces_a_frozen_plan() {
    let (service, config_path, ledger_path) = service_with(Some(configured(
        vec![model(
            "opus5",
            vec![binding("b1", "u1", true), binding("b2", "u2", true)],
        )],
        vec![upstream("u1", true), upstream("u2", false)],
    )));

    assert!(service.is_managed("opus5"));
    let plan = service.plan_for("opus5").unwrap();
    assert_eq!(plan.alias, "opus5");
    assert_eq!(plan.mode, RoutingMode::Sticky, "未覆盖时用全局默认");
    assert_eq!(plan.generation, service.routing().generation());
    assert_eq!(plan.deadline, Duration::from_secs(120));
    assert_eq!(plan.max_attempts, 3);
    assert_eq!(plan.candidates.len(), 2);

    // 上游停用时，挂在它下面的绑定也不可用——否则会选中一个明确被关掉的去处。
    let b2 = plan
        .candidates
        .iter()
        .find(|c| c.binding_id == "b2")
        .unwrap();
    assert!(!b2.enabled, "上游停用应连带让绑定不可用");
    let b1 = plan
        .candidates
        .iter()
        .find(|c| c.binding_id == "b1")
        .unwrap();
    assert!(b1.enabled);

    assert_eq!(plan.binding("b1").unwrap().upstream_model, "real-model-b1");
    assert!(plan.upstream("u1").is_some());
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 模型级路由模式覆盖全局默认。
#[test]
fn a_model_level_mode_overrides_the_default() {
    let mut config = configured(
        vec![model("opus5", vec![binding("b1", "u1", true)])],
        vec![upstream("u1", true)],
    );
    config.models[0].routing_mode = Some(RoutingMode::WeightedRandom);
    let (service, config_path, ledger_path) = service_with(Some(config));
    assert_eq!(
        service.plan_for("opus5").unwrap().mode,
        RoutingMode::WeightedRandom
    );
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 快照在请求开始时冻结：运行中改配置不影响已在飞的请求，下一个请求才看到新配置。
/// 否则一次请求可能按 A 计价、按 B 路由。
#[test]
fn a_plan_is_frozen_while_a_later_request_sees_the_new_config() {
    let (service, config_path, ledger_path) = service_with(Some(configured(
        vec![model("opus5", vec![binding("b1", "u1", true)])],
        vec![upstream("u1", true)],
    )));
    let in_flight = service.plan_for("opus5").unwrap();
    assert_eq!(in_flight.candidates.len(), 1);

    let mut updated = configured(
        vec![model(
            "opus5",
            vec![binding("b1", "u1", true), binding("b2", "u1", true)],
        )],
        vec![upstream("u1", true)],
    );
    updated.request_timeout_secs = 30;
    service.update_config(in_flight.revision, updated).unwrap();

    assert_eq!(in_flight.candidates.len(), 1, "在飞的计划不得被改到脚下");
    assert_eq!(in_flight.deadline, Duration::from_secs(120));
    let fresh = service.plan_for("opus5").unwrap();
    assert_eq!(fresh.candidates.len(), 2, "新请求应看到新配置");
    assert_eq!(fresh.deadline, Duration::from_secs(30));
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 只有让绑定含义变化的更新才作废粘性绑定；调权重不作废。
#[test]
fn only_meaning_changing_updates_bump_the_routing_generation() {
    let (service, config_path, ledger_path) = service_with(Some(configured(
        vec![model("opus5", vec![binding("b1", "u1", true)])],
        vec![upstream("u1", true)],
    )));
    let before = service.routing().generation();

    // 调权重：不作废。
    let mut heavier = configured(
        vec![model("opus5", vec![binding("b1", "u1", true)])],
        vec![upstream("u1", true)],
    );
    heavier.upstreams[0].weight = 500;
    let revision = service.snapshot().revision;
    service.update_config(revision, heavier).unwrap();
    assert_eq!(
        service.routing().generation(),
        before,
        "调权重不该踢掉在途会话"
    );

    // 改路由模式：作废。
    let mut switched = configured(
        vec![model("opus5", vec![binding("b1", "u1", true)])],
        vec![upstream("u1", true)],
    );
    switched.default_routing_mode = RoutingMode::WeightedRandom;
    let revision = service.snapshot().revision;
    service.update_config(revision, switched).unwrap();
    assert!(
        service.routing().generation() > before,
        "模式变化必须作废绑定"
    );
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 对外能力如实来自绑定：能力取各可用路的并集，窗口取最小值。
///
/// 能力取交集会把实际可用的能力报成不可用；窗口取最大值会让客户端把请求构造到
/// 别的路承受不了的大小。
#[test]
fn public_models_report_capabilities_honestly() {
    let mut with_tools = binding("b1", "u1", true);
    with_tools.supports_tools = true;
    with_tools.supports_images = false;
    with_tools.context_window = 200_000;
    with_tools.max_output_tokens = 8_000;

    let mut with_images = binding("b2", "u1", true);
    with_images.supports_tools = false;
    with_images.supports_images = true;
    with_images.context_window = 100_000;
    with_images.max_output_tokens = 4_000;

    let (service, config_path, ledger_path) = service_with(Some(configured(
        vec![
            model("opus5", vec![with_tools, with_images]),
            // 全部停用的模型不出现在对外列表里。
            model("hidden", vec![binding("b3", "u1", false)]),
        ],
        vec![upstream("u1", true)],
    )));

    let models = service.public_models();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "opus5");
    assert!(models[0].supports_tools, "并集：有一条路支持就算支持");
    assert!(models[0].supports_images);
    assert!(!models[0].supports_reasoning);
    assert_eq!(models[0].context_window, 100_000, "窗口取最小者");
    assert_eq!(models[0].max_output_tokens, 4_000);
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 惰性网关的启动收尾必须是**彻底的空操作**：不建账本文件、不报错、不声称做了什么。
/// 未配置网关的部署不因为引入这个特性而改变任何行为。
#[test]
fn a_lazy_gateway_adopts_nothing_at_startup() {
    let (service, config_path, ledger_path) = service_with(None);
    let adoption = service
        .adopt_legacy_state(&[LegacyKeyBalance {
            key_id: 1,
            used: 3.0,
            limit: Some(10.0),
        }])
        .unwrap();
    assert_eq!(adoption, StartupAdoption::default());
    assert!(!ledger_path.exists(), "惰性状态下不该创建账本");
    let _ = std::fs::remove_file(config_path);
}

/// 配置过的网关在启动时把遗留余额搬进账本，并认领上个进程遗留的在飞预留。
#[test]
fn a_configured_gateway_adopts_balances_and_orphaned_reservations() {
    let (service, config_path, ledger_path) = service_with(Some(configured(
        vec![model("opus5", vec![binding("b1", "u1", true)])],
        vec![upstream("u1", true)],
    )));

    let adoption = service
        .adopt_legacy_state(&[LegacyKeyBalance {
            key_id: 7,
            used: 3.5,
            limit: Some(10.0),
        }])
        .unwrap();
    assert_eq!(adoption.import.imported, vec![7]);
    assert!(adoption.import.sealed);
    assert_eq!(adoption.recovered_in_flight, 0, "没有遗留预留时不该虚报");

    let ledger = service.ledger().unwrap();
    let account = ledger
        .accounts(7)
        .unwrap()
        .into_iter()
        .find(|a| a.policy.unit == BillingUnit::KiroCredit)
        .expect("遗留 Key 应已立户");
    assert_eq!(account.available, Some("6.5".parse().unwrap()));

    // 模拟上个进程崩在"已预留、未结算"处：留下一条在飞记录。
    ledger
        .reserve(ReservationInput {
            request_id: "orphan".into(),
            attempt_id: "orphan:1".into(),
            key_id: 7,
            unit: BillingUnit::KiroCredit,
            public_model: "opus5".into(),
            upstream_id: "u1".into(),
            upper_bound: None,
            bound_kind: BoundKind::Unknown,
            snapshot: PriceSnapshot {
                config_revision: 1,
                price_revision: 1,
                binding_id: "b1".into(),
                upstream_kind: UpstreamKind::Kiro,
                upstream_model: "real-model-b1".into(),
                cost_prices: None,
                sell_prices: None,
            },
        })
        .unwrap();
    assert_eq!(ledger.accounts(7).unwrap()[0].in_flight, 1);

    // 再启动一次：认领那条孤儿预留，且封存后不再重复导入。
    let again = service.adopt_legacy_state(&[]).unwrap();
    assert_eq!(
        again.recovered_in_flight, 1,
        "上个进程遗留的预留必须被认领，而不是当作没发生过"
    );
    assert!(again.import.sealed);
    assert_eq!(again.import.imported, Vec::<u64>::new());

    // 认领 = 转成待结算，不是释放：这笔消耗可能真的发生过。
    let account = &ledger.accounts(7).unwrap()[0];
    assert_eq!(account.in_flight, 0);
    assert_eq!(account.customer_pending, 1, "不得被当作没发生过释放掉");

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}
