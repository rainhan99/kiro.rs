//! 库入口：在进程内启动 kiro-rs。
//!
//! 这一层的存在理由是桌面应用——它要在自己的进程里把代理跑起来，因此
//! **装配失败必须返回错误，而不是终止进程**。二进制形态下由 `main.rs`
//! 这层薄壳把错误翻译成退出码，行为与从前一致。

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;

mod accounting;
mod foundation;
mod keys;
mod wiring;

// 子模块之间互相引用这两个交接结构，所以在 runtime 这一层重导出；
// `pub(crate)` 意味着它们出不了这个 crate——对外只有 serve()/Options/RunningServer。
pub(crate) use accounting::{Accounting, accounting};
pub(crate) use foundation::{Foundation, foundation};
// 桌面应用用它生成自己的默认配置——同一份实现，避免默认值与权限策略漂移。
pub use foundation::ensure_config_files_with_host;
pub use keys::{key_report_lines, run_show_keys};
pub use wiring::{log_startup_banner, startup_banner_lines};
use wiring::wiring;

/// 库入口的启动参数。字段公开，调用方直接构造。
pub struct Options {
    pub config_path: PathBuf,
    pub credentials_path: PathBuf,
    /// 桌面端置 false：应用包内的可执行文件由安装包管理，
    /// 让代理去 exec 替换它会破坏代码签名，并留下一个装不回去的应用。
    pub allow_self_update: bool,
}

impl Options {
    pub fn new(config_path: impl Into<PathBuf>, credentials_path: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            credentials_path: credentials_path.into(),
            allow_self_update: true,
        }
    }
}

/// 正在运行的服务。持有实际监听地址与关停手柄。
///
/// 丢弃它会关掉 oneshot 发送端，优雅关停信号随之触发——但不等在飞请求走完。
/// 要等就调 [`RunningServer::shutdown`]。
#[derive(Debug)]
pub struct RunningServer {
    addr: SocketAddr,
    data_dir: PathBuf,
    shutdown: tokio::sync::oneshot::Sender<()>,
    joined: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl RunningServer {
    /// 实际监听地址。配置里写 0、或端口被占用而回退时，这里是真正拿到的那个。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 配置与所有运行期文件的落脚点。
    ///
    /// 调用方本来就该能问「我的数据在哪」：CLI 要打进横幅，桌面端要在界面上
    /// 告诉用户——双击启动的人没有任何别的途径知道它。
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// 优雅关停：停止收新连接，等在飞请求走完。
    ///
    /// 这不是可有可无的礼貌。网关的在飞预留要靠请求自己走完既有的结算/释放
    /// 路径；把 axum 任务直接丢弃会在账本上留下永不结算的记录。
    pub async fn shutdown(self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(());
        self.joined.await??;
        Ok(())
    }

    /// 一直服务到收到 Ctrl-C，然后优雅关停。二进制形态用这个。
    ///
    /// 与从前 `axum::serve(...).await.unwrap()` 的区别：那时 Ctrl-C 直接
    /// 杀进程，在飞请求连同网关预留一起被丢弃。
    pub async fn wait_for_signal(self) -> anyhow::Result<()> {
        tokio::signal::ctrl_c().await?;
        tracing::info!("收到中断信号，等待在飞请求结束…");
        self.shutdown().await
    }
}

/// 在当前 tokio 运行时上启动 kiro-rs。
///
/// **不自建运行时**：Tauri 自带一个，同进程两个运行时会让 `Handle::current()`
/// 拿到错误的那个，表现是随机的 "no reactor running" panic。
pub async fn serve(options: Options) -> anyhow::Result<RunningServer> {
    let (app, addr_spec, data_dir) = assemble(&options).await?;
    let listener = bind_with_fallback(addr_spec).await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let joined = tokio::spawn(async move {
        // with_connect_info：把 TCP 对端地址注入请求扩展，直连部署下作为
        // 客户端 IP 的兜底。与二进制从前的行为一致，不能省。
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = rx.await;
        })
        .await
    });
    Ok(RunningServer {
        addr,
        data_dir,
        shutdown: tx,
        joined,
    })
}

/// 配置端口被占用时换一个。
///
/// 桌面端尤其需要：用户可能同时开着一个命令行实例。回退是**明确记录**的，
/// 不是悄悄换掉——调用方拿 `addr()` 得到真实地址，日志里也如实写出来。
///
/// 只对 `AddrInUse` 回退。权限不足、地址不存在这类错误原样上抛，
/// 换个端口并不能解决它们，掩盖只会让人查错方向。
async fn bind_with_fallback(spec: SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(spec).await {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let fallback = SocketAddr::new(spec.ip(), 0);
            let listener = tokio::net::TcpListener::bind(fallback)
                .await
                .map_err(|e2| anyhow::anyhow!("端口 {} 被占用，回退也失败: {e2}", spec.port()))?;
            tracing::warn!("端口 {} 已被占用，改用 {}", spec.port(), listener.local_addr()?);
            Ok(listener)
        }
        Err(e) => Err(anyhow::anyhow!("监听 {spec} 失败: {e}")),
    }
}

/// 装配整个应用：三段依次做完，任一段失败就整体失败。
///
/// 装配失败返回 `Err`——调用方决定怎么死。库不替它做这个决定。
async fn assemble(options: &Options) -> anyhow::Result<(Router, SocketAddr, PathBuf)> {
    let base = foundation(options)?;
    let books = accounting(&base).await?;

    let addr: SocketAddr = format!("{}:{}", base.config.host, base.config.port)
        .parse()
        .map_err(|e| {
            anyhow::anyhow!(
                "监听地址无法解析 {}:{}: {e}",
                base.config.host,
                base.config.port
            )
        })?;

    let data_dir = base.cache_dir.clone();
    Ok((wiring(&base, books, options), addr, data_dir))
}
