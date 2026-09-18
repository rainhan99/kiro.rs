//! 上游协议之间的请求/响应转换。
//!
//! # 与既有 `anthropic::openai` / `anthropic::responses` 的关系
//!
//! 那两个模块做的是**入站**方向：客户端说 OpenAI 协议，归一成内部的 Anthropic 表示。
//! 这里做的是**出站**方向：内部表示要发给一个说别的协议的上游。方向相反，不是重复实现。
//!
//! # 一条贯穿始终的规则：不支持就报错，不静默丢弃
//!
//! 跨协议转换必然遇到目标协议表达不了的东西。把它悄悄删掉，模型会收到一个看起来完整、
//! 实际上缺了关键约束的请求——比如带签名的推理过程被抹掉、输出上限被降低。调用方无从
//! 察觉，症状表现为"模型突然变笨了"。因此**凡是转不过去的一律报错并指明是什么**。
//!
//! 同协议转发保持逐字透传（仅替换模型名）：上游自己认识的合法字段，网关没有资格因为
//! 自己不认识就删掉它。

use anyhow::{Result, bail, ensure};
use serde_json::{Map, Value, json};

/// 上游的线协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireProtocol {
    Anthropic,
    ChatCompletions,
    Responses,
}

impl WireProtocol {
    pub fn path(self) -> &'static str {
        match self {
            Self::Anthropic => "/v1/messages",
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
        }
    }
}

/// 把请求从 `from` 协议转成 `to` 协议，并把模型名替换为上游真实模型。
pub fn convert_request(
    from: WireProtocol,
    to: WireProtocol,
    body: &Value,
    actual_model: &str,
) -> Result<Value> {
    if from == to {
        return Ok(passthrough(body, "model", actual_model));
    }
    match (from, to) {
        (WireProtocol::Anthropic, WireProtocol::ChatCompletions) => {
            anthropic_request_to_chat(body, actual_model)
        }
        (WireProtocol::Anthropic, WireProtocol::Responses) => {
            anthropic_request_to_responses(body, actual_model)
        }
        _ => bail!(
            "unsupported request conversion {from:?} -> {to:?}; no lossless mapping is implemented, \
             and this gateway refuses to approximate one"
        ),
    }
}

/// 把响应从上游协议转回客户端协议，并把模型名换回对外的公开别名。
pub fn convert_response(
    from: WireProtocol,
    to: WireProtocol,
    body: &Value,
    public_model: &str,
) -> Result<Value> {
    if from == to {
        return Ok(passthrough(body, "model", public_model));
    }
    match (from, to) {
        (WireProtocol::ChatCompletions, WireProtocol::Anthropic) => {
            chat_response_to_anthropic(body, public_model)
        }
        (WireProtocol::Responses, WireProtocol::Anthropic) => {
            responses_response_to_anthropic(body, public_model)
        }
        _ => bail!(
            "unsupported response conversion {from:?} -> {to:?}; no lossless mapping is implemented"
        ),
    }
}

/// 同协议透传：原样保留全部字段，只替换模型名。
///
/// 刻意不做字段白名单过滤：上游自己认识的合法字段，网关没有资格因为自己不认识就删掉。
fn passthrough(body: &Value, model_field: &str, model: &str) -> Value {
    let mut value = body.clone();
    if let Some(object) = value.as_object_mut()
        && object.contains_key(model_field)
    {
        object.insert(model_field.into(), Value::String(model.into()));
    }
    value
}

/// 带签名的推理过程与厂商私有状态跨协议时必须报错。
///
/// 签名无法在另一个厂商那里重新生成，私有状态引用（如 `previous_response_id`）在另一个
/// 厂商那里根本不存在。丢掉它们会让请求看起来正常却丢失了上下文，而模型表现为"变笨"。
fn reject_provider_state(block_type: &str) -> Result<()> {
    bail!(
        "`{block_type}` carries provider-signed state that cannot be re-signed by a different \
         upstream; this conversion is refused rather than silently dropping it"
    )
}

// ---------- Anthropic -> Chat Completions ----------

fn anthropic_request_to_chat(body: &Value, actual_model: &str) -> Result<Value> {
    let mut messages = Vec::new();

    // Anthropic 的 system 是独立字段，Chat 里是第一条 system 消息。
    if let Some(system) = body.get("system") {
        let text = anthropic_system_text(system)?;
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    for message in array_field(body, "messages")? {
        let role = str_field(message, "role")?;
        ensure!(
            matches!(role, "user" | "assistant"),
            "unsupported Anthropic message role `{role}`"
        );
        messages.extend(anthropic_message_to_chat(role, message)?);
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(actual_model));
    out.insert("messages".into(), Value::Array(messages));
    // 输出上限必须原样传递，绝不悄悄调小。
    if let Some(max) = body.get("max_tokens") {
        out.insert("max_tokens".into(), max.clone());
    }
    for field in ["temperature", "top_p", "stop_sequences", "stream"] {
        if let Some(value) = body.get(field) {
            let key = if field == "stop_sequences" { "stop" } else { field };
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(tools) = body.get("tools") {
        out.insert("tools".into(), anthropic_tools_to_chat(tools)?);
    }
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        // Chat 协议必须显式索要用量，否则流里根本不会带 usage；
        // 解析器不会在缺失时凭空造一个。
        out.insert("stream_options".into(), json!({"include_usage": true}));
    }
    Ok(Value::Object(out))
}

fn anthropic_system_text(system: &Value) -> Result<String> {
    match system {
        Value::String(text) => Ok(text.clone()),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("text");
                ensure!(
                    block_type == "text",
                    "unsupported system block `{block_type}`"
                );
                parts.push(str_field(block, "text")?.to_string());
            }
            Ok(parts.join("\n"))
        }
        Value::Null => Ok(String::new()),
        _ => bail!("system must be a string or an array of text blocks"),
    }
}

fn anthropic_message_to_chat(role: &str, message: &Value) -> Result<Vec<Value>> {
    let content = message.get("content").unwrap_or(&Value::Null);
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({"role": role, "content": text})]);
    }
    let Some(blocks) = content.as_array() else {
        bail!("message content must be a string or an array of blocks");
    };

    let mut parts = Vec::new();
    let mut tool_calls = Vec::new();
    // tool_result 在 Chat 里是独立的 role:"tool" 消息，必须排在携带 tool_calls 的
    // assistant 消息之后，所以单独收集、最后追加。
    let mut tool_messages = Vec::new();

    for block in blocks {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => parts.push(json!({"type": "text", "text": str_field(block, "text")?})),
            "image" => parts.push(anthropic_image_to_chat(block)?),
            "tool_use" => tool_calls.push(json!({
                "id": str_field(block, "id")?,
                "type": "function",
                "function": {
                    "name": str_field(block, "name")?,
                    "arguments": serde_json::to_string(block.get("input").unwrap_or(&json!({})))?,
                }
            })),
            "tool_result" => tool_messages.push(json!({
                "role": "tool",
                "tool_call_id": str_field(block, "tool_use_id")?,
                "content": tool_result_text(block),
            })),
            other @ ("thinking" | "redacted_thinking") => reject_provider_state(other)?,
            other => bail!("unsupported Anthropic content block `{other}`"),
        }
    }

    let mut out = Vec::new();
    if !parts.is_empty() || !tool_calls.is_empty() {
        let mut message = Map::new();
        message.insert("role".into(), json!(role));
        // 只有文本时用字符串形式，和绝大多数 Chat 实现的惯例一致。
        let only_text = parts.iter().all(|p| p["type"] == "text");
        if parts.is_empty() {
            message.insert("content".into(), Value::Null);
        } else if only_text {
            let joined: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
            message.insert("content".into(), json!(joined.join("\n")));
        } else {
            message.insert("content".into(), Value::Array(parts));
        }
        if !tool_calls.is_empty() {
            message.insert("tool_calls".into(), Value::Array(tool_calls));
        }
        out.push(Value::Object(message));
    }
    out.extend(tool_messages);
    Ok(out)
}

fn anthropic_image_to_chat(block: &Value) -> Result<Value> {
    let source = block
        .get("source")
        .ok_or_else(|| anyhow::anyhow!("image block requires a source"))?;
    let url = match str_field(source, "type")? {
        "base64" => format!(
            "data:{};base64,{}",
            str_field(source, "media_type")?,
            str_field(source, "data")?
        ),
        "url" => str_field(source, "url")?.to_string(),
        other => bail!("unsupported image source type `{other}`"),
    };
    Ok(json!({"type": "image_url", "image_url": {"url": url}}))
}

fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn anthropic_tools_to_chat(tools: &Value) -> Result<Value> {
    let tools = tools
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tools must be an array"))?;
    let mut out = Vec::new();
    for tool in tools {
        // 服务端内建工具（web_search 等）是厂商特有能力，换个上游就不存在了。
        if let Some(kind) = tool.get("type").and_then(Value::as_str)
            && kind != "custom"
        {
            bail!("provider built-in tool `{kind}` has no equivalent on this upstream");
        }
        out.push(json!({
            "type": "function",
            "function": {
                "name": str_field(tool, "name")?,
                "description": tool.get("description").cloned().unwrap_or(json!("")),
                "parameters": tool.get("input_schema").cloned().unwrap_or(json!({"type":"object"})),
            }
        }));
    }
    Ok(Value::Array(out))
}

// ---------- Anthropic -> Responses ----------

fn anthropic_request_to_responses(body: &Value, actual_model: &str) -> Result<Value> {
    let mut input = Vec::new();
    for message in array_field(body, "messages")? {
        let role = str_field(message, "role")?;
        let content = message.get("content").unwrap_or(&Value::Null);
        let text_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        if let Some(text) = content.as_str() {
            input.push(json!({"role": role, "content": [{"type": text_type, "text": text}]}));
            continue;
        }
        let Some(blocks) = content.as_array() else {
            bail!("message content must be a string or an array of blocks");
        };
        let mut parts = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    parts.push(json!({"type": text_type, "text": str_field(block, "text")?}))
                }
                "image" => {
                    let converted = anthropic_image_to_chat(block)?;
                    parts.push(json!({
                        "type": "input_image",
                        "image_url": converted["image_url"]["url"],
                    }));
                }
                other @ ("thinking" | "redacted_thinking") => reject_provider_state(other)?,
                other => bail!(
                    "content block `{other}` has no lossless Responses mapping in this gateway"
                ),
            }
        }
        input.push(json!({"role": role, "content": parts}));
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(actual_model));
    out.insert("input".into(), Value::Array(input));
    if let Some(system) = body.get("system") {
        let text = anthropic_system_text(system)?;
        if !text.is_empty() {
            out.insert("instructions".into(), json!(text));
        }
    }
    // Responses 用 max_output_tokens 表达同一个约束；不得降低。
    if let Some(max) = body.get("max_tokens") {
        out.insert("max_output_tokens".into(), max.clone());
    }
    for field in ["temperature", "top_p", "stream"] {
        if let Some(value) = body.get(field) {
            out.insert(field.into(), value.clone());
        }
    }
    if let Some(tools) = body.get("tools") {
        let chat = anthropic_tools_to_chat(tools)?;
        let responses_tools: Vec<Value> = chat
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool["function"]["name"],
                    "description": tool["function"]["description"],
                    "parameters": tool["function"]["parameters"],
                })
            })
            .collect();
        out.insert("tools".into(), Value::Array(responses_tools));
    }
    Ok(Value::Object(out))
}

// ---------- 响应方向 ----------

fn chat_response_to_anthropic(body: &Value, public_model: &str) -> Result<Value> {
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or_else(|| anyhow::anyhow!("chat response has no choices"))?;
    let message = choice
        .get("message")
        .ok_or_else(|| anyhow::anyhow!("chat choice has no message"))?;

    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        content.push(json!({"type": "text", "text": text}));
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        let arguments = call["function"]["arguments"].as_str().unwrap_or("{}");
        content.push(json!({
            "type": "tool_use",
            "id": call.get("id").cloned().unwrap_or(json!("")),
            "name": call["function"]["name"],
            // 参数是字符串化的 JSON；解析失败不臆造一个空对象，那会把一次工具调用
            // 悄悄变成"无参数调用"。
            "input": serde_json::from_str::<Value>(arguments)
                .map_err(|e| anyhow::anyhow!("tool call arguments are not valid JSON: {e}"))?,
        }));
    }

    let usage = body.get("usage");
    Ok(json!({
        "id": body.get("id").cloned().unwrap_or(json!("")),
        "type": "message",
        "role": "assistant",
        "model": public_model,
        "content": content,
        "stop_reason": chat_finish_reason(choice.get("finish_reason").and_then(Value::as_str)),
        "usage": {
            "input_tokens": usage.and_then(|u| u.get("prompt_tokens")).cloned().unwrap_or(json!(0)),
            "output_tokens": usage.and_then(|u| u.get("completion_tokens")).cloned().unwrap_or(json!(0)),
        }
    }))
}

fn chat_finish_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        Some("stop") => "end_turn",
        _ => "end_turn",
    }
}

fn responses_response_to_anthropic(body: &Value, public_model: &str) -> Result<Value> {
    let mut content = Vec::new();
    for item in body
        .get("output")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "message" => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .unwrap_or(&Vec::new())
                {
                    if part.get("type").and_then(Value::as_str) == Some("output_text")
                        && let Some(text) = part.get("text").and_then(Value::as_str)
                    {
                        content.push(json!({"type": "text", "text": text}));
                    }
                }
            }
            "function_call" => {
                let arguments = item.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                content.push(json!({
                    "type": "tool_use",
                    "id": item.get("call_id").cloned().unwrap_or(json!("")),
                    "name": item.get("name").cloned().unwrap_or(json!("")),
                    "input": serde_json::from_str::<Value>(arguments).map_err(|e| {
                        anyhow::anyhow!("function call arguments are not valid JSON: {e}")
                    })?,
                }));
            }
            // 推理项带厂商私有状态，转成 Anthropic 的 thinking 需要一个我们造不出的签名。
            "reasoning" => reject_provider_state("reasoning")?,
            other => bail!("unsupported Responses output item `{other}`"),
        }
    }
    let usage = body.get("usage");
    Ok(json!({
        "id": body.get("id").cloned().unwrap_or(json!("")),
        "type": "message",
        "role": "assistant",
        "model": public_model,
        "content": content,
        "stop_reason": if body.get("status").and_then(Value::as_str) == Some("incomplete") {
            "max_tokens"
        } else {
            "end_turn"
        },
        "usage": {
            "input_tokens": usage.and_then(|u| u.get("input_tokens")).cloned().unwrap_or(json!(0)),
            "output_tokens": usage.and_then(|u| u.get("output_tokens")).cloned().unwrap_or(json!(0)),
        }
    }))
}

// ---------- 小工具 ----------

fn str_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing or non-string field `{field}`"))
}

fn array_field<'a>(value: &'a Value, field: &str) -> Result<&'a Vec<Value>> {
    value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("missing or non-array field `{field}`"))
}
