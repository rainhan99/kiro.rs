//! 一次被接管的请求从入口到应答的完整路径。
//!
//! # 状态机不外包
//!
//! 预留 → 发送 → 结算这条链由本模块独占，只有**发送**那一步委托给
//! [`RouteExecutor`]。Kiro 路要走既有的 provider 通道，直连路走 [`super::execute`]，
//! 但两者的记账必须逐字相同——把状态机切成两半交给两个调用方去拼，迟早会拼出
//! 一条只扣费不记账、或只记账不扣费的路径。
//!
//! # 本模块只处理非流式
//!
//! [`RouteExecutor`] 返回整个响应体，因此流式请求不能走这里。流式转发要连着
//! "首个下游事件即提交"一起做（见 [`super::coordinator`] 的第一条规则），
//! 是单独的一步。
//!
//! # 转换失败分两种，后果完全不同
//!
//! **发送之前**转换失败：这条路的协议接不住这个请求，但别的路可能原生就说客户端
//! 的协议。记为这条路失败、换路继续。
//!
//! **发送之后**响应转换失败：上游已经算过钱了。这笔费用如实结算，给客户端报错，
//! 且**绝不重试**——重试等于为同一个请求付两次。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use super::admission::{CandidateVerdict, Refusal, evaluate};
use super::coordinator::{AttemptResult, Coordinator, Next, ReservedAttempt, StopReason};
use super::execute::SendError;
use super::protocol::{WireProtocol, convert_request, convert_response};
use super::routing::RouteContext;
use super::service::{GatewayService, RequestPlan};
use super::settlement::{KiroRoute, Settlement};
use super::usage::{NativeUsage, normalize_usage, token_cost};
use super::{Amount, BillingUnit, ModelBinding, Upstream, UpstreamKind};

/// 分发的结局。
pub enum Dispatched {
    /// 网关不接管这个别名。调用方**原样**走既有路径——不接管就不该改变任何行为。
    NotManaged,
    /// 已完成，这是给客户端的应答（已转回客户端协议）。
    Answered(Value),
    /// 拒绝，附带可直接回给客户端的状态码与原因。
    Refused { status: u16, reason: String },
    /// 选中了一条 Kiro 路。预留已经记在账上，由调用方走既有 Kiro 通道执行，
    /// 并在用量汇聚点用 [`KiroRoute::settlement`] 结算（理由见 [`super::settlement`]）。
    UseKiro(Box<KiroRoute>),
}

impl std::fmt::Debug for Dispatched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotManaged => write!(f, "NotManaged"),
            Self::Answered(value) => write!(f, "Answered({value})"),
            Self::Refused { status, reason } => {
                write!(f, "Refused {{ status: {status}, reason: {reason:?} }}")
            }
            Self::UseKiro(route) => write!(f, "UseKiro({})", route.upstream_model),
        }
    }
}

/// 把一条路上的请求真正发出去。
///
/// 用 trait 而非直接调用，是为了让 Kiro 路与直连路共用同一套状态机；测试里也能
/// 用一个不碰网络的实现跑完整条链。
pub trait RouteExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        upstream: &'a Upstream,
        protocol: WireProtocol,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Value, SendError>> + Send + 'a>>;
}

/// 分发一次请求。
///
/// `request_id` 必须逐请求唯一——账本的幂等性建立在它上面。
pub async fn dispatch(
    gateway: &Arc<GatewayService>,
    executor: &dyn RouteExecutor,
    protocol: WireProtocol,
    body: &Value,
    ctx: &RouteContext,
    request_id: &str,
) -> Dispatched {
    let Some(plan) = gateway.plan_for(&ctx.public_model) else {
        return Dispatched::NotManaged;
    };
    // 惰性网关没有账本。走到这里说明配置声明了模型却没有账本，
    // 那是启动时就该拒绝的状态；这里不静悄悄降级成"免费放行"。
    let Some(ledger) = gateway.ledger() else {
        return Dispatched::Refused {
            status: 500,
            reason: "gateway manages this model but has no ledger to charge against".into(),
        };
    };

    let mut coordinator =
        Coordinator::new(ledger, gateway.routing(), &plan, ctx.key_id, request_id);
    loop {
        let attempt = match coordinator.begin_attempt(ctx) {
            Ok(Some(attempt)) => attempt,
            // 没有可用路线。第一次就没有，说明是准入拒绝，把真实理由告诉客户端；
            // 试过之后才没有，说明可选的路都失败了。
            Ok(None) => return no_route(ledger, &plan, ctx, coordinator.attempts_used()),
            Err(error) => {
                return Dispatched::Refused {
                    status: 500,
                    reason: format!("{error:#}"),
                };
            }
        };

        // Kiro 路不由网关发请求：预留已经记上，执行交回既有通道。
        // 它自带凭据轮换与重试，所以这里不再叠加一层换路重试。
        if plan.upstream(&attempt.upstream_id).map(|u| u.kind) == Some(UpstreamKind::Kiro) {
            let group = plan
                .upstream(&attempt.upstream_id)
                .and_then(|u| u.kiro_group.clone());
            return Dispatched::UseKiro(Box::new(KiroRoute {
                upstream_model: attempt.upstream_model.clone(),
                group,
                settlement: Arc::new(Settlement::new(
                    gateway.clone(),
                    attempt.attempt_id.clone(),
                    attempt.unit,
                )),
            }));
        }

        match attempt_once(executor, &plan, protocol, body, &attempt).await {
            Ok(raw) => {
                return settle_success(&mut coordinator, ctx, &plan, protocol, &attempt, raw);
            }
            Err(RouteError::Unsendable(reason)) => {
                // 这条路接不住，换一条；别的路可能原生说客户端的协议。
                match finish(&mut coordinator, ctx, &attempt, true) {
                    Ok(Next::Retry) => continue,
                    Ok(Next::Stop(stop)) => return stopped(stop, &reason),
                    Err(error) => return internal(error),
                }
            }
            Err(RouteError::Upstream(failure)) => {
                let retryable = failure.is_retryable();
                let reason = failure.to_string();
                match finish(&mut coordinator, ctx, &attempt, retryable) {
                    Ok(Next::Retry) => continue,
                    Ok(Next::Stop(stop)) => return stopped(stop, &reason),
                    Err(error) => return internal(error),
                }
            }
        }
    }
}

pub(super) enum RouteError {
    /// 请求没发出去（转换不了、端点不合法、目标被拒）。
    Unsendable(String),
    Upstream(SendError),
}

async fn attempt_once(
    executor: &dyn RouteExecutor,
    plan: &RequestPlan,
    protocol: WireProtocol,
    body: &Value,
    attempt: &ReservedAttempt,
) -> std::result::Result<Value, RouteError> {
    let upstream = plan
        .upstream(&attempt.upstream_id)
        .ok_or_else(|| RouteError::Unsendable("plan lost its upstream".into()))?;
    let wire = wire_for(upstream.kind);
    let bytes = attempt_body(protocol, wire, body, &attempt.upstream_model)?;

    executor
        .execute(upstream, wire, bytes, plan.deadline)
        .await
        .map_err(classify_send)
}

/// 「没发出去」与「发出去了但失败」必须分开。
///
/// [`SendError::Refused`] 说明请求**根本没上线**：端点配置不合法、目标落在私网。
/// 客户端的请求没有任何问题，所以既不该回 400 说它无效，也不该就此停手——
/// 别的路可能好好的。它和转换失败是同一类：这条路承不住，换下一条。
pub(super) fn classify_send(error: SendError) -> RouteError {
    match error {
        SendError::Refused(reason) => RouteError::Unsendable(format!("{reason:#}")),
        SendError::Upstream(failure) => RouteError::Upstream(SendError::Upstream(failure)),
    }
}

/// 把请求转成这条路的协议并序列化。转换失败即这条路接不住这个请求。
pub(super) fn attempt_body(
    protocol: WireProtocol,
    wire: WireProtocol,
    body: &Value,
    upstream_model: &str,
) -> std::result::Result<Vec<u8>, RouteError> {
    let converted = convert_request(protocol, wire, body, upstream_model)
        .map_err(|e| RouteError::Unsendable(format!("{e:#}")))?;
    serde_json::to_vec(&converted)
        .map_err(|e| RouteError::Unsendable(format!("request could not be serialized: {e}")))
}

fn settle_success(
    coordinator: &mut Coordinator<'_>,
    ctx: &RouteContext,
    plan: &RequestPlan,
    protocol: WireProtocol,
    attempt: &ReservedAttempt,
    raw: Value,
) -> Dispatched {
    let Some(binding) = plan.binding(&attempt.binding_id) else {
        return internal(anyhow::anyhow!("plan lost its binding"));
    };
    let Some(upstream) = plan.upstream(&attempt.upstream_id) else {
        return internal(anyhow::anyhow!("plan lost its upstream"));
    };
    let wire = wire_for(upstream.kind);

    // 算不出金额时如实记为待结算，但**不把原因吞掉**：一笔说不出来历的
    // 待结算义务，运维事后无从判断该不该认。
    let usage = match normalize_usage(upstream.kind, &raw) {
        Ok(usage) => usage,
        Err(error) => {
            tracing::warn!(
                attempt = %attempt.attempt_id,
                upstream = %upstream.id,
                "上游用量无法解析，本次记为待结算: {error:#}"
            );
            None
        }
    };
    let cost = usage
        .as_ref()
        .and_then(|u| match customer_cost(binding, u) {
            Ok(cost) => cost,
            Err(error) => {
                tracing::warn!(
                    attempt = %attempt.attempt_id,
                    binding = %binding.id,
                    "用量已拿到但算不出金额，本次记为待结算: {error:#}"
                );
                None
            }
        });
    let usage_value = usage.as_ref().map(|u| u.raw.clone());

    // 无论响应能不能转回去，钱都已经花掉了，先如实结算。
    let settled = coordinator.finish_attempt(
        ctx,
        attempt,
        AttemptResult::Succeeded {
            cost,
            usage: usage_value,
        },
    );
    if let Err(error) = settled {
        return internal(error);
    }

    match convert_response(wire, protocol, &raw, &plan.alias) {
        Ok(answer) => Dispatched::Answered(answer),
        // 已结算，绝不重试——重试等于为同一个请求付两次。
        Err(error) => Dispatched::Refused {
            status: 502,
            reason: format!(
                "upstream answered and was charged, but the response could not be converted \
                 back to the client protocol: {error:#}"
            ),
        },
    }
}

fn finish(
    coordinator: &mut Coordinator<'_>,
    ctx: &RouteContext,
    attempt: &ReservedAttempt,
    retryable: bool,
) -> Result<Next> {
    coordinator.finish_attempt(
        ctx,
        attempt,
        AttemptResult::FailedBeforeCommitment { retryable },
    )
}

/// 客户可见的这一次花费。
///
/// 原生积分以上游自己报的 `credits` 为准——那是供应商的真相，不是本地估算。
/// 按钱计费的路用配置里的售价乘用量。两者都拿不到时返回 `None`，由账本记为
/// 待结算，**绝不用 0 顶替**。
pub(super) fn customer_cost(binding: &ModelBinding, usage: &NativeUsage) -> Result<Option<Amount>> {
    match binding.billing_unit {
        BillingUnit::KiroCredit => Ok(usage.credits),
        _ => binding
            .sell_prices
            .as_ref()
            .map(|prices| token_cost(prices, usage))
            .transpose(),
    }
}

/// 上游种类决定线上协议。
///
/// Kiro 归到 Anthropic：它的线上格式是 Amazon Q 的事件流，但既有 provider 接受的
/// 请求形状就是 Anthropic 的，而 Kiro 路的"发送"正是交给那个 provider。
pub(super) fn wire_for(kind: UpstreamKind) -> WireProtocol {
    match kind {
        UpstreamKind::Kiro | UpstreamKind::Anthropic => WireProtocol::Anthropic,
        UpstreamKind::OpenaiChat => WireProtocol::ChatCompletions,
        UpstreamKind::OpenaiResponses => WireProtocol::Responses,
    }
}

/// 没有可用路线时，把**真实原因**告诉客户端，而不是一句笼统的不可用。
pub(super) fn no_route(
    ledger: &super::ledger::Ledger,
    plan: &RequestPlan,
    ctx: &RouteContext,
    attempts_used: u32,
) -> Dispatched {
    if attempts_used > 0 {
        return Dispatched::Refused {
            status: 502,
            reason: "every eligible route failed for this request".into(),
        };
    }
    let Ok(verdicts) = evaluate(ledger, ctx.key_id, plan, &plan.candidates) else {
        return Dispatched::Refused {
            status: 503,
            reason: "no route is currently eligible".into(),
        };
    };
    // 多条路各有各的拒绝理由时，取最该让客户看到的那条：能补救的排在前面。
    let refusals: Vec<&Refusal> = verdicts
        .iter()
        .filter_map(|(_, v)| match v {
            CandidateVerdict::Refused(r) => Some(r),
            CandidateVerdict::Eligible(_) => None,
        })
        .collect();
    let Some(chosen) = refusals.iter().copied().min_by_key(|r| rank(r)) else {
        return Dispatched::Refused {
            status: 503,
            reason: "this model has no enabled route".into(),
        };
    };
    let (status, reason) = render(chosen);
    Dispatched::Refused { status, reason }
}

/// 越小越优先展示。并发上限与额度耗尽是客户自己能处理的，排在配置类问题前面。
fn rank(refusal: &Refusal) -> u8 {
    match refusal {
        Refusal::TooManyInFlight { .. } => 0,
        Refusal::Exhausted { .. } => 1,
        Refusal::NoAccountForUnit { .. } => 2,
        Refusal::NotAuthorized { .. } => 3,
        Refusal::UnboundedUnderHardPolicy { .. } => 4,
    }
}

fn render(refusal: &Refusal) -> (u16, String) {
    match refusal {
        Refusal::TooManyInFlight { unit } => (
            429,
            format!("too many requests in flight or awaiting settlement on the {unit:?} account"),
        ),
        Refusal::Exhausted { unit } => (402, format!("the {unit:?} balance is exhausted")),
        Refusal::NoAccountForUnit { unit } => (
            403,
            format!("this key has no {unit:?} account, which every route for this model bills in"),
        ),
        Refusal::NotAuthorized { unit } => (
            403,
            format!("the {unit:?} account is not authorized for this model or upstream"),
        ),
        Refusal::UnboundedUnderHardPolicy { reason } => (
            400,
            format!("a hard budget needs a guaranteed upper bound, which is unavailable: {reason}"),
        ),
    }
}

pub(super) fn stopped(stop: StopReason, reason: &str) -> Dispatched {
    let status = match stop {
        StopReason::NotRetryable => 400,
        StopReason::DeadlineReached => 504,
        _ => 502,
    };
    Dispatched::Refused {
        status,
        reason: format!("{reason} ({stop:?})"),
    }
}

fn internal(error: anyhow::Error) -> Dispatched {
    Dispatched::Refused {
        status: 500,
        reason: format!("{error:#}"),
    }
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
