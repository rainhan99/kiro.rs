//! 在线更新的二进制下载与替换实现。
//!
//! 这套方案不再操作 docker daemon：
//! 1. 从 GitHub Releases 下载与当前平台匹配的 `kiro-rs-<ver>-<plat>.tar.gz`/`.zip`；
//! 2. 验证 `SHA256SUMS.txt` 校验和；
//! 3. 解压取出新二进制，原子替换当前 exe，旧版本写到 `<exe>.backup`；
//! 4. 调用方收到 `need_restart=true` 后再触发进程退出，由 docker 的
//!    `restart: unless-stopped` 接管重启，新版本随之生效。
//!
//! 这样做的好处是更新过程完全不依赖容器自管自，网络断/校验失败时旧二进制
//! 仍然在跑，避免之前 docker compose pull 路径上的"旧停新挂"事故。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::admin::error::AdminServiceError;

/// 单个下载体的最大字节数，避免 GitHub 异常返回时占满磁盘。
/// kiro-rs musl 二进制实测 < 50 MB，留 200 MB 上限足够覆盖未来增长。
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;

/// GitHub Releases 仓库 owner/repo。
///
/// 必须与 `service.rs::GITHUB_RELEASES_REPO` 保持一致：前者查版本与 changelog，
/// 后者下载资产，指向不同仓库会导致版本检查与实际下载来源错配。
const GITHUB_REPO: &str = "rainhan99/kiro.rs";

/// release 包内（解压后）二进制文件名。Linux/macOS 是 `kiro-rs`，
/// Windows 是 `kiro-rs.exe`。
fn binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "kiro-rs.exe"
    } else {
        "kiro-rs"
    }
}

/// 当前进程二进制的绝对路径（已解析符号链接），更新就替换它。
pub fn current_executable() -> Result<PathBuf, AdminServiceError> {
    let exe = std::env::current_exe().map_err(|e| {
        AdminServiceError::InternalError(format!("无法获取当前可执行文件路径: {}", e))
    })?;
    let resolved = std::fs::canonicalize(&exe).unwrap_or(exe);
    Ok(resolved)
}

/// 备份文件路径：`<exe>.backup`。回退接口直接把它换回去。
pub fn backup_path(exe: &Path) -> PathBuf {
    let mut s = exe.as_os_str().to_os_string();
    s.push(".backup");
    PathBuf::from(s)
}

/// 当前平台对应的 release archive 后缀名片段，例如 `Linux-musl-x64.tar.gz`。
///
/// 与 `.github/workflows/release.yaml` 里的矩阵一致：
/// - Linux x86_64：musl 静态二进制（容器/宿主机通用）
/// - Linux aarch64：musl 静态二进制
/// - macOS x86_64 / aarch64：tar.gz
/// - Windows x86_64：zip
fn platform_suffix() -> Result<&'static str, AdminServiceError> {
    let suffix = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "Linux-musl-x64.tar.gz",
        ("linux", "aarch64") => "Linux-musl-arm64.tar.gz",
        ("macos", "x86_64") => "macOS-x64.tar.gz",
        ("macos", "aarch64") => "macOS-arm64.tar.gz",
        ("windows", "x86_64") => "Windows-x64.zip",
        (os, arch) => {
            return Err(AdminServiceError::InternalError(format!(
                "不支持的平台 {}/{}：在线更新仅支持 release.yaml 矩阵中已发布的目标",
                os, arch
            )));
        }
    };
    Ok(suffix)
}

/// 期望的 release archive 文件名，例如 `kiro-rs-0.3.0-Linux-musl-x64.tar.gz`。
fn archive_filename(version: &str) -> Result<String, AdminServiceError> {
    let v = version.trim().trim_start_matches('v');
    if v.is_empty() {
        return Err(AdminServiceError::InternalError(
            "版本号为空，无法定位 release 资产".to_string(),
        ));
    }
    Ok(format!("kiro-rs-{}-{}", v, platform_suffix()?))
}

/// 下载并校验某个 release 版本的二进制压缩包，把内部的 `kiro-rs` 提取到 `dest`。
///
/// `proxy` 为 `Some` 时所有 HTTP 请求走该代理（与项目其它出站路径一致）。
/// 下载并校验某个 release 版本的二进制压缩包，把内部的 `kiro-rs` 提取到 `dest`。
///
/// `proxy` 为 `Some` 时所有 HTTP 请求走该代理（与项目其它出站路径一致）。
/// `github_token` 不为空时给所有请求带上 `Authorization: Bearer <token>`，
/// 把 GitHub API 限流从匿名 60/h 提升到认证 5000/h。
pub async fn download_release_binary(
    version: &str,
    proxy: Option<&str>,
    github_token: Option<&str>,
    dest: &Path,
) -> Result<(), AdminServiceError> {
    let archive = archive_filename(version)?;
    let base = format!(
        "https://github.com/{}/releases/download/v{}",
        GITHUB_REPO,
        version.trim().trim_start_matches('v')
    );
    let archive_url = format!("{}/{}", base, archive);
    let checksums_url = format!("{}/SHA256SUMS.txt", base);

    let client = build_http_client(proxy)?;
    let token = github_token.and_then(|t| {
        let trimmed = t.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    // 下载到临时目录，确保失败时不会污染 exe 所在目录
    let tmp_dir = std::env::temp_dir().join(format!(
        "kiro-rs-update-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    fs::create_dir_all(&tmp_dir).map_err(|e| {
        AdminServiceError::InternalError(format!("创建更新临时目录失败: {}", e))
    })?;
    let archive_path = tmp_dir.join(&archive);

    download_to_file(&client, &archive_url, token.as_deref(), &archive_path).await?;
    verify_checksum(
        &client,
        &checksums_url,
        token.as_deref(),
        &archive,
        &archive_path,
    )
    .await?;

    let extract_dir = tmp_dir.join("extract");
    fs::create_dir_all(&extract_dir).map_err(|e| {
        AdminServiceError::InternalError(format!("创建解压目录失败: {}", e))
    })?;
    extract_archive(&archive_path, &extract_dir)?;

    let extracted = locate_binary(&extract_dir)?;
    // 复制到调用方指定的目标位置（通常是 exe 所在目录的临时文件，方便原子替换）
    fs::copy(&extracted, dest).map_err(|e| {
        AdminServiceError::InternalError(format!("拷贝新二进制失败: {}", e))
    })?;
    set_executable(dest)?;

    // 清理临时目录（失败仅记录，不影响主流程）
    let _ = fs::remove_dir_all(&tmp_dir);
    Ok(())
}

pub(super) fn build_http_client(
    proxy: Option<&str>,
) -> Result<reqwest::Client, AdminServiceError> {
    // `timeout` 是**整体**超时：20MB 的产物在慢链路上会被它硬砍，哪怕一直在传。
    // 换成空闲超时——只要还有数据到达就继续，真正卡住才放弃。连接阶段另设一个
    // 短上限，避免在一个连不上的地址上干等。
    let mut builder = reqwest::Client::builder()
        .user_agent("kiro-rs-updater")
        .connect_timeout(std::time::Duration::from_secs(20))
        .read_timeout(std::time::Duration::from_secs(60));
    if let Some(url) = proxy.and_then(|u| {
        let s = u.trim();
        if s.is_empty() { None } else { Some(s) }
    }) {
        let proxy = reqwest::Proxy::all(url).map_err(|e| {
            AdminServiceError::InternalError(format!("代理配置无效: {}", e))
        })?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| AdminServiceError::InternalError(format!("构造 HTTP 客户端失败: {}", e)))
}

async fn download_to_file(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    dest: &Path,
) -> Result<(), AdminServiceError> {
    let mut req = client.get(url);
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {}", t));
    }
    let resp = req.send().await.map_err(|e| {
        AdminServiceError::InternalError(format!("下载 {} 失败: {}", url, e))
    })?;
    if !resp.status().is_success() {
        return Err(AdminServiceError::InternalError(format!(
            "下载 {} 返回 {}",
            url,
            resp.status()
        )));
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_DOWNLOAD_BYTES {
            return Err(AdminServiceError::InternalError(format!(
                "下载体积 {} 字节超过上限 {} 字节",
                len, MAX_DOWNLOAD_BYTES
            )));
        }
    }

    let bytes = resp.bytes().await.map_err(|e| {
        AdminServiceError::InternalError(format!("读取下载内容失败: {}", e))
    })?;
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err(AdminServiceError::InternalError(format!(
            "实际下载体积 {} 字节超过上限",
            bytes.len()
        )));
    }
    fs::write(dest, &bytes).map_err(|e| {
        AdminServiceError::InternalError(format!("写入下载文件 {} 失败: {}", dest.display(), e))
    })?;
    Ok(())
}

async fn verify_checksum(
    client: &reqwest::Client,
    checksums_url: &str,
    token: Option<&str>,
    archive_name: &str,
    archive_path: &Path,
) -> Result<(), AdminServiceError> {
    let mut req = client.get(checksums_url);
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {}", t));
    }
    let resp = req.send().await.map_err(|e| {
        AdminServiceError::InternalError(format!("下载 SHA256SUMS.txt 失败: {}", e))
    })?;
    if !resp.status().is_success() {
        return Err(AdminServiceError::InternalError(format!(
            "下载 SHA256SUMS.txt 返回 {}",
            resp.status()
        )));
    }
    let body = resp.text().await.map_err(|e| {
        AdminServiceError::InternalError(format!("读取 SHA256SUMS.txt 失败: {}", e))
    })?;

    let expected = body
        .lines()
        .filter_map(|line| {
            let mut iter = line.split_whitespace();
            let hash = iter.next()?;
            let name = iter.next()?.trim_start_matches('*');
            if name == archive_name {
                Some(hash.to_ascii_lowercase())
            } else {
                None
            }
        })
        .next()
        .ok_or_else(|| {
            AdminServiceError::InternalError(format!(
                "SHA256SUMS.txt 中未找到 {} 的校验项",
                archive_name
            ))
        })?;

    let actual = sha256_file(archive_path)?;
    if actual != expected {
        return Err(AdminServiceError::InternalError(format!(
            "校验和不匹配：期望 {}，实际 {}",
            expected, actual
        )));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, AdminServiceError> {
    let mut file = fs::File::open(path).map_err(|e| {
        AdminServiceError::InternalError(format!("打开 {} 失败: {}", path.display(), e))
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| {
            AdminServiceError::InternalError(format!("读取 {} 失败: {}", path.display(), e))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<(), AdminServiceError> {
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        extract_tar_gz(archive, dest)
    } else if name.ends_with(".zip") {
        extract_zip(archive, dest)
    } else {
        Err(AdminServiceError::InternalError(format!(
            "不支持的归档格式: {}",
            name
        )))
    }
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<(), AdminServiceError> {
    let bytes = fs::read(archive).map_err(|e| {
        AdminServiceError::InternalError(format!("读取归档 {} 失败: {}", archive.display(), e))
    })?;
    let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut tar = tar::Archive::new(gz);
    tar.unpack(dest).map_err(|e| {
        AdminServiceError::InternalError(format!("解压 tar.gz 失败: {}", e))
    })
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<(), AdminServiceError> {
    let file = fs::File::open(archive).map_err(|e| {
        AdminServiceError::InternalError(format!("打开 {} 失败: {}", archive.display(), e))
    })?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| {
        AdminServiceError::InternalError(format!("解析 zip 失败: {}", e))
    })?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| {
            AdminServiceError::InternalError(format!("读取 zip 条目失败: {}", e))
        })?;
        let entry_path = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let target = dest.join(entry_path);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|e| {
                AdminServiceError::InternalError(format!("创建目录失败: {}", e))
            })?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                AdminServiceError::InternalError(format!("创建目录失败: {}", e))
            })?;
        }
        let mut out = fs::File::create(&target).map_err(|e| {
            AdminServiceError::InternalError(format!("创建文件 {} 失败: {}", target.display(), e))
        })?;
        std::io::copy(&mut entry, &mut out).map_err(|e| {
            AdminServiceError::InternalError(format!("写入 {} 失败: {}", target.display(), e))
        })?;
    }
    Ok(())
}

fn locate_binary(root: &Path) -> Result<PathBuf, AdminServiceError> {
    let target = binary_name();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| {
            AdminServiceError::InternalError(format!("读取目录 {} 失败: {}", dir.display(), e))
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| {
                AdminServiceError::InternalError(format!("枚举目录项失败: {}", e))
            })?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n == target)
                .unwrap_or(false)
            {
                return Ok(path);
            }
        }
    }
    Err(AdminServiceError::InternalError(format!(
        "归档中未找到 {} 二进制",
        target
    )))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), AdminServiceError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|e| AdminServiceError::InternalError(format!("读取权限失败: {}", e)))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).map_err(|e| {
        AdminServiceError::InternalError(format!("设置可执行权限失败: {}", e))
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), AdminServiceError> {
    Ok(())
}

/// 把 `staged`（已下载的新二进制）安装为当前 exe，并把当前 exe 备份到 `<exe>.backup`。
///
/// Windows 下正在运行的 exe 不能直接覆盖，但可以被重命名；这里一律走
/// "rename current → backup; rename staged → current" 的两步流程，
/// 保证任何一步失败都能回滚。
pub fn install_binary(exe: &Path, staged: &Path) -> Result<(), AdminServiceError> {
    let backup = backup_path(exe);
    // 旧的 backup 留着没用，先清掉。
    let _ = fs::remove_file(&backup);
    fs::rename(exe, &backup).map_err(|e| {
        AdminServiceError::InternalError(format!(
            "备份当前可执行文件 {} 失败: {}",
            exe.display(),
            e
        ))
    })?;
    if let Err(e) = fs::rename(staged, exe) {
        // staged → exe 失败时把 backup 还原，确保旧版本仍可用
        let _ = fs::rename(&backup, exe);
        return Err(AdminServiceError::InternalError(format!(
            "安装新可执行文件失败: {}",
            e
        )));
    }
    Ok(())
}

/// 用 `<exe>.backup` 覆盖当前 exe，实现"回退到上一版本"。
pub fn restore_backup(exe: &Path) -> Result<(), AdminServiceError> {
    let backup = backup_path(exe);
    if !backup.exists() {
        return Err(AdminServiceError::InternalError(
            "未找到本地备份二进制（<exe>.backup 不存在），无法离线回退".to_string(),
        ));
    }
    // 把当前 exe 暂存到 .rollback-current，再把 backup 换成新的 exe。
    let mut rollback_tmp = exe.as_os_str().to_os_string();
    rollback_tmp.push(".rollback-current");
    let rollback_tmp = PathBuf::from(rollback_tmp);
    let _ = fs::remove_file(&rollback_tmp);
    fs::rename(exe, &rollback_tmp).map_err(|e| {
        AdminServiceError::InternalError(format!("暂存当前 exe 失败: {}", e))
    })?;
    if let Err(e) = fs::rename(&backup, exe) {
        let _ = fs::rename(&rollback_tmp, exe);
        return Err(AdminServiceError::InternalError(format!(
            "回退失败: {}",
            e
        )));
    }
    let _ = fs::remove_file(&rollback_tmp);
    Ok(())
}

/// 重启自己要执行的命令：当前可执行文件 + 本次启动的原始参数。
///
/// 参数必须原样带上——配置路径、凭据路径都在里面，丢掉它们等于用另一套配置
/// 重启，而且往往"看起来正常"，因为默认值也能跑起来。
pub fn restart_command() -> Result<(PathBuf, Vec<std::ffi::OsString>), AdminServiceError> {
    let exe = current_executable()?;
    // 跳过 argv[0]：那是旧的调用名，要执行的路径由 exe 给出。
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    Ok((exe, args))
}

/// 在 `delay` 之后用新二进制**替换掉自己**。
///
/// # 为什么不再是「退出，等监管进程拉起来」
///
/// 那要求部署里有 docker 的 `restart: unless-stopped` 或 systemd。前台直接跑
/// `./kiro-rs` 的人没有这些东西，于是「更新并重启」只做了前半句，服务就停在
/// 那里——而界面从不提这件事。
///
/// Unix 上 `exec` 用新二进制替换当前进程映像，PID 不变：前台、systemd、docker
/// 全都成立。对 docker 反而更好——容器不必重启，没有停机窗口。监听套接字由
/// Rust 默认带的 `CLOEXEC` 在 exec 时关闭，新进程照常绑定。
///
/// exec 失败时退回旧行为（退出进程）：此时二进制已经换好，留在原地跑旧代码
/// 同样不对。日志会写明走的是哪一种。
pub fn schedule_self_restart(delay: std::time::Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = std::io::stdout().flush();
        match restart_command() {
            Ok((exe, args)) => {
                tracing::info!(exe = %exe.display(), "以新二进制替换当前进程");
                let _ = std::io::stdout().flush();
                exec_replacing_self(&exe, &args);
                // 能走到这里就说明 exec 没成功——成功是不会返回的。
                tracing::error!("exec 失败，改为退出进程；需要监管进程或手动重启");
            }
            Err(error) => tracing::error!("无法确定重启命令（{error}），改为退出进程"),
        }
        let _ = std::io::stdout().flush();
        std::process::exit(0);
    });
}

/// Unix：`exec` 成功则永不返回。
#[cfg(unix)]
fn exec_replacing_self(exe: &Path, args: &[std::ffi::OsString]) {
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(exe).args(args).exec();
    tracing::error!("exec {} 失败: {error}", exe.display());
}

/// Windows 没有 exec：拉起新进程，随后由调用方退出当前进程。
#[cfg(not(unix))]
fn exec_replacing_self(exe: &Path, args: &[std::ffi::OsString]) {
    match std::process::Command::new(exe).args(args).spawn() {
        Ok(child) => {
            tracing::info!(pid = child.id(), "已拉起新进程，当前进程随后退出");
            // 让新进程先把端口占上，避免两边同时绑定的一瞬冲突。
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        Err(error) => tracing::error!("拉起新进程失败: {error}"),
    }
}

#[cfg(test)]
mod restart_tests {
    use super::*;

    /// 重启命令必须带上**本次启动的原始参数**。
    ///
    /// 配置路径、凭据路径都在参数里；丢掉它们等于用另一套配置重启，而且往往
    /// 静悄悄地"看起来正常"，因为默认值也能跑起来。
    #[test]
    fn the_restart_command_keeps_the_arguments_this_process_was_started_with() {
        let (exe, args) = restart_command().expect("应能确定重启命令");
        assert!(exe.is_absolute(), "要用解析后的绝对路径：{}", exe.display());

        let live: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        assert_eq!(args, live, "参数必须逐个原样保留");
    }
}
