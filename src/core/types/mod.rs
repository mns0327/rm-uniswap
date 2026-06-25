pub(crate) mod atomic_f64;
pub(crate) mod delta;
pub(crate) mod signed;

use serde::{Deserialize, Serialize};

/// Shared initialized-tick representation used by concentrated-liquidity pools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickEntry {
    pub tick_idx: i32,
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
}
