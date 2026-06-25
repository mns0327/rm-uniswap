use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub struct AtomicF64 {
    bits: AtomicU64,
}

impl AtomicF64 {
    #[inline]
    pub fn new(value: f64) -> Self {
        Self {
            bits: AtomicU64::new(value.to_bits()),
        }
    }

    #[inline]
    pub fn load(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn store(&self, value: f64) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }
}

impl Serialize for AtomicF64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_f64(self.load())
    }
}

impl<'de> Deserialize<'de> for AtomicF64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value: f64 = Deserialize::deserialize(deserializer)?;
        Ok(Self::new(value))
    }
}
