use super::*;
use crate::gateway::{
    BillingUnit, BudgetEnforcement, BudgetPolicy, GatewayConfig, ModelBinding, PublicModel,
    RoutingMode, TokenPrices, Upstream, UpstreamKind,
};
use axum::http::StatusCode;
use std::path::PathBuf;

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("kiro-entry-{name}-{}", uuid::Uuid::new_v4()))
}

/// 路径决定协议，调用方无权指定。
#[test]
fn the_path_alone_decides_the_protocol() {
    use WireProtocol::*;
    assert_eq!(protocol_for_path("/v1/messages"), Some(Anthropic));
    assert_eq!(protocol_for_path("/cc/v1/messages"), Some(Anthropic));
    assert_eq!(
        protocol_for_path("/v1/chat/completions"),
        Some(ChatCompletions)
    );
    assert_eq!(protocol_for_path("/v1/responses"), Some(Responses));
    // 这些端点不该被网关接管。
    assert_eq!(protocol_for_path("/v1/models"), None);
    assert_eq!(protocol_for_path("/v1/messages/count_tokens"), None);
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

fn binding() -> ModelBinding {
    ModelBinding {
        id: "b1".into(),
        upstream_id: "u1".into(),
        upstream_model: "real-model".into(),
        enabled: true,
        priority_tier: 0,
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

fn upstream(base: &str) -> Upstream {
    Upstream {
        id: "u1".into(),
        name: "u1".into(),
        kind: UpstreamKind::Anthropic,
        enabled: true,
        weight: 10,
        base_url: Some(base.into()),
        api_key: Some("k".into()),
        has_api_key: true,
        // 回环明文，测试里显式放行。
        allow_private_network: true,
        kiro_group: None,
        cache_usage_policy: None,
    }
}

struct Fixture {
    entry: GatewayEntry,
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

fn fixture(base: &str, funded: bool) -> Fixture {
    let config_path = temp("config.json");
    let ledger_path = temp("billing.db");
    let config = GatewayConfig {
        models: vec![PublicModel {
            id: "opus5".into(),
            display_name: None,
            routing_mode: Some(RoutingMode::Sticky),
            affinity_ttl_secs: None,
            bindings: vec![binding()],
        }],
        upstreams: vec![upstream(base)],
        ..GatewayConfig::default()
    };
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let service = Arc::new(GatewayService::open(&config_path, &ledger_path).unwrap());
    if funded {
        service
            .ledger()
            .unwrap()
            .set_account(
                7,
                BudgetPolicy {
                    unit: BillingUnit::Cny,
                    limit: Some("100".parse().unwrap()),
                    enforcement: BudgetEnforcement::Soft,
                    max_in_flight: 8,
                    max_pending: 16,
                    allowed_models: vec![],
                    allowed_upstreams: vec![],
                },
            )
            .unwrap();
    }
    let client = crate::gateway::execute::build_client(
        std::time::Duration::from_secs(5),
        crate::model::config::TlsBackend::Rustls,
        None,
    )
    .unwrap();
    Fixture {
        entry: GatewayEntry::new(service.clone(), client),
        service,
        config_path,
        ledger_path,
    }
}

/// 起一个只回固定 Anthropic 报文的上游。
async fn upstream_server() -> String {
    let router = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|| async {
            axum::Json(serde_json::json!({
                "id": "msg_1", "type": "message", "role": "assistant",
                "model": "real-model",
                "content": [{"type": "text", "text": "hello"}],
                "usage": {"input_tokens": 1_000, "output_tokens": 500,
                          "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

/// 不接管的别名返回 `None`，调用方据此原样继续走既有路径。
#[tokio::test]
async fn an_unmanaged_alias_hands_the_request_back() {
    let f = fixture("http://127.0.0.1:1", true);
    let body = serde_json::json!({"model": "some-other-model", "messages": []});
    assert!(
        f.entry
            .handle(WireProtocol::Anthropic, body, 7, "r1".into())
            .await
            .is_none()
    );
    // 连 model 字段都没有时同样交回。
    assert!(
        f.entry
            .handle(
                WireProtocol::Anthropic,
                serde_json::json!({}),
                7,
                "r2".into()
            )
            .await
            .is_none()
    );
}

/// 接管的别名走完整条链，应答里保留对外别名。
#[tokio::test]
async fn a_managed_alias_is_served_and_charged() {
    let base = upstream_server().await;
    let f = fixture(&base, true);
    assert!(f.entry.manages_anything());
    assert!(f.entry.manages("opus5"));

    let body = serde_json::json!({
        "model": "opus5", "max_tokens": 100,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let response = f
        .entry
        .handle(WireProtocol::Anthropic, body, 7, "r1".into())
        .await
        .expect("应被接管");
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let answer: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(answer["model"], "opus5", "对外别名不得变成上游真实模型名");
    assert_eq!(answer["content"][0]["text"], "hello");

    let account = f
        .service
        .ledger()
        .unwrap()
        .accounts(7)
        .unwrap()
        .into_iter()
        .find(|a| a.policy.unit == BillingUnit::Cny)
        .unwrap();
    assert_eq!(account.used, "0.003".parse().unwrap());
}

/// 拒绝按客户端说的协议渲染，且原因如实传达。
#[tokio::test]
async fn a_refusal_is_rendered_in_the_client_protocol() {
    // 没有人民币账户 → 403。
    let f = fixture("http://127.0.0.1:1", false);
    let body = serde_json::json!({"model": "opus5", "messages": []});

    let response = f
        .entry
        .handle(WireProtocol::Anthropic, body.clone(), 7, "r1".into())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["type"], "error", "Anthropic 的错误形状");
    assert_eq!(error["error"]["type"], "permission_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("account")
    );

    // 同一个拒绝，OpenAI 形状。
    let response = f
        .entry
        .handle(WireProtocol::ChatCompletions, body, 7, "r2".into())
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(error.get("type").is_none(), "OpenAI 的错误不带顶层 type");
    assert_eq!(error["error"]["type"], "permission_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("account")
    );
}

/// 流式请求走流式通道，带上 SSE 的响应头。
#[tokio::test]
async fn a_streamed_request_gets_an_event_stream_response() {
    let router = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|| async {
            let body = concat!(
                "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n",
                "event: message_delta\ndata: {\"usage\":{\"output_tokens\":500}}\n\n",
                "event: message_stop\ndata: {}\n\n"
            );
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let f = fixture(&format!("http://{addr}"), true);

    let body = serde_json::json!({
        "model": "opus5", "max_tokens": 100, "stream": true,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let response = f
        .entry
        .handle(WireProtocol::Anthropic, body, 7, "r1".into())
        .await
        .expect("应被接管");
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap(),
        "text/event-stream"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("event: message_start"));
    assert!(text.contains("event: message_stop"));
}

// ---------- 穿过真实路由 ----------

/// 最要紧的一条：中间件真的挂上了、排在鉴权之后，且**未接管的请求原样透传**。
///
/// 前两点只能靠真跑一遍路由来证明：单测 `handle` 证明不了层序，也证明不了
/// 不接管时请求毫发无损地落到既有 handler 上。
#[tokio::test]
async fn the_layer_is_wired_after_auth_and_leaves_other_models_alone() {
    let base = upstream_server().await;
    let f = fixture(&base, true);
    let client = crate::gateway::execute::build_client(
        std::time::Duration::from_secs(5),
        crate::model::config::TlsBackend::Rustls,
        None,
    )
    .unwrap();
    let entry = Arc::new(GatewayEntry::new(f.service.clone(), client));

    // 一个真实的入口 Key，其 id 必须是网关账户所属的 7。
    let keys = Arc::new(crate::admin::client_keys::ClientKeyManager::new());
    let key = keys.create_with_key("t".into(), None, None, "sk-test-key".into());
    f.service
        .ledger()
        .unwrap()
        .set_account(
            key.id,
            BudgetPolicy {
                unit: BillingUnit::Cny,
                limit: Some("100".parse().unwrap()),
                enforcement: BudgetEnforcement::Soft,
                max_in_flight: 8,
                max_pending: 16,
                allowed_models: vec![],
                allowed_upstreams: vec![],
            },
        )
        .unwrap();

    // 不带 KiroProvider：既有路径此时必然回 503，正好用来证明"透传到了既有 handler"。
    let app = crate::anthropic::create_router_with_shared_provider(
        None,
        false,
        crate::model::config::ToolCompatibilityMode::default(),
        Some(keys.clone()),
        None,
        None,
        None,
        None,
        Some(entry),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::new();

    // 1) 被接管的别名：网关应答，没有 KiroProvider 也照样成功。
    let response = http
        .post(format!("http://{addr}/v1/messages"))
        .header("x-api-key", "sk-test-key")
        .json(&serde_json::json!({
            "model": "opus5", "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "被接管的别名应由网关直接应答");
    let answer: Value = response.json().await.unwrap();
    assert_eq!(answer["model"], "opus5");

    // 2) 未接管的别名：原样落到既有 handler（此处必然是 503），网关一个字节都没改。
    let response = http
        .post(format!("http://{addr}/v1/messages"))
        .header("x-api-key", "sk-test-key")
        .json(&serde_json::json!({
            "model": "claude-sonnet-4", "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        503,
        "未接管的别名必须原样交给既有路径，由它按自己的规则处理"
    );

    // 3) 没有身份就到不了网关：鉴权在外层先拦下。
    let response = http
        .post(format!("http://{addr}/v1/messages"))
        .json(&serde_json::json!({"model": "opus5", "max_tokens": 1, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401, "鉴权必须排在网关之外");
}
