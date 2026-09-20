//! 装配第二段：计费、用量、分组、网关、trace、缓存计量。
//!
//! 这一段里有四处原本 `std::process::exit(1)`，全部改成返回 `Err`，
//! 致命性一点没降。详见 [`accounting`] 的文档。

use std::sync::Arc;

use crate::{admin, anthropic, gateway};

use super::Foundation;

pub(crate) struct Accounting {
    pub client_key_manager: Arc<crate::admin::ClientKeyManager>,
    pub usage_recorder: Arc<crate::admin::UsageRecorder>,
    pub usage_aggregator: Arc<crate::admin::UsageAggregator>,
    pub group_manager: Arc<crate::admin::GroupManager>,
    pub gateway: Arc<crate::gateway::service::GatewayService>,
    pub gateway_entry: Option<Arc<crate::gateway::entry::GatewayEntry>>,
    pub trace_store: Option<crate::admin::SharedTraceStore>,
    pub cache_meter: Arc<crate::anthropic::cache_metering::CacheMeter>,
}

/// 装配第二段：计费与账务。
///
/// 这一段里有四处原本 `std::process::exit(1)`，全部改成返回 `Err`，
/// **致命性一点没降**：
///
/// - 网关初始化失败 → 账本打不开就不能收费，拒绝启动；
/// - 网关启动收尾失败 → 带着一笔来历不明的在飞预留开始收费是错的；
/// - 读账本最大 key id 失败 → 读不到就可能重用已删 Key 的身份；
/// - 网关 HTTP client 构建失败 → 接管了模型却发不出去。
///
/// 把其中任何一处改成「告警后继续」，服务就会带着一个记不了账的网关跑在
/// 花真钱的路上。`a_gateway_that_cannot_open_its_ledger_aborts_startup`
/// 钉住这条，且做过变异验证。
///
/// 对照：`traces.db` 打不开**不**致命（trace 不可用但服务正常），那处保持
/// `warn` 不变——两者的区别是「记不了账」与「看不了日志」。
pub(crate) async fn accounting(base: &Foundation) -> anyhow::Result<Accounting> {
    let config = &base.config;
    let cache_dir = &base.cache_dir;
    let token_manager = &base.token_manager;
    let configured_api_key = &base.configured_api_key;

    // 升级路径：老部署里躺着的秘密文件可能还是 644。只在写入点设防不够——
    // 好几个文件只有内容变化时才写，不写就永远修不好。开始接受请求之前
    // 先扫一遍。
    crate::common::fs::harden_data_dir(cache_dir);

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
        .map_err(|e| anyhow::anyhow!("网关初始化失败: {e:#}"))?,
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
        let adoption = gateway
            .adopt_legacy_state(&balances)
            .map_err(|e| anyhow::anyhow!("网关启动收尾失败: {e:#}"))?;
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
            Err(error) => anyhow::bail!("读取账本最大 key id 失败: {error:#}"),
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
            // 接管了模型却建不出 client，等于接管了却发不出去。
            Err(error) => anyhow::bail!("网关 HTTP client 构建失败: {error:#}"),
        }
    } else {
        None
    };

    Ok(Accounting {
        client_key_manager,
        usage_recorder,
        usage_aggregator,
        group_manager,
        gateway,
        gateway_entry,
        trace_store,
        cache_meter,
    })
}
