//! 公开模型的上游选择：粘性路由与加权随机。
//!
//! 引擎只做选择，不做准入：候选集由调用方按授权、能力、健康和配额过滤后传入，
//! 引擎不会自己去问预算，也不会因为"这个候选看起来更省钱"而改变结果。
//!
//! # 路由键里有什么、没有什么
//!
//! 键是 `(key_id, public_model, session_id)`。`key_id` 是客户端 Key 的数字标识，
//! **不是密钥本身**；对话内容、鉴权密钥一律不进键。没有可信 session id 时不回退成
//! "按 Key 粘"——那会把同一个 Key 下互不相关的会话绑到同一个上游，而且是一种
//! 用户从未要求过的关联。此时给出明确证据（[`StickyOutcome::NoSessionId`]）。
//!
//! # 什么能让绑定失效
//!
//! 权重变化**不能**——运维调权重不该把正在进行的会话踢走。但禁用、权重归零、被调用方
//! 判定为不合格（预算/能力），以及模式代际变更都**必须**立即失效：前三者意味着该候选
//! 已经不该再被选中，最后一项意味着路由语义本身变了，旧绑定不再代表当前配置。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::RoutingMode;

/// 绑定表容量上限。超出后淘汰最早写入的条目，避免会话无限增长吃满内存。
const MAX_BINDINGS: usize = 65_536;
/// 可信 session id 的长度区间。太短容易碰撞，太长多半不是会话标识。
const MIN_SESSION_LEN: usize = 8;
const MAX_SESSION_LEN: usize = 256;

/// 一次路由请求的上下文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteContext {
    /// 客户端 Key 的数字标识。**不是密钥本身**。
    pub key_id: u64,
    /// 客户端请求的公开模型别名。
    pub public_model: String,
    /// 客户端提供的会话标识；缺失或不可信时为 `None`。
    pub session_id: Option<String>,
    pub needs_tools: bool,
    pub needs_images: bool,
    pub needs_reasoning: bool,
}

impl RouteContext {
    /// 只有长度落在可信区间内的 session id 才用于粘性。
    ///
    /// 不做净化式截断：一个不合规的标识就是不合规，悄悄截断会把两个不同会话截成同一个键。
    fn trusted_session(&self) -> Option<&str> {
        self.session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| (MIN_SESSION_LEN..=MAX_SESSION_LEN).contains(&s.len()))
    }
}

/// 一个已通过调用方准入过滤的候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub binding_id: String,
    pub upstream_id: String,
    /// 数值越小优先级越高。
    pub priority_tier: u32,
    pub weight: u32,
    pub enabled: bool,
    pub supports_tools: bool,
    pub supports_images: bool,
    pub supports_reasoning: bool,
}

/// 候选被排除的原因。空表示可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterReason {
    Disabled,
    ZeroWeight,
    ToolsUnsupported,
    ImagesUnsupported,
    ReasoningUnsupported,
}

/// 粘性判定结果。每一种"没命中"都有独立取值——把它们并成一个 `Miss` 会让运维
/// 无法区分"没有会话标识"和"绑定被禁用"，而这两者的处理方式完全不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StickyOutcome {
    Hit,
    /// 没有可信会话标识。**不回退成按 Key 粘**。
    NoSessionId,
    /// 有会话标识但从未绑定过。
    NoBinding,
    /// 绑定已过期。
    Expired,
    /// 绑定指向的候选已不在可用集合中（禁用、零权重、能力不符或被调用方排除）。
    BindingIneligible,
    /// 路由模式或安全配置变更使旧绑定作废。
    GenerationChanged,
}

/// 一次选择的完整证据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteEvidence {
    pub mode: RoutingMode,
    pub sticky: StickyOutcome,
    /// 本次实际参与选择的优先级组；无可用候选时为 `None`。
    pub group: Option<u32>,
    pub generation: u64,
    pub candidates: Vec<CandidateEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateEvidence {
    pub binding_id: String,
    pub upstream_id: String,
    pub priority_tier: u32,
    pub weight: u32,
    /// 该候选被排除的原因；`None` 表示可用。
    pub filtered: Option<FilterReason>,
    /// 在本次选择中被选中的条件概率（同组内按权重）。不可用或不在所选组内时为 0。
    pub probability: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteSelection {
    pub binding_id: String,
    pub upstream_id: String,
    pub evidence: RouteEvidence,
}

#[derive(Debug, Clone)]
struct BoundRoute {
    binding_id: String,
    generation: u64,
    expires_at: Instant,
}

pub struct RoutingEngine {
    bindings: Mutex<HashMap<String, BoundRoute>>,
    /// 插入顺序，用于容量上限的淘汰。
    order: Mutex<std::collections::VecDeque<String>>,
    generation: AtomicU64,
    ttl: Mutex<Duration>,
}

impl RoutingEngine {
    pub fn new(ttl: Duration) -> Self {
        Self {
            bindings: Mutex::new(HashMap::new()),
            order: Mutex::new(std::collections::VecDeque::new()),
            generation: AtomicU64::new(1),
            ttl: Mutex::new(ttl),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// 作废全部粘性绑定。
    ///
    /// 只应在**路由语义本身变化**时调用（模式切换、影响绑定有效性的安全配置变更）。
    /// 调权重不该走这里：那会把正在进行的会话全部踢走，而权重变化并不意味着旧选择错了。
    pub fn invalidate_bindings(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn set_ttl(&self, ttl: Duration) {
        *self.ttl.lock() = ttl;
    }

    fn route_key(ctx: &RouteContext, session: &str) -> String {
        // 只含 Key 数字标识、模型别名和会话标识；不含密钥，也不含任何对话内容。
        format!("{}\u{1f}{}\u{1f}{}", ctx.key_id, ctx.public_model, session)
    }

    /// 只读预览：展示候选、过滤原因、权重、所选组与条件概率。
    ///
    /// **不产生任何副作用**：不预留预算、不续期粘性、不采样生产随机源、不发网络请求。
    /// 因此它的结果不能当作准入依据——[`Self::select`] 会重新校验运行期有效性。
    pub fn preview(
        &self,
        ctx: &RouteContext,
        candidates: &[Candidate],
        mode: RoutingMode,
    ) -> RouteEvidence {
        let now = Instant::now();
        let (sticky, bound) = self.peek_binding(ctx, candidates, now);
        self.evidence(ctx, candidates, mode, sticky, bound.as_deref())
    }

    /// 实际选择。`ticket` 为 `None` 时使用生产随机源。
    pub fn select(
        &self,
        ctx: &RouteContext,
        candidates: &[Candidate],
        mode: RoutingMode,
        ticket: Option<u64>,
    ) -> Option<RouteSelection> {
        let now = Instant::now();
        let (sticky, bound) = self.peek_binding(ctx, candidates, now);
        let evidence = self.evidence(ctx, candidates, mode, sticky, bound.as_deref());

        let chosen = match (&bound, sticky) {
            (Some(binding_id), StickyOutcome::Hit) => binding_id.clone(),
            _ => {
                let group = evidence.group?;
                let pool: Vec<&Candidate> = candidates
                    .iter()
                    .filter(|c| filter_reason(c, ctx).is_none() && c.priority_tier == group)
                    .collect();
                match mode {
                    // 粘性模式下首次选号与故障转移都取有效权重最高者，
                    // 同分按 binding_id 稳定排序，保证可复现。
                    RoutingMode::Sticky => pool
                        .iter()
                        .max_by(|a, b| {
                            a.weight
                                .cmp(&b.weight)
                                .then_with(|| b.binding_id.cmp(&a.binding_id))
                        })?
                        .binding_id
                        .clone(),
                    RoutingMode::WeightedRandom => weighted_pick(&pool, ticket)?.binding_id.clone(),
                }
            }
        };

        let upstream_id = candidates
            .iter()
            .find(|c| c.binding_id == chosen)?
            .upstream_id
            .clone();
        Some(RouteSelection {
            binding_id: chosen,
            upstream_id,
            evidence,
        })
    }

    /// 上游调用**成功后**才发布绑定。
    ///
    /// 代际不符时丢弃：说明这次请求出发后配置发生了使绑定作废的变更，把它写回去等于
    /// 用过期语义覆盖新语义。失败的尝试也绝不写入，否则会把坏上游钉死在会话上。
    pub fn bind_success(&self, ctx: &RouteContext, binding_id: &str, generation: u64) -> bool {
        if generation != self.generation() {
            return false;
        }
        let Some(session) = ctx.trusted_session() else {
            return false;
        };
        let key = Self::route_key(ctx, session);
        let ttl = *self.ttl.lock();
        let mut bindings = self.bindings.lock();
        let mut order = self.order.lock();
        if !bindings.contains_key(&key) {
            // 有界容量：先进先出淘汰，避免会话无限增长。
            while order.len() >= MAX_BINDINGS {
                if let Some(evicted) = order.pop_front() {
                    bindings.remove(&evicted);
                } else {
                    break;
                }
            }
            order.push_back(key.clone());
        }
        bindings.insert(
            key,
            BoundRoute {
                binding_id: binding_id.to_string(),
                generation,
                expires_at: Instant::now() + ttl,
            },
        );
        true
    }

    /// 某个 binding 不再可用时，清掉所有指向它的绑定。
    pub fn mark_unavailable(&self, binding_id: &str) -> usize {
        let mut bindings = self.bindings.lock();
        let before = bindings.len();
        bindings.retain(|_, bound| bound.binding_id != binding_id);
        before - bindings.len()
    }

    /// 读取当前绑定并判定其是否仍然有效。只读，不续期。
    fn peek_binding(
        &self,
        ctx: &RouteContext,
        candidates: &[Candidate],
        now: Instant,
    ) -> (StickyOutcome, Option<String>) {
        let Some(session) = ctx.trusted_session() else {
            return (StickyOutcome::NoSessionId, None);
        };
        let key = Self::route_key(ctx, session);
        let bindings = self.bindings.lock();
        let Some(bound) = bindings.get(&key) else {
            return (StickyOutcome::NoBinding, None);
        };
        if bound.generation != self.generation() {
            return (
                StickyOutcome::GenerationChanged,
                Some(bound.binding_id.clone()),
            );
        }
        if bound.expires_at <= now {
            return (StickyOutcome::Expired, Some(bound.binding_id.clone()));
        }
        // 权重变化不影响有效性；禁用、零权重、能力不符或被调用方排除则立即失效。
        let still_eligible = candidates
            .iter()
            .any(|c| c.binding_id == bound.binding_id && filter_reason(c, ctx).is_none());
        if !still_eligible {
            return (
                StickyOutcome::BindingIneligible,
                Some(bound.binding_id.clone()),
            );
        }
        (StickyOutcome::Hit, Some(bound.binding_id.clone()))
    }

    fn evidence(
        &self,
        ctx: &RouteContext,
        candidates: &[Candidate],
        mode: RoutingMode,
        sticky: StickyOutcome,
        bound: Option<&str>,
    ) -> RouteEvidence {
        // 粘性命中时不存在"组内竞争"，所选组即绑定所在的组。
        let group = if sticky == StickyOutcome::Hit {
            bound.and_then(|id| {
                candidates
                    .iter()
                    .find(|c| c.binding_id == id)
                    .map(|c| c.priority_tier)
            })
        } else {
            candidates
                .iter()
                .filter(|c| filter_reason(c, ctx).is_none())
                .map(|c| c.priority_tier)
                .min()
        };

        // 同组内权重总和，用于条件概率；求和有界，避免溢出。
        let group_weight: u64 = group
            .map(|g| {
                candidates
                    .iter()
                    .filter(|c| filter_reason(c, ctx).is_none() && c.priority_tier == g)
                    .map(|c| u64::from(c.weight))
                    .sum()
            })
            .unwrap_or(0);

        let candidates = candidates
            .iter()
            .map(|c| {
                let filtered = filter_reason(c, ctx);
                let probability = match (filtered, group, sticky) {
                    // 粘性命中时结果是确定的：命中者概率 1，其余 0。
                    (None, Some(g), StickyOutcome::Hit) if c.priority_tier == g => {
                        if bound == Some(c.binding_id.as_str()) {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    (None, Some(g), _) if c.priority_tier == g && group_weight > 0 => {
                        match mode {
                            RoutingMode::WeightedRandom => {
                                f64::from(c.weight) / group_weight as f64
                            }
                            // 粘性模式的首次选择是确定的（最高权重），不是抽样。
                            RoutingMode::Sticky => 0.0,
                        }
                    }
                    _ => 0.0,
                };
                CandidateEvidence {
                    binding_id: c.binding_id.clone(),
                    upstream_id: c.upstream_id.clone(),
                    priority_tier: c.priority_tier,
                    weight: c.weight,
                    filtered,
                    probability,
                }
            })
            .collect();

        RouteEvidence {
            mode,
            sticky,
            group,
            generation: self.generation(),
            candidates,
        }
    }
}

/// 候选是否被排除。顺序固定，保证同一候选总是报同一个原因。
fn filter_reason(candidate: &Candidate, ctx: &RouteContext) -> Option<FilterReason> {
    if !candidate.enabled {
        return Some(FilterReason::Disabled);
    }
    if candidate.weight == 0 {
        return Some(FilterReason::ZeroWeight);
    }
    if ctx.needs_tools && !candidate.supports_tools {
        return Some(FilterReason::ToolsUnsupported);
    }
    if ctx.needs_images && !candidate.supports_images {
        return Some(FilterReason::ImagesUnsupported);
    }
    if ctx.needs_reasoning && !candidate.supports_reasoning {
        return Some(FilterReason::ReasoningUnsupported);
    }
    None
}

/// 按权重抽取。票号落在 `[0, 总权重)`，累积区间选择。
///
/// 候选按 `binding_id` 排序后再累积，使同一票号在同一候选集合上总是给出同一结果——
/// 否则候选顺序的偶然变化会让"可复现"这件事失效。
fn weighted_pick<'a>(pool: &[&'a Candidate], ticket: Option<u64>) -> Option<&'a Candidate> {
    let total: u64 = pool.iter().map(|c| u64::from(c.weight)).sum();
    if total == 0 {
        return None;
    }
    let mut sorted: Vec<&Candidate> = pool.to_vec();
    sorted.sort_by(|a, b| a.binding_id.cmp(&b.binding_id));
    let ticket = match ticket {
        Some(t) => t % total,
        None => fastrand::u64(0..total),
    };
    let mut cumulative = 0_u64;
    for candidate in sorted {
        cumulative += u64::from(candidate.weight);
        if ticket < cumulative {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
#[path = "routing_tests.rs"]
mod tests;
