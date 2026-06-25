use rm_uniswap::v4::{
    FullMath, Pool, PoolTicks, QuoteState, Quoter, SignedAmount, SqrtPriceMath, SwapMath,
    SwapParams, TickMath, delta_amount_out,
};
use ruint::aliases::U256;

#[test]
fn v4_is_the_public_entry_point() {
    let q96 = U256::ONE << 96;

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

    let pool = Pool::new(
        q96,
        0,
        quote_state.liquidity,
        3_000,
        60,
        PoolTicks::new(60).unwrap(),
    );
    let simulated = pool
        .simulate_swap(SwapParams::new(
            true,
            SignedAmount::negative(U256::from(1_000u64)),
        ))
        .unwrap();
    assert!(delta_amount_out(&simulated.delta, true) > 0);
}
