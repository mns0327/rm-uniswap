/// Signed token delta using Uniswap V4's **caller / PoolManager-perspective**
/// `BalanceDelta` convention, matching the convention used by the current
/// swap simulator.
///
/// # Sign convention
///
/// | Value | Caller / PoolManager-perspective meaning          |
/// |-------|----------------------------------------------------|
/// | `< 0` | Caller owes/sends this token to the PoolManager.   |
/// | `> 0` | Caller receives/is owed this token from the pool.  |
///
/// Example — `zero_for_one` exact-input swap:
/// - `amount0 < 0` — caller paid/owes token0.
/// - `amount1 > 0` — caller receives token1.
///
/// Use [`BalanceDelta::amount_in`] / [`BalanceDelta::amount_out`] for
/// direction-aware unsigned accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BalanceDelta {
    /// Token0 delta.
    ///
    /// Caller perspective:
    /// - negative = caller pays/owes token0
    /// - positive = caller receives token0
    pub amount0: i128,

    /// Token1 delta.
    ///
    /// Caller perspective:
    /// - negative = caller pays/owes token1
    /// - positive = caller receives token1
    pub amount1: i128,
}

impl BalanceDelta {
    /// Direction-aware unsigned output amount.
    ///
    /// Caller / PoolManager perspective:
    /// output token is positive because the caller receives it.
    #[inline]
    #[must_use]
    pub fn amount_out(&self, zero_for_one: bool) -> u128 {
        let raw = if zero_for_one {
            self.amount1 // caller receives token1
        } else {
            self.amount0 // caller receives token0
        };

        if raw > 0 { raw as u128 } else { 0 }
    }

    /// Direction-aware unsigned input amount.
    ///
    /// Caller / PoolManager perspective:
    /// input token is negative because the caller pays/owes it.
    #[inline]
    #[must_use]
    pub fn amount_in(&self, zero_for_one: bool) -> u128 {
        let raw = if zero_for_one {
            self.amount0 // caller pays/owes token0
        } else {
            self.amount1 // caller pays/owes token1
        };

        if raw < 0 { raw.unsigned_abs() } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::BalanceDelta;

    #[test]
    fn zero_for_one_exact_input_uses_caller_perspective() {
        let delta = BalanceDelta {
            amount0: -1_000,
            amount1: 990,
        };

        assert_eq!(delta.amount_in(true), 1_000);
        assert_eq!(delta.amount_out(true), 990);
    }

    #[test]
    fn one_for_zero_exact_input_uses_caller_perspective() {
        let delta = BalanceDelta {
            amount0: 990,
            amount1: -1_000,
        };

        assert_eq!(delta.amount_in(false), 1_000);
        assert_eq!(delta.amount_out(false), 990);
    }

    #[test]
    fn wrong_sign_returns_zero_instead_of_wrong_amount() {
        let delta = BalanceDelta {
            amount0: 1_000,
            amount1: -990,
        };

        assert_eq!(delta.amount_in(true), 0);
        assert_eq!(delta.amount_out(true), 0);
    }
}
