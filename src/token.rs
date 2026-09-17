//! Token 计算模块
//!
//! 提供文本 token 数量计算功能。
//!
//! # 计算规则
//! - 非西文字符：每个计 4.5 个字符单位
//! - 西文字符：每个计 1 个字符单位
//! - 4 个字符单位 = 1 token（四舍五入）

use crate::anthropic::types::{
    CountTokensRequest, CountTokensResponse, Message, SystemMessage, Tool,
};
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use std::sync::OnceLock;

/// Count Tokens API 配置
#[derive(Clone, Default)]
pub struct CountTokensConfig {
    /// 外部 count_tokens API 地址
    pub api_url: Option<String>,
    /// count_tokens API 密钥
    pub api_key: Option<String>,
    /// count_tokens API 认证类型（"x-api-key" 或 "bearer"）
    pub auth_type: String,
    /// 代理配置
    pub proxy: Option<ProxyConfig>,

    pub tls_backend: TlsBackend,
}

/// 全局配置存储
static COUNT_TOKENS_CONFIG: OnceLock<CountTokensConfig> = OnceLock::new();

/// 初始化 count_tokens 配置
///
/// 应在应用启动时调用一次
pub fn init_config(config: CountTokensConfig) {
    let _ = COUNT_TOKENS_CONFIG.set(config);
}

/// 获取配置
fn get_config() -> Option<&'static CountTokensConfig> {
    COUNT_TOKENS_CONFIG.get()
}

/// 判断字符是否为非西文字符
///
/// 西文字符包括：
/// - ASCII 字符 (U+0000..U+007F)
/// - 拉丁字母扩展 (U+0080..U+024F)
/// - 拉丁字母扩展附加 (U+1E00..U+1EFF)
///
/// 返回 true 表示该字符是非西文字符（如中文、日文、韩文、阿拉伯文等）
fn is_non_western_char(c: char) -> bool {
    !matches!(c,
        // 基本 ASCII
        '\u{0000}'..='\u{007F}' |
        // 拉丁字母扩展-A (Latin Extended-A)
        '\u{0080}'..='\u{00FF}' |
        // 拉丁字母扩展-B (Latin Extended-B)
        '\u{0100}'..='\u{024F}' |
        // 拉丁字母扩展附加 (Latin Extended Additional)
        '\u{1E00}'..='\u{1EFF}' |
        // 拉丁字母扩展-C/D/E
        '\u{2C60}'..='\u{2C7F}' |
        '\u{A720}'..='\u{A7FF}' |
        '\u{AB30}'..='\u{AB6F}'
    )
}

/// 计算文本的 token 数量
///
/// # 计算规则
/// - 非西文字符：每个计 4.5 个字符单位
/// - 西文字符：每个计 1 个字符单位
/// - 4 个字符单位 = 1 token（四舍五入）
/// ```
pub fn count_tokens(text: &str) -> u64 {
    // println!("text: {}", text);

    let char_units: f64 = text
        .chars()
        .map(|c| if is_non_western_char(c) { 4.0 } else { 1.0 })
        .sum();

    let tokens = char_units / 4.0;

    let acc_token = if tokens < 100.0 {
        tokens * 1.5
    } else if tokens < 200.0 {
        tokens * 1.3
    } else if tokens < 300.0 {
        tokens * 1.25
    } else if tokens < 800.0 {
        tokens * 1.2
    } else {
        tokens * 1.0
    } as u64;

    // println!("tokens: {}, acc_tokens: {}", tokens, acc_token);
    acc_token
}

/// 估算请求的输入 tokens
///
/// 优先调用远程 API，失败时回退到本地计算
pub(crate) fn count_all_tokens(
    model: String,
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    // 检查是否配置了远程 API
    if let Some(config) = get_config() {
        if let Some(api_url) = &config.api_url {
            // 尝试调用远程 API
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(call_remote_count_tokens(
                    api_url, config, model, &system, &messages, &tools,
                ))
            });

            match result {
                Ok(tokens) => {
                    tracing::debug!("远程 count_tokens API 返回: {}", tokens);
                    return tokens;
                }
                Err(e) => {
                    tracing::warn!("远程 count_tokens API 调用失败，回退到本地计算: {}", e);
                }
            }
        }
    }

    // 本地计算
    count_all_tokens_local(system, messages, tools)
}

/// 调用远程 count_tokens API
async fn call_remote_count_tokens(
    api_url: &str,
    config: &CountTokensConfig,
    model: String,
    system: &Option<Vec<SystemMessage>>,
    messages: &Vec<Message>,
    tools: &Option<Vec<Tool>>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let client = build_client(config.proxy.as_ref(), 300, config.tls_backend)?;

    // 构建请求体
    let request = CountTokensRequest {
        model: model, // 模型名称用于 token 计算
        messages: messages.clone(),
        system: system.clone(),
        tools: tools.clone(),
    };

    // 构建请求
    let mut req_builder = client.post(api_url);

    // 设置认证头
    if let Some(api_key) = &config.api_key {
        if config.auth_type == "bearer" {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        } else {
            req_builder = req_builder.header("x-api-key", api_key);
        }
    }

    // 发送请求
    let response = req_builder
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("API 返回错误状态: {}", response.status()).into());
    }

    let result: CountTokensResponse = response.json().await?;
    Ok(result.input_tokens as u64)
}

/// `redacted_thinking` 块的固定计量。输入侧与输出侧共用，避免两边漂移。
const REDACTED_THINKING_TOKENS: u64 = 8;

/// 内容块递归深度上限。
///
/// 畸形或恶意客户端可以构造任意深的嵌套 `tool_result`，递归计数必须有界终止而不是
/// 打爆栈。超过上限的层不再深入（即宁可少算也不崩）；这只影响**估算数值**，不会
/// 丢弃任何实际发送的内容——内容的取舍由 pipeline 负责，不由计数器负责。
const MAX_CONTENT_DEPTH: usize = 16;

/// 取块内某个字符串字段的 token 数。
fn block_text_tokens(block: &serde_json::Value, key: &str) -> u64 {
    block
        .get(key)
        .and_then(|v| v.as_str())
        .map(count_tokens)
        .unwrap_or(0)
}

/// 按序列化后的 JSON 计量结构化字段，与 [`estimate_output_tokens`] 对 `tool_use.input`
/// 的口径一致。
fn json_value_tokens(value: &serde_json::Value) -> u64 {
    count_tokens(&serde_json::to_string(value).unwrap_or_default())
}

/// 图片块按 [`crate::image_resize::estimate_image_tokens`] 计量。
///
/// 这里不另造公式：该函数已对齐 Anthropic 的 `tokens ≈ (w×h)/750` 并带非零保底，
/// 图片按 0 token 计会直接破坏 cache 口径精度。
fn image_block_tokens(block: &serde_json::Value) -> u64 {
    let Some(source) = block.get("source") else {
        return 0;
    };
    let media_type = source
        .get("media_type")
        .and_then(|v| v.as_str())
        .unwrap_or("image/png");
    let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
    crate::image_resize::estimate_image_tokens(media_type, data) as u64
}

/// 递归统计单个内容块。
fn count_content_block(block: &serde_json::Value, depth: usize) -> u64 {
    match block.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "text" => block_text_tokens(block, "text"),
        "thinking" => block_text_tokens(block, "thinking"),
        "redacted_thinking" => REDACTED_THINKING_TOKENS,
        "tool_use" => block.get("input").map(json_value_tokens).unwrap_or(0),
        "tool_result" => block
            .get("content")
            .map(|content| count_content_value(content, depth + 1))
            .unwrap_or(0),
        "image" => image_block_tokens(block),
        // 未知块只数显式 text，不为不透明载荷臆造公式。document 块在 enforce
        // 模式下由 pipeline 直接拒绝（见 pipeline::tests），不会到达线上。
        _ => block_text_tokens(block, "text"),
    }
}

/// 递归统计 content 字段：它可能是裸字符串、块数组，或单个块对象。
fn count_content_value(content: &serde_json::Value, depth: usize) -> u64 {
    if depth > MAX_CONTENT_DEPTH {
        return 0;
    }
    match content {
        serde_json::Value::String(text) => count_tokens(text),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .map(|block| count_content_block(block, depth))
            .sum(),
        serde_json::Value::Object(_) => count_content_block(content, depth),
        _ => 0,
    }
}

/// 本地计算请求的输入 tokens
///
/// 递归遍历整棵内容树。此前只数第一层 `text`，导致 agentic 会话中最大的一块
/// —— `tool_result` 正文 —— 连同 `tool_use.input`、`thinking` 和图片一起被完全
/// 漏计（实测：一个带大段文件内容的完整轮次，与仅一句用户提问计得一样多）。
///
/// 本函数始终是**估算**：它不是、也不会被当作原生 `metadataEvent.tokenUsage`。
/// 上游给出原生用量时以原生为准，缺失字段保持未知而非用估算填补。
fn count_all_tokens_local(
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    let mut total = 0;

    // 系统消息
    if let Some(ref system) = system {
        for msg in system {
            total += count_tokens(&msg.text);
        }
    }

    // 对话消息：整棵内容树
    for msg in &messages {
        total += count_content_value(&msg.content, 0);
    }

    // 工具定义
    if let Some(ref tools) = tools {
        for tool in tools {
            total += count_tokens(&tool.name);
            total += count_tokens(&tool.description);
            let input_schema_json = serde_json::to_string(&tool.input_schema).unwrap_or_default();
            total += count_tokens(&input_schema_json);
        }
    }

    total.max(1)
}

/// 估算输出 tokens
pub(crate) fn estimate_output_tokens(content: &[serde_json::Value]) -> i32 {
    let mut total = 0;

    for block in content {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            total += count_tokens(text) as i32;
        }
        if let Some(thinking) = block.get("thinking").and_then(|v| v.as_str()) {
            total += count_tokens(thinking) as i32;
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("redacted_thinking") {
            total += REDACTED_THINKING_TOKENS as i32;
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            // 工具调用开销
            if let Some(input) = block.get("input") {
                let input_str = serde_json::to_string(input).unwrap_or_default();
                total += count_tokens(&input_str) as i32;
            }
        }
    }

    total.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(content: serde_json::Value) -> Message {
        Message {
            role: "user".to_string(),
            content,
        }
    }

    fn assistant(content: serde_json::Value) -> Message {
        Message {
            role: "assistant".to_string(),
            content,
        }
    }

    /// 同一结构、只有嵌套位置的内容不同时，token 必须随之上升。
    /// 用「空内容版本」做基线，隔离掉结构本身的开销。
    fn nested_must_count(filled: serde_json::Value, empty: serde_json::Value, what: &str) {
        let filled = count_all_tokens_local(None, vec![user(filled)], None);
        let empty = count_all_tokens_local(None, vec![user(empty)], None);
        assert!(
            filled > empty,
            "{what} 必须计入输入 token：填充={filled} 基线={empty}"
        );
    }

    #[test]
    fn counts_tool_result_string_content() {
        let long = "工具输出内容".repeat(200);
        nested_must_count(
            json!([{"type": "tool_result", "tool_use_id": "t1", "content": long}]),
            json!([{"type": "tool_result", "tool_use_id": "t1", "content": ""}]),
            "tool_result 的字符串 content",
        );
    }

    #[test]
    fn counts_tool_result_array_content() {
        let long = "嵌套数组里的工具输出".repeat(200);
        nested_must_count(
            json!([{"type": "tool_result", "tool_use_id": "t1",
                    "content": [{"type": "text", "text": long}]}]),
            json!([{"type": "tool_result", "tool_use_id": "t1",
                    "content": [{"type": "text", "text": ""}]}]),
            "tool_result 的数组 content",
        );
    }

    #[test]
    fn counts_tool_use_input() {
        let long = "x".repeat(4000);
        nested_must_count(
            json!([{"type": "tool_use", "id": "t1", "name": "Bash",
                    "input": {"command": long}}]),
            json!([{"type": "tool_use", "id": "t1", "name": "Bash",
                    "input": {"command": ""}}]),
            "tool_use 的 input",
        );
    }

    #[test]
    fn counts_thinking_blocks() {
        let long = "推理过程".repeat(200);
        nested_must_count(
            json!([{"type": "thinking", "thinking": long, "signature": "s"}]),
            json!([{"type": "thinking", "thinking": "", "signature": "s"}]),
            "thinking 块",
        );
    }

    /// 图片不得按 0 token 计，且必须复用 image_resize 的 Anthropic 口径估算，
    /// 不得在本模块另造一套公式。用保底路径验证委托关系，避免跨模块复制造图辅助。
    #[test]
    fn counts_image_blocks_with_shared_estimator() {
        let data = "not-valid-base64!!!";
        let tokens = count_all_tokens_local(
            None,
            vec![user(json!([{
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": data}
            }]))],
            None,
        );
        let expected = crate::image_resize::estimate_image_tokens("image/png", data) as u64;
        assert!(expected > 0, "共享估算器自身必须保底非零");
        assert!(
            tokens >= expected,
            "图片 token 必须按共享估算器计入：实测={tokens} 期望至少={expected}"
        );
    }

    /// 真实 agentic 轮次里，嵌套内容才是大头；只数第一层 text 会严重低估。
    #[test]
    fn agentic_round_is_dominated_by_nested_content() {
        let bulk = "文件内容行".repeat(2000);
        let messages = vec![
            user(json!("读一下这个文件")),
            assistant(json!([
                {"type": "thinking", "thinking": "先调用工具"},
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "/a"}}
            ])),
            user(json!([{"type": "tool_result", "tool_use_id": "t1", "content": bulk}])),
        ];
        let surface_only = count_all_tokens_local(None, vec![user(json!("读一下这个文件"))], None);
        let full = count_all_tokens_local(None, messages, None);
        assert!(
            full > surface_only * 10,
            "嵌套内容应主导总量：完整={full} 仅首层={surface_only}"
        );
    }

    /// 病态深嵌套不得打爆栈；计数必须有界终止。
    #[test]
    fn deeply_nested_content_terminates() {
        let mut nested = json!([{"type": "text", "text": "底"}]);
        for _ in 0..512 {
            nested = json!([{"type": "tool_result", "tool_use_id": "t", "content": nested}]);
        }
        let tokens = count_all_tokens_local(None, vec![user(nested)], None);
        assert!(tokens >= 1);
    }

    #[test]
    fn estimate_output_tokens_counts_thinking_blocks() {
        let with_thinking = estimate_output_tokens(&[json!({
            "type": "thinking",
            "thinking": "需要计入输出 token"
        })]);
        let text_only = estimate_output_tokens(&[json!({
            "type": "text",
            "text": ""
        })]);

        assert!(with_thinking > text_only);
    }

    #[test]
    fn estimate_output_tokens_counts_redacted_thinking() {
        let tokens = estimate_output_tokens(&[json!({
            "type": "redacted_thinking",
            "data": "encrypted"
        })]);

        assert!(tokens >= 8);
    }
}
