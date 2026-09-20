//! kiro-rs 的库目标。
//!
//! 二进制（`src/main.rs`）与桌面应用（`desktop/src-tauri`）都是这个库的使用者。
//! 装配逻辑住在 [`runtime`]，它返回 `Result` 而不是终止进程——桌面端在自己的
//! 进程里跑代理，`std::process::exit` 会把整个应用带走。

pub mod admin;
pub mod admin_ui;
pub mod anthropic;
pub mod common;
pub mod gateway;
pub mod http_client;
pub mod image_resize;
pub mod kiro;
pub mod model;
pub mod pipeline;
pub mod token;

pub mod runtime;

pub use runtime::{Options, RunningServer, serve};

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod runtime_tests;
