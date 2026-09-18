use super::*;
use crate::gateway::UpstreamKind;

fn upstream(base: Option<&str>, allow_private: bool) -> Upstream {
    Upstream {
        id: "u1".into(),
        name: "u1".into(),
        kind: UpstreamKind::Anthropic,
        enabled: true,
        weight: 1,
        base_url: base.map(str::to_string),
        api_key: Some("secret-key".into()),
        has_api_key: true,
        allow_private_network: allow_private,
        kiro_group: None,
        cache_usage_policy: None,
    }
}

/// 路径由协议决定，调用方无权指定；基址末尾多余的斜杠要吃掉。
#[test]
fn the_path_comes_from_the_protocol_not_the_caller() {
    for (base, protocol, expected) in [
        (
            "https://api.example.test",
            WireProtocol::Anthropic,
            "https://api.example.test/v1/messages",
        ),
        (
            "https://api.example.test/",
            WireProtocol::ChatCompletions,
            "https://api.example.test/v1/chat/completions",
        ),
        (
            "https://api.example.test",
            WireProtocol::Responses,
            "https://api.example.test/v1/responses",
        ),
    ] {
        let endpoint = resolve_endpoint(&upstream(Some(base), false), protocol).unwrap();
        assert_eq!(endpoint.url, expected);
    }
}

/// 基址里带凭据、查询串或片段一律拒绝——干净的基址不该有这些。
#[test]
fn a_base_url_with_credentials_query_or_fragment_is_refused() {
    for (base, expect) in [
        ("https://user:pass@api.example.test", "embed credentials"),
        ("https://api.example.test?token=abc", "query or fragment"),
        ("https://api.example.test#frag", "query or fragment"),
    ] {
        let error = resolve_endpoint(&upstream(Some(base), false), WireProtocol::Anthropic)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains(expect),
            "{base} 应被拒绝：{error:#}"
        );
    }
}

/// 明文 HTTP 只在显式放行时可用；其它协议一律拒绝。
#[test]
fn plaintext_http_requires_an_explicit_opt_in() {
    let error = resolve_endpoint(
        &upstream(Some("http://api.example.test"), false),
        WireProtocol::Anthropic,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("plaintext http"));

    // 显式放行后可用（同时跳过私网检查）。
    assert!(
        resolve_endpoint(
            &upstream(Some("http://api.example.test"), true),
            WireProtocol::Anthropic
        )
        .is_ok()
    );

    for scheme in ["ftp://h", "file:///etc/passwd", "gopher://h"] {
        let error =
            resolve_endpoint(&upstream(Some(scheme), true), WireProtocol::Anthropic).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported scheme"));
    }
}

/// 私网、回环、链路本地与云元数据地址必须被挡住。
/// 用字面 IP 断言，不依赖 DNS，测试因此不需要网络。
#[test]
fn private_loopback_and_metadata_addresses_are_blocked() {
    for host in [
        "127.0.0.1",
        "10.0.0.5",
        "192.168.1.1",
        "172.16.0.1",
        // 云环境的元数据服务：拿到它等于拿到实例凭据。
        "169.254.169.254",
        // 运营商级 NAT 段，同样不该是公网上游。
        "100.64.0.1",
        "[::1]",
        "[fc00::1]",
        "[fe80::1]",
        // IPv4 映射地址：按内嵌的 v4 判断，否则是个绕过口子。
        "[::ffff:127.0.0.1]",
    ] {
        let base = format!("https://{host}");
        let error = resolve_endpoint(&upstream(Some(&base), false), WireProtocol::Anthropic)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("non-public address"),
            "{host} 必须被挡住：{error:#}"
        );
    }
}

/// 显式放行后私网地址可用——这是运维的明确选择，不是默认。
#[test]
fn an_explicit_opt_in_allows_a_private_destination() {
    let endpoint = resolve_endpoint(
        &upstream(Some("https://10.0.0.5"), true),
        WireProtocol::Anthropic,
    )
    .unwrap();
    assert_eq!(endpoint.url, "https://10.0.0.5/v1/messages");
}

/// 没有 baseUrl 就没有端点可构造。
#[test]
fn a_missing_base_url_is_an_error() {
    let error = resolve_endpoint(&upstream(None, false), WireProtocol::Anthropic).unwrap_err();
    assert!(format!("{error:#}").contains("no baseUrl"));
}

/// 鉴权头只来自该上游配置的密钥，且按协议放在正确位置。
#[test]
fn auth_headers_come_only_from_the_configured_secret() {
    let anthropic = request_headers(&upstream(Some("https://h"), false), WireProtocol::Anthropic);
    assert!(anthropic.contains(&("x-api-key", "secret-key".into())));
    assert!(anthropic.iter().any(|(k, _)| *k == "anthropic-version"));

    for protocol in [WireProtocol::ChatCompletions, WireProtocol::Responses] {
        let headers = request_headers(&upstream(Some("https://h"), false), protocol);
        assert!(headers.contains(&("authorization", "Bearer secret-key".into())));
    }

    // 未配置密钥时不得凭空产生鉴权头。
    let mut anonymous = upstream(Some("https://h"), false);
    anonymous.api_key = None;
    let headers = request_headers(&anonymous, WireProtocol::ChatCompletions);
    assert!(headers.iter().all(|(k, _)| *k != "authorization"));
    assert_eq!(headers.len(), 1, "只剩 content-type");
}

/// 分类只看状态码与结构化字段，**不在任意文本里搜关键词**。
#[test]
fn classification_uses_status_and_structured_fields_not_free_text() {
    // 正文里出现 quota 但状态是 200 段之外的一般错误 → 不得判成配额。
    let body = r#"{"error":{"type":"invalid_request_error","message":"your quota example is wrong"}}"#;
    assert_eq!(
        classify(400, body, None),
        UpstreamFailure::InvalidRequest {
            status: 400,
            snippet: redact(body)
        },
        "正文里偶然出现 quota 不等于配额用尽"
    );

    // 结构化字段明确指出上下文超限。
    let context = r#"{"error":{"code":"context_length_exceeded"}}"#;
    assert!(matches!(
        classify(400, context, None),
        UpstreamFailure::ContextLimit { .. }
    ));
    // 413 本身就是长度语义。
    assert!(matches!(
        classify(413, "{}", None),
        UpstreamFailure::ContextLimit { .. }
    ));
    // 结构化配额码。
    assert!(matches!(
        classify(400, r#"{"error":{"code":"insufficient_quota"}}"#, None),
        UpstreamFailure::Quota { .. }
    ));
}

/// 各状态段的归类与可重试性。
#[test]
fn status_classes_map_to_the_right_failure_and_retryability() {
    assert_eq!(
        classify(401, "{}", None),
        UpstreamFailure::Authentication { status: 401 }
    );
    assert_eq!(
        classify(403, "{}", None),
        UpstreamFailure::Authentication { status: 403 }
    );
    assert!(matches!(
        classify(402, "{}", None),
        UpstreamFailure::Quota { .. }
    ));
    for status in [500_u16, 502, 503, 504, 408] {
        assert!(
            matches!(classify(status, "{}", None), UpstreamFailure::Transient { .. }),
            "{status} 应判为瞬态"
        );
    }
    assert!(classify(429, "{}", None).is_retryable());
    assert!(classify(503, "{}", None).is_retryable());
    assert!(!classify(400, "{}", None).is_retryable());
    assert!(!classify(401, "{}", None).is_retryable());
}

/// Retry-After 只采信规范形式；无法解析的值不猜一个退避时长。
#[test]
fn retry_after_is_taken_only_when_well_formed() {
    let with = |value: Option<&str>| match classify(429, "{}", value) {
        UpstreamFailure::Throttled { retry_after, .. } => retry_after,
        other => panic!("429 应判为限流，实得 {other:?}"),
    };
    assert_eq!(with(Some("30")), Some("30".into()));
    assert_eq!(
        with(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
        Some("Wed, 21 Oct 2026 07:28:00 GMT".into())
    );
    assert_eq!(with(Some("soon")), None, "无法解析的值不予采信");
    assert_eq!(with(Some("")), None);
    assert_eq!(with(None), None);
}

/// 错误片段要截断并压平控制字符，避免把大段上游报文写进日志。
#[test]
fn error_snippets_are_flattened_and_bounded() {
    let noisy = format!("line one\nline two\r\n{}", "x".repeat(2000));
    let snippet = redact(&noisy);
    assert!(!snippet.contains('\n'), "控制字符应被压平");
    assert!(snippet.chars().count() <= MAX_SNIPPET + 1, "应被截断");
    assert!(snippet.ends_with('…'));
}

/// 超时只能收紧，不能放宽到配置上限之外。
#[test]
fn a_caller_deadline_can_only_tighten_the_timeout() {
    let configured = Duration::from_secs(120);
    assert_eq!(effective_timeout(configured, None), configured);
    assert_eq!(
        effective_timeout(configured, Some(Duration::from_secs(5))),
        Duration::from_secs(5)
    );
    assert_eq!(
        effective_timeout(configured, Some(Duration::from_secs(9999))),
        configured,
        "调用方不得把超时放宽到配置之外"
    );
}

/// 域名不在构造期解析——端点构造必须是纯函数，一次 DNS 抖动不该变成"端点构造失败"，
/// 而且离线也要能测。解析检查推迟到连接前，由 `ensure_public_target` 执行。
#[test]
fn domain_hosts_defer_resolution_to_connect_time() {
    let endpoint = resolve_endpoint(
        &upstream(Some("https://api.example.test"), false),
        WireProtocol::Anthropic,
    )
    .expect("构造端点不应需要 DNS");
    assert_eq!(endpoint.host, "api.example.test");
    assert_eq!(endpoint.port, 443);
    assert!(endpoint.check_resolved, "域名仍需在连接前检查解析结果");
}

/// 显式放行私网后，连接前检查整体跳过——这是运维的明确选择。
#[test]
fn an_explicit_opt_in_skips_the_connect_time_check() {
    let endpoint = resolve_endpoint(
        &upstream(Some("https://internal.corp"), true),
        WireProtocol::Anthropic,
    )
    .unwrap();
    assert!(!endpoint.check_resolved);
    assert!(ensure_public_target(&endpoint, "u1").is_ok());
}

/// 解析到回环地址的域名必须在连接前被挡住——只看字面量不够，
/// 一个公网域名完全可以解析到 127.0.0.1。
#[test]
fn a_domain_resolving_to_loopback_is_blocked_before_connecting() {
    // localhost 在所有平台都解析到回环，不需要外部网络。
    let endpoint = resolve_endpoint(
        &upstream(Some("https://localhost"), false),
        WireProtocol::Anthropic,
    )
    .unwrap();
    let error = ensure_public_target(&endpoint, "u1").unwrap_err();
    assert!(
        format!("{error:#}").contains("non-public address"),
        "解析到回环的域名必须被挡住：{error:#}"
    );
}
