.PHONY: help fmt check test test-full test-positions test-forge-parity forge-deps forge-build forge-parity-json doc clippy bench clean

FEATURES_FULL := full-v4
FEATURES_POSITIONS := positions
FORGE_PARITY_SCENARIO ?= all
FORGE_PARITY_OUT ?= fixtures/parity.json

help:
	@printf '%s\n' \
		'Targets:' \
		'  make fmt                Format Rust sources' \
		'  make check              cargo check with full-v4 features' \
		'  make test               Run default cargo tests' \
		'  make test-full          Run all tests with full-v4 features' \
		'  make test-positions     Run Forge parity with positions feature' \
		'  make test-forge-parity  Run committed Forge parity regression' \
		'  make forge-deps         Install Forge script dependencies' \
		'  make forge-build        Compile Forge scripts' \
		'  make forge-parity-json  Regenerate forge/fixtures/parity.json' \
		'  make doc                Build crate docs with full-v4 features' \
		'  make clippy             Run clippy with full-v4 features' \
		'  make bench              Run Criterion benchmarks' \
		'  make clean              Remove Cargo build artifacts'

fmt:
	cargo fmt

check:
	cargo check --features $(FEATURES_FULL)

test:
	cargo test

test-full:
	cargo test --features $(FEATURES_FULL)

test-positions:
	cargo test --features $(FEATURES_POSITIONS) --test forge_parity

test-forge-parity:
	cargo test --features $(FEATURES_FULL) --test forge_parity

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

doc:
	cargo doc --no-deps --features $(FEATURES_FULL)

clippy:
	cargo clippy --features $(FEATURES_FULL) --all-targets -- -D warnings

bench:
	cargo bench --package rm-uniswap --all-features

clean:
	cargo clean
