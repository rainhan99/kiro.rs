//! 装配第三段：路由组装与启动横幅。

use std::net::SocketAddr;

use axum::Router;

use crate::{admin, admin_ui, anthropic};

use super::{Accounting, Foundation, Options};

pub(super) fn wiring(base: &Foundation, books: Accounting, options: &Options) -> Router {
    let config = &base.config;
    let token_manager = &base.token_manager;
    let kiro_provider = &base.kiro_provider;
    let endpoint_names = &base.endpoint_names;
    let Accounting {
        client_key_manager,
        usage_recorder,
        usage_aggregator,
        group_manager,
        gateway,
        gateway_entry,
        trace_store,
        cache_meter,
    } = books;

    let anthropic_app = anthropic::create_router_with_shared_provider(
        Some(kiro_provider.clone()),
        config.extract_thinking,
        config.tool_compatibility_mode,
        Some(client_key_manager.clone()),
        Some(usage_recorder.clone()),
        Some(usage_aggregator.clone()),
        Some(cache_meter.clone()),
        trace_store.clone(),
        gateway_entry,
    );

    // 构建 Admin API 路由（配置了非空 adminApiKey 时启用）
    // 安全检查：空字符串被视为未配置，防止空 key 绕过认证
    // 挂载条件：**有密钥** 或 **还没初始化**。
    //
    // 从前只看「有没有非空 adminApiKey」。首次初始化之后，「没有密钥」
    // 恰恰是需要挂载的那种情况——初始化页就住在 /admin 上。没有它，
    // 全新实例会得到一个 404，用户完全不知道下一步该做什么。
    //
    // 未初始化时被挂上来的 authenticated 路由一律 401（中间件里那道
    // 「空密钥不放行」的守卫），只有 /setup 与 /setup/status 可用。
    let admin_key_configured = config
        .admin_api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty());
    let should_mount_admin = admin_key_configured.is_some() || !base.setup.initialized();

    let app = if should_mount_admin {
        {
            let admin_key = admin_key_configured.unwrap_or("");
            // Admin 查询需要一个确定的 store；traces.db 打开失败时用内存兜底（仅本进程有效）
            let admin_trace_store = trace_store.clone().unwrap_or_else(|| {
                std::sync::Arc::new(
                    admin::TraceStore::open_in_memory().expect("内存 trace store 初始化失败"),
                )
            });
            let admin_service =
                admin::AdminService::new(token_manager.clone(), endpoint_names.clone())
                    .with_kiro_provider(kiro_provider.clone())
                    .with_log_governance(
                        Some(admin_trace_store.clone()),
                        Some(usage_recorder.clone()),
                    )
                    .with_cache_meter(cache_meter.clone())
                    .with_self_update(options.allow_self_update);
            let admin_state = admin::AdminState::new(
                admin_key,
                admin_service,
                client_key_manager.clone(),
                usage_aggregator.clone(),
                admin_trace_store,
                group_manager.clone(),
            )
            .with_gateway(Some(gateway.clone()))
            .with_setup(base.setup.clone())
            .with_sessions(base.sessions.clone());

            // 启动余额后台刷新调度器（每 5 分钟一次，与缓存 TTL 对齐）
            admin_state
                .service
                .start_balance_refresher(std::time::Duration::from_secs(300));

            // 启动代理池健康检查调度器（每 5 分钟一次）
            admin_state
                .service
                .start_proxy_health_checker(std::time::Duration::from_secs(300));

            // 启动自动更新调度器：每分钟检查一次本地时间，到达 update_auto_apply_time
            // 且开启 update_auto_apply 时执行一次更新；否则静默等待。
            admin_state.service.start_auto_update_scheduler();

            let admin_app = admin::create_admin_router(admin_state);

            // 创建 Admin UI 路由
            let admin_ui_app = admin_ui::create_admin_ui_router();

            tracing::info!("Admin API 已启用");
            tracing::info!("Admin UI 已启用: /admin");
            anthropic_app
                .nest("/api/admin", admin_app)
                .nest("/admin", admin_ui_app)
                .route(
                    "/admin/",
                    axum::routing::get(|| async { axum::response::Redirect::temporary("/admin") }),
                )
        }
    } else {
        anthropic_app
    };

    app
}

/// 启动横幅的内容。
///
/// 抽成纯函数是为了能测「不打密钥明文」这条——那是一条容易被后来者无意
/// 破坏的约束（加一行 `apiKey = {}` 很顺手），而日志一旦被 systemd /
/// Docker / CI 采集走就收不回来了。
///
/// 地址取**实际**监听到的那个，不是配置里写的：端口被占用时会回退，
/// 打配置值等于骗人。
pub fn startup_banner_lines(
    addr: SocketAddr,
    data_dir: &std::path::Path,
    setup_token: Option<&str>,
) -> Vec<String> {
    // 绑在通配符上时，`http://0.0.0.0:8990` 是个打不开的地址——它是监听
    // 通配符，不是可访问地址。照着点必然失败，而失败的样子像是服务没起来。
    // 给一个真能打开的（回环），同时如实说明实际监听在哪。
    let reachable = reachable_address(addr);
    let bind_note = if reachable == addr.to_string() {
        String::new()
    } else {
        format!("（实际监听 {addr}，同网段的其它机器用本机 IP 访问）")
    };

    let mut lines = vec![
        format!("服务已就绪: http://{reachable} {bind_note}"),
        format!("  管理界面  http://{reachable}/admin"),
        format!("  API 端点  http://{reachable}/v1/messages"),
        format!("数据目录: {}", data_dir.display()),
        "  配置与密钥在该目录的 config.json 内。".to_string(),
        // 不在这里打密钥明文：日志常被采集。取回方式两端各有一个入口。
        "  要取回密钥: 命令行 `kiro-rs --show-keys`；桌面版见「系统设置 → 安全」。".to_string(),
    ];

    // 未初始化：把 setup token 摆到最显眼的位置。
    //
    // 分隔线不是装饰。这行夹在一串 INFO 日志里，用户扫一眼就得能认出
    // 「这是要我抄走的」——`token = xxx` 混在端点列表中间会被整个略过。
    //
    // **每次启动都打**，不是只打第一次：token 只存内存，错过了就重启再看。
    if let Some(token) = setup_token {
        let rule = "━".repeat(64);
        lines.extend([
            rule.clone(),
            "  尚未设置管理密码。用下面这串一次性口令去初始化：".to_string(),
            String::new(),
            format!("      {token}"),
            String::new(),
            format!("  打开 http://{reachable}/admin ，粘贴它并设置你自己的密码。"),
            "  口令只存在于内存，重启即换；设置完成后立即失效。".to_string(),
            rule,
        ]);
    }

    lines
}

/// 把监听地址翻译成一个**真能打开**的地址。
///
/// `0.0.0.0` / `[::]` 是监听通配符：它表示「所有接口」，本身不是可访问
/// 地址。原样打给用户，第一眼看到的就是一个点不开的链接，而点不开的样子
/// 像是服务没起来。
fn reachable_address(addr: SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        let host = if addr.is_ipv6() { "[::1]" } else { "127.0.0.1" };
        format!("{host}:{}", addr.port())
    } else {
        addr.to_string()
    }
}

/// 启动后把横幅打到日志里。
pub fn log_startup_banner(addr: SocketAddr, data_dir: &std::path::Path, setup_token: Option<&str>) {
    for line in startup_banner_lines(addr, data_dir, setup_token) {
        tracing::info!("{line}");
    }
}
