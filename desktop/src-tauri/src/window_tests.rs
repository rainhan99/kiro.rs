use super::*;

/// 时序契约：窗口的初始 URL 必须是打包进应用的等待页，而不是一个还没人
/// 监听的 http 地址——那会让用户看到浏览器的「无法连接」页面，
/// 而真相只是「再等两秒」。
#[test]
fn the_window_starts_on_the_bundled_waiting_page() {
    let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json"))
        .expect("tauri.conf.json 要是合法 JSON");
    let url = conf["app"]["windows"][0]["url"]
        .as_str()
        .expect("窗口要有初始 URL");
    assert_eq!(url, "waiting.html");
    assert!(!url.starts_with("http"), "初始 URL 不能指向尚未监听的服务");
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
    let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
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

/// 桌面端注入的是 **setup token**，不是管理密钥。
///
/// 这是「免登录」被推翻后的替代品：桌面端自己就是打印那串口令的进程，
/// 没理由让用户再手抄一遍——但密码仍然要用户自己设。
///
/// 键名必须与 `admin-ui/src/App.tsx` 的 `readProvidedSetupToken` 一致。
/// 写错一个字母的表现是「初始化页还是问我要口令」——最难查的那种。
#[test]
fn the_injected_script_carries_the_setup_token_not_the_admin_key() {
    let script = setup_token_script("TOKEN123");
    assert!(script.contains("localStorage.setItem"), "实际: {script}");
    assert!(
        script.contains("\"kiroSetupToken\""),
        "键名必须与 App.tsx 的 readProvidedSetupToken 一致，实际: {script}"
    );
    assert!(script.contains("TOKEN123"));
    assert!(
        !script.contains("adminApiKey"),
        "绝不能注入管理密钥——那就退回免登录了，实际: {script}"
    );
}

/// token 是随机串，但脚本拼接仍要转义：这一行将来可能被改成注入别的东西。
#[test]
fn the_injected_script_escapes_its_payload() {
    let script = setup_token_script("a\"b\\c");
    assert!(!script.contains("a\"b"), "原样拼进去了: {script}");
    assert!(script.contains("\\\""), "引号要被转义，实际: {script}");
}

/// 已初始化时不注入任何东西——那时根本没有 token，注入一个空值只会让
/// 初始化页以为「宿主提供了口令」从而不显示输入框。
#[test]
fn nothing_is_injected_once_the_instance_is_claimed() {
    assert!(setup_token_script_for(None).is_none());
    assert!(
        setup_token_script_for(Some("   ")).is_none(),
        "空白串等同没有"
    );
    assert!(setup_token_script_for(Some("TOKEN123")).is_some());
}

/// `on_page_load` 对**等待页**也会触发。如果服务起得够快、在等待页那次
/// 回调之前就把脚本挂上了，脚本会被等待页吃掉——而等待页跑在 `tauri://`，
/// 与管理界面不同源，注进去毫无用处，且脚本已经被 take 走，真正该注的
/// 那一次反而没有了。表现是「偶尔要登录、偶尔不用」。
#[test]
fn only_the_admin_origin_gets_the_injected_script() {
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
fn the_injected_script_never_leaves_the_loopback() {
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
