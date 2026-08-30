use alloy::primitives::{Address, FixedBytes, keccak256};

/// Number of bytes hashed by Uniswap v4 `PoolIdLibrary.toId`.
///
/// Solidity stores the in-memory `PoolKey` body as five contiguous 32-byte
/// words. Hashing exactly those words matches `keccak256(poolKey, 0xa0)`.
const POOL_KEY_MEMORY_LEN: usize = 32 * 5;

/// A Uniswap v4 pool key.
///
/// `PoolKey` is the canonical tuple that identifies a pool: two sorted
/// currencies, the LP fee, the tick spacing, and the hook contract. The raw
/// numeric fields mirror Solidity's `uint24 fee` and `int24 tickSpacing`
/// values; callers are responsible for supplying protocol-valid values.
///
/// Currency ordering is part of the key. This constructor preserves the
/// provided order and does not sort `currency0`/`currency1`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct PoolKey {
    /// Lower currency address, sorted numerically according to Uniswap v4.
    ///
    /// Native ETH is represented by `Address::ZERO`.
    pub currency0: Address,
    /// Higher currency address, sorted numerically according to Uniswap v4.
    pub currency1: Address,
    /// LP fee in pips.
    ///
    /// Static fees are capped at `1_000_000`; dynamic-fee pools use the v4
    /// dynamic fee flag value.
    pub fee: u32,
    /// Pool tick spacing.
    ///
    /// Positions must use ticks that are multiples of this value.
    pub tick_spacing: u32,
    /// Hook contract address for the pool.
    ///
    /// Use `Address::ZERO` when the pool has no hooks.
    pub hooks: Address,
}

#[allow(dead_code)]
impl PoolKey {
    /// Constructs a pool key from already-ordered, already-validated raw fields.
    ///
    /// The values are stored exactly as provided so callers can reproduce
    /// onchain pool IDs byte-for-byte.
    #[inline(always)]
    pub const fn new(
        currency0: Address,
        currency1: Address,
        fee: u32,
        tick_spacing: u32,
        hooks: Address,
    ) -> Self {
        Self {
            currency0,
            currency1,
            fee,
            tick_spacing,
            hooks,
        }
    }

    /// Returns the Uniswap v4 `PoolId` for this key.
    ///
    /// Matches `PoolIdLibrary.toId`: the hash input is the five-word Solidity
    /// memory body of `PoolKey`, equivalent to `keccak256(poolKey, 0xa0)`.
    pub fn to_id(&self) -> PoolId {
        let mut encoded = [0u8; POOL_KEY_MEMORY_LEN];
        write_address_word(&mut encoded[0..32], &self.currency0);
        write_address_word(&mut encoded[32..64], &self.currency1);
        write_u32_word(&mut encoded[64..96], self.fee);
        write_u32_word(&mut encoded[96..128], self.tick_spacing);
        write_address_word(&mut encoded[128..160], &self.hooks);

        PoolId(keccak256(encoded))
    }
}

/// A Uniswap v4 pool identifier.
///
/// This is the 32-byte hash returned by [`PoolKey::to_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolId(pub FixedBytes<32>);

/// Writes an address as a Solidity memory word.
///
/// Address values are right-aligned in a 32-byte word, leaving the high
/// 12 bytes as zero.
fn write_address_word(word: &mut [u8], address: &Address) {
    word[12..].copy_from_slice(address.as_slice());
}

/// Writes a non-negative integer as a Solidity memory word.
///
/// `PoolKey` currently accepts raw `u32` values for fee and tick spacing. Valid
/// v4 inputs fit in the low 24 bits, but writing the full `u32` keeps the word
/// construction explicit and rejects nothing implicitly.
fn write_u32_word(word: &mut [u8], value: u32) {
    word[28..].copy_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256};

    #[test]
    fn to_id_matches_arbitrum_eth_usdc_pool_id() {
        let key = PoolKey::new(
            Address::ZERO,
            address!("0xaf88d065e77c8cC2239327C5EDb3A432268e5831"),
            500,
            10,
            Address::ZERO,
        );

        assert_eq!(
            key.to_id(),
            PoolId(b256!(
                "0x864abca0a6202dba5b8868772308da953ff125b0f95015adbf89aaf579e903a8"
            ))
        );
    }
}
