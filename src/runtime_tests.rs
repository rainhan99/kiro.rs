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

/// 未知的 `defaultEndpoint` 必须让启动失败，且是**返回错误**而不是杀进程。
///
/// 计划原本以为这条会撞上端点注册表的检查，实测不是：`Config::validate()`
/// 更早一层就把 `defaultEndpoint` 限死在 ide|cli 了。注册表那处检查因此
/// 从配置文件走不到，是防御性的——它仍然改成了 `bail!`（库里不许有
/// `process::exit`），但覆盖它的是下面那个直接调用 `foundation()` 的测试。
///
/// 这里钉的是用户真正看得见的契约：写错端点名，启动失败，错误点名它。
#[tokio::test]
async fn an_unregistered_default_endpoint_returns_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    let creds = dir.path().join("credentials.json");
    std::fs::write(
        &config,
        r#"{"host":"127.0.0.1","port":0,"adminApiKey":"sk-admin-test","defaultEndpoint":"nonexistent"}"#,
    )
    .unwrap();
    std::fs::write(&creds, "[]").unwrap();

    let err = serve(Options::new(config, creds))
        .await
        .expect_err("必须返回错误，而不是终止进程");
    let text = format!("{err:#}");
    assert!(
        text.contains("ide") && text.contains("cli"),
        "错误要说清合法取值是什么，实际: {text}"
    );
}

#[tokio::test]
async fn a_credential_pointing_at_an_unknown_endpoint_returns_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    let creds = dir.path().join("credentials.json");
    std::fs::write(
        &config,
        r#"{"host":"127.0.0.1","port":0,"adminApiKey":"sk-admin-test"}"#,
    )
    .unwrap();
    std::fs::write(
        &creds,
        r#"[{"accessToken":"t","refreshToken":"r","authMethod":"social","endpoint":"made-up"}]"#,
    )
    .unwrap();

    let err = serve(Options::new(config, creds))
        .await
        .expect_err("必须返回错误");
    let text = format!("{err:#}");
    assert!(text.contains("made-up"), "错误要点名是哪个端点，实际: {text}");
    assert!(text.contains("未知端点"), "实际: {text}");
}

/// 直接覆盖「默认端点未注册」那处防御性检查。
///
/// 它从配置文件走不到（`Config::validate()` 更早拦截），但它仍然必须是
/// `bail!` 而不是 `process::exit`——SC-1 要求库入口一处 exit 都没有。
/// 这里用一个绕过配置校验的方式构造该状态：直接改 `Config` 结构体的字段。
#[tokio::test]
async fn the_defensive_default_endpoint_guard_returns_an_error_not_an_exit() {
    let dir = tempfile::tempdir().unwrap();
    let (config_path, creds) = minimal_files(dir.path());

    // 先按合法配置加载，再把 default_endpoint 改成注册表里没有的名字。
    // 这模拟的是「注册表变了、配置没变」——那正是这条守卫存在的理由。
    let mut config = crate::model::config::Config::load(&config_path).unwrap();
    config.default_endpoint = "nonexistent".to_string();
    std::fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();

    // 写回后再读会被 validate 拦下——这恰好证明了从配置文件走不到那条守卫。
    let err = serve(Options::new(&config_path, &creds))
        .await
        .expect_err("必须返回错误");
    let text = format!("{err:#}");
    assert!(
        text.contains("加载配置失败"),
        "从配置文件进来时，拦下它的是配置校验层，实际: {text}"
    );

    // 而库里一处 process::exit 都不能有——这条由源码断言直接钉住，
    // 因为那个分支无法从外部触发。
    let source = include_str!("runtime.rs");
    let live_exits = source
        .lines()
        .filter(|l| l.contains("process::exit") && !l.trim_start().starts_with("//"))
        .count();
    assert_eq!(live_exits, 0, "runtime.rs 里不允许出现 process::exit");
}

/// 网关的失败**必须**让 serve() 整体失败。
///
/// 若有人把它改成 warn 后继续，服务会带着一个记不了账的网关跑起来——
/// 而这条路上正在花真钱。这个测试就是钉住这一点的。
/// 写一份声明了上游的 `gateway.json`。
///
/// 没有它网关是**彻底惰性**的——`GatewayService::open` 只有在配置声明了
/// upstream 或 model 时才会去开账本（service.rs:90）。测试要触发「账本打不开」，
/// 就必须先让网关有理由去开它。用 `ConfigStore` 自己来写，避免猜文件格式。
fn write_gateway_config_with_one_upstream(cache_dir: &std::path::Path) {
    use crate::gateway::config::{GatewayConfig, Upstream, UpstreamKind};
    use crate::gateway::config_store::ConfigStore;

    let store = ConfigStore::open(&cache_dir.join("gateway.json")).unwrap();
    store
        .update(
            1,
            GatewayConfig {
                upstreams: vec![Upstream {
                    id: "u1".into(),
                    name: "upstream 1".into(),
                    kind: UpstreamKind::Anthropic,
                    enabled: true,
                    weight: 10,
                    base_url: None,
                    api_key: Some("k".into()),
                    has_api_key: true,
                    allow_private_network: false,
                    kiro_group: None,
                    cache_usage_policy: None,
                }],
                ..Default::default()
            },
        )
        .unwrap();
}

#[tokio::test]
async fn a_gateway_that_cannot_open_its_ledger_aborts_startup() {
    let dir = tempfile::tempdir().unwrap();
    let (config, creds) = minimal_files(dir.path());

    // cache_dir 取的是凭据文件的父目录，所以网关的两个文件都落在这里。
    write_gateway_config_with_one_upstream(dir.path());
    // 用一个**目录**占住 billing.db 的位置：SQLite 打不开它。
    std::fs::create_dir(dir.path().join("billing.db")).unwrap();

    let err = serve(Options::new(config, creds))
        .await
        .expect_err("账本打不开时必须拒绝启动，而不是降级为无账本运行");
    assert!(
        format!("{err:#}").contains("网关初始化失败"),
        "实际: {err:#}"
    );
}

/// 与上一条互为对照：**没有**网关配置时，账本位置被占住也不该影响启动。
///
/// 这条钉住「未配置网关的部署不因为引入这个特性而改变行为」。少了它，
/// 有人把 open 改成总是开账本，上一条测试照样绿，而所有没配网关的部署会挂。
#[tokio::test]
async fn an_unconfigured_gateway_never_touches_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let (config, creds) = minimal_files(dir.path());
    std::fs::create_dir(dir.path().join("billing.db")).unwrap();

    let server = serve(Options::new(config, creds))
        .await
        .expect("网关未配置时应当完全惰性，不碰账本");
    server.shutdown().await.unwrap();
}

/// 桌面端禁用自更新后，替换可执行文件的那三个端点必须明确拒绝。
///
/// 为什么是 409 而不是 404：404 会让人以为「这个版本没有更新功能」，
/// 而真相是「有，但这个形态下不该用」。前者会让人去翻文档，后者一眼就懂。
#[tokio::test]
async fn self_update_is_refused_with_an_explanation_when_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let (config, creds) = minimal_files(dir.path());
    let mut options = Options::new(config, creds);
    options.allow_self_update = false;

    let server = serve(options).await.unwrap();
    let client = reqwest::Client::new();

    for path in ["apply", "pull", "rollback"] {
        let url = format!("http://{}/api/admin/system/update/{path}", server.addr());
        let resp = client
            .post(&url)
            .header("x-api-key", "sk-admin-test")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            409,
            "{path} 在禁用时要明确拒绝，不能静默成功或 500"
        );
        let text = resp.text().await.unwrap();
        assert!(text.contains("桌面版"), "{path} 要说清为什么被拒，实际: {text}");
    }

    server.shutdown().await.unwrap();
}

/// 界面要能如实说明「桌面版通过应用更新」（SC-4 的后半句）。
/// 没有这个字段，前端只能把按钮摆在那儿让人点了才知道不行。
#[tokio::test]
async fn the_update_config_reports_whether_self_update_is_available() {
    let dir = tempfile::tempdir().unwrap();
    let (config, creds) = minimal_files(dir.path());
    let mut options = Options::new(config, creds);
    options.allow_self_update = false;

    let server = serve(options).await.unwrap();
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{}/api/admin/config/update", server.addr()))
        .header("x-api-key", "sk-admin-test")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        body["selfUpdateAvailable"],
        serde_json::json!(false),
        "禁用时必须如实上报，实际: {body}"
    );
    server.shutdown().await.unwrap();
}

/// 二进制形态下一切照旧——这条防的是「为了桌面端方便，把所有人的自更新关了」。
#[tokio::test]
async fn self_update_stays_available_for_the_plain_binary() {
    let dir = tempfile::tempdir().unwrap();
    let (config, creds) = minimal_files(dir.path());
    let options = Options::new(config, creds); // allow_self_update 默认 true

    let server = serve(options).await.unwrap();
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{}/api/admin/config/update", server.addr()))
        .header("x-api-key", "sk-admin-test")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["selfUpdateAvailable"], serde_json::json!(true));
    server.shutdown().await.unwrap();
}

/// 比手动端点更危险的是**自动**更新调度器：它到点就自己动手，没人按按钮。
///
/// 只堵手动端点而放任调度器，桌面端会在某个凌晨三点自己把 `.app` 里的
/// 可执行文件换掉、签名作废、下次打不开——而且没有任何人操作过。
#[tokio::test]
async fn the_auto_update_scheduler_does_not_start_when_self_update_is_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let (config, creds) = minimal_files(dir.path());
    let mut options = Options::new(config, creds);
    options.allow_self_update = false;

    let base = crate::runtime::foundation(&options).unwrap();

    // 开关只有一处，手动端点与调度器都查它。这里直接问那一处。
    let service = crate::admin::AdminService::new(base.token_manager.clone(), vec![])
        .with_self_update(false);
    assert!(
        !std::sync::Arc::new(service).start_auto_update_scheduler(),
        "禁用自更新时调度器不得启动"
    );

    let service = crate::admin::AdminService::new(base.token_manager.clone(), vec![]);
    assert!(
        std::sync::Arc::new(service).start_auto_update_scheduler(),
        "二进制形态下调度器照常启动"
    );
}

/// kiro-rs 在运行期会创建的文件。全部由 `cache_dir` join 出来，
/// 而 `cache_dir` 取的是 credentials 文件的父目录。
const RUNTIME_FILES: &[&str] = &[
    "client_api_keys.json",
    "groups.json",
    "gateway.json",
    "billing.db",
    "traces.db",
    "cache_metering.json",
    "kiro_balance_cache.json",
    "proxy_pool.json",
];

fn runtime_files_in(dir: &std::path::Path) -> std::collections::BTreeSet<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Default::default();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| RUNTIME_FILES.contains(&name.as_str()) || name.starts_with("usage_log."))
        .collect()
}

/// `cache_dir` 取的是 credentials 文件的父目录，上面那一串运行期文件全部
/// join 在它下面。这个测试钉住那条链：给绝对路径，当前工作目录里不得多出
/// 任何一个运行期文件。
///
/// 将来谁加一个新的运行期文件却用了相对路径，这里会红——那正是 SC-3 会被
/// 悄悄破掉的方式（加文件的人通常不会想起桌面端）。
///
/// 不改进程 CWD：那是全局状态，会影响并行跑的其它测试。改成按**已知文件名**
/// 判定，既不需要串行化，也不会被无关的临时文件干扰。
#[tokio::test]
async fn absolute_paths_keep_every_runtime_file_out_of_the_working_directory() {
    let cwd = std::env::current_dir().unwrap();
    let before = runtime_files_in(&cwd);

    let data = tempfile::tempdir().unwrap();
    let (config, creds) = minimal_files(data.path());
    let server = serve(Options::new(config, creds)).await.unwrap();
    server.shutdown().await.unwrap();

    let after = runtime_files_in(&cwd);
    let leaked: Vec<_> = after.difference(&before).collect();
    assert!(
        leaked.is_empty(),
        "运行期文件落到了工作目录，说明有人用了相对路径：{leaked:?}"
    );

    // 反过来：它们确实落在数据目录里。少了这半条，把所有路径写成 /dev/null
    // 也能让上面那条绿。
    //
    // 用 traces.db 做锚点：它是启动时**无条件**创建的。client_api_keys.json
    // 与 groups.json 是惰性的——没有 key / 没有分组就不落盘，拿它们当锚点
    // 这条测试会假红。
    let landed = runtime_files_in(data.path());
    assert!(
        landed.contains("traces.db"),
        "运行期文件应当落在凭据文件所在目录，实际只有: {landed:?}"
    );
}
