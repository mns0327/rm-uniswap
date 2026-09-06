.PHONY: help fmt check test test-full test-positions test-forge-parity test-historical-replay forge-deps forge-build forge-parity-json historical-replay-json doc clippy bench clean

FORGE_PARITY_SCENARIO ?= all
FORGE_PARITY_OUT ?= fixtures/parity.json
HISTORICAL_REPLAY_OUT ?= forge/fixtures/historical_replay.json
HISTORICAL_REPLAY_MAX_TRANSACTIONS ?= 250
HISTORICAL_REPLAY_BLOCK_CHUNK_SIZE ?= 1000
HISTORICAL_REPLAY_LOG_CONCURRENCY ?= 4
HISTORICAL_REPLAY_TRACE_CONCURRENCY ?= 10
HISTORICAL_REPLAY_CHECKPOINT_CONCURRENCY ?= 4

help:
	@printf '%s\n' \
		'Targets:' \
		'  make fmt                Format Rust sources' \
		'  make check              Run cargo check' \
		'  make test               Run default cargo tests' \
		'  make test-full          Run all cargo tests' \
		'  make test-positions     Run positions integration tests' \
		'  make test-forge-parity  Run committed Forge parity regression' \
		'  make test-historical-replay Run committed historical replay regression' \
		'  make forge-deps         Install Forge script dependencies' \
		'  make forge-build        Compile Forge scripts' \
		'  make forge-parity-json  Regenerate forge/fixtures/parity.json' \
		'  make historical-replay-json Regenerate trace-backed historical replay fixture' \
		'  make doc                Build crate docs' \
		'  make clippy             Run clippy' \
		'  make bench              Run Criterion benchmarks' \
		'  make clean              Remove Cargo build artifacts'

fmt:
	cargo fmt

check:
	cargo check

test:
	cargo test

test-full:
	cargo test

test-positions:
	cargo test --test positions

test-forge-parity:
	cargo test --test forge_parity

test-historical-replay:
	cargo test --test historical_replay

forge-deps:
	@if [ -f forge/lib/forge-std/src/Script.sol ]; then \
		exit 0; \
	fi; \
	if [ -e forge/lib/forge-std ]; then \
		echo "forge/lib/forge-std exists but Script.sol is missing; remove it and rerun make forge-deps"; \
		exit 1; \
	fi; \
	cd forge && forge install foundry-rs/forge-std --no-git --shallow

forge-build: forge-deps
	cd forge && forge build

forge-parity-json: forge-deps
	cd forge && FORGE_PARITY_SCENARIO=$(FORGE_PARITY_SCENARIO) FORGE_PARITY_OUT=$(FORGE_PARITY_OUT) forge script script/ForgeParityFixture.s.sol:ForgeParityFixture -q

historical-replay-json:
	cargo run -p xtask -- historical-replay --rpc-url "$$ETH_RPC_URL" --pool-manager "$$POOL_MANAGER" --currency0 "$$CURRENCY0" --currency1 "$$CURRENCY1" --fee "$$FEE" --tick-spacing "$$TICK_SPACING" --hooks "$$HOOKS" --start-block "$$START_BLOCK" --end-block "$$END_BLOCK" --out "$(HISTORICAL_REPLAY_OUT)" --max-transactions "$(HISTORICAL_REPLAY_MAX_TRANSACTIONS)" --block-chunk-size "$(HISTORICAL_REPLAY_BLOCK_CHUNK_SIZE)" --log-concurrency "$(HISTORICAL_REPLAY_LOG_CONCURRENCY)" --trace-concurrency "$(HISTORICAL_REPLAY_TRACE_CONCURRENCY)" --checkpoint-concurrency "$(HISTORICAL_REPLAY_CHECKPOINT_CONCURRENCY)"

doc:
	cargo doc --no-deps

clippy:
	cargo clippy --all-targets -- -D warnings

bench:
	cargo bench --package rm-uniswap

clean:
	cargo clean
