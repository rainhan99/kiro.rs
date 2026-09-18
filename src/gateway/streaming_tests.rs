use super::*;
use crate::gateway::ledger_types::AccountView;
use crate::gateway::{
    Amount, BillingUnit, BudgetEnforcement, BudgetPolicy, GatewayConfig, ModelBinding, PublicModel,
    RoutingMode, TokenPrices, Upstream, UpstreamKind,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("kiro-stream-{name}-{}", uuid::Uuid::new_v4()))
}

type Script = Box<dyn Fn() -> Result<ByteStream, SendError> + Send + Sync>;

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    scripts: Mutex<HashMap<String, std::sync::Arc<Script>>>,
}

impl Fake {
    fn on(&self, upstream_id: &str, script: Script) -> &Self {
        self.scripts
            .lock()
            .insert(upstream_id.into(), std::sync::Arc::new(script));
        self
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().clone()
    }
}

impl StreamExecutor for Fake {
    fn open_stream(
        &self,
        upstream: Upstream,
        _protocol: WireProtocol,
        _body: Vec<u8>,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<ByteStream, SendError>> + Send + '_>> {
        self.calls.lock().push(upstream.id.clone());
        let script = self.scripts.lock().get(&upstream.id).cloned();
        Box::pin(async move {
            match script {
                Some(script) => script(),
                None => Err(SendError::Upstream(
                    crate::gateway::transport::UpstreamFailure::Transient {
                        status: 503,
                        snippet: "no script".into(),
                    },
                )),
            }
        })
    }
}

fn chunks(items: Vec<Result<Bytes, SendError>>) -> ByteStream {
    Box::pin(futures::stream::iter(items))
}

/// 一段完整的 Anthropic 流：起始（含缓存计数）、一个文本增量、结束。
fn complete_stream() -> Script {
    Box::new(|| {
        Ok(chunks(vec![
            Ok(Bytes::from(
                "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n",
            )),
            Ok(Bytes::from(
                "event: content_block_delta\ndata: {\"delta\":{\"text\":\"hi\"}}\n\n",
            )),
            Ok(Bytes::from(
                "event: message_delta\ndata: {\"usage\":{\"output_tokens\":500}}\n\n",
            )),
            Ok(Bytes::from("event: message_stop\ndata: {}\n\n")),
        ]))
    })
}

fn price(v: &str) -> TokenPrices {
    TokenPrices {
        currency: BillingUnit::Cny,
        input: v.parse().unwrap(),
        output: v.parse().unwrap(),
        cache_read: "0".parse().unwrap(),
        cache_write: "0".parse().unwrap(),
        cache_write_1h: None,
    }
}

fn upstream(id: &str, kind: UpstreamKind) -> Upstream {
    Upstream {
        id: id.into(),
        name: id.into(),
        kind,
        enabled: true,
        weight: 10,
        base_url: Some("https://api.example.test".into()),
        api_key: Some("k".into()),
        has_api_key: true,
        allow_private_network: false,
        kiro_group: None,
        cache_usage_policy: None,
    }
}

fn binding(id: &str, upstream_id: &str, tier: u32) -> ModelBinding {
    ModelBinding {
        id: id.into(),
        upstream_id: upstream_id.into(),
        upstream_model: format!("real-{id}"),
        enabled: true,
        priority_tier: tier,
        weight: 10,
        context_window: 200_000,
        max_output_tokens: 8_000,
        supports_tools: true,
        supports_images: true,
        supports_reasoning: true,
        allow_model_substitution: false,
        billing_unit: BillingUnit::Cny,
        cost_prices: Some(price("1")),
        sell_prices: Some(price("2")),
    }
}

struct Fixture {
    service: Arc<GatewayService>,
    config_path: PathBuf,
    ledger_path: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.config_path);
        let _ = std::fs::remove_file(&self.ledger_path);
    }
}

fn fixture(bindings: Vec<ModelBinding>, upstreams: Vec<Upstream>, limit: Option<&str>) -> Fixture {
    let config_path = temp("config.json");
    let ledger_path = temp("billing.db");
    let config = GatewayConfig {
        models: vec![PublicModel {
            id: "opus5".into(),
            display_name: None,
            routing_mode: Some(RoutingMode::Sticky),
            affinity_ttl_secs: None,
            bindings,
        }],
        upstreams,
        ..GatewayConfig::default()
    };
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let service = Arc::new(GatewayService::open(&config_path, &ledger_path).unwrap());
    if let Some(limit) = limit {
        service
            .ledger()
            .unwrap()
            .set_account(
                7,
                BudgetPolicy {
                    unit: BillingUnit::Cny,
                    limit: Some(limit.parse().unwrap()),
                    enforcement: BudgetEnforcement::Soft,
                    max_in_flight: 8,
                    max_pending: 16,
                    allowed_models: vec![],
                    allowed_upstreams: vec![],
                },
            )
            .unwrap();
    }
    Fixture {
        service,
        config_path,
        ledger_path,
    }
}

fn ctx() -> RouteContext {
    RouteContext {
        key_id: 7,
        public_model: "opus5".into(),
        session_id: Some("session-0123456789".into()),
        needs_tools: false,
        needs_images: false,
        needs_reasoning: false,
    }
}

fn request() -> Value {
    serde_json::json!({"model": "opus5", "max_tokens": 100, "stream": true,
        "messages": [{"role": "user", "content": "hi"}]})
}

fn account(f: &Fixture) -> AccountView {
    f.service
        .ledger()
        .unwrap()
        .accounts(7)
        .unwrap()
        .into_iter()
        .find(|a| a.policy.unit == BillingUnit::Cny)
        .expect("应有人民币账户")
}

async fn drain(mut rx: mpsc::Receiver<Result<Bytes, std::io::Error>>) -> String {
    let mut out = String::new();
    while let Some(item) = rx.recv().await {
        out.push_str(std::str::from_utf8(&item.unwrap()).unwrap());
    }
    out
}

/// 不接管的别名不该开任何流。
#[tokio::test]
async fn an_unmanaged_alias_is_left_to_the_legacy_path() {
    let f = fixture(
        vec![binding("a", "d1", 0)],
        vec![upstream("d1", UpstreamKind::Anthropic)],
        Some("100"),
    );
    let fake = Arc::new(Fake::default());
    let mut other = ctx();
    other.public_model = "elsewhere".into();

    let result = dispatch_stream(
        f.service.clone(),
        fake.clone(),
        WireProtocol::Anthropic,
        request(),
        other,
        "r1".into(),
    )
    .await;

    assert!(matches!(result, StreamDispatched::NotManaged));
    assert!(fake.calls().is_empty());
}

/// 完整一段流：事件如实转发，且用量（含缓存计数）走与非流式相同的定价逻辑。
#[tokio::test]
async fn a_streamed_request_forwards_events_and_is_priced_from_its_usage() {
    let f = fixture(
        vec![binding("a", "d1", 0)],
        vec![upstream("d1", UpstreamKind::Anthropic)],
        Some("100"),
    );
    let fake = Arc::new(Fake::default());
    fake.on("d1", complete_stream());

    let result = dispatch_stream(
        f.service.clone(),
        fake.clone(),
        WireProtocol::Anthropic,
        request(),
        ctx(),
        "r1".into(),
    )
    .await;

    let StreamDispatched::Streaming(rx) = result else {
        panic!("应开流");
    };
    let body = drain(rx).await;
    assert!(body.contains("event: message_start"));
    assert!(body.contains("\"text\":\"hi\""));
    assert!(body.contains("event: message_stop"));

    let cny = account(&f);
    assert_eq!(
        cny.used,
        "0.003".parse().unwrap(),
        "1000 输入 + 500 输出，按 2/百万 计 = 0.003"
    );
    assert_eq!(cny.in_flight, 0);
    assert_eq!(cny.customer_pending, 0, "证据齐全应确认结算");
}

/// 已经发出过下游事件之后中断：**保留义务，且绝不换供应商**。
/// 换一家把剩下的补完，等于把两个模型的输出拼成一个响应。
#[tokio::test]
async fn an_interruption_after_the_first_event_keeps_the_charge_and_never_switches() {
    let f = fixture(
        vec![binding("a", "d1", 0), binding("b", "d2", 1)],
        vec![
            upstream("d1", UpstreamKind::Anthropic),
            upstream("d2", UpstreamKind::Anthropic),
        ],
        Some("100"),
    );
    let fake = Arc::new(Fake::default());
    fake.on(
        "d1",
        Box::new(|| {
            Ok(chunks(vec![
                Ok(Bytes::from(
                    "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n",
                )),
                Err(SendError::Upstream(
                    crate::gateway::transport::UpstreamFailure::StreamInterrupted {
                        detail: "connection reset".into(),
                    },
                )),
            ]))
        }),
    );
    fake.on("d2", complete_stream());

    let StreamDispatched::Streaming(rx) = dispatch_stream(
        f.service.clone(),
        fake.clone(),
        WireProtocol::Anthropic,
        request(),
        ctx(),
        "r1".into(),
    )
    .await
    else {
        panic!("应开流");
    };
    let body = drain(rx).await;
    assert!(body.contains("event: message_start"));

    assert_eq!(fake.calls(), vec!["d1"], "提交之后绝不能再碰第二条路");
    let cny = account(&f);
    assert_eq!(cny.in_flight, 0);
    // 中断时 message_start 的用量已经收齐，所以这笔如实**确认计费**——
    // 客户端没收全不等于上游没算钱。关键是它没有被释放掉。
    assert_eq!(
        cny.used,
        "0.002".parse().unwrap(),
        "1000 输入 × 2/百万；已提交的中断不得让消耗消失"
    );
    assert_eq!(cny.customer_pending, 0, "证据齐全就该确认，而不是挂着");
}

/// 同样是已提交后中断，但**一个用量字段都没收到**：这笔留作待结算义务，
/// 而不是按 0 确认，也不是当作没发生过释放掉。
#[tokio::test]
async fn an_interruption_without_any_usage_evidence_stays_pending() {
    let f = fixture(
        vec![binding("a", "d1", 0)],
        vec![upstream("d1", UpstreamKind::Anthropic)],
        Some("100"),
    );
    let fake = Arc::new(Fake::default());
    fake.on(
        "d1",
        Box::new(|| {
            Ok(chunks(vec![
                // 有内容发给了客户端，但没有任何用量。
                Ok(Bytes::from(
                    "event: content_block_delta\ndata: {\"delta\":{\"text\":\"hi\"}}\n\n",
                )),
                Err(SendError::Upstream(
                    crate::gateway::transport::UpstreamFailure::StreamInterrupted {
                        detail: "connection reset".into(),
                    },
                )),
            ]))
        }),
    );

    let StreamDispatched::Streaming(rx) = dispatch_stream(
        f.service.clone(),
        fake.clone(),
        WireProtocol::Anthropic,
        request(),
        ctx(),
        "r1".into(),
    )
    .await
    else {
        panic!("应开流");
    };
    assert!(drain(rx).await.contains("content_block_delta"));

    let cny = account(&f);
    assert_eq!(cny.in_flight, 0);
    assert_eq!(cny.used, Amount::ZERO, "没有证据不得按 0 确认计费");
    assert_eq!(cny.customer_pending, 1, "必须留下待结算义务");
}

/// 一个字节都没发出去就失败：换下一条路，客户端毫无察觉。
#[tokio::test]
async fn a_failure_before_any_event_moves_to_another_route() {
    let f = fixture(
        vec![binding("a", "d1", 0), binding("b", "d2", 1)],
        vec![
            upstream("d1", UpstreamKind::Anthropic),
            upstream("d2", UpstreamKind::Anthropic),
        ],
        Some("100"),
    );
    let fake = Arc::new(Fake::default());
    fake.on(
        "d1",
        Box::new(|| {
            Err(SendError::Upstream(
                crate::gateway::transport::UpstreamFailure::Transient {
                    status: 503,
                    snippet: "down".into(),
                },
            ))
        }),
    );
    fake.on("d2", complete_stream());

    let StreamDispatched::Streaming(rx) = dispatch_stream(
        f.service.clone(),
        fake.clone(),
        WireProtocol::Anthropic,
        request(),
        ctx(),
        "r1".into(),
    )
    .await
    else {
        panic!("应换路开流");
    };
    assert!(drain(rx).await.contains("message_stop"));
    assert_eq!(fake.calls(), vec!["d1", "d2"]);
    // 失败那次释放掉了，只有成功那次计费。
    assert_eq!(account(&f).used, "0.003".parse().unwrap());
}

/// 跨协议流式尚未实现：换一条协议一致的路，而不是把错协议的帧发出去。
#[tokio::test]
async fn a_cross_protocol_route_cannot_serve_a_stream_and_is_skipped() {
    let f = fixture(
        vec![binding("a", "d1", 0), binding("b", "d2", 1)],
        vec![
            upstream("d1", UpstreamKind::OpenaiChat),
            upstream("d2", UpstreamKind::Anthropic),
        ],
        Some("100"),
    );
    let fake = Arc::new(Fake::default());
    fake.on("d2", complete_stream());

    let StreamDispatched::Streaming(rx) = dispatch_stream(
        f.service.clone(),
        fake.clone(),
        WireProtocol::Anthropic,
        request(),
        ctx(),
        "r1".into(),
    )
    .await
    else {
        panic!("应换到协议一致的那条路");
    };
    assert!(drain(rx).await.contains("message_stop"));
    assert_eq!(fake.calls(), vec!["d2"], "协议不一致的那条路不该被打开");
}

/// 拒绝必须在响应开始**之前**定下来：把 502 塞进已经开始的 SSE 流，客户端无从处理。
#[tokio::test]
async fn a_refusal_is_decided_before_the_response_begins() {
    let f = fixture(
        vec![binding("a", "d1", 0)],
        vec![upstream("d1", UpstreamKind::Anthropic)],
        None, // 没有人民币账户
    );
    let fake = Arc::new(Fake::default());
    fake.on("d1", complete_stream());

    let result = dispatch_stream(
        f.service.clone(),
        fake.clone(),
        WireProtocol::Anthropic,
        request(),
        ctx(),
        "r1".into(),
    )
    .await;

    let StreamDispatched::Refused { status, reason } = result else {
        panic!("没有对应币种账户就不该开流");
    };
    assert_eq!(status, 403);
    assert!(reason.contains("account"), "实得：{reason}");
    assert!(fake.calls().is_empty(), "一条流都不该打开");
}

/// 客户端中途走人：上游已经算过的那笔照样如实入账，不因为没人收而消失。
///
/// 流必须长过通道缓冲（32 格），否则转发任务会在对端走人之前就把全部事件塞完，
/// 根本撞不上断开——那样测到的是"全都发完了"，不是"客户端走了"。
#[tokio::test]
async fn a_client_walking_away_still_leaves_the_obligation() {
    let f = fixture(
        vec![binding("a", "d1", 0)],
        vec![upstream("d1", UpstreamKind::Anthropic)],
        Some("100"),
    );
    let fake = Arc::new(Fake::default());
    fake.on(
        "d1",
        Box::new(|| {
            let mut items = vec![Ok(Bytes::from(
                "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n",
            ))];
            for _ in 0..200 {
                items.push(Ok(Bytes::from(
                    "event: content_block_delta\ndata: {\"delta\":{\"text\":\"x\"}}\n\n",
                )));
            }
            // 输出侧用量排在最后：客户端走人后这些永远到不了。
            items.push(Ok(Bytes::from(
                "event: message_delta\ndata: {\"usage\":{\"output_tokens\":500}}\n\n",
            )));
            items.push(Ok(Bytes::from("event: message_stop\ndata: {}\n\n")));
            Ok(chunks(items))
        }),
    );

    let StreamDispatched::Streaming(mut rx) = dispatch_stream(
        f.service.clone(),
        fake.clone(),
        WireProtocol::Anthropic,
        request(),
        ctx(),
        "r1".into(),
    )
    .await
    else {
        panic!("应开流");
    };
    // 收下第一个事件就走人。
    let _ = rx.recv().await.expect("应有第一个事件");
    drop(rx);

    // 等转发任务收拾完。
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let cny = account(&f);
        if cny.in_flight == 0 {
            // 只收到了 message_start 的用量（1000 输入），输出侧的帧没送出去。
            assert_eq!(
                cny.used,
                "0.002".parse().unwrap(),
                "客户端走了也不能让上游已经算过的这笔消失"
            );
            assert_eq!(cny.customer_pending, 0);
            return;
        }
    }
    panic!("转发任务没有结算");
}
