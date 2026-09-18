//! 直连上游的执行器：把 [`super::execute`] 接到两套分发的执行器接口上。
//!
//! Kiro 路不走这里——它要复用既有的凭据池与请求流水线，由另一个执行器承接。

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;

use super::Upstream;
use super::dispatch::RouteExecutor;
use super::execute::{SendError, open};
use super::protocol::WireProtocol;
use super::streaming::{ByteStream, StreamExecutor};
use super::transport::UpstreamFailure;

pub struct DirectExecutor {
    client: Client,
}

impl DirectExecutor {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

impl RouteExecutor for DirectExecutor {
    fn execute<'a>(
        &'a self,
        upstream: &'a Upstream,
        protocol: WireProtocol,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Value, SendError>> + Send + 'a>> {
        Box::pin(async move {
            let response = open(&self.client, upstream, protocol, body, timeout).await?;
            // 非流式：读完整报文。读坏了按中断处理——上游可能已经算过这笔钱。
            response.json::<Value>().await.map_err(|e| {
                SendError::Upstream(UpstreamFailure::StreamInterrupted {
                    detail: format!("response body was not valid JSON: {}", kind(&e)),
                })
            })
        })
    }
}

impl StreamExecutor for DirectExecutor {
    fn open_stream(
        &self,
        upstream: Upstream,
        protocol: WireProtocol,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<ByteStream, SendError>> + Send + '_>> {
        Box::pin(async move {
            let response = open(&self.client, &upstream, protocol, body, timeout).await?;
            // 报文一个字节都没读过，逐块交出去。
            let stream = response.bytes_stream().map(|chunk| {
                chunk.map_err(|e| {
                    SendError::Upstream(UpstreamFailure::StreamInterrupted { detail: kind(&e) })
                })
            });
            Ok(Box::pin(stream) as ByteStream)
        })
    }
}

/// 只取错误类别。reqwest 的错误链会带上完整 URL 与底层细节，不进证据。
fn kind(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".into()
    } else if error.is_connect() {
        "could not connect".into()
    } else if error.is_decode() {
        "could not decode".into()
    } else {
        "stream failed".into()
    }
}
