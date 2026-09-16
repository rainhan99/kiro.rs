use clap::Parser;

/// Anthropic <-> Kiro API 客户端
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// 配置文件路径
    #[arg(short, long)]
    pub config: Option<String>,

    /// 凭证文件路径
    #[arg(long)]
    pub credentials: Option<String>,

    /// Validate configuration and print effective pipeline settings, without loading credentials or making network requests.
    #[arg(long)]
    pub check_config: bool,

    /// Inspect a local Anthropic request JSON offline; output redacted construction evidence, never send it upstream.
    #[arg(long, value_name = "PATH")]
    pub inspect_request: Option<String>,
}
