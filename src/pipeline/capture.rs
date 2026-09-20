//! 入站请求的形状抓取，用于排查「为什么这个请求被拒/被改」。
//!
//! # 默认只抓形状，不抓内容
//!
//! 诊断这类问题需要的是**结构**：有哪些角色、哪些内容块类型、字段叫什么、各有多长。
//! 提示词原文对判断毫无帮助，却是最敏感的东西。所以默认把所有字符串换成
//! `<str:N>`（N 是字符数），数组和对象保留骨架。这样抓下来的东西可以直接贴给别人
//! 看，不必先自己脱敏一遍——而"先脱敏再发"这一步，现实中没人会做。
//!
//! 确实需要原文的，把 `redactText` 关掉；界面上会写明那意味着什么。
//!
//! # 有界
//!
//! 环形缓冲，条数与单条大小都有上限，进程重启即清空。它是排查工具，不是日志系统：
//! 一个没有上限的抓包开关，忘了关就变成一个慢慢吃光内存、且装满提示词的文件。

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 抓取配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureConfig {
    /// 默认关。开着它才会留下任何东西。
    pub enabled: bool,
    /// 最多保留多少条，超出丢最旧的。
    pub max_requests: usize,
    /// 单条序列化后的字节上限，超出截断并标记。
    pub max_bytes: usize,
    /// 把字符串替换成 `<str:N>`。**默认开**——见模块头。
    pub redact_text: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_requests: 20,
            max_bytes: 256 * 1024,
            redact_text: true,
        }
    }
}

/// 抓到的一条。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Captured {
    pub at: String,
    pub trace_id: Option<String>,
    pub model: String,
    pub stream: bool,
    /// 角色序列，一眼看出形状。
    pub roles: Vec<String>,
    /// 出现过的内容块类型及其条数。
    pub block_types: Vec<(String, usize)>,
    /// 请求体。`redactText` 开着时字符串已换成 `<str:N>`。
    pub body: Value,
    /// 因超出单条上限而被截断。
    pub truncated: bool,
}

/// 环形缓冲。
pub struct CaptureStore {
    entries: Mutex<std::collections::VecDeque<Captured>>,
}

impl Default for CaptureStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureStore {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// 抓一条。`enabled` 为假时什么都不做——包括不做脱敏计算。
    pub fn record(&self, config: &CaptureConfig, trace_id: Option<String>, body: &Value) {
        if !config.enabled {
            return;
        }
        let shaped = if config.redact_text {
            redact(body)
        } else {
            body.clone()
        };
        let serialized = serde_json::to_vec(&shaped).unwrap_or_default();
        let truncated = serialized.len() > config.max_bytes;
        let entry = Captured {
            at: chrono::Utc::now().to_rfc3339(),
            trace_id,
            model: body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("<missing>")
                .to_string(),
            stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
            roles: roles_of(body),
            block_types: block_types_of(body),
            body: if truncated { Value::Null } else { shaped },
            truncated,
        };
        let mut entries = self.entries.lock();
        entries.push_back(entry);
        while entries.len() > config.max_requests.max(1) {
            entries.pop_front();
        }
    }

    /// 最近抓到的，最新在前。
    pub fn recent(&self) -> Vec<Captured> {
        self.entries.lock().iter().rev().cloned().collect()
    }

    pub fn clear(&self) {
        self.entries.lock().clear();
    }
}

fn roles_of(body: &Value) -> Vec<String> {
    body.get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .map(|m| {
                    m.get("role")
                        .and_then(Value::as_str)
                        .unwrap_or("<missing>")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn block_types_of(body: &Value) -> Vec<(String, usize)> {
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            match message.get("content") {
                Some(Value::String(_)) => *counts.entry("<string>".into()).or_default() += 1,
                Some(Value::Array(blocks)) => {
                    for block in blocks {
                        let kind = block
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("<missing>");
                        *counts.entry(kind.to_string()).or_default() += 1;
                    }
                }
                _ => {}
            }
        }
    }
    counts.into_iter().collect()
}

/// 把所有字符串换成 `<str:N>`，保留整棵树的骨架。
///
/// 字段名保留——它们是结构的一部分，且不是用户内容。数字与布尔保留：它们是
/// `max_tokens`、`stream` 这类参数，正是要看的东西。
fn redact(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(format!("<str:{}>", text.chars().count())),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), redact(item)))
                .collect::<Map<_, _>>(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
