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
use crate::{http_client, kiro, model, token};

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
