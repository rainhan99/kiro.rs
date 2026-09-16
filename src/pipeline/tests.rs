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
        .audit(&body.to_string(), "ide", 1, &http::HeaderMap::new())
        .unwrap();
    body["conversationState"]["currentMessage"]["userInputMessage"]["content"] = json!("two");
    let b = p
        .audit(&body.to_string(), "ide", 1, &http::HeaderMap::new())
        .unwrap();
    assert_ne!(a["wireFingerprint"], b["wireFingerprint"]);
    assert_eq!(a["staticPrefixFingerprint"], b["staticPrefixFingerprint"]);
    let mut headers = http::HeaderMap::new();
    headers.insert("amz-sdk-invocation-id", "changed".parse().unwrap());
    headers.insert("authorization", "Bearer SECRET".parse().unwrap());
    let c = p.audit(&body.to_string(), "ide", 2, &headers).unwrap();
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
        let audit = p.audit(&next.to_string(), "ide", 2, &headers).unwrap();
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
    let aa = p.audit(&a, "ide", 1, &http::HeaderMap::new()).unwrap();
    let cc = p.audit(&c, "ide", 1, &http::HeaderMap::new()).unwrap();
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
