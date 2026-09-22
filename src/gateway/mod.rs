#![allow(dead_code, unused_imports)]

pub mod admission;
pub mod amount;
pub mod config;
pub mod config_store;
pub mod coordinator;
pub mod direct;
pub mod dispatch;
pub mod entry;
pub mod execute;
pub mod import;
pub mod ledger;
mod ledger_types;
pub mod protocol;
pub mod routing;
pub mod service;
pub mod settlement;
pub mod sse;
pub mod streaming;
pub mod transport;
pub mod usage;

#[cfg(test)]
#[path = "adapter_tests.rs"]
mod adapter_tests;

pub use amount::Amount;
// 账本类型对外是只读视图，管理面要按它们渲染。
pub use config::{
    BillingUnit, BudgetEnforcement, BudgetPolicy, GatewayConfig, ModelBinding, PublicModel,
    RoutingMode, TokenPrices, Upstream, UpstreamKind,
};
pub use ledger_types::{
    AccountAuditView, AccountView, AdjustmentDirection, AdjustmentInput, AttemptView,
    NewCycleInput, RequestView,
};
pub use usage::{
    CacheCategoryPolicy, CacheEvidence, CacheUsagePolicy, EvidenceKind, normalize_usage_with_policy,
};
pub use usage::{NativeUsage, normalize_usage, token_cost};
