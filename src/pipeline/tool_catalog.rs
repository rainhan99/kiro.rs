//! 按需工具发现（分页目录）。
//!
//! 客户端声明的工具 schema 过多时，不再一次性全发：改为提供一份**分页目录**（只含
//! 名称与描述）和一个揭示接口，由模型自己列目录、索取所需 schema，被索取的工具在
//! 后续轮次才真正声明给上游，从而可被调用。
//!
//! **可达性无损**：没有任何工具被移除，网关也不替模型决定哪些工具重要。真实代价只有
//! 两条，文档里照写不打折扣：
//! 1. 选工具时看到的是名称与描述，而不是完整 schema；
//! 2. 够到一个工具需要多花轮次。
//!
//! 目录直接取自客户端自己的声明：名称与描述原样透传，**不摘要、不改写、不按相关性
//! 排序**——一个按自己猜测的相关性给工具列表重排序的网关，正是在做它声称不做的那个
//! 选择。分页稳定且完整：沿 `next_offset` 走到 null，恰好得到全集，每项一次。

use anyhow::{Result, ensure};
use serde_json::{Value, json};

use crate::anthropic::types::Tool;

pub const LIST_TOOL: &str = "kiro_tool_catalog_list";
pub const REVEAL_TOOL: &str = "kiro_tool_schema_read";

/// 单页最多返回的条目数上限（请求可要更少）。
const MAX_PAGE: usize = 64;
/// 一次揭示最多的工具数。
const MAX_REVEAL: usize = 16;

pub fn is_catalog_tool(name: &str) -> bool {
    matches!(name, LIST_TOOL | REVEAL_TOOL)
}

/// 是否应当对这批工具启用按需发现。
///
/// 判据是**序列化后的 schema 字节数**而不是工具个数：真正撑爆请求的是 schema 体积，
/// 少数几个巨型 schema 和大量小工具都应当被覆盖到。
pub fn should_paginate(tools: &[Tool], budget_bytes: usize) -> bool {
    declared_bytes(tools) > budget_bytes
}

pub fn declared_bytes(tools: &[Tool]) -> usize {
    tools
        .iter()
        .map(|tool| serde_json::to_string(tool).map(|s| s.len()).unwrap_or(0))
        .sum()
}

/// 目录与揭示两个内部工具的定义。
pub fn catalog_tools(total: usize) -> Vec<Tool> {
    let list = json!({"type":"object","additionalProperties":false,"properties":{
        "offset":{"type":"integer","minimum":0,"description":"Start index; continue with next_offset until it is null"},
        "limit":{"type":"integer","minimum":1,"maximum":MAX_PAGE,"description":"Maximum entries in this page"}}});
    let reveal = json!({"type":"object","additionalProperties":false,"required":["names"],"properties":{
        "names":{"type":"array","minItems":1,"maxItems":MAX_REVEAL,"items":{"type":"string"},
                 "description":"Exact tool names from the catalog"}}});
    [
        (
            LIST_TOOL,
            format!(
                "List the {total} tools this client declared, as names and descriptions, in the order the client declared them. This gateway does not reorder them by relevance or summarize them. Paginate with offset and limit and continue until next_offset is null. Their input schemas are not included here; call {REVEAL_TOOL} for the ones you intend to use."
            ),
            list,
        ),
        (
            REVEAL_TOOL,
            format!(
                "Return the exact input schemas for the named tools and declare them so they become callable in the following turns. Names must come from {LIST_TOOL}; an unknown name is an error rather than a silent omission. A revealed tool is declared exactly as the client declared it."
            ),
            reveal,
        ),
    ]
    .into_iter()
    .map(|(name, description, schema)| Tool {
        tool_type: None,
        name: name.into(),
        description,
        input_schema: serde_json::from_value(schema).expect("static catalog tool schema"),
        max_uses: None,
        cache_control: None,
    })
    .collect()
}

/// 执行目录列举。
pub fn list(tools: &[Tool], input: &Value) -> Result<Value> {
    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(MAX_PAGE)
        .clamp(1, MAX_PAGE);
    ensure!(offset <= tools.len(), "catalog offset is past the end");
    let end = tools.len().min(offset + limit);
    // 原样透传，保持客户端声明顺序；不排序、不改写、不摘要。
    let entries: Vec<Value> = tools[offset..end]
        .iter()
        .map(|tool| json!({"name": tool.name, "description": tool.description}))
        .collect();
    Ok(json!({
        "total": tools.len(),
        "entries": entries,
        "next_offset": (end < tools.len()).then_some(end),
    }))
}

/// 执行 schema 揭示，返回 (给模型的结果, 需要在后续轮次声明的工具名)。
pub fn reveal(tools: &[Tool], input: &Value) -> Result<(Value, Vec<String>)> {
    let names = input
        .get("names")
        .and_then(Value::as_array)
        .context_names()?;
    ensure!(!names.is_empty(), "names must not be empty");
    ensure!(
        names.len() <= MAX_REVEAL,
        "at most {MAX_REVEAL} tools per reveal"
    );
    let mut schemas = Vec::new();
    let mut revealed = Vec::new();
    for name in &names {
        // 未知名字必须报错：静默省略会让模型以为某个工具不存在，而它其实存在。
        let tool = tools
            .iter()
            .find(|tool| &tool.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool name: {name}"))?;
        schemas.push(json!({
            "name": tool.name,
            "description": tool.description,
            "input_schema": tool.input_schema,
        }));
        revealed.push(tool.name.clone());
    }
    Ok((json!({"tools": schemas}), revealed))
}

trait ContextNames {
    fn context_names(self) -> Result<Vec<String>>;
}
impl ContextNames for Option<&Vec<Value>> {
    fn context_names(self) -> Result<Vec<String>> {
        let items = self.ok_or_else(|| anyhow::anyhow!("names must be an array of strings"))?;
        items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("names must be an array of strings"))
            })
            .collect()
    }
}

/// 一次请求内的按需发现状态。
///
/// 持有客户端的原始声明与已揭示的工具名。**已揭示的工具原样声明**给上游，网关不做
/// 任何改写；未揭示的工具没有消失，随时可以再列目录、再索取。
pub struct CatalogSession {
    declared: Vec<Tool>,
    revealed: Vec<String>,
}

impl CatalogSession {
    pub fn new(declared: Vec<Tool>) -> Self {
        Self {
            declared,
            revealed: Vec::new(),
        }
    }

    pub fn total(&self) -> usize {
        self.declared.len()
    }

    /// 仅供测试断言使用；运行期没有消费者，所以不留在正式接口上。
    #[cfg(test)]
    pub fn revealed_count(&self) -> usize {
        self.revealed.len()
    }

    /// 本轮应当声明给上游的工具：两个目录工具 + 已揭示的客户端工具（原样）。
    pub fn active_tools(&self) -> Vec<Tool> {
        let mut tools = catalog_tools(self.declared.len());
        tools.extend(
            self.declared
                .iter()
                .filter(|tool| self.revealed.iter().any(|name| name == &tool.name))
                .cloned(),
        );
        tools
    }

    /// 执行目录工具。揭示会更新本会话状态，从而改变后续轮次的声明集合。
    pub fn execute(&mut self, name: &str, input: &Value) -> Result<Value> {
        match name {
            LIST_TOOL => list(&self.declared, input),
            REVEAL_TOOL => {
                let (value, revealed) = reveal(&self.declared, input)?;
                for name in revealed {
                    if !self.revealed.contains(&name) {
                        self.revealed.push(name);
                    }
                }
                Ok(value)
            }
            other => anyhow::bail!("not a catalog tool: {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tool(name: &str) -> Tool {
        let mut schema = BTreeMap::new();
        schema.insert("type".to_string(), json!("object"));
        schema.insert(
            "properties".to_string(),
            json!({"path": {"type": "string", "description": format!("path for {name}")}}),
        );
        Tool {
            tool_type: None,
            name: name.into(),
            description: format!("description of {name}"),
            input_schema: schema,
            max_uses: None,
            cache_control: None,
        }
    }

    fn tools(count: usize) -> Vec<Tool> {
        (0..count).map(|i| tool(&format!("tool_{i}"))).collect()
    }

    /// 沿 next_offset 走到 null，必须恰好得到全集，每项一次，且保持声明顺序。
    #[test]
    fn pagination_is_complete_stable_and_in_declared_order() {
        let all = tools(150);
        let mut seen = Vec::new();
        let mut offset = Some(0_usize);
        while let Some(current) = offset {
            let page = list(&all, &json!({"offset": current, "limit": 7})).unwrap();
            for entry in page["entries"].as_array().unwrap() {
                seen.push(entry["name"].as_str().unwrap().to_string());
            }
            offset = page["next_offset"].as_u64().map(|v| v as usize);
        }
        let expected: Vec<String> = all.iter().map(|t| t.name.clone()).collect();
        assert_eq!(seen, expected, "分页必须完整、不重不漏且保持声明顺序");
    }

    /// 名称与描述原样透传：不摘要、不改写、不按相关性重排。
    #[test]
    fn names_and_descriptions_pass_through_unchanged() {
        let all = vec![tool("zzz_last"), tool("aaa_first")];
        let page = list(&all, &json!({})).unwrap();
        let entries = page["entries"].as_array().unwrap();
        assert_eq!(entries[0]["name"], "zzz_last", "不得按字母或相关性排序");
        assert_eq!(entries[0]["description"], "description of zzz_last");
        assert_eq!(entries[1]["name"], "aaa_first");
    }

    /// 揭示返回的 schema 必须与客户端声明逐字一致。
    #[test]
    fn revealed_schema_is_identical_to_the_declaration() {
        let all = tools(3);
        let (value, revealed) = reveal(&all, &json!({"names": ["tool_1"]})).unwrap();
        assert_eq!(revealed, vec!["tool_1".to_string()]);
        let returned = &value["tools"][0];
        assert_eq!(returned["name"], "tool_1");
        assert_eq!(returned["description"], all[1].description);
        assert_eq!(
            serde_json::to_value(&all[1].input_schema).unwrap(),
            returned["input_schema"],
            "schema 必须逐字一致，不得被网关改写"
        );
    }

    /// 未知名字必须报错：静默省略会让模型以为工具不存在，而它其实存在。
    #[test]
    fn unknown_name_is_an_error_not_a_silent_omission() {
        let all = tools(3);
        let err = reveal(&all, &json!({"names": ["tool_1", "nope"]})).unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn reveal_and_page_sizes_are_bounded() {
        let all = tools(300);
        let names: Vec<String> = (0..MAX_REVEAL + 1).map(|i| format!("tool_{i}")).collect();
        assert!(reveal(&all, &json!({"names": names})).is_err());
        assert!(reveal(&all, &json!({"names": []})).is_err());
        let page = list(&all, &json!({"limit": 10_000})).unwrap();
        assert_eq!(page["entries"].as_array().unwrap().len(), MAX_PAGE);
        assert!(list(&all, &json!({"offset": 9_999})).is_err());
    }

    /// 揭示后，该工具必须出现在后续轮次的声明集合里，且与客户端声明逐字一致；
    /// 未揭示的工具不出现，但并未消失。
    #[test]
    fn revealed_tools_become_declared_in_later_rounds() {
        let all = tools(40);
        let mut session = CatalogSession::new(all.clone());
        let active = session.active_tools();
        assert_eq!(active.len(), 2, "起初只声明两个目录工具");
        assert!(active.iter().all(|tool| is_catalog_tool(&tool.name)));

        session
            .execute(REVEAL_TOOL, &json!({"names": ["tool_7", "tool_3"]}))
            .unwrap();
        let active = session.active_tools();
        assert_eq!(active.len(), 4);
        let revealed: Vec<&str> = active
            .iter()
            .filter(|t| !is_catalog_tool(&t.name))
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(revealed, vec!["tool_3", "tool_7"], "按客户端声明顺序");
        let declared_seven = all.iter().find(|t| t.name == "tool_7").unwrap();
        let active_seven = active.iter().find(|t| t.name == "tool_7").unwrap();
        assert_eq!(
            serde_json::to_value(declared_seven).unwrap(),
            serde_json::to_value(active_seven).unwrap(),
            "揭示后的工具必须与客户端声明逐字一致"
        );
    }

    /// 重复揭示不重复声明。
    #[test]
    fn revealing_twice_does_not_duplicate() {
        let mut session = CatalogSession::new(tools(5));
        for _ in 0..3 {
            session
                .execute(REVEAL_TOOL, &json!({"names": ["tool_1"]}))
                .unwrap();
        }
        assert_eq!(session.revealed_count(), 1);
        assert_eq!(session.active_tools().len(), 3);
    }

    /// 目录工具名必须短到不会被转换器的超长缩短逻辑改写——一旦被改名，
    /// 循环就认不出它们、无法本地执行，模型会拿到一个永远不被响应的工具。
    #[test]
    fn catalog_tool_names_survive_the_converter_untouched() {
        for name in [LIST_TOOL, REVEAL_TOOL] {
            assert!(
                name.len() <= 63,
                "{name} 超过缩短阈值，会被改名并导致循环认不出它"
            );
            assert_eq!(
                crate::anthropic::converter::map_tool_name_for_test(name),
                name
            );
        }
    }

    /// 预算以 schema 字节计，而非工具个数。
    #[test]
    fn budget_is_measured_in_declared_bytes() {
        let few = tools(2);
        let many = tools(200);
        assert!(!should_paginate(&few, declared_bytes(&few)));
        assert!(should_paginate(&many, declared_bytes(&few)));
        assert!(!should_paginate(&many, usize::MAX));
    }
}
