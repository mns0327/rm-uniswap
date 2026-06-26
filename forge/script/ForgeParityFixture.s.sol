// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// Minimal Foundry cheatcode interface.
///
/// We define the small subset used by this script locally so the parity harness
/// does not require `forge-std` or vendored Solidity dependencies.
interface Vm {
    function envAddress(string calldata name) external view returns (address);
    function envOr(string calldata name, address defaultValue) external view returns (address);
    function envUint(string calldata name) external view returns (uint256);
    function envOr(string calldata name, uint256 defaultValue) external view returns (uint256);
    function envInt(string calldata name) external view returns (int256);
    function envBool(string calldata name) external view returns (bool);
    function envOr(string calldata name, bool defaultValue) external view returns (bool);
    function envString(string calldata name) external view returns (string memory);
    function envOr(string calldata name, string calldata defaultValue) external view returns (string memory);
    function createSelectFork(string calldata urlOrAlias) external returns (uint256 forkId);
    function createSelectFork(string calldata urlOrAlias, uint256 blockNumber) external returns (uint256 forkId);
    function startPrank(address msgSender) external;
    function stopPrank() external;
    function deal(address account, uint256 newBalance) external;
    function toString(address value) external pure returns (string memory);
    function toString(uint256 value) external pure returns (string memory);
    function toString(int256 value) external pure returns (string memory);
    function toString(bytes32 value) external pure returns (string memory);
    function store(address target, bytes32 slot, bytes32 value) external;
    function writeFile(string calldata path, string calldata data) external;
}

interface IERC20Minimal {
    function balanceOf(address owner) external view returns (uint256);
    function transfer(address to, uint256 value) external returns (bool);
}

interface IPoolManagerLike {
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

    function unlock(bytes calldata data) external returns (bytes memory result);
    function modifyLiquidity(PoolKey calldata key, ModifyLiquidityParams calldata params, bytes calldata hookData)
        external
        returns (int256 callerDelta, int256 feesAccrued);
    function swap(PoolKey calldata key, SwapParams calldata params, bytes calldata hookData)
        external
        returns (int256 swapDelta);
    function sync(address currency) external;
    function settle() external payable returns (uint256 paid);
    function take(address currency, address to, uint256 amount) external;
    function extsload(bytes32 slot) external view returns (bytes32 value);
    function extsload(bytes32 startSlot, uint256 nSlots) external view returns (bytes32[] memory values);
}

contract ForgeParityFixture {
    Vm internal constant VM = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    function run() external {
        string memory scenarioList = VM.envOr("FORGE_PARITY_SCENARIO", string("all"));
        string memory out = "[";
        bool wrote = false;

        (out, wrote) = appendScenario(out, wrote, scenarioList, "no-cross");
        (out, wrote) = appendScenario(out, wrote, scenarioList, "cross-one-tick");
        (out, wrote) = appendScenario(out, wrote, scenarioList, "exact-output");
        (out, wrote) = appendScenario(out, wrote, scenarioList, "protocol-fee");
        (out, wrote) = appendScenario(out, wrote, scenarioList, "one-for-zero-cross");

        require(wrote, "no matching scenario");
        VM.writeFile(outputPath(), string(abi.encodePacked(out, "]")));
    }

    function appendScenario(string memory out, bool wrote, string memory scenarioList, string memory scenario)
        internal
        returns (string memory, bool)
    {
        if (!shouldRunScenario(scenarioList, scenario)) return (out, wrote);

        createConfiguredFork();
        ForgeParityFixtureHarness.Config memory c = readBaseConfig();
        applyScenarioPreset(c, scenario);
        ForgeParityFixtureHarness harness = new ForgeParityFixtureHarness();
        string memory fixture = harness.run(c);
        return (string(abi.encodePacked(out, wrote ? "," : "", fixture)), true);
    }

    function createConfiguredFork() internal returns (uint256 forkId) {
        string memory rpcUrl = VM.envString("ETH_RPC_URL");
        uint256 forkBlock = VM.envOr("FORK_BLOCK_NUMBER", uint256(0));
        if (forkBlock == 0) {
            forkBlock = VM.envOr("FOUNDRY_FORK_BLOCK_NUMBER", uint256(0));
        }

        if (forkBlock == 0) {
            forkId = VM.createSelectFork(rpcUrl);
        } else {
            forkId = VM.createSelectFork(rpcUrl, forkBlock);
        }
    }

    function readBaseConfig() internal view returns (ForgeParityFixtureHarness.Config memory c) {
        c.poolManager = VM.envAddress("POOL_MANAGER");
        c.currency0 = VM.envAddress("CURRENCY0");
        c.currency1 = VM.envAddress("CURRENCY1");
        c.fee = uint24(VM.envUint("FEE"));
        c.tickSpacing = int24(int256(VM.envUint("TICK_SPACING")));
        c.hooks = VM.envOr("HOOKS", address(0));
        c.owner = VM.envOr("OWNER", address(0));
        c.token0Whale = VM.envOr("TOKEN0_WHALE", address(0));
        c.token1Whale = VM.envOr("TOKEN1_WHALE", address(0));
        c.liquidityDelta = int128(int256(VM.envOr("LIQUIDITY_DELTA", uint256(1_000_000))));
        c.fund0 = VM.envOr("FUND0", uint256(0));
        c.fund1 = VM.envOr("FUND1", uint256(0));
    }

    function applyScenarioPreset(ForgeParityFixtureHarness.Config memory c, string memory scenario) internal view {
        c.scenario = scenario;

        if (eq(scenario, "no-cross")) {
            c.fixtureName = "forge-no-cross";
            c.tickLowerOffset = 120;
            c.tickUpperOffset = 120;
            c.zeroForOne = true;
            c.exactOutput = false;
            c.swapAmount = 1_000_000;
            c.protocolFee = 0;
        } else if (eq(scenario, "cross-one-tick")) {
            c.fixtureName = "forge-cross-one-tick";
            c.tickLowerOffset = 1;
            c.tickUpperOffset = 120;
            c.zeroForOne = true;
            c.exactOutput = false;
            c.swapAmount = 450_000_000_000_000_000;
            c.protocolFee = 0;
        } else if (eq(scenario, "exact-output")) {
            c.fixtureName = "forge-exact-output";
            c.tickLowerOffset = 120;
            c.tickUpperOffset = 120;
            c.zeroForOne = true;
            c.exactOutput = true;
            c.swapAmount = 1_000_000;
            c.protocolFee = 0;
        } else if (eq(scenario, "protocol-fee")) {
            c.fixtureName = "forge-protocol-fee";
            c.tickLowerOffset = 120;
            c.tickUpperOffset = 120;
            c.zeroForOne = true;
            c.exactOutput = false;
            c.swapAmount = 100_000_000_000_000_000;
            c.protocolFee = 500;
        } else if (eq(scenario, "one-for-zero-cross")) {
            c.fixtureName = "forge-one-for-zero-cross";
            c.tickLowerOffset = 120;
            c.tickUpperOffset = 1;
            c.zeroForOne = false;
            c.exactOutput = false;
            c.swapAmount = 300_000_000;
            c.protocolFee = 0;
        } else {
            revert("unknown scenario");
        }

        applyScenarioOverrides(c);
    }

    function applyScenarioOverrides(ForgeParityFixtureHarness.Config memory c) internal view {
        uint256 tickLowerOffset = VM.envOr("TICK_LOWER_OFFSET", uint256(0));
        if (tickLowerOffset != 0) c.tickLowerOffset = uint24(tickLowerOffset);

        uint256 tickUpperOffset = VM.envOr("TICK_UPPER_OFFSET", uint256(0));
        if (tickUpperOffset != 0) c.tickUpperOffset = uint24(tickUpperOffset);

        uint256 swapAmount = VM.envOr("SWAP_AMOUNT", uint256(0));
        if (swapAmount != 0) c.swapAmount = swapAmount;

        uint256 protocolFee = VM.envOr("PROTOCOL_FEE", uint256(0));
        if (protocolFee != 0) c.protocolFee = uint24(protocolFee);

        if (VM.envOr("SWAP_EXACT_OUTPUT", false)) c.exactOutput = true;
    }

    function outputPath() internal view returns (string memory) {
        return VM.envOr("FORGE_PARITY_OUT", string("fixtures/parity.json"));
    }

    function shouldRunScenario(string memory scenarioList, string memory scenario) internal pure returns (bool) {
        if (eq(scenarioList, "all")) return true;
        bytes memory list = bytes(scenarioList);
        bytes memory item = bytes(scenario);
        uint256 start = 0;

        for (uint256 i = 0; i <= list.length; i++) {
            if (i == list.length || list[i] == bytes1(",")) {
                if (rangeEq(list, start, i, item)) return true;
                start = i + 1;
            }
        }

        return false;
    }

    function rangeEq(bytes memory list, uint256 start, uint256 end, bytes memory item) internal pure returns (bool) {
        if (end < start || end - start != item.length) return false;
        for (uint256 i = 0; i < item.length; i++) {
            if (list[start + i] != item[i]) return false;
        }
        return true;
    }

    function eq(string memory a, string memory b) internal pure returns (bool) {
        return keccak256(bytes(a)) == keccak256(bytes(b));
    }
}

contract ForgeParityFixtureHarness {
    Vm internal constant VM = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    uint160 internal constant MIN_SQRT_PRICE = 4_295_128_739;
    uint160 internal constant MAX_SQRT_PRICE = 1_461_446_703_485_210_103_287_273_052_203_988_822_378_723_970_342;
    bytes32 internal constant POOLS_SLOT = bytes32(uint256(6));
    uint256 internal constant FEE_GROWTH_GLOBAL0_OFFSET = 1;
    uint256 internal constant LIQUIDITY_OFFSET = 3;
    uint256 internal constant TICKS_OFFSET = 4;

    struct Config {
        address poolManager;
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
        address owner;
        address token0Whale;
        address token1Whale;
        int24 tickLower;
        int24 tickUpper;
        uint24 tickLowerOffset;
        uint24 tickUpperOffset;
        int128 liquidityDelta;
        bool zeroForOne;
        bool exactOutput;
        uint256 swapAmount;
        uint24 protocolFee;
        uint256 fund0;
        uint256 fund1;
        string fixtureName;
        string scenario;
    }

    struct StepResult {
        int256 principalDelta;
        int256 feeDelta;
        int256 swapDelta;
        uint160 sqrtPriceX96;
        int24 tick;
        uint128 liquidity;
        uint256 feeGrowthGlobal0X128;
        uint256 feeGrowthGlobal1X128;
    }

    Config internal cfg;
    IPoolManagerLike.PoolKey internal key;
    bytes32 internal poolId;
    StepResult internal mintResult;
    StepResult internal swapResult;
    StepResult internal collectResult;
    StepResult internal decreaseResult;

    function run(Config memory input) external returns (string memory) {
        cfg = input;
        validateConfiguredContracts();
        key = IPoolManagerLike.PoolKey({
            currency0: cfg.currency0,
            currency1: cfg.currency1,
            fee: cfg.fee,
            tickSpacing: cfg.tickSpacing,
            hooks: cfg.hooks
        });
        poolId = keccak256(abi.encode(key));
        applyProtocolFeeOverride();
        resolveTickRangeFromCurrentTick();

        fundHarness(cfg.currency0, cfg.token0Whale, cfg.fund0);
        fundHarness(cfg.currency1, cfg.token1Whale, cfg.fund1);

        string memory initialPool = poolSnapshotJson();

        IPoolManagerLike(cfg.poolManager).unlock(abi.encode(uint8(1)));
        IPoolManagerLike(cfg.poolManager).unlock(abi.encode(uint8(2)));
        IPoolManagerLike(cfg.poolManager).unlock(abi.encode(uint8(3)));
        IPoolManagerLike(cfg.poolManager).unlock(abi.encode(uint8(4)));

        string memory finalPool = poolSnapshotJson();
        return fixtureJson(initialPool, finalPool);
    }

    function validateConfiguredContracts() internal view {
        require(cfg.poolManager.code.length != 0, "POOL_MANAGER has no code at fork block");
        if (cfg.currency0 != address(0)) {
            require(cfg.currency0.code.length != 0, "CURRENCY0 has no code at fork block");
        }
        if (cfg.currency1 != address(0)) {
            require(cfg.currency1.code.length != 0, "CURRENCY1 has no code at fork block");
        }
    }

    function applyProtocolFeeOverride() internal {
        if (cfg.protocolFee == 0) return;
        require(cfg.protocolFee <= 1_000, "protocol fee too large");

        bytes32 slot = poolStateSlot();
        uint256 slot0 = uint256(IPoolManagerLike(cfg.poolManager).extsload(slot));
        uint256 clearProtocolFeeMask = ~(uint256(0xFFFFFF) << 184);
        uint256 packedProtocolFee = cfg.zeroForOne ? uint256(cfg.protocolFee) : uint256(cfg.protocolFee) << 12;
        uint256 updated = (slot0 & clearProtocolFeeMask) | (packedProtocolFee << 184);
        VM.store(cfg.poolManager, slot, bytes32(updated));
    }

    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        require(msg.sender == cfg.poolManager, "only PoolManager");
        uint8 action = abi.decode(data, (uint8));

        if (action == 1) {
            (int256 callerDelta, int256 fees) = IPoolManagerLike(cfg.poolManager)
                .modifyLiquidity(
                    key,
                    IPoolManagerLike.ModifyLiquidityParams({
                        tickLower: cfg.tickLower,
                        tickUpper: cfg.tickUpper,
                        liquidityDelta: cfg.liquidityDelta,
                        salt: bytes32(uint256(1))
                    }),
                    bytes("")
                );
            settleDelta(callerDelta);
            fillPositionStep(mintResult, principalDelta(callerDelta, fees), fees);
        } else if (action == 2) {
            uint160 limit = cfg.zeroForOne ? MIN_SQRT_PRICE + 1 : MAX_SQRT_PRICE - 1;
            int256 amountSpecified = cfg.exactOutput ? int256(cfg.swapAmount) : -int256(cfg.swapAmount);
            int256 swapDelta = IPoolManagerLike(cfg.poolManager)
                .swap(
                    key,
                    IPoolManagerLike.SwapParams({
                        zeroForOne: cfg.zeroForOne, amountSpecified: amountSpecified, sqrtPriceLimitX96: limit
                    }),
                    bytes("")
                );
            settleDelta(swapDelta);
            fillSwapStep(swapResult, swapDelta);
        } else if (action == 3) {
            (int256 callerDelta, int256 fees) = IPoolManagerLike(cfg.poolManager)
                .modifyLiquidity(
                    key,
                    IPoolManagerLike.ModifyLiquidityParams({
                        tickLower: cfg.tickLower, tickUpper: cfg.tickUpper, liquidityDelta: 0, salt: bytes32(uint256(1))
                    }),
                    bytes("")
                );
            settleDelta(callerDelta);
            fillPositionStep(collectResult, principalDelta(callerDelta, fees), fees);
        } else if (action == 4) {
            (int256 callerDelta, int256 fees) = IPoolManagerLike(cfg.poolManager)
                .modifyLiquidity(
                    key,
                    IPoolManagerLike.ModifyLiquidityParams({
                        tickLower: cfg.tickLower,
                        tickUpper: cfg.tickUpper,
                        liquidityDelta: -int256(cfg.liquidityDelta),
                        salt: bytes32(uint256(1))
                    }),
                    bytes("")
                );
            settleDelta(callerDelta);
            fillPositionStep(decreaseResult, principalDelta(callerDelta, fees), fees);
        } else {
            revert("unknown action");
        }

        return bytes("");
    }

    function resolveTickRangeFromCurrentTick() internal {
        (, int24 currentTick,,) = getSlot0();
        int24 lowerRaw = currentTick - int24(cfg.tickLowerOffset);
        int24 upperRaw = currentTick + int24(cfg.tickUpperOffset);

        cfg.tickLower = floorToSpacing(lowerRaw, cfg.tickSpacing);
        cfg.tickUpper = ceilToSpacing(upperRaw, cfg.tickSpacing);

        require(cfg.tickLower < currentTick, "lower must be below current tick");
        require(cfg.tickUpper > currentTick, "upper must be above current tick");
        require(cfg.tickLower < cfg.tickUpper, "invalid tick range");
    }

    function floorToSpacing(int24 tick, int24 spacing) internal pure returns (int24) {
        int24 compressed = tick / spacing;
        if (tick < 0 && tick % spacing != 0) compressed -= 1;
        return compressed * spacing;
    }

    function ceilToSpacing(int24 tick, int24 spacing) internal pure returns (int24) {
        int24 floored = floorToSpacing(tick, spacing);
        if (floored == tick) return floored;
        return floored + spacing;
    }

    function fundHarness(address token, address whale, uint256 amount) internal {
        uint256 resolvedAmount = resolveFundAmount(token, whale, amount);
        if (resolvedAmount == 0) return;

        if (token == address(0)) {
            VM.deal(address(this), address(this).balance + resolvedAmount);
            return;
        }

        require(whale != address(0), "missing token whale");
        VM.startPrank(whale);
        require(IERC20Minimal(token).transfer(address(this), resolvedAmount), "fund transfer failed");
        VM.stopPrank();
    }

    function resolveFundAmount(address token, address whale, uint256 configuredAmount) internal view returns (uint256) {
        if (configuredAmount != 0) return configuredAmount;

        if (token == address(0)) {
            if (whale == address(0)) return address(this).balance;
            return whale.balance;
        }

        require(whale != address(0), "missing token whale");
        return IERC20Minimal(token).balanceOf(whale);
    }

    function settleDelta(int256 delta) internal {
        (int128 amount0, int128 amount1) = unpackDelta(delta);
        settleCurrency(cfg.currency0, amount0);
        settleCurrency(cfg.currency1, amount1);
    }

    function principalDelta(int256 callerDelta, int256 feesAccrued) internal pure returns (int256) {
        (int128 caller0, int128 caller1) = unpackDelta(callerDelta);
        (int128 fee0, int128 fee1) = unpackDelta(feesAccrued);
        return packDelta(caller0 - fee0, caller1 - fee1);
    }

    function settleCurrency(address currency, int128 amount) internal {
        if (amount < 0) {
            uint256 owed = uint128(-amount);
            IPoolManagerLike manager = IPoolManagerLike(cfg.poolManager);
            if (currency == address(0)) {
                manager.settle{value: owed}();
            } else {
                manager.sync(currency);
                require(IERC20Minimal(currency).transfer(cfg.poolManager, owed), "settle transfer failed");
                manager.settle();
            }
        } else if (amount > 0) {
            IPoolManagerLike(cfg.poolManager).take(currency, address(this), uint128(amount));
        }
    }

    function fillPositionStep(StepResult storage result, int256 principal, int256 feeDelta) internal {
        result.principalDelta = principal;
        result.feeDelta = feeDelta;
        fillPoolState(result);
    }

    function fillSwapStep(StepResult storage result, int256 swapDelta) internal {
        result.swapDelta = swapDelta;
        fillPoolState(result);
    }

    function fillPoolState(StepResult storage result) internal {
        (uint160 sqrtPriceX96, int24 tick,,) = getSlot0();
        (uint256 feeGrowthGlobal0X128, uint256 feeGrowthGlobal1X128) = getFeeGrowthGlobals();
        result.sqrtPriceX96 = sqrtPriceX96;
        result.tick = tick;
        result.liquidity = getLiquidity();
        result.feeGrowthGlobal0X128 = feeGrowthGlobal0X128;
        result.feeGrowthGlobal1X128 = feeGrowthGlobal1X128;
    }

    function poolSnapshotJson() internal view returns (string memory) {
        (uint160 sqrtPriceX96, int24 tick,, uint24 lpFee) = getSlot0();
        uint128 liquidity = getLiquidity();
        (uint256 feeGrowthGlobal0X128, uint256 feeGrowthGlobal1X128) = getFeeGrowthGlobals();
        (uint128 lowerGross, int128 lowerNet,,) = getTickInfo(cfg.tickLower);
        (uint128 upperGross, int128 upperNet,,) = getTickInfo(cfg.tickUpper);
        int24 anchorTick = cfg.tickLower - cfg.tickSpacing;

        return string(
            abi.encodePacked(
                '{"state":{"sqrt_price_x96":"',
                VM.toString(uint256(sqrtPriceX96)),
                '","tick":',
                VM.toString(int256(tick)),
                ',"liquidity":',
                VM.toString(uint256(liquidity)),
                ',"fee_growth_global0_x128":"',
                VM.toString(feeGrowthGlobal0X128),
                '","fee_growth_global1_x128":"',
                VM.toString(feeGrowthGlobal1X128),
                '"',
                '},"fee":',
                VM.toString(uint256(lpFee)),
                ',"tick_spacing":',
                VM.toString(int256(cfg.tickSpacing)),
                ',"ticks":{"tick_spacing":',
                VM.toString(int256(cfg.tickSpacing)),
                ',"inner":{',
                activeAnchorAndBoundaryTicksJson(
                    anchorTick, liquidity, tick, lowerGross, lowerNet, upperGross, upperNet
                ),
                "}}}"
            )
        );
    }

    function getSlot0() internal view returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee) {
        bytes32 data = IPoolManagerLike(cfg.poolManager).extsload(poolStateSlot());
        assembly ("memory-safe") {
            sqrtPriceX96 := and(data, 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF)
            tick := signextend(2, shr(160, data))
            protocolFee := and(shr(184, data), 0xFFFFFF)
            lpFee := and(shr(208, data), 0xFFFFFF)
        }
    }

    function getLiquidity() internal view returns (uint128 liquidity) {
        bytes32 slot = bytes32(uint256(poolStateSlot()) + LIQUIDITY_OFFSET);
        liquidity = uint128(uint256(IPoolManagerLike(cfg.poolManager).extsload(slot)));
    }

    function getFeeGrowthGlobals() internal view returns (uint256 feeGrowthGlobal0X128, uint256 feeGrowthGlobal1X128) {
        bytes32 slot = bytes32(uint256(poolStateSlot()) + FEE_GROWTH_GLOBAL0_OFFSET);
        bytes32[] memory values = IPoolManagerLike(cfg.poolManager).extsload(slot, 2);
        feeGrowthGlobal0X128 = uint256(values[0]);
        feeGrowthGlobal1X128 = uint256(values[1]);
    }

    function getTickInfo(int24 targetTick)
        internal
        view
        returns (
            uint128 liquidityGross,
            int128 liquidityNet,
            uint256 feeGrowthOutside0X128,
            uint256 feeGrowthOutside1X128
        )
    {
        bytes32[] memory data = IPoolManagerLike(cfg.poolManager).extsload(tickInfoSlot(targetTick), 3);
        bytes32 firstWord = data[0];
        assembly ("memory-safe") {
            liquidityGross := and(firstWord, 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF)
            liquidityNet := sar(128, firstWord)
        }
        feeGrowthOutside0X128 = uint256(data[1]);
        feeGrowthOutside1X128 = uint256(data[2]);
    }

    function poolStateSlot() internal view returns (bytes32) {
        return keccak256(abi.encodePacked(poolId, POOLS_SLOT));
    }

    function tickInfoSlot(int24 targetTick) internal view returns (bytes32) {
        bytes32 ticksMappingSlot = bytes32(uint256(poolStateSlot()) + TICKS_OFFSET);
        return keccak256(abi.encodePacked(int256(targetTick), ticksMappingSlot));
    }

    function activeAnchorAndBoundaryTicksJson(
        int24 anchorTick,
        uint128 activeLiquidity,
        int24 currentTick,
        uint128 lowerGross,
        int128 lowerNet,
        uint128 upperGross,
        int128 upperNet
    ) internal view returns (string memory) {
        int256 anchorNet = int256(uint256(activeLiquidity));
        if (cfg.tickLower <= currentTick) anchorNet -= lowerNet;
        if (cfg.tickUpper <= currentTick) anchorNet -= upperNet;

        string memory out = "";
        bool wrote = false;

        if (anchorNet != 0) {
            out = tickJson(anchorTick, absI256(anchorNet), int128(anchorNet));
            wrote = true;
        }
        if (lowerGross != 0) {
            out = string(abi.encodePacked(out, wrote ? "," : "", tickJson(cfg.tickLower, lowerGross, lowerNet)));
            wrote = true;
        }
        if (upperGross != 0) {
            out = string(abi.encodePacked(out, wrote ? "," : "", tickJson(cfg.tickUpper, upperGross, upperNet)));
        }

        return out;
    }

    function absI256(int256 value) internal pure returns (uint128) {
        uint256 magnitude = value < 0 ? uint256(-value) : uint256(value);
        require(magnitude <= type(uint128).max, "anchor overflow");
        return uint128(magnitude);
    }

    function tickJson(int24 tick, uint128 liquidityGross, int128 liquidityNet) internal pure returns (string memory) {
        return string(
            abi.encodePacked(
                '"',
                VM.toString(int256(tick)),
                '":{"liquidity_gross":',
                VM.toString(uint256(liquidityGross)),
                ',"liquidity_net":',
                VM.toString(int256(liquidityNet)),
                "}"
            )
        );
    }

    function fixtureJson(string memory initialPool, string memory finalPool) internal view returns (string memory) {
        return string(
            abi.encodePacked(
                '{"metadata":{"name":"',
                cfg.fixtureName,
                '","scenario":"',
                cfg.scenario,
                '","chain_id":',
                VM.toString(block.chainid),
                ',"block_number":',
                VM.toString(block.number),
                ',"pool_id":"',
                VM.toString(poolId),
                '","zero_for_one":',
                cfg.zeroForOne ? "true" : "false",
                ',"exact":"',
                cfg.exactOutput ? "output" : "input",
                '","amount":"',
                VM.toString(cfg.swapAmount),
                '","protocol_fee":',
                VM.toString(uint256(cfg.protocolFee)),
                '},"owner":"',
                VM.toString(cfg.owner),
                '","initial_pool":',
                initialPool,
                ',"operations":[',
                mintOperationJson(),
                ",",
                swapOperationJson(),
                ",",
                collectOperationJson(),
                ",",
                decreaseOperationJson(),
                ',{"type":"burn","token_id":1}',
                '],"final_pool":',
                finalPool,
                "}"
            )
        );
    }

    function mintOperationJson() internal view returns (string memory) {
        return string(
            abi.encodePacked(
                '{"type":"mint","tick_lower":',
                VM.toString(int256(cfg.tickLower)),
                ',"tick_upper":',
                VM.toString(int256(cfg.tickUpper)),
                ',"liquidity":',
                VM.toString(uint256(uint128(cfg.liquidityDelta))),
                ',"amount0_max":"340282366920938463463374607431768211455","amount1_max":"340282366920938463463374607431768211455","expect":{"token_id":1,',
                modifyExpectJson(mintResult),
                "}}"
            )
        );
    }

    function swapOperationJson() internal view returns (string memory) {
        return string(
            abi.encodePacked(
                '{"type":"swap","zero_for_one":',
                cfg.zeroForOne ? "true" : "false",
                ',"exact":"',
                cfg.exactOutput ? "output" : "input",
                '","amount":"',
                VM.toString(cfg.swapAmount),
                '"',
                protocolFeeOperationJson(),
                ',"expect":',
                swapExpectJson(swapResult),
                "}"
            )
        );
    }

    function protocolFeeOperationJson() internal view returns (string memory) {
        if (cfg.protocolFee == 0) return "";
        return string(abi.encodePacked(',"protocol_fee":', VM.toString(uint256(cfg.protocolFee))));
    }

    function collectOperationJson() internal view returns (string memory) {
        return
            string(abi.encodePacked('{"type":"collect","token_id":1,"expect":{', modifyExpectJson(collectResult), "}}"));
    }

    function decreaseOperationJson() internal view returns (string memory) {
        return string(
            abi.encodePacked(
                '{"type":"decrease","token_id":1,"liquidity":',
                VM.toString(uint256(uint128(cfg.liquidityDelta))),
                ',"amount0_min":0,"amount1_min":0,"expect":{',
                modifyExpectJson(decreaseResult),
                "}}"
            )
        );
    }

    function modifyExpectJson(StepResult storage result) internal view returns (string memory) {
        (int128 p0, int128 p1) = unpackDelta(result.principalDelta);
        (int128 f0, int128 f1) = unpackDelta(result.feeDelta);
        return string(
            abi.encodePacked(
                '"principal_delta":',
                deltaJson(p0, p1),
                ',"fee_delta":',
                deltaJson(f0, f1),
                ',"liquidity":',
                VM.toString(uint256(result.liquidity))
            )
        );
    }

    function swapExpectJson(StepResult storage result) internal view returns (string memory) {
        (int128 d0, int128 d1) = unpackDelta(result.swapDelta);
        return string(
            abi.encodePacked(
                '{"delta":',
                deltaJson(d0, d1),
                ',"sqrt_price_x96":"',
                VM.toString(uint256(result.sqrtPriceX96)),
                '","tick":',
                VM.toString(int256(result.tick)),
                ',"liquidity":',
                VM.toString(uint256(result.liquidity)),
                ',"fee_growth_global0_x128":"',
                VM.toString(result.feeGrowthGlobal0X128),
                '","fee_growth_global1_x128":"',
                VM.toString(result.feeGrowthGlobal1X128),
                '"}'
            )
        );
    }

    function deltaJson(int128 amount0, int128 amount1) internal pure returns (string memory) {
        return string(
            abi.encodePacked(
                '{"amount0":', VM.toString(int256(amount0)), ',"amount1":', VM.toString(int256(amount1)), "}"
            )
        );
    }

    function unpackDelta(int256 delta) internal pure returns (int128 amount0, int128 amount1) {
        amount0 = int128(delta >> 128);
        amount1 = int128(delta);
    }

    function packDelta(int128 amount0, int128 amount1) internal pure returns (int256) {
        return (int256(amount0) << 128) | int256(uint256(uint128(amount1)));
    }

    receive() external payable {}
}
