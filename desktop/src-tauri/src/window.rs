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

#[cfg(test)]
#[path = "window_tests.rs"]
mod window_tests;
