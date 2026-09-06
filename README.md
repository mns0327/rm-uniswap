# rm-uniswap

[![Build Status](https://github.com/mns0327/rm-uniswap/actions/workflows/ci.yml/badge.svg)](https://github.com/mns0327/rm-uniswap/actions)
[![crates.io](https://img.shields.io/crates/v/rm-uniswap.svg)](https://crates.io/crates/rm-uniswap)
[![Documentation](https://docs.rs/rm-uniswap/badge.svg)](https://docs.rs/rm-uniswap)
[![Rust](https://img.shields.io/badge/rust-1.94.1%2B-blue.svg?maxAge=3600)](https://github.com/mns0327/rm-uniswap)

Rm-uniswap is a Rust port of Uniswap’s concentrated liquidity pool math and state simulation.

The library is built for high-performance simulation and includes both v3 and v4 pool implementations.

## Usage

```toml
[dependencies]
rm-uniswap = "0.1"
```

```rust
use rm_uniswap::{
    Address, Fee, FixedBytes, I256, Liquidity, ModifyLiquidityParams, Pool, ProtocolFee,
    SqrtPriceX96, SwapFee, SwapParams, TickIndex, TickSpacing, U256,
};

// Pools start empty, at price 1.0, with a 0.3% swap fee.
let sqrt_price = SqrtPriceX96::from_u256(U256::ONE << 96).unwrap();
let swap_fee = SwapFee::new(Fee::new(3_000).unwrap(), ProtocolFee::ZERO).unwrap();

let mut pool: Pool = Pool::new(
    sqrt_price,
    TickIndex::new(0).unwrap(),
    Liquidity::ZERO,
    swap_fee,
    TickSpacing::new(60).unwrap(),
);

// A swap needs liquidity in range first, so mint a position around the
// current price before swapping.
pool.modify_liquidity(ModifyLiquidityParams {
    tick_lower: TickIndex::new(-60).unwrap(),
    tick_upper: TickIndex::new(60).unwrap(),
    liquidity_delta: 1_000_000_000_000,
    owner: Address::ZERO,
    salt: FixedBytes::ZERO,
})
.unwrap();

// Exact input swap of 1_000_000 units of token0 for token1.
let result = pool
    .swap(SwapParams::new(true, -I256::from(U256::from(1_000_000u64))))
    .unwrap();

println!("{:?}", result.swap_delta);
```

For pools with initialized ticks or state loaded from elsewhere (e.g. an
indexer or archive node), build a `PoolTicks` snapshot and use
`Pool::try_new`/`Pool::try_from`, which validate every state invariant.

## What's included

- Concentrated-liquidity pool state and swap simulation (`Pool`, `SwapParams`, `SwapResult`)
- Tick math and tick-crossing bookkeeping (`tick_math`, `TickIndex`, `TickSpacing`, `PoolTicks`)
- Fee and protocol-fee accounting (`Fee`, `SwapFee`, `ProtocolFee`)
- Full-precision and sqrt-price math primitives (`full_math`, `sqrt_price_math`, `swap_math`)
- `v4`: hooks, `PoolKey`/`PoolId`, and a single-pool manager with position tracking
- `v3`: reserved for v3-specific types; shared concentrated-liquidity types above already cover v3 pool math

## Flags

This crate has the following Cargo features:

- `serde`: Derives `Serialize`/`Deserialize` for pool, tick, and fee types, and
  enables `Pool`/`PoolSnapshot` round-tripping through any serde format.
  (enabled by default)

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
