#![cfg(feature = "positions")]

use std::{collections::BTreeMap, env, fs, path::PathBuf, str::FromStr};

use alloy::primitives::{Address, B256, I256};
use rm_uniswap::v4::{
    BalanceDelta, Error, ModifyLiquidityParams, ModifyLiquidityResult, Pool, PoolSnapshot,
    ProtocolFee, SqrtPriceX96, SwapParams, TickIndex,
    positions::{PositionIndex, Positions},
};
use ruint::aliases::U256;
use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct ForgeParityFixture {
    #[allow(dead_code)]
    metadata: Option<FixtureMetadata>,
    owner: String,
    initial_pool: PoolSnapshot,
    operations: Vec<Operation>,
    final_pool: Option<PoolSnapshot>,
}

#[derive(Debug, Deserialize)]
struct FixtureMetadata {
    #[allow(dead_code)]
    name: Option<String>,
    #[allow(dead_code)]
    chain_id: Option<u64>,
    #[allow(dead_code)]
    block_number: Option<u64>,
    #[allow(dead_code)]
    pool_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Operation {
    Mint {
        tick_lower: i32,
        tick_upper: i32,
        #[serde(deserialize_with = "deserialize_u128_lossless")]
        liquidity: u128,
        #[serde(deserialize_with = "deserialize_u128_lossless")]
        amount0_max: u128,
        #[serde(deserialize_with = "deserialize_u128_lossless")]
        amount1_max: u128,
        expect: Option<ExpectedMint>,
    },
    Increase {
        token_id: u64,
        #[serde(deserialize_with = "deserialize_u128_lossless")]
        liquidity: u128,
        #[serde(deserialize_with = "deserialize_u128_lossless")]
        amount0_max: u128,
        #[serde(deserialize_with = "deserialize_u128_lossless")]
        amount1_max: u128,
        expect: Option<ExpectedModify>,
    },
    Swap {
        zero_for_one: bool,
        exact: Exactness,
        amount: U256,
        sqrt_price_limit_x96: Option<U256>,
        protocol_fee: Option<u32>,
        expect: Option<ExpectedSwap>,
    },
    Collect {
        token_id: u64,
        expect: Option<ExpectedModify>,
    },
    Decrease {
        token_id: u64,
        #[serde(deserialize_with = "deserialize_u128_lossless")]
        liquidity: u128,
        #[serde(deserialize_with = "deserialize_u128_lossless")]
        amount0_min: u128,
        #[serde(deserialize_with = "deserialize_u128_lossless")]
        amount1_min: u128,
        expect: Option<ExpectedModify>,
    },
    Burn {
        token_id: u64,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Exactness {
    Input,
    Output,
}

#[derive(Debug, Deserialize)]
struct ExpectedMint {
    token_id: Option<u64>,
    #[serde(flatten)]
    result: ExpectedModify,
}

#[derive(Debug, Deserialize)]
struct ExpectedModify {
    principal_delta: ExpectedDelta,
    fee_delta: ExpectedDelta,
    #[serde(deserialize_with = "deserialize_u128_lossless")]
    liquidity: u128,
}

#[derive(Debug, Deserialize)]
struct ExpectedSwap {
    delta: ExpectedDelta,
    sqrt_price_x96: U256,
    tick: i32,
    #[serde(deserialize_with = "deserialize_u128_lossless")]
    liquidity: u128,
    fee_growth_global0_x128: U256,
    fee_growth_global1_x128: U256,
    #[serde(default, deserialize_with = "deserialize_option_u128_lossless")]
    protocol_fee_amount: Option<u128>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct ExpectedDelta {
    #[serde(deserialize_with = "deserialize_i128_lossless")]
    amount0: i128,
    #[serde(deserialize_with = "deserialize_i128_lossless")]
    amount1: i128,
}

#[derive(Debug, Clone, Copy)]
struct FixturePosition {
    owner: Address,
    tick_lower: TickIndex,
    tick_upper: TickIndex,
    salt: B256,
}

impl From<ExpectedDelta> for BalanceDelta {
    fn from(value: ExpectedDelta) -> Self {
        Self {
            amount0: value.amount0,
            amount1: value.amount1,
        }
    }
}

fn deserialize_u128_lossless<'de, D>(deserializer: D) -> Result<u128, D::Error>
where
    D: Deserializer<'de>,
{
    struct U128Visitor;

    impl de::Visitor<'_> for U128Visitor {
        type Value = u128;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a u128 as a decimal string, hex string, or unsigned integer")
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(u128::from(value))
        }

        fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_u128_lossless(value).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&value)
        }
    }

    deserializer.deserialize_any(U128Visitor)
}

fn deserialize_option_u128_lossless<'de, D>(deserializer: D) -> Result<Option<u128>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<LosslessU128>::deserialize(deserializer).map(|value| value.map(|value| value.0))
}

#[derive(Deserialize)]
#[serde(transparent)]
struct LosslessU128(#[serde(deserialize_with = "deserialize_u128_lossless")] u128);

fn deserialize_i128_lossless<'de, D>(deserializer: D) -> Result<i128, D::Error>
where
    D: Deserializer<'de>,
{
    struct I128Visitor;

    impl de::Visitor<'_> for I128Visitor {
        type Value = i128;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an i128 as a decimal string, hex string, or integer")
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(i128::from(value))
        }

        fn visit_i128<E>(self, value: i128) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value)
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(i128::from(value))
        }

        fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            i128::try_from(value).map_err(E::custom)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_i128_lossless(value).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&value)
        }
    }

    deserializer.deserialize_any(I128Visitor)
}

fn parse_u128_lossless(value: &str) -> Result<u128, String> {
    if let Some(hex) = value.strip_prefix("0x") {
        u128::from_str_radix(hex, 16).map_err(|error| error.to_string())
    } else {
        value.parse::<u128>().map_err(|error| error.to_string())
    }
}

fn parse_i128_lossless(value: &str) -> Result<i128, String> {
    if let Some(hex) = value.strip_prefix("-0x") {
        let magnitude = u128::from_str_radix(hex, 16).map_err(|error| error.to_string())?;
        if magnitude == 1u128 << 127 {
            Ok(i128::MIN)
        } else {
            i128::try_from(magnitude)
                .map(|value| -value)
                .map_err(|error| error.to_string())
        }
    } else if let Some(hex) = value.strip_prefix("0x") {
        i128::from_str_radix(hex, 16).map_err(|error| error.to_string())
    } else {
        value.parse::<i128>().map_err(|error| error.to_string())
    }
}

fn tick(index: i32) -> TickIndex {
    TickIndex::new(index).unwrap()
}

fn sqrt_price(value: U256) -> SqrtPriceX96 {
    SqrtPriceX96::from_u256(value).unwrap()
}

#[test]
fn committed_forge_fixtures_match_rust() {
    run_fixture_file("committed", include_str!("../forge/fixtures/parity.json"));
}

#[test]
#[ignore = "manual replay for newly generated Forge fixtures before committing them"]
fn external_forge_fixture_matches_rust() {
    let path = env::var_os("FORGE_PARITY_FIXTURE")
        .map(PathBuf::from)
        .expect("set FORGE_PARITY_FIXTURE to a Forge-generated JSON fixture");
    let json = fs::read_to_string(&path).expect("fixture file must be readable");
    run_fixture_file(&path.display().to_string(), &json);
}

fn run_fixture_file(name: &str, json: &str) {
    let value: Value = serde_json::from_str(json)
        .unwrap_or_else(|error| panic!("fixture file {name} must be valid JSON: {error}"));

    match value {
        Value::Array(fixtures) => {
            assert!(
                !fixtures.is_empty(),
                "fixture file {name} must not be empty"
            );
            for (idx, fixture) in fixtures.into_iter().enumerate() {
                run_fixture_value(&format!("{name}[{idx}]"), fixture);
            }
        }
        fixture => run_fixture_value(name, fixture),
    }
}

fn run_fixture_value(name: &str, value: Value) {
    let final_pool_has_protocol_fee = value
        .get("final_pool")
        .and_then(Value::as_object)
        .is_some_and(|pool| pool.contains_key("protocol_fee"));

    let fixture: ForgeParityFixture = serde_json::from_value(value).unwrap_or_else(|error| {
        panic!("fixture {name} JSON must match forge parity schema: {error}")
    });
    run_fixture(name, fixture, final_pool_has_protocol_fee);
}

fn run_fixture(name: &str, fixture: ForgeParityFixture, final_pool_has_protocol_fee: bool) {
    fixture
        .initial_pool
        .validate()
        .unwrap_or_else(|error| panic!("fixture {name} initial snapshot invalid: {error}"));

    let owner = Address::from_str(&fixture.owner)
        .unwrap_or_else(|error| panic!("fixture {name} owner must be a hex address: {error}"));
    let pool = Pool::try_from(fixture.initial_pool)
        .unwrap_or_else(|error| panic!("fixture {name} initial pool construct failed: {error}"));
    let mut pool = with_positions(pool);
    let mut positions = BTreeMap::<u64, FixturePosition>::new();
    let mut next_token_id = 1u64;

    for (idx, operation) in fixture.operations.into_iter().enumerate() {
        match operation {
            Operation::Mint {
                tick_lower,
                tick_upper,
                liquidity,
                amount0_max,
                amount1_max,
                expect,
            } => {
                let token_id = next_token_id;
                next_token_id = next_token_id
                    .checked_add(1)
                    .unwrap_or_else(|| panic!("fixture {name} operation {idx} token_id overflow"));
                let position = FixturePosition {
                    owner,
                    tick_lower: tick(tick_lower),
                    tick_upper: tick(tick_upper),
                    salt: token_id_salt(token_id),
                };
                let result = modify_position(
                    &mut pool,
                    position,
                    liquidity_to_i128(name, idx, liquidity),
                    amount0_max,
                    amount1_max,
                    0,
                    0,
                )
                .unwrap_or_else(|error| {
                    panic!("fixture {name} operation {idx} mint failed: {error}")
                });
                if let Some(expect) = expect {
                    if let Some(expected_token_id) = expect.token_id {
                        assert_eq!(
                            token_id, expected_token_id,
                            "fixture {name} operation {idx} token_id"
                        );
                    }
                    assert_modify_result(
                        name,
                        idx,
                        &result,
                        &expect.result,
                        pool.snapshot().state.liquidity.value(),
                    );
                }
                positions.insert(token_id, position);
            }
            Operation::Increase {
                token_id,
                liquidity,
                amount0_max,
                amount1_max,
                expect,
            } => {
                let position = position_for(name, idx, &positions, token_id);
                let result = modify_position(
                    &mut pool,
                    position,
                    liquidity_to_i128(name, idx, liquidity),
                    amount0_max,
                    amount1_max,
                    0,
                    0,
                )
                .unwrap_or_else(|error| {
                    panic!("fixture {name} operation {idx} increase failed: {error}")
                });
                if let Some(expect) = expect {
                    assert_modify_result(
                        name,
                        idx,
                        &result,
                        &expect,
                        pool.snapshot().state.liquidity.value(),
                    );
                }
            }
            Operation::Swap {
                zero_for_one,
                exact,
                amount,
                sqrt_price_limit_x96,
                protocol_fee,
                expect,
            } => {
                let signed_amount = I256::try_from(amount).unwrap_or_else(|error| {
                    panic!("fixture {name} operation {idx} amount must fit int256: {error}")
                });
                let signed_amount = match exact {
                    Exactness::Input => -signed_amount,
                    Exactness::Output => signed_amount,
                };
                let mut params = SwapParams::new(zero_for_one, signed_amount);
                if let Some(limit) = sqrt_price_limit_x96 {
                    params.sqrt_price_limit_x96 = sqrt_price(limit);
                }
                {
                    let protocol_fee = protocol_fee
                        .map(|protocol_fee| {
                            u16::try_from(protocol_fee).unwrap_or_else(|error| {
                                panic!(
                                    "fixture {name} operation {idx} protocol_fee must fit u16: {error}"
                                )
                            })
                        })
                        .unwrap_or(0);
                    let protocol_fee = if zero_for_one {
                        ProtocolFee::new(protocol_fee, 0)
                    } else {
                        ProtocolFee::new(0, protocol_fee)
                    }
                    .unwrap_or_else(|| {
                        panic!("fixture {name} operation {idx} protocol_fee must fit v4 maximum")
                    });

                    pool.set_protocol_fee(protocol_fee).unwrap();
                }

                let result = pool.swap(params).unwrap_or_else(|error| {
                    panic!("fixture {name} operation {idx} swap failed: {error}")
                });
                if let Some(expect) = expect {
                    assert_eq!(
                        result.swap_delta,
                        expect.delta.into(),
                        "fixture {name} operation {idx} delta"
                    );
                    assert_eq!(
                        result.sqrt_price_x96,
                        sqrt_price(expect.sqrt_price_x96),
                        "fixture {name} operation {idx} sqrt_price_x96"
                    );
                    assert_eq!(
                        result.tick,
                        tick(expect.tick),
                        "fixture {name} operation {idx} tick"
                    );
                    assert_eq!(
                        result.liquidity.value(),
                        expect.liquidity,
                        "fixture {name} operation {idx} liquidity"
                    );
                    let pool_state = pool.snapshot().state;
                    assert_eq!(
                        pool_state.fee_growth_global0_x128, expect.fee_growth_global0_x128,
                        "fixture {name} operation {idx} fee_growth_global0_x128"
                    );
                    assert_eq!(
                        pool_state.fee_growth_global1_x128, expect.fee_growth_global1_x128,
                        "fixture {name} operation {idx} fee_growth_global1_x128"
                    );
                    if let Some(protocol_fee_amount) = expect.protocol_fee_amount {
                        assert_eq!(
                            result.amount_to_protocol,
                            U256::from(protocol_fee_amount),
                            "fixture {name} operation {idx} protocol_fee_amount"
                        );
                    }
                }
            }
            Operation::Collect { token_id, expect } => {
                let position = position_for(name, idx, &positions, token_id);
                let result = modify_position(&mut pool, position, 0, u128::MAX, u128::MAX, 0, 0)
                    .unwrap_or_else(|error| {
                        panic!("fixture {name} operation {idx} collect failed: {error}")
                    });
                if let Some(expect) = expect {
                    assert_modify_result(
                        name,
                        idx,
                        &result,
                        &expect,
                        pool.snapshot().state.liquidity.value(),
                    );
                }
            }
            Operation::Decrease {
                token_id,
                liquidity,
                amount0_min,
                amount1_min,
                expect,
            } => {
                let position = position_for(name, idx, &positions, token_id);
                let result = modify_position(
                    &mut pool,
                    position,
                    -liquidity_to_i128(name, idx, liquidity),
                    u128::MAX,
                    u128::MAX,
                    amount0_min,
                    amount1_min,
                )
                .unwrap_or_else(|error| {
                    panic!("fixture {name} operation {idx} decrease failed: {error}")
                });
                if let Some(expect) = expect {
                    assert_modify_result(
                        name,
                        idx,
                        &result,
                        &expect,
                        pool.snapshot().state.liquidity.value(),
                    );
                }
            }
            Operation::Burn { token_id } => {
                let position = position_for(name, idx, &positions, token_id);
                burn_position(&mut pool, position).unwrap_or_else(|error| {
                    panic!("fixture {name} operation {idx} burn failed: {error}")
                });
                positions.remove(&token_id);
                assert!(
                    pool.positions.0.get(&position.index()).is_none(),
                    "fixture {name} operation {idx} position should be removed"
                );
            }
        }
    }

    if let Some(mut expected_final_pool) = fixture.final_pool {
        expected_final_pool
            .validate()
            .unwrap_or_else(|error| panic!("fixture {name} final snapshot invalid: {error}"));
        let actual_final_pool = snapshot_without_positions(&pool);
        if !final_pool_has_protocol_fee {
            expected_final_pool.protocol_fee = actual_final_pool.protocol_fee;
        }
        assert_eq!(
            actual_final_pool, expected_final_pool,
            "fixture {name} final pool"
        );
    }

    assert!(
        next_token_id > 1,
        "fixture {name} should mint at least one position"
    );
}

fn with_positions(pool: Pool) -> Pool<Positions> {
    Pool {
        state: pool.state,
        swap_fee: pool.swap_fee,
        tick_spacing: pool.tick_spacing,
        ticks: pool.ticks,
        positions: Positions::new(),
    }
}

fn snapshot_without_positions(pool: &Pool<Positions>) -> PoolSnapshot {
    let snapshot = pool.snapshot();
    PoolSnapshot {
        state: snapshot.state,
        fee: snapshot.fee,
        protocol_fee: snapshot.protocol_fee,
        tick_spacing: snapshot.tick_spacing,
        ticks: snapshot.ticks,
        positions: (),
    }
}

fn position_for(
    fixture: &str,
    idx: usize,
    positions: &BTreeMap<u64, FixturePosition>,
    token_id: u64,
) -> FixturePosition {
    positions.get(&token_id).copied().unwrap_or_else(|| {
        panic!("fixture {fixture} operation {idx} position {token_id} should exist")
    })
}

fn modify_position(
    pool: &mut Pool<Positions>,
    position: FixturePosition,
    liquidity_delta: i128,
    amount0_max: u128,
    amount1_max: u128,
    amount0_min: u128,
    amount1_min: u128,
) -> Result<ModifyLiquidityResult, Error> {
    let params = ModifyLiquidityParams {
        tick_lower: position.tick_lower,
        tick_upper: position.tick_upper,
        liquidity_delta,
        owner: position.owner,
        salt: position.salt,
    };
    let quoted = pool.quote_modify_liquidity(params)?;
    validate_slippage(
        quoted,
        liquidity_delta,
        amount0_max,
        amount1_max,
        amount0_min,
        amount1_min,
    )?;

    pool.modify_liquidity(params)
}

fn burn_position(pool: &mut Pool<Positions>, position: FixturePosition) -> Result<(), Error> {
    let state = pool
        .positions
        .0
        .get(&position.index())
        .ok_or(Error::PositionNotFound)?;
    if state.liquidity.value() != 0 {
        return Err(Error::PositionNotEmpty);
    }
    pool.positions.0.remove(&position.index());
    Ok(())
}

fn validate_slippage(
    delta: BalanceDelta,
    liquidity_delta: i128,
    amount0_max: u128,
    amount1_max: u128,
    amount0_min: u128,
    amount1_min: u128,
) -> Result<(), Error> {
    if liquidity_delta >= 0 {
        let amount0 = negative_delta_to_u128(delta.amount0)?;
        let amount1 = negative_delta_to_u128(delta.amount1)?;
        if amount0 > amount0_max || amount1 > amount1_max {
            return Err(Error::SlippageExceeded);
        }
    } else {
        let amount0 = u128::try_from(delta.amount0).map_err(|_| Error::AmountOverflow)?;
        let amount1 = u128::try_from(delta.amount1).map_err(|_| Error::AmountOverflow)?;
        if amount0 < amount0_min || amount1 < amount1_min {
            return Err(Error::SlippageExceeded);
        }
    }
    Ok(())
}

fn negative_delta_to_u128(value: i128) -> Result<u128, Error> {
    if value > 0 {
        return Err(Error::AmountOverflow);
    }
    Ok(value.unsigned_abs())
}

fn liquidity_to_i128(fixture: &str, idx: usize, liquidity: u128) -> i128 {
    i128::try_from(liquidity)
        .unwrap_or_else(|error| panic!("fixture {fixture} operation {idx} liquidity: {error}"))
}

fn token_id_salt(token_id: u64) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[24..].copy_from_slice(&token_id.to_be_bytes());
    B256::from(bytes)
}

impl FixturePosition {
    fn index(self) -> PositionIndex {
        PositionIndex {
            owner: self.owner,
            tick_lower: self.tick_lower,
            tick_upper: self.tick_upper,
            salt: self.salt,
        }
    }
}

fn assert_modify_result(
    fixture: &str,
    idx: usize,
    actual: &ModifyLiquidityResult,
    expected: &ExpectedModify,
    actual_pool_liquidity: u128,
) {
    assert_eq!(
        actual.delta,
        expected.principal_delta.into(),
        "fixture {fixture} operation {idx} principal_delta"
    );
    assert_eq!(
        actual.fee_delta,
        expected.fee_delta.into(),
        "fixture {fixture} operation {idx} fee_delta"
    );
    assert_eq!(
        actual_pool_liquidity, expected.liquidity,
        "fixture {fixture} operation {idx} pool liquidity"
    );
}
