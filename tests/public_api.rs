use rm_uniswap::{
    v2,
    v3::{FullMath, QuoteState, Quoter, SignedAmount, SqrtPriceMath, SwapMath, TickMath},
    v4,
};
use ruint::aliases::U256;

#[test]
fn version_facades_are_the_public_entry_points() {
    let q96 = U256::ONE << 96;

    assert_eq!(
        v2::Library::get_amount_out(
            U256::from(1_000u64),
            U256::from(10_000u64),
            U256::from(10_000u64),
            U256::from(997u64),
            U256::from(1_000u64),
        ),
        U256::from(906u64)
    );

    assert_eq!(
        FullMath::mul_div(U256::from(6u64), U256::from(7u64), U256::from(3u64)).unwrap(),
        U256::from(14u64)
    );
    assert_eq!(TickMath::get_sqrt_price_at_tick(0).unwrap(), q96);
    assert_eq!(
        SqrtPriceMath::get_amount1_delta(q96, q96 + U256::ONE, U256::ONE, false).unwrap(),
        U256::ZERO
    );

    let quote_state = QuoteState {
        sqrt_price_x96: q96,
        liquidity: 1_000_000,
        tick: 0,
        fee: 3_000,
    };
    let step = Quoter::step_exact_input(&quote_state, U256::from(1_000u64), true).unwrap();
    assert!(step.amount_out > U256::ZERO);

    let direct = SwapMath::compute_swap_step(
        q96,
        TickMath::MIN_SQRT_PRICE + U256::ONE,
        quote_state.liquidity,
        SignedAmount::negative(U256::from(1_000u64)),
        quote_state.fee,
    )
    .unwrap();
    assert_eq!(step, direct);

    let v4_state = v4::PoolState {
        sqrt_price_x96: q96,
        liquidity: quote_state.liquidity,
        tick: 0,
        fee: quote_state.fee,
        tick_spacing: 60,
    };
    assert_eq!(
        v4::Pool::quote_exact_input(&v4_state, U256::from(1_000u64), true).unwrap(),
        step.amount_out
    );
}
