use super::*;

/// `KIRO_DATA_DIR` 是进程级环境变量，动它的测试必须串行——否则一个测试
/// 设上、另一个并发读到，结果随运行顺序变。这种 flake 最难查，因为重跑
/// 常常就好了。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 拿到锁并保证退出时清掉覆盖值。锁中毒（前一个测试 panic）不影响后续，
/// 环境变量该清还是要清。
fn with_clean_env<T>(f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::remove_var("KIRO_DATA_DIR") };
    let out = f();
    unsafe { std::env::remove_var("KIRO_DATA_DIR") };
    out
}

#[test]
fn the_data_directory_is_absolute_and_outside_the_bundle() {
    let dir = with_clean_env(|| data_dir_for("kiro-rs")).expect("必须解析得出");
    assert!(
        dir.is_absolute(),
        "相对路径会跟着 CWD 跑，而桌面端双击启动时 CWD 是 /：{dir:?}"
    );
    assert!(
        !dir.to_string_lossy().contains(".app/"),
        "不能落在 .app 内部——应用包只读，写进去会破坏代码签名：{dir:?}"
    );
}

#[test]
fn first_launch_creates_config_files_in_the_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ensure_data_files(dir.path()).expect("首启动要能建出来");

    assert!(paths.config.exists(), "config.json 没建出来");
    assert!(paths.credentials.exists(), "credentials.json 没建出来");
    assert!(paths.config.is_absolute());
    assert_eq!(
        paths.config.parent(),
        paths.credentials.parent(),
        "两者必须同目录——cache_dir 取的是 credentials 的父目录，\
         分开放会让运行期文件落到别处"
    );
}

#[test]
fn the_generated_config_is_usable_by_the_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ensure_data_files(dir.path()).unwrap();

    // 生成的配置必须能被 kiro-rs 自己读回去。手写一份 JSON 很容易漏字段
    // 或拼错大小写，而那要等到双击应用才发现。
    let config = kiro_rs::model::config::Config::load(&paths.config)
        .expect("生成的配置必须能被代理加载");

    assert_eq!(
        config.host, "127.0.0.1",
        "桌面端必须绑回环。绑 0.0.0.0 会把带凭据的代理暴露到局域网"
    );
    assert!(
        config.api_key.as_deref().is_some_and(|k| k.starts_with("sk-")),
        "要生成可用的客户端 Key"
    );
    // 管理密钥**刻意不生成**：管理权由用户在初始化页自己认领，凭应用
    // 注入的一次性 setup token。一个用户既没设过也记不住的随机串，
    // 最后的下场是被翻出来抄一遍、或者干脆一直不换。
    assert!(
        config.admin_api_key.as_deref().is_none_or(str::is_empty),
        "新配置不该自带管理密钥，实际: {:?}",
        config.admin_api_key
    );
}

#[test]
fn a_second_launch_does_not_overwrite_existing_config() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ensure_data_files(dir.path()).unwrap();
    std::fs::write(&paths.config, r#"{"host":"127.0.0.1","port":9999}"#).unwrap();
    std::fs::write(&paths.credentials, r#"[{"accessToken":"mine"}]"#).unwrap();

    ensure_data_files(dir.path()).unwrap();

    let config_after = std::fs::read_to_string(&paths.config).unwrap();
    assert!(config_after.contains("9999"), "第二次启动把用户配置覆盖了");
    let creds_after = std::fs::read_to_string(&paths.credentials).unwrap();
    assert!(creds_after.contains("mine"), "第二次启动把用户凭据覆盖了");
}

/// 生成的配置里有 adminApiKey 与 apiKey。世界可读的话，同机任何进程都能
/// 拿走它们——桌面端尤其容易中招，因为用户不会想到去看权限。
#[cfg(unix)]
#[test]
fn the_generated_config_is_not_world_readable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let paths = ensure_data_files(dir.path()).unwrap();

    for path in [&paths.config, &paths.credentials] {
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} 权限应为 0600，实际 {mode:o}",
            path.display()
        );
    }
}

/// KIRO_DATA_DIR 只给测试与离线冒烟用，但既然它存在，就不能接受相对路径——
/// 相对路径会跟着 CWD 跑，而桌面端双击启动时 CWD 是 /。
#[test]
fn a_relative_data_dir_override_is_rejected_rather_than_silently_used() {
    let result = with_clean_env(|| {
        unsafe { std::env::set_var("KIRO_DATA_DIR", "some/relative/path") };
        data_dir_for("kiro-rs")
    });

    let err = result.expect_err("相对路径必须被拒绝");
    assert!(
        format!("{err:#}").contains("绝对路径"),
        "实际: {err:#}"
    );
}
