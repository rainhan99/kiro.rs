//! 把构造好的请求真正发出去。
//!
//! 这一层只负责**把请求送到线上并对状态行下判定**：2xx 时把响应原样交回调用方，
//! 由它决定缓冲还是流式转发；非 2xx 时读走报文并交给 [`super::transport::classify`]。
//! 网关不替调用方决定是否缓冲——对 SSE 来说缓冲等于把流式变成非流式。
//!
//! # 与 `crate::http_client::build_client` 的区别
//!
//! 通用 client **跟随重定向**。对网关这是个真窟窿：[`super::transport`] 校验的是
//! 字面目标，一个通过了校验的公网主机只要回一个 302，就能把带着 `Authorization`
//! 的请求引到 `169.254.169.254`。所以这里的 client 一律 `redirect::Policy::none()`。
//!
//! # 关于 DNS 重绑定这道残留
//!
//! [`ensure_public_target`] 解析一次做检查，reqwest 连接时会再解析一次；两次之间
//! 应答可以变。要彻底关掉得把首次解析出的地址钉进 client，代价是每个上游一个
//! 连接池。这里**没有**关掉它，理由是：上游是运维在配置里写死的，能左右某个已配置
//! 域名解析结果的攻击者，本来就控制着那个上游。这道残留写在这里，不是忘了。

use std::time::Duration;

use reqwest::{Client, Method, redirect};

use crate::http_client::ProxyConfig;
use crate::model::config::TlsBackend;

use super::Upstream;
use super::protocol::WireProtocol;
use super::transport::{
    UpstreamFailure, classify, ensure_public_target, request_headers, resolve_endpoint,
};

/// 出错时最多读走的报文字节数。上游报错体可能很大，读全了既无意义也给了对方
/// 一个撑爆内存的口子。
const MAX_ERROR_BODY: usize = 64 * 1024;

/// 送不出去与送出去了但失败，是两回事。
#[derive(Debug)]
pub enum SendError {
    /// 请求**没有发出**：端点不合法、目标落在私网、client 建不起来。
    /// 这条路本次不可用，但换一条路可以继续。
    Refused(anyhow::Error),
    /// 请求发出去了，上游给了个失败的答复，或者连接中途断了。
    Upstream(UpstreamFailure),
}

impl SendError {
    /// 换**同一条**路重试是否可能成功。换别的路由协调器另行决定。
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Refused(_) => false,
            Self::Upstream(failure) => failure.is_retryable(),
        }
    }
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(error) => write!(f, "request was not sent: {error:#}"),
            Self::Upstream(failure) => write!(f, "{failure}"),
        }
    }
}

impl std::error::Error for SendError {}

/// 网关自用的 HTTP client。禁用重定向，理由见模块头。
pub fn build_client(
    timeout: Duration,
    tls_backend: TlsBackend,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<Client> {
    let mut builder = Client::builder()
        .timeout(timeout)
        .redirect(redirect::Policy::none());

    match tls_backend {
        TlsBackend::Rustls => builder = builder.use_rustls_tls(),
        TlsBackend::NativeTls => {
            #[cfg(feature = "native-tls")]
            {
                builder = builder.use_native_tls();
            }
            #[cfg(not(feature = "native-tls"))]
            anyhow::bail!("此构建版本未包含 native-tls 后端，请在配置中改用 rustls");
        }
    }

    if let Some(config) = proxy {
        let mut proxy = reqwest::Proxy::all(&config.url)?;
        if let (Some(user), Some(pass)) = (&config.username, &config.password) {
            proxy = proxy.basic_auth(user, pass);
        }
        builder = builder.proxy(proxy);
    }

    Ok(builder.build()?)
}

/// 发出一次请求。
///
/// 成功（2xx）时把 [`reqwest::Response`] 原样交回——**没有读过它的报文**，
/// 调用方可以逐块转发。非 2xx 时在这里读走并分类，因为错误报文小且必须留证。
pub async fn open(
    client: &Client,
    upstream: &Upstream,
    protocol: WireProtocol,
    body: Vec<u8>,
    timeout: Duration,
) -> Result<reqwest::Response, SendError> {
    let endpoint = resolve_endpoint(upstream, protocol).map_err(SendError::Refused)?;
    // 连接前再查一次解析结果：字面量在构造端点时已判过，域名到这里才知道。
    ensure_public_target(&endpoint, &upstream.id).map_err(SendError::Refused)?;

    let mut request = client
        .request(Method::POST, &endpoint.url)
        .timeout(timeout)
        .body(body);
    // 头只来自本上游的配置，不透传客户端任何头部。
    for (name, value) in request_headers(upstream, protocol) {
        request = request.header(name, value);
    }

    let response = request.send().await.map_err(|e| {
        SendError::Upstream(UpstreamFailure::StreamInterrupted {
            detail: describe(&e),
        })
    })?;

    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(response);
    }

    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = read_bounded(response).await;
    Err(SendError::Upstream(classify(
        status,
        &body,
        retry_after.as_deref(),
    )))
}

/// reqwest 的错误链里会带上完整 URL。URL 来自配置而非凭据，但错误链还可能
/// 挂着底层库的细节，所以只取分类，不把整条链塞进证据。
fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".to_string()
    } else if error.is_connect() {
        "could not connect".to_string()
    } else if error.is_body() || error.is_decode() {
        "response body failed".to_string()
    } else {
        "request failed".to_string()
    }
}

async fn read_bounded(response: reqwest::Response) -> String {
    match response.bytes().await {
        Ok(bytes) => {
            let end = bytes.len().min(MAX_ERROR_BODY);
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        }
        Err(_) => String::new(),
    }
}

#[cfg(test)]
#[path = "execute_tests.rs"]
mod tests;
