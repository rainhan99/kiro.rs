//! 一笔已预留、将由**既有 Kiro 通道**执行的请求的结算句柄。
//!
//! # 为什么 Kiro 路不由网关自己发请求
//!
//! Kiro 的用量真相只有一个来源：原生 `metadataEvent.tokenUsage`。既有路径已经在
//! 正确地提取它，并且顺带做着凭据轮换、流水线、工件循环、trace 与用量统计。网关若
//! 自己再解析一遍事件流来取用量，就等于给「只有原生用量算证据」这条不变量造第二套
//! 实现——两套迟早对不上，而对不上的那一刻没人会发现。
//!
//! 所以 Kiro 路的分工是：**网关选路并预留，既有通道执行，用量汇聚点结算**。
//!
//! # 没等到结算不等于没发生
//!
//! 如果执行途中进程没了，这条预留留在 in-flight，下次启动由 `recover_inflight`
//! 转成待结算。它绝不会被当作没发生过释放掉。

use std::sync::Arc;

use anyhow::Result;

use super::ledger_types::{AttemptCost, AttemptOutcome, EvidenceStatus, SettlementInput};
use super::service::GatewayService;
use super::{Amount, BillingUnit};

/// 把上游下发的原生 credit 转成账本定点数。
///
/// # 为什么不能用 [`super::import::to_amount`]
///
/// 那一个处理的是遗留侧**累加出来**的总量，末几位是反复相加的浮点噪声，所以它
/// 取 12 位小数把噪声确定地丢掉。这里是上游一次下发的**原值**：`0.0169543708291874`
/// 这样的数字，f64 的最短往返表示就是上游给的那串数字本身，截到 12 位等于少收钱，
/// 也违背「原生 credit 必须逐位往返」这条不变量。
///
/// 表示不了就报错，由调用方记为待结算——**绝不用 0 顶替**。
pub fn credits_to_amount(value: f64) -> Result<Amount> {
    anyhow::ensure!(value.is_finite(), "native credit figure is not finite");
    if value == 0.0 {
        // 上游确认这次没有计费。这与"不知道花了多少"不同，后者根本到不了这里。
        return Ok(Amount::ZERO);
    }
    anyhow::ensure!(value > 0.0, "native credit figure is negative");
    // `{}` 是 f64 的最短往返表示：能还原出同一个 f64 的最短十进制串，
    // 也就是上游那串数字。Rust 的 Display 不会用科学计数法。
    format!("{value}").parse()
}

/// 一次尝试的结算句柄。可跨线程持有，直到执行结束。
pub struct Settlement {
    service: Arc<GatewayService>,
    attempt_id: String,
    unit: BillingUnit,
}

impl Settlement {
    pub fn new(service: Arc<GatewayService>, attempt_id: String, unit: BillingUnit) -> Self {
        Self {
            service,
            attempt_id,
            unit,
        }
    }

    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    /// 按原生积分结算一次成功的调用。
    ///
    /// `credits` 是上游 `meteringEvent` 下发的真实计费量。拿不到（`None`）时记为
    /// 待结算，**绝不用 0 顶替**——0 的意思是"确认没花钱"，与"不知道花了多少"
    /// 是两回事。
    pub fn succeeded(
        &self,
        credits: Option<Amount>,
        usage: Option<serde_json::Value>,
    ) -> Result<()> {
        self.write(credits, usage, true, AttemptOutcome::Succeeded)
    }

    /// 结算一次失败的调用。
    ///
    /// `committed` 表示是否已经向下游发出过内容。已提交的失败保留义务；
    /// 未提交的失败释放预留——它确实没产生任何下游承诺。
    pub fn failed(&self, committed: bool) -> Result<()> {
        self.write(None, None, committed, AttemptOutcome::Failed)?;
        if !committed {
            self.ledger()?.release(&self.attempt_id)?;
        }
        Ok(())
    }

    fn write(
        &self,
        credits: Option<Amount>,
        usage: Option<serde_json::Value>,
        committed: bool,
        outcome: AttemptOutcome,
    ) -> Result<()> {
        // 与 `Coordinator::settle` 同一条规则：有确认金额才算 confirmed。
        let evidence = if credits.is_some() {
            EvidenceStatus::Confirmed
        } else {
            EvidenceStatus::Pending
        };
        self.ledger()?.settle(SettlementInput {
            settlement_id: format!("settle:{}", self.attempt_id),
            attempt_id: self.attempt_id.clone(),
            evidence,
            downstream_amount: committed.then_some(credits).flatten(),
            attempt_cost: AttemptCost {
                unit: self.unit,
                evidence,
                amount: credits,
            },
            committed,
            outcome,
            usage,
        })?;
        Ok(())
    }

    fn ledger(&self) -> Result<&super::ledger::Ledger> {
        self.service
            .ledger()
            .ok_or_else(|| anyhow::anyhow!("gateway has no ledger to settle against"))
    }
}

/// 选中了一条 Kiro 路：调用方走既有 Kiro 通道执行，并带上这些**请求级**覆盖。
///
/// 覆盖逐请求传递，绝不改任何全局开关——同一进程里并发的两个请求可以走不同的
/// 模型与分组。
#[derive(Clone)]
pub struct KiroRoute {
    /// 这一路对应的绑定 id。Kiro 失败后要换路时，得知道排除掉哪一条。
    pub binding_id: String,
    /// 这一路的上游真实模型名，替换客户端说的公开别名。
    pub upstream_model: String,
    /// 这一路使用的凭据分组。`None` 表示不限，沿用 Key 自己的绑定。
    pub group: Option<String>,
    /// 是否允许 Kiro 的会话粘性把凭据钉到会话上。
    ///
    /// 网关按权重随机分发时为 `false`：粘性会让第一次选中的凭据接管整个会话，
    /// 于是"随机"只在第一次生效，之后每一轮都落在同一个凭据上。
    pub sticky: bool,
    pub settlement: Arc<Settlement>,
}

impl KiroRoute {
    /// 把请求级覆盖施加到这一次请求上。
    ///
    /// 两者都必须换：只换模型不换分组，请求会拿着这一路的模型去另一组凭据上发；
    /// 只换分组不换模型，上游收到的是它不认识的公开别名。
    pub fn apply(&self, model: &mut String, group: &mut Option<String>) {
        *model = self.upstream_model.clone();
        *group = self.group.clone();
    }
}

#[cfg(test)]
#[path = "settlement_tests.rs"]
mod tests;
