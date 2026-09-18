//! 一次请求的协调状态机：预留 → 发送 → 结算。
//!
//! 本模块**不发网络请求**。它决定下一步做什么、并在账本上留下相应的持久状态；实际的
//! 发送由调用方完成后把结果交回来。这样这套顺序与边界可以完全离线验证。
//!
//! # 两条不能让步的规则
//!
//! **一、向下游发出过事件之后，绝不换供应商。** 换供应商意味着把两个不同模型的输出拼接
//! 成一个响应——客户端收到的是一段前后不属于同一次推理的文本，而且它无从察觉。所以
//! "已提交"之后只能如实报告中断，不再重试、不再换路。
//!
//! **二、被丢弃的尝试不得让费用凭空消失。** 预留之后如果进程崩溃或请求被取消，账本上
//! 那条记录保持 in-flight，下次启动由 `recover_inflight` 转成 pending，而不是被悄悄
//! release 掉。release 只用于**已确认未产生任何下游承诺**的失败尝试。

use std::time::{Duration, Instant};

use anyhow::{Result, ensure};

use super::admission::{CandidateVerdict, evaluate};
use super::ledger::Ledger;
use super::ledger_types::{
    AttemptCost, AttemptOutcome, EvidenceStatus, PriceSnapshot, ReservationInput, SettlementInput,
};
use super::routing::{RouteContext, RouteSelection, RoutingEngine};
use super::service::RequestPlan;
use super::{Amount, BillingUnit};

/// 一次已预留、可以发送的尝试。
pub struct ReservedAttempt {
    pub attempt_id: String,
    pub binding_id: String,
    pub upstream_id: String,
    pub upstream_model: String,
    pub unit: BillingUnit,
    /// 冻结时的路由代际。成功后发布粘性绑定要用它校验。
    pub generation: u64,
}

/// 尝试之后调用方回报的结果。
pub enum AttemptResult {
    /// 上游成功。`cost` 为确认的下游计费额；证据不完整时为 `None`（保持 pending）。
    Succeeded {
        cost: Option<Amount>,
        usage: Option<serde_json::Value>,
    },
    /// 失败，且**尚未**向下游发出任何事件——可以换一条路重试。
    FailedBeforeCommitment { retryable: bool },
    /// 已经向下游发出过事件之后才中断。不得换供应商。
    InterruptedAfterCommitment {
        cost: Option<Amount>,
        usage: Option<serde_json::Value>,
    },
}

/// 协调器给出的下一步。
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// 可以再试一次（调用方应再次 `begin_attempt`）。
    Retry,
    /// 结束。
    Stop(StopReason),
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopReason {
    Succeeded,
    /// 已向下游提交过内容，只能如实报告中断。
    InterruptedAfterCommitment,
    /// 尝试次数用尽。
    AttemptsExhausted,
    /// 期限已到。
    DeadlineReached,
    /// 失败且不可重试（请求本身有问题，换路也不会成功）。
    NotRetryable,
    /// 没有任何可用候选。
    NoEligibleRoute,
}

pub struct Coordinator<'a> {
    ledger: &'a Ledger,
    routing: &'a RoutingEngine,
    plan: &'a RequestPlan,
    key_id: u64,
    request_id: String,
    started_at: Instant,
    attempts_used: u32,
    /// 本次请求中已经失败过的绑定，不再重复选中。
    exhausted: Vec<String>,
    /// 是否已经向下游发出过事件。一旦为真，永不重试。
    committed: bool,
}

impl<'a> Coordinator<'a> {
    pub fn new(
        ledger: &'a Ledger,
        routing: &'a RoutingEngine,
        plan: &'a RequestPlan,
        key_id: u64,
        request_id: impl Into<String>,
    ) -> Self {
        Self {
            ledger,
            routing,
            plan,
            key_id,
            request_id: request_id.into(),
            started_at: Instant::now(),
            attempts_used: 0,
            exhausted: Vec::new(),
            committed: false,
        }
    }

    pub fn attempts_used(&self) -> u32 {
        self.attempts_used
    }

    /// 选路并预留。**预留先于发送**——先发后记账意味着一次崩溃就丢掉一笔费用。
    ///
    /// 返回 `None` 表示没有可用路线（全部被准入拒绝或已在本次请求中失败过）。
    pub fn begin_attempt(&mut self, ctx: &RouteContext) -> Result<Option<ReservedAttempt>> {
        // 与账本自己的「request already committed」构成两道守卫，两道都要。
        // 这一道在**碰账本之前**就失败，错误信息直指原因；账本那道在更底层，
        // 即便有人绕过协调器直接预留也拦得住。缺了这一道，症状会变成一个
        // 底层的账本错误，掩盖"调用方在已提交后还想换供应商"这个真正的问题。
        ensure!(
            !self.committed,
            "coordinator refuses another attempt after commitment: switching supplier now would \
             splice two models' output into one response"
        );
        let verdicts = evaluate(self.ledger, self.key_id, self.plan, &self.plan.candidates)?;

        // 候选集 = 准入通过 ∩ 本次未失败过。路由引擎只在这个集合里选。
        let eligible: Vec<_> = self
            .plan
            .candidates
            .iter()
            .filter(|candidate| {
                !self.exhausted.contains(&candidate.binding_id)
                    && verdicts.iter().any(|(id, verdict)| {
                        id == &candidate.binding_id
                            && matches!(verdict, CandidateVerdict::Eligible(_))
                    })
            })
            .cloned()
            .collect();
        if eligible.is_empty() {
            return Ok(None);
        }

        let Some(selection) = self.routing.select(ctx, &eligible, self.plan.mode, None) else {
            return Ok(None);
        };
        let admission = verdicts
            .iter()
            .find_map(|(id, verdict)| match verdict {
                CandidateVerdict::Eligible(a) if id == &selection.binding_id => Some(a.clone()),
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("selected binding lost its admission"))?;
        let binding = self
            .plan
            .binding(&selection.binding_id)
            .ok_or_else(|| anyhow::anyhow!("plan has no binding {}", selection.binding_id))?;
        let upstream = self
            .plan
            .upstream(&selection.upstream_id)
            .ok_or_else(|| anyhow::anyhow!("plan has no upstream {}", selection.upstream_id))?;

        self.attempts_used += 1;
        let attempt_id = format!("{}:{}", self.request_id, self.attempts_used);
        self.ledger.reserve(ReservationInput {
            request_id: self.request_id.clone(),
            attempt_id: attempt_id.clone(),
            key_id: self.key_id,
            unit: admission.unit,
            public_model: self.plan.alias.clone(),
            upstream_id: selection.upstream_id.clone(),
            upper_bound: admission.bound,
            bound_kind: admission.bound_kind,
            // 快照只含定价与路由身份，不含密钥，也不含任何请求内容。
            snapshot: PriceSnapshot {
                config_revision: self.plan.revision,
                price_revision: self.plan.revision,
                binding_id: binding.id.clone(),
                upstream_kind: upstream.kind,
                upstream_model: binding.upstream_model.clone(),
                cost_prices: binding.cost_prices.clone(),
                sell_prices: binding.sell_prices.clone(),
            },
        })?;

        Ok(Some(ReservedAttempt {
            attempt_id,
            binding_id: selection.binding_id,
            upstream_id: selection.upstream_id,
            upstream_model: binding.upstream_model.clone(),
            unit: admission.unit,
            generation: self.plan.generation,
        }))
    }

    /// 回报一次尝试的结果，并给出下一步。
    pub fn finish_attempt(
        &mut self,
        ctx: &RouteContext,
        attempt: &ReservedAttempt,
        result: AttemptResult,
    ) -> Result<Next> {
        match result {
            AttemptResult::Succeeded { cost, usage } => {
                self.settle(attempt, cost, usage, true, AttemptOutcome::Succeeded)?;
                // 成功才发布粘性绑定，且代际必须仍然有效——期间配置若已变更，
                // 这条绑定代表的语义已经不是当前的了。
                self.routing
                    .bind_success(ctx, &attempt.binding_id, attempt.generation);
                Ok(Next::Stop(StopReason::Succeeded))
            }
            AttemptResult::InterruptedAfterCommitment { cost, usage } => {
                self.committed = true;
                // 已提交：如实记为中断。不 release——下游已经收到内容，
                // 这笔消耗真实发生过。
                self.settle(attempt, cost, usage, true, AttemptOutcome::Interrupted)?;
                Ok(Next::Stop(StopReason::InterruptedAfterCommitment))
            }
            AttemptResult::FailedBeforeCommitment { retryable } => {
                // 确认未产生任何下游承诺，才可以释放预留。
                self.settle(attempt, None, None, false, AttemptOutcome::Failed)?;
                self.ledger.release(&attempt.attempt_id)?;
                self.exhausted.push(attempt.binding_id.clone());
                if !retryable {
                    return Ok(Next::Stop(StopReason::NotRetryable));
                }
                Ok(self.next_after_failure())
            }
        }
    }

    fn next_after_failure(&self) -> Next {
        if self.attempts_used >= self.plan.max_attempts {
            return Next::Stop(StopReason::AttemptsExhausted);
        }
        if self.started_at.elapsed() >= self.plan.deadline {
            return Next::Stop(StopReason::DeadlineReached);
        }
        Next::Retry
    }

    fn settle(
        &self,
        attempt: &ReservedAttempt,
        cost: Option<Amount>,
        usage: Option<serde_json::Value>,
        committed: bool,
        outcome: AttemptOutcome,
    ) -> Result<()> {
        // 有确认金额才算 confirmed；没有就是 pending，绝不用 0 顶替。
        let evidence = if cost.is_some() {
            EvidenceStatus::Confirmed
        } else {
            EvidenceStatus::Pending
        };
        self.ledger.settle(SettlementInput {
            settlement_id: format!("settle:{}", attempt.attempt_id),
            attempt_id: attempt.attempt_id.clone(),
            evidence,
            downstream_amount: committed.then_some(cost).flatten(),
            attempt_cost: AttemptCost {
                unit: attempt.unit,
                evidence,
                amount: cost,
            },
            committed,
            outcome,
            usage,
        })?;
        Ok(())
    }

    /// 已经向下游提交过内容之后，还剩多少余地：没有。
    pub fn is_committed(&self) -> bool {
        self.committed
    }

    /// 标记"第一个下游事件已发出"。此后任何失败都只能报中断。
    pub fn mark_committed(&mut self) {
        self.committed = true;
    }

    /// 期限是否已到。调用方在每次重试前检查，避免无限等待。
    pub fn deadline_reached(&self) -> bool {
        self.started_at.elapsed() >= self.plan.deadline
    }

    /// 仅供测试：把起始时刻往前推，模拟期限耗尽。
    #[cfg(test)]
    pub(crate) fn age_by(&mut self, elapsed: Duration) {
        self.started_at -= elapsed;
    }
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;
