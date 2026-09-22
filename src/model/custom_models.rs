//! 自定义模型全局注册表
//!
//! 启动时由 [`init`] 从 `config.custom_models` 装载，运行期可通过 Admin API
//! 热更新（`RwLock` 写锁替换）。仿 `crate::token` / `crate::kiro::machine_id`
//! 的全局单例惯例，避免把配置表逐层透传进 `map_model` /
//! `get_context_window_size` 等无状态自由函数。
//!
//! 匹配规则：按模型 `id` 大小写不敏感精确匹配；找不到时自动剥离 `-thinking`
//! 后缀再试一次（与内置 `map_model` 对 thinking 变体的处理保持一致）。

use std::collections::HashMap;
use std::sync::RwLock;

use super::config::CustomModel;

/// 自定义模型注册表：保序原始列表（供 `/v1/models` 展示）+ 小写 id 索引（供查询）。
struct CustomModelRegistry {
    /// 原始顺序的完整列表。
    ordered: Vec<CustomModel>,
    /// 小写 id -> `ordered` 下标。
    by_id: HashMap<String, usize>,
}

static REGISTRY: RwLock<Option<CustomModelRegistry>> = RwLock::new(None);

/// 初始化 / 热更新自定义模型注册表。
///
/// 启动时调用一次，运行期可通过 Admin API 再次调用以热更新。后一条同名 id
/// 覆盖前一条的索引，但两者都保留在 `ordered` 中（`/v1/models` 会同时展示）。
pub fn init(models: Vec<CustomModel>) {
    let mut by_id = HashMap::with_capacity(models.len());
    for (idx, m) in models.iter().enumerate() {
        by_id.insert(m.id.to_ascii_lowercase(), idx);
    }
    if let Ok(mut reg) = REGISTRY.write() {
        *reg = Some(CustomModelRegistry {
            ordered: models,
            by_id,
        });
    }
}

/// 按模型名查找自定义模型定义。
///
/// 先按大小写不敏感精确匹配；未命中且名字带 `-thinking` 后缀时，剥离后再试一次。
pub fn lookup(model: &str) -> Option<CustomModel> {
    let reg = REGISTRY.read().ok()?;
    let registry = reg.as_ref()?;
    let key = model.to_ascii_lowercase();
    let idx = registry.by_id.get(&key).copied().or_else(|| {
        key.strip_suffix("-thinking")
            .and_then(|stripped| registry.by_id.get(stripped))
            .copied()
    })?;
    registry.ordered.get(idx).cloned()
}

/// 返回所有已注册的自定义模型（保持配置文件中的原始顺序）。
pub fn all() -> Vec<CustomModel> {
    REGISTRY
        .read()
        .ok()
        .and_then(|r| r.as_ref().map(|reg| reg.ordered.clone()))
        .unwrap_or_default()
}

/// 是否存在 `backend_id` 等于给定值且声明支持 reasoning 的自定义模型。
///
/// `map_model` 把别名映射成 backend_id 后，`model_supports_native_reasoning`
/// 拿到的是 backend_id；这里按 backend_id 反查，让自定义模型能声明 reasoning 能力。
pub fn backend_supports_reasoning(backend_id: &str) -> bool {
    REGISTRY
        .read()
        .ok()
        .and_then(|r| {
            r.as_ref().map(|reg| {
                reg.ordered
                    .iter()
                    .any(|m| m.backend_id == backend_id && m.supports_reasoning == Some(true))
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str, backend: &str) -> CustomModel {
        CustomModel {
            id: id.to_string(),
            backend_id: backend.to_string(),
            display_name: None,
            context_window: None,
            max_tokens: None,
            supports_reasoning: None,
            owned_by: None,
        }
    }

    // 注册表是进程级 OnceLock，无法在多测试间反复 init；这里直接构造 registry
    // 实例，单元测试其查询逻辑（lookup / thinking 后缀剥离）。
    fn build(models: Vec<CustomModel>) -> CustomModelRegistry {
        let mut by_id = HashMap::new();
        for (idx, m) in models.iter().enumerate() {
            by_id.insert(m.id.to_ascii_lowercase(), idx);
        }
        CustomModelRegistry {
            ordered: models,
            by_id,
        }
    }

    fn lookup_in<'a>(reg: &'a CustomModelRegistry, model: &str) -> Option<&'a CustomModel> {
        let key = model.to_ascii_lowercase();
        if let Some(&idx) = reg.by_id.get(&key) {
            return reg.ordered.get(idx);
        }
        if let Some(stripped) = key.strip_suffix("-thinking") {
            if let Some(&idx) = reg.by_id.get(stripped) {
                return reg.ordered.get(idx);
            }
        }
        None
    }

    #[test]
    fn test_lookup_case_insensitive() {
        let reg = build(vec![sample("My-Opus", "claude-opus-4.8")]);
        assert_eq!(
            lookup_in(&reg, "my-opus").map(|m| m.backend_id.as_str()),
            Some("claude-opus-4.8")
        );
        assert_eq!(
            lookup_in(&reg, "MY-OPUS").map(|m| m.backend_id.as_str()),
            Some("claude-opus-4.8")
        );
    }

    #[test]
    fn test_lookup_strips_thinking_suffix() {
        let reg = build(vec![sample("my-opus", "claude-opus-4.8")]);
        assert_eq!(
            lookup_in(&reg, "my-opus-thinking").map(|m| m.backend_id.as_str()),
            Some("claude-opus-4.8")
        );
    }

    #[test]
    fn test_lookup_miss() {
        let reg = build(vec![sample("my-opus", "claude-opus-4.8")]);
        assert!(lookup_in(&reg, "unknown-model").is_none());
    }

    #[test]
    fn test_explicit_thinking_alias_wins_over_strip() {
        // 同时存在 my-opus 与 my-opus-thinking 时，精确匹配优先，不走剥离回退。
        let reg = build(vec![
            sample("my-opus", "claude-opus-4.8"),
            sample("my-opus-thinking", "claude-opus-4.7"),
        ]);
        assert_eq!(
            lookup_in(&reg, "my-opus-thinking").map(|m| m.backend_id.as_str()),
            Some("claude-opus-4.7")
        );
    }
}
