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

/// 免登录：把管理密钥直接写进管理界面的 localStorage。
///
/// 桌面端自己就是读配置文件把服务起起来的那个进程——它本来就持有密钥。
/// 再让用户手输一遍是纯粹的仪式，安全上一点没多：能读配置文件的人本来
/// 就有密钥。
///
/// **必须在导航之后注入**：等待页跑在 `tauri://`，管理界面跑在
/// `http://127.0.0.1:<port>`，两者不同源，localStorage 各是各的。
///
/// 键名与 `admin-ui/src/lib/storage.ts` 里的 `API_KEY_STORAGE_KEY` 一致。
/// 写错一个字母的表现是「没报错，但还是要你登录」。
pub fn auto_login_script(admin_key: &str) -> String {
    // JSON 序列化负责转义：密钥来自配置文件，内容是任意字符串，
    // 直接拼进 JS 会造出语法错误，或者更糟。
    let literal = serde_json::to_string(admin_key).unwrap_or_else(|_| "\"\"".to_string());
    format!("try {{ localStorage.setItem(\"adminApiKey\", {literal}); }} catch (e) {{}}")
}

/// 没有可用密钥时返回 `None`。
///
/// 注入一个空值或 "null" 会让管理界面拿着假密钥去请求然后报 401，
/// 用户完全看不懂——还不如老老实实显示登录页。
pub fn auto_login_script_for(admin_key: Option<&str>) -> Option<String> {
    admin_key
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(auto_login_script)
}

/// 这个 URL 该不该收到免登录脚本。
///
/// 两条限制，各有理由：
///
/// 1. **只认回环**。窗口理论上可以被导航到任何地方（将来加个外链、或者
///    管理界面里有个跳转），把管理密钥注给外站是不可接受的。
/// 2. **只认 /admin 路径**。`on_page_load` 对等待页也会触发；等待页跑在
///    `tauri://`，与管理界面不同源，注进去毫无用处，而脚本已经被取走，
///    真正该注的那一次反而没有了。表现是「偶尔要登录、偶尔不用」。
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
