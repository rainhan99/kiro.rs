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

/// 免登录注入脚本必须写对 localStorage 的键名。
///
/// 管理界面读的是 `adminApiKey`（admin-ui/src/lib/storage.ts:8）。写错一个
/// 字母的表现是「没报错，但还是要你登录」——最难查的那种。
#[test]
fn the_auto_login_script_writes_the_key_the_admin_ui_reads() {
    let script = auto_login_script("sk-admin-abc");
    assert!(
        script.contains("localStorage.setItem"),
        "实际: {script}"
    );
    assert!(
        script.contains("\"adminApiKey\""),
        "键名必须与 admin-ui 的 storage.ts 一致，实际: {script}"
    );
    assert!(script.contains("sk-admin-abc"));
}

/// 密钥来自配置文件，内容是任意字符串——用户可以把它改成带引号、
/// 反斜杠甚至换行的东西。直接拼进 JS 会造出语法错误，或者更糟：注入。
#[test]
fn the_auto_login_script_escapes_the_key() {
    let nasty = "a\"b\\c\nd</script>";
    let script = auto_login_script(nasty);

    assert!(
        !script.contains("a\"b"),
        "原样拼进去了，没有转义: {script}"
    );
    assert!(
        !script.contains('\n') || !script.lines().any(|l| l.contains("a\"b")),
        "换行没被转义: {script}"
    );
    // JSON 转义后应当是一个合法的 JS 字符串字面量
    assert!(script.contains("\\\""), "引号要被转义，实际: {script}");
}

/// 没配置管理密钥时不能注入 "null" 这种字符串——那会让管理界面拿着一个
/// 假密钥去请求，然后报 401，用户完全看不懂。
#[test]
fn no_script_is_produced_without_a_key() {
    assert!(auto_login_script_for(None).is_none());
    assert!(auto_login_script_for(Some("   ")).is_none(), "空白串等同没有");
    assert!(auto_login_script_for(Some("sk-admin-abc")).is_some());
}

/// `on_page_load` 对**等待页**也会触发。如果服务起得够快、在等待页那次
/// 回调之前就把脚本挂上了，脚本会被等待页吃掉——而等待页跑在 `tauri://`，
/// 与管理界面不同源，注进去毫无用处，且脚本已经被 take 走，真正该注的
/// 那一次反而没有了。表现是「偶尔要登录、偶尔不用」。
#[test]
fn only_the_admin_origin_gets_the_auto_login_script() {
    assert!(should_inject_login("http://127.0.0.1:8990/admin"));
    assert!(should_inject_login("http://127.0.0.1:49152/admin/"));
    assert!(should_inject_login("http://127.0.0.1:8990/admin#x"));

    assert!(!should_inject_login("tauri://localhost/waiting.html"));
    assert!(!should_inject_login("tauri://localhost"));
    assert!(!should_inject_login("about:blank"));
}

/// 只认回环。窗口理论上可以被导航到任何地方（将来加个外链、或者管理界面
/// 里有个跳转），把管理密钥注给外站是不可接受的。
#[test]
fn the_auto_login_script_never_leaves_the_loopback() {
    assert!(!should_inject_login("http://example.com/admin"));
    assert!(!should_inject_login("https://evil.test/admin"));
    assert!(!should_inject_login("http://10.0.0.5:8990/admin"));
}

/// 退出钩子必须调 shutdown，不能直接 exit。
///
/// 直接退会把 axum 任务连同在飞请求一起丢弃，网关账本上会留下永不结算的
/// 预留记录——那是钱的记录，不是日志。
#[test]
fn the_exit_handler_shuts_the_proxy_down_before_leaving() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));

    let c = calls.clone();
    let left = on_exit_requested(
        || c.lock().unwrap().push("shutdown"),
        || c.lock().unwrap().push("exit"),
    );

    assert!(left, "退出钩子应当放行退出");
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["shutdown", "exit"],
        "顺序不能反：先关停，再退出"
    );
}
