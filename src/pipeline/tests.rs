use super::*;
use serde_json::{Value, json};

#[test]
fn portable_text_is_the_missing_field_default_but_explicit_legacy_values_survive() {
    let default: config::PipelineConfig = serde_json::from_value(json!({})).unwrap();
    assert_eq!(
        default.unexpressible,
        expressible::UnexpressibleStrategy::PortableText
    );

    for (wire, expected) in [
        ("refuse", expressible::UnexpressibleStrategy::Refuse),
        ("drop", expressible::UnexpressibleStrategy::Drop),
        (
            "portable-text",
            expressible::UnexpressibleStrategy::PortableText,
        ),
    ] {
        let parsed: config::PipelineConfig = serde_json::from_value(json!({
            "unexpressible": wire
        }))
        .unwrap();
        assert_eq!(parsed.unexpressible, expected);
        assert_eq!(serde_json::to_value(parsed).unwrap()["unexpressible"], wire);
    }
}

#[derive(Default)]
struct FakeKiroUpstream {
    received: Vec<String>,
}

impl FakeKiroUpstream {
    fn send(&mut self, body: String) {
        self.received.push(body);
    }
}

fn wire_contains_text(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(text) => text.contains(needle),
        Value::Array(values) => values.iter().any(|value| wire_contains_text(value, needle)),
        Value::Object(values) => values
            .values()
            .any(|value| wire_contains_text(value, needle)),
        _ => false,
    }
}

#[test]
fn cc_switch_history_reaches_fake_kiro_without_private_fields() {
    let mut payload: MessagesRequest = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/portable-history-cc-switch.json"
    )))
    .unwrap();
    let pipeline = RequestPipeline::new(config::PipelineConfig::default());
    let _prepared = pipeline.prepare(&mut payload, 1).unwrap();
    let converted = crate::anthropic::converter::convert_request_with_pipeline(
        &payload,
        crate::model::config::ToolCompatibilityMode::Raw,
        &pipeline.config,
    )
    .unwrap();
    let body = serialize_request(
        &payload,
        &KiroRequest {
            conversation_state: converted.conversation_state,
            profile_arn: None,
            additional_model_request_fields: converted.additional_model_request_fields,
        },
        &pipeline.config,
    )
    .unwrap();
    let mut upstream = FakeKiroUpstream::default();
    upstream.send(body);
    assert_eq!(upstream.received.len(), 1);
    let received: Value = serde_json::from_str(&upstream.received[0]).unwrap();
    assert_eq!(
        received
            .pointer("/conversationState/currentMessage/userInputMessage/content")
            .and_then(Value::as_str),
        Some("Continue after switching providers")
    );
    let history = received
        .pointer("/conversationState/history")
        .and_then(Value::as_array)
        .unwrap();
    let use_entry = history
        .iter()
        .flat_map(|entry| {
            entry
                .pointer("/assistantResponseMessage/toolUses")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .find(|tool| tool.get("toolUseId").and_then(Value::as_str) == Some("read-1"));
    assert!(use_entry.is_some(), "read-1 invocation must reach Kiro");
    let result_entry = history
        .iter()
        .flat_map(|entry| {
            entry
                .pointer("/userInputMessage/userInputMessageContext/toolResults")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .find(|result| result.get("toolUseId").and_then(Value::as_str) == Some("read-1"))
        .expect("read-1 result must reach Kiro");
    let result_text = result_entry
        .get("content")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "PORTABLE_DOCUMENT_TEXT_SENTINEL",
        "FUTURE_READABLE_TEXT_SENTINEL",
    ] {
        assert!(
            result_text.contains(expected),
            "read-1 result missed {expected}: {result_text}"
        );
    }
    let search_text = history
        .iter()
        .filter_map(|entry| {
            entry
                .pointer("/assistantResponseMessage/content")
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "PUBLIC_SEARCH_TITLE_SENTINEL",
        "https://public-search.invalid/portable-history",
    ] {
        assert!(
            search_text.contains(expected),
            "projected search content missed {expected}: {search_text}"
        );
    }
    for forbidden in [
        "SIGNATURE_MUST_NOT_REACH_KIRO",
        "ENCRYPTED_MUST_NOT_REACH_KIRO",
        "REDACTED_MUST_NOT_REACH_KIRO",
        "normalization",
        "messages[",
        "portable_history.",
        "BINARY_ATTACHMENT_MUST_NOT_REACH_KIRO",
        "QklOQVJZX0FUVEFDSE1FTlRfTVVTVF9OT1RfUkVBQ0hfS0lSTw==",
    ] {
        assert!(
            !wire_contains_text(&received, forbidden),
            "fake upstream received private value {forbidden}: {received}"
        );
    }
}

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

fn historical_nested_document_fixture() -> MessagesRequest {
    serde_json::from_value(json!({
        "model":"claude-sonnet-4", "max_tokens":1024,
        "messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"read-1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"read-1","content":[
                {"type":"document","source":{"type":"text","media_type":"text/plain","data":"portable body"}}
            ]}]},
            {"role":"assistant","content":"previous answer"},
            {"role":"user","content":"continue"}
        ]
    })).unwrap()
}

#[test]
fn compatibility_strategy_is_orthogonal_to_pipeline_mode() {
    for mode in [
        config::PipelineMode::Off,
        config::PipelineMode::Audit,
        config::PipelineMode::Enforce,
    ] {
        for strategy in [
            expressible::UnexpressibleStrategy::PortableText,
            expressible::UnexpressibleStrategy::Refuse,
            expressible::UnexpressibleStrategy::Drop,
        ] {
            let pipeline = RequestPipeline::new(config::PipelineConfig {
                mode,
                unexpressible: strategy,
                ..Default::default()
            });
            let mut payload = historical_nested_document_fixture();
            let result = pipeline.prepare(&mut payload, 1);
            match strategy {
                expressible::UnexpressibleStrategy::PortableText => {
                    let outcome = result.unwrap();
                    assert!(outcome.normalization.transformed_blocks > 0);
                    let content = &payload.messages[1].content[0]["content"];
                    assert_eq!(content[0]["type"], "text", "{mode:?}");
                    assert!(content.to_string().contains("portable body"));
                }
                expressible::UnexpressibleStrategy::Refuse => assert!(result.is_err(), "{mode:?}"),
                expressible::UnexpressibleStrategy::Drop => {
                    let outcome = result.unwrap();
                    assert_eq!(outcome.normalization.by_action["legacy_dropped"], 1);
                    assert!(
                        !payload.messages[1]
                            .content
                            .to_string()
                            .contains("portable body"),
                        "{mode:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn normalized_ingress_budget_rejection_is_atomic_in_every_mode() {
    struct Fingerprint;
    impl portable_history::SensitiveFingerprint for Fingerprint {
        fn fingerprint(&self, _: &[u8], _: &[u8]) -> String {
            "local".into()
        }
    }
    for mode in [
        config::PipelineMode::Off,
        config::PipelineMode::Audit,
        config::PipelineMode::Enforce,
    ] {
        let mut payload = historical_nested_document_fixture();
        let original = serde_json::to_value(&payload).unwrap();
        let normalized = portable_history::normalize(
            &payload,
            expressible::UnexpressibleStrategy::PortableText,
            &Fingerprint,
        )
        .unwrap();
        let limit = serde_json::to_vec(&normalized.payload).unwrap().len() - 1;
        let pipeline = RequestPipeline::new(config::PipelineConfig {
            mode,
            ingress_max_bytes: limit,
            ..Default::default()
        });
        let error = pipeline
            .prepare(&mut payload, 1)
            .err()
            .expect("normalized budget must be checked");
        assert_eq!(error.code(), "portable_history.budget_exceeded");
        assert_eq!(error.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(serde_json::to_value(&payload).unwrap(), original);
    }
}

#[test]
fn later_image_failure_does_not_commit_normalization_or_thinking_or_billing() {
    let mut payload = historical_nested_document_fixture();
    payload.model = "claude-sonnet-4-thinking".into();
    payload.system = request_fixture().system;
    payload.messages[3].content = json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"INVALID"}}]);
    let original = serde_json::to_value(&payload).unwrap();
    let mut config = config::PipelineConfig::default();
    config.images.strategy = config::ImageStrategy::LosslessTiles;
    assert!(
        RequestPipeline::new(config)
            .prepare(&mut payload, 1)
            .is_err()
    );
    assert_eq!(serde_json::to_value(&payload).unwrap(), original);
}

#[test]
fn later_artifact_failure_does_not_commit_preparation() {
    let mut payload = historical_nested_document_fixture();
    payload.model = "claude-sonnet-4-thinking".into();
    payload.system = request_fixture().system;
    payload.tools = Some(vec![serde_json::from_value(json!({"name":"kiro_context_read","description":"collision","input_schema":{"type":"object"}})).unwrap()]);
    let original = serde_json::to_value(&payload).unwrap();
    let mut config = config::PipelineConfig::default();
    config.artifacts.enabled = true;
    assert!(
        RequestPipeline::new(config)
            .prepare(&mut payload, 1)
            .is_err()
    );
    assert_eq!(serde_json::to_value(payload).unwrap(), original);
}

#[test]
fn preparation_errors_hide_internal_paths_reasons_and_other_error_contents() {
    let invariant =
        PipelinePrepareError::from(portable_history::PortableHistoryError::InvariantViolation {
            path: "messages[PRIVATE_PATH]".into(),
            reason: "PRIVATE_REASON".into(),
        });
    assert_eq!(invariant.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(invariant.code(), "portable_history.invariant_violation");
    let other = PipelinePrepareError::from(anyhow::anyhow!("PRIVATE_FILE /secret/provider.json"));
    assert_eq!(other.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(other.code(), "pipeline_preparation");
    for error in [invariant, other] {
        assert!(!error.safe_message().contains("PRIVATE"));
        assert!(!error.to_string().contains("PRIVATE"));
    }
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

    // max_tokens 为 0 与「表达不了」无关：它是一个无效请求，两种策略下都报错。
    for invalid in [0, -1] {
        request.max_tokens = invalid;
        assert!(p.prepare(&mut request, 1).is_err(), "{invalid}");
    }
    request.max_tokens = 1000;

    // Current unsupported input is refused by default; explicit Drop is legacy compatibility.
    let refusing = RequestPipeline::new(config::PipelineConfig {
        unexpressible: expressible::UnexpressibleStrategy::Refuse,
        ..config::PipelineConfig::default()
    });
    for content in [
        json!([{"type":"image","source":{"type":"url","url":"https://invalid.example/image.png"}}]),
        json!([{"type":"document","source":{"data":"must not vanish"}}]),
    ] {
        let mut dropping = request_fixture();
        dropping.max_tokens = 1000;
        dropping.messages[0].content = content.clone();
        assert!(p.prepare(&mut dropping, 1).is_err());
        let legacy = RequestPipeline::new(config::PipelineConfig {
            unexpressible: expressible::UnexpressibleStrategy::Drop,
            ..Default::default()
        });
        assert!(legacy.prepare(&mut dropping, 1).is_ok());

        let mut refusing_request = request_fixture();
        refusing_request.max_tokens = 1000;
        refusing_request.messages[0].content = content.clone();
        assert!(
            refusing.prepare(&mut refusing_request, 1).is_err(),
            "选了 refuse 应拒绝：{content}"
        );
    }
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
fn gateway_search_retains_public_history_and_withholds_opaque_data() {
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
    let history = request.messages[1].content.to_string();
    assert!(history.contains("exact query"));
    assert!(history.contains("https://example.invalid/source"));
    assert!(history.contains("Source"));
    let first = request.messages[1].content.clone();
    p.prepare(&mut request, 1).unwrap();
    assert_eq!(first, request.messages[1].content);
    let wire = fixture_wire(&p, &mut request);
    assert!(!wire.contains("opaque source data"));
    assert!(!wire.contains("opaque thinking data"));
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
            .contains("tool history pairing")
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

#[test]
fn only_explicit_drop_removes_prefill_but_never_malformed_or_broken_pairs() {
    for strategy in [
        expressible::UnexpressibleStrategy::PortableText,
        expressible::UnexpressibleStrategy::Refuse,
        expressible::UnexpressibleStrategy::Drop,
    ] {
        let pipeline = RequestPipeline::new(config::PipelineConfig {
            unexpressible: strategy,
            ..Default::default()
        });
        let mut payload = prefill_request();
        let result = pipeline.prepare(&mut payload, 1);
        if strategy == expressible::UnexpressibleStrategy::Drop {
            result.unwrap();
            assert_eq!(payload.messages.len(), 1);
        } else {
            assert!(result.is_err());
        }
        for messages in [
            json!([{"role":"PRIVATE_ROLE","content":"secret"},{"role":"user","content":"continue"}]),
            json!([{"role":"user","content":[false]}]),
            json!([{"role":"assistant","content":[{"type":"tool_use","id":"missing","name":"read","input":{}}]},{"role":"user","content":"continue"}]),
            json!([{"role":"user","content":[{"type":"tool_result","tool_use_id":"orphan","content":[{"type":"document"}]}]}]),
        ] {
            let mut malformed = request_fixture();
            malformed.messages = serde_json::from_value(messages).unwrap();
            let original = serde_json::to_value(&malformed).unwrap();
            assert!(pipeline.prepare(&mut malformed, 1).is_err());
            assert_eq!(serde_json::to_value(malformed).unwrap(), original);
        }
    }
}

#[test]
fn legacy_strategies_redact_known_private_reasoning() {
    for strategy in [
        expressible::UnexpressibleStrategy::Refuse,
        expressible::UnexpressibleStrategy::Drop,
    ] {
        let pipeline = RequestPipeline::new(config::PipelineConfig {
            unexpressible: strategy,
            ..Default::default()
        });
        let mut payload = request_fixture();
        payload.messages.insert(0, crate::anthropic::types::Message { role:"assistant".into(), content:json!([
            {"type":"thinking","thinking":"public reasoning","signature":"SIGNATURE_SENTINEL"},
            {"type":"redacted_thinking","data":"OPAQUE_SENTINEL"}
        ])});
        pipeline.prepare(&mut payload, 1).unwrap();
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(serialized.contains("public reasoning"));
        assert!(!serialized.contains("SIGNATURE_SENTINEL"));
        assert!(!serialized.contains("OPAQUE_SENTINEL"));
    }
}
