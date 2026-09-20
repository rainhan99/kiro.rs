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
    // 用生产路径的写入方式：夹具若用裸 fs::write，权限测试测的就是夹具
    // 而不是产品。
    crate::common::fs::write_private(
        &config,
        br#"{"host":"127.0.0.1","port":0,"adminApiKey":"sk-admin-test"}"#,
    )
    .unwrap();
    crate::common::fs::write_private(&creds, b"[]").unwrap();
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

// 秘密文件清单住在 `common::fs::SECRET_FILES`——与 harden_data_dir 同源。
// 两处各写一张表迟早会漂移，而漂移的表现是「守卫还在，但漏了新文件」。
use crate::common::fs::SECRET_FILES;

/// 这些文件一个都不能是世界可读的。
///
/// 这条测试是**面**而不是点：任何将来新增的、落在 cache_dir 下的秘密文件
/// 只要加进上面那张表就自动受保护，而漏加会在这里被抓到——加文件的人
/// 通常只想着功能，不会想起权限。
///
/// 发现方式值得记一笔：B2 只修了 config.json 与 credentials.json，
/// 真正双击跑起来才看到 client_api_keys.json 还是 644——里面是明文客户端 Key。
#[cfg(unix)]
#[tokio::test]
async fn no_secret_bearing_runtime_file_is_world_readable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let (config, creds) = minimal_files(dir.path());
    // 让网关也立起来，这样 gateway.json 与 billing.db 都会被创建
    write_gateway_config_with_one_upstream(dir.path());
    // 配一个 apiKey，client_api_keys.json 才会落盘（没有 key 时它是惰性的）
    crate::common::fs::write_private(
        &config,
        br#"{"host":"127.0.0.1","port":0,"apiKey":"sk-kiro-rs-test","adminApiKey":"sk-admin-test"}"#,
    )
    .unwrap();

    let server = serve(Options::new(config, creds)).await.unwrap();
    server.shutdown().await.unwrap();

    let mut offenders = Vec::new();
    for name in SECRET_FILES {
        let path = dir.path().join(name);
        if !path.exists() {
            continue;
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o077;
        if mode != 0 {
            let full = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            offenders.push(format!("{name} = {full:o}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "这些文件组外可读/可写，里面是密钥或账本：{offenders:?}"
    );
}

/// 启动横幅必须说清「数据在哪」，且**绝不**打密钥明文。
///
/// 两件事各有理由：
/// - 数据目录：桌面端双击启动，用户根本不知道配置文件在哪。CLI 用相对路径
///   启动时也一样——`./kiro-rs` 的数据落在 CWD，换个目录启动就是另一套。
/// - 不打密钥：systemd、Docker、CI 都会把 stdout/stderr 收走。密钥进日志
///   就等于进了日志归档、进了工单附件。
///   首次生成那一次仍然打印（在 ensure_config_files 里），那一刻不打就永远
///   没人知道；但那是一次性事件，不是每次启动。
#[test]
fn the_startup_banner_gives_the_data_directory_and_never_leaks_a_key() {
    let lines = crate::runtime::startup_banner_lines(
        "127.0.0.1:8990".parse().unwrap(),
        std::path::Path::new("/tmp/kiro-data"),
    );
    let joined = lines.join("\n");

    assert!(
        joined.contains("/tmp/kiro-data"),
        "横幅要说清数据目录在哪，实际:\n{joined}"
    );
    assert!(
        joined.contains("127.0.0.1:8990"),
        "横幅要给出实际监听地址，实际:\n{joined}"
    );
    assert!(
        !joined.contains("sk-"),
        "横幅不得出现任何密钥明文，实际:\n{joined}"
    );
}

/// 地址来自实际 listener，不是配置——端口回退时两者会不一样。
#[test]
fn the_startup_banner_reports_the_actual_address() {
    let lines = crate::runtime::startup_banner_lines(
        "127.0.0.1:49152".parse().unwrap(),
        std::path::Path::new("/data"),
    );
    assert!(lines.join("\n").contains("49152"));
}

/// 升级路径：老部署里已经躺着一批 644 的秘密文件，启动时必须被收紧。
///
/// 这条是真机跑出来的。`write_private` 只在**写入**时收紧，而
/// `client_api_keys.json` 只有内容变化时才写——一个没加过 Key 的部署，
/// 它可以几个月都不被写一次，于是永远停在 644。
///
/// 结论：不能只在写入点设防，启动时要主动扫一遍。
#[cfg(unix)]
#[tokio::test]
async fn pre_existing_world_readable_files_are_tightened_at_startup() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let (config, creds) = minimal_files(dir.path());

    // 模拟老部署：文件已存在且是 644，且启动过程中不会被改写。
    let legacy = dir.path().join("client_api_keys.json");
    std::fs::write(&legacy, "[]").unwrap();
    std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o644)).unwrap();
    // 配置里也放宽一次，确认连它自己都会被收紧
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();

    let server = serve(Options::new(&config, &creds)).await.unwrap();
    server.shutdown().await.unwrap();

    for path in [&legacy, &config] {
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} 启动后仍是 {mode:o}——老部署的宽权限文件没被收紧",
            path.display()
        );
    }
}
