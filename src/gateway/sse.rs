//! 增量 SSE 解析与跨协议流转换。
//!
//! # 解析器的职责边界
//!
//! [`SseParser`] 只负责把字节流切成完整的 SSE 事件：处理跨块的 UTF-8 边界、LF 与 CRLF、
//! 注释行、一个读入块里含多个事件、以及有界的帧长上限。它**不解释**事件内容。
//!
//! # 用量绝不臆造
//!
//! 三种协议的用量口径不同，混淆会直接算错账：
//! - Chat 只在开启 `include_usage` 时于末尾下发一次**最终**用量；
//! - Anthropic 在 `message_start` 给输入侧，在 `message_delta` 给**累计**输出，
//!   两者都是快照而非增量，所以取最后一次、**不能逐条累加**；
//! - Responses 在终结事件里带完整用量。
//!
//! 上游没给就是没给：[`StreamTranslator`] 不会用"已见字符数"之类的估算去补一个数字。
//! 缺失的用量是缺失，不是零。

use anyhow::{Result, bail};
use serde_json::Value;

use super::protocol::WireProtocol;

/// 单个 SSE 帧的字节上限。超过即报错而不是无限缓冲——一个不闭合的流会把内存吃光。
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// 一个完整的 SSE 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// `event:` 字段；缺失时为空串（SSE 默认事件名是 `message`，此处不替调用方补默认值）。
    pub event: String,
    /// `data:` 各行按 SSE 规范以 `\n` 连接。
    pub data: String,
}

/// 增量 SSE 解析器。
#[derive(Default)]
pub struct SseParser {
    /// 尚未构成完整事件的字节。保留原始字节而不是字符串，
    /// 因为一个多字节 UTF-8 字符可能正好被读入块切断。
    buffer: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节，返回本次能够完整解析出的事件。
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>> {
        self.buffer.extend_from_slice(chunk);
        if self.buffer.len() > MAX_FRAME_BYTES {
            bail!(
                "SSE frame exceeds {MAX_FRAME_BYTES} bytes without terminating; refusing to buffer further"
            );
        }
        let mut events = Vec::new();
        // 事件以空行分隔，兼容 LF 与 CRLF。
        while let Some((raw, consumed)) = take_frame(&self.buffer) {
            self.buffer.drain(..consumed);
            if let Some(event) = parse_frame(&raw)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// 流结束时缓冲区仍有残留，说明最后一个事件被截断了。
    ///
    /// 这必须是错误：把半截事件当完整的解析，会让调用方以为拿到了完整响应。
    pub fn finish(&self) -> Result<()> {
        if self.buffer.iter().any(|b| !b.is_ascii_whitespace()) {
            bail!("stream ended mid-event; the response is truncated, not complete");
        }
        Ok(())
    }
}

/// 取出一个以空行结尾的帧，返回 (帧内容, 消耗字节数)。
fn take_frame(buffer: &[u8]) -> Option<(Vec<u8>, usize)> {
    for (index, window) in buffer.windows(2).enumerate() {
        if window == b"\n\n" {
            return Some((buffer[..index].to_vec(), index + 2));
        }
    }
    // CRLFCRLF 需要 4 字节窗口，单独扫一遍。
    for (index, window) in buffer.windows(4).enumerate() {
        if window == b"\r\n\r\n" {
            return Some((buffer[..index].to_vec(), index + 4));
        }
    }
    None
}

fn parse_frame(raw: &[u8]) -> Result<Option<SseEvent>> {
    // 半个 UTF-8 字符不可能出现在完整帧里；真出现说明上游发了非法字节。
    let text =
        std::str::from_utf8(raw).map_err(|_| anyhow::anyhow!("SSE frame is not valid UTF-8"))?;
    let mut event = String::new();
    let mut data: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        // 以冒号开头的是注释（心跳常用），整行忽略。
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => event = value.to_string(),
            "data" => data.push(value),
            // id / retry 等字段与本转换无关，忽略但不报错。
            _ => {}
        }
    }
    if data.is_empty() && event.is_empty() {
        return Ok(None);
    }
    Ok(Some(SseEvent {
        event,
        data: data.join("\n"),
    }))
}

/// 从流中归一出来的用量。**缺失即 `None`**，不补零也不估算。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// 一次流转换的增量产物。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranslatedFrames {
    /// 需要下发给客户端的目标协议事件。
    pub events: Vec<SseEvent>,
    /// 本事件是否是终结事件。
    pub completed: bool,
}

/// 增量流转换器：每次消费一个**完整**的源协议 SSE 事件。
pub struct StreamTranslator {
    from: WireProtocol,
    usage: StreamUsage,
    /// 流里见过的**原始** usage 字段，按出现顺序合并。
    ///
    /// 只留 token 计数是不够的：缓存读写同样按 token 计费，而严格口径下
    /// "字段没出现"不等于 0。把原始对象原样攒下来，就能让流式请求走**和非流式
    /// 完全相同**的证据与定价逻辑，而不是另起一套只认两个字段的简化版——
    /// 那会让每一个流式请求都因证据不全而永远算不出钱。
    usage_payload: serde_json::Map<String, Value>,
    completed: bool,
}

impl StreamTranslator {
    pub fn new(from: WireProtocol) -> Self {
        Self {
            from,
            usage: StreamUsage::default(),
            usage_payload: serde_json::Map::new(),
            completed: false,
        }
    }

    /// 流里见过的原始用量对象，可直接交给 `normalize_usage`。一个字段都没见过时为 `None`
    /// ——没见过就是没见过，不构造一个空对象冒充"上游报了但都是 0"。
    pub fn usage_payload(&self) -> Option<Value> {
        (!self.usage_payload.is_empty()).then(|| Value::Object(self.usage_payload.clone()))
    }

    /// 合并一个 usage 对象。后出现的字段覆盖先出现的——Anthropic 的输入侧在
    /// `message_start`、输出侧在 `message_delta`，两者都是快照。
    fn merge_usage(&mut self, usage: &Value) {
        let Some(object) = usage.as_object() else {
            return;
        };
        for (key, value) in object {
            if value.is_null() {
                continue;
            }
            self.usage_payload.insert(key.clone(), value.clone());
        }
    }

    /// 至此为止从流中读到的用量。上游没给的字段保持 `None`。
    pub fn usage(&self) -> StreamUsage {
        self.usage
    }

    pub fn completed(&self) -> bool {
        self.completed
    }

    /// 消费一个源协议事件，更新用量证据并给出是否终结。
    pub fn consume(&mut self, event: &SseEvent) -> Result<TranslatedFrames> {
        match self.from {
            WireProtocol::ChatCompletions => self.consume_chat(event),
            WireProtocol::Anthropic => self.consume_anthropic(event),
            WireProtocol::Responses => self.consume_responses(event),
        }
    }

    fn consume_chat(&mut self, event: &SseEvent) -> Result<TranslatedFrames> {
        // Chat 用字面量 [DONE] 收尾，它不是 JSON。
        if event.data.trim() == "[DONE]" {
            self.completed = true;
            return Ok(TranslatedFrames {
                events: Vec::new(),
                completed: true,
            });
        }
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|e| anyhow::anyhow!("chat stream frame is not valid JSON: {e}"))?;
        // Chat 的 usage 是最终值而非增量，直接覆盖；上游不发就保持缺失。
        if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
            self.usage.input_tokens = usage.get("prompt_tokens").and_then(Value::as_u64);
            self.usage.output_tokens = usage.get("completion_tokens").and_then(Value::as_u64);
            self.merge_usage(usage);
        }
        Ok(TranslatedFrames {
            events: vec![event.clone()],
            completed: false,
        })
    }

    fn consume_anthropic(&mut self, event: &SseEvent) -> Result<TranslatedFrames> {
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|e| anyhow::anyhow!("anthropic stream frame is not valid JSON: {e}"))?;
        match event.event.as_str() {
            "message_start" => {
                if let Some(usage) = value.pointer("/message/usage") {
                    self.usage.input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
                    // message_start 里的 output_tokens 是起始快照，同样是快照不是增量。
                    self.usage.output_tokens = usage.get("output_tokens").and_then(Value::as_u64);
                    // 缓存读写计数就在这里，非流式路径按它们定价，流式也必须拿到。
                    self.merge_usage(usage);
                }
            }
            "message_delta" => {
                // 这里的 output_tokens 是**累计值**。逐条相加会把总量翻好几倍，
                // 所以取最后一次覆盖，而不是累加。
                if let Some(output) = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                {
                    self.usage.output_tokens = Some(output);
                }
                if let Some(input) = value.pointer("/usage/input_tokens").and_then(Value::as_u64) {
                    self.usage.input_tokens = Some(input);
                }
                if let Some(usage) = value.pointer("/usage") {
                    self.merge_usage(usage);
                }
            }
            "message_stop" => self.completed = true,
            _ => {}
        }
        Ok(TranslatedFrames {
            events: vec![event.clone()],
            completed: self.completed,
        })
    }

    fn consume_responses(&mut self, event: &SseEvent) -> Result<TranslatedFrames> {
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|e| anyhow::anyhow!("responses stream frame is not valid JSON: {e}"))?;
        // 终结事件里带完整用量。
        if let Some(usage) = value.pointer("/response/usage").filter(|u| !u.is_null()) {
            self.usage.input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
            self.usage.output_tokens = usage.get("output_tokens").and_then(Value::as_u64);
            self.merge_usage(usage);
        }
        if matches!(
            event.event.as_str(),
            "response.completed" | "response.incomplete" | "response.failed"
        ) {
            self.completed = true;
        }
        Ok(TranslatedFrames {
            events: vec![event.clone()],
            completed: self.completed,
        })
    }
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
