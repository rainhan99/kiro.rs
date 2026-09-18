use super::*;

fn candidate(id: &str, tier: u32, weight: u32) -> Candidate {
    Candidate {
        binding_id: id.into(),
        upstream_id: format!("up-{id}"),
        priority_tier: tier,
        weight,
        enabled: true,
        supports_tools: true,
        supports_images: true,
        supports_reasoning: true,
    }
}

fn ctx(session: Option<&str>) -> RouteContext {
    RouteContext {
        key_id: 42,
        public_model: "gpt-fast".into(),
        session_id: session.map(str::to_string),
        needs_tools: false,
        needs_images: false,
        needs_reasoning: false,
    }
}

const SESSION: &str = "session-0123456789";

fn engine() -> RoutingEngine {
    RoutingEngine::new(Duration::from_secs(3600))
}

fn pick(engine: &RoutingEngine, c: &[Candidate], ticket: u64) -> String {
    engine
        .select(&ctx(Some(SESSION)), c, RoutingMode::WeightedRandom, Some(ticket))
        .expect("应有候选")
        .binding_id
}

/// 权重 2 / 8 时，票号 0..1 落到第一个，2..9 落到第二个。
#[test]
fn weighted_random_splits_tickets_by_weight() {
    let engine = engine();
    let c = vec![candidate("a", 0, 2), candidate("b", 0, 8)];
    for ticket in 0..2 {
        assert_eq!(pick(&engine, &c, ticket), "a", "票号 {ticket} 应落在 a");
    }
    for ticket in 2..10 {
        assert_eq!(pick(&engine, &c, ticket), "b", "票号 {ticket} 应落在 b");
    }
    // 票号超出总权重时取模，保持同一分布而不是越界失败。
    assert_eq!(pick(&engine, &c, 10), "a");
    assert_eq!(pick(&engine, &c, 12), "b");
}

/// 候选顺序变化不得改变同一票号的结果，否则"可复现"无从谈起。
#[test]
fn weighted_selection_is_order_independent() {
    let engine = engine();
    let forward = vec![candidate("a", 0, 2), candidate("b", 0, 8)];
    let reversed = vec![candidate("b", 0, 8), candidate("a", 0, 2)];
    for ticket in 0..10 {
        assert_eq!(
            pick(&engine, &forward, ticket),
            pick(&engine, &reversed, ticket),
            "票号 {ticket} 在两种候选顺序下应一致"
        );
    }
}

/// 权重变化**不得**踢掉有效绑定——调权重不意味着正在进行的会话选错了。
#[test]
fn a_valid_binding_survives_weight_changes() {
    let engine = engine();
    let before = vec![candidate("a", 0, 2), candidate("b", 0, 8)];
    let context = ctx(Some(SESSION));
    assert!(engine.bind_success(&context, "a", engine.generation()));

    let mut after = before.clone();
    after[0].weight = 1;
    after[1].weight = 99;
    let selection = engine
        .select(&context, &after, RoutingMode::WeightedRandom, Some(9))
        .unwrap();
    assert_eq!(selection.binding_id, "a", "权重变了但绑定仍应命中");
    assert_eq!(selection.evidence.sticky, StickyOutcome::Hit);
}

/// 禁用、权重归零、被调用方排除（候选集里没有）都必须立刻失效。
#[test]
fn a_binding_dies_on_disable_zero_weight_or_exclusion() {
    let context = ctx(Some(SESSION));

    for (name, mutate) in [
        ("禁用", Box::new(|c: &mut Vec<Candidate>| c[0].enabled = false) as Box<dyn Fn(&mut Vec<Candidate>)>),
        ("权重归零", Box::new(|c: &mut Vec<Candidate>| c[0].weight = 0)),
        ("被调用方排除", Box::new(|c: &mut Vec<Candidate>| { c.remove(0); })),
    ] {
        let engine = engine();
        let mut c = vec![candidate("a", 0, 5), candidate("b", 0, 5)];
        assert!(engine.bind_success(&context, "a", engine.generation()));
        mutate(&mut c);
        let selection = engine
            .select(&context, &c, RoutingMode::WeightedRandom, Some(0))
            .unwrap();
        assert_eq!(
            selection.evidence.sticky,
            StickyOutcome::BindingIneligible,
            "{name}后绑定必须失效"
        );
        assert_eq!(selection.binding_id, "b", "{name}后应改选其它候选");
    }
}

/// 模式代际变更作废旧绑定：路由语义本身变了，旧绑定不再代表当前配置。
#[test]
fn a_binding_dies_on_generation_change() {
    let engine = engine();
    let c = vec![candidate("a", 0, 5), candidate("b", 0, 5)];
    let context = ctx(Some(SESSION));
    assert!(engine.bind_success(&context, "a", engine.generation()));

    engine.invalidate_bindings();
    let selection = engine
        .select(&context, &c, RoutingMode::WeightedRandom, Some(9))
        .unwrap();
    assert_eq!(selection.evidence.sticky, StickyOutcome::GenerationChanged);
}

/// 过期绑定失效，且报的是过期而不是笼统的"没命中"。
#[test]
fn an_expired_binding_reports_expiry() {
    let engine = RoutingEngine::new(Duration::from_millis(1));
    let c = vec![candidate("a", 0, 5)];
    let context = ctx(Some(SESSION));
    assert!(engine.bind_success(&context, "a", engine.generation()));
    std::thread::sleep(Duration::from_millis(5));
    let evidence = engine.preview(&context, &c, RoutingMode::Sticky);
    assert_eq!(evidence.sticky, StickyOutcome::Expired);
}

/// 无有效绑定时，高优先级组永远压过低优先级组。
#[test]
fn a_higher_priority_group_always_beats_a_lower_one() {
    let engine = engine();
    // 低优先级组权重给到极高，也不能翻盘。
    let c = vec![candidate("high", 0, 1), candidate("low", 5, 1000)];
    for ticket in 0..20 {
        assert_eq!(pick(&engine, &c, ticket), "high", "票号 {ticket}");
    }
}

/// 没有可信会话标识时给出明确证据，**不得**回退成按 Key 粘。
#[test]
fn a_missing_session_id_is_explicit_and_never_key_wide() {
    let engine = engine();
    let c = vec![candidate("a", 0, 5)];

    for session in [None, Some(""), Some("short"), Some(&"x".repeat(300)[..])] {
        let context = RouteContext {
            session_id: session.map(str::to_string),
            ..ctx(None)
        };
        let evidence = engine.preview(&context, &c, RoutingMode::Sticky);
        assert_eq!(
            evidence.sticky,
            StickyOutcome::NoSessionId,
            "session={session:?} 应判为无可信会话标识"
        );
        // 无会话标识时也不允许写入绑定，否则就变成了按 Key 粘。
        assert!(!engine.bind_success(&context, "a", engine.generation()));
    }
}

/// 失败的尝试不得发布绑定；代际不符的成功也不得写入。
#[test]
fn only_a_current_generation_success_publishes_a_binding() {
    let engine = engine();
    let c = vec![candidate("a", 0, 5)];
    let context = ctx(Some(SESSION));

    let stale = engine.generation();
    engine.invalidate_bindings();
    assert!(
        !engine.bind_success(&context, "a", stale),
        "过期代际的成功不得写入"
    );
    assert_eq!(
        engine.preview(&context, &c, RoutingMode::Sticky).sticky,
        StickyOutcome::NoBinding
    );

    assert!(engine.bind_success(&context, "a", engine.generation()));
    assert_eq!(
        engine.preview(&context, &c, RoutingMode::Sticky).sticky,
        StickyOutcome::Hit
    );
}

/// 路由键不得包含对话内容或鉴权密钥；不同 Key、不同模型、不同会话互不串。
#[test]
fn route_keys_are_scoped_and_carry_no_secret() {
    let engine = engine();
    let c = vec![candidate("a", 0, 5), candidate("b", 0, 5)];
    let base = ctx(Some(SESSION));
    assert!(engine.bind_success(&base, "a", engine.generation()));

    let other_key = RouteContext { key_id: 7, ..base.clone() };
    let other_model = RouteContext { public_model: "other".into(), ..base.clone() };
    let other_session = RouteContext {
        session_id: Some("session-9876543210".into()),
        ..base.clone()
    };
    for (name, context) in [
        ("其它 Key", other_key),
        ("其它模型", other_model),
        ("其它会话", other_session),
    ] {
        assert_eq!(
            engine.preview(&context, &c, RoutingMode::Sticky).sticky,
            StickyOutcome::NoBinding,
            "{name}不应命中别人的绑定"
        );
    }

    // 键由 (key_id, 模型别名, 会话) 构成；key_id 是数字标识而非密钥。
    let key = RoutingEngine::route_key(&base, SESSION);
    assert!(key.contains("42") && key.contains("gpt-fast") && key.contains(SESSION));
}

/// 预览是只读的：不写绑定、不续期、不改代际。
#[test]
fn preview_has_no_side_effects() {
    let engine = engine();
    let c = vec![candidate("a", 0, 2), candidate("b", 0, 8)];
    let context = ctx(Some(SESSION));

    let generation = engine.generation();
    for _ in 0..5 {
        let evidence = engine.preview(&context, &c, RoutingMode::WeightedRandom);
        assert_eq!(evidence.sticky, StickyOutcome::NoBinding, "预览不得写入绑定");
    }
    assert_eq!(engine.generation(), generation, "预览不得改变代际");
    // 预览过后仍然没有绑定，说明它确实没有副作用。
    assert_eq!(
        engine.preview(&context, &c, RoutingMode::Sticky).sticky,
        StickyOutcome::NoBinding
    );
}

/// 预览要给出过滤原因、权重、所选组与条件概率。
#[test]
fn preview_reports_filters_group_and_conditional_probability() {
    let engine = engine();
    let mut c = vec![
        candidate("a", 0, 2),
        candidate("b", 0, 8),
        candidate("off", 0, 5),
        candidate("low", 9, 100),
    ];
    c[2].enabled = false;

    let evidence = engine.preview(&ctx(Some(SESSION)), &c, RoutingMode::WeightedRandom);
    assert_eq!(evidence.group, Some(0), "应选中最高优先级组");
    let by_id = |id: &str| {
        evidence
            .candidates
            .iter()
            .find(|e| e.binding_id == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by_id("off").filtered, Some(FilterReason::Disabled));
    assert_eq!(by_id("off").probability, 0.0);
    assert_eq!(by_id("a").probability, 0.2);
    assert_eq!(by_id("b").probability, 0.8);
    assert_eq!(by_id("low").probability, 0.0, "不在所选组内，概率为 0");
    assert_eq!(by_id("low").filtered, None, "但它本身并没有被过滤掉");
    // 同组概率之和为 1。
    let total: f64 = evidence
        .candidates
        .iter()
        .filter(|e| e.priority_tier == 0)
        .map(|e| e.probability)
        .sum();
    assert!((total - 1.0).abs() < 1e-9, "同组概率和应为 1，实测 {total}");
}

/// 能力不满足的候选按各自原因排除，而不是笼统地"不可用"。
#[test]
fn capability_filters_report_their_own_reason() {
    let engine = engine();
    let mut c = vec![candidate("a", 0, 5)];
    c[0].supports_tools = false;
    c[0].supports_images = false;
    c[0].supports_reasoning = false;

    for (need, expected) in [
        (("tools", true, false, false), FilterReason::ToolsUnsupported),
        (("images", false, true, false), FilterReason::ImagesUnsupported),
        (
            ("reasoning", false, false, true),
            FilterReason::ReasoningUnsupported,
        ),
    ] {
        let context = RouteContext {
            needs_tools: need.1,
            needs_images: need.2,
            needs_reasoning: need.3,
            ..ctx(Some(SESSION))
        };
        let evidence = engine.preview(&context, &c, RoutingMode::Sticky);
        assert_eq!(
            evidence.candidates[0].filtered,
            Some(expected),
            "{} 能力缺失时的原因不对",
            need.0
        );
        assert_eq!(evidence.group, None, "无可用候选时不应给出所选组");
    }
}

/// 粘性模式下首次选择取有效权重最高者，同分按 binding_id 稳定决胜。
#[test]
fn sticky_mode_picks_the_highest_weight_with_a_stable_tie_break() {
    let engine = engine();
    let c = vec![candidate("b", 0, 5), candidate("a", 0, 5), candidate("c", 0, 3)];
    let context = ctx(Some(SESSION));
    let first = engine
        .select(&context, &c, RoutingMode::Sticky, None)
        .unwrap()
        .binding_id;
    assert_eq!(first, "a", "同权重时按 binding_id 稳定取最小者");
    // 多次调用结果一致（粘性模式的首选不是抽样）。
    for _ in 0..5 {
        assert_eq!(
            engine
                .select(&context, &c, RoutingMode::Sticky, None)
                .unwrap()
                .binding_id,
            "a"
        );
    }
}

/// 没有任何可用候选时返回 None，而不是硬选一个不合格的。
#[test]
fn no_eligible_candidate_yields_no_selection() {
    let engine = engine();
    let mut c = vec![candidate("a", 0, 5)];
    c[0].enabled = false;
    assert!(
        engine
            .select(&ctx(Some(SESSION)), &c, RoutingMode::WeightedRandom, Some(0))
            .is_none()
    );
    assert!(engine.select(&ctx(Some(SESSION)), &[], RoutingMode::Sticky, None).is_none());
}

/// 标记不可用应清掉所有指向它的绑定。
#[test]
fn marking_unavailable_clears_bindings_pointing_at_it() {
    let engine = engine();
    let c = vec![candidate("a", 0, 5), candidate("b", 0, 5)];
    let one = ctx(Some(SESSION));
    let two = RouteContext {
        session_id: Some("session-9876543210".into()),
        ..one.clone()
    };
    assert!(engine.bind_success(&one, "a", engine.generation()));
    assert!(engine.bind_success(&two, "a", engine.generation()));

    assert_eq!(engine.mark_unavailable("a"), 2);
    for context in [&one, &two] {
        assert_eq!(
            engine.preview(context, &c, RoutingMode::Sticky).sticky,
            StickyOutcome::NoBinding
        );
    }
}
