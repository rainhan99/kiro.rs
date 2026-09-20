//! 窗口与代理的启动时序。
//!
//! 顺序是有讲究的：**先拿到实际监听地址，再让窗口导航过去**。反过来做，
//! 用户会在服务起来之前看到浏览器的「无法连接」页面——而真相只是「再等两秒」。

use std::net::SocketAddr;

use crate::paths;

/// 管理界面的地址。由**实际**监听地址算出来，不是配置里写的那个——
/// 端口被占用时会回退，两者可以不一样。
pub fn admin_url(addr: SocketAddr) -> String {
    format!("http://{addr}/admin")
}

/// 启动进程内代理。
///
/// 桌面端与命令行走的是同一段装配逻辑（`kiro_rs::serve`），区别只有两处：
/// 配置落在应用数据目录，以及自更新关闭。
pub async fn start_proxy() -> anyhow::Result<kiro_rs::RunningServer> {
    let base = paths::data_dir_for("kiro-rs")?;
    let files = paths::ensure_data_files(&base)?;
    tracing::info!("数据目录: {}", base.display());

    let mut options = kiro_rs::Options::new(files.config, files.credentials);
    // SC-4：应用包内的可执行文件由安装包管理。让代理去 exec 替换它会破坏
    // 代码签名，并留下一个装不回去的应用。
    options.allow_self_update = false;

    kiro_rs::serve(options).await
}

/// 把一次性 setup token 交给管理界面。
///
/// 桌面端自己就是打印这串口令的那个进程，没理由让用户从控制台手抄一遍
/// ——桌面版根本没有控制台。初始化页拿到它就只问密码。
///
/// **注入的是 token 不是管理密钥**。曾经有一版直接注入 adminApiKey 做
/// 免登录，被否决了：那把门整个拆了，任何走到未锁屏机器前的人双击即得
/// 管理权。token 只能用来**设置**密码，而且一次性、设完即废。
///
/// **必须在导航之后注入**：等待页跑在 `tauri://`，管理界面跑在
/// `http://127.0.0.1:<port>`，两者不同源，localStorage 各是各的。
///
/// 键名与 `admin-ui/src/App.tsx` 的 `readProvidedSetupToken` 一致。
pub fn setup_token_script(token: &str) -> String {
    // JSON 序列化负责转义。token 本身是随机字母数字，但这一行将来可能
    // 被改成注入别的东西，转义是便宜的保险。
    let literal = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".to_string());
    format!("try {{ localStorage.setItem(\"kiroSetupToken\", {literal}); }} catch (e) {{}}")
}

/// 已初始化时返回 `None`。
///
/// 那时根本没有 token。注入一个空值会让初始化页以为「宿主提供了口令」
/// 从而不显示输入框——一个既进不去也说不清为什么的死角。
pub fn setup_token_script_for(token: Option<&str>) -> Option<String> {
    token
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(setup_token_script)
}

/// 「每次启动都要验证」时，清掉上次记住的登录密钥。
///
/// 只清登录密钥这一个键。`localStorage.clear()` 会把主题、折叠状态之类
/// 一起清掉，用户每次开应用都会觉得「我的设置怎么没了」。
pub fn forget_key_script_for(require_auth: bool) -> Option<String> {
    require_auth.then(|| {
        "try { localStorage.removeItem(\"adminApiKey\"); } catch (e) {}".to_string()
    })
}

/// 把这一次要注入的东西拼在一起。
///
/// 两件事可能同时发生：首次启动要给 setup token，而「每次启动都要验证」
/// 开着要清掉记住的密钥。它们互不冲突，但必须一次注完——`on_page_load`
/// 只拿一次脚本。
pub fn injection_script(setup_token: Option<&str>, require_auth: bool) -> Option<String> {
    let parts: Vec<String> = [
        forget_key_script_for(require_auth),
        setup_token_script_for(setup_token),
    ]
    .into_iter()
    .flatten()
    .collect();

    (!parts.is_empty()).then(|| parts.join(" "))
}

/// 这个 URL 该不该收到注入脚本。
///
/// 两条限制，各有理由：
///
/// 1. **只认回环**。窗口理论上可以被导航到任何地方（将来加个外链、或者
///    管理界面里有个跳转），把 setup token 注给外站是不可接受的——
///    拿着它就能给这个实例设管理密码。
/// 2. **只认 /admin 路径**。`on_page_load` 对等待页也会触发；等待页跑在
///    `tauri://`，与管理界面不同源，注进去毫无用处，而脚本已经被取走，
///    真正该注的那一次反而没有了。表现是「偶尔要我抄口令、偶尔不用」。
pub fn should_inject_login(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = host_port.split(':').next().unwrap_or_default();
    let is_loopback = host == "127.0.0.1" || host == "localhost" || host == "[::1]";
    is_loopback && (path == "/admin" || path.starts_with("/admin/") || path.starts_with("/admin#"))
}

/// 退出流程：先优雅关停代理，再退出进程。
///
/// 抽成接收两个闭包的纯函数是为了能测顺序。直接在 Tauri 的事件回调里写，
/// 唯一的验证方式就是肉眼读代码——而「顺序反了」在肉眼下和正确的长得一样。
///
/// 返回是否放行退出。
pub fn on_exit_requested(shutdown: impl FnOnce(), exit: impl FnOnce()) -> bool {
    shutdown();
    exit();
    true
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod window_tests;
