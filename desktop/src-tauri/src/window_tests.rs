use super::*;

/// 时序契约：窗口的初始 URL 必须是打包进应用的等待页，而不是一个还没人
/// 监听的 http 地址——那会让用户看到浏览器的「无法连接」页面，
/// 而真相只是「再等两秒」。
#[test]
fn the_window_starts_on_the_bundled_waiting_page() {
    let conf: serde_json::Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json 要是合法 JSON");
    let url = conf["app"]["windows"][0]["url"]
        .as_str()
        .expect("窗口要有初始 URL");
    assert_eq!(url, "waiting.html");
    assert!(
        !url.starts_with("http"),
        "初始 URL 不能指向尚未监听的服务"
    );
}

#[test]
fn the_target_url_points_at_the_admin_ui() {
    assert_eq!(
        admin_url("127.0.0.1:8080".parse().unwrap()),
        "http://127.0.0.1:8080/admin"
    );
}

/// 端口回退后地址会变，所以目标 URL 必须由**实际**地址算出来，
/// 不能把配置里的端口写死。
#[test]
fn the_target_url_follows_the_actual_port() {
    assert_eq!(
        admin_url("127.0.0.1:49152".parse().unwrap()),
        "http://127.0.0.1:49152/admin"
    );
}

#[test]
fn the_bundle_targets_cover_both_platforms() {
    let conf: serde_json::Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
    let targets: Vec<&str> = conf["bundle"]["targets"]
        .as_array()
        .expect("bundle.targets 要是数组")
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert!(targets.contains(&"app"), "SC-10 要 .app");
    assert!(targets.contains(&"dmg"), "SC-10 要 .dmg");
    assert!(targets.contains(&"nsis"), "SC-11 要 Windows 安装包");
}
