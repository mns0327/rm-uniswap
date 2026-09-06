mod historical_replay;
mod rpc;

use std::path::PathBuf;

use alloy::primitives::Address;
use anyhow::Result;
use clap::{Parser, Subcommand};

const DEFAULT_LOG_CONCURRENCY: usize = 4;
const DEFAULT_TRACE_CONCURRENCY: usize = 4;
const DEFAULT_CHECKPOINT_CONCURRENCY: usize = 4;

#[derive(Debug, Parser)]
#[command(version, about = "Repository maintenance tasks")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Generate a trace-backed Uniswap v4 historical replay fixture.
    HistoricalReplay(HistoricalReplayArgs),
}

#[derive(Debug, Parser)]
pub(crate) struct HistoricalReplayArgs {
    /// Archive RPC URL. Must support eth_getLogs, historical eth_call, and debug_traceTransaction.
    ///
    /// Pass multiple --rpc-url values or set ETH_RPC_URL to a comma-separated list.
    #[arg(
        long = "rpc-url",
        env = "ETH_RPC_URL",
        value_delimiter = ',',
        required = true
    )]
    pub(crate) rpc_urls: Vec<String>,
    #[arg(long, env = "POOL_MANAGER")]
    pub(crate) pool_manager: Address,
    /// Pool key as currency0,currency1,fee,tick_spacing,hooks.
    #[arg(long)]
    pub(crate) pool_key: Option<String>,
    #[arg(long, env = "CURRENCY0")]
    pub(crate) currency0: Option<Address>,
    #[arg(long, env = "CURRENCY1")]
    pub(crate) currency1: Option<Address>,
    #[arg(long, env = "FEE")]
    pub(crate) fee: Option<u32>,
    #[arg(long, env = "TICK_SPACING")]
    pub(crate) tick_spacing: Option<i16>,
    #[arg(
        long,
        env = "HOOKS",
        default_value = "0x0000000000000000000000000000000000000000"
    )]
    pub(crate) hooks: Address,
    /// Initial snapshot block. Logs are replayed from start-block + 1.
    #[arg(long)]
    pub(crate) start_block: u64,
    /// Last block included in the replay.
    #[arg(long)]
    pub(crate) end_block: u64,
    #[arg(long, default_value = "forge/fixtures/historical_replay.json")]
    pub(crate) out: PathBuf,
    /// Upper bound for relevant transactions in the generated fixture.
    #[arg(long, default_value_t = 250)]
    pub(crate) max_transactions: usize,
    /// eth_getLogs chunk size.
    #[arg(long, default_value_t = 1_000)]
    pub(crate) block_chunk_size: u64,
    /// Number of eth_getLogs block chunks to fetch in parallel.
    #[arg(long, env = "LOG_CONCURRENCY", default_value_t = DEFAULT_LOG_CONCURRENCY)]
    pub(crate) log_concurrency: usize,
    /// Number of transactions to prefetch with debug_traceTransaction in parallel.
    #[arg(long, env = "TRACE_CONCURRENCY", default_value_t = DEFAULT_TRACE_CONCURRENCY)]
    pub(crate) trace_concurrency: usize,
    /// Number of sparse checkpoint snapshots to fetch in parallel.
    #[arg(
        long,
        env = "CHECKPOINT_CONCURRENCY",
        default_value_t = DEFAULT_CHECKPOINT_CONCURRENCY
    )]
    pub(crate) checkpoint_concurrency: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::HistoricalReplay(args) => {
            historical_replay::generate_historical_replay(args).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_replay_args_accept_multiple_rpc_urls_and_concurrency() {
        let cli = Cli::try_parse_from([
            "xtask",
            "historical-replay",
            "--rpc-url",
            "https://rpc-1.example,https://rpc-2.example",
            "--rpc-url",
            "https://rpc-3.example",
            "--pool-manager",
            "0x000000000004444c5dc75cB358380D2e3dE08A90",
            "--currency0",
            "0x0000000000000000000000000000000000000001",
            "--currency1",
            "0x0000000000000000000000000000000000000002",
            "--fee",
            "3000",
            "--tick-spacing",
            "60",
            "--start-block",
            "10",
            "--end-block",
            "20",
            "--log-concurrency",
            "6",
            "--trace-concurrency",
            "8",
            "--checkpoint-concurrency",
            "5",
        ])
        .unwrap();

        let Commands::HistoricalReplay(args) = cli.command;
        assert_eq!(
            args.rpc_urls,
            [
                "https://rpc-1.example",
                "https://rpc-2.example",
                "https://rpc-3.example"
            ]
        );
        assert_eq!(args.log_concurrency, 6);
        assert_eq!(args.trace_concurrency, 8);
        assert_eq!(args.checkpoint_concurrency, 5);
    }
}
