//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use crate::admin::client_keys::{KeyAuth, SharedClientKeyManager};
use crate::admin::trace_db::{SharedTraceStore, TraceKeySource};
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder};
use crate::common::auth;
use crate::kiro::provider::KiroProvider;

use super::cache_metering::SharedCacheMeter;
use super::types::ErrorResponse;

/// 命中的鉴权上下文（注入到请求扩展，供 handler 记录用量）
#[derive(Clone, Debug)]
pub struct KeyContext {
    /// 命中的客户端 Key id
    pub key_id: u64,
    /// 该 Key 绑定的账号分组；None 表示未绑定，可使用全部账号
    pub group: Option<String>,
    /// 命中的入口 Key 类型。
    pub key_source: TraceKeySource,
    /// 客户端 IP（转发头优先，回落到 TCP 对端），仅用于请求日志
    pub client_ip: Option<String>,
    /// 遗留积分计数器已达上限时的 (已用, 上限)。
    ///
    /// 鉴权层知道这件事，却**不知道请求的是哪个模型**，因此不再自己下结论：
    /// 一个积分耗尽、但人民币账户有余额的 Key，走按钱计费的路完全应该放行。
    /// 判定挪到知道模型名的那一层（见 [`gateway_middleware`]）。
    pub legacy_credit_exhausted: Option<(f64, f64)>,
}

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// 工具兼容模式（ClaudeCode 内置工具名/入参双向适配 / Raw 透传）
    pub tool_compatibility_mode: crate::model::config::ToolCompatibilityMode,
    /// 客户端 Key 管理器（可选，未启用 Admin 时为 None）
    pub client_keys: Option<SharedClientKeyManager>,
    /// 用量日志记录器
    pub usage_recorder: Option<SharedRecorder>,
    /// 用量聚合器
    pub usage_aggregator: Option<SharedAggregator>,
    /// 中转层缓存计量（基于 cache_control 断点的内存缓存）
    pub cache_meter: Option<SharedCacheMeter>,
    /// 请求链路追踪存储（SQLite，可选）
    pub trace_store: Option<SharedTraceStore>,
    /// 多上游网关入口（可选）。`None` 或未接管任何模型时，请求一个字节都不改。
    pub gateway: Option<Arc<crate::gateway::entry::GatewayEntry>>,
}

impl AppState {
    /// 创建新的应用状态（不含 client_keys 的基础构造，供嵌入 / 测试使用）
    #[allow(dead_code)]
    pub fn new(
        extract_thinking: bool,
        tool_compatibility_mode: crate::model::config::ToolCompatibilityMode,
    ) -> Self {
        Self {
            kiro_provider: None,
            extract_thinking,
            tool_compatibility_mode,
            client_keys: None,
            usage_recorder: None,
            usage_aggregator: None,
            cache_meter: None,
            trace_store: None,
            gateway: None,
        }
    }

    /// 注入可与 Admin 控制面共享的 KiroProvider。
    pub fn with_shared_kiro_provider(mut self, provider: Arc<KiroProvider>) -> Self {
        self.kiro_provider = Some(provider);
        self
    }

    /// 注入用量记录组件
    pub fn with_usage(
        mut self,
        client_keys: Option<SharedClientKeyManager>,
        recorder: Option<SharedRecorder>,
        aggregator: Option<SharedAggregator>,
    ) -> Self {
        self.client_keys = client_keys;
        self.usage_recorder = recorder;
        self.usage_aggregator = aggregator;
        self
    }

    /// 注入缓存计量器
    pub fn with_cache_meter(mut self, cache: Option<SharedCacheMeter>) -> Self {
        self.cache_meter = cache;
        self
    }

    /// 注入链路追踪存储
    pub fn with_trace_store(mut self, store: Option<SharedTraceStore>) -> Self {
        self.trace_store = store;
        self
    }

    /// 注入多上游网关入口
    pub fn with_gateway(
        mut self,
        gateway: Option<Arc<crate::gateway::entry::GatewayEntry>>,
    ) -> Self {
        self.gateway = gateway;
        self
    }
}

fn legacy_exhausted(request: &axum::extract::Request) -> Option<(f64, f64)> {
    request
        .extensions()
        .get::<KeyContext>()
        .and_then(|c| c.legacy_credit_exhausted)
}

/// 入站请求抓取中间件。
///
/// 必须拿**原始 JSON**：`MessagesRequest` 在反序列化时会丢掉它不认识的字段，
/// 而排查"客户端到底发了什么"时，恰恰是那些字段最要紧。
///
/// 默认关，关着时**一个字节都不缓冲**——它是排查工具，不该让每个请求为它付代价。
pub async fn capture_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(provider) = state.kiro_provider.as_ref() else {
        return next.run(request).await;
    };
    let config = provider.pipeline().config.capture.clone();
    if !config.enabled {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": "request body could not be read"}
            })),
        )
            .into_response();
    };
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        provider.pipeline().capture().record(&config, None, &value);
    }
    next.run(axum::extract::Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// 网关入口中间件。
///
/// 只有被网关接管的别名会在这里被截走；其余请求**原样**继续走既有路径，
/// 连请求体都不缓冲——未配置网关的部署不该为一个用不上的特性付代价。
///
/// 必须排在鉴权**之后**：路由与计费都按具体的 Key 判定，拿不到身份就无从判定。
pub async fn gateway_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(entry) = state.gateway.clone() else {
        return next.run(request).await;
    };
    if !entry.manages_anything() {
        // 配置可能在鉴权之后被改小了。鉴权已经把判定让给了这一层，
        // 这里不能让一个积分耗尽的 Key 就这么过去。
        if let Some((used, limit)) = legacy_exhausted(&request) {
            return over_limit_response(used, limit);
        }
        return next.run(request).await;
    }
    let Some(protocol) = crate::gateway::entry::protocol_for_path(request.uri().path()) else {
        return next.run(request).await;
    };
    let Some(key_ctx) = request.extensions().get::<KeyContext>().cloned() else {
        // 没有身份就不该走到这里；交回既有路径由它按自己的规则处理。
        return next.run(request).await;
    };
    let key_id = key_ctx.key_id;

    let (parts, body) = request.into_parts();
    // 请求体上限已由路由层的 DefaultBodyLimit 管着，这里不再设第二道。
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!("网关入口读取请求体失败: {error}");
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "type": "error",
                    "error": {"type": "invalid_request_error", "message": "request body could not be read"}
                })),
            )
                .into_response();
        }
    };
    // 解析不了就不是网关的事，交回既有路径去报它自己的错。
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return next
            .run(axum::extract::Request::from_parts(parts, Body::from(bytes)))
            .await;
    };

    // 未被网关接管的别名仍走既有 Kiro 路径，那条路的额度由遗留计数器管着。
    // 只有网关接管的别名才由账本按路判定。
    if let Some((used, limit)) = key_ctx.legacy_credit_exhausted
        && !entry.manages(
            value
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
        )
    {
        return over_limit_response(used, limit);
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    // 备路要用的那份请求体：克隆 `Bytes` 是 O(1) 的引用计数，克隆解析后的 `Value`
    // 则要把整棵树复制一遍——解析后的结构比原始字节还大，而回退是少数情况。
    let retained = bytes.clone();
    match entry.handle(protocol, value, key_id, request_id, &[]).await {
        crate::gateway::entry::Handled::Response(response) => response,
        // Kiro 路：预留已经记在账上，交既有通道执行。覆盖与结算句柄放进扩展，
        // 由 handler 取用——请求级传递，不碰任何全局开关。
        crate::gateway::entry::Handled::UseKiro(route) => {
            let binding_id = route.binding_id.clone();
            let mut request = axum::extract::Request::from_parts(parts, Body::from(bytes));
            request.extensions_mut().insert(*route);
            let response = next.run(request).await;
            if !can_fall_back(&response) {
                return response;
            }
            // Kiro 这一路没走通，而且**一个字节都还没发给客户端**（状态行还在我们手里）。
            // 这正是配了直连备路要解决的情形：把这条路排除掉，让网关另选一条。
            tracing::info!(
                binding = %binding_id,
                status = response.status().as_u16(),
                "Kiro 路未成功且尚未向客户端发出内容，改走备路"
            );
            let Ok(body) = serde_json::from_slice::<serde_json::Value>(&retained) else {
                return response;
            };
            match entry
                .handle(
                    protocol,
                    body,
                    key_id,
                    uuid::Uuid::new_v4().to_string(),
                    std::slice::from_ref(&binding_id),
                )
                .await
            {
                crate::gateway::entry::Handled::Response(fallback) => fallback,
                // 备路也是 Kiro，或者压根没有备路：如实回 Kiro 那次的结果，
                // 不再套一层更笼统的错误。
                _ => response,
            }
        }
        crate::gateway::entry::Handled::NotManaged => {
            next.run(axum::extract::Request::from_parts(parts, Body::from(bytes)))
                .await
        }
    }
}

/// Kiro 路的结果是否还留有换路的余地。
///
/// 只看状态行：它在响应体发出去之前就定下来了，所以此刻客户端还什么都没收到，
/// 换供应商不会把两段输出拼在一起。
///
/// 4xx 里只认 429。其余 4xx 说明请求本身有问题，换一家照样会被拒，白花一次钱。
fn can_fall_back(response: &Response) -> bool {
    let status = response.status().as_u16();
    status == 429 || (500..600).contains(&status)
}

/// API Key 认证中间件
///
/// 所有入口 Key 统一按已存储的完整值精确匹配，不限制前缀。命中后向请求扩展注入
/// [`KeyContext`]，供 handler 记录用量时使用。
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let presented = match auth::extract_api_key(&request) {
        Some(k) => k,
        None => {
            let error = ErrorResponse::authentication_error();
            return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
        }
    };

    if let Some(mgr) = &state.client_keys {
        match mgr.verify_and_touch_ex(&presented) {
            KeyAuth::Ok(id) => {
                let group = mgr.group_of(id);
                let client_ip = auth::extract_client_ip(&request);
                request.extensions_mut().insert(KeyContext {
                    key_id: id,
                    group,
                    key_source: TraceKeySource::ClientKey,
                    client_ip,
                    legacy_credit_exhausted: None,
                });
                return next.run(request).await;
            }
            KeyAuth::OverLimit { id, used, limit } => {
                // 网关没接管任何模型时，这里就是终点，行为与从前逐字一致。
                let gateway_may_serve = state
                    .gateway
                    .as_ref()
                    .is_some_and(|entry| entry.manages_anything());
                if !gateway_may_serve {
                    return over_limit_response(used, limit);
                }
                // 接管了：带着这个事实往下走，由知道模型名的那一层判定。
                // 与从前一样不累加调用次数——这次还没被放行。
                let group = mgr.group_of(id);
                let client_ip = auth::extract_client_ip(&request);
                request.extensions_mut().insert(KeyContext {
                    key_id: id,
                    group,
                    key_source: TraceKeySource::ClientKey,
                    client_ip,
                    legacy_credit_exhausted: Some((used, limit)),
                });
                return next.run(request).await;
            }
            KeyAuth::NotFound => {}
        }
    }

    let error = ErrorResponse::authentication_error();
    (StatusCode::UNAUTHORIZED, Json(error)).into_response()
}

/// 遗留积分上限的拒绝应答。措辞与从前逐字一致，客户端与运维都认得它。
fn over_limit_response(used: f64, limit: f64) -> Response {
    let error = ErrorResponse::new(
        "rate_limit_error",
        format!(
            "该 API Key 已达到积分使用上限（已用 {:.2} / 上限 {:.2}），请联系管理员调整额度或重置统计",
            used, limit
        ),
    );
    (StatusCode::TOO_MANY_REQUESTS, Json(error)).into_response()
}

/// CORS 中间件层
///
/// **安全说明**：当前配置允许所有来源（Any），这是为了支持公开 API 服务。
/// 如果需要更严格的安全控制，请根据实际需求配置具体的允许来源、方法和头信息。
///
/// # 配置说明
/// - `allow_origin(Any)`: 允许任何来源的请求
/// - `allow_methods(Any)`: 允许任何 HTTP 方法
/// - `allow_headers(Any)`: 允许任何请求头
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}
