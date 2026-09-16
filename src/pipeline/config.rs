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
        Ok(())
    }
}
