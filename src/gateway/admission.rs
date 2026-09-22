//! 按路由做准入：资格过滤与硬上界计算。
//!
//! # 账户检查必须落在**被选中的那条路**上，而不是全局
//!
//! 旧行为是「Key 的积分用尽 → 整个 Key 被拒」。多上游之后这是错的：一个积分账户已耗尽、
//! 但人民币账户有余额的 Key，走按钱计费的那条路完全应该放行。反过来，一个**只有**积分
//! 账户的 Key 不能因为存在一条按钱计费的备路就被放行——它在那个币种上根本没有账户。
//!
//! 所以资格判定的粒度是「(这个 Key, 这条绑定的计费币种)」，逐候选判定。
//!
//! # 硬上界不能建立在启发式估算上
//!
//! 按钱计费且 `hard` 强制的账户要求一个**保证不会被突破**的上界。本地 token 估算是启发
//! 式的（见 `crate::token`），把它当保证等于用一个可能偏低的数字去冻结额度，实际花费超出
//! 时钱已经花掉了。因此硬上界只由**配置里声明的**输入/输出上限与轮次上限推出：这些是
//! 运维写死的约束，不是猜的。
//!
//! 声明不全（缺价格或缺上限）就意味着算不出保证值，此时按 `hard` 账户**拒绝**，
//! 而不是退而求其次用一个估算冒充保证。账户显式选择 `soft` 才允许无上界。

use anyhow::{Result, ensure};

use super::ledger::{AccountView, Ledger};
use super::ledger_types::BoundKind;
use super::routing::Candidate;
use super::service::RequestPlan;
use super::{Amount, BillingUnit, BudgetEnforcement, ModelBinding, TokenPrices};

/// 单个候选的准入判定。
#[derive(Debug, Clone, PartialEq)]
pub struct Admission {
    pub binding_id: String,
    pub unit: BillingUnit,
    /// 通过时给出的预留上界与其性质。
    pub bound: Option<Amount>,
    pub bound_kind: BoundKind,
}

/// 候选被拒的原因。每一种独立取值，便于运维分辨「没有这个币种的账户」与「额度用尽」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// 该 Key 在这条路的计费币种上没有账户。
    NoAccountForUnit { unit: BillingUnit },
    /// 有账户但额度已耗尽。
    Exhausted { unit: BillingUnit },
    /// 并发或待结算笔数达到上限。
    TooManyInFlight { unit: BillingUnit },
    /// 硬额度账户但算不出保证上界。
    UnboundedUnderHardPolicy { reason: String },
    /// 该路不被这个账户的授权列表允许。
    NotAuthorized { unit: BillingUnit },
}

/// 逐候选的判定结果。
#[derive(Debug, Clone, PartialEq)]
pub enum CandidateVerdict {
    Eligible(Admission),
    Refused(Refusal),
}

/// 对计划里的每个候选做准入判定。
///
/// 只判定，不预留——预留发生在路由选出唯一一条之后，避免为最终没被选中的候选占用额度。
pub fn evaluate(
    ledger: &Ledger,
    key_id: u64,
    plan: &RequestPlan,
    candidates: &[Candidate],
) -> Result<Vec<(String, CandidateVerdict)>> {
    let accounts = ledger.accounts(key_id)?;
    candidates
        .iter()
        .map(|candidate| {
            let binding = plan
                .binding(&candidate.binding_id)
                .ok_or_else(|| anyhow::anyhow!("plan has no binding {}", candidate.binding_id))?;
            Ok((
                candidate.binding_id.clone(),
                verdict(&accounts, binding, plan),
            ))
        })
        .collect()
}

fn verdict(
    accounts: &[AccountView],
    binding: &ModelBinding,
    plan: &RequestPlan,
) -> CandidateVerdict {
    let unit = binding.billing_unit;
    // 粒度是 (Key, 币种)：只有积分账户的 Key 够不到按钱计费的备路，反之亦然。
    let Some(account) = accounts.iter().find(|a| a.policy.unit == unit) else {
        return CandidateVerdict::Refused(Refusal::NoAccountForUnit { unit });
    };
    if !allowed(&account.policy.allowed_models, &plan.alias)
        || !allowed(&account.policy.allowed_upstreams, &binding.upstream_id)
    {
        return CandidateVerdict::Refused(Refusal::NotAuthorized { unit });
    }
    if account.available == Some(Amount::ZERO) {
        return CandidateVerdict::Refused(Refusal::Exhausted { unit });
    }
    if account.in_flight >= account.policy.max_in_flight
        || account.pending >= account.policy.max_pending
    {
        return CandidateVerdict::Refused(Refusal::TooManyInFlight { unit });
    }

    match account.policy.enforcement {
        // 软额度：允许无保证上界。原生积分本来就没有可证的调用前上界（见 Task 2 裁定）。
        BudgetEnforcement::Soft => CandidateVerdict::Eligible(Admission {
            binding_id: binding.id.clone(),
            unit,
            bound: None,
            bound_kind: BoundKind::Unknown,
        }),
        BudgetEnforcement::Hard => match guaranteed_bound(binding, plan.max_attempts) {
            Ok(bound) => CandidateVerdict::Eligible(Admission {
                binding_id: binding.id.clone(),
                unit,
                bound: Some(bound),
                bound_kind: BoundKind::Guaranteed,
            }),
            Err(error) => CandidateVerdict::Refused(Refusal::UnboundedUnderHardPolicy {
                reason: format!("{error:#}"),
            }),
        },
    }
}

fn allowed(values: &[String], target: &str) -> bool {
    values.is_empty() || values.iter().any(|v| v == target)
}

/// 由**配置声明**推出的最坏情况花费。
///
/// 刻意不接受任何本地 token 估算作为输入：估算偏低时，超出的部分是已经花掉的钱。
/// 这里只用 `contextWindow`（输入上限）、`maxOutputTokens`（输出上限）与请求的轮次上限，
/// 它们都是运维在配置里写死的。
///
/// 缓存类价格一并按输入上限计入：缓存读写同样按 token 计费，若忽略它们，一个大量命中
/// 缓存写入的请求会突破这个"保证"。
pub fn guaranteed_bound(binding: &ModelBinding, max_attempts: u32) -> Result<Amount> {
    let prices = binding
        .sell_prices
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("binding `{}` has no sell prices", binding.id))?;
    ensure!(
        prices.currency == binding.billing_unit,
        "binding `{}` sell price currency does not match its billing unit",
        binding.id
    );
    ensure!(
        binding.context_window > 0 && binding.max_output_tokens > 0,
        "binding `{}` does not declare both a context window and an output maximum, \
         so no guaranteed upper bound exists",
        binding.id
    );
    let per_attempt =
        worst_case_per_attempt(prices, binding.context_window, binding.max_output_tokens)?;
    // 内部可能发生多轮（重试、内部工具轮次）；上界必须覆盖配置允许的全部轮次。
    let attempts = u64::from(max_attempts.max(1));
    per_attempt
        .checked_mul_tokens(attempts.checked_mul(1_000_000).ok_or_else(|| {
            anyhow::anyhow!("attempt multiplier overflow for binding `{}`", binding.id)
        })?)
        .map_err(|e| anyhow::anyhow!("binding `{}` bound overflow: {e:#}", binding.id))
}

fn worst_case_per_attempt(prices: &TokenPrices, input_max: u64, output_max: u64) -> Result<Amount> {
    // 输入侧取最贵的单价：无法预知这次的输入落在普通输入还是缓存读写，
    // 取最大值才是"保证不被突破"。
    let dearest_input = [prices.input, prices.cache_read, prices.cache_write]
        .into_iter()
        .chain(prices.cache_write_1h)
        .fold(
            Amount::ZERO,
            |acc, price| if price > acc { price } else { acc },
        );
    let input = dearest_input.checked_mul_tokens(input_max)?;
    let output = prices.output.checked_mul_tokens(output_max)?;
    input.checked_add(output)
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
