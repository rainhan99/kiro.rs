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
pub fn ensure_data_files(base: &Path) -> anyhow::Result<DataPaths> {
    std::fs::create_dir_all(base)?;
    let config = base.join("config.json");
    let credentials = base.join("credentials.json");
    Ok(DataPaths {
        config,
        credentials,
    })
}

#[cfg(test)]
#[path = "paths_tests.rs"]
mod paths_tests;
