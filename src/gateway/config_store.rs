//! 网关配置的持久化与运行期快照。
//!
//! 配置单独存在 `gateway.json`，与主 `config.json` 并列而不是塞进去：主配置由既有的
//! 凭据 / 管线写入方各自改写，把带密钥的网关配置混进同一个文件，任何一方的整体写回都
//! 可能把另一方的改动覆盖掉。
//!
//! # 几条不肯让步的性质
//!
//! - **文件缺失才用默认值**；文件存在但无法解析或校验不过，是**启动失败**，不是退回空
//!   配置。一个"配置有问题所以当作没有上游"的网关会静悄悄拒绝全部流量，比起不启动更难
//!   排查，而且掩盖了真正的错误。
//! - **持久化失败不得改动运行期快照**。先落盘、再换快照；反过来会出现"接口说保存成功、
//!   重启后配置消失"。
//! - **乐观版本在锁内校验**，避免两个并发写入各自基于旧版本、后者悄悄覆盖前者。
//! - **对外视图去密钥**；更新时省略 `apiKey` 表示保持原值，而不是清空——否则任何一次
//!   从脱敏视图出发的保存都会把密钥抹掉。
//! - **只有会让绑定失效的变更才提升路由代际**。调权重不属于此列：那会把正在进行的会话
//!   全部踢走，而权重变化并不意味着旧选择是错的。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::{GatewayConfig, Upstream};

/// 运行期快照：不可变、可共享。
#[derive(Clone)]
pub struct ConfigSnapshot {
    pub revision: u64,
    pub config: Arc<GatewayConfig>,
}

/// 一次成功更新的结果。
///
/// 可以安全派生 `Debug`：只有版本号和一个布尔值，不含配置也不含密钥。
/// [`ConfigStore`] 本身**不派生** `Debug`——它持有带 `apiKey` 的配置，
/// 一个顺手的 `dbg!` 或 `{:?}` 日志就会把密钥打出来。
#[derive(Debug)]
pub struct UpdateOutcome {
    pub revision: u64,
    /// 本次变更是否让既有粘性绑定失效。
    pub invalidates_routes: bool,
}

struct Stored {
    revision: u64,
    config: Arc<GatewayConfig>,
}

pub struct ConfigStore {
    path: PathBuf,
    state: Mutex<Stored>,
}

impl ConfigStore {
    /// 打开配置。文件缺失即使用默认值；存在但不合法则失败，不退回空配置。
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let config = match fs::read(&path) {
            Ok(bytes) => {
                let config: GatewayConfig = serde_json::from_slice(&bytes).with_context(|| {
                    format!(
                        "{} 无法解析；请修正后再启动，网关不会退回空配置继续运行",
                        path.display()
                    )
                })?;
                config.validate().with_context(|| {
                    format!("{} 未通过校验；网关不会带着无效配置启动", path.display())
                })?;
                config
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => GatewayConfig::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("读取 {} 失败", path.display()));
            }
        };
        Ok(Self {
            path,
            state: Mutex::new(Stored {
                revision: 1,
                config: Arc::new(config),
            }),
        })
    }

    pub fn snapshot(&self) -> ConfigSnapshot {
        let state = self.state.lock();
        ConfigSnapshot {
            revision: state.revision,
            config: Arc::clone(&state.config),
        }
    }

    /// 去密钥视图：`apiKey` 不出现，改以 `hasApiKey` 表示是否已配置。
    pub fn redacted(&self) -> Result<Value> {
        let snapshot = self.snapshot();
        let mut value = serde_json::to_value(snapshot.config.as_ref())?;
        if let Some(upstreams) = value.get_mut("upstreams").and_then(Value::as_array_mut) {
            for (index, upstream) in upstreams.iter_mut().enumerate() {
                let configured = snapshot
                    .config
                    .upstreams
                    .get(index)
                    .and_then(|u| u.api_key.as_ref())
                    .is_some_and(|key| !key.is_empty());
                if let Some(object) = upstream.as_object_mut() {
                    object.remove("apiKey");
                    object.insert("hasApiKey".into(), Value::Bool(configured));
                }
            }
        }
        Ok(json!({ "revision": snapshot.revision, "config": value }))
    }

    /// 更新配置。
    ///
    /// `incoming` 里省略某个上游的 `apiKey` 表示沿用原值；要清除必须显式给空字符串。
    /// 校验 → 落盘 → 换快照，顺序不可颠倒：落盘失败时运行期必须保持原样。
    pub fn update(
        &self,
        expected_revision: u64,
        mut incoming: GatewayConfig,
    ) -> Result<UpdateOutcome> {
        // 整个「比对版本 → 写文件 → 换快照」必须在同一把锁内，否则两个并发更新会各自
        // 基于同一个旧版本通过校验，后写者悄悄覆盖前写者。
        let mut state = self.state.lock();
        ensure!(
            expected_revision == state.revision,
            "configuration_conflict: expected revision {expected_revision}, current is {}",
            state.revision
        );

        carry_over_secrets(&mut incoming, &state.config);
        incoming.validate()?;

        let invalidates_routes = invalidates_routes(&state.config, &incoming);
        atomic_write(&self.path, &incoming)?;

        state.revision += 1;
        state.config = Arc::new(incoming);
        Ok(UpdateOutcome {
            revision: state.revision,
            invalidates_routes,
        })
    }
}

/// 省略的 `apiKey` 沿用原值；显式空字符串表示清除。
fn carry_over_secrets(incoming: &mut GatewayConfig, current: &GatewayConfig) {
    for upstream in &mut incoming.upstreams {
        match upstream.api_key.as_deref() {
            None => {
                upstream.api_key = current
                    .upstreams
                    .iter()
                    .find(|existing| existing.id == upstream.id)
                    .and_then(|existing| existing.api_key.clone());
            }
            Some("") => upstream.api_key = None,
            Some(_) => {}
        }
        upstream.has_api_key = upstream.api_key.as_ref().is_some_and(|k| !k.is_empty());
    }
}

/// 本次变更是否让既有粘性绑定失效。
///
/// 判据是「绑定的含义是否变了」：
/// - 路由模式变了 → 选择语义变了；
/// - 上游的去向或安全边界变了（kind / baseUrl / 私网放行 / 密钥）→ 同一个绑定现在指向
///   的其实是另一个东西；
/// - 绑定改挂到别的上游 → 同理。
///
/// **权重、名称、展示名、价格、超时、重试次数都不在此列。**调权重踢掉全部在途会话是
/// 一种代价高昂且没有必要的副作用。
fn invalidates_routes(current: &GatewayConfig, incoming: &GatewayConfig) -> bool {
    if current.default_routing_mode != incoming.default_routing_mode {
        return true;
    }
    for model in &incoming.models {
        let Some(previous) = current.models.iter().find(|m| m.id == model.id) else {
            continue;
        };
        if previous.routing_mode != model.routing_mode {
            return true;
        }
        for binding in &model.bindings {
            if let Some(before) = previous.bindings.iter().find(|b| b.id == binding.id)
                && before.upstream_id != binding.upstream_id
            {
                return true;
            }
        }
    }
    for upstream in &incoming.upstreams {
        let Some(previous) = current.upstreams.iter().find(|u| u.id == upstream.id) else {
            continue;
        };
        if security_identity(previous) != security_identity(upstream) {
            return true;
        }
    }
    false
}

/// 决定「这个上游还是不是原来那个」的字段集合。
fn security_identity(upstream: &Upstream) -> (super::UpstreamKind, Option<&str>, bool, bool) {
    (
        upstream.kind,
        upstream.base_url.as_deref(),
        upstream.allow_private_network,
        upstream.api_key.is_some(),
    )
}

/// 原子写入，权限 0600。
///
/// 文件带密钥，所以新建时就必须是 0600，而不是先以默认权限创建再改——那中间存在一个
/// 其他用户可读的窗口。已存在时沿用其权限，避免把运维刻意收紧过的权限放宽。
fn atomic_write(path: &Path, config: &GatewayConfig) -> Result<()> {
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&directory)
        .with_context(|| format!("创建配置目录 {} 失败", directory.display()))?;

    // 已存在时跟随符号链接，替换目标本身而不是链接。
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(metadata) = fs::metadata(&target)
        && !metadata.is_file()
    {
        bail!("{} 不是普通文件，拒绝写入", target.display());
    }

    let temp = directory.join(format!(".kiro-gateway-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .with_context(|| format!("创建临时文件 {} 失败", temp.display()))?;

    let result = (|| -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(config)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        // 已存在则保留原权限；新建保持 0600。
        if let Ok(metadata) = fs::metadata(&target) {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &target)?;
        fs::File::open(&directory).and_then(|d| d.sync_all())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.context("网关配置持久化失败；运行期配置保持不变")
}

#[cfg(test)]
#[path = "config_store_tests.rs"]
mod tests;
