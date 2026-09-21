// Windows 上不要在启动 GUI 时弹一个控制台窗口。debug 构建保留控制台，
// 否则开发时看不到日志。
#![cfg_attr(not(debug_assertions), cfg_attr(windows, windows_subsystem = "windows"))]

use std::sync::Mutex;

use kiro_rs_desktop_lib::{
    admin_url, start_proxy,
    window::{on_exit_requested, setup_token_script_for, should_inject_login},
};
use tauri::{AppHandle, Manager};

/// 被 Tauri 托管的运行期状态。
///
/// - `server`：退出时取回来做优雅关停（否则网关的在飞预留会被直接丢弃）。
/// - `pending_login`：导航到管理界面后要注入的免登录脚本，注入一次就清空。
#[derive(Default)]
struct AppState {
    server: Mutex<Option<kiro_rs::RunningServer>>,
    pending_inject: Mutex<Option<String>>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // KIRO_HEADLESS：只起代理，不开窗口。
    //
    // 离线冒烟（SC-12）需要它：要验的是「安装后的应用能启动、服务就绪、
    // 对话走完一轮」，而 CI 上没有图形界面可开。不给这条路的话，冒烟
    // 只能去跑 target/ 里的裸二进制——那验的就不是「安装后的应用」了。
    if std::env::var_os("KIRO_HEADLESS").is_some() {
        return run_headless();
    }

    tauri::Builder::default()
        .manage(AppState::default())
        .setup(|app| {
            let handle = app.handle().clone();
            // 窗口此刻显示的是打包进应用的等待页。代理在后台起，拿到实际
            // 地址后再导航过去——顺序反了用户会看到「无法连接」。
            tauri::async_runtime::spawn(async move {
                match start_proxy().await {
                    Ok(server) => {
                        // 与命令行同一份横幅：形态不同不该让「服务在哪、
                        // 数据在哪」这两件事的说法也不同。
                        kiro_rs::runtime::log_startup_banner(
                            server.addr(),
                            server.data_dir(),
                            server.setup_token(),
                        );

                        let url = admin_url(server.addr());
                        let state = handle.state::<AppState>();
                        // 注入的是**一次性 setup token**，不是管理密钥——
                        // 桌面版没有控制台，用户无从抄那串口令；但密码仍然
                        // 要他自己设。要在**导航之后**注入：等待页跑在
                        // tauri://，管理界面跑在 http://127.0.0.1，不同源。
                        // 只注入 setup token。「记不记住登录」现在由服务端
                        // 会话的 TTL 决定，桌面端不再插手——两处各管一半
                        // 只会让「为什么它记住了/为什么它忘了」无从解释。
                        *state.pending_inject.lock().unwrap() =
                            setup_token_script_for(server.setup_token());

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
                        *state.server.lock().unwrap() = Some(server);
                    }
                    Err(error) => report_startup_failure(&handle, error),
                }
            });
            Ok(())
        })
        .on_page_load(|window, payload| {
            // 页面开始加载时注入，赶在管理界面的脚本读 localStorage 之前。
            // 只对管理界面那个源注入，见 should_inject_login 的注释。
            // 注入一次就清空：token 是一次性的，设完即废。
            if !should_inject_login(payload.url().as_str()) {
                return;
            }
            let state = window.state::<AppState>();
            let script = state.pending_inject.lock().unwrap().take();
            if let Some(script) = script {
                if let Err(error) = window.eval(script) {
                    tracing::warn!("setup token 注入失败，初始化页会要求手动粘贴: {error}");
                }
            }
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // 先拦下关闭，等在飞请求走完再真的退出。
                api.prevent_close();
                let app = window.app_handle().clone();
                // 关停要等在飞请求，不能阻塞 UI 线程。
                std::thread::spawn(move || {
                    let for_exit = app.clone();
                    on_exit_requested(|| shutdown_proxy(&app), move || for_exit.exit(0));
                });
            }
        })
        .run(tauri::generate_context!())
        .expect("Tauri 运行时启动失败");
}

/// 启动失败不能留一个空窗口。把原因摆到等待页上。
///
/// 用 `eval` 而不是 Tauri 事件：事件要在 webview 里暴露全局 Tauri API
/// （withGlobalTauri），为了一条错误消息不值得扩大那个面。
fn report_startup_failure(handle: &tauri::AppHandle, error: anyhow::Error) {
    let message = format!("{error:#}");
    tracing::error!("{message}");
    if let Some(window) = handle.get_webview_window("main") {
        // JSON 序列化负责转义——错误串里可能有引号和换行。
        let literal =
            serde_json::to_string(&message).unwrap_or_else(|_| "\"启动失败\"".to_string());
        let _ = window.eval(format!("window.__kiroStartupFailed({literal})"));
    }
}

/// 关窗即退出时走优雅关停。
///
/// 直接 exit 会把 axum 任务连同在飞请求一起丢弃，网关账本上会留下永不
/// 结算的预留记录——那是钱的记录，不是日志。
fn shutdown_proxy(app: &AppHandle) {
    let state = app.state::<AppState>();
    let server = state.server.lock().unwrap().take();
    if let Some(server) = server {
        tauri::async_runtime::block_on(async {
            if let Err(error) = server.shutdown().await {
                tracing::warn!("优雅关停失败: {error:#}");
            }
        });
    }
}

/// 无窗口模式：起代理，打横幅，等 Ctrl-C。
///
/// 走的是与窗口模式**完全相同**的 `start_proxy()`——否则冒烟验的就是
/// 另一条代码路径，那种绿灯没有意义。
fn run_headless() {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("无法创建运行时: {error}");
            std::process::exit(1);
        }
    };

    runtime.block_on(async {
        let server = match start_proxy().await {
            Ok(server) => server,
            Err(error) => {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        };
        kiro_rs::runtime::log_startup_banner(
            server.addr(),
            server.data_dir(),
            server.setup_token(),
        );
        if let Err(error) = server.wait_for_signal().await {
            eprintln!("服务异常退出: {error:#}");
            std::process::exit(1);
        }
    });
}
