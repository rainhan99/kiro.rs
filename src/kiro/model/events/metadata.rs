//! 上游元数据事件
//!
//! Kiro 在 `metadataEvent.tokenUsage` 中返回本次模型调用的精确 token 用量。
//! 四个字段是单次调用的最终快照，不是增量事件；调用方应在同一条流内保留最后一份快照。

use serde::{Deserialize, Deserializer, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 单次 Kiro 模型调用的精确 token 用量。
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// 未命中缓存、也未写入缓存的输入 token。
    #[serde(deserialize_with = "nonnegative_counter")]
    pub uncached_input_tokens: i32,
    /// 模型输出 token。
    #[serde(deserialize_with = "nonnegative_counter")]
    pub output_tokens: i32,
    /// 从服务端 prompt cache 读取的输入 token。
    #[serde(deserialize_with = "nonnegative_counter")]
    pub cache_read_input_tokens: i32,
    /// 本次写入服务端 prompt cache 的输入 token。
    #[serde(deserialize_with = "nonnegative_counter")]
    pub cache_write_input_tokens: i32,
}

fn nonnegative_counter<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i32, D::Error> {
    let counter = i32::deserialize(deserializer)?;
    if counter < 0 {
        return Err(serde::de::Error::custom(
            "native token counter must be nonnegative",
        ));
    }
    Ok(counter)
}

/// Incomplete usage must not reject the surrounding metadata event or become zero truth.
fn optional_native_usage<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<TokenUsage>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

impl TokenUsage {
    /// 清理不可信上游值，确保所有计数非负。
    pub fn sanitized(self) -> Self {
        Self {
            uncached_input_tokens: self.uncached_input_tokens.max(0),
            output_tokens: self.output_tokens.max(0),
            cache_read_input_tokens: self.cache_read_input_tokens.max(0),
            cache_write_input_tokens: self.cache_write_input_tokens.max(0),
        }
    }

    #[cfg(test)]
    /// OpenAI 口径的总输入 token（缓存读取是其中的子集）。
    pub fn total_input_tokens(self) -> i32 {
        let usage = self.sanitized();
        usage
            .uncached_input_tokens
            .saturating_add(usage.cache_write_input_tokens)
            .saturating_add(usage.cache_read_input_tokens)
    }

    /// 合并多次真实 provider 调用的用量。
    pub fn saturating_add(self, other: Self) -> Self {
        let left = self.sanitized();
        let right = other.sanitized();
        Self {
            uncached_input_tokens: left
                .uncached_input_tokens
                .saturating_add(right.uncached_input_tokens),
            output_tokens: left.output_tokens.saturating_add(right.output_tokens),
            cache_read_input_tokens: left
                .cache_read_input_tokens
                .saturating_add(right.cache_read_input_tokens),
            cache_write_input_tokens: left
                .cache_write_input_tokens
                .saturating_add(right.cache_write_input_tokens),
        }
    }
}

/// `metadataEvent` payload。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    /// 有些 metadataEvent 只携带 stopReason，因此 tokenUsage 必须保持可选。
    #[serde(default, deserialize_with = "optional_native_usage")]
    pub token_usage: Option<TokenUsage>,
}

impl EventPayload for MetadataEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_official_token_usage_shape() {
        let event: MetadataEvent = serde_json::from_str(
            r#"{
                "tokenUsage": {
                    "uncachedInputTokens": 101,
                    "outputTokens": 23,
                    "cacheReadInputTokens": 300,
                    "cacheWriteInputTokens": 40
                },
                "stopReason": "end_turn"
            }"#,
        )
        .unwrap();

        let usage = event.token_usage.unwrap();
        assert_eq!(usage.uncached_input_tokens, 101);
        assert_eq!(usage.output_tokens, 23);
        assert_eq!(usage.cache_read_input_tokens, 300);
        assert_eq!(usage.cache_write_input_tokens, 40);
        assert_eq!(usage.total_input_tokens(), 441);
    }

    #[test]
    fn metadata_without_token_usage_is_not_treated_as_zero_truth() {
        let event: MetadataEvent = serde_json::from_str(r#"{"stopReason":"end_turn"}"#).unwrap();
        assert!(event.token_usage.is_none());
    }

    #[test]
    fn incomplete_native_snapshots_remain_unknown() {
        let complete = serde_json::json!({
            "uncachedInputTokens": 0,
            "outputTokens": 9,
            "cacheReadInputTokens": 0,
            "cacheWriteInputTokens": 0,
        });
        for missing in [
            "uncachedInputTokens",
            "outputTokens",
            "cacheReadInputTokens",
            "cacheWriteInputTokens",
        ] {
            let mut usage = complete.clone();
            usage.as_object_mut().unwrap().remove(missing);
            let event: MetadataEvent = serde_json::from_value(serde_json::json!({
                "tokenUsage": usage,
                "stopReason": "end_turn",
            }))
            .unwrap();
            assert!(event.token_usage.is_none(), "missing {missing} is unknown");
        }
    }

    #[test]
    fn malformed_native_counters_do_not_discard_the_metadata_event() {
        for field in [
            "uncachedInputTokens",
            "outputTokens",
            "cacheReadInputTokens",
            "cacheWriteInputTokens",
        ] {
            for invalid in [
                serde_json::json!(-1),
                serde_json::json!(null),
                serde_json::json!("7"),
                serde_json::json!(1.5),
                serde_json::json!(true),
                serde_json::json!([]),
                serde_json::json!({}),
                serde_json::json!(2_147_483_648_u64),
            ] {
                let mut usage = serde_json::json!({
                    "uncachedInputTokens": 1,
                    "outputTokens": 9,
                    "cacheReadInputTokens": 3,
                    "cacheWriteInputTokens": 2,
                });
                usage[field] = invalid;
                let event: MetadataEvent = serde_json::from_value(serde_json::json!({
                    "tokenUsage": usage,
                    "stopReason": "end_turn",
                }))
                .unwrap();
                assert!(event.token_usage.is_none(), "malformed {field} is unknown");
            }
        }
    }

    #[test]
    fn absent_or_malformed_usage_is_unknown_but_complete_zero_usage_is_native() {
        for usage in [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!([]),
            serde_json::json!(0),
            serde_json::json!("unavailable"),
        ] {
            let event: MetadataEvent = serde_json::from_value(serde_json::json!({
                "tokenUsage": usage,
                "stopReason": "end_turn",
            }))
            .unwrap();
            assert!(event.token_usage.is_none());
        }
        let event: MetadataEvent = serde_json::from_value(serde_json::json!({
            "tokenUsage": {
                "uncachedInputTokens": 0,
                "outputTokens": 0,
                "cacheReadInputTokens": 0,
                "cacheWriteInputTokens": 0,
            },
        }))
        .unwrap();
        assert_eq!(event.token_usage, Some(TokenUsage::default()));
    }

    #[test]
    fn sanitizes_negative_values() {
        let usage = TokenUsage {
            uncached_input_tokens: -1,
            output_tokens: -2,
            cache_read_input_tokens: -3,
            cache_write_input_tokens: -4,
        }
        .sanitized();

        assert_eq!(usage, TokenUsage::default());
    }

    #[test]
    fn adds_multiple_provider_calls_without_overflowing() {
        let first = TokenUsage {
            uncached_input_tokens: i32::MAX,
            output_tokens: 3,
            cache_read_input_tokens: 20,
            cache_write_input_tokens: 4,
        };
        let second = TokenUsage {
            uncached_input_tokens: 7,
            output_tokens: 5,
            cache_read_input_tokens: 11,
            cache_write_input_tokens: 2,
        };

        assert_eq!(
            first.saturating_add(second),
            TokenUsage {
                uncached_input_tokens: i32::MAX,
                output_tokens: 8,
                cache_read_input_tokens: 31,
                cache_write_input_tokens: 6,
            }
        );
    }
}
