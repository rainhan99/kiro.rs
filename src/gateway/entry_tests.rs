use super::*;
use crate::gateway::{
    BillingUnit, BudgetEnforcement, BudgetPolicy, GatewayConfig, ModelBinding, PublicModel,
    RoutingMode, TokenPrices, Upstream, UpstreamKind,
};
use axum::http::StatusCode;

impl Handled {
    fn is_not_managed(&self) -> bool {
        matches!(self, Self::NotManaged)
    }
    fn into_response_or_panic(self) -> Response {
        match self {
            Self::Response(response) => response,
            Self::NotManaged => panic!("应被接管，实得 NotManaged"),
            Self::UseKiro(_) => panic!("应由网关直接应答，实得 UseKiro"),
        }
    }
}
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
            .is_not_managed()
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
            .is_not_managed()
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
        .into_response_or_panic();
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
        .into_response_or_panic();
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
        .into_response_or_panic();
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
        .into_response_or_panic();
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

/// `/v1/models` 必须如实列出网关接管的别名，且**没有 Kiro 也列得出来**——
/// 这些别名与 Kiro 凭据无关，Kiro 那边不可用不该让它们从列表里消失。
#[tokio::test]
async fn managed_aliases_are_listed_even_without_a_kiro_provider() {
    let f = fixture("http://127.0.0.1:1", true);
    let client = crate::gateway::execute::build_client(
        std::time::Duration::from_secs(5),
        crate::model::config::TlsBackend::Rustls,
        None,
    )
    .unwrap();
    let entry = Arc::new(GatewayEntry::new(f.service.clone(), client));

    let keys = Arc::new(crate::admin::client_keys::ClientKeyManager::new());
    keys.create_with_key("t".into(), None, None, "sk-test-key".into());

    let app = crate::anthropic::create_router_with_shared_provider(
        None, // 没有 KiroProvider
        false,
        crate::model::config::ToolCompatibilityMode::default(),
        Some(keys),
        None,
        None,
        None,
        None,
        Some(entry),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let response = reqwest::Client::new()
        .get(format!("http://{addr}/v1/models"))
        .header("x-api-key", "sk-test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "有网关模型时不该回 503");

    let listed: Value = response.json().await.unwrap();
    assert_eq!(listed["object"], "list");
    let models = listed["data"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], "opus5");
    assert_eq!(models[0]["owned_by"], "gateway");
    // 能力如实来自绑定声明，不是编的。
    assert_eq!(models[0]["context_window"], 200_000);
    assert_eq!(models[0]["max_tokens"], 8_000);
}

/// 额度判定从鉴权挪到了知道模型名的那一层。三种情形必须各自正确：
///
/// 1. 积分耗尽的 Key 请求**网关接管**的按钱计费别名 → 放行（账本按路判定）；
/// 2. 同一个 Key 请求**未接管**的别名 → 仍按遗留计数器拒绝，措辞不变；
/// 3. 网关没接管任何模型时 → 与从前逐字一致，鉴权层直接拒绝。
///
/// 第 1 条是整件事的目的：一个积分用完、但人民币账户有余额的 Key，
/// 走按钱计费的路完全应该放行。从前它在鉴权层就被拦死了。
#[tokio::test]
async fn a_credit_exhausted_key_still_reaches_a_money_route() {
    let base = upstream_server().await;
    let f = fixture(&base, false);
    let client = crate::gateway::execute::build_client(
        std::time::Duration::from_secs(5),
        crate::model::config::TlsBackend::Rustls,
        None,
    )
    .unwrap();
    let entry = Arc::new(GatewayEntry::new(f.service.clone(), client));

    let keys = Arc::new(crate::admin::client_keys::ClientKeyManager::new());
    let key = keys.create_with_key("t".into(), None, None, "sk-test-key".into());
    // 积分用满：遗留计数器判定为耗尽。
    assert!(keys.set_max_credits(key.id, Some(10.0)));
    keys.record_usage(key.id, 0, 0, 0, 0, 10.0);
    // 但人民币账户有余额。
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

    // 1) 接管的别名：放行。
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
    assert_eq!(
        response.status(),
        200,
        "积分耗尽不该挡住一条按钱计费、且账户有余额的路"
    );

    // 2) 未接管的别名：仍按遗留计数器拒绝。
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
    assert_eq!(response.status(), 429, "遗留路径仍由遗留计数器管着");
    let error: Value = response.json().await.unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("积分使用上限"),
        "措辞不得变，运维与客户端都认得它"
    );
}

/// 没有网关时行为与从前逐字一致：鉴权层直接 429。
#[tokio::test]
async fn without_a_gateway_the_legacy_limit_still_stops_at_auth() {
    let keys = Arc::new(crate::admin::client_keys::ClientKeyManager::new());
    let key = keys.create_with_key("t".into(), None, None, "sk-test-key".into());
    assert!(keys.set_max_credits(key.id, Some(10.0)));
    keys.record_usage(key.id, 0, 0, 0, 0, 10.0);

    let app = crate::anthropic::create_router_with_shared_provider(
        None,
        false,
        crate::model::config::ToolCompatibilityMode::default(),
        Some(keys),
        None,
        None,
        None,
        None,
        None, // 没有网关
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-api-key", "sk-test-key")
        .json(&serde_json::json!({"model": "anything", "max_tokens": 1, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
}

/// Kiro 路的端到端：网关选路并预留 → 交回既有通道 → 既有通道执行失败 →
/// 用量汇聚点把这笔**释放掉**，而不是留一条挂死的在飞记录。
///
/// 这里没有 KiroProvider，所以既有通道必然以 503 收场——正好用来验证失败侧的结算。
/// 成功侧需要真实凭据与上游，不在离线测试范围内；那条路上的金额换算由
/// `settlement` 的往返测试单独钉住。
#[tokio::test]
async fn a_kiro_route_reserves_then_releases_when_the_legacy_path_fails() {
    let config_path = temp("config.json");
    let ledger_path = temp("billing.db");
    let kiro = Upstream {
        id: "u1".into(),
        name: "u1".into(),
        kind: UpstreamKind::Kiro,
        enabled: true,
        weight: 10,
        base_url: None,
        api_key: None,
        has_api_key: false,
        allow_private_network: false,
        kiro_group: Some("team-a".into()),
        cache_usage_policy: None,
    };
    let mut credit = binding();
    credit.billing_unit = BillingUnit::KiroCredit;
    credit.cost_prices = None;
    credit.sell_prices = None;
    let config = GatewayConfig {
        models: vec![PublicModel {
            id: "opus5".into(),
            display_name: None,
            routing_mode: Some(RoutingMode::Sticky),
            affinity_ttl_secs: None,
            bindings: vec![credit],
        }],
        upstreams: vec![kiro],
        ..GatewayConfig::default()
    };
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let service = Arc::new(GatewayService::open(&config_path, &ledger_path).unwrap());

    let keys = Arc::new(crate::admin::client_keys::ClientKeyManager::new());
    let key = keys.create_with_key("t".into(), None, None, "sk-test-key".into());
    crate::gateway::import::import_opening_balances(
        service.ledger().unwrap(),
        &[crate::gateway::import::LegacyKeyBalance {
            key_id: key.id,
            used: 0.0,
            limit: Some(100.0),
        }],
    )
    .unwrap();

    let client = crate::gateway::execute::build_client(
        std::time::Duration::from_secs(5),
        crate::model::config::TlsBackend::Rustls,
        None,
    )
    .unwrap();
    let app = crate::anthropic::create_router_with_shared_provider(
        None, // 没有 KiroProvider：既有通道必然失败
        false,
        crate::model::config::ToolCompatibilityMode::default(),
        Some(keys),
        None,
        None,
        None,
        None,
        Some(Arc::new(GatewayEntry::new(service.clone(), client))),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-api-key", "sk-test-key")
        .json(&serde_json::json!({
            "model": "opus5", "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503, "既有通道无可用 provider");

    let account = service
        .ledger()
        .unwrap()
        .accounts(key.id)
        .unwrap()
        .into_iter()
        .find(|a| a.policy.unit == BillingUnit::KiroCredit)
        .expect("应有积分账户");
    assert_eq!(account.in_flight, 0, "失败的预留必须被释放，不能挂死");
    assert_eq!(
        account.customer_pending, 0,
        "没向下游发过内容就不该留下义务"
    );
    assert_eq!(account.used, crate::gateway::Amount::ZERO);

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 上游报回一个记不进账本的 credit 值（NaN / 负数 / 小到 18 位小数放不下）：
/// 记为**待结算**，绝不按 0 确认——0 的意思是"确认没花钱"，与"不知道花了多少"
/// 是两回事，后者按 0 入账就是白送一次调用。
///
/// 这个分支在真实成功路径上才走得到，而那条路需要真实凭据，所以直接构造用量汇聚点
/// 来打它。
#[tokio::test]
async fn an_unrepresentable_native_credit_settles_as_pending() {
    use crate::gateway::ledger_types::{BoundKind, PriceSnapshot, ReservationInput};

    let config_path = temp("config.json");
    let ledger_path = temp("billing.db");
    let mut credit = binding();
    credit.billing_unit = BillingUnit::KiroCredit;
    credit.cost_prices = None;
    credit.sell_prices = None;
    let config = GatewayConfig {
        models: vec![PublicModel {
            id: "opus5".into(),
            display_name: None,
            routing_mode: None,
            affinity_ttl_secs: None,
            bindings: vec![credit],
        }],
        upstreams: vec![Upstream {
            kind: UpstreamKind::Kiro,
            base_url: None,
            api_key: None,
            has_api_key: false,
            ..upstream("unused")
        }],
        ..GatewayConfig::default()
    };
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let service = Arc::new(GatewayService::open(&config_path, &ledger_path).unwrap());
    let ledger = service.ledger().unwrap();
    crate::gateway::import::import_opening_balances(
        ledger,
        &[crate::gateway::import::LegacyKeyBalance {
            key_id: 7,
            used: 0.0,
            limit: Some(100.0),
        }],
    )
    .unwrap();
    ledger
        .reserve(ReservationInput {
            request_id: "r1".into(),
            attempt_id: "r1:1".into(),
            key_id: 7,
            unit: BillingUnit::KiroCredit,
            public_model: "opus5".into(),
            upstream_id: "u1".into(),
            upper_bound: None,
            bound_kind: BoundKind::Unknown,
            snapshot: PriceSnapshot {
                config_revision: 1,
                price_revision: 1,
                binding_id: "b1".into(),
                upstream_kind: UpstreamKind::Kiro,
                upstream_model: "real-model".into(),
                cost_prices: None,
                sell_prices: None,
            },
        })
        .unwrap();

    let hook = crate::anthropic::handlers::UsageRecordHook {
        recorder: None,
        aggregator: None,
        client_keys: None,
        key_id: 7,
        model: "opus5".into(),
        started_at: std::time::Instant::now(),
        settlement: Some(Arc::new(crate::gateway::settlement::Settlement::new(
            service.clone(),
            "r1:1".into(),
            BillingUnit::KiroCredit,
        ))),
    };
    // 成功，但上游报回的 credit 记不进账本。
    hook.record(1, 100, 50, 0, 0, f64::NAN, "success");

    let account = ledger
        .accounts(7)
        .unwrap()
        .into_iter()
        .find(|a| a.policy.unit == BillingUnit::KiroCredit)
        .unwrap();
    assert_eq!(account.in_flight, 0);
    assert_eq!(
        account.used,
        crate::gateway::Amount::ZERO,
        "换算不了就不该按 0 确认计费"
    );
    assert_eq!(account.customer_pending, 1, "必须留下待结算义务");

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 已删 Key 的身份**永不重用**。
///
/// `next_id` 是按存活 Key 的最大 id 推出来的，所以删掉 id 最大的那个 Key 再重启，
/// 下一个新建的 Key 会拿到同一个 id——连同账本里前一个同 id Key 的余额与历史。
/// 账本记得所有出现过的 key_id，启动时用它把下界抬上来。
#[tokio::test]
async fn a_deleted_key_id_is_never_handed_to_a_new_key() {
    let keys_path = temp("keys.json");
    let config_path = temp("config.json");
    let ledger_path = temp("billing.db");
    let mut credit = binding();
    credit.billing_unit = BillingUnit::KiroCredit;
    credit.cost_prices = None;
    credit.sell_prices = None;
    let config = GatewayConfig {
        models: vec![PublicModel {
            id: "opus5".into(),
            display_name: None,
            routing_mode: None,
            affinity_ttl_secs: None,
            bindings: vec![credit],
        }],
        upstreams: vec![Upstream {
            kind: UpstreamKind::Kiro,
            base_url: None,
            api_key: None,
            has_api_key: false,
            ..upstream("unused")
        }],
        ..GatewayConfig::default()
    };
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let service = Arc::new(GatewayService::open(&config_path, &ledger_path).unwrap());

    // 三个 Key，第三个在账本上有余额。
    let keys = crate::admin::client_keys::ClientKeyManager::load(keys_path.clone()).unwrap();
    for name in ["a", "b", "c"] {
        keys.create(name.into(), None, None);
    }
    crate::gateway::import::import_opening_balances(
        service.ledger().unwrap(),
        &[crate::gateway::import::LegacyKeyBalance {
            key_id: 3,
            used: 40.0,
            limit: Some(100.0),
        }],
    )
    .unwrap();

    // 删掉 id 最大的那个，然后重启（重新 load）。
    assert!(keys.delete(3));
    drop(keys);
    let reloaded = crate::admin::client_keys::ClientKeyManager::load(keys_path.clone()).unwrap();

    // 没有这道保护的话，新建的 Key 会拿到 id 3。
    let highest = service.highest_recorded_key_id().unwrap().unwrap();
    assert_eq!(highest, 3, "账本记得 3 号出现过");
    assert!(reloaded.reserve_ids_through(highest), "下界应被抬高");

    let fresh = reloaded.create("d".into(), None, None);
    assert_eq!(fresh.id, 4, "绝不能重用 3 号——那会继承前一个 Key 的余额");

    // 3 号的历史仍在账本上，没有因为 Key 被删而消失。
    let account = service
        .ledger()
        .unwrap()
        .accounts(3)
        .unwrap()
        .into_iter()
        .find(|a| a.policy.unit == BillingUnit::KiroCredit)
        .expect("删 Key 不该删掉账本历史");
    assert_eq!(account.used, "40".parse().unwrap());

    let _ = std::fs::remove_file(keys_path);
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}

/// 「重置统计」碰不到账本：它只清分析视图，恢复不了财务额度。
#[tokio::test]
async fn resetting_stats_cannot_restore_ledger_balance() {
    let keys_path = temp("keys.json");
    let config_path = temp("config.json");
    let ledger_path = temp("billing.db");
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&GatewayConfig {
            models: vec![PublicModel {
                id: "opus5".into(),
                display_name: None,
                routing_mode: None,
                affinity_ttl_secs: None,
                bindings: vec![binding()],
            }],
            upstreams: vec![upstream("https://api.example.test")],
            ..GatewayConfig::default()
        })
        .unwrap(),
    )
    .unwrap();
    let service = GatewayService::open(&config_path, &ledger_path).unwrap();

    let keys = crate::admin::client_keys::ClientKeyManager::load(keys_path.clone()).unwrap();
    let key = keys.create("a".into(), None, None);
    keys.record_usage(key.id, 10, 10, 0, 0, 7.5);
    crate::gateway::import::import_opening_balances(
        service.ledger().unwrap(),
        &[crate::gateway::import::LegacyKeyBalance {
            key_id: key.id,
            used: 7.5,
            limit: Some(10.0),
        }],
    )
    .unwrap();

    assert!(keys.reset_stats(key.id));

    let account = service
        .ledger()
        .unwrap()
        .accounts(key.id)
        .unwrap()
        .into_iter()
        .find(|a| a.policy.unit == BillingUnit::KiroCredit)
        .unwrap();
    assert_eq!(
        account.used,
        "7.5".parse().unwrap(),
        "重置统计只清分析视图，动不了账本"
    );
    assert_eq!(account.available, Some("2.5".parse().unwrap()));

    let _ = std::fs::remove_file(keys_path);
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(ledger_path);
}
