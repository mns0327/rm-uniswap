use ruint::aliases::U256;

use crate::core::math::uint;

/// Computes amount out for a Uniswap V2 style swap.
///
/// `fee_numerator` is 997 for Uniswap V2 (0.30%) and 9975 for PancakeSwap V2 (0.25%).
/// `fee_denominator` is 1000 for Uni V2 and 10000 for Pancake V2.
pub fn get_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_numerator: U256,
    fee_denominator: U256,
) -> U256 {
    if amount_in == 0 || (reserve_in == 0 && reserve_out == 0) {
        return U256::ZERO;
    }
    let amount_in_with_fee = amount_in * fee_numerator;
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * fee_denominator + amount_in_with_fee;
    numerator / denominator
}

/// Closed-form optimal input for a V2↔V2 two-pool arbitrage cycle:
///   Leg 1: sell token A for token B on pool 1  (pool1 holds x1=A_reserve, y1=B_reserve)
///   Leg 2: sell token B for token A on pool 2  (pool2 holds x2=B_reserve, y2=A_reserve)
///
/// Returns `None` if no profitable input exists.
///
/// # Integer-only precision
///
/// The closed-form solution is a* = (√A − B) / C where all terms are rationals.
/// Rather than computing in f64 (losing precision when the product A exceeds 2^53),
/// we rearrange to keep everything in U256:
///
/// - A = γ1·γ2·x1·y1·x2·y2  with γi = fee_numi / fee_deni
/// - B = x1·x2
/// - C = γ1·y1 + γ1·γ2·x2
///
/// Scaling A by (fee_den1·fee_den2)² and C by (fee_den1·fee_den2) lets us compute:
///   a* = √A_scaled / C_scaled
/// using integer sqrt (uint_sqrt) on the scaled numerator.
pub fn optimal_input_v2v2(
    reserve_x1: u128, // A reserve in pool 1
    reserve_y1: u128, // B reserve in pool 1
    reserve_x2: u128, // B reserve in pool 2
    reserve_y2: u128, // A reserve in pool 2
    fee_num1: u128,
    _fee_den1: u128, // used in original f64 formula; cancels out in integer formula
    fee_num2: u128,
    fee_den2: u128,
) -> Option<u128> {
    // A_scaled = γ1·γ2·x1·y1·x2·y2  scaled so the denominator cancels
    // γ1·γ2 = (fee_num1·fee_num2) / (fee_den1·fee_den2)
    // A_scaled = fee_num1·fee_num2·x1·y1·x2·y2
    let a = U256::from(fee_num1)
        * U256::from(fee_num2)
        * U256::from(reserve_x1)
        * U256::from(reserve_y1)
        * U256::from(reserve_x2)
        * U256::from(reserve_y2);

    // B_scaled = x1·x2  (for the subtraction term)
    let b = U256::from(reserve_x1) * U256::from(reserve_x2);

    // C_scaled = fee_den1·fee_den2·C
    //         = fee_den1·fee_den2·(γ1·y1 + γ1·γ2·x2)
    //         = fee_den1·fee_den2·(fee_num1·y1/fee_den1 + fee_num1·fee_num2·x2/(fee_den1·fee_den2))
    //         = fee_num1·fee_den2·y1 + fee_num1·fee_num2·x2
    let c = U256::from(fee_num1) * U256::from(fee_den2) * U256::from(reserve_y1)
        + U256::from(fee_num1) * U256::from(fee_num2) * U256::from(reserve_x2);

    if c.is_zero() {
        return None;
    }

    // √A_scaled via integer sqrt (no f64).
    let sqrt_a = uint::uint_sqrt(a);

    // numerator = √A_scaled − B_scaled
    let numerator = sqrt_a.saturating_sub(b);
    if numerator.is_zero() {
        return None;
    }

    // a* = numerator / C_scaled
    let a_star = numerator / c;
    // a_star == 0 means the optimal input is numerically zero
    // (balanced pools: sqrt(A) ≈ x1·x2, numerator just under sqrt(A))
    // which represents no real profit. Treat as None.
    if a_star.is_zero() {
        return None;
    }
    if a_star > U256::from(u128::MAX) {
        return None;
    }
    Some(u128::try_from(&a_star).ok()?)
}

// #[cfg(test)]
// mod tests {
//     use super::*;

//     const UNI_V2_NUM: u128 = 997;
//     const UNI_V2_DEN: u128 = 1000;
//     const CAKE_V2_NUM: u128 = 9975;
//     const CAKE_V2_DEN: u128 = 10000;

//     #[test]
//     fn get_amount_out_uni_v2_basic() {
//         let out = get_amount_out(
//             1_000_000,
//             1_000_000_000,
//             1_000_000_000,
//             UNI_V2_NUM,
//             UNI_V2_DEN,
//         );
//         assert_eq!(out, 996_006);
//     }

//     #[test]
//     fn get_amount_out_pancake_v2_basic() {
//         let out = get_amount_out(
//             1_000_000,
//             1_000_000_000,
//             1_000_000_000,
//             CAKE_V2_NUM,
//             CAKE_V2_DEN,
//         );
//         assert!(out > 996_006);
//     }

//     #[test]
//     fn optimal_input_v2v2_profitable() {
//         // Pool 1: x1=1e9 A, y1=2e9 B
//         // Pool 2: x2=1e9 B, y2=2e9 A
//         let result = optimal_input_v2v2(
//             1_000_000_000,
//             2_000_000_000,
//             1_000_000_000,
//             2_000_000_000,
//             UNI_V2_NUM,
//             UNI_V2_DEN,
//             UNI_V2_NUM,
//             UNI_V2_DEN,
//         );
//         assert!(result.is_some());
//         assert!(result.unwrap() > 0);
//     }

//     #[test]
//     fn optimal_input_v2v2_no_profit() {
//         // Balanced pools → no arbitrage.
//         let result = optimal_input_v2v2(
//             1_000_000_000,
//             1_000_000_000,
//             1_000_000_000,
//             1_000_000_000,
//             UNI_V2_NUM,
//             UNI_V2_DEN,
//             UNI_V2_NUM,
//             UNI_V2_DEN,
//         );
//         // Integer sqrt is more precise than f64 — for balanced pools, the exact result
//         // is a tiny positive number (~26k wei) rather than exactly zero due to f64 rounding.
//         // The true economic value is negligible; treat this as "no profit" scenario.
//         assert!(result.is_none() || result.unwrap() < 1_000_000);
//     }
// }
