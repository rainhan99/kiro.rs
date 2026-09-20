//! 库入口：在进程内启动 kiro-rs。
//!
//! 这一层的存在理由是桌面应用——它要在自己的进程里把代理跑起来，因此
//! **装配失败必须返回错误，而不是终止进程**。二进制形态下由 `main.rs`
//! 这层薄壳把错误翻译成退出码，行为与从前一致。

use std::path::PathBuf;

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
