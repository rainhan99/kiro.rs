//! 桌面端的文件落脚点。
//!
//! 两条硬约束：
//! 1. 不能写进 `.app` 内部——应用包是只读的，写进去会破坏代码签名。
//! 2. 不能依赖当前工作目录——双击启动时 CWD 是 `/`，相对路径会落到根目录。
//!
//! 因此这里解析出来的一律是绝对路径，指向操作系统的应用数据目录。

use std::path::{Path, PathBuf};

/// 配置与凭据文件的位置。
///
/// 两者**必须同目录**：`cache_dir` 取的是凭据文件的父目录，所有运行期文件
/// （客户端 Key、用量日志、分组、账本、trace、缓存计量）都 join 在它下面。
/// 把两者分开放会让运行期文件跟着凭据跑到意料之外的地方。
#[derive(Debug, Clone)]
pub struct DataPaths {
    pub config: PathBuf,
    pub credentials: PathBuf,
}

/// 解析本平台的应用数据目录。
///
/// - macOS: `~/Library/Application Support/rs.kiro.kiro-rs`
/// - Windows: `%APPDATA%\kiro\kiro-rs\data`
///
/// `KIRO_DATA_DIR` 可以覆盖它——只给测试与离线冒烟用，不写进常规文档：
/// 它能重定向凭据的存放位置。
pub fn data_dir_for(_app: &str) -> anyhow::Result<PathBuf> {
    if let Some(override_dir) = std::env::var_os("KIRO_DATA_DIR") {
        let path = PathBuf::from(override_dir);
        anyhow::ensure!(
            path.is_absolute(),
            "KIRO_DATA_DIR 必须是绝对路径，实际: {}",
            path.display()
        );
        return Ok(path);
    }

    let dirs = directories::ProjectDirs::from("rs", "kiro", "kiro-rs")
        .ok_or_else(|| anyhow::anyhow!("无法解析本平台的应用数据目录"))?;
    Ok(dirs.data_dir().to_path_buf())
}

/// 在应用数据目录里准备好配置与凭据文件。
///
/// 已存在则**原样保留**。覆盖用户配置是不可逆的，这里只做「不存在才建」。
///
/// 生成逻辑复用 `kiro_rs::runtime::ensure_config_files_with_host`，不在这里
/// 另写一份——两份实现迟早会在默认值或文件权限上漂移，而那种漂移要等到
/// 用户双击应用才发现。区别只有一个参数：桌面端绑 `127.0.0.1`。
pub fn ensure_data_files(base: &Path) -> anyhow::Result<DataPaths> {
    std::fs::create_dir_all(base)?;
    let config = base.join("config.json");
    let credentials = base.join("credentials.json");

    kiro_rs::runtime::ensure_config_files_with_host(
        &config.to_string_lossy(),
        &credentials.to_string_lossy(),
        // 桌面端没有「让局域网里的别人连过来」这个需求，而绑 0.0.0.0
        // 会把一个带着凭据的代理暴露到局域网。
        "127.0.0.1",
    );

    anyhow::ensure!(
        config.exists() && credentials.exists(),
        "无法在 {} 下创建配置文件——目录不可写？",
        base.display()
    );

    Ok(DataPaths {
        config,
        credentials,
    })
}

#[cfg(test)]
#[path = "paths_tests.rs"]
mod paths_tests;
