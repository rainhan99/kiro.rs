//! 把遗留 Key 的积分开账余额一次性搬进账本。
//!
//! # 搬的是「开账余额」，不是历史用量
//!
//! 遗留侧用两个字段表达额度：`total_credits`（累计已用）与 `max_credits`（上限），
//! 剩余 = 上限 − 已用。账本要接着这个状态往下记，所以导入把已用写成账户的初始
//! `used`。这是一笔**开账余额**，不是把历史请求重放一遍——账本里不会因此多出
//! 任何一条请求记录。
//!
//! # 为什么导入过的 Key 绝不再碰
//!
//! 网关接管之后，遗留的 `total_credits` 仍然被老路径继续累加。如果下次启动拿着
//! 这个新数字再导一次，要么把已经计过的量再计一遍，要么因为幂等载荷与首次不符
//! 而让启动直接失败。两种都不能接受。所以判据是**账本里有没有这个积分账户**：
//! 有就跳过，连 `import_legacy` 都不调用。
//!
//! # 封存是一道单向门
//!
//! 全部导完才封存。封存之后新建的 Key 从零开始——它的消耗由账本从头记，再拿遗留
//! 统计导一次就是二次扣费。反过来，只要这一轮有任何一个 Key 被拒，就不封存：把
//! 待修的数据永久锁在门外，比多等一次重启糟得多。

use anyhow::{Result, ensure};

use super::Amount;
use super::ledger::Ledger;

/// f64 能逐一分辨整数的上界（2^53）。超过这里，相邻整数在 f64 里都是同一个值，
/// 再往账本写小数位就是编造精度。
const MAX_EXACT_INTEGER: f64 = 9_007_199_254_740_992.0;

/// 转换保留的小数位。
///
/// f64 在积分这个量级上只有约 15–17 位有效十进制位，写满 `Amount` 的 18 位等于把
/// 浮点累加的噪声当成真金白银记进账本（`0.1 + 0.2` 会变成 `0.30000000000000004`）。
/// 12 位远超积分实际用到的精度，又保证格式化结果必然落在 `Amount` 接受的范围内。
const FRACTION_DIGITS: usize = 12;

/// 一个遗留 Key 的积分状况快照。
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyKeyBalance {
    pub key_id: u64,
    /// 遗留的累计已用 `total_credits`。
    pub used: f64,
    /// 遗留的上限 `max_credits`；`None` 表示不限。
    pub limit: Option<f64>,
}

/// 一次导入的如实结果。三个名单互斥，合起来覆盖全部入参。
#[derive(Debug, Default, PartialEq)]
pub struct ImportReport {
    /// 本轮新建了账户的 Key。
    pub imported: Vec<u64>,
    /// 账本里已经有积分账户，原样保留。
    pub skipped_existing: Vec<u64>,
    /// 数字表示不了，连同原因。
    pub rejected: Vec<(u64, String)>,
    /// 本轮结束后迁移是否已封存。
    pub sealed: bool,
}

/// 遗留 f64 → 账本定点数。这是两套数字体系唯一的接缝。
///
/// 同一个 f64 永远映射到同一个 `Amount`；表示不了的一律报错，**绝不退化成 0**
/// ——把一个未知的已用量当成 0，等于白送一整个额度。
pub fn to_amount(value: f64) -> Result<Amount> {
    ensure!(
        value.is_finite(),
        "legacy credit figure is not a finite number"
    );
    // -0.0 也走这里，避免格式化出一个带负号、`Amount` 不收的字符串。
    if value == 0.0 {
        return Ok(Amount::ZERO);
    }
    ensure!(value > 0.0, "legacy credit figure is negative");
    ensure!(
        value <= MAX_EXACT_INTEGER,
        "legacy credit figure exceeds the range where f64 counts exactly"
    );
    format!("{value:.FRACTION_DIGITS$}").parse()
}

/// 为尚未在账本立户的遗留 Key 建立积分开账余额。
///
/// 返回的名单按 `key_id` 升序，与调用方的遍历顺序无关——遗留 Key 存在 map 里，
/// 顺序本来就不确定，报告不该跟着抖。
pub fn import_opening_balances(ledger: &Ledger, keys: &[LegacyKeyBalance]) -> Result<ImportReport> {
    if ledger.legacy_migration_complete()? {
        // 已封存：此后的 Key 一律由账本从零记起。
        return Ok(ImportReport {
            sealed: true,
            ..ImportReport::default()
        });
    }

    let mut ordered: Vec<&LegacyKeyBalance> = keys.iter().collect();
    ordered.sort_by_key(|k| k.key_id);

    let mut report = ImportReport::default();
    for key in ordered {
        if has_credit_account(ledger, key.key_id)? {
            report.skipped_existing.push(key.key_id);
            continue;
        }
        let converted = to_amount(key.used).and_then(|used| {
            let limit = key.limit.map(to_amount).transpose()?;
            Ok((used, limit))
        });
        match converted {
            Ok((used, limit)) => {
                ledger.import_legacy(key.key_id, used, limit)?;
                report.imported.push(key.key_id);
            }
            Err(error) => report.rejected.push((key.key_id, format!("{error:#}"))),
        }
    }

    // 有一个没搬成就不关门。
    if report.rejected.is_empty() {
        ledger.mark_legacy_migration_complete()?;
        report.sealed = true;
    }
    Ok(report)
}

fn has_credit_account(ledger: &Ledger, key_id: u64) -> Result<bool> {
    Ok(ledger
        .accounts(key_id)?
        .iter()
        .any(|a| a.policy.unit == super::BillingUnit::KiroCredit))
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
