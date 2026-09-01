use alloy::primitives::{Address, I256};
use ruint::aliases::U256;

use crate::{
    core::types::{delta::BeforeSwapDelta, pool_key::PoolKey},
    v4::{BalanceDelta, Fee, ModifyLiquidityParams, SwapParams},
};

/// Convenience adapter that applies Uniswap V4 hook accounting around raw
/// [`Hooks`] callbacks.
///
/// The raw callback trait exposes the same lifecycle points a hook contract
/// would implement. This adapter performs the PoolManager-style bookkeeping
/// that sits around those callbacks: selecting add/remove liquidity hooks from
/// `liquidity_delta`, applying `BeforeSwapDelta` to the executable swap amount,
/// and splitting the final operation delta into caller and hook portions.
///
/// Returned `BalanceDelta` values always use the caller / PoolManager
/// convention documented on [`BalanceDelta`]. The first element of adapter
/// return tuples is the caller-settled delta after hook claims are removed; the
/// second element is the hook-settled delta.
pub trait HooksImpl: Hooks {
    /// Routes a liquidity modification to the add or remove callback.
    ///
    /// Positive liquidity is treated as an add. Zero and negative liquidity are
    /// routed through the remove path, matching the branch used by the
    /// accounting adapter below.
    #[inline]
    fn before_modify_liquidity(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: ModifyLiquidityParams,
        hook_data: &[u8],
    ) -> Result<(), crate::Error> {
        if params.liquidity_delta > 0 {
            self.before_add_liquidity(sender, pool_key, params, hook_data)
        } else {
            self.before_remove_liquidity(sender, pool_key, params, hook_data)
        }
    }

    /// Runs the post-liquidity callback and separates caller and hook deltas.
    ///
    /// The raw hook returns the `BalanceDelta` assigned to the hook. That hook
    /// delta is subtracted from the pool-produced `delta`; the remainder is the
    /// amount the original caller settles.
    #[inline]
    fn after_modify_liquidity(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: ModifyLiquidityParams,
        delta: BalanceDelta,
        fees_accrued: BalanceDelta,
        hook_data: &[u8],
    ) -> Result<(BalanceDelta, BalanceDelta), crate::Error> {
        let hook_delta = if params.liquidity_delta > 0 {
            self.after_add_liquidity(sender, pool_key, params, delta, fees_accrued, hook_data)
        } else {
            self.after_remove_liquidity(sender, pool_key, params, delta, fees_accrued, hook_data)
        }?;

        let caller_delta = delta.checked_sub(&hook_delta)?;

        Ok((caller_delta, hook_delta))
    }

    /// Runs the pre-swap callback and returns the executable swap amount.
    ///
    /// `BeforeSwapDelta::specified_delta` is applied to `amount_specified`
    /// before swap execution. The adjustment changes the caller's effective
    /// exact-input amount or exact-output target, but it must not flip the swap
    /// from exact input to exact output or vice versa.
    ///
    /// Dynamic fee support is intentionally not wired through this adapter yet,
    /// so the returned fee is currently `Fee::ZERO`.
    #[inline]
    fn before_swap(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: SwapParams,
        hook_data: &[u8],
    ) -> Result<(I256, BeforeSwapDelta, Fee), crate::Error> {
        let mut amount_to_swap = params.amount_specified;

        // TODO: add dynamic fee feature
        let (before_swap_delta, _fee) =
            Hooks::before_swap(&mut *self, sender, pool_key, params, hook_data)?;

        let hook_delta_specified = before_swap_delta.specified_delta;

        if hook_delta_specified != 0 {
            let exact_input = amount_to_swap.is_negative();
            amount_to_swap = amount_to_swap
                .checked_add(I256::try_from(hook_delta_specified).expect("should not overflow"))
                .ok_or(crate::Error::AmountOverflow)?;

            if exact_input {
                if amount_to_swap.is_positive() {
                    return Err(crate::Error::AmountOverflow);
                }
            } else {
                if amount_to_swap.is_negative() {
                    return Err(crate::Error::AmountOverflow);
                }
            }
        }

        Ok((amount_to_swap, before_swap_delta, Fee::ZERO))
    }

    /// Runs the post-swap callback and separates caller and hook deltas.
    ///
    /// The final hook delta is built from the pre-swap specified delta and the
    /// accumulated unspecified delta returned by both swap callbacks. The
    /// specified/unspecified values are then mapped onto token0/token1 according
    /// to the swap direction and exact-input/exact-output mode.
    fn after_swap(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: SwapParams,
        swap_delta: BalanceDelta,
        hook_data: &[u8],
        before_swap_hook_return: BeforeSwapDelta,
    ) -> Result<(BalanceDelta, BalanceDelta), crate::Error> {
        let BeforeSwapDelta {
            specified_delta: hook_delta_specified,
            unspecified_delta: mut hook_delta_unspecified,
        } = before_swap_hook_return;

        hook_delta_unspecified = hook_delta_unspecified
            .checked_add(Hooks::after_swap(
                &mut *self, sender, pool_key, params, swap_delta, hook_data,
            )?)
            .ok_or(crate::Error::I128Overflow)?;

        let hook_delta = if params.amount_specified.is_negative() == params.zero_for_one {
            BalanceDelta {
                amount0: hook_delta_specified,
                amount1: hook_delta_unspecified,
            }
        } else {
            BalanceDelta {
                amount0: hook_delta_unspecified,
                amount1: hook_delta_specified,
            }
        };

        let caller_delta = swap_delta.checked_sub(&hook_delta)?;

        Ok((caller_delta, hook_delta))
    }
}

/// Uniswap V4-style hook callbacks for concentrated-liquidity pools.
///
/// The real V4 PoolManager decides which callbacks to invoke from permission
/// bits encoded in the hook contract address. This trait models only the
/// callback surface: callers decide whether a hook is installed and when each
/// method should run.
///
/// `sender` is the original actor that started the pool operation, `pool_key`
/// identifies the pool being acted on, and `hook_data` is opaque
/// caller-provided data forwarded to the hook implementation.
///
/// Implementations should return only the delta or adjustment owned by the hook
/// itself. [`HooksImpl`] is responsible for subtracting hook deltas from the
/// pool-produced operation delta to derive the final caller delta.
pub trait Hooks: Send + Sync {
    /// Called before liquidity is added to a position.
    ///
    /// Implementations may validate the request or use `hook_data` to enforce
    /// application-specific policy before any pool or position state changes are
    /// committed.
    fn before_add_liquidity(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: ModifyLiquidityParams,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;

    /// Called after liquidity has been added.
    ///
    /// `delta` is the caller-facing balance delta for the add-liquidity action.
    /// `fees_accrued` contains fees realized since the position was last
    /// updated or collected.
    ///
    /// Return the hook's own balance delta. The adapter subtracts this from
    /// `delta` so the caller settles only the remaining amount.
    fn after_add_liquidity(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: ModifyLiquidityParams,
        delta: BalanceDelta,
        fees_accrued: BalanceDelta,
        hook_data: &[u8],
    ) -> Result<BalanceDelta, crate::Error>;

    /// Called before liquidity is removed from a position.
    ///
    /// Implementations may reject the request before pool or position state is
    /// mutated.
    fn before_remove_liquidity(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: ModifyLiquidityParams,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;

    /// Called after liquidity has been removed.
    ///
    /// `delta` is the caller-facing balance delta for the remove-liquidity
    /// action. `fees_accrued` contains fees realized since the position was last
    /// updated or collected.
    ///
    /// Return the hook's own balance delta. The adapter subtracts this from
    /// `delta` so the caller settles only the remaining amount.
    fn after_remove_liquidity(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: ModifyLiquidityParams,
        delta: BalanceDelta,
        fees_accrued: BalanceDelta,
        hook_data: &[u8],
    ) -> Result<BalanceDelta, crate::Error>;

    /// Called before a swap is executed.
    ///
    /// `params` uses V4 `amountSpecified` semantics: negative for exact input
    /// and positive for exact output.
    ///
    /// Return a [`BeforeSwapDelta`] to adjust the specified and/or unspecified
    /// side of the swap. The returned fee is reserved for dynamic-fee support by
    /// callers that opt into fee overrides.
    fn before_swap(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: SwapParams,
        hook_data: &[u8],
    ) -> Result<(BeforeSwapDelta, Fee), crate::Error>;

    /// Called after a swap has executed.
    ///
    /// `delta` is the caller / PoolManager balance delta produced by the swap:
    /// positive values are owed to the caller, and negative values are owed to
    /// the pool.
    ///
    /// Return the hook's additional unspecified-token delta. It is combined
    /// with `BeforeSwapDelta::unspecified_delta` before the adapter derives the
    /// final caller and hook balance deltas.
    fn after_swap(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        params: SwapParams,
        delta: BalanceDelta,
        hook_data: &[u8],
    ) -> Result<i128, crate::Error>;

    /// Called before a donation is applied to the pool.
    ///
    /// `amount0` and `amount1` are the token amounts being donated to current
    /// in-range liquidity.
    fn before_donate(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        amount0: U256,
        amount1: U256,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;

    /// Called after a donation has been applied to the pool.
    ///
    /// `amount0` and `amount1` are the token amounts donated to current
    /// in-range liquidity.
    fn after_donate(
        &mut self,
        sender: &Address,
        pool_key: &PoolKey,
        amount0: U256,
        amount1: U256,
        hook_data: &[u8],
    ) -> Result<(), crate::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Error,
        core::types::{sqrt_price::SqrtPriceX96, tick::TickIndex},
    };

    struct RecordingHook {
        events: Vec<&'static str>,
        after_add_delta: BalanceDelta,
        after_remove_delta: BalanceDelta,
        before_swap_delta: BeforeSwapDelta,
        before_swap_fee: Fee,
        after_swap_delta: i128,
    }

    impl Default for RecordingHook {
        fn default() -> Self {
            Self {
                events: Vec::new(),
                after_add_delta: BalanceDelta::DEFAULT,
                after_remove_delta: BalanceDelta::DEFAULT,
                before_swap_delta: BeforeSwapDelta::ZERO,
                before_swap_fee: Fee::ZERO,
                after_swap_delta: 0,
            }
        }
    }

    impl HooksImpl for RecordingHook {}

    impl Hooks for RecordingHook {
        fn before_add_liquidity(
            &mut self,
            _sender: &Address,
            _pool_key: &PoolKey,
            _params: ModifyLiquidityParams,
            _hook_data: &[u8],
        ) -> Result<(), Error> {
            self.events.push("before_add_liquidity");
            Ok(())
        }

        fn after_add_liquidity(
            &mut self,
            _sender: &Address,
            _pool_key: &PoolKey,
            _params: ModifyLiquidityParams,
            _delta: BalanceDelta,
            _fees_accrued: BalanceDelta,
            _hook_data: &[u8],
        ) -> Result<BalanceDelta, Error> {
            self.events.push("after_add_liquidity");
            Ok(self.after_add_delta)
        }

        fn before_remove_liquidity(
            &mut self,
            _sender: &Address,
            _pool_key: &PoolKey,
            _params: ModifyLiquidityParams,
            _hook_data: &[u8],
        ) -> Result<(), Error> {
            self.events.push("before_remove_liquidity");
            Ok(())
        }

        fn after_remove_liquidity(
            &mut self,
            _sender: &Address,
            _pool_key: &PoolKey,
            _params: ModifyLiquidityParams,
            _delta: BalanceDelta,
            _fees_accrued: BalanceDelta,
            _hook_data: &[u8],
        ) -> Result<BalanceDelta, Error> {
            self.events.push("after_remove_liquidity");
            Ok(self.after_remove_delta)
        }

        fn before_swap(
            &mut self,
            _sender: &Address,
            _pool_key: &PoolKey,
            _params: SwapParams,
            _hook_data: &[u8],
        ) -> Result<(BeforeSwapDelta, Fee), Error> {
            self.events.push("before_swap");
            Ok((
                BeforeSwapDelta {
                    specified_delta: self.before_swap_delta.specified_delta,
                    unspecified_delta: self.before_swap_delta.unspecified_delta,
                },
                self.before_swap_fee,
            ))
        }

        fn after_swap(
            &mut self,
            _sender: &Address,
            _pool_key: &PoolKey,
            _params: SwapParams,
            _delta: BalanceDelta,
            _hook_data: &[u8],
        ) -> Result<i128, Error> {
            self.events.push("after_swap");
            Ok(self.after_swap_delta)
        }

        fn before_donate(
            &mut self,
            _sender: &Address,
            _pool_key: &PoolKey,
            _amount0: U256,
            _amount1: U256,
            _hook_data: &[u8],
        ) -> Result<(), Error> {
            self.events.push("before_donate");
            Ok(())
        }

        fn after_donate(
            &mut self,
            _sender: &Address,
            _pool_key: &PoolKey,
            _amount0: U256,
            _amount1: U256,
            _hook_data: &[u8],
        ) -> Result<(), Error> {
            self.events.push("after_donate");
            Ok(())
        }
    }

    fn sender() -> Address {
        Address::repeat_byte(0x11)
    }

    fn pool_key() -> PoolKey {
        PoolKey::new(
            Address::repeat_byte(0x01),
            Address::repeat_byte(0x02),
            3_000,
            60,
            Address::repeat_byte(0x03),
        )
    }

    fn modify_params(liquidity_delta: i128) -> ModifyLiquidityParams {
        ModifyLiquidityParams {
            tick_lower: TickIndex::new(-60).unwrap(),
            tick_upper: TickIndex::new(60).unwrap(),
            liquidity_delta,
            info: None,
        }
    }

    fn swap_params(zero_for_one: bool, amount_specified: I256) -> SwapParams {
        SwapParams {
            zero_for_one,
            amount_specified,
            sqrt_price_limit_x96: SqrtPriceX96::extreme_price_limit(zero_for_one),
        }
    }

    fn i256(value: u64) -> I256 {
        I256::from(U256::from(value))
    }

    fn assert_error<T>(result: Result<T, Error>, expected: Error) {
        match result {
            Ok(_) => panic!("expected {expected:?}, got Ok(_)"),
            Err(error) => assert_eq!(error, expected),
        }
    }

    #[test]
    fn before_modify_liquidity_routes_by_liquidity_delta() {
        let sender = sender();
        let pool_key = pool_key();
        let mut hook = RecordingHook::default();

        hook.before_modify_liquidity(&sender, &pool_key, modify_params(100), b"add")
            .unwrap();
        hook.before_modify_liquidity(&sender, &pool_key, modify_params(0), b"collect")
            .unwrap();
        hook.before_modify_liquidity(&sender, &pool_key, modify_params(-100), b"remove")
            .unwrap();

        assert_eq!(
            hook.events,
            [
                "before_add_liquidity",
                "before_remove_liquidity",
                "before_remove_liquidity"
            ]
        );
    }

    #[test]
    fn after_modify_liquidity_returns_caller_and_hook_deltas() {
        let sender = sender();
        let pool_key = pool_key();
        let pool_delta = BalanceDelta {
            amount0: 100,
            amount1: -80,
        };
        let fees_accrued = BalanceDelta {
            amount0: 7,
            amount1: 11,
        };
        let mut hook = RecordingHook {
            after_add_delta: BalanceDelta {
                amount0: 12,
                amount1: -5,
            },
            after_remove_delta: BalanceDelta {
                amount0: -3,
                amount1: 9,
            },
            ..RecordingHook::default()
        };

        let (caller_delta, hook_delta) = hook
            .after_modify_liquidity(
                &sender,
                &pool_key,
                modify_params(100),
                pool_delta,
                fees_accrued,
                b"add",
            )
            .unwrap();
        assert_eq!(hook_delta, hook.after_add_delta);
        assert_eq!(
            caller_delta,
            BalanceDelta {
                amount0: 88,
                amount1: -75,
            }
        );

        let (caller_delta, hook_delta) = hook
            .after_modify_liquidity(
                &sender,
                &pool_key,
                modify_params(0),
                pool_delta,
                fees_accrued,
                b"remove",
            )
            .unwrap();
        assert_eq!(hook_delta, hook.after_remove_delta);
        assert_eq!(
            caller_delta,
            BalanceDelta {
                amount0: 103,
                amount1: -89,
            }
        );
        assert_eq!(
            hook.events,
            ["after_add_liquidity", "after_remove_liquidity"]
        );
    }

    #[test]
    fn before_swap_applies_specified_delta_without_mode_flip() {
        let sender = sender();
        let pool_key = pool_key();
        let mut exact_input_hook = RecordingHook {
            before_swap_delta: BeforeSwapDelta {
                specified_delta: 40,
                unspecified_delta: -7,
            },
            before_swap_fee: Fee::new(500).unwrap(),
            ..RecordingHook::default()
        };
        let mut exact_output_hook = RecordingHook {
            before_swap_delta: BeforeSwapDelta {
                specified_delta: -25,
                unspecified_delta: 8,
            },
            before_swap_fee: Fee::new(100).unwrap(),
            ..RecordingHook::default()
        };

        let (amount_to_swap, before_swap_delta, fee) = HooksImpl::before_swap(
            &mut exact_input_hook,
            &sender,
            &pool_key,
            swap_params(true, -i256(100)),
            b"input",
        )
        .unwrap();
        assert_eq!(amount_to_swap, -i256(60));
        assert_eq!(before_swap_delta.specified_delta, 40);
        assert_eq!(before_swap_delta.unspecified_delta, -7);
        assert_eq!(fee, Fee::ZERO);

        let (amount_to_swap, before_swap_delta, fee) = HooksImpl::before_swap(
            &mut exact_output_hook,
            &sender,
            &pool_key,
            swap_params(false, i256(100)),
            b"output",
        )
        .unwrap();
        assert_eq!(amount_to_swap, i256(75));
        assert_eq!(before_swap_delta.specified_delta, -25);
        assert_eq!(before_swap_delta.unspecified_delta, 8);
        assert_eq!(fee, Fee::ZERO);
    }

    #[test]
    fn before_swap_rejects_specified_delta_that_flips_exactness() {
        let sender = sender();
        let pool_key = pool_key();
        let mut flips_exact_input = RecordingHook {
            before_swap_delta: BeforeSwapDelta {
                specified_delta: 101,
                unspecified_delta: 0,
            },
            ..RecordingHook::default()
        };
        let mut flips_exact_output = RecordingHook {
            before_swap_delta: BeforeSwapDelta {
                specified_delta: -101,
                unspecified_delta: 0,
            },
            ..RecordingHook::default()
        };

        assert_error(
            HooksImpl::before_swap(
                &mut flips_exact_input,
                &sender,
                &pool_key,
                swap_params(true, -i256(100)),
                b"input",
            ),
            Error::AmountOverflow,
        );
        assert_error(
            HooksImpl::before_swap(
                &mut flips_exact_output,
                &sender,
                &pool_key,
                swap_params(false, i256(100)),
                b"output",
            ),
            Error::AmountOverflow,
        );
    }

    #[test]
    fn after_swap_combines_before_and_after_unspecified_delta() {
        let sender = sender();
        let pool_key = pool_key();

        let cases = [
            (
                true,
                -i256(100),
                BalanceDelta {
                    amount0: -1_000,
                    amount1: 900,
                },
                BalanceDelta {
                    amount0: 12,
                    amount1: 10,
                },
                BalanceDelta {
                    amount0: -1_012,
                    amount1: 890,
                },
            ),
            (
                false,
                -i256(100),
                BalanceDelta {
                    amount0: 900,
                    amount1: -1_000,
                },
                BalanceDelta {
                    amount0: 10,
                    amount1: 12,
                },
                BalanceDelta {
                    amount0: 890,
                    amount1: -1_012,
                },
            ),
            (
                true,
                i256(100),
                BalanceDelta {
                    amount0: -1_000,
                    amount1: 900,
                },
                BalanceDelta {
                    amount0: 10,
                    amount1: 12,
                },
                BalanceDelta {
                    amount0: -1_010,
                    amount1: 888,
                },
            ),
            (
                false,
                i256(100),
                BalanceDelta {
                    amount0: 900,
                    amount1: -1_000,
                },
                BalanceDelta {
                    amount0: 12,
                    amount1: 10,
                },
                BalanceDelta {
                    amount0: 888,
                    amount1: -1_010,
                },
            ),
        ];

        for (zero_for_one, amount_specified, swap_delta, expected_hook, expected_caller) in cases {
            let mut hook = RecordingHook {
                after_swap_delta: 3,
                ..RecordingHook::default()
            };
            let before_swap_delta = BeforeSwapDelta {
                specified_delta: 12,
                unspecified_delta: 7,
            };

            let (caller_delta, hook_delta) = HooksImpl::after_swap(
                &mut hook,
                &sender,
                &pool_key,
                swap_params(zero_for_one, amount_specified),
                swap_delta,
                b"swap",
                before_swap_delta,
            )
            .unwrap();

            assert_eq!(hook_delta, expected_hook);
            assert_eq!(caller_delta, expected_caller);
            assert_eq!(hook.events, ["after_swap"]);
        }
    }

    #[test]
    fn after_swap_rejects_unspecified_delta_overflow() {
        let sender = sender();
        let pool_key = pool_key();
        let mut hook = RecordingHook {
            after_swap_delta: 1,
            ..RecordingHook::default()
        };

        assert_error(
            HooksImpl::after_swap(
                &mut hook,
                &sender,
                &pool_key,
                swap_params(true, -i256(100)),
                BalanceDelta::DEFAULT,
                b"swap",
                BeforeSwapDelta {
                    specified_delta: 0,
                    unspecified_delta: i128::MAX,
                },
            ),
            Error::I128Overflow,
        );
    }
}
