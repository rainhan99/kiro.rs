use super::*;
use crate::gateway::{RoutingMode, UpstreamKind};

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("kiro-gateway-{name}-{}.json", uuid::Uuid::new_v4()))
}

/// 带密钥的场景必须用非 Kiro 类型：Task 1 的校验规定 Kiro 上游走既有凭据池，
/// 既不带 apiKey 也不覆盖 baseUrl。夹具违反这条只能说明夹具错了，不是校验太严。
fn upstream(id: &str, key: Option<&str>) -> Upstream {
    Upstream {
        id: id.into(),
        name: format!("upstream {id}"),
        kind: UpstreamKind::Anthropic,
        enabled: true,
        weight: 10,
        base_url: None,
        api_key: key.map(str::to_string),
        has_api_key: key.is_some(),
        allow_private_network: false,
        kiro_group: None,
        cache_usage_policy: None,
    }
}

fn config_with(upstreams: Vec<Upstream>) -> GatewayConfig {
    GatewayConfig {
        upstreams,
        ..GatewayConfig::default()
    }
}

/// 文件缺失时使用默认值。
#[test]
fn a_missing_file_yields_defaults() {
    let store = ConfigStore::open(temp_path("absent")).unwrap();
    let snapshot = store.snapshot();
    assert_eq!(snapshot.revision, 1);
    assert!(snapshot.config.upstreams.is_empty());
}

/// 文件存在但无法解析或校验不过 → 启动失败，**不得**退回空配置。
/// 一个"配置坏了所以当作没有上游"的网关会静悄悄拒绝全部流量，比不启动更难查。
#[test]
fn an_invalid_file_fails_startup_instead_of_falling_back() {
    let path = temp_path("broken");
    fs::write(&path, b"{ this is not json").unwrap();
    let Err(error) = ConfigStore::open(&path) else {
        panic!("无法解析的配置必须启动失败");
    };
    assert!(format!("{error:#}").contains("不会退回空配置"));

    let invalid = temp_path("invalid");
    // affinityTtlSecs 超出允许区间，能解析但过不了校验。
    fs::write(&invalid, br#"{"affinityTtlSecs": 1}"#).unwrap();
    let Err(error) = ConfigStore::open(&invalid) else {
        panic!("校验不过的配置必须启动失败");
    };
    assert!(format!("{error:#}").contains("不会带着无效配置启动"));

    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&invalid);
}

/// 乐观版本不符时拒绝，且运行期配置一字不动。
#[test]
fn a_stale_revision_is_rejected_without_touching_runtime() {
    let path = temp_path("conflict");
    let store = ConfigStore::open(&path).unwrap();
    store.update(1, config_with(vec![upstream("a", None)])).unwrap();
    assert_eq!(store.snapshot().revision, 2);

    let error = store
        .update(1, config_with(vec![upstream("b", None)]))
        .unwrap_err();
    assert!(format!("{error:#}").contains("configuration_conflict"));
    let snapshot = store.snapshot();
    assert_eq!(snapshot.revision, 2, "冲突不得推进版本");
    assert_eq!(snapshot.config.upstreams[0].id, "a", "冲突不得改动运行期配置");
    let _ = fs::remove_file(&path);
}

/// 更新时省略 apiKey 表示沿用原值；显式空字符串才是清除。
/// 否则任何一次从脱敏视图出发的保存都会把密钥抹掉。
#[test]
fn an_omitted_api_key_is_preserved_and_an_empty_one_clears_it() {
    let path = temp_path("secrets");
    let store = ConfigStore::open(&path).unwrap();
    store
        .update(1, config_with(vec![upstream("a", Some("secret-key"))]))
        .unwrap();

    // 省略：保持原值
    store
        .update(2, config_with(vec![upstream("a", None)]))
        .unwrap();
    assert_eq!(
        store.snapshot().config.upstreams[0].api_key.as_deref(),
        Some("secret-key")
    );
    assert!(store.snapshot().config.upstreams[0].has_api_key);

    // 显式空串：清除
    store
        .update(3, config_with(vec![upstream("a", Some(""))]))
        .unwrap();
    assert_eq!(store.snapshot().config.upstreams[0].api_key, None);
    assert!(!store.snapshot().config.upstreams[0].has_api_key);
    let _ = fs::remove_file(&path);
}

/// 对外视图不含密钥，只用 hasApiKey 表示是否已配置。
#[test]
fn the_redacted_view_never_carries_a_secret() {
    let path = temp_path("redacted");
    let store = ConfigStore::open(&path).unwrap();
    store
        .update(
            1,
            config_with(vec![upstream("a", Some("secret-key")), upstream("b", None)]),
        )
        .unwrap();

    let view = store.redacted().unwrap();
    let dumped = view.to_string();
    assert!(!dumped.contains("secret-key"), "脱敏视图泄露了密钥：{dumped}");
    assert_eq!(view["config"]["upstreams"][0]["hasApiKey"], true);
    assert_eq!(view["config"]["upstreams"][1]["hasApiKey"], false);
    assert!(view["config"]["upstreams"][0].get("apiKey").is_none());
    assert_eq!(view["revision"], 2);
    let _ = fs::remove_file(&path);
}

/// 调权重**不得**提升路由代际——那会把正在进行的会话全部踢走，
/// 而权重变化并不意味着旧选择是错的。
#[test]
fn a_weight_change_does_not_invalidate_routes() {
    let path = temp_path("weights");
    let store = ConfigStore::open(&path).unwrap();
    store.update(1, config_with(vec![upstream("a", None)])).unwrap();

    let mut heavier = config_with(vec![upstream("a", None)]);
    heavier.upstreams[0].weight = 9999;
    heavier.upstreams[0].name = "renamed".into();
    let outcome = store.update(2, heavier).unwrap();
    assert!(!outcome.invalidates_routes, "权重/名称变化不该作废绑定");
    let _ = fs::remove_file(&path);
}

/// 路由模式与上游身份（kind / baseUrl / 私网放行 / 是否带密钥）变化必须作废绑定：
/// 同一个绑定此时指向的其实已经是另一个东西。
#[test]
fn mode_and_upstream_identity_changes_invalidate_routes() {
    type Mutation = (&'static str, Box<dyn Fn(&mut GatewayConfig)>);
    let cases: Vec<Mutation> = vec![
        (
            "默认路由模式",
            Box::new(|c: &mut GatewayConfig| c.default_routing_mode = RoutingMode::WeightedRandom),
        ),
        (
            "上游 kind",
            Box::new(|c: &mut GatewayConfig| c.upstreams[0].kind = UpstreamKind::OpenaiChat),
        ),
        (
            "baseUrl",
            Box::new(|c: &mut GatewayConfig| {
                c.upstreams[0].base_url = Some("https://elsewhere.example".into())
            }),
        ),
        (
            "私网放行",
            Box::new(|c: &mut GatewayConfig| c.upstreams[0].allow_private_network = true),
        ),
    ];

    for (name, mutate) in cases {
        let path = temp_path("identity");
        let store = ConfigStore::open(&path).unwrap();
        store.update(1, config_with(vec![upstream("a", None)])).unwrap();
        let mut changed = config_with(vec![upstream("a", None)]);
        mutate(&mut changed);
        let outcome = store.update(2, changed).unwrap();
        assert!(outcome.invalidates_routes, "{name}变化必须作废绑定");
        let _ = fs::remove_file(&path);
    }
}

/// 落盘后重新打开，内容与版本语义都应成立；文件权限为 0600。
#[test]
fn a_saved_config_survives_reopen_with_owner_only_permissions() {
    let path = temp_path("roundtrip");
    let store = ConfigStore::open(&path).unwrap();
    store
        .update(1, config_with(vec![upstream("a", Some("secret-key"))]))
        .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "带密钥的配置必须是 0600，实测 {mode:o}");
    }

    let reopened = ConfigStore::open(&path).unwrap();
    let snapshot = reopened.snapshot();
    assert_eq!(snapshot.config.upstreams[0].id, "a");
    assert_eq!(
        snapshot.config.upstreams[0].api_key.as_deref(),
        Some("secret-key")
    );
    assert_eq!(snapshot.revision, 1, "重新打开后版本从 1 开始计数");
    let _ = fs::remove_file(&path);
}

/// 校验不过的更新不落盘、不推进版本、不改运行期。
#[test]
fn an_invalid_update_changes_nothing() {
    let path = temp_path("reject");
    let store = ConfigStore::open(&path).unwrap();
    store.update(1, config_with(vec![upstream("a", None)])).unwrap();

    let mut invalid = config_with(vec![upstream("a", None)]);
    invalid.affinity_ttl_secs = 1; // 超出允许区间
    assert!(store.update(2, invalid).is_err());

    let snapshot = store.snapshot();
    assert_eq!(snapshot.revision, 2);
    assert_eq!(snapshot.config.affinity_ttl_secs, 3_600);
    let on_disk: GatewayConfig = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(on_disk.affinity_ttl_secs, 3_600, "无效配置不得落盘");
    let _ = fs::remove_file(&path);
}

/// 快照是不可变共享值：持有旧快照的调用方不会被后续更新改到脚下。
#[test]
fn a_snapshot_is_immutable_once_taken() {
    let path = temp_path("snapshot");
    let store = ConfigStore::open(&path).unwrap();
    store.update(1, config_with(vec![upstream("a", None)])).unwrap();
    let held = store.snapshot();

    store.update(2, config_with(vec![upstream("b", None)])).unwrap();
    assert_eq!(held.config.upstreams[0].id, "a", "旧快照不应被后续更新改写");
    assert_eq!(held.revision, 2);
    assert_eq!(store.snapshot().config.upstreams[0].id, "b");
    let _ = fs::remove_file(&path);
}
