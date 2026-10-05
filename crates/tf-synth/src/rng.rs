//! SplitMix64, implemented here rather than pulled from `rand`: the synthetic
//! stream must be bit-identical across platforms and across dependency
//! upgrades, and `rand`'s generators don't promise value stability.

#[derive(Clone, Debug)]
pub struct SplitMix64(u64);

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

impl SplitMix64 {
    pub const fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    /// Independent stream derived from `(seed, stream)`.
    pub fn fork(seed: u64, stream: u64) -> Self {
        let mut mixer = SplitMix64::new(seed ^ stream.wrapping_mul(GAMMA));
        SplitMix64::new(mixer.next_u64())
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GAMMA);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`), via a multiply-shift; no modulo bias worth caring about here.
    pub fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        debug_assert!(lo <= hi);
        lo + self.below(hi - lo + 1)
    }

    /// True with probability `p / 1000`.
    pub fn permille(&mut self, p: u32) -> bool {
        self.below(1000) < u64::from(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_vector_for_seed_zero() {
        let mut r = SplitMix64::new(0);
        assert_eq!(r.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(r.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(r.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn bounded_draws_stay_in_bounds() {
        let mut r = SplitMix64::new(7);
        for _ in 0..10_000 {
            assert!(r.below(10) < 10);
            let v = r.range(5, 8);
            assert!((5..=8).contains(&v));
        }
    }

    #[test]
    fn forked_streams_differ_and_are_reproducible() {
        let a = SplitMix64::fork(1, 0).next_u64();
        let b = SplitMix64::fork(1, 1).next_u64();
        assert_ne!(a, b);
        assert_eq!(a, SplitMix64::fork(1, 0).next_u64());
    }
}
