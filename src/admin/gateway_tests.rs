//! 真实回环 HTTP 与临时文件；不碰推理上游，也不碰现网配置。
use crate::admin::client_keys::ClientKeyManager;
use crate::admin::groups::GroupManager;
use crate::admin::middleware::AdminState;
use crate::admin::router::create_admin_router;
use crate::admin::service::AdminService;
use crate::admin::trace_db::TraceStore;
use crate::admin::usage_stats::UsageAggregator;
use crate::gateway::service::GatewayService;
use crate::{kiro::token_manager::MultiTokenManager, model::config::Config};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

const ADMIN_KEY: &str = "gateway-fixture-admin";

struct Fixture {
    directory: PathBuf,
    gateway: Arc<GatewayService>,
    client: reqwest::Client,
    base: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn sample_config(base: &str) -> Value {
    json!({
        "defaultRoutingMode": "sticky",
        "affinityTtlSecs": 900,
        "requestTimeoutSecs": 120,
        "maxAttempts": 3,
        "upstreams": [{
            "id": "u1", "name": "主用", "kind": "anthropic", "enabled": true, "weight": 10,
            "baseUrl": base, "apiKey": "super-secret-upstream-key"
        }],
        "models": [{
            "id": "opus5",
            "bindings": [{
                "id": "b1", "upstreamId": "u1", "upstreamModel": "real-model",
                "enabled": true, "priorityTier": 0, "weight": 10,
                "contextWindow": 200000, "maxOutputTokens": 8000,
                "supportsTools": true, "supportsImages": true, "supportsReasoning": false,
                "billingUnit": "CNY",
                "costPrices": {"currency": "CNY", "input": "1", "output": "1", "cacheRead": "0", "cacheWrite": "0"},
                "sellPrices": {"currency": "CNY", "input": "2", "output": "2", "cacheRead": "0", "cacheWrite": "0"}
            }]
        }]
    })
}

impl Fixture {
    async fn new(with_gateway: bool) -> Self {
        let directory =
            std::env::temp_dir().join(format!("kiro-web-gateway-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let gateway_path = directory.join("gateway.json");
        if with_gateway {
            std::fs::write(
                &gateway_path,
                serde_json::to_vec_pretty(&sample_config("https://api.example.test")).unwrap(),
            )
            .unwrap();
        }
        let gateway =
            Arc::new(GatewayService::open(&gateway_path, &directory.join("billing.db")).unwrap());

        let manager =
            Arc::new(MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap());
        let state = AdminState::new(
            ADMIN_KEY,
            AdminService::new(manager.clone(), vec![]),
            Arc::new(ClientKeyManager::new()),
            Arc::new(UsageAggregator::new()),
            Arc::new(TraceStore::open_in_memory().unwrap()),
            Arc::new(GroupManager::new()),
        )
        .with_gateway(Some(gateway.clone()));

        let api = create_admin_router(state);
        let router = axum::Router::new().nest("/api/admin", api);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/admin", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            directory,
            gateway,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
            base,
            task,
        }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .send()
            .await
            .unwrap()
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: &Value) -> reqwest::Response {
        self.client
            .request(method, format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .json(body)
            .send()
            .await
            .unwrap()
    }
}

/// 未认证的调用既读不到也改不了配置——上游密钥就在这份配置里。
#[tokio::test]
async fn gateway_config_is_unreachable_without_admin_authentication() {
    let f = Fixture::new(true).await;
    let anonymous = reqwest::Client::builder().no_proxy().build().unwrap();

    let response = anonymous
        .get(format!("{}/gateway/config", f.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    let response = anonymous
        .put(format!("{}/gateway/config", f.base))
        .json(&json!({"revision": 0, "config": sample_config("https://evil.test")}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // 配置没被改动。
    assert_eq!(
        f.gateway.snapshot().config.upstreams[0].base_url.as_deref(),
        Some("https://api.example.test")
    );
}

/// 读配置时密钥必须是掩码；原值不得出现在响应里的任何位置。
#[tokio::test]
async fn reading_the_configuration_never_exposes_upstream_secrets() {
    let f = Fixture::new(true).await;
    let body: Value = f.get("/gateway/config").await.json().await.unwrap();

    assert_eq!(body["managedModels"], json!(["opus5"]));
    let rendered = body.to_string();
    assert!(
        !rendered.contains("super-secret-upstream-key"),
        "上游密钥泄露到了管理接口：{rendered}"
    );
    assert!(
        body["config"]["upstreams"][0]["hasApiKey"] == json!(true),
        "但要如实告诉运维这里配了密钥"
    );
}

/// 保存掩码值**不得**把真实密钥替换成那串星号。
#[tokio::test]
async fn saving_a_masked_secret_keeps_the_real_one() {
    let f = Fixture::new(true).await;
    let current: Value = f.get("/gateway/config").await.json().await.unwrap();
    let mut config = current["config"].clone();
    // 原样把读到的（掩码的）配置回写，只改一个无关字段。
    config["affinityTtlSecs"] = json!(600);

    let response = f
        .send(
            reqwest::Method::PUT,
            "/gateway/config",
            &json!({"revision": current["revision"], "config": config}),
        )
        .await;
    assert_eq!(response.status(), 200, "{:?}", response.text().await);

    assert_eq!(
        f.gateway.snapshot().config.upstreams[0].api_key.as_deref(),
        Some("super-secret-upstream-key"),
        "掩码回写把真实密钥抹掉了"
    );
    assert_eq!(f.gateway.snapshot().config.affinity_ttl_secs, 600);
}

/// 保存之后再读，读到的就是刚保存的那一版。
#[tokio::test]
async fn an_update_is_visible_at_the_revision_it_reports() {
    let f = Fixture::new(true).await;
    let current: Value = f.get("/gateway/config").await.json().await.unwrap();
    let mut config = current["config"].clone();
    config["maxAttempts"] = json!(2);

    let saved: Value = f
        .send(
            reqwest::Method::PUT,
            "/gateway/config",
            &json!({"revision": current["revision"], "config": config}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_ne!(saved["revision"], current["revision"], "保存必须推进版本号");

    let reread: Value = f.get("/gateway/config").await.json().await.unwrap();
    assert_eq!(reread["revision"], saved["revision"]);
    assert_eq!(reread["config"]["maxAttempts"], json!(2));
}

/// 过期版本是冲突，不是「后写者胜」——否则两个人同时编辑，先保存的那个静静消失。
#[tokio::test]
async fn a_stale_revision_is_a_conflict_not_last_writer_wins() {
    let f = Fixture::new(true).await;
    let current: Value = f.get("/gateway/config").await.json().await.unwrap();
    let stale = current["revision"].clone();
    let mut first = current["config"].clone();
    first["maxAttempts"] = json!(2);
    f.send(
        reqwest::Method::PUT,
        "/gateway/config",
        &json!({"revision": stale, "config": first}),
    )
    .await;

    // 第二个人还拿着旧版本。
    let mut second = current["config"].clone();
    second["maxAttempts"] = json!(5);
    let response = f
        .send(
            reqwest::Method::PUT,
            "/gateway/config",
            &json!({"revision": stale, "config": second}),
        )
        .await;
    assert_eq!(response.status(), 409);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "configuration_conflict");

    assert_eq!(
        f.gateway.snapshot().config.max_attempts,
        2,
        "先保存的那份必须还在"
    );
}

/// 不合法的配置以 `invalid_configuration` 被拒，且**一个字节都不写盘**。
#[tokio::test]
async fn an_invalid_configuration_is_rejected_without_writing() {
    let f = Fixture::new(true).await;
    let current: Value = f.get("/gateway/config").await.json().await.unwrap();
    let mut config = current["config"].clone();
    // 绑定指向一个不存在的上游。
    config["models"][0]["bindings"][0]["upstreamId"] = json!("nope");

    let response = f
        .send(
            reqwest::Method::PUT,
            "/gateway/config",
            &json!({"revision": current["revision"], "config": config}),
        )
        .await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_configuration");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("nope"),
        "要指明是哪里不对：{body}"
    );

    assert_eq!(
        f.gateway.snapshot().revision,
        current["revision"].as_u64().unwrap(),
        "拒绝的更新不得改变版本"
    );
}

/// 未配置网关时，这些端点一律 404——而不是回一个空壳让人以为配好了。
#[tokio::test]
async fn an_unconfigured_gateway_reports_that_plainly() {
    let f = Fixture::new(false).await;
    for path in [
        "/gateway/config",
        "/gateway/requests?keyId=1",
        "/client-keys/1/budgets",
    ] {
        let response = f.get(path).await;
        assert_eq!(response.status(), 404, "{path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "gateway_not_configured", "{path}");
    }
}

/// 额度用十进制字符串来回传递，绝不经过浮点。
///
/// 取的这个值 f64 **存不下**（18 位小数，超过双精度约 17 位有效十进制位）。
/// 用一个恰好能被 f64 往返的数字做断言，等于什么也没测出来。
#[tokio::test]
async fn budgets_round_trip_as_decimal_strings() {
    let f = Fixture::new(true).await;
    let response = f
        .send(
            reqwest::Method::PUT,
            "/client-keys/7/budgets",
            &json!({
                "unit": "CNY", "limit": "0.123456789012345678", "enforcement": "soft",
                "maxInFlight": 4, "maxPending": 8
            }),
        )
        .await;
    assert_eq!(response.status(), 200, "{:?}", response.text().await);

    let body: Value = f.get("/client-keys/7/budgets").await.json().await.unwrap();
    let budget = &body["budgets"][0];
    assert_eq!(budget["unit"], "CNY");
    assert_eq!(
        budget["limit"], "0.123456789012345678",
        "小数必须逐位往返，不得经浮点"
    );
    assert_eq!(budget["used"], "0");
    assert_eq!(budget["available"], "0.123456789012345678");
}

/// 「不限额」与「一分都不能花」是两回事，不能都渲染成同一个值。
#[tokio::test]
async fn unlimited_and_zero_are_not_the_same_budget() {
    let f = Fixture::new(true).await;
    f.send(
        reqwest::Method::PUT,
        "/client-keys/7/budgets",
        &json!({"unit": "CNY", "limit": null, "enforcement": "soft", "maxInFlight": 4, "maxPending": 8}),
    )
    .await;
    let body: Value = f.get("/client-keys/7/budgets").await.json().await.unwrap();
    assert_eq!(body["budgets"][0]["limit"], Value::Null, "不限额");
    assert_eq!(body["budgets"][0]["available"], Value::Null);

    f.send(
        reqwest::Method::PUT,
        "/client-keys/7/budgets",
        &json!({"unit": "CNY", "limit": "0", "enforcement": "soft", "maxInFlight": 4, "maxPending": 8}),
    )
    .await;
    let body: Value = f.get("/client-keys/7/budgets").await.json().await.unwrap();
    assert_eq!(body["budgets"][0]["limit"], "0", "零额度");
    assert_eq!(body["budgets"][0]["available"], "0");
}

/// 改额度政策不得顺手抹掉已用量——那是有审计的调整操作才能做的事。
#[tokio::test]
async fn changing_a_limit_does_not_erase_what_was_spent() {
    let f = Fixture::new(true).await;
    f.send(
        reqwest::Method::PUT,
        "/client-keys/7/budgets",
        &json!({"unit": "CNY", "limit": "100", "enforcement": "soft", "maxInFlight": 4, "maxPending": 8}),
    )
    .await;
    // 记一笔支出。
    f.send(
        reqwest::Method::POST,
        "/client-keys/7/adjustments",
        &json!({"adjustmentId": "a1", "unit": "CNY", "direction": "debit", "amount": "30", "reason": "试用"}),
    )
    .await;

    // 把上限改小。
    f.send(
        reqwest::Method::PUT,
        "/client-keys/7/budgets",
        &json!({"unit": "CNY", "limit": "50", "enforcement": "soft", "maxInFlight": 4, "maxPending": 8}),
    )
    .await;

    let body: Value = f.get("/client-keys/7/budgets").await.json().await.unwrap();
    assert_eq!(body["budgets"][0]["used"], "30", "已用量不得被改额度抹掉");
    assert_eq!(body["budgets"][0]["available"], "20");
}

/// 调整是幂等的：同一个 id 重复提交只生效一次。网络重试不该重复扣钱。
#[tokio::test]
async fn an_adjustment_with_the_same_id_applies_once() {
    let f = Fixture::new(true).await;
    f.send(
        reqwest::Method::PUT,
        "/client-keys/7/budgets",
        &json!({"unit": "CNY", "limit": "100", "enforcement": "soft", "maxInFlight": 4, "maxPending": 8}),
    )
    .await;
    for _ in 0..3 {
        let response = f
            .send(
                reqwest::Method::POST,
                "/client-keys/7/adjustments",
                &json!({"adjustmentId": "same", "unit": "CNY", "direction": "debit", "amount": "10", "reason": "重试"}),
            )
            .await;
        assert_eq!(response.status(), 200);
    }
    let body: Value = f.get("/client-keys/7/budgets").await.json().await.unwrap();
    assert_eq!(body["budgets"][0]["used"], "10", "重复提交只该扣一次");

    // 审计里留得下这笔。
    let audit: Value = f
        .get("/client-keys/7/ledger-audit")
        .await
        .json()
        .await
        .unwrap();
    assert!(!audit["entries"].as_array().unwrap().is_empty());
}

/// 路由预览只回答「会走哪条路」，**不预留、不发请求**。
#[tokio::test]
async fn preview_explains_the_route_without_reserving_anything() {
    let f = Fixture::new(true).await;
    f.send(
        reqwest::Method::PUT,
        "/client-keys/7/budgets",
        &json!({"unit": "CNY", "limit": "100", "enforcement": "soft", "maxInFlight": 4, "maxPending": 8}),
    )
    .await;

    let body: Value = f
        .send(
            reqwest::Method::POST,
            "/gateway/preview",
            &json!({"keyId": 7, "model": "opus5"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["managed"], true);
    assert_eq!(body["selected"], "b1");
    assert_eq!(body["candidates"][0]["eligible"], true);
    assert_eq!(body["candidates"][0]["upstreamModel"], "real-model");

    // 什么都没被预留。
    let budgets: Value = f.get("/client-keys/7/budgets").await.json().await.unwrap();
    assert_eq!(budgets["budgets"][0]["inFlight"], 0);
    assert_eq!(budgets["budgets"][0]["reserved"], "0");
}

/// 预览要说清**为什么**不可用，而不是只给一个空结果。
#[tokio::test]
async fn preview_names_the_reason_a_route_is_unavailable() {
    let f = Fixture::new(true).await;
    // 7 号没有人民币账户。
    let body: Value = f
        .send(
            reqwest::Method::POST,
            "/gateway/preview",
            &json!({"keyId": 7, "model": "opus5"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["managed"], true);
    assert_eq!(body["selected"], Value::Null);
    assert_eq!(body["candidates"][0]["eligible"], false);
    assert!(
        body["candidates"][0]["refusal"]
            .as_str()
            .unwrap()
            .contains("NoAccountForUnit"),
        "要指出是「没有这个币种的账户」：{body}"
    );

    // 未接管的别名如实说不接管。
    let body: Value = f
        .send(
            reqwest::Method::POST,
            "/gateway/preview",
            &json!({"keyId": 7, "model": "something-else"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["managed"], false);
}
