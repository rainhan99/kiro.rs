//! Kiro 表达不了的东西，统一在这里取舍。
//!
//! # 拒绝并不能把内容保住
//!
//! 转换器对它不认识的东西**本来就是静默跳过的**：角色不是 `user`/`assistant` 的
//! 整条消息被 `if user / else if assistant` 漏掉，未知内容块也不会出现在送出的
//! 请求里。所以取舍从来不是「保住还是丢掉」，而是：
//!
//! | | 那部分内容 | 请求 |
//! |---|---|---|
//! | [`UnexpressibleStrategy::PortableText`] | 历史上的不兼容内容转为可携带的引用文本；当前的不兼容内容 | 成功或失败 |
//! | [`UnexpressibleStrategy::Refuse`] | 用不上 | 失败 |
//! | [`UnexpressibleStrategy::Drop`] | 用不上 | 成功，且**逐项记明丢了什么** |
//!
//! 0.9.0 做的是第二行的前半句：丢了，但没有后半句，没人知道丢了什么。0.9.1 改成
//! 第一行，于是一批本来能跑的请求开始失败——而拒绝并没有把任何东西保住。
//!
//! 这里把后半句补上，并让两种处理都**指名道姓**：实际收到的角色是什么、内容块
//! 类型是什么。此前只说"角色必须是 user 或 assistant"，不说收到的是什么，排查
//! 时只能靠猜。
//!
//! # 为什么是一个设置而不是一串
//!
//! prefill、未知角色、未知内容块、URL 图片、assistant 图片——它们是同一件事的
//! 不同实例：Kiro 表达不了。给每一种单配一个开关，等于把同一个判断切成 N 份，
//! 下一个形状出现时又要加第 N+1 个。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::anthropic::types::MessagesRequest;

/// 遇到 Kiro 表达不了的内容时怎么办。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnexpressibleStrategy {
    /// 历史上的不兼容内容变成带来源的可携带文本；当前的不兼容内容使请求失败。
    #[default]
    PortableText,
    /// 拒绝整个请求，并说明是哪一项、实际收到的是什么。
    Refuse,
    /// 丢弃那一部分并逐项记录，请求继续。0.9.0 及更早的实际行为（但那时没有记录）。
    ///
    /// 这是有意造成信息损失的兼容模式，已弃用；请优先使用 [`Self::PortableText`]。
    Drop,
}

/// 一次为了让请求可被 Kiro 表达而做的删除。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Removal {
    /// 位置，如 `messages[3].content[1]`。
    pub path: String,
    /// 原因，**含实际看到的值**（类型名、角色名），不含任何内容文本。
    pub reason: String,
}

impl std::fmt::Display for Removal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

/// 让请求变成 Kiro 能表达的形状。
///
/// 返回做过的删除；`Refuse` 下遇到第一项就报错，且**不改动请求**。
pub fn make_expressible(
    payload: &mut MessagesRequest,
    strategy: UnexpressibleStrategy,
) -> anyhow::Result<Vec<Removal>> {
    let planned = plan(payload);
    if let Some(first) = planned.first()
        && strategy == UnexpressibleStrategy::Refuse
    {
        anyhow::bail!(
            "Kiro cannot express this request: {first}{}. \
             Roles received: [{}]. Set requestPipeline.unexpressible to \"drop\" to \
             drop the unexpressible parts and continue (each one is recorded).",
            if planned.len() > 1 {
                format!(" (and {} more)", planned.len() - 1)
            } else {
                String::new()
            },
            role_sequence(payload)
        );
    }
    apply(payload, &planned);
    Ok(planned)
}

/// 消息的角色序列，用于诊断。**只有 role**，长序列中段折叠。
pub fn role_sequence(payload: &MessagesRequest) -> String {
    const EDGE: usize = 4;
    let roles: Vec<&str> = payload.messages.iter().map(|m| m.role.as_str()).collect();
    if roles.len() <= EDGE * 2 + 1 {
        return roles.join(", ");
    }
    format!(
        "{}, …{} more…, {}",
        roles[..EDGE].join(", "),
        roles.len() - EDGE * 2,
        roles[roles.len() - EDGE..].join(", ")
    )
}

/// 要删除的位置，按**从后往前**排好，便于原地删除时下标不失效。
fn plan(payload: &MessagesRequest) -> Vec<Removal> {
    let mut out = Vec::new();
    for (mi, message) in payload.messages.iter().enumerate() {
        // 角色不是 user/assistant：转换器会整条跳过，所以整条都留不住。
        if !matches!(message.role.as_str(), "user" | "assistant") {
            out.push(Removal {
                path: format!("messages[{mi}]"),
                reason: format!(
                    "role {:?} is not one of user/assistant; the Kiro adapter can only carry those two",
                    message.role
                ),
            });
            continue;
        }
        let Some(blocks) = message.content.as_array() else {
            // 字符串内容一律可表达；其它标量形状留给下游按原样处理。
            continue;
        };
        for (bi, block) in blocks.iter().enumerate() {
            if let Some(reason) = block_problem(block, &message.role) {
                out.push(Removal {
                    path: format!("messages[{mi}].content[{bi}]"),
                    reason,
                });
            }
        }
    }
    plan_trailing_prefill(payload, &mut out);
    out
}

/// 末尾的 assistant 轮次（prefill）。
///
/// Kiro 只能从一条 user 轮次往下生成，末尾的 assistant 开头它用不上——放进去也不会
/// 被续写。所以它同样属于「表达不了」，不该单独一个开关。
///
/// 判定要在上面那些删除**生效之后**的形状上做：一条内容块被删光的消息本身也不再
/// 算数，否则会把它当成有效的末尾轮次。
fn plan_trailing_prefill(payload: &MessagesRequest, out: &mut Vec<Removal>) {
    let dropped: std::collections::HashSet<usize> = out
        .iter()
        .filter(|r| !r.path.contains(".content["))
        .map(|r| parse_path(&r.path).0)
        .collect();
    let mut emptied: std::collections::HashMap<usize, usize> = Default::default();
    for removal in out.iter().filter(|r| r.path.contains(".content[")) {
        *emptied.entry(parse_path(&removal.path).0).or_default() += 1;
    }

    for (mi, message) in payload.messages.iter().enumerate().rev() {
        if dropped.contains(&mi) {
            continue;
        }
        if let Some(blocks) = message.content.as_array()
            && emptied.get(&mi).copied().unwrap_or(0) >= blocks.len()
        {
            continue;
        }
        if message.role == "user" {
            return;
        }
        out.push(Removal {
            path: format!("messages[{mi}]"),
            reason: format!(
                "trailing {} turn (assistant prefill); Kiro generates only from a final user turn",
                message.role
            ),
        });
    }
}

/// 这个块能不能被 Kiro 表达；不能则给出**含实际值**的原因。
fn block_problem(block: &Value, role: &str) -> Option<String> {
    let Some(kind) = block.get("type").and_then(Value::as_str) else {
        return Some("content block has no type".into());
    };
    // 按类型的具体判断排在**通用解析检查之前**：先跑通用检查的话，一个 URL 图片
    // 只会得到"这个 image 块解析不了"，而真正有用的是"source 是 url，Kiro 只收
    // 内联 base64"。说不清楚的错误正是这轮要治的毛病。
    let specific = match kind {
        "text" => (!block.get("text").is_some_and(Value::is_string))
            .then(|| "text block has no text string".to_string()),
        "image" => image_problem(block, role),
        "tool_result" if role == "user" => {
            (!block.get("tool_use_id").is_some_and(Value::is_string))
                .then(|| "tool_result has no tool_use_id".to_string())
        }
        "thinking" if role == "assistant" => (!block.get("thinking").is_some_and(Value::is_string))
            .then(|| "thinking block has no string".to_string()),
        "tool_use" if role == "assistant" => (!(block.get("id").is_some_and(Value::is_string)
            && block.get("name").is_some_and(Value::is_string)))
        .then(|| "tool_use has no id or no name".to_string()),
        other => Some(format!(
            "block type {other:?} is not expressible on a {role} turn"
        )),
    };
    if specific.is_some() {
        return specific;
    }
    // 类型认识、字段也对，但整体反序列化不过：形状里还有别的毛病。
    (serde_json::from_value::<crate::anthropic::types::ContentBlock>(block.clone()).is_err())
        .then(|| format!("{kind:?} block does not parse as a known content block"))
}

fn image_problem(block: &Value, role: &str) -> Option<String> {
    if role != "user" {
        return Some("Kiro carries images only on user turns".into());
    }
    let Some(source) = block.get("source") else {
        return Some("image has no source".into());
    };
    let source_type = source
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("<missing>");
    if source_type != "base64" || !source.get("data").is_some_and(Value::is_string) {
        return Some(format!(
            "image source is {source_type:?}; Kiro takes only inline base64"
        ));
    }
    let media = source
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("<missing>");
    if !matches!(
        media,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) {
        return Some(format!(
            "image media type {media:?} is not one Kiro accepts"
        ));
    }
    None
}

/// 就地删除。按下标从大到小执行，避免删前面的把后面的下标顶掉。
fn apply(payload: &mut MessagesRequest, removals: &[Removal]) {
    if removals.is_empty() {
        return;
    }
    let mut message_drops: Vec<usize> = Vec::new();
    let mut block_drops: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
    for removal in removals {
        match parse_path(&removal.path) {
            (mi, None) => message_drops.push(mi),
            (mi, Some(bi)) => block_drops.entry(mi).or_default().push(bi),
        }
    }
    for (mi, mut indices) in block_drops {
        let Some(message) = payload.messages.get_mut(mi) else {
            continue;
        };
        let Some(blocks) = message.content.as_array_mut() else {
            continue;
        };
        indices.sort_unstable_by(|a, b| b.cmp(a));
        for bi in indices {
            if bi < blocks.len() {
                blocks.remove(bi);
            }
        }
    }
    message_drops.sort_unstable_by(|a, b| b.cmp(a));
    for mi in message_drops {
        if mi < payload.messages.len() {
            payload.messages.remove(mi);
        }
    }
    // 块全被删光的消息留着只会变成一条空轮次；一并去掉并保持顺序。
    payload
        .messages
        .retain(|m| !m.content.as_array().is_some_and(|b| b.is_empty()));
}

fn parse_path(path: &str) -> (usize, Option<usize>) {
    let digits = |s: &str| -> usize {
        s.chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    };
    match path.split_once(".content[") {
        Some((head, tail)) => (digits(head), Some(digits(tail))),
        None => (digits(path), None),
    }
}

#[cfg(test)]
#[path = "expressible_tests.rs"]
mod tests;
