use super::*;
use crate::gateway::{Upstream, UpstreamKind};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// 测试服务器的行为与它实际收到的东西。
#[derive(Default)]
struct Seen {
    hits: AtomicU32,
    headers: parking_lot::Mutex<Vec<(String, String)>>,
    body: parking_lot::Mutex<Vec<u8>>,
}

/// 状态码、附加响应头、报文。
type Reply = (StatusCode, Vec<(String, String)>, String);

#[derive(Clone)]
struct Behaviour {
    seen: Arc<Seen>,
    reply: Arc<dyn Fn() -> Reply + Send + Sync>,
    delay: Option<Duration>,
}

async fn handle(
    State(state): State<Behaviour>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    state.seen.hits.fetch_add(1, Ordering::SeqCst);
    *state.seen.headers.lock() = headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or("<binary>").to_string(),
            )
        })
        .collect();
    *state.seen.body.lock() = body.to_vec();
    if let Some(delay) = state.delay {
        tokio::time::sleep(delay).await;
    }
    let (status, extra, payload) = (state.reply)();
    let mut response = (status, payload).into_response();
    for (name, value) in extra {
        response.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    response
}

struct Server {
    base: String,
    seen: Arc<Seen>,
}

async fn serve(
    reply: impl Fn() -> Reply + Send + Sync + 'static,
    delay: Option<Duration>,
) -> Server {
    let seen = Arc::new(Seen::default());
    let behaviour = Behaviour {
        seen: seen.clone(),
        reply: Arc::new(reply),
        delay,
    };
    let router = axum::Router::new()
        .route("/v1/messages", post(handle))
        .route("/v1/chat/completions", post(handle))
        .route("/elsewhere", post(handle))
        .with_state(behaviour);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server {
        base: format!("http://{addr}"),
        seen,
    }
}

fn ok_reply() -> Reply {
    (StatusCode::OK, vec![], r#"{"ok":true}"#.to_string())
}

/// 明文回环只有显式放行私网才可达，测试上游据此构造。
fn upstream(base: &str, key: Option<&str>) -> Upstream {
    Upstream {
        id: "u1".into(),
        name: "u1".into(),
        kind: UpstreamKind::Anthropic,
        enabled: true,
        weight: 10,
        base_url: Some(base.into()),
        api_key: key.map(str::to_string),
        has_api_key: key.is_some(),
        allow_private_network: true,
        kiro_group: None,
        cache_usage_policy: None,
    }
}

fn client() -> Client {
    build_client(Duration::from_secs(5), TlsBackend::Rustls, None).unwrap()
}

/// 成功时响应**原样**交回，报文没有被读过——否则流式就退化成了非流式。
/// 同时确认出站头只来自该上游的配置。
#[tokio::test]
async fn a_successful_response_is_handed_back_unread_with_only_configured_headers() {
    let server = serve(ok_reply, None).await;
    let response = open(
        &client(),
        &upstream(&server.base, Some("secret-key")),
        WireProtocol::Anthropic,
        br#"{"model":"m"}"#.to_vec(),
        Duration::from_secs(5),
    )
    .await
    .expect("应成功");

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), r#"{"ok":true}"#);

    let headers = server.seen.headers.lock().clone();
    let get = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    assert_eq!(get("x-api-key").as_deref(), Some("secret-key"));
    assert_eq!(get("anthropic-version").as_deref(), Some("2023-06-01"));
    assert_eq!(get("content-type").as_deref(), Some("application/json"));
    assert_eq!(get("authorization"), None, "Anthropic 协议不该带 Bearer");
    assert_eq!(*server.seen.body.lock(), br#"{"model":"m"}"#.to_vec());
}

/// 最关键的一条：**重定向绝不跟随**。一个通过了目标校验的主机回个 302，
/// 就能把带着凭据的请求引到任意地址——包括云元数据服务。
#[tokio::test]
async fn a_redirect_is_never_followed() {
    let elsewhere = serve(ok_reply, None).await;
    let target = format!("{}/elsewhere", elsewhere.base);
    let server = serve(
        move || {
            (
                StatusCode::FOUND,
                vec![("location".into(), target.clone())],
                String::new(),
            )
        },
        None,
    )
    .await;

    let error = open(
        &client(),
        &upstream(&server.base, Some("secret-key")),
        WireProtocol::Anthropic,
        b"{}".to_vec(),
        Duration::from_secs(5),
    )
    .await
    .expect_err("302 不是成功");

    assert!(matches!(
        error,
        SendError::Upstream(UpstreamFailure::InvalidRequest { status: 302, .. })
    ));
    assert_eq!(
        elsewhere.seen.hits.load(Ordering::SeqCst),
        0,
        "被指向的那台服务器一次都不该被碰到"
    );
}

/// 限流要保留 Retry-After，且判为可重试。
#[tokio::test]
async fn a_throttled_response_keeps_its_retry_after() {
    let server = serve(
        || {
            (
                StatusCode::TOO_MANY_REQUESTS,
                vec![("retry-after".into(), "30".into())],
                r#"{"error":{"type":"rate_limit"}}"#.to_string(),
            )
        },
        None,
    )
    .await;

    let error = open(
        &client(),
        &upstream(&server.base, None),
        WireProtocol::Anthropic,
        b"{}".to_vec(),
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();

    assert!(error.is_retryable());
    let SendError::Upstream(UpstreamFailure::Throttled { retry_after, .. }) = error else {
        panic!("429 应判为限流");
    };
    assert_eq!(retry_after.as_deref(), Some("30"));
}

/// 5xx 是瞬态的，可重试；报文留证。
#[tokio::test]
async fn a_server_error_is_transient_and_keeps_its_snippet() {
    let server = serve(
        || {
            (
                StatusCode::BAD_GATEWAY,
                vec![],
                "upstream pool empty".to_string(),
            )
        },
        None,
    )
    .await;

    let error = open(
        &client(),
        &upstream(&server.base, None),
        WireProtocol::Anthropic,
        b"{}".to_vec(),
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();

    assert!(error.is_retryable());
    let SendError::Upstream(UpstreamFailure::Transient { status, snippet }) = error else {
        panic!("502 应判为瞬态");
    };
    assert_eq!(status, 502);
    assert_eq!(snippet, "upstream pool empty");
}

/// 私网**字面量**在端点构造时就被拒，且不算可重试——换同一条路再来也一样。
#[tokio::test]
async fn a_private_literal_is_refused_when_the_endpoint_is_built() {
    let mut u = upstream("https://10.0.0.1", Some("secret-key"));
    u.allow_private_network = false;

    let error = open(
        &client(),
        &u,
        WireProtocol::Anthropic,
        b"{}".to_vec(),
        Duration::from_millis(200),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, SendError::Refused(_)));
    assert!(!error.is_retryable());
    assert!(format!("{error}").contains("10.0.0.1"));
}

/// 只看字面量拦不住一个**解析**到私网的域名——这是两道不同的检查，
/// 各自都得有测试。（最初只有上面那条字面量的，于是删掉连接前的解析检查，
/// 八个测试照过。）`localhost` 走 /etc/hosts，离线也成立。
#[tokio::test]
async fn a_hostname_resolving_into_the_private_range_is_refused_before_connecting() {
    let server = serve(ok_reply, None).await;
    let port = server.base.rsplit(':').next().unwrap().to_string();
    let mut u = upstream(&format!("https://localhost:{port}"), Some("secret-key"));
    u.allow_private_network = false;

    let error = open(
        &client(),
        &u,
        WireProtocol::Anthropic,
        b"{}".to_vec(),
        Duration::from_millis(500),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(error, SendError::Refused(_)),
        "解析到回环必须在连接前被拒，实得 {error:?}"
    );
    assert_eq!(
        server.seen.hits.load(Ordering::SeqCst),
        0,
        "不该建立任何连接"
    );
}

/// 无论怎么失败，凭据都不得出现在错误里。
#[tokio::test]
async fn an_error_never_carries_the_api_key() {
    let server = serve(
        || (StatusCode::UNAUTHORIZED, vec![], "bad key".to_string()),
        None,
    )
    .await;

    let error = open(
        &client(),
        &upstream(&server.base, Some("sk-super-secret-value")),
        WireProtocol::Anthropic,
        b"{}".to_vec(),
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();

    for rendered in [format!("{error}"), format!("{error:?}")] {
        assert!(
            !rendered.contains("sk-super-secret-value"),
            "凭据泄进了错误：{rendered}"
        );
    }
}

/// 超时按中断上报，且可重试。
#[tokio::test]
async fn a_timeout_is_reported_as_an_interruption() {
    let server = serve(ok_reply, Some(Duration::from_secs(5))).await;

    let error = open(
        &client(),
        &upstream(&server.base, None),
        WireProtocol::Anthropic,
        b"{}".to_vec(),
        Duration::from_millis(150),
    )
    .await
    .unwrap_err();

    assert!(error.is_retryable());
    let SendError::Upstream(UpstreamFailure::StreamInterrupted { detail }) = error else {
        panic!("超时应判为中断，实得 {error:?}");
    };
    assert_eq!(detail, "timed out");
}

/// OpenAI 系协议走 Bearer，且路径由协议决定而非调用方。
#[tokio::test]
async fn the_openai_protocol_sends_a_bearer_on_its_own_path() {
    let server = serve(ok_reply, None).await;
    open(
        &client(),
        &upstream(&server.base, Some("secret-key")),
        WireProtocol::ChatCompletions,
        b"{}".to_vec(),
        Duration::from_secs(5),
    )
    .await
    .expect("应成功");

    let headers = server.seen.headers.lock().clone();
    let get = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    assert_eq!(get("authorization").as_deref(), Some("Bearer secret-key"));
    assert_eq!(get("x-api-key"), None);
}
