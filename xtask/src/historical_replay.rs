use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    str::FromStr,
    time::{Duration, Instant},
};

use alloy::{
    primitives::{Address, B256, I256},
    rpc::types::Log,
    sol_types::SolCall,
};
use anyhow::{Context, Result, bail};
use futures::{StreamExt, stream};
use rm_uniswap::{
    Fee, ModifyLiquidityParams, Pool, PoolSnapshot, PoolState, ProtocolFee, SqrtPriceX96,
    SwapParams, TickIndex, TickInfoInner, tick_math, v4::PoolKey,
};
use ruint::aliases::U256;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::{
    HistoricalReplayArgs,
    rpc::{CallFrame, IPoolManager, RpcClient, bounded_concurrency},
};

#[derive(Debug, Clone, Serialize)]
struct HistoricalReplayFixture {
    schema_version: &'static str,
    metadata: ReplayMetadata,
    initial_pool: PoolSnapshot,
    transactions: Vec<ReplayTransaction>,
}

#[derive(Debug, Clone, Serialize)]
struct ReplayMetadata {
    chain_id: u64,
    pool_manager: Address,
    pool_key: PoolKeyFixture,
    pool_id: B256,
    start_block: u64,
    end_block: u64,
    tx_count: usize,
    log_count: usize,
    checkpoint_kind: &'static str,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PoolKeyFixture {
    currency0: Address,
    currency1: Address,
    fee: u32,
    tick_spacing: i16,
    hooks: Address,
}

#[derive(Debug, Clone, Serialize)]
struct ReplayTransaction {
    tx_hash: B256,
    block_number: u64,
    log_count: usize,
    operations: Vec<ReplayOperation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_checkpoint: Option<PoolCheckpoint>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PoolCheckpoint {
    pub(crate) state: PoolState,
    pub(crate) fee: Fee,
    pub(crate) protocol_fee: ProtocolFee,
    pub(crate) ticks: BTreeMap<TickIndex, TickInfoInner>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ReplayOperation {
    Swap {
        sender: Address,
        zero_for_one: bool,
        amount_specified: String,
        sqrt_price_limit_x96: String,
    },
    ModifyLiquidity {
        sender: Address,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: String,
        salt: B256,
    },
    Donate {
        sender: Address,
        amount0: String,
        amount1: String,
    },
}

impl PoolCheckpoint {
    #[cfg(test)]
    fn from_snapshot(snapshot: PoolSnapshot, touched_ticks: &BTreeSet<TickIndex>) -> Self {
        let ticks = touched_ticks
            .iter()
            .map(|tick| {
                (
                    *tick,
                    snapshot
                        .ticks
                        .inner
                        .get(tick)
                        .copied()
                        .unwrap_or(TickInfoInner::DEFAULT),
                )
            })
            .collect();

        Self {
            state: snapshot.state,
            fee: snapshot.fee,
            protocol_fee: snapshot.protocol_fee,
            ticks,
        }
    }
}

impl ReplayOperation {
    fn touched_ticks(&self, before: &PoolSnapshot) -> Result<BTreeSet<TickIndex>> {
        match self {
            Self::Swap {
                zero_for_one,
                sqrt_price_limit_x96,
                ..
            } => {
                let limit_tick =
                    tick_math::get_tick_at_sqrt_price(&parse_sqrt_price(sqrt_price_limit_x96)?);
                Ok(before
                    .ticks
                    .inner
                    .keys()
                    .filter(|tick| {
                        tick_in_swap_range(**tick, before.state.tick, limit_tick, *zero_for_one)
                    })
                    .copied()
                    .collect())
            }
            Self::ModifyLiquidity {
                tick_lower,
                tick_upper,
                ..
            } => Ok(BTreeSet::from([
                parse_tick_index(*tick_lower)?,
                parse_tick_index(*tick_upper)?,
            ])),
            Self::Donate { .. } => Ok(BTreeSet::new()),
        }
    }

    fn replay_on(&self, pool: &mut Pool) -> Result<()> {
        match self {
            Self::Swap {
                zero_for_one,
                amount_specified,
                sqrt_price_limit_x96,
                ..
            } => {
                pool.swap(SwapParams {
                    zero_for_one: *zero_for_one,
                    amount_specified: I256::from_str(amount_specified)?,
                    sqrt_price_limit_x96: parse_sqrt_price(sqrt_price_limit_x96)?,
                })?;
                Ok(())
            }
            Self::ModifyLiquidity {
                sender,
                tick_lower,
                tick_upper,
                liquidity_delta,
                salt,
            } => {
                pool.modify_liquidity(ModifyLiquidityParams {
                    tick_lower: parse_tick_index(*tick_lower)?,
                    tick_upper: parse_tick_index(*tick_upper)?,
                    liquidity_delta: liquidity_delta.parse()?,
                    owner: *sender,
                    salt: *salt,
                })?;
                Ok(())
            }
            Self::Donate { .. } => {
                bail!(
                    "sparse historical replay generation does not support donate until Pool::donate is implemented"
                )
            }
        }
    }
}

pub(crate) async fn generate_historical_replay(args: HistoricalReplayArgs) -> Result<()> {
    HistoricalReplayPipeline::new(args)?.run().await
}

struct HistoricalReplayPipeline {
    args: HistoricalReplayArgs,
    progress: Progress,
    rpc: RpcClient,
    pool_key: PoolKey,
    pool_id: B256,
}

struct ScannedTransactions {
    tx_order: Vec<B256>,
    tx_logs: BTreeMap<B256, Vec<Log>>,
    checkpoint_tx_hashes: BTreeSet<B256>,
}

#[derive(Debug, Clone)]
pub(crate) struct Progress {
    pub(crate) started_at: Instant,
}

impl Progress {
    fn start(command: &str) -> Self {
        let progress = Self {
            started_at: Instant::now(),
        };
        progress.log("start", command);
        progress
    }

    pub(crate) fn log(&self, phase: &str, detail: impl std::fmt::Display) {
        eprintln!(
            "{}",
            progress_line(self.started_at.elapsed(), phase, &detail.to_string())
        );
    }
}

impl HistoricalReplayPipeline {
    fn new(args: HistoricalReplayArgs) -> Result<Self> {
        if args.start_block >= args.end_block {
            bail!("start-block must be lower than end-block");
        }

        let progress = Progress::start("historical-replay");
        progress.log("pool key", "resolving");
        let pool_key = resolve_pool_key(&args)?;
        let pool_id = pool_key.to_id().0;
        progress.log(
            "pool key",
            format!(
                "resolved pool_id={pool_id}, fee={}, tick_spacing={}",
                pool_key.fee, pool_key.tick_spacing
            ),
        );

        let rpc = RpcClient::new(args.rpc_urls.clone())?;
        progress.log(
            "rpc",
            format!(
                "configured endpoints={} log_concurrency={} trace_concurrency={} checkpoint_concurrency={}",
                rpc.endpoint_count(),
                args.log_concurrency.max(1),
                args.trace_concurrency.max(1),
                args.checkpoint_concurrency.max(1)
            ),
        );

        Ok(Self {
            args,
            progress,
            rpc,
            pool_key,
            pool_id,
        })
    }

    async fn run(self) -> Result<()> {
        self.progress.log("chain id", "fetching");
        let chain_id = self.rpc.chain_id().await?;
        self.progress
            .log("chain id", format!("resolved {chain_id}"));

        let initial_pool = self.load_initial_pool().await?;
        let scanned = self.scan_transactions().await?;
        let mut traces = self
            .prefetch_traces(&scanned.tx_order, selected_checkpoint_count(&scanned))
            .await?;
        let transactions = self
            .replay_and_checkpoint(initial_pool.clone(), scanned, &mut traces)
            .await?;
        self.write_fixture(chain_id, initial_pool, transactions)?;
        Ok(())
    }

    async fn load_initial_pool(&self) -> Result<PoolSnapshot> {
        self.progress.log(
            "initial snapshot",
            format!("starting block {}", self.args.start_block),
        );
        let initial_pool = self
            .rpc
            .pool_snapshot(
                self.args.pool_manager,
                self.pool_id,
                self.pool_key.tick_spacing,
                self.args.start_block,
                &self.progress,
                "initial snapshot",
            )
            .await?;
        self.progress.log(
            "initial snapshot",
            format!(
                "done block {}, initialized_ticks={}",
                self.args.start_block,
                initial_pool.ticks.inner.len()
            ),
        );
        Ok(initial_pool)
    }

    async fn scan_transactions(&self) -> Result<ScannedTransactions> {
        self.progress.log(
            "log scan",
            format!(
                "starting blocks {}-{} with chunk_size={}",
                self.args.start_block + 1,
                self.args.end_block,
                self.args.block_chunk_size.max(1)
            ),
        );
        let mut logs = self
            .rpc
            .pool_logs(
                self.args.pool_manager,
                self.pool_id,
                self.args.start_block + 1,
                self.args.end_block,
                self.args.block_chunk_size,
                self.args.log_concurrency,
                &self.progress,
            )
            .await?;
        let total_logs = logs.len();
        require_logs_found(total_logs, self.args.start_block + 1, self.args.end_block)?;
        logs.sort_by_key(|log| {
            (
                log.block_number.unwrap_or_default(),
                log.log_index.unwrap_or_default(),
            )
        });

        let mut tx_order = Vec::<B256>::new();
        let mut tx_logs = BTreeMap::<B256, Vec<Log>>::new();
        for log in logs {
            let tx_hash = log
                .transaction_hash
                .context("PoolManager log is missing transaction_hash")?;
            if !tx_logs.contains_key(&tx_hash) {
                tx_order.push(tx_hash);
            }
            tx_logs.entry(tx_hash).or_default().push(log);
        }
        self.progress.log(
            "log scan",
            format!(
                "done total_logs={}, transactions={}",
                total_logs,
                tx_order.len()
            ),
        );

        let checkpoint_tx_hashes = checkpoint_tx_hashes(&tx_order, &tx_logs)?;

        if tx_order.len() > self.args.max_transactions {
            self.progress.log(
                "tx select",
                format!(
                    "truncating transactions from {} to max_transactions={}",
                    tx_order.len(),
                    self.args.max_transactions
                ),
            );
            tx_order.truncate(self.args.max_transactions);
        }

        Ok(ScannedTransactions {
            tx_order,
            tx_logs,
            checkpoint_tx_hashes,
        })
    }

    async fn prefetch_traces(
        &self,
        tx_order: &[B256],
        selected_checkpoints: usize,
    ) -> Result<BTreeMap<B256, CallFrame>> {
        self.progress.log(
            "tx replay",
            format!(
                "starting {} transactions with {} checkpoints, checkpoint_concurrency={}",
                tx_order.len(),
                selected_checkpoints,
                bounded_concurrency(self.args.checkpoint_concurrency, selected_checkpoints)
            ),
        );
        fetch_transaction_traces(
            &self.rpc,
            tx_order,
            self.args.trace_concurrency,
            &self.progress,
        )
        .await
    }

    async fn replay_and_checkpoint(
        &self,
        initial_pool: PoolSnapshot,
        scanned: ScannedTransactions,
        traces: &mut BTreeMap<B256, CallFrame>,
    ) -> Result<Vec<ReplayTransaction>> {
        let total_txs = scanned.tx_order.len();
        let selected_checkpoints = selected_checkpoint_count(&scanned);
        let mut checkpoints = CheckpointQueue::new(
            self.rpc.clone(),
            self.args.pool_manager,
            self.pool_id,
            self.pool_key.tick_spacing,
            self.args.checkpoint_concurrency,
            selected_checkpoints,
            self.progress.clone(),
        );
        let mut tx_logs = scanned.tx_logs;
        let mut current_block = None;
        let mut current_block_touched_ticks = BTreeSet::<TickIndex>::new();
        let mut replay_pool = Pool::try_from(initial_pool)?;
        let mut transactions = Vec::new();

        for (tx_index, tx_hash) in scanned.tx_order.into_iter().enumerate() {
            let logs = tx_logs
                .remove(&tx_hash)
                .expect("transaction hash came from tx_logs");
            let block_number = logs
                .first()
                .context("transaction log group is empty")?
                .block_number
                .context("transaction log is missing block_number")?;
            self.progress.log(
                "tx replay",
                format!(
                    "{}/{} replaying tx={} block={} logs={}",
                    tx_index + 1,
                    total_txs,
                    tx_hash,
                    block_number,
                    logs.len()
                ),
            );
            let trace = traces
                .remove(&tx_hash)
                .expect("transaction trace was prefetched");
            let operations = self.decode_operations(&tx_hash, logs.len(), &trace)?;
            if current_block != Some(block_number) {
                current_block = Some(block_number);
                current_block_touched_ticks.clear();
            }

            for operation in &operations {
                let before = replay_pool.snapshot();
                current_block_touched_ticks.extend(operation.touched_ticks(&before)?);
                operation.replay_on(&mut replay_pool)?;
            }

            let has_checkpoint = scanned.checkpoint_tx_hashes.contains(&tx_hash);
            if has_checkpoint {
                checkpoints
                    .push(
                        tx_index,
                        tx_hash,
                        block_number,
                        current_block_touched_ticks.clone(),
                    )
                    .await?;
            }
            self.progress.log(
                "tx replay",
                format!(
                    "{}/{} done tx={} operations={} checkpoint={}",
                    tx_index + 1,
                    total_txs,
                    tx_hash,
                    operations.len(),
                    if has_checkpoint { "yes" } else { "no" }
                ),
            );
            transactions.push(ReplayTransaction {
                tx_hash,
                block_number,
                log_count: logs.len(),
                operations,
                expected_checkpoint: None,
            });
        }

        checkpoints.finish(&mut transactions).await?;
        Ok(transactions)
    }

    fn decode_operations(
        &self,
        tx_hash: &B256,
        expected_log_count: usize,
        trace: &CallFrame,
    ) -> Result<Vec<ReplayOperation>> {
        let mut operations = Vec::new();
        collect_pool_manager_operations(
            trace,
            self.args.pool_manager,
            &self.pool_key,
            &mut operations,
        )?;
        if operations.is_empty() {
            bail!("no PoolManager operations decoded in trace for {tx_hash}");
        }
        if operations.len() != expected_log_count {
            bail!(
                "trace/log mismatch for {tx_hash}: decoded {} non-reverted PoolManager operations but found {} persisted target events; reverted internal calls, unsupported events, or an incomplete trace may be present",
                operations.len(),
                expected_log_count
            );
        }
        Ok(operations)
    }

    fn fixture(
        &self,
        chain_id: u64,
        initial_pool: PoolSnapshot,
        transactions: Vec<ReplayTransaction>,
    ) -> HistoricalReplayFixture {
        let log_count = transactions.iter().map(|tx| tx.log_count).sum();
        HistoricalReplayFixture {
            schema_version: "historical-replay-v2",
            metadata: ReplayMetadata {
                chain_id,
                pool_manager: self.args.pool_manager,
                pool_key: PoolKeyFixture {
                    currency0: self.pool_key.currency0,
                    currency1: self.pool_key.currency1,
                    fee: self.pool_key.fee,
                    tick_spacing: self.pool_key.tick_spacing as i16,
                    hooks: self.pool_key.hooks,
                },
                pool_id: self.pool_id,
                start_block: self.args.start_block,
                end_block: self.args.end_block,
                tx_count: transactions.len(),
                log_count,
                checkpoint_kind: "sparse_block_end",
                notes: vec![
                    "Generated from PoolManager logs, debug_traceTransaction calldata, and archive storage snapshots.".to_string(),
                    "For blocks with multiple relevant transactions, only the last relevant transaction in that block has a checkpoint because archive storage reads expose block-end state.".to_string(),
                    "Sparse checkpoints compare full pool state and only touched/crossable initialized ticks.".to_string(),
                    "Donate operations are rejected until Pool::donate replay support is implemented.".to_string(),
                ],
            },
            initial_pool,
            transactions,
        }
    }

    fn write_fixture(
        &self,
        chain_id: u64,
        initial_pool: PoolSnapshot,
        transactions: Vec<ReplayTransaction>,
    ) -> Result<()> {
        let fixture = self.fixture(chain_id, initial_pool, transactions);
        self.progress.log("write fixture", "serializing");
        if let Some(parent) = self.args.out.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&fixture)?;
        self.progress.log(
            "write fixture",
            format!("writing {}", self.args.out.display()),
        );
        fs::write(&self.args.out, format!("{json}\n"))?;
        self.progress
            .log("write fixture", format!("done {}", self.args.out.display()));
        println!(
            "wrote {} transactions / {} logs to {}",
            fixture.metadata.tx_count,
            fixture.metadata.log_count,
            self.args.out.display()
        );
        Ok(())
    }
}

#[derive(Debug)]
struct CheckpointFetch {
    tx_index: usize,
    tx_hash: B256,
    block_number: u64,
    snapshot: PoolCheckpoint,
}

struct CheckpointQueue {
    rpc: RpcClient,
    pool_manager: Address,
    pool_id: B256,
    tick_spacing: u32,
    concurrency: usize,
    total: usize,
    progress: Progress,
    tasks: JoinSet<Result<CheckpointFetch>>,
    results: BTreeMap<usize, PoolCheckpoint>,
}

impl CheckpointQueue {
    fn new(
        rpc: RpcClient,
        pool_manager: Address,
        pool_id: B256,
        tick_spacing: u32,
        requested_concurrency: usize,
        total: usize,
        progress: Progress,
    ) -> Self {
        Self {
            rpc,
            pool_manager,
            pool_id,
            tick_spacing,
            concurrency: bounded_concurrency(requested_concurrency, total),
            total,
            progress,
            tasks: JoinSet::new(),
            results: BTreeMap::new(),
        }
    }

    async fn push(
        &mut self,
        tx_index: usize,
        tx_hash: B256,
        block_number: u64,
        touched_ticks: BTreeSet<TickIndex>,
    ) -> Result<()> {
        while self.tasks.len() >= self.concurrency {
            self.collect_next().await?;
        }

        self.progress.log(
            "checkpoint snapshot",
            format!(
                "queued tx={} block={} touched_ticks={} active={}/{}",
                tx_hash,
                block_number,
                touched_ticks.len(),
                self.tasks.len() + 1,
                self.concurrency
            ),
        );

        let rpc = self.rpc.clone();
        let pool_manager = self.pool_manager;
        let pool_id = self.pool_id;
        let tick_spacing = self.tick_spacing;
        self.tasks.spawn(async move {
            let snapshot = rpc
                .pool_checkpoint(
                    pool_manager,
                    pool_id,
                    tick_spacing,
                    block_number,
                    &touched_ticks,
                )
                .await?;
            Ok(CheckpointFetch {
                tx_index,
                tx_hash,
                block_number,
                snapshot,
            })
        });
        Ok(())
    }

    async fn finish(mut self, transactions: &mut [ReplayTransaction]) -> Result<()> {
        while !self.tasks.is_empty() {
            self.collect_next().await?;
        }

        for (tx_index, checkpoint) in self.results {
            let Some(transaction) = transactions.get_mut(tx_index) else {
                bail!("checkpoint result index {tx_index} is out of bounds");
            };
            transaction.expected_checkpoint = Some(checkpoint);
        }
        Ok(())
    }

    async fn collect_next(&mut self) -> Result<()> {
        let joined = self
            .tasks
            .join_next()
            .await
            .context("checkpoint fetch task queue is empty")?;
        let checkpoint = joined.context("checkpoint fetch task failed")??;
        self.progress.log(
            "checkpoint snapshot",
            format!(
                "done tx={} block={} checked_ticks={} completed={}/{}",
                checkpoint.tx_hash,
                checkpoint.block_number,
                checkpoint.snapshot.ticks.len(),
                self.results.len() + 1,
                self.total
            ),
        );
        self.results
            .insert(checkpoint.tx_index, checkpoint.snapshot);
        Ok(())
    }
}

async fn fetch_transaction_traces(
    rpc: &RpcClient,
    tx_order: &[B256],
    requested_concurrency: usize,
    progress: &Progress,
) -> Result<BTreeMap<B256, CallFrame>> {
    let total = tx_order.len();
    if total == 0 {
        return Ok(BTreeMap::new());
    }

    let concurrency = bounded_concurrency(requested_concurrency, total);
    progress.log(
        "trace fetch",
        format!("starting {total} transactions with concurrency={concurrency}"),
    );

    let mut fetches = stream::iter(tx_order.iter().copied().enumerate())
        .map(|(tx_index, tx_hash)| async move {
            let trace = rpc.debug_call_trace(tx_hash).await?;
            anyhow::Ok((tx_index, tx_hash, trace))
        })
        .buffer_unordered(concurrency);

    let mut traces = BTreeMap::new();
    let mut completed = 0usize;
    while let Some(fetched) = fetches.next().await {
        let (tx_index, tx_hash, trace) = fetched?;
        completed += 1;
        traces.insert(tx_hash, trace);
        progress.log(
            "trace fetch",
            format!(
                "{completed}/{total} done tx_index={} tx={}",
                tx_index + 1,
                tx_hash
            ),
        );
    }

    Ok(traces)
}

fn selected_checkpoint_count(scanned: &ScannedTransactions) -> usize {
    scanned
        .tx_order
        .iter()
        .filter(|tx_hash| scanned.checkpoint_tx_hashes.contains(*tx_hash))
        .count()
}

fn require_logs_found(total_logs: usize, from_block: u64, to_block: u64) -> Result<()> {
    if total_logs == 0 {
        bail!(
            "no PoolManager logs found for blocks {from_block}-{to_block}; refusing to write an empty historical replay fixture. start-block is the initial snapshot block, so replay scans from start-block + 1. Check the block range and whether the RPC endpoints support complete historical eth_getLogs results."
        );
    }
    Ok(())
}

fn resolve_pool_key(args: &HistoricalReplayArgs) -> Result<PoolKey> {
    if let Some(raw) = &args.pool_key {
        let parts = raw.split(',').map(str::trim).collect::<Vec<_>>();
        if parts.len() != 5 {
            bail!("--pool-key must be currency0,currency1,fee,tick_spacing,hooks");
        }
        return Ok(PoolKey::new(
            Address::from_str(parts[0])?,
            Address::from_str(parts[1])?,
            parts[2].parse()?,
            parse_positive_i16(parts[3])? as u32,
            Address::from_str(parts[4])?,
        ));
    }

    let tick_spacing = args
        .tick_spacing
        .context("set --tick-spacing or provide --pool-key")?;
    if tick_spacing <= 0 {
        bail!("tick spacing must be positive");
    }

    Ok(PoolKey::new(
        args.currency0
            .context("set --currency0 or provide --pool-key")?,
        args.currency1
            .context("set --currency1 or provide --pool-key")?,
        args.fee.context("set --fee or provide --pool-key")?,
        tick_spacing as u32,
        args.hooks,
    ))
}

fn parse_positive_i16(value: &str) -> Result<i16> {
    let parsed = value.parse::<i16>()?;
    if parsed <= 0 {
        bail!("tick spacing must be positive");
    }
    Ok(parsed)
}

fn checkpoint_tx_hashes(
    tx_order: &[B256],
    tx_logs: &BTreeMap<B256, Vec<Log>>,
) -> Result<BTreeSet<B256>> {
    let mut last_by_block = BTreeMap::<u64, B256>::new();
    for tx_hash in tx_order {
        let block_number = tx_logs
            .get(tx_hash)
            .and_then(|logs| logs.first())
            .context("transaction log group is empty")?
            .block_number
            .context("transaction log is missing block_number")?;
        last_by_block.insert(block_number, *tx_hash);
    }
    Ok(last_by_block.into_values().collect())
}

fn collect_pool_manager_operations(
    frame: &CallFrame,
    pool_manager: Address,
    pool_key: &PoolKey,
    operations: &mut Vec<ReplayOperation>,
) -> Result<()> {
    if frame.reverted() {
        return Ok(());
    }

    if frame.to == Some(pool_manager)
        && let Some(input) = &frame.input
    {
        let data = input.as_ref();
        if data.starts_with(&IPoolManager::swapCall::SELECTOR) {
            let call = IPoolManager::swapCall::abi_decode(data)?;
            if call_key_matches(&call.key, pool_key) {
                operations.push(ReplayOperation::Swap {
                    sender: frame.from.unwrap_or_default(),
                    zero_for_one: call.params.zeroForOne,
                    amount_specified: call.params.amountSpecified.to_string(),
                    sqrt_price_limit_x96: call.params.sqrtPriceLimitX96.to_string(),
                });
            }
        } else if data.starts_with(&IPoolManager::modifyLiquidityCall::SELECTOR) {
            let call = IPoolManager::modifyLiquidityCall::abi_decode(data)?;
            if call_key_matches(&call.key, pool_key) {
                operations.push(ReplayOperation::ModifyLiquidity {
                    sender: frame.from.unwrap_or_default(),
                    tick_lower: signed_to_i32(call.params.tickLower.to_string())?,
                    tick_upper: signed_to_i32(call.params.tickUpper.to_string())?,
                    liquidity_delta: call.params.liquidityDelta.to_string(),
                    salt: call.params.salt,
                });
            }
        } else if data.starts_with(&IPoolManager::donateCall::SELECTOR) {
            let call = IPoolManager::donateCall::abi_decode(data)?;
            if call_key_matches(&call.key, pool_key) {
                operations.push(ReplayOperation::Donate {
                    sender: frame.from.unwrap_or_default(),
                    amount0: call.amount0.to_string(),
                    amount1: call.amount1.to_string(),
                });
            }
        }
    }

    for child in &frame.calls {
        collect_pool_manager_operations(child, pool_manager, pool_key, operations)?;
    }
    Ok(())
}

fn call_key_matches(call_key: &IPoolManager::PoolKey, pool_key: &PoolKey) -> bool {
    call_key.currency0 == pool_key.currency0
        && call_key.currency1 == pool_key.currency1
        && call_key.fee.to::<u32>() == pool_key.fee
        && signed_to_i32(call_key.tickSpacing.to_string()).ok()
            == Some(pool_key.tick_spacing as i32)
        && call_key.hooks == pool_key.hooks
}

fn signed_to_i32(value: String) -> Result<i32> {
    Ok(value.parse::<i32>()?)
}

fn parse_tick_index(value: i32) -> Result<TickIndex> {
    TickIndex::new(value).with_context(|| format!("invalid tick index {value}"))
}

fn parse_sqrt_price(value: &str) -> Result<SqrtPriceX96> {
    let raw = U256::from_str(value)?;
    SqrtPriceX96::from_u256(raw).with_context(|| format!("invalid sqrt price {value}"))
}

fn tick_in_swap_range(
    tick: TickIndex,
    current_tick: TickIndex,
    limit_tick: TickIndex,
    zero_for_one: bool,
) -> bool {
    if zero_for_one {
        limit_tick <= tick && tick <= current_tick
    } else {
        current_tick <= tick && tick <= limit_tick
    }
}

fn progress_line(elapsed: Duration, phase: &str, detail: &str) -> String {
    format!("[{}] {phase}: {detail}", format_elapsed(elapsed))
}

fn format_elapsed(elapsed: Duration) -> String {
    let total_seconds = elapsed.as_secs();
    let millis = elapsed.subsec_millis();
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
    } else {
        format!("{minutes:02}:{seconds:02}.{millis:03}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Log as PrimitiveLog, LogData};
    use rm_uniswap::{Liquidity, PoolTicksSnapshot, TickSpacing};

    #[test]
    fn reverted_call_frames_skip_their_subtree() {
        let pool_manager = Address::repeat_byte(0x22);
        let pool_key = PoolKey::new(
            Address::repeat_byte(0x01),
            Address::repeat_byte(0x02),
            500,
            10,
            Address::ZERO,
        );
        let bad_swap_input = format!("0x{}00", hex_bytes(&IPoolManager::swapCall::SELECTOR));
        let trace = CallFrame {
            from: None,
            to: None,
            input: None,
            error: Some("execution reverted".to_string()),
            revert_reason: None,
            calls: vec![CallFrame {
                from: Some(Address::repeat_byte(0x33)),
                to: Some(pool_manager),
                input: Some(bad_swap_input.parse().unwrap()),
                error: None,
                revert_reason: None,
                calls: Vec::new(),
            }],
        };
        let mut operations = Vec::new();

        collect_pool_manager_operations(&trace, pool_manager, &pool_key, &mut operations)
            .expect("reverted subtree should not decode invalid calldata");

        assert!(operations.is_empty());
    }

    #[test]
    fn checkpoint_hashes_keep_only_last_relevant_tx_per_block() {
        let first = B256::repeat_byte(0x01);
        let second = B256::repeat_byte(0x02);
        let third = B256::repeat_byte(0x03);
        let logs = BTreeMap::from([
            (first, vec![rpc_log(100, first, 1)]),
            (second, vec![rpc_log(100, second, 2)]),
            (third, vec![rpc_log(101, third, 3)]),
        ]);

        let checkpoints = checkpoint_tx_hashes(&[first, second, third], &logs).unwrap();

        assert_eq!(checkpoints, BTreeSet::from([second, third]));
    }

    #[test]
    fn sparse_checkpoint_serializes_only_touched_ticks() {
        let snapshot = test_pool_snapshot(BTreeMap::from([
            (tick(-60), tick_info(1_000_000, 1_000_000)),
            (tick(60), tick_info(1_000_000, -1_000_000)),
            (tick(120), tick_info(2_000_000, -2_000_000)),
        ]));

        let checkpoint = PoolCheckpoint::from_snapshot(
            snapshot,
            &BTreeSet::from([tick(-60), tick(120), tick(180)]),
        );
        let value = serde_json::to_value(&checkpoint).unwrap();

        assert!(value["ticks"].get("-60").is_some());
        assert!(value["ticks"].get("60").is_none());
        assert!(value["ticks"].get("120").is_some());
        assert_eq!(value["ticks"]["180"]["liquidity_gross"], 0);
    }

    #[test]
    fn modify_liquidity_operation_touches_its_boundary_ticks() {
        let operation = ReplayOperation::ModifyLiquidity {
            sender: Address::repeat_byte(0x11),
            tick_lower: -60,
            tick_upper: 60,
            liquidity_delta: "1000000".to_string(),
            salt: B256::ZERO,
        };
        let snapshot = test_pool_snapshot(BTreeMap::new());

        let touched = operation.touched_ticks(&snapshot).unwrap();

        assert_eq!(touched, BTreeSet::from([tick(-60), tick(60)]));
    }

    #[test]
    fn swap_operation_touches_initialized_ticks_in_price_limit_range() {
        let operation = ReplayOperation::Swap {
            sender: Address::repeat_byte(0x11),
            zero_for_one: true,
            amount_specified: "-1000000".to_string(),
            sqrt_price_limit_x96: tick(-120).sqrt_price_x96().as_u256().to_string(),
        };
        let snapshot = test_pool_snapshot(BTreeMap::from([
            (tick(-180), tick_info(1_000_000, 1_000_000)),
            (tick(-60), tick_info(1_000_000, 1_000_000)),
            (tick(60), tick_info(1_000_000, -1_000_000)),
        ]));

        let touched = operation.touched_ticks(&snapshot).unwrap();

        assert_eq!(touched, BTreeSet::from([tick(-60)]));
    }

    #[test]
    fn progress_line_formats_elapsed_time_phase_and_detail() {
        assert_eq!(
            progress_line(
                Duration::from_millis(65_432),
                "log scan",
                "chunk 2/3 blocks 10-19"
            ),
            "[01:05.432] log scan: chunk 2/3 blocks 10-19"
        );
        assert_eq!(
            progress_line(Duration::from_millis(3_661_007), "tx replay", "1/2 tracing"),
            "[01:01:01.007] tx replay: 1/2 tracing"
        );
    }

    #[test]
    fn empty_log_scan_is_an_error() {
        let error = require_logs_found(0, 100, 101).unwrap_err().to_string();

        assert!(error.contains("no PoolManager logs found"));
        assert!(error.contains("100-101"));
        assert!(error.contains("start-block + 1"));
    }

    #[test]
    fn non_empty_log_scan_is_ok() {
        require_logs_found(1, 100, 101).unwrap();
    }

    fn rpc_log(block_number: u64, transaction_hash: B256, log_index: u64) -> Log {
        Log {
            inner: PrimitiveLog {
                address: Address::ZERO,
                data: LogData::default(),
            },
            block_hash: None,
            block_number: Some(block_number),
            block_timestamp: None,
            transaction_hash: Some(transaction_hash),
            transaction_index: None,
            log_index: Some(log_index),
            removed: false,
        }
    }

    fn hex_bytes(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn test_pool_snapshot(ticks: BTreeMap<TickIndex, TickInfoInner>) -> PoolSnapshot {
        let tick_spacing = TickSpacing::new(60).unwrap();
        PoolSnapshot {
            state: PoolState {
                sqrt_price_x96: tick(0).sqrt_price_x96(),
                tick: tick(0),
                liquidity: Liquidity::ZERO,
                fee_growth_global0_x128: U256::ZERO,
                fee_growth_global1_x128: U256::ZERO,
            },
            fee: Fee::new(3000).unwrap(),
            protocol_fee: ProtocolFee::ZERO,
            tick_spacing,
            ticks: PoolTicksSnapshot {
                tick_spacing,
                inner: ticks,
            },
            positions: (),
        }
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
}
