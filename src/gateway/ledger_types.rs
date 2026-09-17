use serde::{Deserialize, Serialize};

use super::{Amount, BillingUnit, BudgetEnforcement, BudgetPolicy, TokenPrices, UpstreamKind};

/// Frozen, nonsecret route and tariffs. No endpoint, credential or GatewayConfig.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceSnapshot {
    pub config_revision: u64,
    pub price_revision: u64,
    pub binding_id: String,
    pub upstream_kind: UpstreamKind,
    pub upstream_model: String,
    pub cost_prices: Option<TokenPrices>,
    pub sell_prices: Option<TokenPrices>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundKind {
    Guaranteed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReservationInput {
    pub request_id: String,
    pub attempt_id: String,
    pub key_id: u64,
    pub unit: BillingUnit,
    pub public_model: String,
    pub upstream_id: String,
    pub upper_bound: Option<Amount>,
    pub bound_kind: BoundKind,
    pub snapshot: PriceSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Pending,
    Confirmed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptCost {
    pub unit: BillingUnit,
    pub evidence: EvidenceStatus,
    pub amount: Option<Amount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Succeeded,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptInput {
    pub event_id: String,
    pub attempt_id: String,
    pub cost: AttemptCost,
    pub committed: bool,
    pub outcome: AttemptOutcome,
    /// Raw native usage payload; provider identity is frozen in the reservation.
    pub usage: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettlementInput {
    pub settlement_id: String,
    pub attempt_id: String,
    pub evidence: EvidenceStatus,
    pub downstream_amount: Option<Amount>,
    pub attempt_cost: AttemptCost,
    pub committed: bool,
    pub outcome: AttemptOutcome,
    pub usage: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationState {
    InFlight,
    Pending,
    Settled,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    pub key_id: u64,
    pub policy: BudgetPolicy,
    pub cycle: u64,
    pub used: Amount,
    pub reserved: Amount,
    pub available: Option<Amount>,
    pub in_flight: u32,
    pub pending: u32,
    pub customer_pending: u32,
    pub provider_only_pending: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptView {
    pub reservation: ReservationInput,
    pub cycle: u64,
    pub enforcement: BudgetEnforcement,
    pub state: ReservationState,
    pub record: Option<AttemptInput>,
    pub settlement: Option<SettlementInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestView {
    pub request_id: String,
    pub key_id: u64,
    pub public_model: String,
    pub committed_attempt_id: Option<String>,
    pub attempts: Vec<AttemptView>,
    pub events: Vec<LedgerEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEvent {
    pub event_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountAuditView {
    pub operation_id: String,
    pub kind: String,
    pub key_id: u64,
    pub unit: BillingUnit,
    pub reason: String,
    pub before: Option<AccountView>,
    pub after: AccountView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjustmentDirection {
    Debit,
    Credit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdjustmentInput {
    pub adjustment_id: String,
    pub key_id: u64,
    pub unit: BillingUnit,
    pub direction: AdjustmentDirection,
    pub amount: Amount,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewCycleInput {
    pub operation_id: String,
    pub key_id: u64,
    pub unit: BillingUnit,
    pub reason: String,
}
