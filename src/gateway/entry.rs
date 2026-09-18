//! 网关入口：判定是否接管，接管则走完整条链并渲染应答。
//!
//! # 不接管就一个字节都不改
//!
//! [`GatewayEntry::handle`] 返回 `None` 表示"这个别名与网关无关"，调用方原样继续走
//! 既有路径。现有部署在未配置网关时看不出任何差别——这是引入这个特性的前提。
//!
//! # 错误按客户端说的协议渲染
//!
//! 拒绝的原因对客户端有用（余额耗尽、并发过高、这个 Key 没有对应币种的账户），
//! 所以如实传达，但要按它认识的错误形状。

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use reqwest::Client;
use serde_json::{Value, json};

use super::direct::DirectExecutor;
use super::dispatch::{Dispatched, dispatch};
use super::protocol::WireProtocol;
use super::routing::RouteContext;
use super::service::GatewayService;
use super::streaming::{StreamDispatched, dispatch_stream, into_stream};

pub struct GatewayEntry {
    service: Arc<GatewayService>,
    direct: Arc<DirectExecutor>,
}

impl GatewayEntry {
    pub fn new(service: Arc<GatewayService>, client: Client) -> Self {
        Self {
            service: service.clone(),
            direct: Arc::new(DirectExecutor::new(client)),
        }
    }

    /// 网关有没有接管任何模型。没有就完全不必缓冲请求体。
    pub fn manages_anything(&self) -> bool {
        self.service.has_managed_models()
    }

    pub fn manages(&self, alias: &str) -> bool {
        self.service.is_managed(alias)
    }

    /// 对外可见的模型及其能力，供 `/v1/models` 如实列出。
    pub fn public_models(&self) -> Vec<super::service::PublicModelCapabilities> {
        self.service.public_models()
    }

    /// 处理一次请求。返回 `None` 表示网关不接管，调用方继续走既有路径。
    pub async fn handle(
        &self,
        protocol: WireProtocol,
        body: Value,
        key_id: u64,
        request_id: String,
    ) -> Option<Response> {
        let alias = body.get("model")?.as_str()?.to_string();
        // 快路径，不是守卫：真正兜底的是 `plan_for`，它对未接管的别名同样返回
        // `NotManaged`（变异测试确认过：去掉这一句行为不变）。留着它是为了在
        // 不接管时**不去扫描请求体**建路由上下文——那是每个请求都要付的钱。
        if !self.service.is_managed(&alias) {
            return None;
        }
        let ctx = route_context(key_id, alias, &body);

        if streaming_requested(&body) {
            return match dispatch_stream(
                self.service.clone(),
                self.direct.clone(),
                protocol,
                body,
                ctx,
                request_id,
            )
            .await
            {
                StreamDispatched::NotManaged => None,
                StreamDispatched::Refused { status, reason } => {
                    Some(error_response(protocol, status, &reason))
                }
                StreamDispatched::Streaming(rx) => {
                    let mut response = Body::from_stream(into_stream(rx)).into_response();
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/event-stream"),
                    );
                    response
                        .headers_mut()
                        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                    Some(response)
                }
            };
        }

        match dispatch(
            &self.service,
            self.direct.as_ref(),
            protocol,
            &body,
            &ctx,
            &request_id,
        )
        .await
        {
            Dispatched::NotManaged => None,
            Dispatched::Answered(answer) => Some((StatusCode::OK, Json(answer)).into_response()),
            Dispatched::Refused { status, reason } => {
                Some(error_response(protocol, status, &reason))
            }
        }
    }
}

/// 请求路径决定协议。调用方无权指定。
pub fn protocol_for_path(path: &str) -> Option<WireProtocol> {
    if path.ends_with("/messages") {
        Some(WireProtocol::Anthropic)
    } else if path.ends_with("/chat/completions") {
        Some(WireProtocol::ChatCompletions)
    } else if path.ends_with("/responses") {
        Some(WireProtocol::Responses)
    } else {
        None
    }
}

fn streaming_requested(body: &Value) -> bool {
    body.get("stream").and_then(Value::as_bool).unwrap_or(false)
}

/// 从请求里读出路由需要知道的东西。
///
/// 能力标记宁可**多报**：一条声称不支持图片的路接到带图请求会直接失败，
/// 而多报只是少用一条本来也能用的路。
fn route_context(key_id: u64, alias: String, body: &Value) -> RouteContext {
    RouteContext {
        key_id,
        public_model: alias,
        session_id: session_id(body),
        needs_tools: body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty()),
        needs_images: mentions_images(body),
        needs_reasoning: body
            .get("thinking")
            .and_then(|t| t.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|t| t == "enabled"),
    }
}

/// 粘性路由的会话标识沿用 Claude Code 在 `metadata.user_id` 里带的那个，
/// 不另发明请求头——客户端已经在发它了。
fn session_id(body: &Value) -> Option<String> {
    body.get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(Value::as_str)
        .and_then(crate::anthropic::cache_metering::extract_session_id)
}

/// 只在消息内容块这一层找图片，不做无界递归。
fn mentions_images(body: &Value) -> bool {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return false;
    };
    messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|block| block.get("type").and_then(Value::as_str) == Some("image"))
            })
    })
}

fn error_response(protocol: WireProtocol, status: u16, reason: &str) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let name = error_name(status);
    let body = match protocol {
        WireProtocol::Anthropic => json!({
            "type": "error",
            "error": {"type": name, "message": reason}
        }),
        WireProtocol::ChatCompletions | WireProtocol::Responses => json!({
            "error": {"message": reason, "type": name, "code": Value::Null}
        }),
    };
    (code, Json(body)).into_response()
}

fn error_name(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401..=403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "api_error",
    }
}

#[cfg(test)]
#[path = "entry_tests.rs"]
mod tests;
