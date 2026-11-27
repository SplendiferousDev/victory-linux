//! `VICTORY_PATCH_SPANS` (off by default; `1` on; `alt` in turn): a page
//! that goes up as a masked patch (`crate::shadow::masked_patch_now`) costs
//! the submitting thread one pass over its bytes and one note.
//!
//! Why: with the patch on (run qE2, `VICTORY_PATCH_LEAN=1`) the frame was
//! 141 ms against 116 off, though the settles fell 1.2 -> 0.4 a frame and
//! the card's own work stayed ~69 ms. The loss was bookkeeping per changed
//! run: ~300,000 runs a frame (1.17 MB over ~190 pages, ~4 bytes each).
//! Each page's runs were listed twice, its words made byte by byte from
//! them, and every run noted as a range of its own: in the patch's pending
//! ranges, in the copy's notes of what was sent (each later landing of a
//! fetch filtered all of them, for every piece it landed), sorted and
//! joined again for the hash passes.
//!
//! With the switch, for such a page:
//! - [`page_words`] compares the new bytes with the CPU side's 64 at a time
//!   and makes the patch's words (one per dword holding a changed byte, in
//!   order) in the same pass, with the number of changed runs and bytes;
//!   the words equal those made from the runs bit for bit (tests);
//! - the page is noted once, as a [`PageSpan`]: the stretch from its first
//!   changed byte to its last, with its words. The hash passes take the
//!   stretch (one piece, as the runs' pieces were: a page is a piece). A
//!   landing does NOT take the stretch: there a byte between the title's
//!   runs may be a shader's write that must land, so it takes the exact
//!   changed bytes, read off the words ([`runs_in`]), and only for spans
//!   that overlap what it lands.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A masked patch's word: the dword's offset in the copy, the mask of the
/// bytes it stores, their values (`crate::shadow::PatchWord`).
pub(crate) type Word = [u32; 3];

static SWITCH: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_PATCH_SPANS",
    "pages that go up as a masked patch are compared once, word by word, and noted as one span each",
);

/// Whether the switch is on now.
pub(crate) fn on() -> bool {
    SWITCH.on()
}

/// Whether the switch is set (`1` or `alt`), for the frame line.
pub(crate) fn set() -> bool {
    SWITCH.set()
}

/// Since the last frame line: pages patched as spans, spans noted as sent,
/// words made, nanoseconds making them and noting the page (under the
/// copy's lock), and exact runs a landing read off spans.
pub(crate) static METER: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Adds to [`METER`]'s entry `i`.
pub(crate) fn count(i: usize, n: u64) {
    METER[i].fetch_add(n, Ordering::Relaxed);
}

/// A patched page, noted once: offsets `[from, to)` of the copy (its first
/// changed byte to its last, inclusive) and the patch's words, in order of
/// their offsets, one per dword, each masking exactly the changed bytes.
#[derive(Clone, Debug)]
pub(crate) struct PageSpan {
    pub(crate) from: usize,
    pub(crate) to: usize,
    pub(crate) words: Arc<[Word]>,
}

impl PageSpan {
    /// The span of `words` (not empty, in order, each with a mask).
    pub(crate) fn new(words: Vec<Word>) -> PageSpan {
        let (first, last) = (words[0], words[words.len() - 1]);
        let from = first[0] as usize + (first[1].trailing_zeros() / 8) as usize;
        let to = last[0] as usize + 4 - (last[1].leading_zeros() / 8) as usize;
        PageSpan { from, to, words: words.into() }
    }

    /// Whether it overlaps offsets `[from, to)`.
    pub(crate) fn overlaps(&self, from: usize, to: usize) -> bool {
        self.from < to && self.to > from
    }
}

/// The mask of a dword's bytes from 4 bits (bit `k`: byte `k`).
const LANES: [u32; 16] = {
    let mut out = [0u32; 16];
    let mut i = 0;
    while i < 16 {
        let mut k = 0;
        while k < 4 {
            if i & (1 << k) != 0 {
                out[i] |= 0xff << (8 * k);
            }
            k += 1;
        }
        i += 1;
    }
    out
};

/// Bit `k`: byte `k` of the 64 differs.
#[cfg(target_arch = "x86_64")]
fn differ64(now: &[u8], was: &[u8]) -> u64 {
    use core::arch::x86_64::{__m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8};
    debug_assert!(now.len() >= 64 && was.len() >= 64);
    let mut differ = 0u64;
    for part in 0..4 {
        // SAFETY: both slices hold 64 bytes (asserted by the callers'
        // bounds); unaligned loads. SSE2 is part of x86-64.
        let equal = unsafe {
            let a = _mm_loadu_si128(now.as_ptr().add(16 * part).cast::<__m128i>());
            let b = _mm_loadu_si128(was.as_ptr().add(16 * part).cast::<__m128i>());
            _mm_movemask_epi8(_mm_cmpeq_epi8(a, b)) as u32 & 0xffff
        };
        differ |= u64::from(!equal & 0xffff) << (16 * part);
    }
    differ
}

#[cfg(not(target_arch = "x86_64"))]
fn differ64(now: &[u8], was: &[u8]) -> u64 {
    (0..64).fold(0u64, |m, k| m | (u64::from(now[k] != was[k]) << k))
}

/// One byte that differs, at offset `offset` of the copy, into `out`.
fn push_byte(out: &mut Vec<Word>, offset: usize, value: u8) {
    let word = (offset & !3) as u32;
    let lane = ((offset & 3) * 8) as u32;
    let (mask, value) = (0xffu32 << lane, u32::from(value) << lane);
    match out.last_mut() {
        Some(w) if w[0] == word => {
            w[1] |= mask;
            w[2] |= value;
        }
        _ => out.push([word, mask, value]),
    }
}

/// The words of a masked patch of the page at offset `at` of a copy whose
/// new bytes are `now` and whose CPU side holds `was` (one length, under
/// `u32::MAX` with `at`), appended to `out` in order of their offsets: per
/// dword holding a byte that differs, the mask of those bytes and their
/// new values (0 elsewhere). Returns the changed runs (as
/// `crate::shadow`'s `changed_runs` counts them) and bytes. One pass.
pub(crate) fn page_words(at: usize, now: &[u8], was: &[u8], out: &mut Vec<Word>) -> (usize, usize) {
    let n = now.len().min(was.len());
    let (now, was) = (&now[..n], &was[..n]);
    let (mut runs, mut bytes) = (0usize, 0usize);
    // Whether the byte before the next one looked at differed.
    let mut open = false;
    // A byte at a time (the ends that are not whole blocks of 64).
    let one = |out: &mut Vec<Word>, i: usize, open: &mut bool, runs: &mut usize, bytes: &mut usize| {
        if now[i] != was[i] {
            *runs += usize::from(!*open);
            *bytes += 1;
            *open = true;
            push_byte(out, at + i, now[i]);
        } else {
            *open = false;
        }
    };
    // Up to the first whole dword of the copy.
    let head = ((4 - (at & 3)) & 3).min(n);
    for i in 0..head {
        one(out, i, &mut open, &mut runs, &mut bytes);
    }
    let mut base = head;
    while base + 64 <= n {
        let differ = differ64(&now[base..base + 64], &was[base..base + 64]);
        if differ == 0 {
            open = false;
            base += 64;
            continue;
        }
        runs += (differ & !((differ << 1) | u64::from(open))).count_ones() as usize;
        bytes += differ.count_ones() as usize;
        open = differ >> 63 != 0;
        let mut left = differ;
        while left != 0 {
            let dword = (left.trailing_zeros() / 4) as usize;
            let mask = LANES[((differ >> (4 * dword)) & 15) as usize];
            let k = base + 4 * dword;
            let value = u32::from_le_bytes([now[k], now[k + 1], now[k + 2], now[k + 3]]) & mask;
            out.push([(at + k) as u32, mask, value]);
            left &= !(15u64 << (4 * dword));
        }
        base += 64;
    }
    for i in base..n {
        one(out, i, &mut open, &mut runs, &mut bytes);
    }
    (runs, bytes)
}

/// The changed bytes of `words` (in order) inside offsets `[from, to)`, as
/// ranges of offsets `(start, end)` in order, adjacent bytes as one,
/// appended to `out`. Returns how many.
pub(crate) fn runs_in(words: &[Word], from: usize, to: usize, out: &mut Vec<(usize, usize)>) -> usize {
    let first = words.partition_point(|w| w[0] as usize + 4 <= from);
    let before = out.len();
    let mut open: Option<(usize, usize)> = None;
    for w in &words[first..] {
        let word = w[0] as usize;
        if word >= to {
            break;
        }
        for lane in 0..4 {
            let i = word + lane;
            if w[1] & (0xff << (8 * lane)) == 0 || i < from || i >= to {
                continue;
            }
            match &mut open {
                Some((_, end)) if *end == i => *end = i + 1,
                _ => {
                    if let Some(run) = open.take() {
                        out.push(run);
                    }
                    open = Some((i, i + 1));
                }
            }
        }
    }
    if let Some(run) = open {
        out.push(run);
    }
    out.len() - before
}

/// The changed runs of `words` of the page at offset `at`, as (offset in the
/// page, bytes) in order: what `crate::shadow`'s `changed_runs` lists for
/// the bytes the words were made of.
pub(crate) fn page_runs(words: &[Word], at: usize, len: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    runs_in(words, at, at + len, &mut out);
    out.into_iter().map(|(a, b)| (a - at, b - a)).collect()
}

/// Puts the bytes `words` mask into `held`, the copy's bytes from offset
/// `at` on (the page `words` were made of): exactly the changed bytes.
pub(crate) fn apply(words: &[Word], at: usize, held: &mut [u8]) {
    let end = at + held.len();
    for w in words {
        let word = w[0] as usize;
        if word >= at && word + 4 <= end {
            let k = word - at;
            let old = u32::from_le_bytes([held[k], held[k + 1], held[k + 2], held[k + 3]]);
            held[k..k + 4].copy_from_slice(&((old & !w[1]) | (w[2] & w[1])).to_le_bytes());
            continue;
        }
        for lane in 0..4 {
            let i = word + lane;
            if w[1] & (0xff << (8 * lane)) != 0 && i >= at && i < end {
                held[i - at] = (w[2] >> (8 * lane)) as u8;
            }
        }
    }
}

/// Whether `words` are in strictly rising order of offsets (one per
/// dword): folding them changes nothing then.
pub(crate) fn strictly_rising(words: &[Word]) -> bool {
    words.windows(2).all(|p| p[0][0] < p[1][0])
}

// Tests: `patchspan_tests.rs`, inside `crate::shadow` (they compare with its
// run-by-run path and land on its copies).
