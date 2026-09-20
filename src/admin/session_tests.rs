use super::*;
use std::time::Duration;

fn store(ttl_hours: u64) -> SessionStore {
    SessionStore::new(ttl_hours)
}

#[test]
fn a_fresh_token_is_accepted() {
    let s = store(1);
    let issued = s.issue();
    assert!(s.validate(&issued.token));
}

#[test]
fn an_unknown_token_is_rejected() {
    let s = store(1);
    s.issue();
    assert!(!s.validate("not-a-real-token"));
    assert!(!s.validate(""));
}

/// token 要足够长。它等价于管理密码，短了就能爆破。
#[test]
fn the_token_is_long_and_never_repeats() {
    let s = store(1);
    let a = s.issue().token;
    let b = s.issue().token;
    assert!(a.len() >= 32, "token 太短：{} 位", a.len());
    assert_ne!(a, b);
}

/// TTL = 0 表示不过期。这是默认值，与引入会话之前的行为一致——
/// 升级上来的人不该某天突然被要求重新登录。
#[test]
fn a_zero_ttl_never_expires() {
    let s = store(0);
    let issued = s.issue();
    assert!(issued.expires_at.is_none(), "不过期就不该有到期时间");
    s.advance_clock_for_test(Duration::from_secs(86_400 * 365));
    assert!(s.validate(&issued.token), "TTL=0 的会话不该过期");
}

/// 闲置跨过 TTL 就失效。
///
/// 注意不能在中途 validate 一次再断言过期——那一次调用**就是**滑动续期，
/// 会把到期时间推后。这条测试第一版就是这么写错的。
#[test]
fn an_idle_session_expires_after_its_ttl() {
    let s = store(1);
    let issued = s.issue();
    assert!(issued.expires_at.is_some());

    s.advance_clock_for_test(Duration::from_secs(3600 - 1));
    // 这里**故意不**调 validate

    s.advance_clock_for_test(Duration::from_secs(2));
    assert!(!s.validate(&issued.token), "闲置超过 TTL 必须失效");
}

/// 快到点但还没到时仍然有效——「1 小时过期」不该提前把人踢掉。
///
/// 不测**恰好** 3600 秒那一刻：`issue` 与 `validate` 之间真实时钟会走
/// 几微秒，那个断言会随机红。测「差一秒」既稳定又抓得住「提前过期」。
#[test]
fn a_session_survives_until_just_before_the_boundary() {
    let s = store(1);
    let issued = s.issue();
    s.advance_clock_for_test(Duration::from_secs(3599));
    assert!(s.validate(&issued.token), "还差一秒就被踢了");
}

/// 滑动续期：一直在用就不该被踢出去。
///
/// 绝对过期会让人在填一半表单时被登出，而那正是最恼人的时刻。
#[test]
fn activity_slides_the_expiry_forward() {
    let s = store(1);
    let token = s.issue().token;

    // 每隔 50 分钟动一次，总共跨过 2.5 小时——绝对过期早该踢了
    for _ in 0..3 {
        s.advance_clock_for_test(Duration::from_secs(3000));
        assert!(s.validate(&token), "活跃会话不该过期");
    }

    // 停下来超过 TTL 就该没了
    s.advance_clock_for_test(Duration::from_secs(3601));
    assert!(!s.validate(&token));
}

#[test]
fn logging_out_kills_that_token_immediately() {
    let s = store(1);
    let a = s.issue().token;
    let b = s.issue().token;

    s.revoke(&a);
    assert!(!s.validate(&a));
    assert!(s.validate(&b), "登出一个不该影响另一个");
}

/// 过期的条目要被清掉，否则一个长跑的服务会随登录次数无限涨。
#[test]
fn expired_entries_are_swept_not_merely_rejected() {
    let s = store(1);
    for _ in 0..50 {
        s.issue();
    }
    assert_eq!(s.len_for_test(), 50);

    s.advance_clock_for_test(Duration::from_secs(3601));
    s.issue(); // 签发时顺手清扫
    assert_eq!(s.len_for_test(), 1, "过期条目没被清掉");
}

/// 改 TTL 之后，**已有**会话按新 TTL 算。
///
/// 反过来（老会话沿用老 TTL）会让「我把过期时间调短了」这个动作
/// 在最需要它生效的那一刻不生效。
#[test]
fn changing_the_ttl_applies_to_existing_sessions() {
    let s = store(24);
    let token = s.issue().token;

    s.set_ttl_hours(1);
    s.advance_clock_for_test(Duration::from_secs(3601));
    assert!(!s.validate(&token), "调短 TTL 后老会话也该按新值过期");
}

/// 关掉过期（改成 0）之后，已经过期的会话不该复活。
#[test]
fn disabling_expiry_does_not_resurrect_a_dead_session() {
    let s = store(1);
    let token = s.issue().token;
    s.advance_clock_for_test(Duration::from_secs(3601));
    assert!(!s.validate(&token));

    s.set_ttl_hours(0);
    assert!(!s.validate(&token), "已经死掉的会话不能因为关了过期就复活");
}
