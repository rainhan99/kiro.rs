//! 直连上游的传输层：URL 校验、鉴权注入与错误分类。
//!
//! # 为什么这些决定必须归传输层所有
//!
//! 端点、鉴权与网络边界由本模块单独决定，调用方**不能**传入 URL 或请求头。上游的
//! `baseUrl` 是运维可配的，一旦允许调用方在其上拼接路径或追加头部，一个被构造的
//! 配置或参数就能把请求打到内网地址、或把凭据带到别处去。
//!
//! # 防护点
//!
//! - 只允许 HTTPS；明文 HTTP 仅在该上游显式开启 `allowPrivateNetwork` 时放行；
//! - 拒绝 URL 内嵌凭据（`user:pass@`）、片段和查询串——它们都不该出现在基址里；
//! - 解析目标地址并检查是否落在私网/回环/链路本地，除非显式放行；
//! - **禁用重定向**：一个 302 就能把带着 Authorization 的请求引到任意主机；
//! - 鉴权头只来自该上游配置的密钥，不透传客户端任何头部；
//! - 错误片段一律脱敏后再记录。

use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Result, bail, ensure};

use super::{Upstream, protocol::WireProtocol};

/// 错误片段保留的最大长度。上游报文可能带请求 ID 或内部细节，不宜全量落库。
const MAX_SNIPPET: usize = 512;

/// 上游调用的分类错误。
///
/// 分类**只依据状态码与结构化报文字段**，不靠在任意文本里搜关键词——
/// 一段正常内容里出现 "quota" 不等于配额用尽。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamFailure {
    /// 请求本身不合法，重试与换号都不会成功。
    InvalidRequest { status: u16, snippet: String },
    /// 超出上下文/长度限制。
    ContextLimit { status: u16, snippet: String },
    /// 配额或余额耗尽。
    Quota { status: u16, snippet: String },
    /// 限流。`retry_after` 来自响应头，仅在合法时保留。
    Throttled {
        status: u16,
        retry_after: Option<String>,
    },
    /// 鉴权失败。
    Authentication { status: u16 },
    /// 可重试的瞬态故障。
    Transient { status: u16, snippet: String },
    /// 连接或流在中途断开。
    StreamInterrupted { detail: String },
}

impl std::fmt::Display for UpstreamFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest { status, .. } => write!(f, "upstream rejected the request ({status})"),
            Self::ContextLimit { status, .. } => write!(f, "upstream context limit ({status})"),
            Self::Quota { status, .. } => write!(f, "upstream quota exhausted ({status})"),
            Self::Throttled { status, .. } => write!(f, "upstream throttled ({status})"),
            Self::Authentication { status } => write!(f, "upstream authentication failed ({status})"),
            Self::Transient { status, .. } => write!(f, "upstream transient failure ({status})"),
            Self::StreamInterrupted { detail } => write!(f, "upstream stream interrupted: {detail}"),
        }
    }
}

impl std::error::Error for UpstreamFailure {}

impl UpstreamFailure {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Throttled { .. } | Self::Transient { .. } | Self::StreamInterrupted { .. }
        )
    }
}

/// 按状态码与结构化字段分类，绝不在任意文本里搜关键词。
pub fn classify(status: u16, body: &str, retry_after: Option<&str>) -> UpstreamFailure {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let code = parsed.as_ref().and_then(|value| {
        value
            .pointer("/error/code")
            .or_else(|| value.pointer("/error/type"))
            .or_else(|| value.get("type"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    });
    let snippet = redact(body);

    match status {
        401 | 403 => UpstreamFailure::Authentication { status },
        429 => UpstreamFailure::Throttled {
            status,
            retry_after: retry_after.and_then(normalize_retry_after),
        },
        // 402 与结构化的配额码才算配额；报文里偶然出现 "quota" 不算。
        402 => UpstreamFailure::Quota { status, snippet },
        400 | 413 | 422 => {
            let structured = code.as_deref().unwrap_or("");
            if structured.contains("context_length")
                || structured.contains("context_window")
                || structured.contains("too_long")
                || status == 413
            {
                UpstreamFailure::ContextLimit { status, snippet }
            } else if structured.contains("quota") || structured.contains("insufficient_quota") {
                UpstreamFailure::Quota { status, snippet }
            } else {
                UpstreamFailure::InvalidRequest { status, snippet }
            }
        }
        408 | 409 | 425 | 500..=599 => UpstreamFailure::Transient { status, snippet },
        _ => UpstreamFailure::InvalidRequest { status, snippet },
    }
}

fn normalize_retry_after(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    // 只接受规范形式；无法解析的值不予采信，而不是猜一个退避时长。
    (value.parse::<u64>().is_ok() || httpdate::parse_http_date(value).is_ok())
        .then(|| value.to_string())
}

/// 截断并去掉换行，避免把大段上游报文或多行细节写进日志/证据。
fn redact(body: &str) -> String {
    let flattened: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = flattened.trim();
    match trimmed.char_indices().nth(MAX_SNIPPET) {
        Some((index, _)) => format!("{}…", &trimmed[..index]),
        None => trimmed.to_string(),
    }
}

/// 经过校验的上游端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    /// 主机名，供连接前的解析检查使用。
    pub host: String,
    pub port: u16,
    /// 是否仍需在连接前做解析检查（显式放行私网时为 false）。
    pub check_resolved: bool,
}

/// 构造并校验目标 URL。路径由协议决定，调用方无权指定。
pub fn resolve_endpoint(upstream: &Upstream, protocol: WireProtocol) -> Result<Endpoint> {
    let Some(base) = upstream.base_url.as_deref() else {
        bail!("upstream `{}` has no baseUrl to build an endpoint from", upstream.id);
    };
    let parsed = reqwest::Url::parse(base)
        .map_err(|e| anyhow::anyhow!("upstream `{}` baseUrl is not a URL: {e}", upstream.id))?;

    // 基址里带凭据、片段或查询串，说明它不是一个干净的基址。
    ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "upstream `{}` baseUrl must not embed credentials",
        upstream.id
    );
    ensure!(
        parsed.fragment().is_none() && parsed.query().is_none(),
        "upstream `{}` baseUrl must not carry a query or fragment",
        upstream.id
    );

    match parsed.scheme() {
        "https" => {}
        "http" => ensure!(
            upstream.allow_private_network,
            "upstream `{}` uses plaintext http; enable allowPrivateNetwork explicitly if this is intended",
            upstream.id
        ),
        other => bail!("upstream `{}` uses unsupported scheme `{other}`", upstream.id),
    }

    // 字面量 IP 当场判定（纯函数，不碰网络）；域名留到连接前由
    // `ensure_public_target` 解析后再查——把 DNS 放进端点构造会让它依赖网络，
    // 一次解析抖动就变成"端点构造失败"，而且无法离线测试。
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("upstream `{}` baseUrl has no host", upstream.id))?;
    if !upstream.allow_private_network
        && let Some(literal) = parse_ip_literal(host)
    {
        ensure_public_address(literal, &upstream.id)?;
    }

    // 路径拼接只发生在这里，且来自协议常量，不含任何调用方输入。
    let url = format!("{}{}", base.trim_end_matches('/'), protocol.path());
    Ok(Endpoint {
        url,
        host: parsed.host_str().unwrap_or_default().to_string(),
        port: parsed.port_or_known_default().unwrap_or(443),
        check_resolved: !upstream.allow_private_network,
    })
}

/// 连接前的解析检查。只对域名有意义——字面量 IP 在构造端点时已经判过。
///
/// 只看字面量是不够的：一个公网域名完全可以解析到 127.0.0.1 或 169.254.169.254。
pub fn ensure_public_target(endpoint: &Endpoint, upstream_id: &str) -> Result<()> {
    if !endpoint.check_resolved {
        return Ok(());
    }
    let addresses: Vec<IpAddr> = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("upstream `{upstream_id}` host does not resolve: {e}"))?
        .map(|addr| addr.ip())
        .collect();
    ensure!(
        !addresses.is_empty(),
        "upstream `{upstream_id}` host resolved to no address"
    );
    for address in addresses {
        ensure_public_address(address, upstream_id)?;
    }
    Ok(())
}

/// 把主机字符串按 IP 字面量解析；不是字面量则返回 `None`（是域名，留待解析后再查）。
///
/// `Url::host_str` 对 IPv6 会保留方括号，标准库的解析器不接受，所以先剥掉。
fn parse_ip_literal(host: &str) -> Option<IpAddr> {
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    bare.parse::<IpAddr>().ok()
}

fn ensure_public_address(address: IpAddr, upstream_id: &str) -> Result<()> {
    ensure!(
        !is_private(address),
        "upstream `{upstream_id}` resolves to the non-public address {address}; \
         set allowPrivateNetwork if this is deliberate"
    );
    Ok(())
}

fn is_private(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                // 100.64.0.0/10 运营商级 NAT，云环境的元数据服务常在此类地址上。
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 唯一本地地址，fe80::/10 链路本地
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4 映射地址要按其内嵌的 v4 地址判断，否则是个绕过口子
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private(IpAddr::V4(v4)))
        }
    }
}

/// 构造出站请求头。
///
/// 只包含协议必需的头与该上游配置的密钥；**不透传客户端任何头部**——
/// 客户端头里可能带别处的凭据、追踪标识或被构造的转发头。
pub fn request_headers(upstream: &Upstream, protocol: WireProtocol) -> Vec<(&'static str, String)> {
    let mut headers = vec![("content-type", "application/json".to_string())];
    if let Some(key) = upstream.api_key.as_deref().filter(|k| !k.is_empty()) {
        match protocol {
            WireProtocol::Anthropic => {
                headers.push(("x-api-key", key.to_string()));
                headers.push(("anthropic-version", "2023-06-01".to_string()));
            }
            WireProtocol::ChatCompletions | WireProtocol::Responses => {
                headers.push(("authorization", format!("Bearer {key}")));
            }
        }
    }
    headers
}

/// 传输层超时。调用方给的 deadline 只能收紧，不能放宽到配置上限之外。
pub fn effective_timeout(configured: Duration, deadline: Option<Duration>) -> Duration {
    match deadline {
        Some(d) => d.min(configured),
        None => configured,
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
