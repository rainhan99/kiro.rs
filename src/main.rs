// 模块住在库里（src/lib.rs）。这个二进制只是库的一个使用者——
// 桌面应用是另一个。
use kiro_rs::{admin, admin_ui, anthropic, gateway, kiro, model, pipeline};

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
    let kiro_rs::runtime::Foundation {
        config,
        token_manager,
        kiro_provider,
        endpoint_names,
        configured_api_key,
        cache_dir,
    } = match kiro_rs::runtime::foundation(&options) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e:#}");
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
    };

    // 客户端 Key 管理器 + 用量记录器 + 聚合器（cache_dir 由 foundation 交出，与凭据文件同目录）
    let client_keys_path = admin::client_keys::default_path_in(&cache_dir);
    let client_key_manager = std::sync::Arc::new(
        admin::ClientKeyManager::load(&client_keys_path).unwrap_or_else(|e| {
            tracing::warn!(
                "加载客户端 Key 失败 ({}): {}",
                client_keys_path.display(),
                e
            );
            admin::ClientKeyManager::new()
        }),
    );
    let usage_recorder = std::sync::Arc::new(admin::UsageRecorder::with_retention(
        cache_dir.clone(),
        config.usage_log_retention_days as i64,
    ));
    let usage_aggregator = std::sync::Arc::new(admin::UsageAggregator::new());
    usage_aggregator.rebuild_from_logs(&cache_dir);

    // 账号分组注册表（持久化到 groups.json）。
    // 启动时若文件不存在则首次创建，并把现有凭据 / 客户端 Key 的 groups 字段反向迁移进去，
    // 保证老用户升级后所有已用分组都自动注册，不会因为本次改造而消失。
    let groups_path = admin::groups::default_path_in(&cache_dir);
    let group_manager =
        std::sync::Arc::new(admin::GroupManager::load(&groups_path).unwrap_or_else(|e| {
            tracing::warn!("加载分组注册表失败 ({}): {}", groups_path.display(), e);
            admin::GroupManager::new()
        }));
    {
        let mut all_used: Vec<String> = token_manager.list_credential_groups();
        all_used.extend(client_key_manager.used_group_names());
        let added = group_manager.bootstrap_from_existing(all_used);
        if added > 0 {
            tracing::info!("分组注册表：自动迁移 {} 个已用分组", added);
        }
    }

    // 多上游网关。`gateway.json` 与 `billing.db` 与既有缓存文件并列。
    //
    // 与 traces.db 不同，这里的失败**是致命的**：
    // - 配置存在但不合法 → 拒绝启动，而不是当作"没有上游"静悄悄拒绝全部流量；
    // - 配置声明了上游/模型但账本打不开 → 拒绝启动，记不了账就不能收费。
    //
    // 配置缺失时网关完全惰性，任何别名都不接管，请求原样走既有路径——
    // 现有部署不会因为引入这个特性而改变行为。
    let gateway = std::sync::Arc::new(
        gateway::service::GatewayService::open(
            &cache_dir.join("gateway.json"),
            &cache_dir.join("billing.db"),
        )
        .unwrap_or_else(|e| {
            tracing::error!("网关初始化失败: {:#}", e);
            std::process::exit(1);
        }),
    );
    // 启动收尾必须在开始接受请求**之前**做完：先认领上个进程遗留的在飞预留，
    // 再为尚未立户的遗留 Key 建立积分开账余额。失败与网关初始化同样致命——
    // 带着一笔来历不明的在飞预留、或者一批还没立户的 Key 开始收费，是错的。
    {
        let balances: Vec<gateway::import::LegacyKeyBalance> = client_key_manager
            .list()
            .iter()
            .map(|k| gateway::import::LegacyKeyBalance {
                key_id: k.id,
                used: k.total_credits,
                limit: k.max_credits,
            })
            .collect();
        let adoption = gateway.adopt_legacy_state(&balances).unwrap_or_else(|e| {
            tracing::error!("网关启动收尾失败: {:#}", e);
            std::process::exit(1);
        });
        if adoption.recovered_in_flight > 0 {
            tracing::warn!(
                count = adoption.recovered_in_flight,
                "认领上次运行遗留的在飞预留，已转为待结算"
            );
        }
        if !adoption.import.imported.is_empty() {
            tracing::info!(
                count = adoption.import.imported.len(),
                "已为遗留 Key 建立积分开账余额"
            );
        }
        // Key 的 id 一旦在账本上出现过就永不重用：`next_id` 是按存活 Key 推的，
        // 删掉 id 最大的 Key 再重启，新建的 Key 会拿到同一个 id，连同前一个
        // 同 id Key 的余额与历史一起继承。
        match gateway.highest_recorded_key_id() {
            Ok(Some(highest)) => {
                if client_key_manager.reserve_ids_through(highest) {
                    tracing::info!(
                        highest,
                        "已把新建 Key 的 id 下界抬到账本记录之上，避免重用已删 Key 的身份"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!("读取账本最大 key id 失败: {error:#}");
                std::process::exit(1);
            }
        }

        for (id, reason) in &adoption.import.rejected {
            tracing::error!(
                key_id = id,
                "遗留积分数字无法记入账本，该 Key 未立户（迁移保持未封存，修好后重启可补）: {}",
                reason
            );
        }
    }

    {
        let managed = gateway.public_models();
        if managed.is_empty() {
            tracing::info!("多上游网关未配置，全部请求走既有 Kiro 路径");
        } else {
            tracing::info!(
                models = managed.len(),
                "多上游网关已启用，接管模型: {}",
                managed
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    // 请求链路追踪存储（SQLite，traces.db）。失败不致命：trace 不可用但服务正常。
    let trace_store: Option<admin::SharedTraceStore> = match admin::TraceStore::open(
        cache_dir.join("traces.db"),
        config.trace_enabled,
        config.trace_retention_days,
    ) {
        Ok(s) => Some(std::sync::Arc::new(s)),
        Err(e) => {
            tracing::warn!("打开 traces.db 失败，请求链路追踪不可用: {}", e);
            None
        }
    };

    // 启动后定期清理过期 usage_log 与 trace 记录
    {
        let recorder = usage_recorder.clone();
        let trace_store = trace_store.clone();
        tokio::spawn(async move {
            let day = std::time::Duration::from_secs(24 * 3600);
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            loop {
                recorder.cleanup_old_logs();
                if let Some(ts) = &trace_store {
                    ts.cleanup();
                }
                tokio::time::sleep(day).await;
            }
        });
    }

    if let Some(initial_key) = configured_api_key.as_ref() {
        client_key_manager.sync_system_key(
            "默认密钥".to_string(),
            Some("由 config.json apiKey 自动同步（系统密钥）".to_string()),
            initial_key.clone(),
        );
    }

    // CacheMeter：本地计量模拟，支持可选 Redis 共享元数据层；不承载真实 KV Cache。
    // 持久化到 cache_dir/cache_metering.json，启动时自动加载未过期条目。
    let cache_metering_enabled = config.request_pipeline.allow_simulated_cache
        && config
            .cache_metering_enabled
            .or_else(anthropic::cache_metering::CacheMeter::metering_enabled_from_env)
            .unwrap_or(false);
    let cache_meter = std::sync::Arc::new(
        anthropic::cache_metering::CacheMeter::from_env(Some(
            cache_dir.join("cache_metering.json"),
        ))
        .await
        .with_enabled(cache_metering_enabled),
    );
    cache_meter.clone().spawn_background();
    if !cache_metering_enabled {
        tracing::info!(
            "模拟缓存计量已关闭；缓存证据仅使用 Kiro 原生 metadataEvent.tokenUsage，缺失时为未知"
        );
    } else {
        tracing::warn!("本地 prompt cache 模拟计量已显式开启；这些估算不是 Kiro 缓存命中证据");
    }

    // 会话粘性路由：绑定表过期条目定期清理（lookup 只顺手清自己命中的那条）
    {
        let affinity = token_manager.session_affinity();
        if affinity.is_enabled() {
            tracing::info!(
                "会话粘性路由已开启，TTL {}s：同一会话优先沿用上一轮凭据以保住上游 prompt cache",
                affinity.ttl_secs()
            );
        } else {
            tracing::info!("会话粘性路由已关闭");
        }
        let tm = token_manager.clone();
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(300);
            loop {
                tokio::time::sleep(interval).await;
                tm.session_affinity().evict_expired();
            }
        });
    }

    // 网关入口。只有网关接管了模型时才建——没接管就不该有这个对象，
    // 也不该为它建一条 HTTP 连接池。
    let gateway_entry = if gateway.has_managed_models() {
        let timeout =
            std::time::Duration::from_secs(gateway.snapshot().config.request_timeout_secs);
        match gateway::execute::build_client(timeout, config.tls_backend, None) {
            Ok(client) => Some(std::sync::Arc::new(gateway::entry::GatewayEntry::new(
                gateway.clone(),
                client,
            ))),
            Err(error) => {
                // 接管了模型却建不出 client，等于接管了却发不出去。
                tracing::error!("网关 HTTP client 构建失败: {error:#}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

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
