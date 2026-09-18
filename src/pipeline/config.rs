use serde::{Deserialize, Serialize};

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
    pub audit_enabled: bool,
    pub allow_simulated_cache: bool,
    pub kiro_only: bool,
}
impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            mode: PipelineMode::Enforce,
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
        Ok(())
    }
}
