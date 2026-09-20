//! 装配第一段：配置、凭据池、端点注册、provider。
//!
//! 这一段里有三处原本 `std::process::exit(1)`，全部改成返回 `Err`。
//! 详见 [`foundation`] 的文档。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::kiro::endpoint::{CliEndpoint, IdeEndpoint, KiroEndpoint};
use crate::kiro::model::credentials::{CredentialsConfig, KiroCredentials};
use crate::kiro::provider::KiroProvider;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::Config;
use crate::{http_client, kiro, model, token};

use super::Options;

/// 装配的第一段产物：配置、凭据池、provider。
///
/// 装配内部的交接结构，不是对外 API——`serve()` 才是。
pub(crate) struct Foundation {
    pub config: Config,
    pub token_manager: Arc<MultiTokenManager>,
    pub kiro_provider: Arc<KiroProvider>,
    pub endpoint_names: Vec<String>,
    pub configured_api_key: Option<String>,
    /// 首次初始化状态。未初始化时持有一次性 setup token。
    pub setup: Arc<crate::admin::setup::SetupState>,
    /// 所有运行期文件的落脚点：客户端 Key、用量日志、分组、账本、trace、缓存计量。
    /// 取的是凭据文件的父目录——只要调用方传绝对路径，这些文件就不会落到 CWD。
    pub cache_dir: PathBuf,
}

/// 装配第一段：读配置与凭据、注册端点、建 token 管理器与 provider。
///
/// 三处原本 `std::process::exit(1)` 的地方现在返回 `Err`：配置加载、凭据加载、
/// Token 管理器创建。**致命性没有变**——只是把「终止进程」换成「返回错误」，
/// 由调用方决定怎么死。库不能替调用方做这个决定。
pub(crate) fn foundation(options: &Options) -> anyhow::Result<Foundation> {
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

    let setup = Arc::new(crate::admin::setup::SetupState::from_configured_key(
        config.admin_api_key.as_deref(),
    ));

    Ok(Foundation {
        config,
        setup,
        token_manager,
        kiro_provider,
        endpoint_names,
        configured_api_key,
        cache_dir,
    })
}

/// 文件不存在时初始化配置/凭证文件。二进制形态用，绑 `0.0.0.0` 适配容器。
fn ensure_config_files(config_path: &str, credentials_path: &str) {
    ensure_config_files_with_host(config_path, credentials_path, "0.0.0.0");
}

/// 文件不存在时初始化配置/凭证文件，`host` 由调用方决定。
///
/// 桌面端传 `127.0.0.1`：它没有「让局域网里的别人连过来」这个需求，
/// 而绑 `0.0.0.0` 会把一个带着凭据的代理暴露到局域网。
///
/// 两种形态共用这一份实现，避免各写一份后默认值与权限策略漂移。
/// 任一步失败都仅打印警告，不中断启动；后续 `Config::load` /
/// `CredentialsConfig::load` 仍会按既有逻辑处理（失败再退出）。
pub fn ensure_config_files_with_host(config_path: &str, credentials_path: &str, host: &str) {
    let config_p = std::path::Path::new(config_path);
    if !config_p.exists() {
        if let Some(parent) = config_p.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!("创建配置目录失败 {}: {}", parent.display(), e);
                }
            }
        }
        // apiKey 照旧生成：它是**客户端**凭据，客户端要能立刻用起来。
        //
        // adminApiKey 刻意不生成：管理权要由用户自己认领（设自己的密码），
        // 凭控制台打印的一次性 setup token。一个用户既没设过也记不住的
        // 随机串，最后的下场是被翻出来抄一遍、或者干脆一直不换。
        let api_key = format!("sk-kiro-rs-{}", random_token(24));
        let default = serde_json::json!({
            "host": host,
            "port": 8990,
            "apiKey": api_key,
            "region": "us-east-1",
            "tlsBackend": "rustls",
            "defaultEndpoint": "ide"
        });
        match serde_json::to_string_pretty(&default)
            .map_err(anyhow::Error::from)
            .and_then(|s| crate::common::fs::write_private(config_p, s.as_bytes()).map_err(anyhow::Error::from))
        {
            Ok(_) => {
                tracing::info!("已生成默认配置: {}", config_p.display());
                tracing::info!("  apiKey = {}（客户端用，同时同步为系统 Key）", api_key);
                // 管理密码不在这里给——它由用户在初始化页自己设，
                // 凭启动横幅里的 setup token。
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
        if let Err(e) = crate::common::fs::write_private(cred_p, b"[]\n") {
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
