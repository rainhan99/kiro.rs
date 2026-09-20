use super::*;

/// 老部署一律视为已初始化。这条排第一——引入新特性把现有用户锁在门外
/// 是最不可接受的失败。
#[test]
fn an_existing_deployment_is_already_initialized_and_gets_no_token() {
    let state = SetupState::from_configured_key(Some("sk-admin-existing"));
    assert!(state.initialized());
    assert!(state.token().is_none(), "已初始化就不该发 token");
}

#[test]
fn a_fresh_deployment_is_uninitialized_and_gets_a_token() {
    let state = SetupState::from_configured_key(None);
    assert!(!state.initialized());
    let token = state.token().expect("未初始化必须发 token");
    assert!(token.len() >= 32, "token 太短，实际 {} 位", token.len());
}

/// 空字符串与空白串不算「有密钥」——配置里 `"adminApiKey": ""` 是常见手滑，
/// 把它当成已初始化会让人永远进不去：既没密码，也拿不到 token。
#[test]
fn a_blank_configured_key_counts_as_uninitialized() {
    for blank in ["", "   ", "\t"] {
        let state = SetupState::from_configured_key(Some(blank));
        assert!(!state.initialized(), "空白串 {blank:?} 不该算已初始化");
        assert!(state.token().is_some());
    }
}

/// token 只存内存：每次启动都是新的。落盘就多一个会被备份、被打包、
/// 被 scp 走的秘密。
#[test]
fn every_start_mints_a_different_token() {
    let a = SetupState::from_configured_key(None).token().unwrap();
    let b = SetupState::from_configured_key(None).token().unwrap();
    assert_ne!(a, b);
}

#[test]
fn the_right_token_is_accepted_once_and_then_the_door_closes() {
    let state = SetupState::from_configured_key(None);
    let token = state.token().unwrap();

    assert!(state.consume(&token), "第一次应当通过");
    assert!(state.initialized(), "用掉之后就算已初始化");
    assert!(
        !state.consume(&token),
        "同一个 token 不能用第二次——重放就等于第二个人也能认领管理员"
    );
}

#[test]
fn a_wrong_token_is_rejected_and_does_not_burn_the_real_one() {
    let state = SetupState::from_configured_key(None);
    let token = state.token().unwrap();

    assert!(!state.consume("nope"));
    assert!(!state.initialized(), "试错不该让状态前进");
    assert!(state.consume(&token), "真 token 仍然有效");
}

/// 已初始化后即使给出「曾经正确」的 token 也要拒绝。
#[test]
fn an_initialized_instance_refuses_every_token() {
    let state = SetupState::from_configured_key(Some("sk-admin-existing"));
    assert!(!state.consume("anything"));
    assert!(!state.consume(""));
}

#[test]
fn a_too_short_or_empty_admin_key_is_rejected() {
    assert!(validate_admin_key("").is_err());
    assert!(validate_admin_key("   ").is_err());
    assert!(validate_admin_key("short").is_err());
    assert_eq!(validate_admin_key("  goodpassword  ").unwrap(), "goodpassword");
}

/// 长度按**字符**算不按字节算：中文密码「密码密码」是 4 个字符 12 个字节，
/// 按字节算会让它蒙混过关。
#[test]
fn the_length_check_counts_characters_not_bytes() {
    assert!(
        validate_admin_key("密码密码").is_err(),
        "4 个汉字是 12 字节，但只有 4 个字符"
    );
    assert!(validate_admin_key("密码密码密码密码").is_ok(), "8 个字符应当通过");
}
