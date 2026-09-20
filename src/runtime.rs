//! 库入口：在进程内启动 kiro-rs。
//!
//! 这一层的存在理由是桌面应用——它要在自己的进程里把代理跑起来，因此
//! **装配失败必须返回错误，而不是终止进程**。二进制形态下由 `main.rs`
//! 这层薄壳把错误翻译成退出码，行为与从前一致。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;

use crate::kiro::endpoint::{CliEndpoint, IdeEndpoint, KiroEndpoint};
use crate::kiro::model::credentials::{CredentialsConfig, KiroCredentials};
use crate::kiro::provider::KiroProvider;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::Config;
use crate::{admin, anthropic, gateway, http_client, kiro, model, token};

/// 库入口的启动参数。字段公开，调用方直接构造。
pub struct Options {
    pub config_path: PathBuf,
    pub credentials_path: PathBuf,
    /// 桌面端置 false：应用包内的可执行文件由安装包管理，
    /// 让代理去 exec 替换它会破坏代码签名，并留下一个装不回去的应用。
    pub allow_self_update: bool,
}

impl Options {
    pub fn new(config_path: impl Into<PathBuf>, credentials_path: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            credentials_path: credentials_path.into(),
            allow_self_update: true,
        }
    }
}

/// 正在运行的服务。持有实际监听地址与关停手柄。
///
/// 丢弃它会关掉 oneshot 发送端，优雅关停信号随之触发——但不等在飞请求走完。
/// 要等就调 [`RunningServer::shutdown`]。
#[derive(Debug)]
pub struct RunningServer {
    addr: SocketAddr,
    shutdown: tokio::sync::oneshot::Sender<()>,
    joined: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl RunningServer {
    /// 实际监听地址。配置里写 0、或端口被占用而回退时，这里是真正拿到的那个。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 优雅关停：停止收新连接，等在飞请求走完。
    ///
    /// 这不是可有可无的礼貌。网关的在飞预留要靠请求自己走完既有的结算/释放
    /// 路径；把 axum 任务直接丢弃会在账本上留下永不结算的记录。
    pub async fn shutdown(self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(());
        self.joined.await??;
        Ok(())
    }
}

/// 在当前 tokio 运行时上启动 kiro-rs。
///
/// **不自建运行时**：Tauri 自带一个，同进程两个运行时会让 `Handle::current()`
/// 拿到错误的那个，表现是随机的 "no reactor running" panic。
pub async fn serve(options: Options) -> anyhow::Result<RunningServer> {
    let (app, addr_spec) = assemble(&options).await?;
    let listener = bind_with_fallback(addr_spec).await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let joined = tokio::spawn(async move {
        // with_connect_info：把 TCP 对端地址注入请求扩展，直连部署下作为
        // 客户端 IP 的兜底。与二进制从前的行为一致，不能省。
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = rx.await;
        })
        .await
    });
    Ok(RunningServer {
        addr,
        shutdown: tx,
        joined,
    })
}

/// 装配的第一段产物：配置、凭据池、provider。
///
/// 这些是后面每一段都要用到的东西。
///
/// **过渡接口**：装配还没全部下沉，二进制暂时也要直接用它，而二进制是独立
/// 的 crate，所以只能 `pub`。A7 把 `main.rs` 瘦身成薄壳后收回 `pub(crate)`。
pub struct Foundation {
    pub config: Config,
    pub token_manager: Arc<MultiTokenManager>,
    pub kiro_provider: Arc<KiroProvider>,
    pub endpoint_names: Vec<String>,
    pub configured_api_key: Option<String>,
    /// 所有运行期文件的落脚点：客户端 Key、用量日志、分组、账本、trace、缓存计量。
    /// 取的是凭据文件的父目录——只要调用方传绝对路径，这些文件就不会落到 CWD。
    pub cache_dir: PathBuf,
}

/// 装配第一段：读配置与凭据、注册端点、建 token 管理器与 provider。
///
/// 三处原本 `std::process::exit(1)` 的地方现在返回 `Err`：配置加载、凭据加载、
/// Token 管理器创建。**致命性没有变**——只是把「终止进程」换成「返回错误」，
/// 由调用方决定怎么死。库不能替调用方做这个决定。
pub fn foundation(options: &Options) -> anyhow::Result<Foundation> {
    let config_path = options.config_path.to_string_lossy().into_owned();
    let credentials_path = options.credentials_path.to_string_lossy().into_owned();

    // 文件不存在时自动初始化（Docker 首次部署友好）
    ensure_config_files(&config_path, &credentials_path);

    // 加载配置
    let config =
        Config::load(&config_path).map_err(|e| anyhow::anyhow!("加载配置失败: {e}"))?;

    // 加载凭证（支持单对象或数组格式）
    let credentials_config = CredentialsConfig::load(&credentials_path)
        .map_err(|e| anyhow::anyhow!("加载凭证失败: {e}"))?;

    // 判断是否为多凭据格式（用于刷新后回写）
    let is_multiple_format = credentials_config.is_multiple();

    // 转换为按优先级排序的凭据列表
    let mut credentials_list = credentials_config.into_sorted_credentials();

    // 检查 KIRO_API_KEY 环境变量，自动创建 API Key 凭据
    if let Ok(kiro_api_key) = std::env::var("KIRO_API_KEY") {
        if kiro_api_key.is_empty() {
            tracing::warn!("KIRO_API_KEY 环境变量已设置但为空，视为未配置");
        } else {
            tracing::info!("检测到 KIRO_API_KEY 环境变量，添加 API Key 凭据（最高优先级）");
            let api_key_cred = KiroCredentials {
                kiro_api_key: Some(kiro_api_key),
                auth_method: Some("api_key".to_string()),
                priority: 0,
                ..Default::default()
            };
            credentials_list.insert(0, api_key_cred);
        }
    }

    tracing::info!("已加载 {} 个凭据配置", credentials_list.len());

    // 仅显示安全的元数据，避免在日志里泄露 token / client_secret
    let first_credentials = credentials_list.first().cloned().unwrap_or_default();
    tracing::debug!(
        id = ?first_credentials.id,
        email = ?first_credentials.email,
        auth_method = ?first_credentials.auth_method,
        priority = first_credentials.priority,
        endpoint = ?first_credentials.endpoint,
        "已选定主凭证"
    );

    let configured_api_key = config.api_key.clone().filter(|k| !k.trim().is_empty());

    // 构建代理配置（空字符串视为未配置）
    let proxy_config = config
        .proxy_url
        .as_ref()
        .filter(|url| !url.trim().is_empty())
        .map(|url| {
            let mut proxy = http_client::ProxyConfig::new(url);
            if let (Some(username), Some(password)) =
                (&config.proxy_username, &config.proxy_password)
            {
                proxy = proxy.with_auth(username, password);
            }
            proxy
        });

    if proxy_config.is_some() {
        tracing::info!("已配置 HTTP 代理: {}", config.proxy_url.as_ref().unwrap());
    }

    // 启动 Kiro IDE 版本自动获取：从官方元数据端点拉取 currentRelease，
    // 用于流式端点 User-Agent（替代写死的版本号）；失败时回退 config.kiroVersion。
    kiro::kiro_version::spawn_refresher(
        proxy_config.clone(),
        config.tls_backend,
        std::time::Duration::from_secs(12 * 3600),
    );

    // 构建端点注册表
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    {
        let ide = IdeEndpoint::new();
        endpoints.insert(ide.name().to_string(), Arc::new(ide));
        let cli = CliEndpoint::new();
        endpoints.insert(cli.name().to_string(), Arc::new(cli));
    }

    // 校验默认端点存在。
    //
    // 防御性：`Config::validate()` 已经把 default_endpoint 限死在 ide|cli，
    // 从配置文件走不到这里。留着是因为注册表将来可能变（加端点、按 feature
    // 裁剪），那时这条就是唯一的守卫。
    if !endpoints.contains_key(&config.default_endpoint) {
        anyhow::bail!("默认端点 \"{}\" 未注册", config.default_endpoint);
    }

    // 校验所有凭据声明的端点都已注册。
    // 凭据的 endpoint 字段没有反序列化期校验，这里是唯一的守卫。
    for cred in &credentials_list {
        let name = cred.endpoint.as_deref().unwrap_or(&config.default_endpoint);
        if !endpoints.contains_key(name) {
            anyhow::bail!(
                "凭据 id={:?} 指定了未知端点 \"{}\"（已注册: {:?}）",
                cred.id,
                name,
                endpoints.keys().collect::<Vec<_>>()
            );
        }
    }

    let endpoint_names: Vec<String> = endpoints.keys().cloned().collect();

    // 创建 MultiTokenManager 和 KiroProvider
    let token_manager = MultiTokenManager::new(
        config.clone(),
        credentials_list,
        proxy_config.clone(),
        Some(credentials_path.into()),
        is_multiple_format,
    )
    .map_err(|e| anyhow::anyhow!("创建 Token 管理器失败: {e}"))?;
    let token_manager = Arc::new(token_manager);
    token_manager.start_model_cache_warmer();
    let kiro_provider = Arc::new(KiroProvider::with_proxy(
        token_manager.clone(),
        proxy_config.clone(),
        endpoints,
        config.default_endpoint.clone(),
    ));

    // 初始化自定义模型注册表（启动时装载一次，运行期只读）
    model::custom_models::init(config.custom_models.clone());

    // 初始化 count_tokens 配置
    token::init_config(token::CountTokensConfig {
        api_url: config.count_tokens_api_url.clone(),
        api_key: config.count_tokens_api_key.clone(),
        auth_type: config.count_tokens_auth_type.clone(),
        proxy: proxy_config,
        tls_backend: config.tls_backend,
    });

    let cache_dir = token_manager
        .cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    Ok(Foundation {
        config,
        token_manager,
        kiro_provider,
        endpoint_names,
        configured_api_key,
        cache_dir,
    })
}

fn ensure_config_files(config_path: &str, credentials_path: &str) {
    let config_p = std::path::Path::new(config_path);
    if !config_p.exists() {
        if let Some(parent) = config_p.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!("创建配置目录失败 {}: {}", parent.display(), e);
                }
            }
        }
        let api_key = format!("sk-kiro-rs-{}", random_token(24));
        let admin_api_key = format!("sk-admin-{}", random_token(24));
        let default = serde_json::json!({
            "host": "0.0.0.0",
            "port": 8990,
            "apiKey": api_key,
            "adminApiKey": admin_api_key,
            "region": "us-east-1",
            "tlsBackend": "rustls",
            "defaultEndpoint": "ide"
        });
        match serde_json::to_string_pretty(&default)
            .map_err(anyhow::Error::from)
            .and_then(|s| std::fs::write(config_p, s).map_err(anyhow::Error::from))
        {
            Ok(_) => {
                tracing::info!("已生成默认配置: {}", config_p.display());
                tracing::info!("  apiKey      = {}（每次启动时同步为系统 Key）", api_key);
                tracing::info!("  adminApiKey = {}（管理面板登录密钥）", admin_api_key);
                tracing::info!("请妥善保存上述密钥，可在配置文件中修改");
            }
            Err(e) => tracing::warn!("写入默认配置失败 {}: {}", config_p.display(), e),
        }
    }

    let cred_p = std::path::Path::new(credentials_path);
    if !cred_p.exists() {
        if let Some(parent) = cred_p.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!("创建凭证目录失败 {}: {}", parent.display(), e);
                }
            }
        }
        if let Err(e) = std::fs::write(cred_p, "[]\n") {
            tracing::warn!("写入空凭证文件失败 {}: {}", cred_p.display(), e);
        } else {
            tracing::info!(
                "已生成空凭证文件: {}（可通过 Admin UI 添加凭据）",
                cred_p.display()
            );
        }
    }
}

/// 生成一段长度为 `len` 的字母数字随机字符串，用于默认 API Key
fn random_token(len: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    (0..len)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 装配的第二段产物：计费、用量、分组、网关、trace、缓存计量。
///
/// **过渡接口**，与 [`Foundation`] 同理：A7 后收回 `pub(crate)`。
pub struct Accounting {
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
pub async fn accounting(
    base: &Foundation,
    options: &Options,
) -> anyhow::Result<Accounting> {
    let config = &base.config;
    let cache_dir = &base.cache_dir;
    let token_manager = &base.token_manager;
    let configured_api_key = &base.configured_api_key;
    let _ = options;

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

/// 配置端口被占用时换一个。
///
/// 桌面端尤其需要：用户可能同时开着一个命令行实例。回退是**明确记录**的，
/// 不是悄悄换掉——调用方拿 `addr()` 得到真实地址，日志里也如实写出来。
///
/// 只对 `AddrInUse` 回退。权限不足、地址不存在这类错误原样上抛，
/// 换个端口并不能解决它们，掩盖只会让人查错方向。
async fn bind_with_fallback(spec: SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(spec).await {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let fallback = SocketAddr::new(spec.ip(), 0);
            let listener = tokio::net::TcpListener::bind(fallback)
                .await
                .map_err(|e2| anyhow::anyhow!("端口 {} 被占用，回退也失败: {e2}", spec.port()))?;
            tracing::warn!("端口 {} 已被占用，改用 {}", spec.port(), listener.local_addr()?);
            Ok(listener)
        }
        Err(e) => Err(anyhow::anyhow!("监听 {spec} 失败: {e}")),
    }
}

/// 装配整个应用。装配失败返回 `Err`——调用方决定怎么死。
///
/// 目前只到「读配置、定地址」。凭据池、端点、网关与路由在 A4–A7 逐步搬入。
async fn assemble(options: &Options) -> anyhow::Result<(Router, SocketAddr)> {
    let base = foundation(options)?;
    let _books = accounting(&base, options).await?;

    let addr: SocketAddr = format!("{}:{}", base.config.host, base.config.port)
        .parse()
        .map_err(|e| {
            anyhow::anyhow!(
                "监听地址无法解析 {}:{}: {e}",
                base.config.host,
                base.config.port
            )
        })?;

    Ok((Router::new(), addr))
}
