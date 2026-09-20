//! Admin API 中间件

use std::sync::Arc;

use parking_lot::RwLock;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use super::client_keys::SharedClientKeyManager;
use super::groups::SharedGroupManager;
use super::service::AdminService;
use super::types::AdminErrorResponse;
use super::usage_stats::SharedAggregator;
use super::trace_db::SharedTraceStore;
use crate::common::auth;

/// Admin API 共享状态
#[derive(Clone)]
pub struct AdminState {
    /// 登录API密钥（管理面板登录用，运行时可修改）
    pub admin_api_key: Arc<RwLock<String>>,
    /// Admin 服务
    pub service: Arc<AdminService>,
    /// 客户端 Key 管理器（与 anthropic 路由共享）
    pub client_keys: SharedClientKeyManager,
    /// 用量聚合器（与 anthropic 路由共享）
    pub usage_aggregator: SharedAggregator,
    /// 请求链路追踪存储（与 anthropic 路由共享）
    pub trace_store: SharedTraceStore,
    /// 账号分组注册表（持久化到 groups.json）
    pub groups: SharedGroupManager,
    /// 多上游网关（可选）。`None` 表示未配置，网关相关端点一律回 404。
    pub gateway: Option<Arc<crate::gateway::service::GatewayService>>,
    /// 首次初始化状态。未初始化时持有一次性 setup token。
    pub setup: Arc<super::setup::SetupState>,
    /// 管理界面的会话。浏览器拿 token，脚本继续拿原始密钥。
    pub sessions: Arc<super::session::SessionStore>,
}

impl AdminState {
    /// 注入多上游网关。
    pub fn with_gateway(
        mut self,
        gateway: Option<Arc<crate::gateway::service::GatewayService>>,
    ) -> Self {
        self.gateway = gateway;
        self
    }

    /// 注入首次初始化状态。
    pub fn with_setup(mut self, setup: Arc<super::setup::SetupState>) -> Self {
        self.setup = setup;
        self
    }

    /// 注入会话表。
    pub fn with_sessions(mut self, sessions: Arc<super::session::SessionStore>) -> Self {
        self.sessions = sessions;
        self
    }

    pub fn new(
        admin_api_key: impl Into<String>,
        service: AdminService,
        client_keys: SharedClientKeyManager,
        usage_aggregator: SharedAggregator,
        trace_store: SharedTraceStore,
        groups: SharedGroupManager,
    ) -> Self {
        Self {
            admin_api_key: Arc::new(RwLock::new(admin_api_key.into())),
            service: Arc::new(service),
            client_keys,
            usage_aggregator,
            trace_store,
            groups,
            gateway: None,
            // 默认「已初始化」：这个构造器的调用方都已经给出了 admin_api_key。
            // 真正的状态由 wiring 用 with_setup 注入。
            setup: Arc::new(super::setup::SetupState::from_configured_key(Some(
                "configured",
            ))),
            // 默认不过期，由 wiring 用 with_sessions 换成真正的那份。
            sessions: Arc::new(super::session::SessionStore::new(0)),
        }
    }
}

/// Admin API 认证中间件 — 校验登录API密钥（adminApiKey）
pub async fn admin_auth_middleware(
    State(state): State<AdminState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let api_key = auth::extract_api_key(&request);

    let current_key = state.admin_api_key.read().clone();

    // 空密钥的意思是「还没配」，永远不是「谁都行」。
    //
    // 不加这道守卫会有个安静的洞：`extract_api_key` 对空 header 返回
    // `Some("")`，而 `constant_time_eq("", "")` 为真——空密钥 + 空 header
    // 就通过了。从前够不到，是因为密钥为空时整个 admin 路由不挂载；
    // 首次初始化把那层保护拆了（未初始化的实例必须挂上 /admin 才能显示
    // 初始化页），所以这里必须自己守住。
    if current_key.trim().is_empty() {
        let error = AdminErrorResponse::authentication_error();
        return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
    }

    // 两种凭证都收：
    //
    // - **会话 token**：浏览器登录后拿到的，有 TTL，过期即废。
    // - **原始管理密钥**：脚本、curl、既有自动化用的就是它，
    //   `--show-keys` 给出的也是它。引入会话不能把这些全打断——
    //   那是一次静默的破坏性变更，用户只会看到「升级后我的脚本全 401 了」。
    //
    // 这也是会话的防护上限所在：谁能读 config.json 谁就永远进得去。
    // 界面上如实说明，不让人以为加了会话就锁死了。
    match api_key {
        Some(key) if auth::constant_time_eq(&key, &current_key) => next.run(request).await,
        Some(key) if state.sessions.validate(&key) => next.run(request).await,
        _ => {
            let error = AdminErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}
