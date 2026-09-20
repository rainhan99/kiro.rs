//! kiro-rs 的命令行入口。
//!
//! 这是一层薄壳：解析参数 → 调库入口 → 把错误翻译成退出码。装配逻辑住在
//! `kiro_rs::runtime`，桌面应用用的是同一段代码——区别只在失败时怎么死。

use clap::Parser;
use kiro_rs::kiro::model::credentials::KiroCredentials;
use kiro_rs::model::arg::Args;
use kiro_rs::model::config::Config;
use kiro_rs::pipeline;

#[tokio::main]
async fn main() {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());

    // Offline commands exit before credential loading, generated config files,
    // version/model warmers, Redis or any provider HTTP client is initialized.
    if args.check_config || args.inspect_request.is_some() {
        if let Err(error) = pipeline::inspect::run(&config_path, args.inspect_request.as_deref()) {
            eprintln!("{error:#}");
            std::process::exit(2);
        }
        return;
    }

    let credentials_path = args
        .credentials
        .unwrap_or_else(|| KiroCredentials::default_credentials_path().to_string());

    let options = kiro_rs::Options::new(&config_path, &credentials_path);
    let server = match kiro_rs::serve(options).await {
        Ok(server) => server,
        Err(error) => fail(error),
    };

    kiro_rs::runtime::log_startup_banner(server.addr());

    if let Err(error) = server.wait_for_signal().await {
        fail(error);
    }
}

/// 启动失败的唯一出口。
///
/// 只走 `eprintln!`，**不**走 `tracing::error!`：致命的启动错误不能被日志
/// 过滤器吃掉。`RUST_LOG=off` 是运维会真用的设置，那时如果原因只走 tracing，
/// 用户就只剩一个退出码 1，什么都看不到。两个都写则同一句话会打两遍。
/// `tests/pipeline_cli.rs` 里的黑盒测试钉住这两条。
fn fail(error: anyhow::Error) -> ! {
    eprintln!("{error:#}");
    std::process::exit(1);
}
