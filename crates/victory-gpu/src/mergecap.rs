//! Two checks of the merge on the card (`crate::shadow::merge_on_card`):
//!
//! - `VICTORY_MERGE_CAP` (off by default; `1` on, `alt` in turn): the
//!   title's bytes put in under flights' shader writes go through the
//!   merge on the card only as far as the cap of changed runs a submission
//!   (`VICTORY_MERGE_RUNS`) leaves, as the copies of the runs do without
//!   it; past the cap the flights are settled. With the cap the merge on
//!   the card is only another way up for the same bytes; without it it
//!   also takes the pages the cap used to settle for (hundreds of
//!   thousands of changed runs a frame where the copies take a few
//!   hundred). A run with and one without tell which of the two a fault
//!   with the merge on comes from.
//! - Torn words, counted whenever the merge on the card is wanted: when a
//!   flight lands (`Shadow::card_merge_relay`), the aligned words of 4, 8
//!   and 16 bytes that hold both a byte the merge takes from the title (a
//!   mask bit) and a byte a shader wrote on the card (the card's side
//!   differs from the CPU's there, outside the title's bytes still on
//!   their way up). Such a word ends up on the card part title, part
//!   shader: a value no one wrote whole, if the word is one value.

use std::sync::atomic::{AtomicU64, Ordering};

static CAP: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_MERGE_CAP",
    "the merge on the card takes the title's changed runs only up to the cap of VICTORY_MERGE_RUNS a submission; past it the flights are settled",
);

/// Whether the merge on the card keeps to the cap of changed runs now.
pub(crate) fn cap_on() -> bool {
    CAP.on()
}

/// The cap's state for the log.
pub(crate) fn cap_said() -> &'static str {
    match CAP.mode() {
        0 => "off",
        1 => "on",
        _ => "alt",
    }
}

/// Since the last frame-rate line: landed pieces that held a byte of the
/// merge on the card, and torn words of 4, 8 and 16 bytes in them.
static TORN: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Counts one landed piece's torn words ([`torn_words`]).
pub(crate) fn count(torn: [u64; 3]) {
    TORN[0].fetch_add(1, Ordering::Relaxed);
    for (i, n) in torn.into_iter().enumerate() {
        TORN[i + 1].fetch_add(n, Ordering::Relaxed);
    }
}

/// [`TORN`] since the last asking, per frame of `frames`.
pub(crate) fn taken(frames: f64) -> [f64; 4] {
    [0, 1, 2, 3].map(|i| TORN[i].swap(0, Ordering::Relaxed) as f64 / frames)
}

/// Of `len` bytes from address `base` on: the aligned words of 4, 8 and
/// 16 bytes that hold a byte of `masked` (offsets from `base`, in order)
/// and another byte `i`, not in `masked`, for which `wrote(i)`. Bytes
/// outside `[0, len)` are neither.
pub(crate) fn torn_words(base: usize, len: usize, masked: &[usize], wrote: impl Fn(usize) -> bool) -> [u64; 3] {
    let mut out = [0u64; 3];
    if masked.is_empty() {
        return out;
    }
    let mut is_masked = vec![false; len];
    for &i in masked {
        if i < len {
            is_masked[i] = true;
        }
    }
    for (k, size) in [4usize, 8, 16].into_iter().enumerate() {
        let mut last_word = usize::MAX;
        for &i in masked {
            if i >= len {
                continue;
            }
            let word = (base + i) / size;
            if word == last_word {
                continue;
            }
            last_word = word;
            let lo = (word * size).saturating_sub(base);
            let hi = ((word + 1) * size - base).min(len);
            if (lo..hi).any(|j| !is_masked[j] && wrote(j)) {
                out[k] += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::torn_words;

    #[test]
    fn a_word_is_torn_only_with_both_a_title_byte_and_a_shader_byte() {
        // Title bytes at 0 and 9; the shader wrote 2 and 20.
        let wrote = |i: usize| i == 2 || i == 20;
        // 4: [0,4) torn, [8,12) not; 8: [0,8) torn; 16: [0,16) torn once.
        assert_eq!(torn_words(0, 32, &[0, 9], wrote), [1, 1, 1]);
        // Nothing written by the shader: nothing torn.
        assert_eq!(torn_words(0, 32, &[0, 9], |_| false), [0, 0, 0]);
        // No title bytes: nothing torn.
        assert_eq!(torn_words(0, 32, &[], wrote), [0, 0, 0]);
    }

    #[test]
    fn a_masked_byte_is_no_shader_byte() {
        // Every byte of the 4-byte word is the title's: not torn; the 8-
        // and 16-byte words hold shader bytes beside them.
        assert_eq!(torn_words(0, 8, &[0, 1, 2, 3], |_| true), [0, 1, 1]);
    }

    #[test]
    fn words_are_aligned_on_the_address() {
        // From address 6: offset 1 is address 7 (words 4..8, 0..8, 0..16),
        // offset 2 is address 8 (words 8..12, 8..16, 0..16).
        let wrote = |i: usize| i == 2;
        assert_eq!(torn_words(6, 16, &[1], wrote), [0, 0, 1]);
        // A word cut by the piece's start or end looks only inside.
        assert_eq!(torn_words(6, 3, &[0], |i| i == 5), [0, 0, 0]);
    }
}
