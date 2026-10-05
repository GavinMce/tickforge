//! A log-linear latency histogram: integer-only, fixed size, no allocation.
//!
//! Values below 16 get their own bucket; above that each power of two is split
//! into 16 equal sub-buckets, so a reported percentile is at most 1/16 (6.25%)
//! above the true value. Percentiles report a bucket's upper edge, capped at the
//! largest value recorded, so they never understate a tail.

const SUB_BITS: u32 = 4;
const SUB: u64 = 1 << SUB_BITS;

const fn index(v: u64) -> usize {
    if v < SUB {
        return v as usize;
    }
    let msb = 63 - v.leading_zeros();
    let shift = msb - SUB_BITS;
    (((shift as u64 + 1) << SUB_BITS) + ((v >> shift) & (SUB - 1))) as usize
}

const BUCKETS: usize = index(u64::MAX) + 1;

/// The largest value that lands in bucket `idx`.
fn upper(idx: usize) -> u64 {
    let idx = idx as u64;
    if idx < SUB {
        return idx;
    }
    let (k, sub) = (idx >> SUB_BITS, idx & (SUB - 1));
    let shift = k - 1;
    let lower = (SUB + sub) << shift;
    lower.saturating_add((1u64 << shift) - 1)
}

#[derive(Clone, Debug)]
pub struct Histogram {
    counts: [u64; BUCKETS],
    total: u64,
    max: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub const fn new() -> Self {
        Histogram {
            counts: [0; BUCKETS],
            total: 0,
            max: 0,
        }
    }

    #[inline]
    pub fn record(&mut self, v: u64) {
        self.counts[index(v)] += 1;
        self.total += 1;
        self.max = self.max.max(v);
    }

    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += b;
        }
        self.total += other.total;
        self.max = self.max.max(other.max);
    }

    pub fn count(&self) -> u64 {
        self.total
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    /// The value at or below which `permille / 1000` of samples fall
    /// (500 is the median, 999 is p99.9). Zero for an empty histogram.
    pub fn percentile(&self, permille: u32) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let rank = (u128::from(self.total) * u128::from(permille.min(1000)))
            .div_ceil(1000)
            .max(1) as u64;
        let mut seen = 0;
        for (i, &c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return upper(i).min(self.max);
            }
        }
        self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_synth::SplitMix64;

    #[test]
    fn indexing_is_continuous_and_monotone() {
        assert_eq!(BUCKETS, 976);
        let mut prev = 0;
        for v in (0..100_000u64).chain([u64::MAX / 2, u64::MAX - 1, u64::MAX]) {
            let i = index(v);
            assert!(i >= prev && i < BUCKETS, "{v} -> {i}");
            assert!(upper(i) >= v, "bucket {i} upper {} < {v}", upper(i));
            assert!(
                i == 0 || upper(i - 1) < v,
                "{v} should not fit bucket {}",
                i - 1
            );
            prev = i;
        }
        assert_eq!(upper(BUCKETS - 1), u64::MAX);
    }

    #[test]
    fn percentiles_are_within_one_sixteenth_above_the_exact_value() {
        let mut rng = SplitMix64::new(5);
        for scale in [20u32, 40, 60] {
            // Log-uniform samples, so every magnitude is exercised.
            let mut xs: Vec<u64> = (0..20_000)
                .map(|_| {
                    let bits = rng.range(1, u64::from(scale));
                    1 + rng.below(1 << bits)
                })
                .collect();
            let mut h = Histogram::new();
            xs.iter().for_each(|&x| h.record(x));
            xs.sort_unstable();
            for q in [1u32, 100, 500, 900, 990, 999, 1000] {
                let rank = (xs.len() as u64 * u64::from(q)).div_ceil(1000).max(1) as usize;
                let exact = xs[rank - 1];
                let got = h.percentile(q);
                assert!(got >= exact, "q{q}: {got} understates {exact}");
                assert!(
                    got <= exact + exact / 16 + 1,
                    "q{q}: {got} is more than 1/16 above {exact}"
                );
            }
            assert_eq!((h.count(), h.max()), (xs.len() as u64, *xs.last().unwrap()));
        }
    }

    #[test]
    fn small_values_are_exact_and_empty_is_zero() {
        let mut h = Histogram::new();
        assert_eq!((h.percentile(500), h.percentile(999)), (0, 0));
        for v in [3, 3, 3, 9] {
            h.record(v);
        }
        assert_eq!((h.percentile(500), h.percentile(1000)), (3, 9));
    }

    #[test]
    fn merging_equals_recording_everything_in_one() {
        let (mut a, mut b, mut all) = (Histogram::new(), Histogram::new(), Histogram::new());
        let mut rng = SplitMix64::new(9);
        for i in 0..5000 {
            let v = rng.below(1_000_000);
            all.record(v);
            if i % 2 == 0 { a.record(v) } else { b.record(v) }
        }
        a.merge(&b);
        for q in [500, 990, 999, 1000] {
            assert_eq!(a.percentile(q), all.percentile(q));
        }
        assert_eq!((a.count(), a.max()), (all.count(), all.max()));
    }
}
