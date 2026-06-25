use ruint::aliases::U256;

use crate::core::math::{constant_product, uint};

/// Uniswap V2 pair reserves and fee configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pool {
    pub reserve_in: U256,
    pub reserve_out: U256,
    pub fee_numerator: U256,
    pub fee_denominator: U256,
}

impl Pool {
    /// Returns the output amount for a swap against this pair.
    pub fn get_amount_out(&self, amount_in: U256) -> U256 {
        Library::get_amount_out(
            amount_in,
            self.reserve_in,
            self.reserve_out,
            self.fee_numerator,
            self.fee_denominator,
        )
    }
}

/// Rust facade for `UniswapV2Library`.
pub struct Library;

impl Library {
    pub fn sqrt(value: U256) -> U256 {
        uint::uint_sqrt(value)
    }

    pub fn get_amount_out(
        amount_in: U256,
        reserve_in: U256,
        reserve_out: U256,
        fee_numerator: U256,
        fee_denominator: U256,
    ) -> U256 {
        constant_product::get_amount_out(
            amount_in,
            reserve_in,
            reserve_out,
            fee_numerator,
            fee_denominator,
        )
    }

    pub fn optimal_input_v2v2(
        reserve_x1: u128,
        reserve_y1: u128,
        reserve_x2: u128,
        reserve_y2: u128,
        fee_num1: u128,
        fee_den1: u128,
        fee_num2: u128,
        fee_den2: u128,
    ) -> Option<u128> {
        constant_product::optimal_input_v2v2(
            reserve_x1, reserve_y1, reserve_x2, reserve_y2, fee_num1, fee_den1, fee_num2, fee_den2,
        )
    }
}
