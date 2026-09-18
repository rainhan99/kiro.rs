//! Shared typed errors for Kiro upstream calls.

/// The upstream still returned HTTP 429 after any applicable failover/retry.
#[derive(Debug, Clone, thiserror::Error)]
#[error("upstream rate limited")]
pub struct UpstreamRateLimitError {
    retry_after: Option<String>,
}

impl UpstreamRateLimitError {
    pub(crate) fn new(retry_after: Option<String>) -> Self {
        Self {
            retry_after: retry_after.and_then(normalize_retry_after),
        }
    }

    pub(crate) fn from_headers(headers: &http::HeaderMap) -> Self {
        let retry_after = headers
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Self::new(retry_after)
    }

    pub fn retry_after(&self) -> Option<&str> {
        self.retry_after.as_deref()
    }

    /// Without an explicit upstream delay, a short local retry is still useful.
    pub(crate) fn should_retry_locally(&self) -> bool {
        self.retry_after.is_none()
    }
}

fn normalize_retry_after(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if value.parse::<u64>().is_ok() || httpdate::parse_http_date(value).is_ok() {
        Some(value.to_string())
    } else {
        None
    }
}

/// 上游在请求层面拒绝了本次调用的分类。
///
/// 分类**只在读取响应体的那一处**完成，并随类型携带到下游；下游不得再对格式化后的
/// 错误字符串做子串匹配还原分类——那样会绕过字段确认（见 [`UpstreamRequestError`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamRejectionKind {
    /// `CONTENT_LENGTH_EXCEEDS_THRESHOLD`。
    ///
    /// 上游只表示"输入长度被拒"，**没有**指明是总 body、单个字段、图片还是模型
    /// 上下文窗口。因此不得渲染成 "Context window is full"，也不得据此自动重发、
    /// 截断或降级模型。
    ContentLengthThreshold,
    /// 上游明确的 "Input is too long"。
    InputTooLong,
    /// 调用方 messages 数组本身违反协议（tool_use↔tool_result 配对/顺序）。
    /// 根因在请求体，重试和换凭据都不可能成功。
    ClientValidation,
    /// 其它请求层拒绝：不臆测其含义。
    Unclassified,
}

impl UpstreamRejectionKind {
    /// 该分类是否指向某条**长度**预算。
    ///
    /// 只有这类拒绝才可能被无损修正补救：协议配对错误再怎么改尺寸也不会通过，
    /// 未分类的拒绝更不该被当作长度问题去乱动 payload。
    pub fn names_a_length_budget(self) -> bool {
        matches!(self, Self::ContentLengthThreshold | Self::InputTooLong)
    }
}

/// 上游请求层拒绝的结构化错误。
///
/// 取代此前把 `状态码 + 报文` 拼成一行字符串、再由 handler 用 `contains` 还原分类的
/// 做法。那种做法有一个具体缺陷：拼接后的字符串不是合法 JSON，导致
/// [`crate::kiro::endpoint::default_is_client_validation_error`] 的字段确认分支必然
/// 解析失败并退化为裸子串匹配——报文里任何位置偶然出现关键词都会被误判。
///
/// `Display` 由 `label` 保留各调用路径的原有前缀，逐字复现旧格式，既有日志与
/// trace 片段不受影响。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{label}: {status} {body}")]
pub struct UpstreamRequestError {
    label: String,
    status: http::StatusCode,
    body: String,
    kind: UpstreamRejectionKind,
    reason: Option<String>,
    message: Option<String>,
}

impl UpstreamRequestError {
    /// 主 API 路径（流式/非流式）。
    pub(crate) fn api(api_type: &str, status: http::StatusCode, body: &str) -> Self {
        Self::with_label(format!("{api_type} API 请求失败"), status, body)
    }

    /// MCP 路径（WebSearch 等工具调用）。分类与主路径共用同一套字段确认逻辑，
    /// 避免两条路径各有一套口径。
    pub(crate) fn mcp(status: http::StatusCode, body: &str) -> Self {
        Self::with_label("MCP 请求失败".to_string(), status, body)
    }

    fn with_label(label: String, status: http::StatusCode, body: &str) -> Self {
        let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
        let reason = parsed.as_ref().and_then(|v| field(v, "reason"));
        let message = parsed.as_ref().and_then(|v| field(v, "message"));
        Self {
            kind: classify(body, parsed.as_ref(), reason.as_deref(), message.as_deref()),
            label,
            status,
            body: body.to_string(),
            reason,
            message,
        }
    }

    pub fn kind(&self) -> UpstreamRejectionKind {
        self.kind
    }

    pub fn status(&self) -> http::StatusCode {
        self.status
    }

    /// 保留的上游原始报文。用于证据与排障，不直接回显给客户端——它可能携带
    /// 请求 ID 或上游内部校验细节。
    ///
    /// 与下面两个访问器一样，消费者是 Budget Report（本阶段 Task 3）：报告需要把
    /// 上游给出的 reason/message 与本地预算分项对照，说明该拒绝与哪条预算线吻合。
    /// 在那之前它们没有调用方，但数据本身在分类时已经解析出来，不重复解析。
    #[allow(dead_code)]
    pub fn body(&self) -> &str {
        &self.body
    }

    #[allow(dead_code)]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    #[allow(dead_code)]
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

/// 取顶层或嵌套 `error.` 下的字符串字段；另认 AWS 风格的 `__type`。
fn field(value: &serde_json::Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(|v| v.as_str())
        .or_else(|| value.pointer(&format!("/error/{name}")).and_then(|v| v.as_str()))
        .or_else(|| {
            if name == "reason" {
                value.get("__type").and_then(|v| v.as_str())
            } else {
                None
            }
        })
        .map(str::to_owned)
}

/// 上游的输入长度拒绝 reason 码。
const CONTENT_LENGTH_REASON: &str = "CONTENT_LENGTH_EXCEEDS_THRESHOLD";
/// message 级特征短语（无结构化 reason 的纯文本报文场景）。
const INPUT_TOO_LONG_MARKER: &str = "Input is too long";

/// 与 [`crate::kiro::endpoint`] 的既有分类器同构：先廉价子串快扫，命中后用已解析的
/// JSON 字段确认；报文非 JSON 时才回退到子串，兼容纯文本错误响应。
fn classify(
    body: &str,
    parsed: Option<&serde_json::Value>,
    reason: Option<&str>,
    message: Option<&str>,
) -> UpstreamRejectionKind {
    // 顺序与旧 handler 的判定顺序一致，避免分类漂移。
    if body.contains(CONTENT_LENGTH_REASON) {
        let confirmed = match parsed {
            Some(_) => reason.is_some_and(|r| r.contains(CONTENT_LENGTH_REASON)),
            // 非 JSON 报文没有字段可确认，只能采信子串。
            None => true,
        };
        if confirmed {
            return UpstreamRejectionKind::ContentLengthThreshold;
        }
    }

    if body.contains(INPUT_TOO_LONG_MARKER) {
        let confirmed = match parsed {
            Some(_) => message.is_some_and(|m| m.contains(INPUT_TOO_LONG_MARKER)),
            None => true,
        };
        if confirmed {
            return UpstreamRejectionKind::InputTooLong;
        }
    }

    // 字段确认在这里真正生效：传入的是原始报文，不是拼接后的字符串。
    if crate::kiro::endpoint::default_is_client_validation_error(body) {
        return UpstreamRejectionKind::ClientValidation;
    }

    UpstreamRejectionKind::Unclassified
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(body: &str) -> UpstreamRejectionKind {
        UpstreamRequestError::api("IDE", http::StatusCode::BAD_REQUEST, body).kind()
    }

    #[test]
    fn content_length_threshold_is_confirmed_from_reason_field() {
        assert_eq!(
            kind_of(r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#),
            UpstreamRejectionKind::ContentLengthThreshold
        );
        assert_eq!(
            kind_of(r#"{"error":{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}}"#),
            UpstreamRejectionKind::ContentLengthThreshold
        );
    }

    /// 关键词只出现在被回显的文本里时，不得判成长度拒绝。
    /// 旧实现在拼接字符串上做裸 `contains`，这一条必然误判。
    #[test]
    fn keyword_echoed_in_other_fields_is_not_a_threshold_rejection() {
        assert_eq!(
            kind_of(
                r#"{"reason":"INTERNAL_SERVER_ERROR","detail":"user asked about CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#
            ),
            UpstreamRejectionKind::Unclassified
        );
    }

    /// 同理：偶然提及精确 reason 码不等于该 reason 成立。
    #[test]
    fn client_validation_requires_the_reason_field_not_a_mention() {
        assert_eq!(
            kind_of(r#"{"reason":"TOOL_USE_RESULT_MISMATCH"}"#),
            UpstreamRejectionKind::ClientValidation
        );
        assert_eq!(
            kind_of(
                r#"{"reason":"INTERNAL_SERVER_ERROR","message":"tool output mentioned TOOL_USE_RESULT_MISMATCH"}"#
            ),
            UpstreamRejectionKind::Unclassified
        );
    }

    #[test]
    fn input_too_long_is_confirmed_from_message_field() {
        assert_eq!(
            kind_of(r#"{"message":"Input is too long for requested model."}"#),
            UpstreamRejectionKind::InputTooLong
        );
    }

    /// 纯文本报文没有字段可确认，只能采信子串——但这条回退不得反过来放宽 JSON 报文。
    #[test]
    fn non_json_body_falls_back_to_substring() {
        assert_eq!(
            kind_of("CONTENT_LENGTH_EXCEEDS_THRESHOLD"),
            UpstreamRejectionKind::ContentLengthThreshold
        );
        assert_eq!(
            kind_of("Expected toolResult blocks"),
            UpstreamRejectionKind::ClientValidation
        );
    }

    #[test]
    fn status_and_raw_body_are_retained_and_display_is_unchanged() {
        let body = r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#;
        let err = UpstreamRequestError::api("IDE", http::StatusCode::BAD_REQUEST, body);
        assert_eq!(err.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(err.body(), body);
        assert_eq!(err.reason(), Some("CONTENT_LENGTH_EXCEEDS_THRESHOLD"));
        assert_eq!(
            err.to_string(),
            format!("IDE API 请求失败: {} {}", http::StatusCode::BAD_REQUEST, body),
            "Display 必须与旧格式逐字一致，既有日志与 trace 片段才不受影响"
        );
    }

    #[test]
    fn accepts_delta_seconds_and_http_date() {
        let seconds = UpstreamRateLimitError::new(Some(" 1800 ".to_string()));
        assert_eq!(seconds.retry_after(), Some("1800"));
        assert!(!seconds.should_retry_locally());

        let date = "Sun, 12 Jul 2026 02:30:00 GMT";
        let http_date = UpstreamRateLimitError::new(Some(date.to_string()));
        assert_eq!(http_date.retry_after(), Some(date));
        assert!(!http_date.should_retry_locally());
    }

    #[test]
    fn rejects_invalid_retry_after() {
        let error = UpstreamRateLimitError::new(Some("not-a-retry-delay".to_string()));
        assert_eq!(error.retry_after(), None);
        assert!(error.should_retry_locally());
    }
}
