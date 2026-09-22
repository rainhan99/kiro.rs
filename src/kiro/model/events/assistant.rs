//! 助手响应事件
//!
//! 处理 assistantResponseEvent 类型的事件

use serde::{Deserialize, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 剥离混入 assistant 文本的字面 `<tool_use ...>...</tool_use>` XML 泄漏。
///
/// Kiro（上游）有时把工具调用意图以字面 XML 吐进文本里——真正的调用走结构化
/// `toolUseEvent`，这段 XML 是重复噪声，需删除以免原样透传给客户端。
///
/// - 真标签判定：`<tool_use` 之后必须紧跟空白或直接 `>`，从而保留形似但非标签的
///   文本（如 `<tool_user>`）。
/// - 找到 `</tool_use>` 则整段删除；未闭合（截断的开标签）则从 `<tool_use` 丢弃到末尾。
/// - 结果 `trim`。
pub(crate) fn strip_tool_use_xml_leaks(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(start) = rest.find("<tool_use") {
        out.push_str(&rest[..start]);
        let after_start = &rest[start..];
        let Some(open_end) = after_start.find('>') else {
            // 开标签未闭合（被截断）：丢弃到末尾。
            rest = "";
            break;
        };
        let tag_head = &after_start[..open_end];
        if !tag_head
            .get("<tool_use".len()..)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
        {
            // 形似但非真标签（如 `<tool_user>`）：原样保留 `<tool_use`，继续扫描其后。
            out.push_str(&after_start[.."<tool_use".len()]);
            rest = &after_start["<tool_use".len()..];
            continue;
        }

        let after_open = &after_start[open_end + 1..];
        if let Some(close_start) = after_open.find("</tool_use>") {
            rest = &after_open[close_start + "</tool_use>".len()..];
        } else {
            // 有合法开标签但无闭合：丢弃到末尾。
            rest = "";
            break;
        }
    }

    out.push_str(rest);
    out.trim().to_string()
}

/// 助手响应事件
///
/// 包含 AI 助手的流式响应内容
///
/// # 设计说明
///
/// 此结构体只保留实际使用的 `content` 字段，其他 API 返回的字段
/// 通过 `#[serde(flatten)]` 捕获到 `extra` 中，确保反序列化不会失败。
///
/// # 示例
///
/// ```rust
/// use kiro_rs::kiro::model::events::AssistantResponseEvent;
///
/// let json = r#"{"content":"Hello, world!"}"#;
/// let event: AssistantResponseEvent = serde_json::from_str(json).unwrap();
/// assert_eq!(event.content, "Hello, world!");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantResponseEvent {
    /// 响应内容片段
    #[serde(default)]
    pub content: String,

    /// 捕获其他未使用的字段，确保反序列化兼容性
    #[serde(flatten)]
    #[serde(skip_serializing)]
    #[allow(dead_code)]
    extra: serde_json::Value,
}

impl EventPayload for AssistantResponseEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

impl Default for AssistantResponseEvent {
    fn default() -> Self {
        Self {
            content: String::new(),
            extra: serde_json::Value::Null,
        }
    }
}

impl std::fmt::Display for AssistantResponseEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_tool_use_xml_leaks() {
        // 剥离标签块本身，保留其两侧文本（周围换行原样保留）。
        let content = "before\n<tool_use id=\"toolu_1\" name=\"Read\">\n{\"path\":\"/a\"}\n</tool_use>\nafter";
        assert_eq!(strip_tool_use_xml_leaks(content), "before\n\nafter");
    }

    #[test]
    fn test_strip_tool_use_xml_leaks_keeps_similar_text() {
        // `<tool_user>` 不是真标签（`<tool_use` 后紧跟 `r`），应原样保留。
        let content = "use <tool_user> as an example";
        assert_eq!(strip_tool_use_xml_leaks(content), content);
    }

    #[test]
    fn test_strip_tool_use_xml_leaks_drops_truncated_open_tag() {
        let content = "before\n\n<tool_use id=\"toolu_1\" name=\"Write\"";
        assert_eq!(strip_tool_use_xml_leaks(content), "before");
    }

    #[test]
    fn test_deserialize_simple() {
        let json = r#"{"content":"Hello, world!"}"#;
        let event: AssistantResponseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.content, "Hello, world!");
    }

    #[test]
    fn test_deserialize_with_extra_fields() {
        // 确保包含额外字段时反序列化不会失败
        let json = r#"{
            "content": "Done",
            "conversationId": "conv-123",
            "messageId": "msg-456",
            "messageStatus": "COMPLETED",
            "followupPrompt": {
                "content": "Would you like me to explain further?",
                "userIntent": "EXPLAIN_CODE_SELECTION"
            }
        }"#;
        let event: AssistantResponseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.content, "Done");
    }

    #[test]
    fn test_serialize_minimal() {
        let event = AssistantResponseEvent::default();
        let event = AssistantResponseEvent {
            content: "Test".to_string(),
            ..event
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"content\":\"Test\""));
        // extra 字段不应该被序列化
        assert!(!json.contains("extra"));
    }

    #[test]
    fn test_display() {
        let event = AssistantResponseEvent {
            content: "test".to_string(),
            ..Default::default()
        };
        assert_eq!(format!("{}", event), "test");
    }
}
