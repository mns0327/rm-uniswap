use std::str::FromStr;

use alloy::primitives::{Address, B256, I256};
use rm_uniswap::{
    Fee, ModifyLiquidityParams, Pool, PoolSnapshot, PoolState, ProtocolFee, SqrtPriceX96,
    SwapParams, TickIndex, TickInfoInner,
};
use ruint::aliases::U256;
use serde::{Deserialize, Deserializer, de};
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
struct HistoricalReplayFixture {
    schema_version: String,
    metadata: ReplayMetadata,
    initial_pool: PoolSnapshot,
    transactions: Vec<ReplayTransaction>,
}

#[derive(Debug, Deserialize)]
struct ReplayMetadata {
    chain_id: u64,
    pool_manager: Address,
    pool_key: PoolKeyFixture,
    pool_id: B256,
    start_block: u64,
    end_block: u64,
    tx_count: usize,
    log_count: usize,
    checkpoint_kind: String,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PoolKeyFixture {
    currency0: Address,
    currency1: Address,
    fee: u32,
    tick_spacing: u32,
    hooks: Address,
}

#[derive(Debug, Deserialize)]
struct ReplayTransaction {
    tx_hash: B256,
    block_number: u64,
    log_count: usize,
    operations: Vec<ReplayOperation>,
    #[serde(default)]
    expected_end_pool: Option<PoolSnapshot>,
    #[serde(default)]
    expected_checkpoint: Option<PoolCheckpoint>,
}

#[derive(Debug, Deserialize)]
struct PoolCheckpoint {
    state: PoolState,
    fee: Fee,
    protocol_fee: ProtocolFee,
    ticks: BTreeMap<TickIndex, TickInfoInner>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ReplayOperation {
    Swap {
        #[allow(dead_code)]
        sender: Address,
        zero_for_one: bool,
        #[serde(deserialize_with = "deserialize_i256_lossless")]
        amount_specified: I256,
        sqrt_price_limit_x96: U256,
    },
    ModifyLiquidity {
        #[allow(dead_code)]
        sender: Address,
        tick_lower: i32,
        tick_upper: i32,
        #[serde(deserialize_with = "deserialize_i128_lossless")]
        liquidity_delta: i128,
        salt: B256,
    },
    Donate {
        #[allow(dead_code)]
        sender: Address,
        #[allow(dead_code)]
        amount0: U256,
        #[allow(dead_code)]
        amount1: U256,
    },
}

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

fn deserialize_i256_lossless<'de, D>(deserializer: D) -> Result<I256, D::Error>
where
    D: Deserializer<'de>,
{
    struct I256Visitor;

    impl de::Visitor<'_> for I256Visitor {
        type Value = I256;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an int256 as a decimal string or integer")
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(I256::unchecked_from(value))
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            I256::try_from(U256::from(value)).map_err(E::custom)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            I256::from_str(value).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&value)
        }
    }

    deserializer.deserialize_any(I256Visitor)
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
    TickIndex::new(index).unwrap_or_else(|| panic!("historical fixture tick {index} is invalid"))
}

fn sqrt_price(value: U256) -> SqrtPriceX96 {
    SqrtPriceX96::from_u256(value)
        .unwrap_or_else(|| panic!("historical fixture sqrt price {value} is invalid"))
}

#[test]
fn committed_historical_replay_matches_rust() {
    run_fixture(
        "committed historical replay",
        include_str!("../forge/fixtures/historical_replay.json"),
    );
}

#[test]
fn sparse_historical_replay_checkpoint_matches_only_expected_fields() {
    run_fixture("sparse historical replay", SPARSE_FIXTURE);
}

fn run_fixture(name: &str, json: &str) {
    let fixture: HistoricalReplayFixture = serde_json::from_str(json)
        .unwrap_or_else(|error| panic!("{name} fixture JSON must match schema: {error}"));

    assert!(
        fixture.schema_version == "historical-replay-v1"
            || fixture.schema_version == "historical-replay-v2",
        "{name} fixture schema version must be historical-replay-v1 or historical-replay-v2"
    );
    assert!(
        matches!(
            fixture.metadata.checkpoint_kind.as_str(),
            "transaction_end" | "block_end" | "sparse_block_end"
        ),
        "{name} fixture checkpoint kind is unsupported"
    );
    assert_eq!(fixture.metadata.tx_count, fixture.transactions.len());
    assert!(fixture.metadata.start_block <= fixture.metadata.end_block);

    let mut pool = Pool::try_from(fixture.initial_pool)
        .unwrap_or_else(|error| panic!("{name} initial pool snapshot invalid: {error}"));

    let mut replayed_logs = 0usize;

    for (tx_idx, tx) in fixture.transactions.into_iter().enumerate() {
        assert!(
            fixture.metadata.start_block < tx.block_number
                && tx.block_number <= fixture.metadata.end_block,
            "{name} tx {tx_idx} block is outside replay range"
        );
        assert!(
            !tx.operations.is_empty(),
            "{name} tx {tx_idx} must contain at least one decoded operation"
        );

        for (op_idx, operation) in tx.operations.into_iter().enumerate() {
            match operation {
                ReplayOperation::Swap {
                    zero_for_one,
                    amount_specified,
                    sqrt_price_limit_x96,
                    ..
                } => {
                    pool.swap(SwapParams {
                        zero_for_one,
                        amount_specified,
                        sqrt_price_limit_x96: sqrt_price(sqrt_price_limit_x96),
                    })
                    .unwrap_or_else(|error| {
                        panic!("{name} tx {tx_idx} operation {op_idx} swap replay failed: {error}")
                    });
                }
                ReplayOperation::ModifyLiquidity {
                    sender,
                    tick_lower,
                    tick_upper,
                    liquidity_delta,
                    salt,
                } => {
                    pool.modify_liquidity(ModifyLiquidityParams {
                        tick_lower: tick(tick_lower),
                        tick_upper: tick(tick_upper),
                        liquidity_delta,
                        owner: sender,
                        salt,
                    })
                    .unwrap_or_else(|error| {
                        panic!(
                            "{name} tx {tx_idx} operation {op_idx} modify replay failed: {error}"
                        )
                    });
                }
                ReplayOperation::Donate { .. } => {
                    panic!(
                        "{name} tx {tx_idx} operation {op_idx} contains donate; Pool::donate replay support is not implemented yet"
                    );
                }
            }
        }

        replayed_logs = replayed_logs
            .checked_add(tx.log_count)
            .expect("historical replay log count overflow");

        if let Some(expected) = tx.expected_end_pool {
            expected
                .validate()
                .unwrap_or_else(|error| panic!("{name} tx {tx_idx} checkpoint invalid: {error}"));
            assert_eq!(
                pool.snapshot(),
                expected,
                "{name} tx {tx_idx} ({}) end checkpoint",
                tx.tx_hash
            );
        }
        if let Some(expected) = tx.expected_checkpoint {
            let actual = pool.snapshot();
            assert_eq!(
                actual.state, expected.state,
                "{name} tx {tx_idx} ({}) state checkpoint",
                tx.tx_hash
            );
            assert_eq!(
                actual.fee, expected.fee,
                "{name} tx {tx_idx} ({}) fee checkpoint",
                tx.tx_hash
            );
            assert_eq!(
                actual.protocol_fee, expected.protocol_fee,
                "{name} tx {tx_idx} ({}) protocol fee checkpoint",
                tx.tx_hash
            );
            for (tick, expected_tick) in expected.ticks {
                if expected_tick.liquidity_gross.is_zero() {
                    assert!(
                        actual
                            .ticks
                            .inner
                            .get(&tick)
                            .is_none_or(|tick| tick.liquidity_gross.is_zero()),
                        "{name} tx {tx_idx} ({}) tick {tick:?} should be absent/deinitialized",
                        tx.tx_hash
                    );
                } else {
                    assert_eq!(
                        actual.ticks.inner.get(&tick),
                        Some(&expected_tick),
                        "{name} tx {tx_idx} ({}) tick {tick:?} checkpoint",
                        tx.tx_hash
                    );
                }
            }
        }
    }

    assert_eq!(fixture.metadata.log_count, replayed_logs);
    assert!(fixture.metadata.pool_key.fee <= 1_000_000);
    assert!(fixture.metadata.pool_key.tick_spacing > 0);
    assert_ne!(fixture.metadata.pool_id, B256::ZERO);
    let _pool_manager = fixture.metadata.pool_manager;
    assert!(fixture.metadata.chain_id > 0);
    assert!(fixture.metadata.notes.len() <= 8);
    assert_ne!(
        fixture.metadata.pool_key.currency0,
        fixture.metadata.pool_key.currency1
    );
    let _hooks = fixture.metadata.pool_key.hooks;
}

const SPARSE_FIXTURE: &str = r#"{
  "schema_version": "historical-replay-v2",
  "metadata": {
    "chain_id": 1,
    "pool_manager": "0x0000000000000000000000000000000000000000",
    "pool_key": {
      "currency0": "0x0000000000000000000000000000000000000000",
      "currency1": "0x0000000000000000000000000000000000000001",
      "fee": 3000,
      "tick_spacing": 60,
      "hooks": "0x0000000000000000000000000000000000000000"
    },
    "pool_id": "0x214e66d3e6337d8d97fc90800244dd54c85f51e92185118959f7d2702937ce52",
    "start_block": 1,
    "end_block": 2,
    "tx_count": 1,
    "log_count": 1,
    "checkpoint_kind": "sparse_block_end",
    "notes": [
      "Sparse smoke fixture."
    ]
  },
  "initial_pool": {
    "state": {
      "sqrt_price_x96": "79228162514264337593543950336",
      "tick": 0,
      "liquidity": 0,
      "fee_growth_global0_x128": "0",
      "fee_growth_global1_x128": "0"
    },
    "fee": 3000,
    "protocol_fee": {
      "zero_for_one_fee": 0,
      "one_for_zero_fee": 0
    },
    "tick_spacing": 60,
    "ticks": {
      "tick_spacing": 60,
      "inner": {}
    }
  },
  "transactions": [
    {
      "tx_hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
      "block_number": 2,
      "log_count": 1,
      "operations": [
        {
          "type": "modify_liquidity",
          "sender": "0x1111111111111111111111111111111111111111",
          "tick_lower": -60,
          "tick_upper": 60,
          "liquidity_delta": "1000000",
          "salt": "0x0000000000000000000000000000000000000000000000000000000000000000"
        }
      ],
      "expected_checkpoint": {
        "state": {
          "sqrt_price_x96": "79228162514264337593543950336",
          "tick": 0,
          "liquidity": 1000000,
          "fee_growth_global0_x128": "0",
          "fee_growth_global1_x128": "0"
        },
        "fee": 3000,
        "protocol_fee": {
          "zero_for_one_fee": 0,
          "one_for_zero_fee": 0
        },
        "ticks": {
          "-60": {
            "liquidity_gross": 1000000,
            "liquidity_net": 1000000
          },
          "60": {
            "liquidity_gross": 1000000,
            "liquidity_net": -1000000
          }
        }
      }
    }
  ]
}"#;
