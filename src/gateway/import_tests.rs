use super::*;
use crate::gateway::ledger::Ledger;
use crate::gateway::{BillingUnit, BudgetEnforcement, BudgetPolicy};

fn ledger() -> Ledger {
    Ledger::open_in_memory().unwrap()
}

fn key(id: u64, used: f64, limit: Option<f64>) -> LegacyKeyBalance {
    LegacyKeyBalance {
        key_id: id,
        used,
        limit,
    }
}

fn credit(ledger: &Ledger, id: u64) -> Option<super::super::ledger_types::AccountView> {
    ledger
        .accounts(id)
        .unwrap()
        .into_iter()
        .find(|a| a.policy.unit == BillingUnit::KiroCredit)
}

/// 开账余额要如实带上遗留的「已用」与「上限」，剩余额度才对得上。
#[test]
fn an_opening_balance_carries_the_legacy_ceiling_and_consumption() {
    let l = ledger();
    let report = import_opening_balances(&l, &[key(1, 3.5, Some(10.0))]).unwrap();
    assert_eq!(report.imported, vec![1]);

    let account = credit(&l, 1).expect("应建出积分账户");
    assert_eq!(account.used, "3.5".parse().unwrap());
    assert_eq!(account.policy.limit, Some("10".parse().unwrap()));
    assert_eq!(account.available, Some("6.5".parse().unwrap()));
}

/// 最关键的一条：导入必须幂等，且**遗留计数器仍在继续增长**这件事不能把它变成冲突。
/// 遗留侧的 `total_credits` 在网关接管后仍会被老路径累加；如果第二次启动拿着新数字
/// 再导一遍，要么二次扣费，要么因为幂等载荷对不上直接启动失败。两种都不行。
///
/// 这里故意夹一个坏 Key 让迁移保持未封存：封存会让第二轮直接短路，那样测到的是
/// 「门关了」，而不是「已立户的 Key 被认了出来」——后者才是真正的守卫。
#[test]
fn a_second_pass_neither_charges_again_nor_fails_on_a_moved_counter() {
    let l = ledger();
    let keep_open = key(2, f64::NAN, None);
    import_opening_balances(&l, &[key(1, 3.5, Some(10.0)), keep_open.clone()]).unwrap();
    assert!(!l.legacy_migration_complete().unwrap(), "前提：迁移仍开着");

    // 老路径继续记账，数字变了。
    let report = import_opening_balances(&l, &[key(1, 7.25, Some(10.0)), keep_open]).unwrap();
    assert_eq!(report.imported, Vec::<u64>::new());
    assert_eq!(report.skipped_existing, vec![1]);

    let account = credit(&l, 1).unwrap();
    assert_eq!(account.used, "3.5".parse().unwrap(), "开账余额不得被改写");
}

/// 迁移封存之后，这个调用是个彻底的空操作——不再逐 Key 判定，也不报错。
#[test]
fn a_sealed_migration_makes_the_call_a_no_op() {
    let l = ledger();
    assert!(
        import_opening_balances(&l, &[key(1, 1.0, None)])
            .unwrap()
            .sealed
    );

    let report = import_opening_balances(&l, &[key(1, 9.0, None), key(2, 5.0, None)]).unwrap();
    assert_eq!(
        report,
        ImportReport {
            sealed: true,
            ..ImportReport::default()
        }
    );
    assert_eq!(credit(&l, 1).unwrap().used, "1".parse().unwrap());
}

/// 不限额的 Key 导进来仍然不限额——不得顺手安一个上限。
#[test]
fn a_key_without_a_ceiling_imports_as_unlimited() {
    let l = ledger();
    import_opening_balances(&l, &[key(1, 2.0, None)]).unwrap();
    let account = credit(&l, 1).unwrap();
    assert_eq!(account.policy.limit, None);
    assert_eq!(account.available, None);
}

/// 已经超额的 Key：剩余是 0，不是负数。
#[test]
fn consumption_beyond_the_ceiling_leaves_nothing_available() {
    let l = ledger();
    import_opening_balances(&l, &[key(1, 12.0, Some(10.0))]).unwrap();
    let account = credit(&l, 1).unwrap();
    assert_eq!(account.used, "12".parse().unwrap());
    assert_eq!(account.available, Some(Amount::ZERO));
}

/// 表示不了的数字要被点名拒绝，**不能悄悄当成 0**——那等于白送一整个额度。
/// 且一个坏 Key 不能拖垮整批。
#[test]
fn an_unrepresentable_figure_is_named_not_silently_zeroed() {
    let l = ledger();
    let report = import_opening_balances(
        &l,
        &[
            key(1, f64::NAN, Some(10.0)),
            key(2, f64::INFINITY, None),
            key(3, -1.0, None),
            key(4, 1e300, None),
            key(5, 1.0, Some(f64::NAN)),
            key(6, 2.0, Some(10.0)), // 好的那个仍要导进去
        ],
    )
    .unwrap();

    assert_eq!(report.imported, vec![6], "坏数据不得拖垮整批");
    let rejected: Vec<u64> = report.rejected.iter().map(|(id, _)| *id).collect();
    assert_eq!(rejected, vec![1, 2, 3, 4, 5]);
    for (id, reason) in &report.rejected {
        assert!(!reason.trim().is_empty(), "key {id} 被拒必须给出原因");
    }
    for id in [1, 2, 3, 4, 5] {
        assert!(credit(&l, id).is_none(), "key {id} 不该建出账户");
    }
}

/// 有 Key 被拒时不封存迁移：封存是一道单向门，把待修的数据永久锁在门外。
#[test]
fn a_rejected_key_leaves_the_migration_open_for_a_fix() {
    let l = ledger();
    let report = import_opening_balances(&l, &[key(1, f64::NAN, None), key(2, 1.0, None)]).unwrap();
    assert!(!report.sealed, "有拒绝就不能封存");
    assert!(!l.legacy_migration_complete().unwrap());

    // 运维修好数据后重跑：补进来，这次才封存。
    let report = import_opening_balances(&l, &[key(1, 4.0, None), key(2, 1.0, None)]).unwrap();
    assert_eq!(report.imported, vec![1]);
    assert_eq!(report.skipped_existing, vec![2]);
    assert!(report.sealed);
    assert!(l.legacy_migration_complete().unwrap());
    assert_eq!(credit(&l, 1).unwrap().used, "4".parse().unwrap());
}

/// 封存之后新建的 Key 从零开始，不再走遗留导入——它的用量是新账本记的，
/// 拿遗留统计再导一次就是二次扣费。
#[test]
fn a_key_created_after_the_seal_is_not_imported() {
    let l = ledger();
    let report = import_opening_balances(&l, &[key(1, 1.0, None)]).unwrap();
    assert!(report.sealed);

    let report =
        import_opening_balances(&l, &[key(1, 1.0, None), key(9, 50.0, Some(99.0))]).unwrap();
    assert_eq!(report.imported, Vec::<u64>::new());
    assert!(credit(&l, 9).is_none(), "封存后不得再导入");
}

/// 账本里已有的账户原样保留：它可能是运维手工设过的，遗留数字不该盖掉它。
#[test]
fn an_existing_account_is_left_verbatim() {
    let l = ledger();
    l.set_account(
        1,
        BudgetPolicy {
            unit: BillingUnit::KiroCredit,
            limit: Some("99".parse().unwrap()),
            enforcement: BudgetEnforcement::Soft,
            max_in_flight: 4,
            max_pending: 8,
            allowed_models: vec![],
            allowed_upstreams: vec![],
        },
    )
    .unwrap();

    let report = import_opening_balances(&l, &[key(1, 3.5, Some(10.0))]).unwrap();
    assert_eq!(report.skipped_existing, vec![1]);

    let account = credit(&l, 1).unwrap();
    assert_eq!(
        account.policy.limit,
        Some("99".parse().unwrap()),
        "不得被遗留值覆盖"
    );
    assert_eq!(account.used, Amount::ZERO);
    assert_eq!(account.policy.max_in_flight, 4);
}

/// f64 → 定点数是两套数字体系唯一的接缝。它必须确定，且不得把浮点噪声
/// 当成真金白银记进账本。
#[test]
fn the_conversion_is_deterministic_and_does_not_invent_precision() {
    // 0.1+0.2 在 f64 里是 0.30000000000000004；记进账本的应是 0.3。
    let noisy = 0.1_f64 + 0.2_f64;
    assert_ne!(noisy, 0.3_f64, "前提：这个和确实带噪声");
    assert_eq!(to_amount(noisy).unwrap(), "0.3".parse().unwrap());

    // 同一个输入永远得到同一个输出。
    assert_eq!(to_amount(noisy).unwrap(), to_amount(noisy).unwrap());

    // 整数与常见小数原样通过。
    assert_eq!(to_amount(0.0).unwrap(), Amount::ZERO);
    assert_eq!(to_amount(1.0).unwrap(), "1".parse().unwrap());
    assert_eq!(to_amount(2.5).unwrap(), "2.5".parse().unwrap());
    assert_eq!(to_amount(0.000001).unwrap(), "0.000001".parse().unwrap());
}

/// 超出 f64 整数精度的数字要拒绝：那个量级上 f64 连相邻整数都分不开，
/// 再往账本里写 12 位小数是编造。
#[test]
fn a_figure_beyond_integer_precision_is_refused() {
    // 先钉住这道上界存在的理由：2^53+1 这个 f64 根本不存在，它就是 2^53。
    // （最初这条测试用 2^53+1 当"越界值"，于是断言了一个界内的数——正是
    //  这种静默取整让"再多写 12 位小数"变成编造。）
    assert_eq!(9_007_199_254_740_993.0_f64, 9_007_199_254_740_992.0_f64);
    assert!(to_amount(9_007_199_254_740_994.0).is_err());
    assert!(to_amount(f64::MAX).is_err());
    assert!(to_amount(-0.5).is_err());
    assert!(to_amount(f64::NAN).is_err());
    assert!(to_amount(f64::INFINITY).is_err());
    // 边界本身可用。
    assert!(to_amount(9_007_199_254_740_992.0).is_ok());
}
