use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::Arc,
};

use alloy::{
    primitives::{Address, B256, Bytes, I256, keccak256},
    providers::{Provider, RootProvider},
    rpc::{
        client::ClientBuilder,
        types::{Filter, Log},
    },
    sol,
    sol_types::{SolEvent, SolValue},
    transports::{
        http::Http,
        layers::{FallbackService, RetryBackoffLayer},
    },
};
use anyhow::{Context, Result, ensure};
use futures::{StreamExt, stream};
use rm_uniswap::{
    Fee, Liquidity, PoolSnapshot, PoolState, PoolTicksSnapshot, ProtocolFee, SqrtPriceX96,
    TickIndex, TickInfoInner, TickSpacing,
};
use ruint::aliases::{U160, U256};
use serde::{Deserialize, Serialize};

use crate::historical_replay::{PoolCheckpoint, Progress};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CallFrame {
    #[serde(default)]
    pub(crate) from: Option<Address>,
    #[serde(default)]
    pub(crate) to: Option<Address>,
    #[serde(default)]
    pub(crate) input: Option<Bytes>,
    #[serde(default)]
    pub(crate) error: Option<String>,
    #[serde(default)]
    pub(crate) revert_reason: Option<String>,
    #[serde(default)]
    pub(crate) calls: Vec<CallFrame>,
}

impl CallFrame {
    pub(crate) fn reverted(&self) -> bool {
        self.error.is_some() || self.revert_reason.is_some()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DebugTraceOptions {
    tracer: &'static str,
}

const POOLS_SLOT: u64 = 6;
const FEE_GROWTH_GLOBAL0_OFFSET: u64 = 1;
const FEE_GROWTH_GLOBAL1_OFFSET: u64 = 2;
const LIQUIDITY_OFFSET: u64 = 3;
const TICKS_OFFSET: u64 = 4;
const TICK_BITMAP_OFFSET: u64 = 5;
pub(crate) const STORAGE_BATCH_SIZE: usize = 256;
const RPC_MAX_RETRIES: u32 = 6;
const RPC_INITIAL_BACKOFF_MS: u64 = 1_000;
const RPC_COMPUTE_UNITS_PER_SECOND: u64 = 100;

sol! {
    #[sol(rpc)]
    interface IPoolManager {
        struct PoolKey {
            address currency0;
            address currency1;
            uint24 fee;
            int24 tickSpacing;
            address hooks;
        }

        struct ModifyLiquidityParams {
            int24 tickLower;
            int24 tickUpper;
            int256 liquidityDelta;
            bytes32 salt;
        }

        struct SwapParams {
            bool zeroForOne;
            int256 amountSpecified;
            uint160 sqrtPriceLimitX96;
        }

        event Swap(
            bytes32 indexed id,
            address indexed sender,
            int128 amount0,
            int128 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick,
            uint24 fee
        );

        event ModifyLiquidity(
            bytes32 indexed id,
            address indexed sender,
            int24 tickLower,
            int24 tickUpper,
            int256 liquidityDelta,
            bytes32 salt
        );

        event Donate(
            bytes32 indexed id,
            address indexed sender,
            uint256 amount0,
            uint256 amount1
        );

        function modifyLiquidity(
            PoolKey key,
            ModifyLiquidityParams params,
            bytes hookData
        ) external returns (int256 callerDelta, int256 feesAccrued);

        function swap(
            PoolKey key,
            SwapParams params,
            bytes hookData
        ) external returns (int256 swapDelta);

        function donate(
            PoolKey key,
            uint256 amount0,
            uint256 amount1,
            bytes hookData
        ) external returns (int256 delta);

        function extsload(bytes32[] calldata slots) external view returns (bytes32[] memory values);
    }
}

#[derive(Debug, Clone)]
struct PoolCoreSnapshot {
    state: PoolState,
    fee: Fee,
    protocol_fee: ProtocolFee,
    tick_spacing: TickSpacing,
}

#[derive(Clone)]
pub(crate) struct RpcClient {
    provider: Arc<RootProvider>,
    endpoint_count: usize,
}

impl RpcClient {
    pub(crate) fn new(urls: Vec<String>) -> Result<Self> {
        let urls = urls
            .into_iter()
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
            .collect::<Vec<_>>();
        let endpoint_count = NonZeroUsize::new(urls.len())
            .context("at least one --rpc-url or ETH_RPC_URL value is required")?;

        let transports = urls
            .iter()
            .map(|url| Ok(Http::new(url.parse()?)))
            .collect::<Result<Vec<_>>>()?;

        // FallbackService rotates to the next endpoint when one keeps failing
        // (including "historical state is not available" on non-archive nodes).
        // RetryBackoffLayer handles 429 / -32005 with exponential backoff.
        let fallback = FallbackService::new(transports, endpoint_count.get());
        let client = ClientBuilder::default()
            .layer(RetryBackoffLayer::new(
                RPC_MAX_RETRIES,
                RPC_INITIAL_BACKOFF_MS,
                RPC_COMPUTE_UNITS_PER_SECOND,
            ))
            .transport(fallback, false);

        Ok(Self {
            provider: Arc::new(RootProvider::new(client)),
            endpoint_count: endpoint_count.get(),
        })
    }

    pub(crate) fn endpoint_count(&self) -> usize {
        self.endpoint_count
    }

    pub(crate) async fn chain_id(&self) -> Result<u64> {
        Ok(self.provider.get_chain_id().await?)
    }

    pub(crate) async fn pool_logs(
        &self,
        pool_manager: Address,
        pool_id: B256,
        from_block: u64,
        to_block: u64,
        chunk_size: u64,
        requested_concurrency: usize,
        progress: &Progress,
    ) -> Result<Vec<Log>> {
        let chunks = block_chunks(from_block, to_block, chunk_size);
        let total_chunks = chunks.len();
        if total_chunks == 0 {
            return Ok(Vec::new());
        }

        let concurrency = bounded_concurrency(requested_concurrency, total_chunks);
        progress.log(
            "log scan",
            format!(
                "fetching {total_chunks} chunks with concurrency={concurrency}, requests_per_chunk=3"
            ),
        );

        let mut fetches = stream::iter(chunks.into_iter().enumerate())
            .map(|(chunk_index, (start, end))| async move {
                let logs = self
                    .pool_logs_chunk(pool_manager, pool_id, start, end)
                    .await?;
                anyhow::Ok((chunk_index, start, end, logs))
            })
            .buffer_unordered(concurrency);

        let mut out = Vec::new();
        let mut completed = 0usize;
        while let Some(fetched) = fetches.next().await {
            let (chunk_index, start, end, mut logs) = fetched?;
            let chunk_logs = logs.len();
            out.append(&mut logs);
            completed += 1;
            progress.log(
                "log scan",
                format!(
                    "chunk {}/{} completed={} blocks {}-{} chunk_logs={} total_logs={}",
                    chunk_index + 1,
                    total_chunks,
                    completed,
                    start,
                    end,
                    chunk_logs,
                    out.len()
                ),
            );
        }

        Ok(out)
    }

    /// One request per event topic: some providers reject OR-arrays in `topics[0]`.
    async fn pool_logs_chunk(
        &self,
        pool_manager: Address,
        pool_id: B256,
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<Log>> {
        let mut out = Vec::new();
        for event_topic in POOL_EVENT_TOPICS {
            let mut logs = self
                .provider
                .get_logs(&pool_logs_filter(
                    pool_manager,
                    pool_id,
                    event_topic,
                    from_block,
                    to_block,
                ))
                .await?;
            out.append(&mut logs);
        }
        Ok(out)
    }

    pub(crate) async fn debug_call_trace(&self, tx_hash: B256) -> Result<CallFrame> {
        let frame = self
            .provider
            .client()
            .request(
                "debug_traceTransaction",
                (
                    tx_hash,
                    DebugTraceOptions {
                        tracer: "callTracer",
                    },
                ),
            )
            .await?;
        Ok(frame)
    }

    pub(crate) async fn pool_snapshot(
        &self,
        pool_manager: Address,
        pool_id: B256,
        tick_spacing: u32,
        block_number: u64,
        progress: &Progress,
        progress_label: &str,
    ) -> Result<PoolSnapshot> {
        let core = self
            .pool_core_snapshot(pool_manager, pool_id, tick_spacing, block_number)
            .await?;
        let ticks = self
            .initialized_ticks(
                pool_manager,
                pool_state_slot(pool_id),
                core.tick_spacing,
                block_number,
                progress,
                progress_label,
            )
            .await?;

        Ok(PoolSnapshot {
            state: core.state,
            fee: core.fee,
            protocol_fee: core.protocol_fee,
            tick_spacing: core.tick_spacing,
            ticks: PoolTicksSnapshot {
                tick_spacing: core.tick_spacing,
                inner: ticks,
            },
            positions: (),
        })
    }

    pub(crate) async fn pool_checkpoint(
        &self,
        pool_manager: Address,
        pool_id: B256,
        tick_spacing: u32,
        block_number: u64,
        touched_ticks: &BTreeSet<TickIndex>,
    ) -> Result<PoolCheckpoint> {
        let core = self
            .pool_core_snapshot(pool_manager, pool_id, tick_spacing, block_number)
            .await?;
        let ticks_mapping_slot = slot_add(pool_state_slot(pool_id), TICKS_OFFSET);
        let tick_slots = touched_ticks
            .iter()
            .map(|tick| Ok((*tick, mapping_slot_i256(tick.value(), ticks_mapping_slot)?)))
            .collect::<Result<Vec<_>>>()?;
        let loaded_ticks = self
            .tick_infos(pool_manager, &tick_slots, block_number, None)
            .await?;

        Ok(PoolCheckpoint {
            state: core.state,
            fee: core.fee,
            protocol_fee: core.protocol_fee,
            ticks: sparse_checkpoint_ticks(touched_ticks, &loaded_ticks),
        })
    }

    async fn pool_core_snapshot(
        &self,
        pool_manager: Address,
        pool_id: B256,
        tick_spacing: u32,
        block_number: u64,
    ) -> Result<PoolCoreSnapshot> {
        let tick_spacing =
            TickSpacing::new(i16::try_from(tick_spacing)?).context("invalid tick spacing")?;
        let pool_state_slot = pool_state_slot(pool_id);
        let core_slots = [
            pool_state_slot,
            slot_add(pool_state_slot, FEE_GROWTH_GLOBAL0_OFFSET),
            slot_add(pool_state_slot, FEE_GROWTH_GLOBAL1_OFFSET),
            slot_add(pool_state_slot, LIQUIDITY_OFFSET),
        ];
        let values = self
            .storage_slots(pool_manager, &core_slots, block_number, None)
            .await?;
        let [
            slot0,
            fee_growth_global0_x128,
            fee_growth_global1_x128,
            liquidity,
        ] = <[U256; 4]>::try_from(values).map_err(|values| {
            anyhow::anyhow!(
                "extsload returned {} core pool slots, expected 4",
                values.len()
            )
        })?;
        let (sqrt_price_x96, tick, protocol_fee, lp_fee) = parse_slot0(slot0)?;

        Ok(PoolCoreSnapshot {
            state: PoolState {
                sqrt_price_x96,
                tick,
                liquidity: Liquidity::new(liquidity.to::<u128>()),
                fee_growth_global0_x128,
                fee_growth_global1_x128,
            },
            fee: Fee::new(lp_fee).context("invalid lp fee")?,
            protocol_fee,
            tick_spacing,
        })
    }

    async fn initialized_ticks(
        &self,
        pool_manager: Address,
        pool_state_slot: B256,
        tick_spacing: TickSpacing,
        block_number: u64,
        progress: &Progress,
        progress_label: &str,
    ) -> Result<BTreeMap<TickIndex, TickInfoInner>> {
        let bitmap_mapping_slot = slot_add(pool_state_slot, TICK_BITMAP_OFFSET);
        let ticks_mapping_slot = slot_add(pool_state_slot, TICKS_OFFSET);
        let bitmap_slots = initialized_tick_bitmap_slots(tick_spacing, bitmap_mapping_slot)?;
        let (word_positions, bitmap_slots): (Vec<i32>, Vec<B256>) =
            bitmap_slots.into_iter().unzip();
        let bitmap_label = format!("{progress_label} bitmap");
        let bitmaps = self
            .storage_slots(
                pool_manager,
                &bitmap_slots,
                block_number,
                Some((progress, "tick scan", &bitmap_label)),
            )
            .await?;

        let mut tick_slots = Vec::new();
        let mut nonzero_words = 0usize;
        for (word_pos, bitmap) in word_positions.into_iter().zip(bitmaps) {
            if bitmap.is_zero() {
                continue;
            }
            nonzero_words += 1;
            for bit in 0usize..256 {
                if !bitmap.bit(bit) {
                    continue;
                }
                let raw_tick = word_pos
                    .checked_mul(256)
                    .and_then(|base| base.checked_add(bit as i32))
                    .and_then(|compressed| compressed.checked_mul(tick_spacing.as_i32()))
                    .context("tick overflow")?;
                let Some(tick) = TickIndex::new(raw_tick) else {
                    continue;
                };
                tick_slots.push((tick, mapping_slot_i256(raw_tick, ticks_mapping_slot)?));
            }
        }

        progress.log(
            "tick scan",
            format!(
                "{} block={} bitmap done nonzero_words={} candidate_ticks={}",
                progress_label,
                block_number,
                nonzero_words,
                tick_slots.len()
            ),
        );

        let tick_info_label = format!("{progress_label} tick-info");
        let ticks = self
            .tick_infos(
                pool_manager,
                &tick_slots,
                block_number,
                Some((progress, "tick scan", &tick_info_label)),
            )
            .await?
            .into_iter()
            .filter(|(_, tick_info)| tick_info.liquidity_gross.value() != 0)
            .collect::<BTreeMap<_, _>>();

        progress.log(
            "tick scan",
            format!(
                "{} block={} tick-info done initialized_ticks={}",
                progress_label,
                block_number,
                ticks.len()
            ),
        );

        Ok(ticks)
    }

    async fn tick_infos(
        &self,
        pool_manager: Address,
        tick_slots: &[(TickIndex, B256)],
        block_number: u64,
        progress: Option<(&Progress, &str, &str)>,
    ) -> Result<BTreeMap<TickIndex, TickInfoInner>> {
        let slots = tick_slots
            .iter()
            .flat_map(|(_, tick_slot)| tick_info_slots(*tick_slot))
            .collect::<Vec<_>>();
        let values = self
            .storage_slots(pool_manager, &slots, block_number, progress)
            .await?;
        decode_tick_infos(tick_slots, &values)
    }

    async fn storage_slots(
        &self,
        address: Address,
        slots: &[B256],
        block_number: u64,
        progress: Option<(&Progress, &str, &str)>,
    ) -> Result<Vec<U256>> {
        let batches = slots.chunks(STORAGE_BATCH_SIZE);
        let total_batches = batches.len();
        let mut values = Vec::with_capacity(slots.len());

        for (batch_index, chunk) in batches.enumerate() {
            if let Some((progress, phase, label)) = progress {
                progress.log(
                    phase,
                    format!(
                        "{} batch {}/{} block={} slots={}",
                        label,
                        batch_index + 1,
                        total_batches,
                        block_number,
                        chunk.len()
                    ),
                );
            }
            values.extend(self.extsload(address, chunk, block_number).await?);
        }

        Ok(values)
    }

    async fn extsload(
        &self,
        address: Address,
        slots: &[B256],
        block_number: u64,
    ) -> Result<Vec<U256>> {
        if slots.is_empty() {
            return Ok(Vec::new());
        }

        let values = IPoolManager::new(address, self.provider.as_ref())
            .extsload(slots.to_vec())
            .block(block_number.into())
            .call()
            .await?;
        ensure!(
            values.len() == slots.len(),
            "extsload returned {} slots, expected {}",
            values.len(),
            slots.len()
        );

        Ok(values
            .into_iter()
            .map(|value| U256::from_be_bytes(value.0))
            .collect())
    }
}

fn parse_slot0(slot0: U256) -> Result<(SqrtPriceX96, TickIndex, ProtocolFee, u32)> {
    let sqrt = slot0 & ((U256::ONE << 160usize) - U256::ONE);
    let tick_raw = ((slot0 >> 160usize) & U256::from(0xFF_FFFFu32)).to::<u32>();
    let protocol_fee_raw = ((slot0 >> 184usize) & U256::from(0xFF_FFFFu32)).to::<u32>();
    let lp_fee = ((slot0 >> 208usize) & U256::from(0xFF_FFFFu32)).to::<u32>();

    Ok((
        SqrtPriceX96::new(sqrt.to::<U160>()).context("invalid sqrt price in slot0")?,
        TickIndex::new(sign_extend_24(tick_raw)).context("invalid tick in slot0")?,
        ProtocolFee::new(
            (protocol_fee_raw & 0xFFF) as u16,
            (protocol_fee_raw >> 12) as u16,
        )
        .context("invalid protocol fee")?,
        lp_fee,
    ))
}

fn sign_extend_24(value: u32) -> i32 {
    let value = value & 0xFF_FFFF;
    if value & 0x80_0000 != 0 {
        (value as i32) | !0xFF_FFFF
    } else {
        value as i32
    }
}

pub(crate) fn pool_state_slot(pool_id: B256) -> B256 {
    keccak256((pool_id, U256::from(POOLS_SLOT)).abi_encode())
}

fn slot_add(slot: B256, offset: u64) -> B256 {
    B256::from((U256::from_be_bytes(slot.0) + U256::from(offset)).to_be_bytes::<32>())
}

fn mapping_slot_i256(key: i32, mapping_slot: B256) -> Result<B256> {
    Ok(keccak256((I256::try_from(key)?, mapping_slot).abi_encode()))
}

const POOL_EVENT_TOPICS: [B256; 3] = [
    IPoolManager::Swap::SIGNATURE_HASH,
    IPoolManager::ModifyLiquidity::SIGNATURE_HASH,
    IPoolManager::Donate::SIGNATURE_HASH,
];

fn pool_logs_filter(
    pool_manager: Address,
    pool_id: B256,
    event_topic: B256,
    from_block: u64,
    to_block: u64,
) -> Filter {
    Filter::new()
        .address(pool_manager)
        .event_signature(event_topic)
        .topic1(pool_id)
        .from_block(from_block)
        .to_block(to_block)
}

fn sparse_checkpoint_ticks(
    touched_ticks: &BTreeSet<TickIndex>,
    loaded_ticks: &BTreeMap<TickIndex, TickInfoInner>,
) -> BTreeMap<TickIndex, TickInfoInner> {
    touched_ticks
        .iter()
        .map(|tick| {
            let tick_info = loaded_ticks
                .get(tick)
                .copied()
                .filter(|tick_info| tick_info.liquidity_gross.value() != 0)
                .unwrap_or(TickInfoInner::DEFAULT);
            (*tick, tick_info)
        })
        .collect()
}

pub(crate) fn initialized_tick_bitmap_slots(
    tick_spacing: TickSpacing,
    bitmap_mapping_slot: B256,
) -> Result<Vec<(i32, B256)>> {
    let min_word = TickIndex::MIN
        .value()
        .div_euclid(tick_spacing.as_i32())
        .div_euclid(256);
    let max_word = TickIndex::MAX
        .value()
        .div_euclid(tick_spacing.as_i32())
        .div_euclid(256);

    (min_word..=max_word)
        .map(|word_pos| Ok((word_pos, mapping_slot_i256(word_pos, bitmap_mapping_slot)?)))
        .collect()
}

fn tick_info_slots(tick_slot: B256) -> [B256; 3] {
    [tick_slot, slot_add(tick_slot, 1), slot_add(tick_slot, 2)]
}

pub(crate) fn decode_tick_infos(
    tick_slots: &[(TickIndex, B256)],
    values: &[U256],
) -> Result<BTreeMap<TickIndex, TickInfoInner>> {
    ensure!(
        values.len() == tick_slots.len() * 3,
        "expected {} tick info storage values for {} ticks, got {}",
        tick_slots.len() * 3,
        tick_slots.len(),
        values.len()
    );

    tick_slots
        .iter()
        .zip(values.chunks_exact(3))
        .map(|((tick, _), slots)| Ok((*tick, parse_tick_info_slots(slots)?)))
        .collect()
}

fn parse_tick_info_slots(slots: &[U256]) -> Result<TickInfoInner> {
    let [first, fee_growth_outside0_x128, fee_growth_outside1_x128] = slots
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 3 tick info storage values, got {}", slots.len()))?;
    let bytes = first.to_be_bytes::<32>();

    Ok(TickInfoInner {
        liquidity_gross: Liquidity::new(u128::from_be_bytes(bytes[16..32].try_into()?)),
        liquidity_net: i128::from_be_bytes(bytes[0..16].try_into()?),
        fee_growth_outside0_x128,
        fee_growth_outside1_x128,
    })
}

pub(crate) fn bounded_concurrency(requested_concurrency: usize, total: usize) -> usize {
    requested_concurrency.max(1).min(total.max(1))
}

fn block_chunks(from_block: u64, to_block: u64, chunk_size: u64) -> Vec<(u64, u64)> {
    if from_block > to_block {
        return Vec::new();
    }

    let chunk_size = chunk_size.max(1);
    (from_block..=to_block)
        .step_by(chunk_size as usize)
        .map(|start| (start, to_block.min(start.saturating_add(chunk_size - 1))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_extends_int24_values() {
        assert_eq!(sign_extend_24(0), 0);
        assert_eq!(sign_extend_24(0x7f_ffff), 8_388_607);
        assert_eq!(sign_extend_24(0xff_ffff), -1);
        assert_eq!(sign_extend_24(0x80_0000), -8_388_608);
    }

    #[test]
    fn pool_state_slot_matches_solidity_layout_shape() {
        let pool_id = B256::repeat_byte(0x11);
        let slot = pool_state_slot(pool_id);
        assert_ne!(slot, B256::ZERO);
        assert_ne!(slot, pool_id);
    }

    #[test]
    fn block_chunks_cover_the_range_inclusively() {
        assert_eq!(block_chunks(10, 30, 10), [(10, 19), (20, 29), (30, 30)]);
        assert_eq!(block_chunks(10, 10, 1_000), [(10, 10)]);
        assert!(block_chunks(10, 9, 10).is_empty());
    }

    #[test]
    fn rpc_client_trims_and_counts_configured_endpoints() {
        let rpc =
            RpcClient::new(vec![" https://rpc-1.example ".to_string(), String::new()]).unwrap();

        assert_eq!(rpc.endpoint_count(), 1);
        assert!(RpcClient::new(Vec::new()).is_err());
    }

    #[test]
    fn initialized_tick_bitmap_slots_match_word_range() {
        let slots =
            initialized_tick_bitmap_slots(TickSpacing::new(10).unwrap(), B256::repeat_byte(0x44))
                .unwrap();

        assert_eq!(slots.len(), 694);
        assert_eq!(slots.first().map(|(word_pos, _)| *word_pos), Some(-347));
        assert_eq!(slots.last().map(|(word_pos, _)| *word_pos), Some(346));
    }

    #[test]
    fn decode_tick_infos_reads_three_slot_batches() {
        let first = tick(-60);
        let second = tick(60);
        let first_info = tick_info(1_000_000, 1_000_000);
        let second_info = tick_info(2_000_000, -2_000_000);
        let tick_slots = [
            (first, B256::repeat_byte(0x11)),
            (second, B256::repeat_byte(0x22)),
        ];
        let values = [
            tick_info_first_slot(first_info),
            first_info.fee_growth_outside0_x128,
            first_info.fee_growth_outside1_x128,
            tick_info_first_slot(second_info),
            second_info.fee_growth_outside0_x128,
            second_info.fee_growth_outside1_x128,
        ];

        let decoded = decode_tick_infos(&tick_slots, &values).unwrap();

        assert_eq!(decoded.get(&first), Some(&first_info));
        assert_eq!(decoded.get(&second), Some(&second_info));
    }

    #[test]
    fn pool_logs_filter_uses_single_event_topic_and_pool_id() {
        let pool_manager = Address::repeat_byte(0x12);
        let pool_id = B256::repeat_byte(0x34);
        let event_topic = POOL_EVENT_TOPICS[1];

        let filter = pool_logs_filter(pool_manager, pool_id, event_topic, 10, 20);

        assert!(filter.address.matches(&pool_manager));
        assert!(filter.topics[0].matches(&event_topic));
        assert!(filter.topics[1].matches(&pool_id));
    }

    #[test]
    fn sparse_checkpoint_ticks_keep_only_touched_ticks_and_default_deinitialized_ticks() {
        let initialized = tick(-60);
        let deinitialized = tick(60);
        let untouched = tick(120);
        let initialized_info = tick_info(1_000_000, 1_000_000);
        let loaded_ticks = BTreeMap::from([
            (initialized, initialized_info),
            (deinitialized, tick_info(0, -1_000_000)),
            (untouched, tick_info(2_000_000, -2_000_000)),
        ]);

        let ticks =
            sparse_checkpoint_ticks(&BTreeSet::from([initialized, deinitialized]), &loaded_ticks);

        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks.get(&initialized), Some(&initialized_info));
        assert_eq!(ticks.get(&deinitialized), Some(&TickInfoInner::DEFAULT));
        assert!(ticks.get(&untouched).is_none());
    }

    fn tick(value: i32) -> TickIndex {
        TickIndex::new(value).unwrap()
    }

    fn tick_info(liquidity_gross: u128, liquidity_net: i128) -> TickInfoInner {
        TickInfoInner {
            liquidity_gross: Liquidity::new(liquidity_gross),
            liquidity_net,
            fee_growth_outside0_x128: U256::ZERO,
            fee_growth_outside1_x128: U256::ZERO,
        }
    }

    fn tick_info_first_slot(tick_info: TickInfoInner) -> U256 {
        let mut bytes = [0u8; 32];
        bytes[0..16].copy_from_slice(&tick_info.liquidity_net.to_be_bytes());
        bytes[16..32].copy_from_slice(&tick_info.liquidity_gross.value().to_be_bytes());
        U256::from_be_slice(&bytes)
    }
}
