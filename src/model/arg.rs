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

    /// 打印配置里的 API Key、管理密钥与数据目录后退出。
    ///
    /// 密钥只在首次生成时打印过一次，之后被日志刷走；桌面版根本没有日志
    /// 出口。没有这个入口，用户唯一的办法是去翻一个他不知道在哪的 JSON。
    /// 与 --check-config 同一层：不加载凭据、不建文件、不连网。
    #[arg(long)]
    pub show_keys: bool,
}
