/// Maximum swap fee: 100%, expressed in hundredths of a bip.
pub(crate) const MAX_SWAP_FEE: u32 = 1_000_000;

#[derive(Debug, Clone, Copy)]
pub(crate) struct FeeParams {
    pub(crate) pips: u32,
    pub(crate) complement: u32,
}

impl FeeParams {
    #[inline(always)]
    pub(crate) const fn new(fee_pips: u32) -> Option<Self> {
        if fee_pips > MAX_SWAP_FEE {
            return None;
        }

        Some(Self {
            pips: fee_pips,
            complement: MAX_SWAP_FEE - fee_pips,
        })
    }

    #[inline(always)]
    pub(crate) const fn is_zero(self) -> bool {
        self.pips == 0
    }

    #[inline(always)]
    pub(crate) const fn is_max(self) -> bool {
        self.pips == MAX_SWAP_FEE
    }
}
