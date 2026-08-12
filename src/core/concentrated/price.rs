use ruint::aliases::U256;

use crate::core::types::{atomic_f64::AtomicF64, sqrt_price::SqrtPriceX96};

/// Fee denominator used by Uniswap-style fee pips.
///
/// A fee of `500` means `500 / 1_000_000`, or `0.05%`.
const FEE_DENOMINATOR: f64 = 1_000_000.0;

const Q96_F64: f64 = 7.922_816_251_426_434e28;

/// Lock-free cached pool price for both swap directions.
///
/// `PriceCache` stores the fee-adjusted price for:
///
/// - `zero_for_one`: token0 -> token1
/// - `one_for_zero`: token1 -> token0
///
/// The values are stored as [`AtomicF64`] so readers can access prices without
/// taking a lock. This is intended for hot paths where prices are read very
/// frequently and updated only occasionally.
///
/// The cache stores **fee-adjusted prices only** because those are usually the
/// values needed for arbitrage scoring and path evaluation.
#[derive(Debug)]
pub struct PriceCache {
    /// Fee-adjusted price for swapping token0 -> token1.
    ///
    /// Formula:
    ///
    /// ```text
    /// zero_for_one_price_with_fee = zero_for_one_price * fee_multiplier
    /// ```
    zero_for_one_price_with_fee: AtomicF64,

    /// Fee-adjusted price for swapping token1 -> token0.
    ///
    /// Formula:
    ///
    /// ```text
    /// one_for_zero_price_with_fee = (1.0 / zero_for_one_price) * fee_multiplier
    /// ```
    one_for_zero_price_with_fee: AtomicF64,

    /// Fee multiplier applied to raw prices.
    ///
    /// Example:
    ///
    /// ```text
    /// fee_pips = 500
    /// fee = (1_000_000 - 500) / 1_000_000
    /// fee = 0.9995
    /// ```
    fee: f64,
}

impl PriceCache {
    /// Creates a new [`PriceCache`] from the token0 -> token1 raw price and pool fee.
    ///
    /// `zero_for_one_price` is the raw spot price for token0 -> token1 before fees.
    ///
    /// `fee` is expressed in pips, using a denominator of `1_000_000`.
    /// For example:
    ///
    /// - `500` = `0.05%`
    /// - `3_000` = `0.30%`
    /// - `10_000` = `1.00%`
    ///
    /// Both swap directions are precomputed and stored as fee-adjusted prices.
    pub fn new(zero_for_one_price: f64, fee: u32) -> Self {
        let fee = (FEE_DENOMINATOR - fee as f64) / FEE_DENOMINATOR;
        let one_for_zero_price = 1.0 / zero_for_one_price;

        let zero_for_one_price_with_fee = zero_for_one_price * fee;
        let one_for_zero_price_with_fee = one_for_zero_price * fee;

        Self {
            zero_for_one_price_with_fee: AtomicF64::new(zero_for_one_price_with_fee),
            one_for_zero_price_with_fee: AtomicF64::new(one_for_zero_price_with_fee),
            fee,
        }
    }

    /// Returns the fee-adjusted price for the requested swap direction.
    ///
    /// When `zero_for_one` is:
    ///
    /// - `true`: returns the token0 -> token1 price with fees applied.
    /// - `false`: returns the token1 -> token0 price with fees applied.
    ///
    /// This method performs a single atomic load and does not acquire a lock.
    #[inline]
    pub fn get_price_with_fee(&self, zero_for_one: bool) -> f64 {
        if zero_for_one {
            self.zero_for_one_price_with_fee.load()
        } else {
            self.one_for_zero_price_with_fee.load()
        }
    }

    /// Updates the cached prices using a new raw token0 -> token1 price.
    ///
    /// The opposite direction is recalculated as:
    ///
    /// ```text
    /// one_for_zero_price = 1.0 / zero_for_one_price
    /// ```
    ///
    /// Then both directions are multiplied by the cached fee multiplier.
    ///
    /// This method performs two atomic stores. Readers may briefly observe one
    /// updated direction and one previous direction if they read during an update.
    /// For single-direction reads this is normally acceptable. If both directions
    /// must be read as one consistent snapshot, use a snapshot-based structure such
    /// as `ArcSwap` or add a sequence-lock around the update.
    pub fn update_price(&self, zero_for_one_price: f64) {
        let one_for_zero_price = 1.0 / zero_for_one_price;
        let zero_for_one_price_with_fee = zero_for_one_price * self.fee;
        let one_for_zero_price_with_fee = one_for_zero_price * self.fee;

        self.zero_for_one_price_with_fee
            .store(zero_for_one_price_with_fee);
        self.one_for_zero_price_with_fee
            .store(one_for_zero_price_with_fee);
    }
}

fn u256_to_f64(v: U256) -> f64 {
    let limbs = v.into_limbs();
    if limbs[2] == 0 && limbs[3] == 0 {
        let lo = (limbs[0] as u128) | ((limbs[1] as u128) << 64);
        return lo as f64;
    }
    (limbs[0] as f64)
        + (limbs[1] as f64) * 1.844_674_407_370_955_2e19
        + (limbs[2] as f64) * 3.402_823_669_209_385e38
        + (limbs[3] as f64) * 6.277_101_735_386_68e57
}

#[inline]
pub fn sqrt_price_x96_to_price(sqrt_price_x96: SqrtPriceX96) -> f64 {
    let sqrt_price = u256_to_f64(sqrt_price_x96.as_u256()) / Q96_F64;

    sqrt_price * sqrt_price
}
