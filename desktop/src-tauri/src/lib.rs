//! kiro-rs 桌面应用。
//!
//! 一个原生窗口，进程内启动 kiro-rs，窗口指向它自己内嵌的管理界面。
//! 界面只有一份实现——桌面端与浏览器端看到的是同一个东西。

pub mod paths;
pub mod window;

pub use window::{admin_url, start_proxy};
