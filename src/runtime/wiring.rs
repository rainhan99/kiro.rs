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
    let app = if let Some(admin_key) = &config.admin_api_key {
        if admin_key.trim().is_empty() {
            tracing::warn!("admin_api_key 配置为空，Admin API 未启用");
            anthropic_app
        } else {
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
            .with_gateway(Some(gateway.clone()));

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
pub fn startup_banner_lines(addr: SocketAddr, data_dir: &std::path::Path) -> Vec<String> {
    vec![
        format!("服务已就绪: http://{addr}"),
        format!("  管理界面  http://{addr}/admin"),
        format!("  API 端点  http://{addr}/v1/messages"),
        format!("数据目录: {}", data_dir.display()),
        "  配置与密钥在该目录的 config.json 内。".to_string(),
        // 不在这里打密钥明文：日志常被采集。取回方式两端各有一个入口。
        "  要取回密钥: 命令行 `kiro-rs --show-keys`；桌面版见「系统设置 → 安全」。".to_string(),
    ]
}

/// 启动后把横幅打到日志里。
pub fn log_startup_banner(addr: SocketAddr, data_dir: &std::path::Path) {
    for line in startup_banner_lines(addr, data_dir) {
        tracing::info!("{line}");
    }
}
