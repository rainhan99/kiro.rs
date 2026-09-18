//! 多上游网关的管理端点。
//!
//! # 密钥永不回流
//!
//! 读配置时上游密钥以掩码呈现。写回时掩码值**不会**被当成新密钥——
//! [`crate::gateway::config_store`] 的 `carry_over_secrets` 把原值补回去。
//! 否则每保存一次，真实密钥就被那串星号替换一次。
//!
//! # 错误是有类型的
//!
//! 前端要能分辨「配置不合法」「版本冲突」「额度不足」「写盘失败」——它们对应的
//! 操作完全不同：改表单、重新加载、充值、看磁盘。笼统的 500 让人无从下手。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::gateway::admission::{CandidateVerdict, evaluate};
use crate::gateway::routing::RouteContext;
use crate::gateway::{AdjustmentDirection, AdjustmentInput, NewCycleInput};
use crate::gateway::{Amount, BillingUnit, BudgetPolicy, GatewayConfig};

use super::middleware::AdminState;

/// 结构化错误。每一种对应一个明确的补救动作。
pub enum GatewayAdminError {
    /// 网关未配置：没有 `gateway.json`，也就没有账本。
    NotConfigured,
    /// 配置不合法。改表单。
    InvalidConfiguration(String),
    /// 版本冲突：别人先改了。重新加载再改。
    Conflict(String),
    /// 额度不足。
    QuotaExceeded(String),
    /// 写盘或写库失败。
    Persistence(String),
}

impl IntoResponse for GatewayAdminError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::NotConfigured => (
                StatusCode::NOT_FOUND,
                "gateway_not_configured",
                "多上游网关未配置；创建 gateway.json 后重启即可启用".to_string(),
            ),
            Self::InvalidConfiguration(m) => (StatusCode::BAD_REQUEST, "invalid_configuration", m),
            Self::Conflict(m) => (StatusCode::CONFLICT, "configuration_conflict", m),
            Self::QuotaExceeded(m) => (StatusCode::PAYMENT_REQUIRED, "quota_exceeded", m),
            Self::Persistence(m) => (StatusCode::INTERNAL_SERVER_ERROR, "persistence_error", m),
        };
        (
            status,
            Json(json!({"error": {"code": code, "message": message}})),
        )
            .into_response()
    }
}

/// 「未配置」不只是没注入网关对象：缺少 `gateway.json` 时网关处于惰性状态，
/// 既没有上游也没有模型，更没有账本。那时回一个空壳配置会让人以为配好了。
fn gateway(
    state: &AdminState,
) -> Result<&crate::gateway::service::GatewayService, GatewayAdminError> {
    let service = state
        .gateway
        .as_deref()
        .ok_or(GatewayAdminError::NotConfigured)?;
    if !service.is_configured() {
        return Err(GatewayAdminError::NotConfigured);
    }
    Ok(service)
}

fn ledger(state: &AdminState) -> Result<&crate::gateway::ledger::Ledger, GatewayAdminError> {
    gateway(state)?
        .ledger()
        .ok_or(GatewayAdminError::NotConfigured)
}

// ---------- 配置 ----------

/// GET /api/admin/gateway/config
pub async fn get_config(State(state): State<AdminState>) -> Response {
    let Ok(service) = gateway(&state) else {
        return GatewayAdminError::NotConfigured.into_response();
    };
    // `redacted()` 已经给出 `{revision, config}`；这里只补上"当前真正被接管的
    // 别名"——定义了却全部停用的模型不算，运维需要看到实际生效的那一份。
    match service.config_store().redacted() {
        Ok(mut view) => {
            if let Some(object) = view.as_object_mut() {
                object.insert(
                    "managedModels".into(),
                    json!(
                        service
                            .public_models()
                            .into_iter()
                            .map(|model| model.id)
                            .collect::<Vec<_>>()
                    ),
                );
            }
            Json(view).into_response()
        }
        Err(error) => GatewayAdminError::Persistence(format!("{error:#}")).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigUpdate {
    /// 客户端读到的版本号。与当前不符即为冲突，绝不「后写者胜」。
    pub revision: u64,
    pub config: GatewayConfig,
}

/// PUT /api/admin/gateway/config
pub async fn put_config(
    State(state): State<AdminState>,
    payload: Result<Json<ConfigUpdate>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(service) = gateway(&state) else {
        return GatewayAdminError::NotConfigured.into_response();
    };
    let update = match payload {
        Ok(Json(update)) => update,
        Err(error) => {
            return GatewayAdminError::InvalidConfiguration(format!(
                "请求体不是合法的网关配置：{error}"
            ))
            .into_response();
        }
    };
    match service.update_config(update.revision, update.config) {
        Ok(outcome) => Json(json!({
            "revision": outcome.revision,
            "invalidatesRoutes": outcome.invalidates_routes,
        }))
        .into_response(),
        Err(error) => classify_update(&error).into_response(),
    }
}

/// 更新失败的原因决定了前端该让人做什么，所以要分开。
fn classify_update(error: &anyhow::Error) -> GatewayAdminError {
    let rendered = format!("{error:#}");
    if rendered.contains("configuration changed since") || rendered.contains("revision") {
        GatewayAdminError::Conflict(rendered)
    } else if rendered.contains("could not be written") || rendered.contains("io error") {
        GatewayAdminError::Persistence(rendered)
    } else {
        GatewayAdminError::InvalidConfiguration(rendered)
    }
}

// ---------- 路由预览 ----------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRequest {
    pub key_id: u64,
    pub model: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub needs_tools: bool,
    #[serde(default)]
    pub needs_images: bool,
    #[serde(default)]
    pub needs_reasoning: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewView {
    pub managed: bool,
    pub mode: Option<String>,
    /// 逐候选的判定，含被拒的理由。
    pub candidates: Vec<CandidateView>,
    /// 按当前配置会被选中的那一条。全部被拒时为 `None`。
    pub selected: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateView {
    pub binding_id: String,
    pub upstream_id: String,
    pub upstream_model: String,
    pub unit: String,
    pub eligible: bool,
    /// 被拒的原因；通过时为 `None`。
    pub refusal: Option<String>,
}

/// POST /api/admin/gateway/preview
///
/// **只判定，不预留、不发请求**。运维用它回答「这个 Key 请求这个模型会走哪条路」，
/// 而不必真发一次请求去试。
pub async fn preview(
    State(state): State<AdminState>,
    Json(request): Json<PreviewRequest>,
) -> Response {
    let (service, ledger) = match (gateway(&state), ledger(&state)) {
        (Ok(service), Ok(ledger)) => (service, ledger),
        _ => return GatewayAdminError::NotConfigured.into_response(),
    };
    let Some(plan) = service.plan_for(&request.model) else {
        return Json(PreviewView {
            managed: false,
            mode: None,
            candidates: Vec::new(),
            selected: None,
        })
        .into_response();
    };

    let verdicts = match evaluate(ledger, request.key_id, &plan, &plan.candidates) {
        Ok(verdicts) => verdicts,
        Err(error) => return GatewayAdminError::Persistence(format!("{error:#}")).into_response(),
    };
    let mut candidates = Vec::new();
    let mut eligible = Vec::new();
    for candidate in &plan.candidates {
        let verdict = verdicts
            .iter()
            .find(|(id, _)| id == &candidate.binding_id)
            .map(|(_, verdict)| verdict);
        let binding = plan.binding(&candidate.binding_id);
        let (is_eligible, refusal) = match verdict {
            Some(CandidateVerdict::Eligible(_)) => (true, None),
            Some(CandidateVerdict::Refused(refusal)) => (false, Some(format!("{refusal:?}"))),
            None => (false, Some("no verdict".to_string())),
        };
        if is_eligible {
            eligible.push(candidate.clone());
        }
        candidates.push(CandidateView {
            binding_id: candidate.binding_id.clone(),
            upstream_id: candidate.upstream_id.clone(),
            upstream_model: binding
                .map(|b| b.upstream_model.clone())
                .unwrap_or_default(),
            unit: binding
                .map(|b| format!("{:?}", b.billing_unit))
                .unwrap_or_default(),
            eligible: is_eligible,
            refusal,
        });
    }

    let ctx = RouteContext {
        key_id: request.key_id,
        public_model: request.model.clone(),
        session_id: request.session_id.clone(),
        needs_tools: request.needs_tools,
        needs_images: request.needs_images,
        needs_reasoning: request.needs_reasoning,
    };
    // 预览不得改变粘性状态：只读地问「现在会选谁」。
    let selected = service
        .routing()
        .select(&ctx, &eligible, plan.mode, None)
        .map(|selection| selection.binding_id);

    Json(PreviewView {
        managed: true,
        mode: Some(format!("{:?}", plan.mode)),
        candidates,
        selected,
    })
    .into_response()
}

// ---------- Key 额度 ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetView {
    pub unit: String,
    pub enforcement: String,
    /// 十进制字符串，绝不用浮点。
    pub limit: Option<String>,
    pub used: String,
    pub reserved: String,
    pub available: Option<String>,
    pub cycle: u64,
    pub in_flight: u32,
    pub pending: u32,
    pub customer_pending: u32,
    pub max_in_flight: u32,
    pub max_pending: u32,
    pub allowed_models: Vec<String>,
    pub allowed_upstreams: Vec<String>,
}

/// GET /api/admin/client-keys/{id}/budgets
pub async fn get_budgets(State(state): State<AdminState>, Path(id): Path<u64>) -> Response {
    let Ok(ledger) = ledger(&state) else {
        return GatewayAdminError::NotConfigured.into_response();
    };
    match ledger.accounts(id) {
        Ok(accounts) => Json(json!({
            "keyId": id,
            "budgets": accounts.iter().map(budget_view).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(error) => GatewayAdminError::Persistence(format!("{error:#}")).into_response(),
    }
}

fn budget_view(account: &crate::gateway::AccountView) -> BudgetView {
    BudgetView {
        unit: unit_name(account.policy.unit).into(),
        enforcement: format!("{:?}", account.policy.enforcement).to_lowercase(),
        limit: account.policy.limit.map(|a| a.to_string()),
        used: account.used.to_string(),
        reserved: account.reserved.to_string(),
        available: account.available.map(|a| a.to_string()),
        cycle: account.cycle,
        in_flight: account.in_flight,
        pending: account.pending,
        customer_pending: account.customer_pending,
        max_in_flight: account.policy.max_in_flight,
        max_pending: account.policy.max_pending,
        allowed_models: account.policy.allowed_models.clone(),
        allowed_upstreams: account.policy.allowed_upstreams.clone(),
    }
}

fn unit_name(unit: BillingUnit) -> &'static str {
    match unit {
        BillingUnit::KiroCredit => "kiroCredit",
        BillingUnit::Cny => "CNY",
        BillingUnit::Usd => "USD",
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetUpdate {
    pub unit: BillingUnit,
    /// 十进制字符串。`null` 表示不限额，与 `"0"`（一分钱都不能花）是两回事。
    #[serde(default)]
    pub limit: Option<String>,
    pub enforcement: crate::gateway::BudgetEnforcement,
    pub max_in_flight: u32,
    pub max_pending: u32,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub allowed_upstreams: Vec<String>,
}

/// PUT /api/admin/client-keys/{id}/budgets
///
/// 只设**政策**（上限、强制方式、并发），不碰已用量。改额度不该顺手把账抹平——
/// 那是有审计的调整操作要做的事。
pub async fn put_budget(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(update): Json<BudgetUpdate>,
) -> Response {
    let Ok(ledger) = ledger(&state) else {
        return GatewayAdminError::NotConfigured.into_response();
    };
    let limit = match update
        .limit
        .as_deref()
        .map(str::parse::<Amount>)
        .transpose()
    {
        Ok(limit) => limit,
        Err(error) => {
            return GatewayAdminError::InvalidConfiguration(format!(
                "额度必须是十进制字符串：{error:#}"
            ))
            .into_response();
        }
    };
    let policy = BudgetPolicy {
        unit: update.unit,
        limit,
        enforcement: update.enforcement,
        max_in_flight: update.max_in_flight,
        max_pending: update.max_pending,
        allowed_models: update.allowed_models,
        allowed_upstreams: update.allowed_upstreams,
    };
    match ledger.set_account(id, policy) {
        Ok(account) => Json(budget_view(&account)).into_response(),
        Err(error) => GatewayAdminError::InvalidConfiguration(format!("{error:#}")).into_response(),
    }
}

// ---------- 审计过的调整 ----------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdjustmentRequest {
    /// 幂等标识。重复提交同一个 id 不会重复调整。
    pub adjustment_id: String,
    pub unit: BillingUnit,
    pub direction: AdjustmentDirection,
    /// 十进制字符串。
    pub amount: String,
    pub reason: String,
}

/// POST /api/admin/client-keys/{id}/adjustments
pub async fn post_adjustment(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(request): Json<AdjustmentRequest>,
) -> Response {
    let Ok(ledger) = ledger(&state) else {
        return GatewayAdminError::NotConfigured.into_response();
    };
    let amount = match request.amount.parse::<Amount>() {
        Ok(amount) => amount,
        Err(error) => {
            return GatewayAdminError::InvalidConfiguration(format!(
                "金额必须是十进制字符串：{error:#}"
            ))
            .into_response();
        }
    };
    match ledger.adjust(AdjustmentInput {
        adjustment_id: request.adjustment_id,
        key_id: id,
        unit: request.unit,
        direction: request.direction,
        amount,
        reason: request.reason,
    }) {
        Ok(account) => Json(budget_view(&account)).into_response(),
        Err(error) => {
            let rendered = format!("{error:#}");
            if rendered.contains("underflow") {
                GatewayAdminError::QuotaExceeded(rendered).into_response()
            } else {
                GatewayAdminError::InvalidConfiguration(rendered).into_response()
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewCycleRequest {
    pub operation_id: String,
    pub unit: BillingUnit,
    pub reason: String,
}

/// POST /api/admin/client-keys/{id}/cycles
///
/// 开新周期：把当前账户存档，已用量归零。**存档不删**——历史仍可查。
pub async fn post_cycle(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(request): Json<NewCycleRequest>,
) -> Response {
    let Ok(ledger) = ledger(&state) else {
        return GatewayAdminError::NotConfigured.into_response();
    };
    match ledger.new_cycle(NewCycleInput {
        operation_id: request.operation_id,
        key_id: id,
        unit: request.unit,
        reason: request.reason,
    }) {
        Ok(account) => Json(budget_view(&account)).into_response(),
        Err(error) => GatewayAdminError::InvalidConfiguration(format!("{error:#}")).into_response(),
    }
}

// ---------- 审计与请求记录 ----------

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub key_id: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// GET /api/admin/gateway/requests?keyId=&limit=
pub async fn list_requests(
    State(state): State<AdminState>,
    Query(query): Query<ListQuery>,
) -> Response {
    let Ok(ledger) = ledger(&state) else {
        return GatewayAdminError::NotConfigured.into_response();
    };
    let Some(key_id) = query.key_id else {
        return GatewayAdminError::InvalidConfiguration("必须指定 keyId".into()).into_response();
    };
    match ledger.list_requests(key_id, query.limit.unwrap_or(50).min(500)) {
        Ok(requests) => Json(json!({"keyId": key_id, "requests": requests})).into_response(),
        Err(error) => GatewayAdminError::Persistence(format!("{error:#}")).into_response(),
    }
}

/// GET /api/admin/client-keys/{id}/ledger-audit
pub async fn list_audit(State(state): State<AdminState>, Path(id): Path<u64>) -> Response {
    let Ok(ledger) = ledger(&state) else {
        return GatewayAdminError::NotConfigured.into_response();
    };
    match ledger.list_account_audit(id, 100) {
        Ok(entries) => Json(json!({"keyId": id, "entries": entries})).into_response(),
        Err(error) => GatewayAdminError::Persistence(format!("{error:#}")).into_response(),
    }
}

#[cfg(test)]
#[path = "gateway_tests.rs"]
mod tests;
