//! 网关运行期装配：配置快照、路由、账本三者的持有者。
//!
//! # 未配置时必须与改造前逐字节一致
//!
//! 这是本模块第一条、也是最重要的性质。`gateway.json` 缺失或没有声明任何公开模型时，
//! [`GatewayService::plan_for`] 对任何别名都返回 `None`，调用方据此原样走既有路径。
//! 一个"顺便改变了既有行为"的集成是不可接受的：现有部署没有要求过这个特性。
//!
//! # 账本的失败是启动失败——但只在真的要用它的时候
//!
//! 记不了账就不能做收费的事，所以账本打不开必须 fail-closed。但若网关本身是惰性的
//! （没有任何上游与模型），就没有账要记，此时强行要求一个可写的 `billing.db` 会把
//! 与本特性无关的既有部署挡在门外。因此：**配置里声明了东西才要求账本能开**。
//!
//! # 每请求快照
//!
//! 一次请求的模型映射、资费、路由模式与重试期限在开始时一次冻结。运行中改配置不影响
//! 已经在飞的请求，而下一个请求会看到新配置——否则一次请求可能按 A 计价、按 B 路由。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use super::config_store::{ConfigSnapshot, ConfigStore};
use super::import::{ImportReport, LegacyKeyBalance, import_opening_balances};
use super::ledger::Ledger;
use super::routing::{Candidate, RoutingEngine};
use super::{GatewayConfig, ModelBinding, PublicModel, RoutingMode};

/// 一次请求冻结下来的全部决策依据。
pub struct RequestPlan {
    /// 冻结时的配置版本，便于把证据与当时的配置对上。
    pub revision: u64,
    pub alias: String,
    /// 路由模式：模型级覆盖优先于全局默认。
    pub mode: RoutingMode,
    /// 冻结时的路由代际。成功后发布绑定要用它校验。
    pub generation: u64,
    /// 本次请求的总期限。
    pub deadline: Duration,
    pub max_attempts: u32,
    /// 该别名下的全部候选（尚未按账户预算过滤——那是调用方的事）。
    pub candidates: Vec<Candidate>,
    /// 候选 id → 绑定定义，供计价与上游查找。
    pub bindings: Vec<ModelBinding>,
    /// 冻结时的完整配置，供上游查找与资费读取。
    pub config: Arc<GatewayConfig>,
}

impl RequestPlan {
    pub fn binding(&self, binding_id: &str) -> Option<&ModelBinding> {
        self.bindings.iter().find(|b| b.id == binding_id)
    }

    pub fn upstream(&self, upstream_id: &str) -> Option<&super::Upstream> {
        self.config.upstreams.iter().find(|u| u.id == upstream_id)
    }
}

/// 启动收尾的如实结果。
#[derive(Debug, Default, PartialEq)]
pub struct StartupAdoption {
    /// 上个进程遗留、本次转成待结算的预留笔数。
    pub recovered_in_flight: u64,
    pub import: ImportReport,
}

pub struct GatewayService {
    config: ConfigStore,
    routing: RoutingEngine,
    ledger: Option<Ledger>,
}

impl GatewayService {
    /// 装配网关。
    ///
    /// `gateway.json` 缺失即为惰性状态；存在但不合法则启动失败（见 [`ConfigStore::open`]）。
    /// 仅当配置真的声明了上游或模型时才要求账本可用——记不了账就不能收费，
    /// 但一个惰性的网关没有账要记。
    pub fn open(config_path: &Path, ledger_path: &Path) -> Result<Self> {
        let config = ConfigStore::open(config_path)?;
        let snapshot = config.snapshot();
        let ledger = if snapshot.config.upstreams.is_empty() && snapshot.config.models.is_empty() {
            None
        } else {
            Some(Ledger::open(ledger_path).with_context(|| {
                format!(
                    "网关已配置上游或模型，但账本 {} 无法打开；记不了账就不能收费，因此拒绝启动",
                    ledger_path.display()
                )
            })?)
        };
        let routing = RoutingEngine::new(Duration::from_secs(snapshot.config.affinity_ttl_secs));
        Ok(Self {
            config,
            routing,
            ledger,
        })
    }

    /// 启动时的一次性收尾，在开始接受请求**之前**做完。
    ///
    /// 两件事都只在账本存在时发生。惰性网关没有账本，这里必须是彻底的空操作——
    /// 未配置网关的部署不因为引入这个特性而改变任何行为。
    ///
    /// 一、认领上个进程遗留的在飞预留。预留先于发送，所以崩溃时账本上会留下一条
    /// in-flight 记录；它代表一次**可能已经发生**的消耗，转成待结算等人工或证据
    /// 裁决，而不是当作没发生过释放掉。
    ///
    /// 二、为尚未立户的遗留 Key 建立积分开账余额（见 [`super::import`]）。
    pub fn adopt_legacy_state(&self, keys: &[LegacyKeyBalance]) -> Result<StartupAdoption> {
        let Some(ledger) = self.ledger.as_ref() else {
            return Ok(StartupAdoption::default());
        };
        Ok(StartupAdoption {
            recovered_in_flight: ledger.recover_inflight()?,
            import: import_opening_balances(ledger, keys)?,
        })
    }

    /// 账本里出现过的最大 key id。惰性网关下为 `None`。
    ///
    /// 用来防止已删 Key 的 id 被重新分配——那会把前一个同 id Key 的余额与历史
    /// 交给一个毫无关系的新 Key。
    pub fn highest_recorded_key_id(&self) -> Result<Option<u64>> {
        match self.ledger.as_ref() {
            Some(ledger) => ledger.max_recorded_key_id(),
            None => Ok(None),
        }
    }

    pub fn snapshot(&self) -> ConfigSnapshot {
        self.config.snapshot()
    }

    pub fn routing(&self) -> &RoutingEngine {
        &self.routing
    }

    /// 账本。惰性网关下为 `None`——此时不存在需要记账的请求。
    pub fn ledger(&self) -> Option<&Ledger> {
        self.ledger.as_ref()
    }

    pub fn config_store(&self) -> &ConfigStore {
        &self.config
    }

    /// 配置里是否真的声明了东西。
    ///
    /// 与 [`Self::has_managed_models`] 不同：一份声明了上游与模型、但全部停用的配置
    /// **是**已配置的（运维正是要在管理面里把它们打开），只是暂时没接管任何别名。
    pub fn is_configured(&self) -> bool {
        let snapshot = self.config.snapshot();
        !snapshot.config.upstreams.is_empty() || !snapshot.config.models.is_empty()
    }

    /// 网关有没有接管任何模型。入口据此决定要不要缓冲请求体——
    /// 未配置网关的部署不该为一个用不上的特性付出缓冲代价。
    pub fn has_managed_models(&self) -> bool {
        let snapshot = self.config.snapshot();
        snapshot.config.models.iter().any(|model| {
            model.bindings.iter().any(|binding| {
                binding.enabled
                    && snapshot
                        .config
                        .upstreams
                        .iter()
                        .any(|u| u.id == binding.upstream_id && u.enabled)
            })
        })
    }

    /// 网关是否接管了这个别名。
    ///
    /// 只有既定义了该公开模型、又至少有一个**启用的**绑定，才算接管。一个定义了却全部
    /// 停用的模型不该把请求从既有路径上劫走——那会让它直接失败，而不是回到原来能用的路。
    pub fn is_managed(&self, alias: &str) -> bool {
        let snapshot = self.config.snapshot();
        managed_model(&snapshot.config, alias).is_some()
    }

    /// 为一次请求冻结决策依据。未接管时返回 `None`，调用方据此走既有路径。
    pub fn plan_for(&self, alias: &str) -> Option<RequestPlan> {
        let snapshot = self.config.snapshot();
        let model = managed_model(&snapshot.config, alias)?;
        let mode = model
            .routing_mode
            .unwrap_or(snapshot.config.default_routing_mode);

        let bindings: Vec<ModelBinding> = model.bindings.clone();
        let candidates = bindings
            .iter()
            .map(|binding| {
                let upstream = snapshot
                    .config
                    .upstreams
                    .iter()
                    .find(|u| u.id == binding.upstream_id);
                Candidate {
                    binding_id: binding.id.clone(),
                    upstream_id: binding.upstream_id.clone(),
                    priority_tier: binding.priority_tier,
                    weight: binding.weight,
                    // 上游本身停用时，挂在它下面的绑定也不可用——否则会选中一个
                    // 明确被运维关掉的去处。
                    enabled: binding.enabled && upstream.is_some_and(|u| u.enabled),
                    supports_tools: binding.supports_tools,
                    supports_images: binding.supports_images,
                    supports_reasoning: binding.supports_reasoning,
                }
            })
            .collect();

        Some(RequestPlan {
            revision: snapshot.revision,
            alias: alias.to_string(),
            mode,
            generation: self.routing.generation(),
            deadline: Duration::from_secs(snapshot.config.request_timeout_secs),
            max_attempts: snapshot.config.max_attempts,
            candidates,
            bindings,
            config: Arc::clone(&snapshot.config),
        })
    }

    /// 应用一次配置更新，并在必要时作废粘性绑定。
    ///
    /// 只有更新本身判定"绑定含义变了"才作废；调权重不会（见 `config_store`）。
    pub fn update_config(
        &self,
        expected_revision: u64,
        incoming: GatewayConfig,
    ) -> Result<super::config_store::UpdateOutcome> {
        let ttl = Duration::from_secs(incoming.affinity_ttl_secs);
        let outcome = self.config.update(expected_revision, incoming)?;
        self.routing.set_ttl(ttl);
        if outcome.invalidates_routes {
            self.routing.invalidate_bindings();
        }
        Ok(outcome)
    }

    /// 对外暴露的公开模型列表，能力如实来自绑定声明。
    ///
    /// 能力取各**启用**绑定的并集：只要有一条路支持工具，这个别名就支持工具。
    /// 取交集会把一个实际可用的能力报成不可用。
    pub fn public_models(&self) -> Vec<PublicModelCapabilities> {
        let snapshot = self.config.snapshot();
        snapshot
            .config
            .models
            .iter()
            .filter_map(|model| {
                let enabled: Vec<&ModelBinding> = model
                    .bindings
                    .iter()
                    .filter(|b| {
                        b.enabled
                            && snapshot
                                .config
                                .upstreams
                                .iter()
                                .any(|u| u.id == b.upstream_id && u.enabled)
                    })
                    .collect();
                if enabled.is_empty() {
                    return None;
                }
                Some(PublicModelCapabilities {
                    id: model.id.clone(),
                    display_name: model.display_name.clone(),
                    // 上下文窗口取各路中最小者：报一个只有部分路径能满足的数字，
                    // 会让客户端把请求构造到别的路承受不了的大小。
                    context_window: enabled.iter().map(|b| b.context_window).min().unwrap_or(0),
                    max_output_tokens: enabled
                        .iter()
                        .map(|b| b.max_output_tokens)
                        .min()
                        .unwrap_or(0),
                    supports_tools: enabled.iter().any(|b| b.supports_tools),
                    supports_images: enabled.iter().any(|b| b.supports_images),
                    supports_reasoning: enabled.iter().any(|b| b.supports_reasoning),
                })
            })
            .collect()
    }
}

/// 对外的公开模型能力。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicModelCapabilities {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub supports_tools: bool,
    pub supports_images: bool,
    pub supports_reasoning: bool,
}

/// 找到被接管的模型：定义存在，且至少有一个启用的绑定挂在一个启用的上游上。
fn managed_model<'a>(config: &'a GatewayConfig, alias: &str) -> Option<&'a PublicModel> {
    config.models.iter().find(|model| {
        model.id == alias
            && model.bindings.iter().any(|binding| {
                binding.enabled
                    && config
                        .upstreams
                        .iter()
                        .any(|u| u.id == binding.upstream_id && u.enabled)
            })
    })
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
