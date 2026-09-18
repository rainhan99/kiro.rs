//! 分块处理（chunked map），**代价已声明**。
//!
//! # 这不是无损机制
//!
//! 把一段文本切块分别处理，意味着**没有任何一轮同时看到超过一块**。任何依赖"把两块
//! 原文放在一起看"才能得出的结论，在这个机制下都不可能被得出——这是切块本身的性质，
//! 不是实现质量缺陷，再多测试也不会把它变成"与一次性读全文等价"。
//!
//! 因此：默认关闭；开启后由**模型自己调用**，而不是网关背着模型悄悄套用，模型才不会
//! 在不知情的情况下拿到由碎片推出的结论；配置项、工具描述、返回结果和文档一律照此措辞，
//! 不称其为无损，也不称其为"不降智"。
//!
//! # 什么是无损的
//!
//! 切分本身逐字节无损（复用 [`crate::pipeline::split_lossless`]，切点落在 UTF-8 字符
//! 边界，按序拼接可还原原文）；每块结果都带**字节区间**，答案的每一部分都能追溯到它
//! 所依据的原文；**合并发生在模型自己的上下文里**——各块结果一起返回，由模型自行关联，
//! 所以只有 map 阶段是碎片化的。原文始终留在 artifact 存储里，随时可完整读取。

use anyhow::{Result, ensure};
use serde_json::{Value, json};

use crate::anthropic::types::Tool;

pub const MAP_TOOL: &str = "kiro_context_map";

pub fn is_map_tool(name: &str) -> bool {
    name == MAP_TOOL
}

/// 一块的字节区间（左闭右开）。
pub type ChunkRange = (usize, usize);

/// 切分计划。
///
/// 块数超过上限时**明确报错**而不是静默截断——静默截断会让模型以为它处理了全文。
pub fn plan_chunks(text: &str, chunk_bytes: usize, max_chunks: usize) -> Result<Vec<ChunkRange>> {
    ensure!(chunk_bytes > 0, "chunkBytes must be positive");
    ensure!(!text.is_empty(), "artifact is empty; nothing to map");
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for part in crate::pipeline::split_lossless(text, chunk_bytes) {
        ranges.push((start, start + part.len()));
        start += part.len();
    }
    ensure!(
        ranges.len() <= max_chunks,
        "{} chunks exceed the configured maxChunks {max_chunks}; raise the limit or narrow the artifact instead of silently processing only part of it",
        ranges.len()
    );
    Ok(ranges)
}

/// 工具定义。描述里就写明它不是无损的——模型在决定调用之前就该知道代价。
pub fn map_tool(chunk_bytes: usize, max_chunks: usize) -> Tool {
    let schema = json!({"type":"object","additionalProperties":false,
        "required":["artifact_id","instruction"],
        "properties":{
            "artifact_id":{"type":"string","pattern":"^[0-9a-f]{64}$","description":"artifact_id from a kiro-context reference in this conversation"},
            "instruction":{"type":"string","minLength":1,"maxLength":4096,"description":"What to extract or determine from each chunk independently"}}});
    Tool {
        tool_type: None,
        name: MAP_TOOL.into(),
        description: format!(
            "Apply an instruction to each chunk of a stored context artifact separately and return every per-chunk result tagged with its byte range. NOT LOSSLESS: the text is split into at most {max_chunks} chunks of about {chunk_bytes} bytes and no round sees more than one chunk, so any conclusion that requires relating two chunks' raw text cannot be reached this way. Use it when the text will not fit otherwise; prefer kiro_context_read when it will. Each chunk costs one upstream round on this same model. You combine the results yourself from the ranges returned."
        ),
        input_schema: serde_json::from_value(schema).expect("static chunked map schema"),
        max_uses: None,
        cache_control: None,
    }
}

/// 单块结果。
pub struct ChunkOutput {
    pub range: ChunkRange,
    pub output: String,
}

/// 组装返回给模型的结果。
///
/// 代价声明与数据同在一个对象里，模型读结果时必然读到它，而不是藏在文档某处。
pub fn build_result(artifact_id: &str, total_bytes: usize, chunks: &[ChunkOutput]) -> Value {
    json!({
        "artifact_id": artifact_id,
        "total_bytes": total_bytes,
        "chunk_count": chunks.len(),
        "chunks": chunks.iter().map(|chunk| json!({
            "range": [chunk.range.0, chunk.range.1],
            "output": chunk.output,
        })).collect::<Vec<_>>(),
        "not_lossless": "Each chunk was processed independently; no round saw more than one chunk. A conclusion requiring two chunks' raw text together cannot have been derived here. Ranges are UTF-8 byte offsets into the original artifact, which remains fully readable with kiro_context_read.",
    })
}

/// 每块请求的构造：同模型、无工具、只带这一块原文与指令。
pub fn chunk_request(
    model: &str,
    instruction: &str,
    range: ChunkRange,
    total_bytes: usize,
    chunk_text: &str,
    max_tokens: i32,
) -> crate::anthropic::types::MessagesRequest {
    use crate::anthropic::types::{Message, MessagesRequest, SystemMessage};
    MessagesRequest {
        model: model.to_string(),
        max_tokens,
        messages: vec![Message {
            role: "user".to_string(),
            content: json!(format!(
                "{instruction}\n\n<excerpt bytes=\"{}-{}\" of=\"{total_bytes}\">\n{chunk_text}\n</excerpt>",
                range.0, range.1
            )),
        }],
        stream: false,
        system: Some(vec![SystemMessage {
            text: "You are given one excerpt of a larger document, identified by its byte range. Answer only from this excerpt. If the excerpt alone does not settle the question, say so plainly rather than guessing from context you cannot see.".to_string(),
            cache_control: None,
        }]),
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        cache_control: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_reconstruct_the_original_exactly_on_char_boundaries() {
        let text = "文档内容一行\n".repeat(500);
        let ranges = plan_chunks(&text, 1024, 64).unwrap();
        assert!(ranges.len() > 1);
        let mut rebuilt = String::new();
        let mut previous_end = 0;
        for (start, end) in &ranges {
            assert_eq!(*start, previous_end, "区间必须首尾相接，不留空洞不重叠");
            assert!(text.is_char_boundary(*start) && text.is_char_boundary(*end));
            rebuilt.push_str(&text[*start..*end]);
            previous_end = *end;
        }
        assert_eq!(previous_end, text.len());
        assert_eq!(rebuilt, text, "按序拼接必须逐字节还原原文");
    }

    /// 超过块数上限必须报错。静默只处理一部分会让模型以为它看过全文。
    #[test]
    fn exceeding_the_chunk_limit_is_an_error_not_silent_truncation() {
        let text = "x".repeat(100_000);
        let err = plan_chunks(&text, 1024, 8).unwrap_err();
        assert!(err.to_string().contains("maxChunks"));
        assert!(plan_chunks(&text, 1024, 1024).is_ok());
    }

    #[test]
    fn empty_or_invalid_input_is_rejected() {
        assert!(plan_chunks("", 1024, 8).is_err());
        assert!(plan_chunks("abc", 0, 8).is_err());
    }

    /// 代价声明必须和数据在同一个返回对象里，模型读结果时必然读到。
    #[test]
    fn the_result_states_the_cost_next_to_the_data() {
        let chunks = vec![
            ChunkOutput { range: (0, 10), output: "first".into() },
            ChunkOutput { range: (10, 18), output: "second".into() },
        ];
        let value = build_result(&"a".repeat(64), 18, &chunks);
        assert_eq!(value["chunk_count"], 2);
        assert_eq!(value["chunks"][1]["range"], json!([10, 18]));
        let note = value["not_lossless"].as_str().unwrap();
        assert!(note.contains("no round saw more than one chunk"));
    }

    /// 工具描述本身必须写明不是无损的——模型在决定调用前就该知道。
    #[test]
    fn the_tool_description_declares_it_is_not_lossless() {
        let tool = map_tool(32_768, 8);
        assert!(tool.description.contains("NOT LOSSLESS"));
        assert!(tool.description.contains("no round sees more than one chunk"));
        assert!(
            tool.description.contains("kiro_context_read"),
            "必须指出存在无损替代路径"
        );
    }

    /// 每块请求只带这一块原文，且不带工具——它是一次受限的从属调用。
    #[test]
    fn chunk_request_carries_only_its_own_excerpt() {
        let request = chunk_request("claude-sonnet-4", "列出人名", (10, 20), 100, "仅此一块", 512);
        assert_eq!(request.model, "claude-sonnet-4");
        assert!(request.tools.is_none());
        assert!(!request.stream);
        let content = request.messages[0].content.as_str().unwrap();
        assert!(content.contains("仅此一块"));
        assert!(content.contains("bytes=\"10-20\""));
        assert!(content.contains("列出人名"));
    }
}
