// 模块住在库里（src/lib.rs）。这个二进制只是库的一个使用者——
// 桌面应用是另一个。
use kiro_rs::{admin, admin_ui, anthropic, kiro, model, pipeline};

use clap::Parser;
use kiro::model::credentials::KiroCredentials;
use model::arg::Args;
use model::config::Config;

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 初始化日志
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 解析配置/凭证路径
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());

    // Offline commands exit before credential loading, generated config files,
    // version/model warmers, Redis or any provider HTTP client is initialized.
    if args.check_config || args.inspect_request.is_some() {
        if let Err(error) = pipeline::inspect::run(&config_path, args.inspect_request.as_deref()) {
            eprintln!("{error:#}");
            std::process::exit(2);
        }
        return;
    }
    let credentials_path = args
        .credentials
        .unwrap_or_else(|| KiroCredentials::default_credentials_path().to_string());

    // 装配第一段下沉到库里。二进制与桌面应用共用同一段逻辑——
    // 区别只在失败时怎么死：这里退出进程，桌面端弹一个窗。
    let options = kiro_rs::Options::new(&config_path, &credentials_path);
    let base = match kiro_rs::runtime::foundation(&options) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e:#}");
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
    };

    // 装配第二段（计费、用量、分组、网关、trace、缓存计量）也下沉到库里。
    let kiro_rs::runtime::Accounting {
        client_key_manager,
        usage_recorder,
        usage_aggregator,
        group_manager,
        gateway,
        gateway_entry,
        trace_store,
        cache_meter,
    } = match kiro_rs::runtime::accounting(&base, &options).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e:#}");
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
    };

    // 路由装配还在这里（A7 下沉）。
    let kiro_rs::runtime::Foundation {
        config,
        token_manager,
        kiro_provider,
        endpoint_names,
        ..
    } = base;

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
                    .with_cache_meter(cache_meter.clone());
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

    // 启动服务器
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("启动 Anthropic API 端点: {}", addr);
    tracing::info!("可用 API:");
    tracing::info!("  GET  /v1/models");
    tracing::info!("  POST /v1/messages");
    tracing::info!("  POST /v1/messages/count_tokens");
    tracing::info!("Admin API:");
    tracing::info!("  GET  /api/admin/credentials");
    tracing::info!("  POST /api/admin/credentials/:index/disabled");
    tracing::info!("  POST /api/admin/credentials/:index/priority");
    tracing::info!("  POST /api/admin/credentials/:index/reset");
    tracing::info!("  GET  /api/admin/credentials/:index/balance");
    tracing::info!("Admin UI:");
    tracing::info!("  GET  /admin");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    // with_connect_info：把 TCP 对端地址注入请求扩展，直连部署下作为客户端 IP 的兜底
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .unwrap();
}
