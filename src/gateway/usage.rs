use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Amount, TokenPrices, UpstreamKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheCategoryPolicy {
    Reported,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheUsagePolicy {
    pub cache_read: CacheCategoryPolicy,
    pub cache_write: CacheCategoryPolicy,
    pub cache_write_1h: CacheCategoryPolicy,
}

impl CacheUsagePolicy {
    pub const fn strict_for(kind: UpstreamKind) -> Self {
        match kind {
            UpstreamKind::Kiro => Self {
                cache_read: CacheCategoryPolicy::Reported,
                cache_write: CacheCategoryPolicy::Reported,
                cache_write_1h: CacheCategoryPolicy::NotApplicable,
            },
            UpstreamKind::Anthropic => Self {
                cache_read: CacheCategoryPolicy::Reported,
                cache_write: CacheCategoryPolicy::Reported,
                cache_write_1h: CacheCategoryPolicy::Reported,
            },
            UpstreamKind::OpenaiChat | UpstreamKind::OpenaiResponses => Self {
                cache_read: CacheCategoryPolicy::Reported,
                cache_write: CacheCategoryPolicy::Reported,
                cache_write_1h: CacheCategoryPolicy::NotApplicable,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    Reported,
    NotApplicable,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheEvidence {
    pub cache_read: EvidenceKind,
    pub cache_write: EvidenceKind,
    pub cache_write_1h: EvidenceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_write_1h: u64,
    pub credits: Option<Amount>,
    pub cache_evidence: CacheEvidence,
    pub raw: Value,
}

pub fn normalize_usage(kind: UpstreamKind, raw: &Value) -> anyhow::Result<Option<NativeUsage>> {
    normalize_usage_with_policy(kind, raw, CacheUsagePolicy::strict_for(kind))
}

pub fn normalize_usage_with_policy(
    kind: UpstreamKind,
    raw: &Value,
    policy: CacheUsagePolicy,
) -> anyhow::Result<Option<NativeUsage>> {
    match kind {
        UpstreamKind::Kiro => normalize_kiro(raw, policy),
        UpstreamKind::Anthropic => normalize_anthropic(raw, policy),
        UpstreamKind::OpenaiChat => normalize_openai(
            raw,
            policy,
            "prompt_tokens",
            "completion_tokens",
            "prompt_tokens_details",
        ),
        UpstreamKind::OpenaiResponses => normalize_openai(
            raw,
            policy,
            "input_tokens",
            "output_tokens",
            "input_tokens_details",
        ),
    }
}

pub fn token_cost(prices: &TokenPrices, usage: &NativeUsage) -> anyhow::Result<Amount> {
    ensure!(
        !matches!(usage.cache_evidence.cache_read, EvidenceKind::Missing)
            && !matches!(usage.cache_evidence.cache_write, EvidenceKind::Missing)
            && !matches!(usage.cache_evidence.cache_write_1h, EvidenceKind::Missing),
        "cannot calculate token cost from missing cache evidence"
    );
    prices.validate()?;
    let categories = [
        (prices.input, usage.input, "input"),
        (prices.output, usage.output, "output"),
        (prices.cache_read, usage.cache_read, "cache read"),
        (prices.cache_write, usage.cache_write, "cache write"),
    ];
    let mut total = Amount::ZERO;
    for (price, tokens, label) in categories {
        total = total.checked_add(
            price
                .checked_mul_tokens(tokens)
                .with_context(|| format!("{label} cost"))?,
        )?;
    }
    if usage.cache_write_1h > 0 {
        let price = prices
            .cache_write_1h
            .context("cacheWrite1h price is required for reported 1h writes")?;
        total = total.checked_add(
            price
                .checked_mul_tokens(usage.cache_write_1h)
                .context("1h cache write cost")?,
        )?;
    }
    Ok(total)
}

/// 留作证据的原始用量。
///
/// **只取用量那一块**。传进来的往往是整条上游响应，里面有模型输出与工具参数；
/// 这个值会经结算写进账本，再由管理接口读出来。账本是财务记录，不该装内容。
///
/// 没有 `usage` 键时原样返回：那种形状下 `normalize_*` 找不到必需的计数，
/// 根本构造不出 `NativeUsage`，所以走不到这里。
fn evidence(raw: &Value) -> Value {
    match raw.get("usage") {
        Some(usage) => usage.clone(),
        None => raw.clone(),
    }
}

fn usage_object(raw: &Value) -> anyhow::Result<Option<&serde_json::Map<String, Value>>> {
    match raw.get("usage") {
        Some(value) => value
            .as_object()
            .map(Some)
            .context("usage must be an object"),
        None => Ok(raw.as_object()),
    }
}

fn optional_counter(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> anyhow::Result<Option<u64>> {
    object
        .get(field)
        .map(|value| {
            value
                .as_u64()
                .with_context(|| format!("{field} must be a nonnegative integer"))
        })
        .transpose()
}

fn required_counter(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> anyhow::Result<Option<u64>> {
    optional_counter(object, field)
}

fn category(
    value: Option<u64>,
    policy: CacheCategoryPolicy,
    name: &str,
) -> anyhow::Result<Option<(u64, EvidenceKind)>> {
    match policy {
        CacheCategoryPolicy::Reported => Ok(value.map(|value| (value, EvidenceKind::Reported))),
        CacheCategoryPolicy::NotApplicable => {
            ensure!(
                value.unwrap_or(0) == 0,
                "{name} is nonzero but configured not_applicable"
            );
            Ok(Some((0, EvidenceKind::NotApplicable)))
        }
    }
}

fn normalize_kiro(raw: &Value, policy: CacheUsagePolicy) -> anyhow::Result<Option<NativeUsage>> {
    let outer = raw.as_object().context("Kiro usage must be an object")?;
    let tokens = outer
        .get("tokenUsage")
        .and_then(Value::as_object)
        .or_else(|| outer.contains_key("uncachedInputTokens").then_some(outer));
    let credits = outer
        .get("credit_usage")
        .or_else(|| outer.get("usage"))
        .map(parse_amount_value)
        .transpose()?;
    let Some(tokens) = tokens else {
        return Ok(credits.map(|credits| NativeUsage {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: 0,
            credits: Some(credits),
            cache_evidence: CacheEvidence {
                cache_read: EvidenceKind::Missing,
                cache_write: EvidenceKind::Missing,
                cache_write_1h: EvidenceKind::Missing,
            },
            raw: evidence(raw),
        }));
    };
    let Some(input) = required_counter(tokens, "uncachedInputTokens")? else {
        return Ok(None);
    };
    let Some(output) = required_counter(tokens, "outputTokens")? else {
        return Ok(None);
    };
    let Some((cache_read, read_evidence)) = category(
        optional_counter(tokens, "cacheReadInputTokens")?,
        policy.cache_read,
        "cache read",
    )?
    else {
        return Ok(None);
    };
    let Some((cache_write, write_evidence)) = category(
        optional_counter(tokens, "cacheWriteInputTokens")?,
        policy.cache_write,
        "cache write",
    )?
    else {
        return Ok(None);
    };
    let Some((cache_write_1h, write_1h_evidence)) =
        category(None, policy.cache_write_1h, "1h cache write")?
    else {
        return Ok(None);
    };
    Ok(Some(NativeUsage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h,
        credits,
        cache_evidence: CacheEvidence {
            cache_read: read_evidence,
            cache_write: write_evidence,
            cache_write_1h: write_1h_evidence,
        },
        raw: evidence(raw),
    }))
}

fn normalize_anthropic(
    raw: &Value,
    policy: CacheUsagePolicy,
) -> anyhow::Result<Option<NativeUsage>> {
    let Some(object) = usage_object(raw)? else {
        return Ok(None);
    };
    let Some(input) = required_counter(object, "input_tokens")? else {
        return Ok(None);
    };
    let Some(output) = required_counter(object, "output_tokens")? else {
        return Ok(None);
    };
    let Some((cache_read, read_evidence)) = category(
        optional_counter(object, "cache_read_input_tokens")?,
        policy.cache_read,
        "cache read",
    )?
    else {
        return Ok(None);
    };
    let write_total = optional_counter(object, "cache_creation_input_tokens")?;
    let details = match object.get("cache_creation") {
        Some(value) => Some(
            value
                .as_object()
                .context("cache_creation must be an object")?,
        ),
        None => None,
    };
    let reported_write_5m = details
        .map(|details| optional_counter(details, "ephemeral_5m_input_tokens"))
        .transpose()?
        .flatten();
    let reported_write_1h = details
        .map(|details| optional_counter(details, "ephemeral_1h_input_tokens"))
        .transpose()?
        .flatten();

    if let Some(total) = write_total {
        let known = reported_write_5m
            .unwrap_or(0)
            .checked_add(reported_write_1h.unwrap_or(0))
            .context("Anthropic cache TTL write breakdown overflow")?;
        ensure!(
            known <= total,
            "Anthropic cache TTL write breakdown exceeds cache_creation_input_tokens"
        );
        if reported_write_5m.is_some() && reported_write_1h.is_some() {
            ensure!(
                known == total,
                "Anthropic cache TTL write breakdown does not sum to cache_creation_input_tokens"
            );
        }
    }

    let (cache_write, write_evidence, cache_write_1h, write_1h_evidence) = if details.is_some() {
        let five_minutes = category(reported_write_5m, policy.cache_write, "5m cache write")?;
        let one_hour = category(reported_write_1h, policy.cache_write_1h, "1h cache write")?;
        let (Some((five_minutes, write_evidence)), Some((one_hour, write_1h_evidence))) =
            (five_minutes, one_hour)
        else {
            return Ok(None);
        };
        let Some(total) = write_total else {
            return Ok(None);
        };
        ensure!(
            five_minutes.checked_add(one_hour) == Some(total),
            "Anthropic cache TTL write breakdown does not sum to cache_creation_input_tokens"
        );
        (five_minutes, write_evidence, one_hour, write_1h_evidence)
    } else {
        match (policy.cache_write, policy.cache_write_1h, write_total) {
            (CacheCategoryPolicy::Reported, CacheCategoryPolicy::Reported, Some(0)) => {
                (0, EvidenceKind::Reported, 0, EvidenceKind::Reported)
            }
            (CacheCategoryPolicy::Reported, CacheCategoryPolicy::NotApplicable, Some(total)) => (
                total,
                EvidenceKind::Reported,
                0,
                EvidenceKind::NotApplicable,
            ),
            (CacheCategoryPolicy::NotApplicable, CacheCategoryPolicy::Reported, Some(total)) => (
                0,
                EvidenceKind::NotApplicable,
                total,
                EvidenceKind::Reported,
            ),
            (CacheCategoryPolicy::NotApplicable, CacheCategoryPolicy::NotApplicable, total) => {
                ensure!(
                    total.unwrap_or(0) == 0,
                    "cache write total is nonzero but all write buckets are not_applicable"
                );
                (
                    0,
                    EvidenceKind::NotApplicable,
                    0,
                    EvidenceKind::NotApplicable,
                )
            }
            _ => return Ok(None),
        }
    };
    Ok(Some(NativeUsage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h,
        credits: None,
        cache_evidence: CacheEvidence {
            cache_read: read_evidence,
            cache_write: write_evidence,
            cache_write_1h: write_1h_evidence,
        },
        raw: evidence(raw),
    }))
}

fn normalize_openai(
    raw: &Value,
    policy: CacheUsagePolicy,
    input_field: &str,
    output_field: &str,
    details_field: &str,
) -> anyhow::Result<Option<NativeUsage>> {
    let Some(object) = usage_object(raw)? else {
        return Ok(None);
    };
    let Some(input_total) = required_counter(object, input_field)? else {
        return Ok(None);
    };
    let Some(output) = required_counter(object, output_field)? else {
        return Ok(None);
    };
    let details = match object.get(details_field) {
        Some(value) => Some(
            value
                .as_object()
                .with_context(|| format!("{details_field} must be an object"))?,
        ),
        None => None,
    };
    let read = details
        .map(|details| optional_counter(details, "cached_tokens"))
        .transpose()?
        .flatten();
    let write = details
        .map(|details| optional_counter(details, "cache_write_tokens"))
        .transpose()?
        .flatten();
    let Some((cache_read, read_evidence)) = category(read, policy.cache_read, "cache read")? else {
        return Ok(None);
    };
    let Some((cache_write, write_evidence)) = category(write, policy.cache_write, "cache write")?
    else {
        return Ok(None);
    };
    let Some((cache_write_1h, write_1h_evidence)) =
        category(None, policy.cache_write_1h, "1h cache write")?
    else {
        return Ok(None);
    };
    let input = input_total
        .checked_sub(cache_read)
        .and_then(|value| value.checked_sub(cache_write))
        .context("OpenAI input total is smaller than its cache subsets")?;
    Ok(Some(NativeUsage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h,
        credits: None,
        cache_evidence: CacheEvidence {
            cache_read: read_evidence,
            cache_write: write_evidence,
            cache_write_1h: write_1h_evidence,
        },
        raw: evidence(raw),
    }))
}

fn parse_amount_value(value: &Value) -> anyhow::Result<Amount> {
    match value {
        Value::String(value) => value.parse(),
        Value::Number(value) => value.to_string().parse(),
        _ => anyhow::bail!("credit usage must be a decimal string or number"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::BillingUnit;
    use serde_json::json;

    #[test]
    fn cached_input_is_not_charged_as_ordinary_input() {
        let raw = json!({"prompt_tokens":1000,"completion_tokens":100,
            "prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":100}});
        let usage = normalize_usage(UpstreamKind::OpenaiChat, &raw)
            .unwrap()
            .unwrap();
        assert_eq!(
            (usage.input, usage.cache_read, usage.cache_write),
            (100, 800, 100)
        );
    }

    #[test]
    fn kiro_preserves_native_credit_and_complete_token_evidence() {
        let credit_only = json!({"unit":"credit","usage":0.0169543708291874});
        let usage = normalize_usage(UpstreamKind::Kiro, &credit_only)
            .unwrap()
            .unwrap();
        assert_eq!(usage.credits.unwrap().to_string(), "0.0169543708291874");
        assert_eq!(usage.cache_evidence.cache_read, EvidenceKind::Missing);

        let raw = json!({"tokenUsage": {
            "uncachedInputTokens": 11, "outputTokens": 3,
            "cacheReadInputTokens": 7, "cacheWriteInputTokens": 5
        }});
        let usage = normalize_usage(UpstreamKind::Kiro, &raw).unwrap().unwrap();
        assert_eq!(
            (
                usage.input,
                usage.output,
                usage.cache_read,
                usage.cache_write
            ),
            (11, 3, 7, 5)
        );
        assert_eq!(
            usage.cache_evidence.cache_write_1h,
            EvidenceKind::NotApplicable
        );
        assert_eq!(usage.raw, raw);
    }

    #[test]
    fn kiro_numeric_credit_preserves_the_json_lexeme() {
        let raw: Value =
            serde_json::from_str(r#"{"unit":"credit","usage":0.123456789012345678}"#).unwrap();
        let usage = normalize_usage(UpstreamKind::Kiro, &raw).unwrap().unwrap();
        assert_eq!(usage.credits.unwrap().to_string(), "0.123456789012345678");
    }

    #[test]
    fn missing_required_totals_are_unknown_but_complete_zero_is_known() {
        assert!(
            normalize_usage(UpstreamKind::OpenaiResponses, &json!({"input_tokens": 1}))
                .unwrap()
                .is_none()
        );
        let usage = normalize_usage(
            UpstreamKind::OpenaiResponses,
            &json!({
                "input_tokens":0,"output_tokens":0,
                "input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0}
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!((usage.input, usage.output), (0, 0));
    }

    #[test]
    fn anthropic_ttl_breakdown_must_equal_total_writes() {
        let raw = json!({
            "input_tokens": 10, "output_tokens": 2, "cache_read_input_tokens": 3,
            "cache_creation_input_tokens": 8,
            "cache_creation": {"ephemeral_5m_input_tokens": 5, "ephemeral_1h_input_tokens": 4}
        });
        assert!(normalize_usage(UpstreamKind::Anthropic, &raw).is_err());
    }

    #[test]
    fn anthropic_nonzero_writes_without_ttl_breakdown_are_unknown() {
        let raw = json!({
            "input_tokens": 10, "output_tokens": 2, "cache_read_input_tokens": 3,
            "cache_creation_input_tokens": 8
        });
        assert!(
            normalize_usage(UpstreamKind::Anthropic, &raw)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn anthropic_zero_writes_without_ttl_details_are_confirmed_zero() {
        let raw = json!({
            "input_tokens": 10, "output_tokens": 2, "cache_read_input_tokens": 3,
            "cache_creation_input_tokens": 0
        });
        let usage = normalize_usage(UpstreamKind::Anthropic, &raw)
            .unwrap()
            .unwrap();
        assert_eq!((usage.cache_write, usage.cache_write_1h), (0, 0));
        assert_eq!(usage.cache_evidence.cache_write, EvidenceKind::Reported);
        assert_eq!(usage.cache_evidence.cache_write_1h, EvidenceKind::Reported);
        assert_eq!(usage.raw, raw);
    }

    #[test]
    fn anthropic_not_applicable_buckets_still_validate_raw_ttl_breakdown() {
        let policy = CacheUsagePolicy {
            cache_read: CacheCategoryPolicy::Reported,
            cache_write: CacheCategoryPolicy::NotApplicable,
            cache_write_1h: CacheCategoryPolicy::NotApplicable,
        };
        let raw = json!({
            "input_tokens": 10, "output_tokens": 2, "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
            "cache_creation": {"ephemeral_5m_input_tokens": 5, "ephemeral_1h_input_tokens": 0}
        });
        assert!(normalize_usage_with_policy(UpstreamKind::Anthropic, &raw, policy).is_err());
    }

    #[test]
    fn anthropic_missing_reported_ttl_bucket_remains_unknown() {
        let raw = json!({
            "input_tokens": 10, "output_tokens": 2, "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 8,
            "cache_creation": {"ephemeral_5m_input_tokens": 8}
        });
        assert!(
            normalize_usage(UpstreamKind::Anthropic, &raw)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn computes_each_bucket_exactly() {
        let prices = TokenPrices {
            currency: BillingUnit::Usd,
            input: "1.25".parse().unwrap(),
            output: "2.5".parse().unwrap(),
            cache_read: "0.1".parse().unwrap(),
            cache_write: "3".parse().unwrap(),
            cache_write_1h: Some("4".parse().unwrap()),
        };
        let usage = NativeUsage {
            input: 1_200_000,
            output: 200_000,
            cache_read: 500_000,
            cache_write: 100_000,
            cache_write_1h: 50_000,
            credits: None,
            cache_evidence: CacheEvidence {
                cache_read: EvidenceKind::Reported,
                cache_write: EvidenceKind::Reported,
                cache_write_1h: EvidenceKind::Reported,
            },
            raw: Value::Null,
        };
        assert_eq!(token_cost(&prices, &usage).unwrap().to_string(), "2.55");
    }

    #[test]
    fn accumulated_token_cost_overflow_is_rejected() {
        let prices = TokenPrices {
            currency: BillingUnit::Usd,
            input: "100000000000000000000".parse().unwrap(),
            output: "100000000000000000000".parse().unwrap(),
            cache_read: Amount::ZERO,
            cache_write: Amount::ZERO,
            cache_write_1h: None,
        };
        let usage = NativeUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: 0,
            credits: None,
            cache_evidence: CacheEvidence {
                cache_read: EvidenceKind::Reported,
                cache_write: EvidenceKind::Reported,
                cache_write_1h: EvidenceKind::NotApplicable,
            },
            raw: Value::Null,
        };
        assert!(token_cost(&prices, &usage).is_err());
    }

    #[test]
    fn cache_write_not_applicable_is_explicit_and_rejects_contradictory_usage() {
        let policy = CacheUsagePolicy {
            cache_read: CacheCategoryPolicy::Reported,
            cache_write: CacheCategoryPolicy::NotApplicable,
            cache_write_1h: CacheCategoryPolicy::NotApplicable,
        };
        let raw = json!({"prompt_tokens":100,"completion_tokens":5,
            "prompt_tokens_details":{"cached_tokens":20}});
        let usage = normalize_usage_with_policy(UpstreamKind::OpenaiChat, &raw, policy)
            .unwrap()
            .unwrap();
        assert_eq!(
            usage.cache_evidence.cache_write,
            EvidenceKind::NotApplicable
        );
        assert_eq!(
            (usage.input, usage.cache_read, usage.cache_write),
            (80, 20, 0)
        );

        let contradictory = json!({"prompt_tokens":100,"completion_tokens":5,
            "prompt_tokens_details":{"cached_tokens":20,"cache_write_tokens":1}});
        assert!(
            normalize_usage_with_policy(UpstreamKind::OpenaiChat, &contradictory, policy).is_err()
        );
    }

    #[test]
    fn strict_openai_policy_does_not_turn_missing_cache_write_into_zero() {
        let raw = json!({"prompt_tokens":100,"completion_tokens":5,
            "prompt_tokens_details":{"cached_tokens":20}});
        assert!(
            normalize_usage(UpstreamKind::OpenaiChat, &raw)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn malformed_usage_objects_are_rejected() {
        assert!(normalize_usage(UpstreamKind::OpenaiChat, &json!({"usage": 7})).is_err());
        assert!(
            normalize_usage(
                UpstreamKind::Anthropic,
                &json!({
                    "input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0, "cache_creation": "bad"
                })
            )
            .is_err()
        );
    }

    #[test]
    fn token_cost_rejects_missing_cache_evidence() {
        let prices = TokenPrices {
            currency: BillingUnit::Usd,
            input: "1".parse().unwrap(),
            output: "1".parse().unwrap(),
            cache_read: "1".parse().unwrap(),
            cache_write: "1".parse().unwrap(),
            cache_write_1h: Some("1".parse().unwrap()),
        };
        let usage = NativeUsage {
            input: 1,
            output: 1,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: 0,
            credits: None,
            cache_evidence: CacheEvidence {
                cache_read: EvidenceKind::Missing,
                cache_write: EvidenceKind::Missing,
                cache_write_1h: EvidenceKind::Missing,
            },
            raw: Value::Null,
        };
        assert!(token_cost(&prices, &usage).is_err());
    }
}
