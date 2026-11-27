//! A quick hasher for maps keyed by our own numbers (addresses, lengths, flags), on the record
//! path: the standard one (SipHash, seeded) is built to resist keys chosen by an attacker, which
//! these are not, and costs several times as much per key. In a profile of the title's five
//! recording threads SipHash was about 4% of their time, much of it in the per-thread map of
//! [`crate::shadow`]'s lookups.
//!
//! The mixing is the multiply-and-rotate of rustc's own hasher (FxHash), with a final
//! multiply-shift so that the high bits hashbrown picks its groups by are mixed too.

use std::hash::{BuildHasherDefault, Hasher};

/// See the module.
#[derive(Default, Clone, Copy)]
pub(crate) struct Fast(u64);

const K: u64 = 0xf135_7aea_2e62_a9c5;

impl Fast {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(26) ^ word).wrapping_mul(K);
    }
}

impl Hasher for Fast {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap_or([0; 8])));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(last) ^ ((rest.len() as u64) << 56));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        // Spread the low bits into the high ones too.
        let h = self.0.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h ^ (h >> 29)
    }
}

/// A `HashMap` with [`Fast`].
pub(crate) type FastMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<Fast>>;

/// A `HashSet` with [`Fast`].
pub(crate) type FastSet<T> = std::collections::HashSet<T, BuildHasherDefault<Fast>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{BuildHasher, Hash};

    fn hash<T: Hash>(t: &T) -> u64 {
        BuildHasherDefault::<Fast>::default().hash_one(t)
    }

    #[test]
    fn equal_keys_hash_alike_and_near_keys_apart() {
        assert_eq!(hash(&(0x1000u64, 64u64, true)), hash(&(0x1000u64, 64u64, true)));
        let mut seen = std::collections::HashSet::new();
        for a in 0..4096u64 {
            for any in [false, true] {
                assert!(seen.insert(hash(&(0x7_0000_0000 + a * 16, 256u64, any))));
            }
        }
    }

    #[test]
    fn a_fast_map_works_as_a_map() {
        let mut m: FastMap<(u64, u64, bool), u32> = FastMap::default();
        for i in 0..10_000u64 {
            m.insert((i * 64, i, i % 2 == 0), i as u32);
        }
        for i in 0..10_000u64 {
            assert_eq!(m.get(&(i * 64, i, i % 2 == 0)), Some(&(i as u32)));
            assert_eq!(m.get(&(i * 64, i, i % 2 == 1)), None);
        }
    }

    #[test]
    fn bytes_of_any_length() {
        let mut a = Fast::default();
        a.write(b"abcdefghij");
        let mut b = Fast::default();
        b.write(b"abcdefghik");
        assert_ne!(a.finish(), b.finish());
    }
}
