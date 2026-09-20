//! 文件写入的共用规矩。
//!
//! kiro-rs 在运行期会落一批文件，其中好几个足以让人冒充或花钱：
//! `config.json`（apiKey / adminApiKey）、`credentials.json`（刷新令牌）、
//! `client_api_keys.json`（明文 `sk-…`）、`gateway.json`（上游 API Key）、
//! `billing.db`（账本）。它们**一个都不该是世界可读的**。
//!
//! 这里只有一份实现。分散在各模块里各写各的 `fs::write`，漏一处就等于
//! 没做——`client_api_keys.json` 就是这么漏掉的：配置文件改成 0600 之后，
//! 它还是 644，而里面装的是明文客户端 Key。

use std::path::Path;

/// 只有属主可读写的权限位。
pub const PRIVATE_MODE: u32 = 0o600;

/// 写一个只有属主可读写的文件。
///
/// 已存在的文件也会被收紧权限——升级上来的部署里那些 644 的旧文件，
/// 下一次写入就顺手修好，不需要用户做任何事。
///
/// 非 Unix 平台没有 mode 概念，退化成普通写入（Windows 上用户目录默认
/// 就不是世界可读的）。
pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(PRIVATE_MODE)
            .open(path)?;
        file.write_all(contents)?;
        // `.mode()` 只作用于**新建**的文件。已存在的沿用原有权限，
        // 所以这里再显式收一次——否则升级上来的 644 旧文件永远修不好。
        harden(path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

/// 把一个已存在的文件收紧到 0600。
///
/// 供那些不经 [`write_private`] 落盘的文件用——典型是 SQLite，它自己
/// 管文件句柄，只能在它建好之后再收。
pub fn harden(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_MODE))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// 会落在数据目录下、且内容足以让人冒充或花钱的文件。
///
/// 这张表是**唯一**的真相源：`harden_data_dir` 按它扫，
/// `runtime_tests` 的守卫也按它查。新增一个秘密文件只要加进这里。
pub const SECRET_FILES: &[&str] = &[
    "config.json",            // apiKey / adminApiKey
    "credentials.json",       // Kiro 刷新令牌
    "client_api_keys.json",   // 明文 sk-… 客户端 Key
    "groups.json",            // 分组本身不是秘密，但同一套规矩
    "gateway.json",           // 上游 API Key
    "billing.db",             // 账本
    "traces.db",              // 请求链路
    "kiro_balance_cache.json",
    "proxy_pool.json",
    "cache_metering.json",
];

/// 启动时把数据目录里已有的秘密文件收紧到 0600。
///
/// 为什么不能只在写入点设防：`write_private` 只在**写入**时生效，而好几个
/// 文件只有内容变化时才写——一个没加过 Key 的部署，`client_api_keys.json`
/// 可以几个月不被写一次，于是永远停在升级前的 644。这是真机跑出来的。
///
/// SQLite 的 `-wal` / `-shm` 一并扫：WAL 里有还没落主库的数据。
pub fn harden_data_dir(dir: &Path) {
    for name in SECRET_FILES {
        for suffix in ["", "-wal", "-shm"] {
            let path = dir.join(format!("{name}{suffix}"));
            if let Err(error) = harden(&path) {
                tracing::warn!("收紧 {} 权限失败: {error}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_new_file_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        write_private(&path, b"{}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "实际 {mode:o}");
    }

    /// 升级路径：老部署里已经有一个 644 的文件，下一次写入必须顺手修好。
    /// 只靠 `OpenOptions::mode()` 做不到——它对已存在的文件无效。
    #[cfg(unix)]
    #[test]
    fn an_existing_world_readable_file_is_tightened_on_the_next_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.json");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&path, b"new").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "已存在的宽权限文件没有被收紧，实际 {mode:o}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn harden_tightens_a_file_it_did_not_create() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sqlite.db");
        std::fs::write(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        harden(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "实际 {mode:o}");
    }

    /// 文件不存在时 harden 不该报错——调用方常常在「可能没建成」的路径上调它。
    #[test]
    fn harden_is_a_no_op_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        harden(&dir.path().join("never-created")).unwrap();
    }

    /// 目录扫描收紧已有文件，且不因为缺文件而中断。
    #[cfg(unix)]
    #[test]
    fn the_directory_sweep_tightens_what_is_there_and_ignores_what_is_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();

        let present = dir.path().join("client_api_keys.json");
        std::fs::write(&present, "[]").unwrap();
        std::fs::set_permissions(&present, std::fs::Permissions::from_mode(0o644)).unwrap();
        // 其余九个都不存在——扫描必须照样走完

        harden_data_dir(dir.path());

        let mode = std::fs::metadata(&present).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "实际 {mode:o}");
    }

    /// 非秘密文件不该被顺手改权限——那会让人以为程序在乱动他的文件。
    #[cfg(unix)]
    #[test]
    fn the_sweep_leaves_unrelated_files_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let unrelated = dir.path().join("README.txt");
        std::fs::write(&unrelated, "hi").unwrap();
        std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o644)).unwrap();

        harden_data_dir(dir.path());

        let mode = std::fs::metadata(&unrelated).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "无关文件被改了权限");
    }
}
