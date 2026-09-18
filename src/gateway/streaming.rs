//! 流式请求的分发与转发。
//!
//! # 为什么整条链必须活在一个任务里
//!
//! 协调器借着账本，而给 axum 的响应体必须是 `'static`。所以选路、预留、转发、结算
//! 全部发生在一个持有 `Arc<GatewayService>` 的任务内部，任务通过一个 oneshot 把
//! "开流成功还是被拒"回传给调用方。拒绝要在**返回响应之前**定下来——把一个 502
//! 塞进已经开始的 SSE 流里，客户端没有任何办法处理它。
//!
//! # 跨协议流式：拒绝，而不是把错协议的帧塞给客户端
//!
//! [`StreamTranslator`] 目前只从流里提取用量与终结信号，**不做协议转换**。因此
//! 上下游协议不一致的路无法承接流式请求：这时把这条路记为失败换下一条，而不是
//! 把 Chat 的帧原样发给一个等着 Anthropic 事件的客户端——那种失败客户端察觉不到。
//!
//! # 首个下游事件即提交
//!
//! 一旦有一个字节发给了客户端，就不再换供应商（见 [`super::coordinator`] 的第一条
//! 规则）。此后无论怎么断，都只能如实报中断，且那笔消耗保留为义务。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::RoutingMode;
use super::coordinator::{AttemptResult, Coordinator, Next};
use super::dispatch::{
    Dispatched, RouteError, attempt_body, classify_send, customer_cost, no_route, stopped, wire_for,
};
use super::execute::SendError;
use super::protocol::WireProtocol;
use super::routing::RouteContext;
use super::service::GatewayService;
use super::settlement::{KiroRoute, Settlement};
use super::sse::{SseEvent, SseParser, StreamTranslator};
use super::usage::normalize_usage;

/// 上游回来的字节流。
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, SendError>> + Send>>;

/// 打开一条上游流。
///
/// 与非流式的执行器分开：流式要的是一个**还没读过**的字节流，而不是完整报文。
pub trait StreamExecutor: Send + Sync {
    fn open_stream(
        &self,
        upstream: super::Upstream,
        protocol: WireProtocol,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<ByteStream, SendError>> + Send + '_>>;
}

/// 流式分发的结局。
pub enum StreamDispatched {
    /// 网关不接管这个别名，调用方原样走既有路径。
    NotManaged,
    Refused {
        status: u16,
        reason: String,
    },
    /// 已开流。逐块取出发给客户端。
    Streaming(mpsc::Receiver<Result<Bytes, std::io::Error>>),
    /// 选中了一条 Kiro 路：交既有 Kiro 通道执行（它自带 SSE 转换与原生用量提取）。
    UseKiro(Box<KiroRoute>),
}

/// 任务回传给调用方的决策。必须在流开起来**之前**定下来——把错误塞进一段
/// 已经开始的 SSE，客户端没有任何办法处理。
enum Decision {
    Opened,
    Refused(u16, String),
    UseKiro(Box<KiroRoute>),
}

/// 把接收端变成 axum 能用的流，不引入新依赖。
pub fn into_stream(
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|i| (i, rx)) })
}

pub async fn dispatch_stream(
    gateway: Arc<GatewayService>,
    executor: Arc<dyn StreamExecutor>,
    protocol: WireProtocol,
    body: Value,
    ctx: RouteContext,
    request_id: String,
    exclude: Vec<String>,
) -> StreamDispatched {
    if gateway.plan_for(&ctx.public_model).is_none() {
        return StreamDispatched::NotManaged;
    }
    if gateway.ledger().is_none() {
        return StreamDispatched::Refused {
            status: 500,
            reason: "gateway manages this model but has no ledger to charge against".into(),
        };
    }

    let (decided, decision) = oneshot::channel::<Decision>();
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);

    tokio::spawn(async move {
        run(
            gateway, executor, protocol, body, ctx, request_id, exclude, decided, tx,
        )
        .await;
    });

    match decision.await {
        Ok(Decision::Opened) => StreamDispatched::Streaming(rx),
        Ok(Decision::Refused(status, reason)) => StreamDispatched::Refused { status, reason },
        Ok(Decision::UseKiro(route)) => StreamDispatched::UseKiro(route),
        // 任务在回话之前就没了。宁可报一个明确的 500，也不要交回一个永远不出数据的流。
        Err(_) => StreamDispatched::Refused {
            status: 500,
            reason: "gateway stream task ended before it reported a decision".into(),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    gateway: Arc<GatewayService>,
    executor: Arc<dyn StreamExecutor>,
    protocol: WireProtocol,
    body: Value,
    ctx: RouteContext,
    request_id: String,
    exclude: Vec<String>,
    decided: oneshot::Sender<Decision>,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    let Some(plan) = gateway.plan_for(&ctx.public_model) else {
        let _ = decided.send(Decision::Refused(
            500,
            "plan disappeared mid-dispatch".into(),
        ));
        return;
    };
    let Some(ledger) = gateway.ledger() else {
        let _ = decided.send(Decision::Refused(
            500,
            "ledger disappeared mid-dispatch".into(),
        ));
        return;
    };
    let mut coordinator =
        Coordinator::new(ledger, gateway.routing(), &plan, ctx.key_id, &request_id)
            .excluding(&exclude);
    let mut decided = Some(decided);

    loop {
        let attempt = match coordinator.begin_attempt(&ctx) {
            Ok(Some(attempt)) => attempt,
            Ok(None) => {
                let refusal = no_route(ledger, &plan, &ctx, coordinator.attempts_used());
                return refuse(&mut decided, refusal);
            }
            Err(error) => {
                return refuse(
                    &mut decided,
                    Dispatched::Refused {
                        status: 500,
                        reason: format!("{error:#}"),
                    },
                );
            }
        };

        // Kiro 路交回既有通道：它自带 SSE 转换、凭据轮换与原生用量提取。
        if plan.upstream(&attempt.upstream_id).map(|u| u.kind) == Some(super::UpstreamKind::Kiro) {
            let group = plan
                .upstream(&attempt.upstream_id)
                .and_then(|u| u.kiro_group.clone());
            if let Some(decided) = decided.take() {
                let _ = decided.send(Decision::UseKiro(Box::new(KiroRoute {
                    binding_id: attempt.binding_id.clone(),
                    upstream_model: attempt.upstream_model.clone(),
                    group,
                    sticky: plan.mode != RoutingMode::WeightedRandom,
                    settlement: Arc::new(Settlement::new(
                        gateway.clone(),
                        attempt.attempt_id.clone(),
                        attempt.unit,
                    )),
                })));
            }
            return;
        }

        // 开流之前先确定这条路能不能表达这个请求。
        let opened = match prepare(&plan, protocol, &body, &attempt) {
            Ok((upstream, wire, bytes)) => executor
                .open_stream(upstream, wire, bytes, plan.deadline)
                .await
                .map(|stream| (stream, wire))
                .map_err(classify_send),
            Err(error) => Err(error),
        };

        let (stream, wire) = match opened {
            Ok(pair) => pair,
            Err(error) => {
                let (retryable, reason) = match &error {
                    RouteError::Unsendable(reason) => (true, reason.clone()),
                    RouteError::Upstream(failure) => (failure.is_retryable(), failure.to_string()),
                };
                match coordinator.finish_attempt(
                    &ctx,
                    &attempt,
                    AttemptResult::FailedBeforeCommitment { retryable },
                ) {
                    Ok(Next::Retry) => continue,
                    Ok(Next::Stop(stop)) => return refuse(&mut decided, stopped(stop, &reason)),
                    Err(error) => {
                        return refuse(
                            &mut decided,
                            Dispatched::Refused {
                                status: 500,
                                reason: format!("{error:#}"),
                            },
                        );
                    }
                }
            }
        };

        // 流已打开，此后不再换路。告诉调用方可以开始回应了。
        if let Some(decided) = decided.take() {
            let _ = decided.send(Decision::Opened);
        }
        forward(&mut coordinator, &ctx, &plan, &attempt, wire, stream, &tx).await;
        return;
    }
}

/// 构造这一路的出站报文，并确认协议接得住。
fn prepare(
    plan: &super::service::RequestPlan,
    protocol: WireProtocol,
    body: &Value,
    attempt: &super::coordinator::ReservedAttempt,
) -> Result<(super::Upstream, WireProtocol, Vec<u8>), RouteError> {
    let upstream = plan
        .upstream(&attempt.upstream_id)
        .ok_or_else(|| RouteError::Unsendable("plan lost its upstream".into()))?;
    let wire = wire_for(upstream.kind);
    if wire != protocol {
        // 帧级协议转换尚未实现。把错协议的帧发给客户端，它察觉不到。
        return Err(RouteError::Unsendable(format!(
            "streaming across protocols ({protocol:?} -> {wire:?}) is not implemented; \
             this route cannot serve a streamed request"
        )));
    }
    let bytes = attempt_body(protocol, wire, body, &attempt.upstream_model)?;
    Ok((upstream.clone(), wire, bytes))
}

/// 逐块转发，并在结束时如实结算。
async fn forward(
    coordinator: &mut Coordinator<'_>,
    ctx: &RouteContext,
    plan: &super::service::RequestPlan,
    attempt: &super::coordinator::ReservedAttempt,
    wire: WireProtocol,
    mut stream: ByteStream,
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    let mut parser = SseParser::new();
    let mut translator = StreamTranslator::new(wire);
    let mut interrupted: Option<String> = None;

    'outer: while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                interrupted = Some(error.to_string());
                break;
            }
        };
        let events = match parser.push(&chunk) {
            Ok(events) => events,
            Err(error) => {
                interrupted = Some(format!("{error:#}"));
                break;
            }
        };
        for event in events {
            let frames = match translator.consume(&event) {
                Ok(frames) => frames,
                Err(error) => {
                    interrupted = Some(format!("{error:#}"));
                    break 'outer;
                }
            };
            for out in &frames.events {
                // 第一个下游事件即提交：此后永不换供应商。
                coordinator.mark_committed();
                if tx.send(Ok(render(out))).await.is_err() {
                    // 客户端走了。上游已经算过的那部分照记不误。
                    interrupted = Some("client disconnected".into());
                    break 'outer;
                }
            }
            if frames.completed {
                break 'outer;
            }
        }
    }

    let usage = translator.usage_payload();
    let binding = plan.binding(&attempt.binding_id);
    let native = usage.as_ref().and_then(|payload| {
        let upstream = plan.upstream(&attempt.upstream_id)?;
        match normalize_usage(upstream.kind, &serde_json::json!({ "usage": payload })) {
            Ok(usage) => usage,
            Err(error) => {
                tracing::warn!(attempt = %attempt.attempt_id, "流式用量无法解析，记为待结算: {error:#}");
                None
            }
        }
    });
    let cost = match (binding, native.as_ref()) {
        (Some(binding), Some(usage)) => match customer_cost(binding, usage) {
            Ok(cost) => cost,
            Err(error) => {
                tracing::warn!(attempt = %attempt.attempt_id, "流式用量算不出金额，记为待结算: {error:#}");
                None
            }
        },
        _ => None,
    };
    let usage_value = native.as_ref().map(|u| u.raw.clone());

    let result = match &interrupted {
        // 中途断了，但已经向下游发过内容：如实记为中断，**不 release**。
        Some(_) if coordinator.is_committed() => AttemptResult::InterruptedAfterCommitment {
            cost,
            usage: usage_value,
        },
        // 一个字节都没发出去就断了：这次尝试没产生任何下游承诺。
        Some(_) => AttemptResult::FailedBeforeCommitment { retryable: false },
        None => AttemptResult::Succeeded {
            cost,
            usage: usage_value,
        },
    };
    if let Err(error) = coordinator.finish_attempt(ctx, attempt, result) {
        tracing::error!(attempt = %attempt.attempt_id, "流式请求结算失败: {error:#}");
    }
    if let Some(detail) = interrupted {
        tracing::warn!(attempt = %attempt.attempt_id, committed = coordinator.is_committed(), "流式转发中断: {detail}");
    }
}

/// 把一个事件写回 SSE 线格式。
fn render(event: &SseEvent) -> Bytes {
    let mut out = String::new();
    if !event.event.is_empty() {
        out.push_str("event: ");
        out.push_str(&event.event);
        out.push('\n');
    }
    for line in event.data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    Bytes::from(out)
}

fn refuse(decided: &mut Option<oneshot::Sender<Decision>>, refusal: Dispatched) {
    let (status, reason) = match refusal {
        Dispatched::Refused { status, reason } => (status, reason),
        _ => (500, "unexpected dispatch outcome".to_string()),
    };
    if let Some(decided) = decided.take() {
        let _ = decided.send(Decision::Refused(status, reason));
    }
}

#[cfg(test)]
#[path = "streaming_tests.rs"]
mod tests;
