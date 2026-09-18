use super::*;

/// CLAUDE.md 点名的不变量：这个原生 credit 必须逐位往返。
#[test]
fn the_native_credit_fixture_survives_bit_for_bit() {
    let native = 0.016_954_370_829_187_4_f64;
    let amount = credits_to_amount(native).unwrap();
    assert_eq!(amount.to_string(), "0.0169543708291874");
    // 再转回 f64 仍是同一个数——最短往返表示的定义。
    assert_eq!(amount.to_string().parse::<f64>().unwrap(), native);
}

/// 遗留导入那条 12 位截断的规则用在这里会少收钱，两条规则必须分开。
#[test]
fn the_legacy_rule_would_lose_money_here() {
    let native = 0.016_954_370_829_187_4_f64;
    let truncated = crate::gateway::import::to_amount(native).unwrap();
    assert_eq!(truncated.to_string(), "0.016954370829");
    assert!(
        credits_to_amount(native).unwrap() > truncated,
        "按遗留规则会少记一笔"
    );
}

/// 上游确认为 0 是一个事实；它与"不知道"不同，后者根本到不了这一步。
#[test]
fn an_upstream_confirmed_zero_is_representable() {
    assert_eq!(credits_to_amount(0.0).unwrap(), Amount::ZERO);
}

/// 表示不了就报错，不退化成 0。
#[test]
fn an_unrepresentable_credit_is_refused() {
    assert!(credits_to_amount(f64::NAN).is_err());
    assert!(credits_to_amount(f64::INFINITY).is_err());
    assert!(credits_to_amount(-0.1).is_err());
    // 小到超过 18 位小数：Amount 收不下，报错而不是悄悄截断。
    assert!(credits_to_amount(1e-20).is_err());
}

/// 常见量级原样通过。
#[test]
fn ordinary_figures_pass_through_unchanged() {
    for (value, expected) in [(1.0, "1"), (2.5, "2.5"), (0.3, "0.3"), (12.75, "12.75")] {
        assert_eq!(credits_to_amount(value).unwrap().to_string(), expected);
    }
}

/// 覆盖必须**两者都换**：只换模型不换分组，请求会拿着这一路的模型去另一组凭据上发；
/// 只换分组不换模型，上游收到的是它不认识的公开别名。
#[test]
fn applying_a_route_replaces_both_the_model_and_the_group() {
    let route = KiroRoute {
        binding_id: "b1".into(),
        upstream_model: "real-upstream-model".into(),
        group: Some("team-a".into()),
        sticky: true,
        settlement: Arc::new(Settlement::new(
            Arc::new(
                GatewayService::open(
                    &std::env::temp_dir().join(format!("gw-{}.json", uuid::Uuid::new_v4())),
                    &std::env::temp_dir().join(format!("gw-{}.db", uuid::Uuid::new_v4())),
                )
                .unwrap(),
            ),
            "a:1".into(),
            BillingUnit::KiroCredit,
        )),
    };

    let mut model = "opus5".to_string();
    let mut group = Some("whatever-the-key-was-bound-to".to_string());
    route.apply(&mut model, &mut group);
    assert_eq!(model, "real-upstream-model");
    assert_eq!(group.as_deref(), Some("team-a"));

    // 不限分组的路要把 Key 原有的绑定也清掉，而不是留着上一次的值。
    let unbound = KiroRoute {
        group: None,
        ..route
    };
    let mut group = Some("stale".to_string());
    unbound.apply(&mut model, &mut group);
    assert_eq!(group, None, "不限分组就是不限，不该沿用上一次的值");
}
