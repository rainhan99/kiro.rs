//! 上下文使用率事件
//!
//! 处理 contextUsageEvent 类型的事件

use serde::Deserialize;
use serde_json::Value;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 脱敏结构的递归深度上限。
const SHAPE_MAX_DEPTH: usize = 6;
/// 单个对象/数组保留的成员数上限。
const SHAPE_MAX_MEMBERS: usize = 32;

/// 上下文使用率事件
///
/// 包含当前上下文窗口的使用百分比
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageEvent {
    /// 上下文使用百分比 (0-100)
    #[serde(default)]
    pub context_usage_percentage: f64,

    /// 上游在该事件里实际发送的载荷**结构**（已脱敏）。
    ///
    /// 此前本类型只反序列化百分比且没有 `deny_unknown_fields`，上游发送的其它字段
    /// （例如需求里要的 breakdown）被静默丢弃——结果是我们既不知道 breakdown 长什么
    /// 样，又不允许发探测流量去看。保留结构可以让形状从**本来就会发生的业务流量**
    /// 中被观察到。
    ///
    /// 只保留字段名、数值与容器结构；字符串值一律替换为长度标记（见
    /// [`redact_shape`]）。我们要知道的是「有哪些字段、数值是多少」，而不是任意
    /// 文本；在不知道上游会发什么的前提下，这是从根上杜绝提示词泄漏的唯一办法。
    #[serde(skip)]
    pub shape: Option<Value>,
}

/// 保留结构与数值，丢弃字符串内容。
///
/// - 数字 / 布尔 / null：原样保留（breakdown 的信息量就在这里）
/// - 字符串：替换为 `"<str:N>"`，只留长度
/// - 对象 / 数组：递归，受深度与成员数上限约束，超出部分记为标记而非静默截断
fn redact_shape(value: &Value, depth: usize) -> Value {
    if depth >= SHAPE_MAX_DEPTH {
        return Value::String("<depth-capped>".to_string());
    }
    match value {
        Value::String(s) => Value::String(format!("<str:{}>", s.len())),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, item) in map.iter().take(SHAPE_MAX_MEMBERS) {
                out.insert(key.clone(), redact_shape(item, depth + 1));
            }
            if map.len() > SHAPE_MAX_MEMBERS {
                out.insert(
                    "<truncated-members>".to_string(),
                    Value::from(map.len() - SHAPE_MAX_MEMBERS),
                );
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            let mut out: Vec<Value> = items
                .iter()
                .take(SHAPE_MAX_MEMBERS)
                .map(|item| redact_shape(item, depth + 1))
                .collect();
            if items.len() > SHAPE_MAX_MEMBERS {
                out.push(Value::String(format!(
                    "<truncated-items:{}>",
                    items.len() - SHAPE_MAX_MEMBERS
                )));
            }
            Value::Array(out)
        }
        other => other.clone(),
    }
}

impl EventPayload for ContextUsageEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        Self::from_payload(&frame.payload)
    }
}

impl ContextUsageEvent {
    /// 从原始载荷构造。百分比的解析与消费方式完全不变，只是额外留下脱敏结构。
    pub(crate) fn from_payload(payload: &[u8]) -> ParseResult<Self> {
        let mut event: Self = serde_json::from_slice(payload)
            .map_err(crate::kiro::parser::error::ParseError::PayloadDeserialize)?;
        // 结构提取失败不影响事件本身：它只是证据，不是功能。
        event.shape = serde_json::from_slice::<Value>(payload)
            .ok()
            .map(|value| redact_shape(&value, 0));
        Ok(event)
    }

    /// 获取格式化的百分比字符串
    pub fn formatted_percentage(&self) -> String {
        format!("{:.2}%", self.context_usage_percentage)
    }
}

impl std::fmt::Display for ContextUsageEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.formatted_percentage())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(payload: serde_json::Value) -> ContextUsageEvent {
        ContextUsageEvent::from_payload(payload.to_string().as_bytes()).unwrap()
    }

    /// 上游发送的未建模字段必须被留下来，否则 breakdown 的形状永远无从得知。
    #[test]
    fn unmodelled_fields_survive_as_shape() {
        let event = parse(json!({
            "contextUsagePercentage": 42.5,
            "breakdown": {"systemTokens": 1200, "historyTokens": 30000, "toolTokens": 800},
            "windowTokens": 200000
        }));
        assert_eq!(event.context_usage_percentage, 42.5);
        let shape = event.shape.expect("必须保留结构");
        assert_eq!(shape["breakdown"]["historyTokens"], 30000);
        assert_eq!(shape["windowTokens"], 200000);
    }

    /// 字符串值一律只留长度：我们不知道上游会在这里放什么，
    /// 结构化丢弃是唯一能在未知前提下杜绝提示词泄漏的办法。
    #[test]
    fn string_values_are_reduced_to_their_length() {
        let event = parse(json!({
            "contextUsagePercentage": 1.0,
            "note": "PRIVATE_PROMPT_TEXT",
            "nested": {"alsoText": "SECRET"}
        }));
        let shape = event.shape.unwrap();
        let dumped = shape.to_string();
        assert!(!dumped.contains("PRIVATE_PROMPT_TEXT"));
        assert!(!dumped.contains("SECRET"));
        assert_eq!(shape["note"], "<str:19>");
        assert_eq!(shape["nested"]["alsoText"], "<str:6>");
    }

    /// 只有百分比的载荷行为完全不变。
    #[test]
    fn percentage_only_payload_is_unchanged() {
        let event = parse(json!({"contextUsagePercentage": 99.9}));
        assert_eq!(event.context_usage_percentage, 99.9);
        assert_eq!(event.formatted_percentage(), "99.90%");
        assert_eq!(event.shape.unwrap(), json!({"contextUsagePercentage": 99.9}));
    }

    /// 病态深度/宽度必须有界终止，且超出部分要留下标记而不是静默消失。
    #[test]
    fn shape_is_bounded_and_marks_what_it_dropped() {
        let mut deep = json!({"contextUsagePercentage": 1.0});
        for _ in 0..40 {
            deep = json!({"contextUsagePercentage": 1.0, "next": deep});
        }
        let shape = parse(deep).shape.unwrap();
        assert!(shape.to_string().contains("<depth-capped>"));

        let mut wide = serde_json::Map::new();
        wide.insert("contextUsagePercentage".into(), json!(1.0));
        for i in 0..100 {
            wide.insert(format!("f{i}"), json!(i));
        }
        let shape = parse(Value::Object(wide)).shape.unwrap();
        assert!(shape["<truncated-members>"].as_u64().unwrap() > 0);
    }

    /// 缺失百分比仍按既有默认值处理，不因为新增结构提取而改变。
    #[test]
    fn missing_percentage_still_defaults() {
        let event = parse(json!({"somethingElse": 1}));
        assert_eq!(event.context_usage_percentage, 0.0);
    }
}
