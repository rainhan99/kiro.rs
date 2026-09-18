use serde::{Deserialize, Serialize};

/// 末尾出现 assistant 消息（prefill）时怎么办。
///
/// # 拒绝并不能把 prefill 保住
///
/// Kiro 不支持 assistant prefill，**两种处理下它都用不上**。所以取舍不是
/// "保住内容还是丢掉内容"，而是：
///
/// | | prefill | 请求 |
/// |---|---|---|
/// | `Refuse` | 用不上 | 失败 |
/// | `Drop` | 用不上 | 成功，并在日志与 trace 里留痕 |
///
/// 主流客户端会拿 assistant prefill 约束小工具调用的输出格式（例如用两条消息、
/// 64 个 token 生成一个标题）。默认拒绝会让这类客户端陷进固定间隔的重试循环，
/// 而换来的并不是"内容被保住"。所以默认是 `Drop`。
///
/// 丢弃**不是静默的**：日志会记，trace 也会记。要严格拒绝的把它改成 `Refuse`。
///
/// 这个选择与 `mode` **正交**——关掉预算强制不等于同意改变 prefill 的处理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrefillStrategy {
    /// 截断到最后一条 user 继续，并记录。这是 0.9.0 及更早的行为。
    #[default]
    Drop,
    /// 报错。请求失败，但 prefill 同样用不上。
    Refuse,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PipelineMode {
    Off,
    Audit,
    #[default]
    Enforce,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheStrategy {
    #[default]
    Off,
    StaticPrefix,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageStrategy {
    #[default]
    Preserve,
    LosslessTiles,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactConfig {
    pub enabled: bool,
    pub threshold_bytes: usize,
    pub max_store_bytes: usize,
    pub max_artifact_bytes: usize,
    pub ttl_secs: u64,
    pub read_bytes: usize,
    pub max_rounds: usize,
}
impl Default for ArtifactConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_bytes: 128 * 1024,
            max_store_bytes: 64 * 1024 * 1024,
            max_artifact_bytes: 16 * 1024 * 1024,
            ttl_secs: 3600,
            read_bytes: 16 * 1024,
            max_rounds: 4,
        }
    }
}

/// 分块处理策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChunkedMapStrategy {
    /// 既有行为：不提供分块处理。
    #[default]
    Off,
    /// 提供由**模型自己调用**的分块工具。
    ///
    /// **这不是无损机制**：切块后没有任何一轮同时看到超过一块，依赖跨块原文的结论
    /// 无法得出。它不是"不降智"的实现，也不应被这样描述。网关不会背着模型自动套用，
    /// 模型必须显式调用，才不会在不知情的情况下拿到由碎片推出的结论。
    ModelInvoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ChunkedMapConfig {
    pub strategy: ChunkedMapStrategy,
    /// 单块字节上限。
    pub chunk_bytes: usize,
    /// 块数上限。超过即报错，绝不静默只处理一部分。每块都是一次真实上游轮次。
    pub max_chunks: usize,
}
impl Default for ChunkedMapConfig {
    fn default() -> Self {
        Self {
            strategy: ChunkedMapStrategy::Off,
            chunk_bytes: 32_768,
            max_chunks: 8,
        }
    }
}

/// 工具声明的发送策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCatalogStrategy {
    /// 既有行为：一次性发送全部工具 schema。
    #[default]
    Inline,
    /// 声明体积超过 `budgetBytes` 时改为分页目录 + 按需揭示。
    ///
    /// **可达性无损**：没有工具被移除，网关也不替模型判断哪些工具重要。代价是选工具
    /// 时看到的是名称与描述而非完整 schema，且够到一个工具要多花轮次。
    OnDemand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCatalogConfig {
    pub strategy: ToolCatalogStrategy,
    /// 全部工具序列化后的字节预算；超过才启用按需发现。
    pub budget_bytes: usize,
}
impl Default for ToolCatalogConfig {
    fn default() -> Self {
        Self {
            strategy: ToolCatalogStrategy::Inline,
            budget_bytes: 131_072,
        }
    }
}

/// 发送前的 token 准入策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdmissionStrategy {
    /// 既有行为：不做 token 层面的本地拦截。
    #[default]
    Off,
    /// 估算输入 token 超过**上游声明**的 `maxInputTokens` 时，发送前拒绝。
    ///
    /// 判据是本地启发式估算，不是上游自己的计数，因此**可能拒掉上游本来会接受的
    /// 请求**。这就是它默认关闭的原因。写死的模型名窗口表永远不得充当这里的上限：
    /// 它是一个猜测，拿猜测做拦截等于把猜测升级成门禁。
    DeclaredCeiling,
}

/// 收到分类明确的长度拒绝后的恢复策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryStrategy {
    /// 既有行为：上游拒绝即结束。
    #[default]
    Off,
    /// 对 payload 施加一次无损修正并**仅重发一次**，同模型。
    ///
    /// 不改变正常请求的稳态形状：修正只作用于这一次重试，所以接受性未验证的形状
    /// 只会在上游**已经拒绝**了常规形状之后发出——它可能搞砸的那次尝试本来就已经
    /// 失败了。修正后若字节毫无变化则不重发，那是被禁止的盲目原样重发。
    LosslessRetry,
}

/// 工具结果的线上形状策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolResultStrategy {
    /// 既有行为：把全部片段用换行拼成单个 text 条目。
    #[default]
    Join,
    /// 超过 `chunkBytes` 的正文切成多个 text 条目，**字节完全保留**。
    ///
    /// 上游 `toolResults[].content` 本就是数组，此前恒为单条目是转换器的选择而非
    /// schema 限制。但**上游是否接受多于一个条目尚未验证**：本仓库从未发送过这种
    /// 载荷，且不允许为探测而发试探流量。因此该策略默认关闭，与 static-prefix
    /// cachePoint 同为可撤销的实验特性；若开启后出现 400，应改回 `join`，不得
    /// 自动改形或重试去绕过拒绝。
    LosslessChunks,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolResultConfig {
    pub strategy: ToolResultStrategy,
    /// 单个 text 条目的字节上限。单个字符本身超过该值时整体成为一个分片，
    /// 宁可超限也不切断 UTF-8 字符。
    pub chunk_bytes: usize,
}
impl Default for ToolResultConfig {
    fn default() -> Self {
        Self {
            strategy: ToolResultStrategy::Join,
            chunk_bytes: 400_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageConfig {
    pub strategy: ImageStrategy,
    pub tile_max_base64_bytes: usize,
    pub max_tiles: usize,
    pub max_pixels: u64,
}
impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            strategy: ImageStrategy::Preserve,
            tile_max_base64_bytes: 400_000,
            max_tiles: 32,
            max_pixels: 40_000_000,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct WireLimits {
    pub body_bytes: Option<usize>,
    pub text_field_bytes: Option<usize>,
    pub tool_result_bytes: Option<usize>,
    pub image_base64_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct PipelineConfig {
    pub mode: PipelineMode,
    /// 末尾 assistant 消息的处理方式。与 `mode` 正交——关掉预算强制不等于
    /// 同意悄悄丢内容。
    #[serde(default)]
    pub prefill: PrefillStrategy,
    pub strip_billing_header: bool,
    pub cache_strategy: CacheStrategy,
    pub agent_mode: String,
    pub ingress_max_bytes: usize,
    pub limits: WireLimits,
    pub artifacts: ArtifactConfig,
    pub images: ImageConfig,
    pub tool_results: ToolResultConfig,
    pub admission: AdmissionStrategy,
    pub recovery: RecoveryStrategy,
    pub tool_catalog: ToolCatalogConfig,
    pub chunked_map: ChunkedMapConfig,
    pub audit_enabled: bool,
    pub allow_simulated_cache: bool,
    pub kiro_only: bool,
}
impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            mode: PipelineMode::Enforce,
            prefill: PrefillStrategy::Drop,
            strip_billing_header: true,
            cache_strategy: CacheStrategy::Off,
            agent_mode: "vibe".into(),
            ingress_max_bytes: 50 * 1024 * 1024,
            limits: WireLimits::default(),
            artifacts: ArtifactConfig::default(),
            images: ImageConfig::default(),
            tool_results: ToolResultConfig::default(),
            admission: AdmissionStrategy::Off,
            recovery: RecoveryStrategy::Off,
            tool_catalog: ToolCatalogConfig::default(),
            chunked_map: ChunkedMapConfig::default(),
            audit_enabled: true,
            allow_simulated_cache: false,
            kiro_only: true,
        }
    }
}
impl PipelineConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self.agent_mode.as_str(), "vibe" | "spec"),
            "requestPipeline.agentMode must be vibe or spec"
        );
        anyhow::ensure!(
            (1024..=100 * 1024 * 1024).contains(&self.ingress_max_bytes),
            "requestPipeline.ingressMaxBytes must be between 1024 and 104857600"
        );
        for (name, value) in [
            ("bodyBytes", self.limits.body_bytes),
            ("textFieldBytes", self.limits.text_field_bytes),
            ("toolResultBytes", self.limits.tool_result_bytes),
            ("imageBase64Bytes", self.limits.image_base64_bytes),
        ] {
            anyhow::ensure!(
                value.is_none_or(|n| n > 0 && n <= self.ingress_max_bytes),
                "requestPipeline.limits.{name} must be positive and <= ingressMaxBytes"
            );
        }
        let a = &self.artifacts;
        anyhow::ensure!(
            a.threshold_bytes >= 1024
                && a.threshold_bytes <= a.max_artifact_bytes
                && a.max_artifact_bytes <= a.max_store_bytes
                && a.max_store_bytes <= 1024 * 1024 * 1024,
            "invalid artifact byte budgets"
        );
        anyhow::ensure!(
            (256..=65536).contains(&a.read_bytes)
                && (1..=16).contains(&a.max_rounds)
                && (60..=86400).contains(&a.ttl_secs),
            "invalid artifact readBytes/maxRounds/ttlSecs"
        );
        let i = &self.images;
        anyhow::ensure!(
            i.tile_max_base64_bytes >= 4096
                && i.tile_max_base64_bytes <= 100 * 1024 * 1024
                && (1..=128).contains(&i.max_tiles)
                && (1..=100_000_000).contains(&i.max_pixels),
            "invalid lossless image budgets"
        );
        if i.strategy == ImageStrategy::LosslessTiles {
            anyhow::ensure!(
                i.tile_max_base64_bytes <= self.ingress_max_bytes,
                "active lossless tile budget must fit ingressMaxBytes"
            );
        }
        let t = &self.tool_results;
        // 未启用的预算不得卡住配置：与 lossless-tiles 同理，只做与策略无关的下界校验。
        anyhow::ensure!(
            (1024..=100 * 1024 * 1024).contains(&t.chunk_bytes),
            "requestPipeline.toolResults.chunkBytes must be between 1024 and 104857600"
        );
        if t.strategy == ToolResultStrategy::LosslessChunks {
            anyhow::ensure!(
                t.chunk_bytes <= self.ingress_max_bytes,
                "active toolResults.chunkBytes must fit ingressMaxBytes"
            );
            // 分片是为了绕开单字段预算；分片本身还大于该预算就没有意义。
            anyhow::ensure!(
                self.limits
                    .tool_result_bytes
                    .is_none_or(|limit| t.chunk_bytes <= limit),
                "active toolResults.chunkBytes must fit limits.toolResultBytes"
            );
        }
        let c = &self.tool_catalog;
        anyhow::ensure!(
            (1024..=100 * 1024 * 1024).contains(&c.budget_bytes),
            "requestPipeline.toolCatalog.budgetBytes must be between 1024 and 104857600"
        );
        let m = &self.chunked_map;
        anyhow::ensure!(
            (1024..=10 * 1024 * 1024).contains(&m.chunk_bytes) && (1..=64).contains(&m.max_chunks),
            "requestPipeline.chunkedMap chunkBytes must be 1024..=10485760 and maxChunks 1..=64"
        );
        Ok(())
    }
}
