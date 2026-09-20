//! 主动取回密钥。
//!
//! 密钥只在首次生成时打印一次，之后就只在配置文件里。命令行有
//! `--show-keys`，桌面端有「系统设置 → 安全」——两端都是「启动只说在哪，
//! 要看就主动要」。启动横幅本身不打明文：日志常被 systemd / Docker / CI
//! 采集走。

use std::path::Path;

/// `--show-keys` 的输出。
///
/// 返回行而不是直接打印，是为了能测——「密钥确实打出来了」和「数据目录
/// 确实说清了」这两件事值得钉住。
pub fn key_report_lines(config: &crate::model::config::Config, data_dir: &Path) -> Vec<String> {
    let show = |label: &str, value: Option<&String>, hint: &str| -> String {
        match value.map(|v| v.trim()).filter(|v| !v.is_empty()) {
            Some(key) => format!("{label} = {key}    （{hint}）"),
            None => format!("{label} = （未配置）"),
        }
    };

    vec![
        format!("数据目录: {}", data_dir.display()),
        format!("配置文件: {}", data_dir.join("config.json").display()),
        String::new(),
        show(
            "apiKey     ",
            config.api_key.as_ref(),
            "客户端用，填给 Claude Code / Codex 等",
        ),
        show(
            "adminApiKey",
            config.admin_api_key.as_ref(),
            "管理界面登录用",
        ),
    ]
}

/// 离线执行 `--show-keys`：读配置、打印、返回退出码。
///
/// 不加载凭据、不建任何运行期文件、不连网——与 `--check-config` 同一层。
pub fn run_show_keys(config_path: &str, credentials_path: &str) -> i32 {
    let config = match crate::model::config::Config::load(config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("加载配置失败: {error}");
            return 2;
        }
    };

    // 数据目录 = 凭据文件的父目录，与运行期完全同一条推导，
    // 否则这里报的位置和实际落盘的位置会对不上。
    let data_dir = Path::new(credentials_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    for line in key_report_lines(&config, &data_dir) {
        println!("{line}");
    }
    0
}

#[cfg(test)]
#[path = "keys_tests.rs"]
mod keys_tests;
