use super::*;

fn frames(parser: &mut SseParser, chunk: &str) -> Vec<SseEvent> {
    parser.push(chunk.as_bytes()).unwrap()
}

/// 一个读入块里含多个事件时要全部切出来。
#[test]
fn multiple_events_in_one_chunk_are_all_parsed() {
    let mut parser = SseParser::new();
    let events = frames(
        &mut parser,
        "event: a\ndata: one\n\nevent: b\ndata: two\n\n",
    );
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, "a");
    assert_eq!(events[0].data, "one");
    assert_eq!(events[1].data, "two");
}

/// LF 与 CRLF 都要支持。
#[test]
fn both_lf_and_crlf_terminate_events() {
    let mut parser = SseParser::new();
    assert_eq!(frames(&mut parser, "data: lf\n\n")[0].data, "lf");
    assert_eq!(frames(&mut parser, "data: crlf\r\n\r\n")[0].data, "crlf");
}

/// 一个多字节 UTF-8 字符被读入块切断时不得报错，要等下一块拼齐。
#[test]
fn a_utf8_character_split_across_chunks_survives() {
    let mut parser = SseParser::new();
    let text = "data: 中文内容\n\n";
    let bytes = text.as_bytes();
    // 在第一个汉字中间切开。
    let split = text.find('中').unwrap() + 1;
    assert!(parser.push(&bytes[..split]).unwrap().is_empty());
    let events = parser.push(&bytes[split..]).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "中文内容");
}

/// 一个事件跨多个读入块时要等齐再产出。
#[test]
fn an_event_split_across_chunks_is_emitted_once_complete() {
    let mut parser = SseParser::new();
    assert!(frames(&mut parser, "event: par").is_empty());
    assert!(frames(&mut parser, "tial\ndata: bo").is_empty());
    let events = frames(&mut parser, "dy\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "partial");
    assert_eq!(events[0].data, "body");
}

/// 注释行（心跳）整行忽略，不产生事件。
#[test]
fn comment_lines_are_ignored() {
    let mut parser = SseParser::new();
    assert!(frames(&mut parser, ": heartbeat\n\n").is_empty());
    let events = frames(&mut parser, ": ping\ndata: real\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "real");
}

/// 多行 data 按 SSE 规范以 \n 连接。
#[test]
fn multi_line_data_joins_with_newlines() {
    let mut parser = SseParser::new();
    let events = frames(&mut parser, "data: first\ndata: second\n\n");
    assert_eq!(events[0].data, "first\nsecond");
}

/// 帧长有上限：不闭合的流不得无限缓冲把内存吃光。
#[test]
fn an_unterminated_frame_is_bounded() {
    let mut parser = SseParser::new();
    let huge = "x".repeat(2 * 1024 * 1024);
    let error = parser.push(huge.as_bytes()).unwrap_err();
    assert!(format!("{error:#}").contains("refusing to buffer further"));
}

/// 流在事件中途结束是**截断**，必须报错——把半截当完整会让调用方以为响应完整。
#[test]
fn a_truncated_stream_is_an_error_not_a_complete_response() {
    let mut parser = SseParser::new();
    assert!(frames(&mut parser, "data: half").is_empty());
    assert!(parser.finish().is_err());

    let mut clean = SseParser::new();
    frames(&mut clean, "data: whole\n\n");
    assert!(clean.finish().is_ok(), "干净收尾不应报错");
}

/// Chat 的 usage 是最终值：覆盖而非累加；[DONE] 是终结标记且不是 JSON。
#[test]
fn chat_usage_is_a_final_value_and_done_terminates() {
    let mut translator = StreamTranslator::new(WireProtocol::ChatCompletions);
    translator
        .consume(&SseEvent {
            event: String::new(),
            data: r#"{"choices":[{"delta":{"content":"hi"}}]}"#.into(),
        })
        .unwrap();
    assert_eq!(
        translator.usage(),
        StreamUsage::default(),
        "上游未给则保持缺失"
    );

    translator
        .consume(&SseEvent {
            event: String::new(),
            data: r#"{"usage":{"prompt_tokens":10,"completion_tokens":4}}"#.into(),
        })
        .unwrap();
    assert_eq!(translator.usage().input_tokens, Some(10));
    assert_eq!(translator.usage().output_tokens, Some(4));

    let done = translator
        .consume(&SseEvent {
            event: String::new(),
            data: "[DONE]".into(),
        })
        .unwrap();
    assert!(done.completed);
    assert!(translator.completed());
}

/// Anthropic 的 message_delta 里 output_tokens 是**累计值**。
/// 逐条相加会把总量翻好几倍，必须取最后一次覆盖。
#[test]
fn anthropic_cumulative_output_is_not_added_twice() {
    let mut translator = StreamTranslator::new(WireProtocol::Anthropic);
    translator
        .consume(&SseEvent {
            event: "message_start".into(),
            data: r#"{"message":{"usage":{"input_tokens":100,"output_tokens":1}}}"#.into(),
        })
        .unwrap();
    for cumulative in [5_u64, 12, 30] {
        translator
            .consume(&SseEvent {
                event: "message_delta".into(),
                data: format!(r#"{{"usage":{{"output_tokens":{cumulative}}}}}"#),
            })
            .unwrap();
    }
    assert_eq!(
        translator.usage().output_tokens,
        Some(30),
        "应为最后一次累计值 30，而不是 1+5+12+30=48"
    );
    assert_eq!(translator.usage().input_tokens, Some(100));

    let stop = translator
        .consume(&SseEvent {
            event: "message_stop".into(),
            data: "{}".into(),
        })
        .unwrap();
    assert!(stop.completed);
}

/// Responses 的终结事件带完整用量。
#[test]
fn responses_terminal_event_carries_usage() {
    for terminal in [
        "response.completed",
        "response.incomplete",
        "response.failed",
    ] {
        let mut translator = StreamTranslator::new(WireProtocol::Responses);
        let frame = translator
            .consume(&SseEvent {
                event: terminal.into(),
                data: r#"{"response":{"usage":{"input_tokens":7,"output_tokens":2}}}"#.into(),
            })
            .unwrap();
        assert!(frame.completed, "{terminal} 应判为终结");
        assert_eq!(translator.usage().input_tokens, Some(7));
        assert_eq!(translator.usage().output_tokens, Some(2));
    }
}

/// 上游全程不给用量时，转换器**不得**凭空造一个。缺失就是缺失，不是零。
#[test]
fn a_stream_without_usage_reports_missing_not_zero() {
    let mut translator = StreamTranslator::new(WireProtocol::ChatCompletions);
    for _ in 0..3 {
        translator
            .consume(&SseEvent {
                event: String::new(),
                data: r#"{"choices":[{"delta":{"content":"x"}}]}"#.into(),
            })
            .unwrap();
    }
    translator
        .consume(&SseEvent {
            event: String::new(),
            data: "[DONE]".into(),
        })
        .unwrap();
    assert_eq!(
        translator.usage(),
        StreamUsage {
            input_tokens: None,
            output_tokens: None
        },
        "没有用量就报缺失，绝不按已见内容估算补一个"
    );
}

/// 非法 JSON 的帧必须报错，不能当作空事件跳过。
#[test]
fn a_malformed_frame_is_an_error() {
    let mut translator = StreamTranslator::new(WireProtocol::ChatCompletions);
    let error = translator
        .consume(&SseEvent {
            event: String::new(),
            data: "{not json".into(),
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("not valid JSON"));
}

/// 缓存计数必须原样留下：流式与非流式走同一套定价，只留 token 数会让每个流式
/// 请求都因证据不全而永远算不出钱。
#[test]
fn the_raw_usage_object_survives_the_stream_for_pricing() {
    let mut translator = StreamTranslator::new(WireProtocol::Anthropic);
    assert_eq!(translator.usage_payload(), None, "什么都没见过就是 None");

    translator
        .consume(&SseEvent {
            event: "message_start".into(),
            data: serde_json::json!({
                "message": {"usage": {
                    "input_tokens": 1_000,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 40,
                    "cache_creation_input_tokens": 12
                }}
            })
            .to_string(),
        })
        .unwrap();
    translator
        .consume(&SseEvent {
            event: "message_delta".into(),
            data: serde_json::json!({"usage": {"output_tokens": 500}}).to_string(),
        })
        .unwrap();

    let payload = translator.usage_payload().expect("应有用量");
    // 输入侧与缓存计数来自 message_start，输出侧被 message_delta 覆盖。
    assert_eq!(payload["input_tokens"], 1_000);
    assert_eq!(payload["output_tokens"], 500, "累计值取最后一次，不是相加");
    assert_eq!(payload["cache_read_input_tokens"], 40);
    assert_eq!(payload["cache_creation_input_tokens"], 12);
}

/// 上游一个用量字段都没发时保持 `None`——不构造一个空对象冒充"报了但都是 0"。
#[test]
fn a_stream_without_usage_reports_none_rather_than_an_empty_object() {
    let mut translator = StreamTranslator::new(WireProtocol::Anthropic);
    translator
        .consume(&SseEvent {
            event: "content_block_delta".into(),
            data: serde_json::json!({"delta": {"text": "hi"}}).to_string(),
        })
        .unwrap();
    assert_eq!(translator.usage_payload(), None);
}

/// Chat 的最终用量对象整体留下，包括缓存明细。
#[test]
fn a_chat_stream_retains_its_final_usage_object_whole() {
    let mut translator = StreamTranslator::new(WireProtocol::ChatCompletions);
    translator
        .consume(&SseEvent {
            event: String::new(),
            data: serde_json::json!({
                "usage": {
                    "prompt_tokens": 800,
                    "completion_tokens": 200,
                    "prompt_tokens_details": {"cached_tokens": 64, "cache_write_tokens": 0}
                }
            })
            .to_string(),
        })
        .unwrap();

    let payload = translator.usage_payload().unwrap();
    assert_eq!(payload["prompt_tokens"], 800);
    assert_eq!(payload["prompt_tokens_details"]["cached_tokens"], 64);
}
