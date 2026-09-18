use super::*;
use serde_json::json;

#[test]
fn strict_configuration_rejects_typos_and_zero_limits() {
    assert!(
        serde_json::from_value::<config::PipelineConfig>(json!({"cacheStratgey":"static-prefix"}))
            .is_err()
    );
    let mut config = config::PipelineConfig::default();
    config.limits.text_field_bytes = Some(0);
    assert!(config.validate().is_err());
    assert!(!config::PipelineConfig::default().allow_simulated_cache);
}

#[test]
fn billing_cleanup_is_exact_and_idempotent() {
    let source = "x-anthropic-billing-header: nonce=one\r\nKeep every byte\n";
    assert_eq!(strip_billing_line(source), "Keep every byte\n");
    assert_eq!(
        strip_billing_line(strip_billing_line(source)),
        "Keep every byte\n"
    );
    for literal in [
        "prefix x-anthropic-billing-header: keep",
        "text\nx-anthropic-billing-header: keep",
        " x-anthropic-billing-header: keep",
    ] {
        assert_eq!(strip_billing_line(literal), literal);
    }
}

#[test]
fn recursive_metrics_measure_utf8_tool_results_and_base64_separately() {
    let body = json!({"conversationState":{"currentMessage":{"userInputMessage":{
        "content":"中\n\"", "images":[{"format":"png","source":{"bytes":"YWJjZA=="}}],
        "userInputMessageContext":{"toolResults":[{"toolUseId":"t","content":[{"text":"結果"}]}]}
    }}}})
    .to_string();
    let metrics = measure_wire(&body).unwrap();
    assert_eq!(metrics.body_bytes, body.len());
    assert_eq!(metrics.image_base64_bytes, 8);
    assert_eq!(metrics.largest_image_base64_bytes, 8);
    assert!(metrics.largest_text_bytes >= "結果".len());
    assert!(metrics.largest_tool_result_bytes > "結果".len());
}

#[test]
fn configured_local_boundary_is_not_an_upstream_claim() {
    let mut cfg = config::PipelineConfig::default();
    cfg.limits.text_field_bytes = Some(4);
    let p = RequestPipeline::new(cfg.clone());
    assert!(p.preflight(r#"{"content":"1234"}"#).is_ok());
    let err = p.preflight(r#"{"content":"12345"}"#).unwrap_err();
    assert!(err.to_string().contains("local_payload_limit"));
    cfg.mode = config::PipelineMode::Audit;
    assert!(
        RequestPipeline::new(cfg)
            .preflight(r#"{"content":"12345"}"#)
            .is_ok()
    );
}

#[test]
fn fingerprints_ignore_headers_but_separate_wire_from_static_prefix() {
    let p = RequestPipeline::new(config::PipelineConfig::default());
    let mut body = json!({"profileArn":"secret-profile", "conversationState":{"conversationId":"a", "agentTaskType":"vibe", "history":[{"userInputMessage":{"content":"static","cachePoint":{"type":"default"}}}], "currentMessage":{"userInputMessage":{"content":"one","modelId":"m"}}}});
    let a = p
        .audit(&body.to_string(), "ide", 1, &http::HeaderMap::new(), None)
        .unwrap();
    body["conversationState"]["currentMessage"]["userInputMessage"]["content"] = json!("two");
    let b = p
        .audit(&body.to_string(), "ide", 1, &http::HeaderMap::new(), None)
        .unwrap();
    assert_ne!(a["wireFingerprint"], b["wireFingerprint"]);
    assert_eq!(a["staticPrefixFingerprint"], b["staticPrefixFingerprint"]);
    let mut headers = http::HeaderMap::new();
    headers.insert("amz-sdk-invocation-id", "changed".parse().unwrap());
    headers.insert("authorization", "Bearer SECRET".parse().unwrap());
    let c = p
        .audit(&body.to_string(), "ide", 2, &headers, None)
        .unwrap();
    assert_eq!(b["wireFingerprint"], c["wireFingerprint"]);
    assert!(!c.to_string().contains("SECRET"));
    assert!(!c.to_string().contains("secret-profile"));
    assert_eq!(b["scopeFingerprint"], c["scopeFingerprint"]); // F: same scope, account differs; not cache proof.
    for (pointer, changed) in [
        ("/profileArn", "other-profile"),
        ("/conversationState/agentTaskType", "spec"),
        (
            "/conversationState/currentMessage/userInputMessage/modelId",
            "another-model",
        ),
    ] {
        let mut next = body.clone();
        *next.pointer_mut(pointer).unwrap() = json!(changed);
        let audit = p
            .audit(&next.to_string(), "ide", 2, &headers, None)
            .unwrap();
        assert_ne!(audit["scopeFingerprint"], c["scopeFingerprint"]); // G diagnostic partition only.
    }
}

fn request_fixture() -> MessagesRequest {
    serde_json::from_value(json!({"model":"claude-sonnet-4", "system":"x-anthropic-billing-header: nonce=one\nStatic instructions", "messages":[{"role":"user","content":"Question one"}], "metadata":{"user_id":"user_test_session_6a4d25ce-039d-4fd5-8712-3645c3d387f7"}})).unwrap()
}

fn fixture_wire(pipeline: &RequestPipeline, payload: &mut MessagesRequest) -> String {
    pipeline.prepare(payload, 1).unwrap();
    let converted = crate::anthropic::converter::convert_request_with_pipeline(
        payload,
        crate::model::config::ToolCompatibilityMode::Raw,
        &pipeline.config,
    )
    .unwrap();
    serialize_request(
        payload,
        &KiroRequest {
            conversation_state: converted.conversation_state,
            profile_arn: None,
            additional_model_request_fields: converted.additional_model_request_fields,
        },
        &pipeline.config,
    )
    .unwrap()
}

#[test]
fn offline_abce_prove_construction_not_cache_hits() {
    let mut cfg = config::PipelineConfig::default();
    cfg.cache_strategy = config::CacheStrategy::StaticPrefix;
    let p = RequestPipeline::new(cfg);
    let a = fixture_wire(&p, &mut request_fixture());
    assert_eq!(a, fixture_wire(&p, &mut request_fixture())); // B exactly equal with fixed session.
    let mut e = request_fixture();
    e.system.as_mut().unwrap()[0].text =
        "x-anthropic-billing-header: nonce=two\nStatic instructions".into();
    assert_eq!(a, fixture_wire(&p, &mut e)); // E only known billing nonce differs.
    let mut c = request_fixture();
    c.messages.extend([
        crate::anthropic::types::Message {
            role: "assistant".into(),
            content: json!("Answer one"),
        },
        crate::anthropic::types::Message {
            role: "user".into(),
            content: json!("Question two"),
        },
    ]);
    let c = fixture_wire(&p, &mut c);
    let aa = p
        .audit(&a, "ide", 1, &http::HeaderMap::new(), None)
        .unwrap();
    let cc = p
        .audit(&c, "ide", 1, &http::HeaderMap::new(), None)
        .unwrap();
    assert_eq!(aa["staticPrefixFingerprint"], cc["staticPrefixFingerprint"]);
    assert_eq!(aa["metrics"]["cachePointCount"], 1);
    assert_eq!(aa["cacheHitProven"], false);
    assert!(
        serde_json::from_str::<Value>(&a)
            .unwrap()
            .pointer("/conversationState/currentMessage/userInputMessage/cachePoint")
            .is_none()
    );
}

#[test]
fn off_and_audit_leave_billing_and_cache_unmodified() {
    for mode in [config::PipelineMode::Off, config::PipelineMode::Audit] {
        let mut cfg = config::PipelineConfig::default();
        cfg.mode = mode;
        cfg.cache_strategy = config::CacheStrategy::StaticPrefix;
        let wire = fixture_wire(&RequestPipeline::new(cfg), &mut request_fixture());
        assert!(wire.contains("nonce=one"));
        assert!(!wire.contains("cachePoint"));
    }
}

#[test]
fn request_budgets_and_long_tool_description_are_not_silently_reduced() {
    let mut payload = request_fixture();
    payload.thinking =
        Some(serde_json::from_value(json!({"type":"enabled","budget_tokens":32000})).unwrap());
    payload.tools = Some(vec![serde_json::from_value(json!({"name":"custom_tool","description":"詳".repeat(12000),"input_schema":{"type":"object"}})).unwrap()]);
    let wire = fixture_wire(
        &RequestPipeline::new(config::PipelineConfig::default()),
        &mut payload,
    );
    assert!(wire.contains("<max_thinking_length>32000</max_thinking_length>"));
    let wire: Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(wire.pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools/0/toolSpecification/description").unwrap().as_str().unwrap().chars().count(), 12000);
}

#[test]
fn each_local_wire_limit_has_independent_boundary_tests() {
    // Boundary testing is local only; it never binary-searches an upstream.
    let wire = json!({"conversationState":{"currentMessage":{"userInputMessage":{"content":"1234567890","images":[{"source":{"bytes":"YWJjZA=="}}],"userInputMessageContext":{"toolResults":[{"content":[{"text":"abc"}]}]}}}}}).to_string();
    let measured = measure_wire(&wire).unwrap();
    let boundaries = [
        measured.body_bytes,
        measured.largest_text_bytes,
        measured.largest_tool_result_bytes,
        measured.largest_image_base64_bytes,
    ];
    for (index, boundary) in boundaries.into_iter().enumerate() {
        let mut cfg = config::PipelineConfig::default();
        let slot = match index {
            0 => &mut cfg.limits.body_bytes,
            1 => &mut cfg.limits.text_field_bytes,
            2 => &mut cfg.limits.tool_result_bytes,
            _ => &mut cfg.limits.image_base64_bytes,
        };
        *slot = Some(boundary);
        assert!(RequestPipeline::new(cfg.clone()).preflight(&wire).is_ok());
        let slot = match index {
            0 => &mut cfg.limits.body_bytes,
            1 => &mut cfg.limits.text_field_bytes,
            2 => &mut cfg.limits.tool_result_bytes,
            _ => &mut cfg.limits.image_base64_bytes,
        };
        *slot = Some(boundary - 1);
        assert!(RequestPipeline::new(cfg).preflight(&wire).is_err());
    }
}

#[test]
fn later_billing_literal_and_budget_survive_sanitizer_boundaries() {
    let p = RequestPipeline::new(config::PipelineConfig::default());
    let mut payload = request_fixture();
    payload.system = Some(vec![
        crate::anthropic::types::SystemMessage {
            text: "Explain this header:".into(),
            cache_control: None,
        },
        crate::anthropic::types::SystemMessage {
            text: "x-anthropic-billing-header: keep this literal".into(),
            cache_control: None,
        },
    ]);
    p.prepare(&mut payload, 1).unwrap();
    assert!(
        payload.system.as_ref().unwrap()[1]
            .text
            .contains("keep this literal")
    );
    payload.system = Some(vec![crate::anthropic::types::SystemMessage {
        text: "x-anthropic-billing-header: nonce=one".into(),
        cache_control: None,
    }]);
    payload.thinking =
        Some(serde_json::from_value(json!({"type":"enabled","budget_tokens":32000})).unwrap());
    let wire = fixture_wire(&p, &mut payload);
    assert!(wire.contains("<max_thinking_length>32000</max_thinking_length>"));
}

#[test]
fn offline_and_live_share_normalization_and_reject_unsupported_inputs() {
    let p = RequestPipeline::new(config::PipelineConfig::default());
    let mut request = request_fixture();
    request.model = "claude-sonnet-4-thinking".into();
    p.prepare(&mut request, 1).unwrap();
    assert!(request.thinking.is_some());
    request.max_tokens = 0;
    assert!(p.prepare(&mut request, 1).is_err());
    request.max_tokens = 1000;
    request.messages[0].content =
        json!([{"type":"image","source":{"type":"url","url":"https://invalid.example/image.png"}}]);
    assert!(p.prepare(&mut request, 1).is_err());
    request.messages[0].content = json!([{"type":"document","source":{"data":"must not vanish"}}]);
    assert!(p.prepare(&mut request, 1).is_err());
}

#[test]
fn total_body_limit_waits_for_final_endpoint_shape() {
    let mut cfg = config::PipelineConfig::default();
    cfg.limits.body_bytes = Some(15);
    let p = RequestPipeline::new(cfg);
    assert!(
        p.preflight_before_endpoint(r#"{"agentContinuationId":"removed-by-cli"}"#)
            .is_ok()
    );
    assert!(p.preflight(r#"{}"#).is_ok());
    assert!(p.preflight(r#"{"content":"actually too long"}"#).is_err());
}

#[test]
fn arbitrary_tool_keys_cannot_impersonate_wire_fields() {
    let wire = json!({"conversationState":{"history":[{"assistantResponseMessage":{"toolUses":[{"input":{"images":{"bytes":"x".repeat(1000)},"cachePoint":{"type":"default"},"toolResults":[{"content":"not a result"}]}}]}}]}}).to_string();
    let metrics = measure_wire(&wire).unwrap();
    assert_eq!(metrics.largest_text_bytes, 1000);
    assert_eq!(metrics.image_count, 0);
    assert_eq!(metrics.tool_result_count, 0);
    assert_eq!(metrics.cache_point_count, 0);
    let mut cfg = config::PipelineConfig::default();
    cfg.limits.text_field_bytes = Some(999);
    assert!(RequestPipeline::new(cfg).preflight(&wire).is_err());
}

#[test]
fn inactive_tile_budget_does_not_prevent_small_ingress() {
    let mut cfg = config::PipelineConfig::default();
    cfg.ingress_max_bytes = 65536;
    assert!(cfg.validate().is_ok());
    cfg.images.strategy = config::ImageStrategy::LosslessTiles;
    assert!(cfg.validate().is_err());
}

#[test]
fn offline_cli_uses_actual_endpoint_removals_before_total_budget() {
    let mut config = crate::model::config::Config::default();
    config.default_endpoint = "cli".into();
    let p = RequestPipeline::new(config.request_pipeline.clone());
    let body = fixture_wire(&p, &mut request_fixture());
    let final_body = super::inspect::endpoint_wire(&body, &config);
    assert!(final_body.len() < body.len());
    config.request_pipeline.limits.body_bytes = Some(final_body.len());
    let p = RequestPipeline::new(config.request_pipeline);
    assert!(p.preflight_before_endpoint(&body).is_ok());
    assert!(p.preflight(&body).is_err());
    assert!(p.preflight(&final_body).is_ok());
}

#[test]
fn gateway_search_and_opaque_thinking_responses_can_be_replayed_without_data_loss() {
    let p = RequestPipeline::new(config::PipelineConfig::default());
    let mut request = request_fixture();
    let search = json!({"type":"server_tool_use","id":"search-1","name":"web_search","input":{"query":"exact query"}});
    let result = json!({"type":"web_search_tool_result","tool_use_id":"search-1","content":[{"type":"web_search_result","url":"https://example.invalid/source","title":"Source","encrypted_content":"opaque source data"}]});
    let redacted = json!({"type":"redacted_thinking","data":"opaque thinking data"});
    request.messages.extend([
        crate::anthropic::types::Message {
            role: "assistant".into(),
            content: json!([search,result,redacted,{"type":"text","text":"Prior answer"}]),
        },
        crate::anthropic::types::Message {
            role: "user".into(),
            content: json!("Continue"),
        },
    ]);
    p.prepare(&mut request, 1).unwrap();
    for (index, original) in [search, result, redacted].into_iter().enumerate() {
        let quoted = request.messages[1].content[index]["text"]
            .as_str()
            .unwrap()
            .split_once('\n')
            .unwrap()
            .1;
        assert_eq!(serde_json::from_str::<Value>(quoted).unwrap(), original);
    }
    let first = request.messages[1].content.clone();
    p.prepare(&mut request, 1).unwrap();
    assert_eq!(first, request.messages[1].content);
    let wire = fixture_wire(&p, &mut request);
    assert!(wire.contains("opaque source data"));
    assert!(wire.contains("opaque thinking data"));
}

#[test]
fn invalid_tool_pairs_are_rejected_instead_of_removed() {
    let p = RequestPipeline::new(config::PipelineConfig::default());
    let mut request = request_fixture();
    request.messages[0].content =
        json!([{"type":"tool_result","tool_use_id":"orphan","content":"cannot be dropped"}]);
    assert!(
        p.prepare(&mut request, 1)
            .err()
            .unwrap()
            .to_string()
            .contains("orphan")
    );
    request.messages = vec![
        crate::anthropic::types::Message {
            role: "assistant".into(),
            content: json!([{"type":"tool_use","id":"missing","name":"custom","input":{}}]),
        },
        crate::anthropic::types::Message {
            role: "user".into(),
            content: json!("continue"),
        },
    ];
    assert!(p.prepare(&mut request, 1).is_err());
}

#[test]
fn legacy_gateway_search_result_without_id_is_preserved_but_ambiguity_fails() {
    let p = RequestPipeline::new(config::PipelineConfig::default());
    let mut request = request_fixture();
    request.messages.extend([
        crate::anthropic::types::Message {role:"assistant".into(),content:json!([
            {"type":"server_tool_use","id":"search1","name":"web_search","input":{"query":"q"}},
            {"type":"web_search_tool_result","content":[{"type":"web_search_result","title":"retained source","url":"https://example.invalid"}]}
        ])},
        crate::anthropic::types::Message {role:"user".into(),content:json!("next")}
    ]);
    let original = request.messages[1].content.clone();
    p.prepare(&mut request, 1).unwrap();
    assert!(
        request.messages[1].content[1]["text"]
            .as_str()
            .unwrap()
            .contains("retained source")
    );
    request.messages[1].content = original;
    request.messages[1].content.as_array_mut().unwrap().insert(
        1,
        json!({"type":"server_tool_use","id":"search2","name":"web_search","input":{"query":"q2"}}),
    );
    assert!(p.prepare(&mut request, 1).is_err());
}

/// 线上 token 分项必须互不重叠：tools / toolResults / images 嵌在 current 或 history
/// 之下，若不显式改判归属就会被重复计入父分项，总量对不上各项之和。
#[test]
fn wire_tokens_are_attributed_to_disjoint_sections() {
    let wire = json!({"conversationState":{
        "history":[
            {"userInputMessage":{"content":"历史提问内容"}},
            {"assistantResponseMessage":{"content":"历史回答内容"}}
        ],
        "currentMessage":{"userInputMessage":{
            "content":"当前这一轮的提问",
            "modelId":"claude-sonnet-4",
            "images":[{"format":"png","source":{"bytes":"bm90LXZhbGlkLWJhc2U2NA=="}}],
            "userInputMessageContext":{
                "tools":[{"toolSpecification":{"name":"Read","description":"读取文件内容"}}],
                "toolResults":[{"toolUseId":"t","content":[{"text":"工具返回的大段正文"}]}]
            }
        }}
    }})
    .to_string();

    let t = measure_wire_tokens(&wire).unwrap();
    assert!(t.current > 0, "当前轮文本应计入");
    assert!(t.history > 0, "历史应计入");
    assert!(t.tools > 0, "工具声明应计入");
    assert!(t.tool_results > 0, "工具结果应计入");
    assert!(t.images > 0, "图片应计入");
    assert_eq!(
        t.total,
        t.current + t.history + t.tools + t.tool_results + t.images + t.other,
        "分项必须互不重叠，总量等于各项之和"
    );
}

/// 图片按共享的 (w×h)/750 口径计，不得把 base64 串当文本数——后者会离谱高估。
#[test]
fn image_tokens_use_shared_estimator_not_base64_text_length() {
    let data = "A".repeat(20_000);
    let wire = json!({"conversationState":{"currentMessage":{"userInputMessage":{
        "content":"",
        "images":[{"format":"png","source":{"bytes":data}}]
    }}}})
    .to_string();

    let t = measure_wire_tokens(&wire).unwrap();
    let as_text = crate::token::count_tokens(&"A".repeat(20_000));
    assert_eq!(
        t.images,
        crate::image_resize::estimate_image_tokens("image/png", &"A".repeat(20_000)) as u64
    );
    assert!(
        t.images < as_text,
        "把 base64 当文本数会高估：估算器={} 文本口径={}",
        t.images,
        as_text
    );
}

/// 模型上限未知时必须如实报未知，不得猜测、不得从拒绝反推。
#[test]
fn unknown_model_ceiling_is_reported_as_unknown() {
    let p = RequestPipeline::new(config::PipelineConfig::default());
    let wire = json!({"conversationState":{"currentMessage":{"userInputMessage":{
        "content":"问题","modelId":"某个没有缓存上限的模型"
    }}}});
    let audit = p
        .audit(&wire.to_string(), "ide", 1, &http::HeaderMap::new(), None)
        .unwrap();
    assert_eq!(
        audit["tokenMetrics"]["maxInputTokens"],
        serde_json::Value::Null
    );
    assert_eq!(audit["tokenMetrics"]["headroom"], serde_json::Value::Null);
    assert!(audit["tokenMetrics"]["total"].as_u64().unwrap() > 0);
}

/// 已知上限时给出余量；余量可为负（已超出），不夹到 0，否则会掩盖越界程度。
#[test]
fn known_ceiling_yields_headroom_that_may_go_negative() {
    let p = RequestPipeline::new(config::PipelineConfig::default());
    let wire = json!({"conversationState":{"currentMessage":{"userInputMessage":{
        "content":"问题正文".repeat(50)
    }}}});
    let audit = p
        .audit(
            &wire.to_string(),
            "ide",
            1,
            &http::HeaderMap::new(),
            Some(1),
        )
        .unwrap();
    let total = audit["tokenMetrics"]["total"].as_i64().unwrap();
    assert_eq!(audit["tokenMetrics"]["maxInputTokens"], 1);
    assert_eq!(audit["tokenMetrics"]["headroom"], 1 - total);
    assert!(audit["tokenMetrics"]["headroom"].as_i64().unwrap() < 0);
}

/// 字节与 token 是两个口径，必须并列且各自标注，任何一方都不得替代另一方。
#[test]
fn byte_and_token_dimensions_stay_separately_labelled() {
    let p = RequestPipeline::new(config::PipelineConfig::default());
    let wire =
        json!({"conversationState":{"currentMessage":{"userInputMessage":{"content":"问题"}}}});
    let audit = p
        .audit(&wire.to_string(), "ide", 1, &http::HeaderMap::new(), None)
        .unwrap();
    assert!(
        audit["metrics"]["bodyBytes"].as_u64().unwrap() > 0,
        "字节口径仍在"
    );
    assert!(
        audit["tokenMetrics"]["total"].as_u64().unwrap() > 0,
        "token 口径并列"
    );
    assert_eq!(
        audit["tokenMetrics"]["source"], "estimate",
        "token 分项是估算，必须自带标注，不得被读成原生用量"
    );
}

fn admission_wire(repeat: usize) -> String {
    json!({"conversationState":{"currentMessage":{"userInputMessage":{
        "content": "问题正文".repeat(repeat), "modelId": "claude-sonnet-4"
    }}}})
    .to_string()
}

fn admitting_pipeline() -> RequestPipeline {
    let mut cfg = config::PipelineConfig::default();
    cfg.admission = config::AdmissionStrategy::DeclaredCeiling;
    RequestPipeline::new(cfg)
}

/// 开启后，估算超过声明上限即在发送前拒绝，并说明这是估算。
#[test]
fn admission_refuses_above_the_declared_ceiling() {
    let p = admitting_pipeline();
    let wire = admission_wire(500);
    let estimated = measure_wire_tokens(&wire).unwrap().total;
    assert!(
        p.admit(&wire, Some(estimated as i64)).is_ok(),
        "恰好等于上限应放行"
    );
    let err = p.admit(&wire, Some(estimated as i64 - 1)).unwrap_err();
    let message = err.to_string();
    assert!(message.contains("local_token_admission"));
    assert!(
        message.contains("local heuristic"),
        "错误必须说明判据是本地估算，可能拒掉上游本会接受的请求：{message}"
    );
}

/// 上限未知时不拦截：未知不是无限，也不是零，它只是没有依据去拦。
#[test]
fn admission_does_not_refuse_without_a_declared_ceiling() {
    let p = admitting_pipeline();
    let wire = admission_wire(5000);
    assert!(p.admit(&wire, None).is_ok(), "未知上限不得拦截");
    assert!(
        p.admit(&wire, Some(0)).is_ok(),
        "0 视为无有效声明，不得当成零配额拦死"
    );
    assert!(p.admit(&wire, Some(-1)).is_ok());
}

/// 关闭时（默认）行为与改造前完全一致，任何大小都不拦。
#[test]
fn admission_is_inert_while_disabled() {
    let p = RequestPipeline::new(config::PipelineConfig::default());
    let wire = admission_wire(5000);
    assert!(p.admit(&wire, Some(1)).is_ok());
}

/// 写死的模型名窗口表永远不得充当上限：即便它给出一个很小的值，
/// 只要上游没有声明，就不拦——拿猜测做拦截等于把猜测升级成门禁。
#[test]
fn hardcoded_window_table_is_never_used_as_a_ceiling() {
    let p = admitting_pipeline();
    let wire = admission_wire(5000);
    let guessed = crate::anthropic::converter::get_context_window_size("claude-sonnet-4");
    assert!(guessed > 0, "该模型确实有一个写死的窗口值");
    // admit 只接受显式传入的声明上限；没有任何路径会把猜测表喂进来。
    assert!(p.admit(&wire, None).is_ok());
}

fn tool_result_payload(body: &str) -> crate::anthropic::types::MessagesRequest {
    use crate::anthropic::types::{Message, MessagesRequest};
    MessagesRequest {
        model: "claude-sonnet-4.5".into(),
        max_tokens: 1024,
        messages: vec![
            Message {
                role: "user".into(),
                content: json!("读文件"),
            },
            Message {
                role: "assistant".into(),
                content: json!([
                {"type":"tool_use","id":"t1","name":"read","input":{"path":"/a"}}]),
            },
            Message {
                role: "user".into(),
                content: json!([
                {"type":"tool_result","tool_use_id":"t1","content": body}]),
            },
        ],
        stream: false,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        cache_control: None,
    }
}

fn body_for(
    payload: &crate::anthropic::types::MessagesRequest,
    cfg: &config::PipelineConfig,
) -> String {
    let converted = crate::anthropic::converter::convert_request_with_pipeline(
        payload,
        crate::model::config::ToolCompatibilityMode::Raw,
        cfg,
    )
    .unwrap();
    serialize_request(
        payload,
        &KiroRequest {
            conversation_state: converted.conversation_state,
            profile_arn: None,
            additional_model_request_fields: converted.additional_model_request_fields,
        },
        cfg,
    )
    .unwrap()
}

/// 开启恢复策略后，修正体应当真的不同于原体（分片生效）。
#[test]
fn recovery_body_applies_the_lossless_correction() {
    let mut cfg = config::PipelineConfig::default();
    cfg.recovery = config::RecoveryStrategy::LosslessRetry;
    cfg.tool_results.chunk_bytes = 4096;
    let payload = tool_result_payload(&"工具输出的一行\n".repeat(2000));
    let original = body_for(&payload, &cfg);
    let corrected = crate::pipeline::recovery_body(
        &payload,
        crate::model::config::ToolCompatibilityMode::Raw,
        &cfg,
        &original,
    )
    .expect("超长工具结果应产出修正体");
    assert_ne!(corrected, original);
    // 稳态形状不受影响：原体仍是单条目。
    assert_eq!(original.matches("\"text\"").count(), 1);
    assert!(corrected.matches("\"text\"").count() > 1);
}

/// 修正后字节毫无变化时必须返回 None——原样重发一个刚被拒的 payload 是碰运气。
#[test]
fn recovery_body_refuses_to_resend_identical_bytes() {
    let mut cfg = config::PipelineConfig::default();
    cfg.recovery = config::RecoveryStrategy::LosslessRetry;
    cfg.tool_results.chunk_bytes = 4096;
    // 工具结果很短，分片无从下手 → 修正体与原体一致 → 不得重发。
    let payload = tool_result_payload("short");
    let original = body_for(&payload, &cfg);
    assert!(
        crate::pipeline::recovery_body(
            &payload,
            crate::model::config::ToolCompatibilityMode::Raw,
            &cfg,
            &original
        )
        .is_none()
    );

    // 分片已在稳态启用时，重试产出的也是同样的字节 → 同样不得重发。
    let mut already = cfg.clone();
    already.tool_results.strategy = config::ToolResultStrategy::LosslessChunks;
    let payload = tool_result_payload(&"工具输出的一行\n".repeat(2000));
    let original = body_for(&payload, &already);
    assert!(
        crate::pipeline::recovery_body(
            &payload,
            crate::model::config::ToolCompatibilityMode::Raw,
            &already,
            &original
        )
        .is_none()
    );
}

/// 默认关闭时永不产出修正体。
#[test]
fn recovery_body_is_inert_while_disabled() {
    let cfg = config::PipelineConfig::default();
    let payload = tool_result_payload(&"工具输出的一行\n".repeat(2000));
    let original = body_for(&payload, &cfg);
    assert!(
        crate::pipeline::recovery_body(
            &payload,
            crate::model::config::ToolCompatibilityMode::Raw,
            &cfg,
            &original
        )
        .is_none()
    );
}

// ---------- 末尾 assistant（prefill）----------

fn refusing_config() -> config::PipelineConfig {
    config::PipelineConfig {
        prefill: config::PrefillStrategy::Refuse,
        ..config::PipelineConfig::default()
    }
}

fn prefill_request() -> MessagesRequest {
    serde_json::from_value(json!({
        "model": "claude-sonnet-4",
        "max_tokens": 100,
        "messages": [
            {"role": "user", "content": "Question one"},
            {"role": "assistant", "content": "I'll start by"}
        ]
    }))
    .unwrap()
}

/// 显式选了 `refuse` 就必须拒绝，且请求原样不动。
#[test]
fn choosing_refuse_rejects_without_touching_the_request() {
    let mut payload = prefill_request();
    let before = payload.messages.len();
    let pipeline = RequestPipeline::new(refusing_config());

    // `ContextSession` 刻意不实现 Debug——它持有转存的原文，实现 Debug 等于
    // 给内容开一条进日志的路。所以这里用 let-else 而不是 unwrap_err。
    let Err(error) = pipeline.prepare(&mut payload, 1) else {
        panic!("末尾 assistant 必须被拒绝");
    };
    let rendered = format!("{error:#}");
    assert!(rendered.contains("assistant prefill"), "{rendered}");
    assert!(
        rendered.contains("no messages have been dropped"),
        "要说清什么都没丢：{rendered}"
    );
    assert_eq!(payload.messages.len(), before, "拒绝的请求不得被改动");
}

/// 错误里要带**角色序列**，否则分不清这是刻意的开头续写还是别的形状被误判。
/// 序列里只有 role，内容一个字都不出现。
#[test]
fn the_refusal_names_the_role_sequence_without_any_content() {
    let mut payload = prefill_request();
    let pipeline = RequestPipeline::new(refusing_config());
    let Err(error) = pipeline.prepare(&mut payload, 1) else {
        panic!("末尾 assistant 必须被拒绝");
    };
    let rendered = format!("{error:#}");

    assert!(
        rendered.contains("Roles received: [user, assistant]"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("Question one") && !rendered.contains("I'll start by"),
        "错误里不得出现任何消息内容：{rendered}"
    );
    // 并且要告诉运维怎么改回旧行为。
    assert!(rendered.contains("requestPipeline.prefill"), "{rendered}");
}

/// 长会话的角色序列要折叠，不能把整段历史铺开。
#[test]
fn a_long_conversation_folds_its_role_sequence() {
    let mut messages = Vec::new();
    for _ in 0..10 {
        messages.push(json!({"role": "user", "content": "q"}));
        messages.push(json!({"role": "assistant", "content": "a"}));
    }
    let mut payload: MessagesRequest = serde_json::from_value(json!({
        "model": "claude-sonnet-4", "max_tokens": 100, "messages": messages
    }))
    .unwrap();
    let pipeline = RequestPipeline::new(refusing_config());
    let Err(error) = pipeline.prepare(&mut payload, 1) else {
        panic!("末尾 assistant 必须被拒绝");
    };
    let rendered = format!("{error:#}");
    assert!(rendered.contains("…12 more…"), "中段应折叠：{rendered}");
}

/// 默认（`drop`）保持 0.9.0 的行为：截断到最后一条 user 继续。
#[test]
fn the_default_keeps_the_pre_0_9_1_truncation() {
    let pipeline = RequestPipeline::new(config::PipelineConfig::default());
    let mut payload = prefill_request();

    assert!(
        pipeline.prepare(&mut payload, 1).is_ok(),
        "默认就是 drop，不该报错"
    );
    let converted = crate::anthropic::converter::convert_request_with_pipeline(
        &payload,
        crate::model::config::ToolCompatibilityMode::Raw,
        &pipeline.config,
    )
    .expect("转换应当成功");
    // 转换器截断到最后一条 user；payload 本身没有被就地改写。
    assert_eq!(payload.messages.len(), 2, "prepare 不该就地删消息");
    assert!(
        !serde_json::to_string(&converted.conversation_state)
            .unwrap()
            .contains("I'll start by"),
        "被丢弃的 prefill 不该出现在送出的请求里"
    );
}

/// prefill 的处理与 `mode` **正交**：把预算强制关掉，不等于同意悄悄丢内容。
///
/// 这两件事此前是绑在一起的——`mode` 一旦不是 enforce，`prepare` 就提前返回，
/// 转换器那条静默丢弃路径接管，同一份配置在两条路径上表现不同。
#[test]
fn turning_off_enforcement_does_not_silently_re_enable_dropping() {
    for mode in [
        config::PipelineMode::Off,
        config::PipelineMode::Audit,
        config::PipelineMode::Enforce,
    ] {
        let mut cfg = refusing_config();
        cfg.mode = mode;
        let pipeline = RequestPipeline::new(cfg);
        let mut payload = prefill_request();
        assert!(
            pipeline.prepare(&mut payload, 1).is_err(),
            "{mode:?}：选了 refuse 就该拒绝，与 mode 无关"
        );
    }
}

/// 转换器是最后一道：`prepare()` 不在调用路径上时（内部轮次、compaction 通道）
/// 由它兜住，免得同一份配置在两条路径上表现不同。
#[test]
fn the_converter_refuses_too_when_prepare_is_not_in_the_path() {
    let cfg = refusing_config();
    let payload = prefill_request();
    let error = crate::anthropic::converter::convert_request_with_pipeline(
        &payload,
        crate::model::config::ToolCompatibilityMode::Raw,
        &cfg,
    )
    .unwrap_err();
    assert!(
        format!("{error}").contains("assistant prefill"),
        "实得：{error}"
    );
}

/// 默认必须是 `drop`。
///
/// 拒绝并**没有**把 prefill 保住——Kiro 两种情况下都用不上它。默认拒绝换不来
/// "内容被保住"，只会让拿 prefill 约束小工具调用输出格式的客户端陷进重试循环。
#[test]
fn the_default_is_drop_because_refusing_saves_nothing() {
    assert_eq!(
        config::PipelineConfig::default().prefill,
        config::PrefillStrategy::Drop
    );
    assert_eq!(config::PrefillStrategy::default(), config::PrefillStrategy::Drop);
}
