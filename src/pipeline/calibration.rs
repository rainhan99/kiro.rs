//! 被动分母校准。
//!
//! 上游的 `contextUsageEvent` 只给一个百分比，没说分母是什么；本项目当前用一张写死
//! 的模型名窗口表去乘它，误差因此不可见。但**原生 `metadataEvent.tokenUsage` 与该
//! 百分比会在同一次响应里一起到达**，两者一比就能反推上游实际用的分母。
//!
//! 这条推导只消费本来就会发生的业务流量：不发探测请求、不做阈值二分、不重放、不施加
//! 压力、不为了取样而强制换号。没有样本就是没有样本，绝不为了凑数而放宽条件。
//!
//! 推导结果是「在已见样本上对某个 provider 算术的观察」，**不是**实测或公布的上游上
//! 限。它不修改任何配置，也不会自己流入准入判定。

use serde::{Deserialize, Serialize};

/// 一次可用的校准样本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationSample {
    pub model: String,
    pub endpoint: String,
    pub percentage: f64,
    pub native_input_tokens: u64,
    /// `native_input_tokens × 100 / percentage`，即上游隐含使用的分母。
    pub implied_window_tokens: u64,
}

/// 百分比达到该值即视为可能被上游钳位，不可用于反推。
///
/// 流式处理在 `>= 100` 时会置 `model_context_window_exceeded`，说明上游确实会给出
/// 100。若它实际是钳位后的结果，反推出的分母会等于本次输入量本身，从而系统性低估窗口
/// 并在后续把这个低估值当成观测。宁可丢弃该样本。
const CLAMP_SUSPECT_PERCENTAGE: f64 = 100.0;

/// 由同一次响应的原生用量与上下文百分比反推分母。
///
/// 任一前提不成立就返回 `None`——缺失不是零，没有样本也绝不记为「一致」。
pub fn derive_sample(
    model: &str,
    endpoint: &str,
    percentage: Option<f64>,
    native_input_tokens: Option<u64>,
    completed: bool,
) -> Option<CalibrationSample> {
    // 中断的响应可能只拿到半截事件；此时两个数字未必描述同一个完整请求。
    if !completed {
        return None;
    }
    let percentage = percentage?;
    let native_input_tokens = native_input_tokens?;
    if !percentage.is_finite() || percentage <= 0.0 || percentage >= CLAMP_SUSPECT_PERCENTAGE {
        return None;
    }
    // 上游报了非零占用却说输入为 0，两者自相矛盾，不取样。
    if native_input_tokens == 0 {
        return None;
    }
    let implied = (native_input_tokens as f64) * 100.0 / percentage;
    if !implied.is_finite() || implied < 1.0 || implied > u64::MAX as f64 {
        return None;
    }
    Some(CalibrationSample {
        model: model.to_string(),
        endpoint: endpoint.to_string(),
        percentage,
        native_input_tokens,
        implied_window_tokens: implied.round() as u64,
    })
}

/// 某个模型 / 端点上的观测汇总。
///
/// `samples` 必须与数值一同展示：三个样本和三百个样本不是一回事。`min`/`max` 的跨度
/// 本身就是信号——跨度小说明分母稳定，跨度大说明百分比并非简单比例，此时**不能**把
/// 均值当成窗口。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationAggregate {
    pub model: String,
    pub endpoint: String,
    pub samples: u64,
    pub min_window_tokens: u64,
    pub max_window_tokens: u64,
    pub mean_window_tokens: u64,
    pub last_seen: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        percentage: Option<f64>,
        native: Option<u64>,
        completed: bool,
    ) -> Option<CalibrationSample> {
        derive_sample("claude-sonnet-4", "ide", percentage, native, completed)
    }

    #[test]
    fn derives_the_denominator_from_a_complete_pair() {
        let s = sample(Some(25.0), Some(50_000), true).expect("完整配对应产出样本");
        assert_eq!(s.implied_window_tokens, 200_000);
        assert_eq!(s.native_input_tokens, 50_000);
        assert_eq!(s.percentage, 25.0);
    }

    /// 缺任何一半都不取样：缺失不是零。
    #[test]
    fn a_missing_half_produces_no_sample() {
        assert!(sample(None, Some(50_000), true).is_none());
        assert!(sample(Some(25.0), None, true).is_none());
        assert!(sample(None, None, true).is_none());
    }

    /// 零百分比什么都证明不了，且会除零。
    #[test]
    fn zero_or_negative_percentage_produces_no_sample() {
        assert!(sample(Some(0.0), Some(50_000), true).is_none());
        assert!(sample(Some(-1.0), Some(50_000), true).is_none());
        assert!(sample(Some(f64::NAN), Some(50_000), true).is_none());
    }

    /// 100% 可能是钳位值；采信它会系统性低估窗口。
    #[test]
    fn clamped_percentage_produces_no_sample() {
        assert!(sample(Some(100.0), Some(50_000), true).is_none());
        assert!(sample(Some(137.0), Some(50_000), true).is_none());
        assert!(sample(Some(99.9), Some(50_000), true).is_some());
    }

    /// 中断的响应两个数字未必描述同一个完整请求。
    #[test]
    fn interrupted_response_produces_no_sample() {
        assert!(sample(Some(25.0), Some(50_000), false).is_none());
    }

    /// 非零占用配零输入自相矛盾，不取样。
    #[test]
    fn contradictory_zero_input_produces_no_sample() {
        assert!(sample(Some(25.0), Some(0), true).is_none());
    }
}
