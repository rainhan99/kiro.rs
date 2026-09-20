use super::*;

#[test]
fn the_data_directory_is_absolute_and_outside_the_bundle() {
    // 不受测试环境里可能存在的覆盖影响
    unsafe { std::env::remove_var("KIRO_DATA_DIR") };

    let dir = data_dir_for("kiro-rs").expect("必须解析得出");
    assert!(
        dir.is_absolute(),
        "相对路径会跟着 CWD 跑，而桌面端双击启动时 CWD 是 /：{dir:?}"
    );
    assert!(
        !dir.to_string_lossy().contains(".app/"),
        "不能落在 .app 内部——应用包只读，写进去会破坏代码签名：{dir:?}"
    );
}
