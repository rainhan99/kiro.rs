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

#[tokio::test]
async fn falls_back_to_a_free_port_when_the_configured_one_is_taken() {
    let dir = tempfile::tempdir().unwrap();
    // 先自己占住一个端口，再把它写进配置
    let squatter = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = squatter.local_addr().unwrap().port();

    let config = dir.path().join("config.json");
    let creds = dir.path().join("credentials.json");
    std::fs::write(
        &config,
        format!(r#"{{"host":"127.0.0.1","port":{taken},"adminApiKey":"sk-admin-test"}}"#),
    )
    .unwrap();
    std::fs::write(&creds, "[]").unwrap();

    let server = serve(Options::new(config, creds))
        .await
        .expect("应当回退而不是失败");
    assert_ne!(server.addr().port(), taken, "必须换一个端口");
    assert_ne!(server.addr().port(), 0);
    assert!(std::net::TcpStream::connect(server.addr()).is_ok());
    server.shutdown().await.unwrap();
    drop(squatter);
}

/// 回退只针对「端口被占用」。换个端口治不了「这个地址根本不属于本机」，
/// 悄悄回退只会让人对着一个错误的地址查半天。
///
/// 192.0.2.0/24 是 RFC 5737 的 TEST-NET-1，保证不会配在任何真实接口上，
/// 因此这个测试离线可跑、结果确定。
#[tokio::test]
async fn a_non_addr_in_use_bind_error_is_reported_instead_of_silently_retried() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    let creds = dir.path().join("credentials.json");
    std::fs::write(
        &config,
        r#"{"host":"192.0.2.1","port":18080,"adminApiKey":"sk-admin-test"}"#,
    )
    .unwrap();
    std::fs::write(&creds, "[]").unwrap();

    let err = serve(Options::new(config, creds))
        .await
        .expect_err("地址不可用时必须报错，不能回退到 0 号端口假装成功");
    let text = format!("{err:#}");
    assert!(
        text.contains("192.0.2.1"),
        "错误要点名是哪个地址，实际: {text}"
    );
    assert!(
        !text.contains("被占用"),
        "不能把「地址不可用」误报成「端口被占用」，实际: {text}"
    );
}

/// 注：配置加载在 A2 就已经用 `?` 写进 assemble()（拿端口必须先读配置），
/// 所以这条一写出来就是绿的。留着是回归钉——将来谁把它改回 exit(1)，
/// 整个测试二进制会被杀掉，这里会立刻红。
#[tokio::test]
async fn a_broken_config_returns_an_error_instead_of_killing_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    let creds = dir.path().join("credentials.json");
    std::fs::write(&config, "{ this is not json").unwrap();
    std::fs::write(&creds, "[]").unwrap();

    let err = serve(Options::new(config, creds))
        .await
        .expect_err("必须返回错误");
    let text = format!("{err:#}");
    assert!(
        text.contains("加载配置失败"),
        "错误要说清是哪一步崩的，实际: {text}"
    );
}

#[tokio::test]
async fn broken_credentials_return_an_error_instead_of_killing_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    let creds = dir.path().join("credentials.json");
    std::fs::write(
        &config,
        r#"{"host":"127.0.0.1","port":0,"adminApiKey":"sk-admin-test"}"#,
    )
    .unwrap();
    std::fs::write(&creds, "{ not json either").unwrap();

    let err = serve(Options::new(config, creds))
        .await
        .expect_err("必须返回错误");
    let text = format!("{err:#}");
    assert!(text.contains("加载凭证失败"), "实际: {text}");
}
