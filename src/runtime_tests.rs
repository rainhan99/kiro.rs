//! 库入口的测试。
//!
//! 挂载范式与 `gateway/ledger.rs`、`pipeline/mod.rs` 一致：
//! `#[cfg(test)] #[path = "runtime_tests.rs"] mod runtime_tests;`

use super::*;

/// 库目标存在的最小证明：这些类型必须能从库根被外部 crate 看见。
/// desktop crate 就是那个外部 crate。
#[test]
fn library_root_exports_the_entry_points() {
    let opts = Options::new("config.json", "credentials.json");
    assert_eq!(opts.config_path.file_name().unwrap(), "config.json");
    assert_eq!(
        opts.credentials_path.file_name().unwrap(),
        "credentials.json"
    );
    assert!(opts.allow_self_update, "二进制形态下自更新默认开着");
}
