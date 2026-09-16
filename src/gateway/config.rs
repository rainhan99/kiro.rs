use std::collections::HashSet;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use super::amount::{Amount, validate_price};
use super::usage::CacheUsagePolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BillingUnit {
    #[serde(rename = "kiro_credit")]
    KiroCredit,
    #[serde(rename = "CNY")]
    Cny,
    #[serde(rename = "USD")]
    Usd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    #[default]
    Sticky,
    WeightedRandom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamKind {
    Kiro,
    Anthropic,
    OpenaiChat,
    OpenaiResponses,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenPrices {
    pub currency: BillingUnit,
    pub input: Amount,
    pub output: Amount,
    pub cache_read: Amount,
    pub cache_write: Amount,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<Amount>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenPricesWire {
    currency: BillingUnit,
    #[serde(deserialize_with = "deserialize_price")]
    input: Amount,
    #[serde(deserialize_with = "deserialize_price")]
    output: Amount,
    #[serde(deserialize_with = "deserialize_price")]
    cache_read: Amount,
    #[serde(deserialize_with = "deserialize_price")]
    cache_write: Amount,
    #[serde(default, deserialize_with = "deserialize_optional_price")]
    cache_write_1h: Option<Amount>,
}

fn deserialize_price<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Amount, D::Error> {
    let value = String::deserialize(deserializer)?;
    let digits = value
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    if digits > 6 {
        return Err(serde::de::Error::custom(
            "price must have at most six fractional digits",
        ));
    }
    value.parse().map_err(serde::de::Error::custom)
}

fn deserialize_optional_price<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Amount>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|value| {
            let digits = value
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len());
            if digits > 6 {
                return Err(serde::de::Error::custom(
                    "price must have at most six fractional digits",
                ));
            }
            value.parse().map_err(serde::de::Error::custom)
        })
        .transpose()
}

impl<'de> Deserialize<'de> for TokenPrices {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = TokenPricesWire::deserialize(deserializer)?;
        Ok(Self {
            currency: wire.currency,
            input: wire.input,
            output: wire.output,
            cache_read: wire.cache_read,
            cache_write: wire.cache_write,
            cache_write_1h: wire.cache_write_1h,
        })
    }
}

impl TokenPrices {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            matches!(self.currency, BillingUnit::Cny | BillingUnit::Usd),
            "token price currency must be CNY or USD"
        );
        for amount in [self.input, self.output, self.cache_read, self.cache_write] {
            validate_price(amount)?;
        }
        if let Some(amount) = self.cache_write_1h {
            validate_price(amount)?;
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Upstream {
    pub id: String,
    pub name: String,
    pub kind: UpstreamKind,
    pub enabled: bool,
    pub weight: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_deserializing)]
    pub has_api_key: bool,
    #[serde(default)]
    pub allow_private_network: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kiro_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_usage_policy: Option<CacheUsagePolicy>,
}

impl Upstream {
    pub fn effective_cache_usage_policy(&self) -> CacheUsagePolicy {
        self.cache_usage_policy
            .unwrap_or_else(|| CacheUsagePolicy::strict_for(self.kind))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelBinding {
    pub id: String,
    pub upstream_id: String,
    pub upstream_model: String,
    pub enabled: bool,
    pub priority_tier: u32,
    pub weight: u32,
    pub context_window: u64,
    pub max_output_tokens: u64,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_images: bool,
    #[serde(default)]
    pub supports_reasoning: bool,
    #[serde(default)]
    pub allow_model_substitution: bool,
    pub billing_unit: BillingUnit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_prices: Option<TokenPrices>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sell_prices: Option<TokenPrices>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_mode: Option<RoutingMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_ttl_secs: Option<u64>,
    #[serde(default)]
    pub bindings: Vec<ModelBinding>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayConfig {
    #[serde(default)]
    pub default_routing_mode: RoutingMode,
    #[serde(default = "default_affinity_ttl")]
    pub affinity_ttl_secs: u64,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,
    #[serde(default)]
    pub upstreams: Vec<Upstream>,
    #[serde(default)]
    pub models: Vec<PublicModel>,
}

const fn default_affinity_ttl() -> u64 {
    3_600
}
const fn default_max_attempts() -> u32 {
    3
}
const fn default_request_timeout() -> u64 {
    120
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            default_routing_mode: RoutingMode::Sticky,
            affinity_ttl_secs: default_affinity_ttl(),
            max_attempts: default_max_attempts(),
            request_timeout_secs: default_request_timeout(),
            upstreams: Vec::new(),
            models: Vec::new(),
        }
    }
}

impl GatewayConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            (60..=86_400).contains(&self.affinity_ttl_secs),
            "affinityTtlSecs must be in 60..=86400"
        );
        ensure!(
            (1..=10).contains(&self.max_attempts),
            "maxAttempts must be in 1..=10"
        );
        ensure!(
            (1..=600).contains(&self.request_timeout_secs),
            "requestTimeoutSecs must be in 1..=600"
        );

        let mut upstream_ids = HashSet::new();
        for upstream in &self.upstreams {
            ensure_nonempty("upstream id", &upstream.id)?;
            ensure_nonempty("upstream name", &upstream.name)?;
            ensure!(
                upstream_ids.insert(upstream.id.as_str()),
                "duplicate upstream id `{}`",
                upstream.id
            );
            ensure!(
                upstream.weight <= 10_000,
                "upstream `{}` weight exceeds 10000",
                upstream.id
            );
            if let Some(base_url) = &upstream.base_url {
                validate_base_url(base_url)
                    .with_context(|| format!("upstream `{}` baseUrl", upstream.id))?;
            }
            if let Some(api_key) = &upstream.api_key {
                ensure!(
                    !api_key.trim().is_empty(),
                    "upstream `{}` apiKey is empty",
                    upstream.id
                );
            }
            if let Some(group) = &upstream.kiro_group {
                ensure_nonempty("kiroGroup", group)?;
            }
            match upstream.kind {
                UpstreamKind::Kiro => {
                    ensure!(
                        upstream.api_key.is_none(),
                        "Kiro upstream `{}` must use the existing credential pool",
                        upstream.id
                    );
                    ensure!(
                        upstream.base_url.is_none(),
                        "Kiro upstream `{}` cannot override baseUrl",
                        upstream.id
                    );
                    ensure!(
                        upstream
                            .cache_usage_policy
                            .as_ref()
                            .is_none_or(|policy| *policy
                                == CacheUsagePolicy::strict_for(UpstreamKind::Kiro)),
                        "Kiro upstream `{}` cannot mark native cache evidence not applicable",
                        upstream.id
                    );
                }
                _ => ensure!(
                    upstream.kiro_group.is_none(),
                    "non-Kiro upstream `{}` cannot set kiroGroup",
                    upstream.id
                ),
            }
        }

        let upstream_by_id = self
            .upstreams
            .iter()
            .map(|upstream| (upstream.id.as_str(), upstream))
            .collect::<std::collections::HashMap<_, _>>();
        let mut model_ids = HashSet::new();
        let mut binding_ids = HashSet::new();
        for model in &self.models {
            ensure_nonempty("model id", &model.id)?;
            ensure!(
                model_ids.insert(model.id.as_str()),
                "duplicate model id `{}`",
                model.id
            );
            if let Some(ttl) = model.affinity_ttl_secs {
                ensure!(
                    (60..=86_400).contains(&ttl),
                    "model `{}` affinityTtlSecs must be in 60..=86400",
                    model.id
                );
            }
            let mut routes = HashSet::new();
            for binding in &model.bindings {
                ensure_nonempty("binding id", &binding.id)?;
                ensure_nonempty("upstreamModel", &binding.upstream_model)?;
                ensure!(
                    binding_ids.insert(binding.id.as_str()),
                    "duplicate binding id `{}`",
                    binding.id
                );
                ensure!(
                    binding.weight <= 10_000,
                    "binding `{}` weight exceeds 10000",
                    binding.id
                );
                ensure!(
                    binding.context_window > 0,
                    "binding `{}` contextWindow must be positive",
                    binding.id
                );
                ensure!(
                    binding.max_output_tokens > 0
                        && binding.max_output_tokens <= binding.context_window,
                    "binding `{}` maxOutputTokens must be in 1..=contextWindow",
                    binding.id
                );
                ensure!(
                    routes.insert((
                        binding.upstream_id.as_str(),
                        binding.upstream_model.as_str(),
                        binding.priority_tier
                    )),
                    "model `{}` has duplicate upstream/model/tier binding",
                    model.id
                );
                let upstream = upstream_by_id
                    .get(binding.upstream_id.as_str())
                    .with_context(|| {
                        format!(
                            "binding `{}` references unknown upstream `{}`",
                            binding.id, binding.upstream_id
                        )
                    })?;
                validate_binding(binding, upstream.kind)
                    .with_context(|| format!("binding `{}`", binding.id))?;
            }
        }
        Ok(())
    }
}

fn ensure_nonempty(label: &str, value: &str) -> anyhow::Result<()> {
    ensure!(!value.trim().is_empty(), "{label} must not be empty");
    Ok(())
}

fn validate_base_url(value: &str) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "scheme must be http or https"
    );
    ensure!(url.host_str().is_some(), "host is required");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "embedded credentials are forbidden"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "query and fragment are forbidden"
    );
    ensure!(!url.cannot_be_a_base(), "URL must be usable as a base");
    Ok(())
}

fn validate_binding(binding: &ModelBinding, kind: UpstreamKind) -> anyhow::Result<()> {
    if let Some(prices) = &binding.cost_prices {
        prices.validate().context("invalid costPrices")?;
    }
    if let Some(prices) = &binding.sell_prices {
        prices.validate().context("invalid sellPrices")?;
    }

    match kind {
        UpstreamKind::Kiro => {
            ensure!(
                binding.cost_prices.is_none(),
                "Kiro cost is native credit evidence, not a token price table"
            );
        }
        _ => {
            ensure!(
                binding.billing_unit != BillingUnit::KiroCredit,
                "direct API cannot be billed as kiro_credit"
            );
            ensure!(
                binding.cost_prices.is_some(),
                "direct API requires monetary costPrices"
            );
        }
    }
    match binding.billing_unit {
        BillingUnit::KiroCredit => ensure!(
            binding.sell_prices.is_none(),
            "kiro_credit cannot use a token sell price"
        ),
        BillingUnit::Cny | BillingUnit::Usd => {
            let sell = binding
                .sell_prices
                .as_ref()
                .context("monetary billing requires sellPrices")?;
            ensure!(
                sell.currency == binding.billing_unit,
                "sellPrices currency must match billingUnit"
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetEnforcement {
    Hard,
    Soft,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetPolicy {
    pub unit: BillingUnit,
    pub limit: Option<Amount>,
    pub enforcement: BudgetEnforcement,
    pub max_in_flight: u32,
    pub max_pending: u32,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub allowed_upstreams: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prices(currency: BillingUnit) -> TokenPrices {
        TokenPrices {
            currency,
            input: "1".parse().unwrap(),
            output: "2".parse().unwrap(),
            cache_read: "0.1".parse().unwrap(),
            cache_write: "3".parse().unwrap(),
            cache_write_1h: None,
        }
    }

    #[test]
    fn empty_json_uses_backward_compatible_defaults() {
        let config: GatewayConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.default_routing_mode, RoutingMode::Sticky);
        assert_eq!(
            (
                config.affinity_ttl_secs,
                config.max_attempts,
                config.request_timeout_secs
            ),
            (3600, 3, 120)
        );
        config.validate().unwrap();
    }

    #[test]
    fn rejects_prices_with_more_than_six_fractional_digits() {
        let json = r#"{
            "currency":"USD","input":"0.1234567","output":"1",
            "cacheRead":"0","cacheWrite":"0"
        }"#;
        assert!(serde_json::from_str::<TokenPrices>(json).is_err());
    }

    #[test]
    fn rejects_direct_api_binding_billed_as_kiro_credit() {
        let config = GatewayConfig {
            upstreams: vec![Upstream {
                id: "direct".into(),
                name: "Direct".into(),
                kind: UpstreamKind::Anthropic,
                enabled: true,
                weight: 100,
                base_url: None,
                api_key: Some("secret".into()),
                has_api_key: true,
                allow_private_network: false,
                kiro_group: None,
                cache_usage_policy: None,
            }],
            models: vec![PublicModel {
                id: "opus".into(),
                display_name: None,
                routing_mode: None,
                affinity_ttl_secs: None,
                bindings: vec![ModelBinding {
                    id: "bad".into(),
                    upstream_id: "direct".into(),
                    upstream_model: "real".into(),
                    enabled: true,
                    priority_tier: 0,
                    weight: 100,
                    context_window: 200_000,
                    max_output_tokens: 8_192,
                    supports_tools: true,
                    supports_images: false,
                    supports_reasoning: false,
                    allow_model_substitution: false,
                    billing_unit: BillingUnit::KiroCredit,
                    cost_prices: Some(prices(BillingUnit::Usd)),
                    sell_prices: None,
                }],
            }],
            ..GatewayConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn upstream_cache_policy_defaults_strictly_and_round_trips_explicit_overrides() {
        let mut upstream = Upstream {
            id: "openai".into(),
            name: "OpenAI compatible".into(),
            kind: UpstreamKind::OpenaiChat,
            enabled: true,
            weight: 1,
            base_url: None,
            api_key: None,
            has_api_key: false,
            allow_private_network: false,
            kiro_group: None,
            cache_usage_policy: None,
        };
        assert_eq!(
            upstream.effective_cache_usage_policy(),
            CacheUsagePolicy::strict_for(UpstreamKind::OpenaiChat)
        );

        upstream.cache_usage_policy = Some(CacheUsagePolicy {
            cache_read: super::super::usage::CacheCategoryPolicy::Reported,
            cache_write: super::super::usage::CacheCategoryPolicy::NotApplicable,
            cache_write_1h: super::super::usage::CacheCategoryPolicy::NotApplicable,
        });
        let json = serde_json::to_value(&upstream).unwrap();
        assert_eq!(json["cacheUsagePolicy"]["cacheWrite"], "not_applicable");
        let restored: Upstream = serde_json::from_value(json).unwrap();
        assert_eq!(
            restored.effective_cache_usage_policy(),
            upstream.cache_usage_policy.unwrap()
        );
    }
}
