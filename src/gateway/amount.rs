use std::{fmt, str::FromStr};

use anyhow::ensure;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Amount(i128);

impl Amount {
    const SCALE: i128 = 1_000_000_000_000_000_000;
    const TOKENS_PER_MILLION: i128 = 1_000_000;
    const PRICE_QUANTUM: i128 = Self::SCALE / Self::TOKENS_PER_MILLION;

    pub const ZERO: Self = Self(0);

    pub fn checked_add(self, rhs: Self) -> anyhow::Result<Self> {
        let atoms = self
            .0
            .checked_add(rhs.0)
            .ok_or_else(|| anyhow::anyhow!("amount overflow"))?;
        Ok(Self(atoms))
    }

    pub fn checked_sub(self, rhs: Self) -> anyhow::Result<Self> {
        let atoms = self
            .0
            .checked_sub(rhs.0)
            .ok_or_else(|| anyhow::anyhow!("amount underflow"))?;
        ensure!(atoms >= 0, "amount underflow");
        Ok(Self(atoms))
    }

    pub fn checked_mul_tokens(self, tokens: u64) -> anyhow::Result<Self> {
        let tokens = i128::from(tokens);
        let product = self
            .0
            .checked_mul(tokens)
            .ok_or_else(|| anyhow::anyhow!("token cost overflow"))?;
        ensure!(
            product % Self::TOKENS_PER_MILLION == 0,
            "token cost is not exact at amount scale"
        );
        Ok(Self(product / Self::TOKENS_PER_MILLION))
    }
}

impl FromStr for Amount {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        ensure!(!value.is_empty(), "amount is empty");
        ensure!(
            !value.starts_with(['-', '+']),
            "amount must be nonnegative without a sign"
        );

        let mut parts = value.split('.');
        let whole = parts.next().unwrap_or_default();
        let fraction = parts.next();
        ensure!(
            parts.next().is_none(),
            "amount has more than one decimal point"
        );
        ensure!(
            !whole.is_empty() && whole.bytes().all(|byte| byte.is_ascii_digit()),
            "amount must be a plain decimal string"
        );

        let whole = whole.parse::<i128>()?;
        let whole_atoms = whole
            .checked_mul(Self::SCALE)
            .ok_or_else(|| anyhow::anyhow!("amount overflow"))?;
        let fraction_atoms = match fraction {
            None => 0,
            Some(fraction) => {
                ensure!(!fraction.is_empty(), "amount fraction is empty");
                ensure!(
                    fraction.len() <= 18,
                    "amount has more than 18 fractional digits"
                );
                ensure!(
                    fraction.bytes().all(|byte| byte.is_ascii_digit()),
                    "amount must be a plain decimal string"
                );
                let parsed = fraction.parse::<i128>()?;
                let padding = u32::try_from(18_usize - fraction.len())?;
                parsed
                    .checked_mul(10_i128.pow(padding))
                    .ok_or_else(|| anyhow::anyhow!("amount overflow"))?
            }
        };
        let atoms = whole_atoms
            .checked_add(fraction_atoms)
            .ok_or_else(|| anyhow::anyhow!("amount overflow"))?;
        Ok(Self(atoms))
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / Self::SCALE;
        let fraction = self.0 % Self::SCALE;
        if fraction == 0 {
            return write!(f, "{whole}");
        }
        let fraction = format!("{fraction:018}");
        write!(f, "{whole}.{}", fraction.trim_end_matches('0'))
    }
}

impl fmt::Debug for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Serialize for Amount {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

pub(super) fn validate_price(amount: Amount) -> anyhow::Result<()> {
    ensure!(
        amount.0 % Amount::PRICE_QUANTUM == 0,
        "price must have at most six fractional digits"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_real_native_credit_precision() {
        let amount: Amount = "0.0169543708291874".parse().unwrap();
        assert_eq!(amount.to_string(), "0.0169543708291874");
        assert_eq!(
            serde_json::to_string(&amount).unwrap(),
            r#""0.0169543708291874""#
        );
    }

    #[test]
    fn rejects_non_decimal_and_excess_precision() {
        for invalid in [
            "-1",
            "+1",
            "NaN",
            "inf",
            "1e3",
            "1.",
            ".1",
            "0.1234567890123456789",
        ] {
            assert!(invalid.parse::<Amount>().is_err(), "accepted {invalid}");
        }
        assert!(serde_json::from_str::<Amount>("1").is_err());
    }

    #[test]
    fn checked_arithmetic_rejects_underflow_and_keeps_exact_products() {
        assert_eq!(
            "1.2"
                .parse::<Amount>()
                .unwrap()
                .checked_add("3.45".parse().unwrap())
                .unwrap()
                .to_string(),
            "4.65"
        );
        let unit_price: Amount = "1.234567".parse().unwrap();
        assert_eq!(
            unit_price.checked_mul_tokens(3).unwrap().to_string(),
            "0.000003703701"
        );
        assert!((Amount::ZERO.checked_sub("0.1".parse().unwrap())).is_err());
    }
}
