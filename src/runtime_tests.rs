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

/// 最小可用配置：不加凭据、不开网关，只要能把 HTTP 面起起来。
/// host 写死 127.0.0.1 而不是靠默认值——测试要连回自己，绑到什么地址必须确定。
fn minimal_files(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let config = dir.join("config.json");
    let creds = dir.join("credentials.json");
    std::fs::write(
        &config,
        r#"{"host":"127.0.0.1","port":0,"adminApiKey":"sk-admin-test"}"#,
    )
    .unwrap();
    std::fs::write(&creds, "[]").unwrap();
    (config, creds)
}

#[tokio::test]
async fn serve_reports_its_address_and_stops_on_request() {
    let dir = tempfile::tempdir().unwrap();
    let (config, creds) = minimal_files(dir.path());

    let server = serve(Options::new(config, creds)).await.expect("装配成功");
    let addr = server.addr();
    assert_ne!(addr.port(), 0, "必须报告实际端口，不能把 0 原样回传");

    // 起来了就必须真的在监听
    let probe = std::net::TcpStream::connect(addr);
    assert!(probe.is_ok(), "serve() 返回后端口必须已经在 listen");
    drop(probe);

    server.shutdown().await.expect("优雅关停");

    // 停了就必须真的不再监听
    assert!(
        std::net::TcpStream::connect(addr).is_err(),
        "shutdown() 返回后端口必须已经释放"
    );
}
