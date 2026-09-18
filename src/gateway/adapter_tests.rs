use super::protocol::{WireProtocol, convert_request, convert_response};
use serde_json::{Value, json};

fn anthropic_request() -> Value {
    json!({
        "model": "opus5",
        "max_tokens": 32,
        "messages": [{"role": "user", "content": "hello"}]
    })
}

/// brief 里给出的最小用例：模型名换成上游真实模型，文本原样过去。
#[test]
fn a_minimal_anthropic_request_maps_to_chat_completions() {
    let converted = convert_request(
        WireProtocol::Anthropic,
        WireProtocol::ChatCompletions,
        &anthropic_request(),
        "real-model",
    )
    .unwrap();
    assert_eq!(converted["model"], "real-model");
    assert_eq!(converted["messages"][0]["content"], "hello");
    assert_eq!(converted["max_tokens"], 32, "输出上限必须原样传递");
}

/// 同协议转发必须逐字透传：上游认识的合法字段，网关没有资格因为自己不认识就删掉。
#[test]
fn same_protocol_preserves_unknown_but_legitimate_fields() {
    for protocol in [
        WireProtocol::Anthropic,
        WireProtocol::ChatCompletions,
        WireProtocol::Responses,
    ] {
        let body = json!({
            "model": "public-alias",
            "messages": [{"role": "user", "content": "hi"}],
            "some_new_upstream_field": {"nested": [1, 2, 3]},
            "another": true
        });
        let converted =
            convert_request(protocol, protocol, &body, "real-model").unwrap();
        assert_eq!(converted["model"], "real-model", "{protocol:?} 只换模型名");
        assert_eq!(
            converted["some_new_upstream_field"],
            json!({"nested": [1, 2, 3]}),
            "{protocol:?} 未知字段必须保留"
        );
        assert_eq!(converted["another"], true);
    }
}

/// 工具定义、工具调用与工具结果三者要一起对上：
/// tool_use 变 assistant 的 tool_calls，tool_result 变独立的 role:"tool" 消息，
/// 且 id 必须一一对应，否则上游无法配对。
#[test]
fn tools_calls_and_results_keep_their_ids_across_protocols() {
    let body = json!({
        "model": "opus5",
        "max_tokens": 64,
        "tools": [{"name": "read", "description": "read a file",
                   "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}],
        "messages": [
            {"role": "user", "content": "read /a"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call-1", "name": "read", "input": {"path": "/a"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call-1", "content": "file body"}]}
        ]
    });
    let converted = convert_request(
        WireProtocol::Anthropic,
        WireProtocol::ChatCompletions,
        &body,
        "real-model",
    )
    .unwrap();

    assert_eq!(converted["tools"][0]["type"], "function");
    assert_eq!(converted["tools"][0]["function"]["name"], "read");
    assert_eq!(
        converted["tools"][0]["function"]["parameters"]["properties"]["path"]["type"],
        "string",
        "input_schema 必须成为 parameters"
    );

    let messages = converted["messages"].as_array().unwrap();
    let assistant = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(assistant["tool_calls"][0]["id"], "call-1");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "read");
    // 参数在 Chat 协议里是字符串化的 JSON。
    let arguments = assistant["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(arguments).unwrap(),
        json!({"path": "/a"})
    );

    let tool = messages.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(tool["tool_call_id"], "call-1", "结果必须挂回同一个调用 id");
    assert_eq!(tool["content"], "file body");
}

/// base64 与 URL 两种图片都要能过去，且 base64 要变成 data URL。
#[test]
fn images_convert_as_base64_data_urls_or_plain_urls() {
    let body = json!({
        "model": "opus5", "max_tokens": 16,
        "messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}},
            {"type": "image", "source": {"type": "url", "url": "https://example.test/a.png"}},
            {"type": "text", "text": "what is this"}
        ]}]
    });
    let converted = convert_request(
        WireProtocol::Anthropic,
        WireProtocol::ChatCompletions,
        &body,
        "real-model",
    )
    .unwrap();
    let parts = converted["messages"][0]["content"].as_array().unwrap();
    assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,QUJD");
    assert_eq!(parts[1]["image_url"]["url"], "https://example.test/a.png");
    assert_eq!(parts[2]["text"], "what is this");
}

/// 带签名的推理过程跨协议必须**报错**而不是丢弃：签名无法在另一个厂商那里重新生成，
/// 丢掉它请求看起来正常却缺了上下文，表现为"模型突然变笨"。
#[test]
fn signed_reasoning_is_rejected_rather_than_dropped() {
    for block in [
        json!({"type": "thinking", "thinking": "chain", "signature": "sig"}),
        json!({"type": "redacted_thinking", "data": "opaque"}),
    ] {
        let body = json!({
            "model": "opus5", "max_tokens": 16,
            "messages": [{"role": "assistant", "content": [block]}]
        });
        for target in [WireProtocol::ChatCompletions, WireProtocol::Responses] {
            let error = convert_request(WireProtocol::Anthropic, target, &body, "m").unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("provider-signed state"),
                "必须明确拒绝而不是静默丢弃：{message}"
            );
        }
    }
}

/// 厂商内建的服务端工具换个上游就不存在，必须报错。
#[test]
fn provider_built_in_tools_are_rejected() {
    let body = json!({
        "model": "opus5", "max_tokens": 16,
        "tools": [{"type": "web_search_20250305", "name": "web_search"}],
        "messages": [{"role": "user", "content": "hi"}]
    });
    let error = convert_request(
        WireProtocol::Anthropic,
        WireProtocol::ChatCompletions,
        &body,
        "m",
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("built-in tool"));
}

/// 没有实现的跨协议方向必须明确报"不支持"，而不是给一个近似结果。
#[test]
fn unimplemented_conversions_are_refused_not_approximated() {
    let pairs = [
        (WireProtocol::ChatCompletions, WireProtocol::Responses),
        (WireProtocol::Responses, WireProtocol::ChatCompletions),
        (WireProtocol::ChatCompletions, WireProtocol::Anthropic),
    ];
    for (from, to) in pairs {
        let error = convert_request(from, to, &anthropic_request(), "m").unwrap_err();
        assert!(
            format!("{error:#}").contains("unsupported request conversion"),
            "{from:?} -> {to:?} 应明确拒绝"
        );
    }
}

/// 流式请求必须显式索要用量：Chat 协议不开 include_usage 就不会下发 usage，
/// 而解析器不会在缺失时凭空造一个。
#[test]
fn a_streaming_chat_request_asks_for_usage() {
    let mut body = anthropic_request();
    body["stream"] = json!(true);
    let converted = convert_request(
        WireProtocol::Anthropic,
        WireProtocol::ChatCompletions,
        &body,
        "m",
    )
    .unwrap();
    assert_eq!(converted["stream"], true);
    assert_eq!(converted["stream_options"]["include_usage"], true);

    // 非流式不应带 stream_options。
    let converted = convert_request(
        WireProtocol::Anthropic,
        WireProtocol::ChatCompletions,
        &anthropic_request(),
        "m",
    )
    .unwrap();
    assert!(converted.get("stream_options").is_none());
}

/// Anthropic 的 system 独立字段要落到各协议对应的位置。
#[test]
fn system_prompts_land_in_the_right_place() {
    let mut body = anthropic_request();
    body["system"] = json!([{"type": "text", "text": "be brief"}]);

    let chat = convert_request(
        WireProtocol::Anthropic,
        WireProtocol::ChatCompletions,
        &body,
        "m",
    )
    .unwrap();
    assert_eq!(chat["messages"][0]["role"], "system");
    assert_eq!(chat["messages"][0]["content"], "be brief");

    let responses =
        convert_request(WireProtocol::Anthropic, WireProtocol::Responses, &body, "m").unwrap();
    assert_eq!(responses["instructions"], "be brief");
    assert_eq!(
        responses["max_output_tokens"], 32,
        "输出上限换了名字但不得降低"
    );
}

/// Chat 响应转回 Anthropic：文本、工具调用、停止原因、用量都要对上。
#[test]
fn a_chat_response_maps_back_to_anthropic() {
    let body = json!({
        "id": "chatcmpl-1",
        "choices": [{"finish_reason": "tool_calls", "message": {
            "role": "assistant",
            "content": "let me look",
            "tool_calls": [{"id": "call-9", "type": "function",
                            "function": {"name": "read", "arguments": "{\"path\":\"/a\"}"}}]
        }}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7}
    });
    let converted = convert_response(
        WireProtocol::ChatCompletions,
        WireProtocol::Anthropic,
        &body,
        "public-alias",
    )
    .unwrap();

    assert_eq!(converted["model"], "public-alias", "对外换回公开别名");
    assert_eq!(converted["content"][0]["text"], "let me look");
    assert_eq!(converted["content"][1]["type"], "tool_use");
    assert_eq!(converted["content"][1]["id"], "call-9");
    assert_eq!(converted["content"][1]["input"], json!({"path": "/a"}));
    assert_eq!(converted["stop_reason"], "tool_use");
    assert_eq!(converted["usage"]["input_tokens"], 11);
    assert_eq!(converted["usage"]["output_tokens"], 7);
}

/// 工具参数不是合法 JSON 时必须报错，不能退回一个空对象——
/// 那会把一次带参数的调用悄悄变成"无参数调用"。
#[test]
fn malformed_tool_arguments_are_an_error_not_an_empty_object() {
    let body = json!({
        "choices": [{"finish_reason": "tool_calls", "message": {
            "tool_calls": [{"id": "c", "function": {"name": "f", "arguments": "{broken"}}]
        }}]
    });
    let error = convert_response(
        WireProtocol::ChatCompletions,
        WireProtocol::Anthropic,
        &body,
        "m",
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("not valid JSON"));
}

/// Responses 的 reasoning 项带厂商私有状态，转 Anthropic 需要我们造不出的签名。
#[test]
fn responses_reasoning_output_is_rejected() {
    let body = json!({"output": [{"type": "reasoning", "summary": []}]});
    let error = convert_response(
        WireProtocol::Responses,
        WireProtocol::Anthropic,
        &body,
        "m",
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("provider-signed state"));
}

/// Responses 响应的文本与函数调用要能转回来，截断状态要映射成 max_tokens。
#[test]
fn a_responses_payload_maps_back_to_anthropic() {
    let body = json!({
        "id": "resp-1",
        "status": "incomplete",
        "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "partial"}]},
            {"type": "function_call", "call_id": "fc-1", "name": "read", "arguments": "{\"path\":\"/b\"}"}
        ],
        "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    let converted = convert_response(
        WireProtocol::Responses,
        WireProtocol::Anthropic,
        &body,
        "public-alias",
    )
    .unwrap();
    assert_eq!(converted["content"][0]["text"], "partial");
    assert_eq!(converted["content"][1]["id"], "fc-1");
    assert_eq!(converted["content"][1]["input"], json!({"path": "/b"}));
    assert_eq!(converted["stop_reason"], "max_tokens");
    assert_eq!(converted["usage"]["input_tokens"], 5);
}
