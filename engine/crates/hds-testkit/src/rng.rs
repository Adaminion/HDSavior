//! Deterministic pseudo-random numbers.
//!
//! Everything random in an image (file contents, sizes, times, names of the
//! generated trees) comes from a `u64` seed through this module, so the same
//! seed gives the same bytes on every platform and toolchain. The generators
//! are SplitMix64 and xoshiro256** as published by Vigna; nothing here is
//! meant to be cryptographic.

/// One SplitMix64 step: returns the output for state `x` (after adding the
/// golden-ratio increment). Used as a mixing function throughout the crate.
#[inline]
pub fn mix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Derives an independent sub-seed from a seed and a label, so that adding a
/// file to a scenario does not change the content of the others.
pub fn derive(seed: u64, label: &str) -> u64 {
    let mut h = mix64(seed ^ 0x6864_735F_7465_7374); // "hds_test"
    for &b in label.as_bytes() {
        h = mix64(h ^ b as u64);
    }
    mix64(h ^ label.len() as u64)
}

/// Derives a sub-seed from a seed and a number.
pub fn derive_n(seed: u64, n: u64) -> u64 {
    mix64(mix64(seed) ^ n.rotate_left(17))
}

/// xoshiro256** seeded through SplitMix64.
#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// A generator for `seed`.
    pub fn new(seed: u64) -> Self {
        let mut x = seed;
        let mut s = [0u64; 4];
        for slot in &mut s {
            *slot = mix64(x);
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        }
        if s == [0; 4] {
            s[0] = 1;
        }
        Rng { s }
    }

    /// Next 64 random bits.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform value in `0..bound` (`bound` must not be 0).
    pub fn below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "below(0)");
        // Lemire's multiply-shift with rejection: unbiased.
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let m = (self.next_u64() as u128) * (bound as u128);
            if (m as u64) >= threshold {
                return (m >> 64) as u64;
            }
        }
    }

    /// Uniform value in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        assert!(lo <= hi, "range({lo}, {hi})");
        if lo == 0 && hi == u64::MAX {
            return self.next_u64();
        }
        lo + self.below(hi - lo + 1)
    }

    /// `true` with probability `num / den`.
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }

    /// Fills `buf` with random bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        let mut chunks = buf.chunks_exact_mut(8);
        for chunk in &mut chunks {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        let rest = chunks.into_remainder();
        if !rest.is_empty() {
            let bytes = self.next_u64().to_le_bytes();
            rest.copy_from_slice(&bytes[..rest.len()]);
        }
    }

    /// Picks one element.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_matches_published_sequence() {
        // First outputs of SplitMix64 for state 0 (Vigna's reference code;
        // the values are quoted in many test suites, e.g. Java's
        // SplittableRandom compatibility tests).
        let mut state = 0u64;
        let mut out = Vec::new();
        for _ in 0..3 {
            out.push(mix64(state));
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        }
        assert_eq!(
            out,
            [
                0xE220_A839_7B1D_CDAF,
                0x6E78_9E6A_A1B9_65F4,
                0x06C4_5D18_8009_454F
            ]
        );
    }

    #[test]
    fn same_seed_same_stream_and_seeds_differ() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        let mut c = Rng::new(43);
        let xs: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
        let zs: Vec<u64> = (0..16).map(|_| c.next_u64()).collect();
        assert_eq!(xs, ys);
        assert_ne!(xs, zs);
        assert_ne!(derive(1, "a"), derive(1, "b"));
        assert_ne!(derive(1, "a"), derive(2, "a"));
        assert_ne!(derive(1, "ab"), derive(1, "ba"));
        assert_ne!(derive_n(1, 0), derive_n(1, 1));
    }

    #[test]
    fn bounded_values_stay_in_range_and_cover_it() {
        let mut r = Rng::new(7);
        let mut seen = [false; 10];
        for _ in 0..1000 {
            let v = r.below(10);
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
        for _ in 0..1000 {
            let v = r.range(5, 8);
            assert!((5..=8).contains(&v));
        }
        assert_eq!(r.range(9, 9), 9);
        let mut buf = [0u8; 13];
        r.fill(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
    }
}
