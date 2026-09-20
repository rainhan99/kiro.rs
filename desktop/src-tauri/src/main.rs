// Windows 上不要在启动 GUI 时弹一个控制台窗口。debug 构建保留控制台，
// 否则开发时看不到日志。
#![cfg_attr(not(debug_assertions), cfg_attr(windows, windows_subsystem = "windows"))]

use kiro_rs_desktop_lib::{admin_url, start_proxy};
use tauri::Manager;

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            // 窗口此刻显示的是打包进应用的等待页。代理在后台起，
            // 拿到实际地址后再导航过去——顺序反了用户会看到「无法连接」。
            tauri::async_runtime::spawn(async move {
                match start_proxy().await {
                    Ok(server) => {
                        let url = admin_url(server.addr());
                        tracing::info!("服务就绪: {url}");
                        if let Some(window) = handle.get_webview_window("main") {
                            match url.parse() {
                                Ok(parsed) => {
                                    if let Err(error) = window.navigate(parsed) {
                                        tracing::error!("导航到管理界面失败: {error}");
                                    }
                                }
                                Err(error) => tracing::error!("地址无法解析 {url}: {error}"),
                            }
                        }
                        // 交给 Tauri 托管，退出时才能拿回来做优雅关停（B4）。
                        handle.manage(ProxyHandle(std::sync::Mutex::new(Some(server))));
                    }
                    Err(error) => {
                        // 启动失败不能留一个空窗口。把原因摆到等待页上。
                        //
                        // 用 eval 而不是 Tauri 事件：事件要在 webview 里暴露
                        // 全局 Tauri API（withGlobalTauri），为了一条错误消息
                        // 不值得扩大那个面。JSON 序列化负责转义——错误串里
                        // 可能有引号和换行。
                        let message = format!("{error:#}");
                        tracing::error!("{message}");
                        if let Some(window) = handle.get_webview_window("main") {
                            let literal = serde_json::to_string(&message)
                                .unwrap_or_else(|_| "\"启动失败\"".to_string());
                            let _ = window.eval(format!("window.__kiroStartupFailed({literal})"));
                        }
                    }
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Tauri 运行时启动失败");
}

/// 被 Tauri 托管的代理句柄。退出时取回来做优雅关停。
pub struct ProxyHandle(pub std::sync::Mutex<Option<kiro_rs::RunningServer>>);
