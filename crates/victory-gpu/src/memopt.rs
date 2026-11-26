//! What the refreshes of host copies copy (the frame line's "copies N ms
//! M MB"), and one exact cut of it: `VICTORY_MEM_SUBPAGE`.
//!
//! Ported by hand from ScarlettPort's `memopt.rs` (branch fx-mem;
//! GPL-3.0-or-later, credit to the ScarlettPort authors), on this tree's
//! `Shadow::copy_stale`, which also has the 4 KiB narrowing
//! (`VICTORY_DIRTY_4K`).
//!
//! - Counters, always on, logged with the frame-rate line ("refresh bytes
//!   per frame by kind"): bytes copied on pages a refresh holds whole, on
//!   pages it holds only in part, of memory that is not write-watched
//!   (copied whole every time), of those into copies in the card's memory,
//!   and the bytes `VICTORY_MEM_SUBPAGE` left out as already taken.
//! - `VICTORY_MEM_SUBPAGE=1` (off by default; `VICTORY_MEM_SUBPAGE=alt`:
//!   twenty seconds off, twenty on, in turn): a 64 KiB write-watch page
//!   that a refresh holds only in part was never noted as taken (the copy's
//!   `copied[page]` is set only for pages held whole), so every refresh of
//!   a small range (a constant buffer, the gaps around ranges shaders
//!   write) copied its bytes again, written or not. With the switch each
//!   such page keeps the byte ranges taken and the asking each was taken
//!   at; a later refresh copies only what is not held by a range taken at
//!   or after the page's last write ([`gaps`]). The rule is the whole
//!   page's (`write > copied` is stale), applied to byte ranges: exact by
//!   the same write-watch evidence.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static SUBPAGE: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_MEM_SUBPAGE",
    "refreshes note the byte ranges they take of a 64 KiB page they hold only in part, and copy them again only once the page is written",
);

/// `VICTORY_MEM_SUBPAGE`: 1 on, `alt` in turn. While off no range is kept
/// or used; ranges kept before stay valid (each is dropped by the page's
/// next write, as ever), so the phases can change at any time.
pub(crate) fn subpage_on() -> bool {
    SUBPAGE.on()
}

/// Most ranges kept for one page; past it the oldest are let go (a range
/// let go is copied again once: never a byte missed).
pub(crate) const MOST: usize = 32;

/// One page's taken ranges: `(from, to, asking)`, byte offsets in the page.
pub(crate) type Parts = Vec<(u32, u32, u64)>;

/// The pieces of `[lo, hi)` (offsets in one page) to copy now: those not
/// held by a range of `parts` taken at an asking `>= write` (and not 0).
/// Ranges taken before `write` are dropped; `[lo, hi)` is then noted taken
/// at `now` (the caller copies the pieces returned, the rest is held
/// already). `write`: the asking that last found the page written (0:
/// none), `now`: the refresh's asking, `now >= write`.
pub(crate) fn gaps(parts: &mut Parts, lo: u32, hi: u32, write: u64, now: u64) -> Vec<(u32, u32)> {
    parts.retain(|&(_, _, at)| at != 0 && at >= write);
    let mut held: Vec<(u32, u32)> = parts
        .iter()
        .filter(|&&(a, e, _)| a < hi && e > lo)
        .map(|&(a, e, _)| (a.max(lo), e.min(hi)))
        .collect();
    held.sort_unstable();
    let mut out = Vec::new();
    let mut at = lo;
    for (a, e) in held {
        if a > at {
            out.push((at, a));
        }
        at = at.max(e);
    }
    if at < hi {
        out.push((at, hi));
    }
    // Noted at `now`: ranges it covers whole go (theirs is the older note).
    parts.retain(|&(a, e, _)| !(a >= lo && e <= hi));
    parts.push((lo, hi, now));
    if parts.len() > MOST {
        parts.sort_unstable_by_key(|&(_, _, at)| at);
        parts.drain(..parts.len() - MOST);
    }
    out
}

/// The 4 KiB pages of a 64 KiB page.
const KERNEL_PAGE: usize = 4096;

/// For `Shadow::copy_stale`: the title addresses to copy of `[lo, hi)`,
/// which lies in the 64 KiB page at `page_at` (held only in part by the
/// refresh), and the bytes of it held already by `parts`. `bits`: with
/// `VICTORY_DIRTY_4K`, the 4 KiB pages written since the copy last took
/// the page whole (the others are current too); the pieces are cut to
/// them. The whole of `[lo, hi)` is current once the pieces are copied.
pub(crate) fn pieces(
    parts: &mut Parts,
    page_at: usize,
    lo: usize,
    hi: usize,
    write: u64,
    now: u64,
    bits: Option<u16>,
) -> (Vec<(usize, usize)>, usize) {
    debug_assert!(page_at <= lo && lo < hi && hi <= page_at + crate::watch::PAGE);
    let gaps = gaps(
        parts,
        (lo - page_at) as u32,
        (hi - page_at) as u32,
        write,
        now,
    );
    let left: usize = gaps.iter().map(|&(a, e)| (e - a) as usize).sum();
    let held = (hi - lo) - left;
    let mut out = Vec::with_capacity(gaps.len());
    for (a, e) in gaps {
        let (a, e) = (page_at + a as usize, page_at + e as usize);
        let Some(bits) = bits else {
            out.push((a, e));
            continue;
        };
        for k in 0..crate::watch::PAGE / KERNEL_PAGE {
            if bits & (1 << k) == 0 {
                continue;
            }
            let (p, q) = (
                (page_at + k * KERNEL_PAGE).max(a),
                (page_at + (k + 1) * KERNEL_PAGE).min(e),
            );
            if p < q {
                match out.last_mut() {
                    Some(last) if last.1 == p => last.1 = q,
                    _ => out.push((p, q)),
                }
            }
        }
    }
    (out, held)
}

/// Counters since the last line, bytes.
pub(crate) enum K {
    /// Copied on pages the refresh held whole.
    Whole,
    /// Copied on pages held only in part.
    Part,
    /// Memory not write-watched, copied whole.
    Unwatched,
    /// Of all the above, into copies in the card's memory.
    Card,
    /// Left out by `VICTORY_MEM_SUBPAGE` (held already).
    SubpageSaved,
    /// Refreshes (`Shadow::update` with bytes to look at), a count.
    Calls,
}

static COUNT: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

pub(crate) fn add(k: K, n: u64) {
    COUNT[k as usize].fetch_add(n, Relaxed);
}

/// The line, with the frame-rate line (`frames`: frames since the last).
pub(crate) fn line(frames: f64) {
    let c: Vec<f64> = COUNT.iter().map(|a| a.swap(0, Relaxed) as f64).collect();
    let mb = |x: f64| x / frames / 1e6;
    tracing::info!(
        "refresh bytes per frame by kind: {:.1} MB on pages held whole, {:.1} MB on pages held in part, {:.1} MB of memory not write-watched; {:.1} MB of these into copies on the card; {:.0} refreshes; {:.1} MB skipped as already taken (VICTORY_MEM_SUBPAGE {})",
        mb(c[K::Whole as usize]),
        mb(c[K::Part as usize]),
        mb(c[K::Unwatched as usize]),
        mb(c[K::Card as usize]),
        c[K::Calls as usize] / frames,
        mb(c[K::SubpageSaved as usize]),
        said(&SUBPAGE),
    );
}

/// `ON`, `off` or the phase of `alt`, for a stats line.
fn said(s: &crate::alt::Switch) -> &'static str {
    match s.mode() {
        0 => "off",
        1 => "ON",
        _ if s.on() => "alt, ON now",
        _ => "alt, off now",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 65536;

    /// A model of one page: title bytes, the copy, the page's last write
    /// (as the write watch numbers askings). Refreshes of random ranges
    /// with `gaps` must leave the copy equal to title memory over the
    /// range every time, as copying the range whole does, and save bytes.
    #[test]
    fn gaps_refresh_is_byte_identical_to_copying_whole() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _case in 0..200 {
            let mut title = vec![0u8; PAGE];
            let mut copy = vec![0xAAu8; PAGE];
            let mut parts = Parts::new();
            let (mut asking, mut write) = (1u64, 0u64);
            let mut saved = 0i64;
            for _step in 0..300 {
                if rnd(4) == 0 {
                    // A title write: some bytes change; the next asking
                    // finds the page written.
                    let at = rnd(PAGE as u64) as usize;
                    let n = (1 + rnd(512) as usize).min(PAGE - at);
                    for b in &mut title[at..at + n] {
                        *b = b.wrapping_add(1 + rnd(255) as u8);
                    }
                    write = asking + 1;
                }
                // A refresh: a new asking, then the gaps of its range.
                asking = asking.max(write) + 1;
                let lo = rnd(PAGE as u64) as u32;
                let hi = (lo + 1 + rnd(24576) as u32).min(PAGE as u32);
                for (a, e) in gaps(&mut parts, lo, hi, write, asking) {
                    saved -= i64::from(e - a);
                    copy[a as usize..e as usize].copy_from_slice(&title[a as usize..e as usize]);
                }
                saved += i64::from(hi - lo);
                assert_eq!(
                    copy[lo as usize..hi as usize],
                    title[lo as usize..hi as usize]
                );
                assert!(parts.len() <= MOST);
            }
            assert!(saved > 0);
        }
    }

    #[test]
    fn a_write_after_the_note_takes_the_range_again() {
        let mut parts = Parts::new();
        assert_eq!(gaps(&mut parts, 100, 200, 0, 5), [(100, 200)]);
        assert!(gaps(&mut parts, 120, 180, 0, 6).is_empty());
        assert_eq!(gaps(&mut parts, 150, 250, 0, 7), [(200, 250)]);
        // Written after asking 7: all again.
        assert_eq!(gaps(&mut parts, 100, 250, 8, 9), [(100, 250)]);
        // Written at the asking of the note: still held (as the whole
        // page's rule: `write > copied` is stale).
        assert!(gaps(&mut parts, 100, 250, 9, 10).is_empty());
    }

    /// Overlapping notes: pieces between and around them, none twice.
    #[test]
    fn overlapping_notes_leave_only_the_holes_between_them() {
        let mut parts = Parts::new();
        gaps(&mut parts, 0, 100, 0, 2);
        gaps(&mut parts, 80, 300, 0, 3);
        gaps(&mut parts, 500, 600, 0, 4);
        assert_eq!(gaps(&mut parts, 50, 700, 0, 5), [(300, 500), (600, 700)]);
        // The note of [50, 700) covered the three before it but [0, 100)
        // only in part: that one stays.
        assert_eq!(parts.len(), 2);
        assert!(gaps(&mut parts, 0, 700, 0, 6).is_empty());
    }

    /// A range up to the page's last byte, and one at its first.
    #[test]
    fn the_page_boundaries_are_held_like_any_other_byte() {
        let mut parts = Parts::new();
        let end = PAGE as u32;
        assert_eq!(gaps(&mut parts, end - 16, end, 0, 2), [(end - 16, end)]);
        assert_eq!(
            gaps(&mut parts, end - 32, end, 0, 3),
            [(end - 32, end - 16)]
        );
        assert_eq!(gaps(&mut parts, 0, 1, 0, 4), [(0, 1)]);
        assert!(gaps(&mut parts, 0, 1, 0, 5).is_empty());
    }

    /// Past [`MOST`] notes the oldest go: copied again, never missed.
    #[test]
    fn notes_past_the_most_are_let_go_oldest_first() {
        let mut parts = Parts::new();
        for k in 0..(MOST as u32 + 4) {
            gaps(&mut parts, k * 100, k * 100 + 10, 0, 2 + u64::from(k));
        }
        assert_eq!(parts.len(), MOST);
        // The first four are gone.
        assert_eq!(gaps(&mut parts, 0, 10, 0, 100), [(0, 10)]);
        assert!(gaps(&mut parts, 3500, 3510, 0, 101).is_empty());
    }

    /// The same rule on the kernel's write watch, as `Shadow::copy_stale`
    /// takes it (`crate::watch::writes`): title bytes written after a range
    /// was taken are copied again at the next refresh, every time, however
    /// the refreshes and the writes interleave; and bytes not written are
    /// not copied twice.
    #[test]
    fn real_write_watch_never_leaves_a_byte_written_after_its_taking() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        const PAGES: usize = 2;
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                PAGES * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        // SAFETY: our allocation, committed read-write, nothing else uses it.
        let title = unsafe { std::slice::from_raw_parts_mut(p as *mut u8, PAGES * PAGE) };
        let mut copy = vec![0xAAu8; PAGES * PAGE];
        let mut parts: std::collections::HashMap<usize, Parts> = Default::default();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let (mut asked, mut copied) = (0usize, 0usize);
        for step in 0..3000 {
            if rnd(40) == 0 {
                let at = rnd((PAGES * PAGE) as u64) as usize;
                let n = (1 + rnd(64) as usize).min(PAGES * PAGE - at);
                for b in &mut title[at..at + n] {
                    *b = b.wrapping_add(1 + rnd(255) as u8);
                }
            }
            // A refresh of part of one page (one of four places in it, as
            // constant buffers are refreshed, a little longer or shorter
            // each time, up to the page's last byte): an asking, then the
            // pieces.
            let page = rnd(PAGES as u64) as usize;
            let lo = rnd(4) as usize * (PAGE / 4) + rnd(64) as usize;
            let hi = (lo + 1 + rnd(PAGE as u64 / 4) as usize).min(PAGE);
            if lo == 0 && hi == PAGE {
                continue;
            }
            let (from, to) = (p + page * PAGE + lo, p + page * PAGE + hi);
            let (now, written) = crate::watch::writes(from, to, false).expect("watched");
            let write = written
                .iter()
                .find(|&&(q, _)| q == (p + page * PAGE) / crate::watch::PAGE)
                .map_or(0, |&(_, w)| w);
            let kept = parts.entry(page).or_default();
            let (pieces, held) = pieces(kept, p + page * PAGE, from, to, write, now, None);
            for (a, e) in pieces {
                copy[a - p..e - p].copy_from_slice(&title[a - p..e - p]);
                copied += e - a;
            }
            asked += to - from;
            assert!(held <= to - from);
            assert_eq!(
                copy[from - p..to - p],
                title[from - p..to - p],
                "refresh {step}: a byte written after it was taken was left stale"
            );
        }
        assert!(
            copied < asked / 2,
            "the notes save bytes: {copied} of {asked} copied"
        );
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// With the 4 KiB pages: only the written ones among the gaps.
    #[test]
    fn pieces_are_cut_to_the_written_kernel_pages() {
        let base = 0x10_0000usize;
        let mut parts = Parts::new();
        // Nothing held yet; 4 KiB pages 1 and 2 written: [4096, 12288).
        let (p, held) = pieces(
            &mut parts,
            base,
            base + 100,
            base + 20_000,
            3,
            4,
            Some(0b110),
        );
        assert_eq!(p, [(base + 4096, base + 12288)]);
        assert_eq!(held, 0);
        // Written again (asking 5): all of the range is a gap; without the
        // 4 KiB pages it is copied whole.
        let (p, held) = pieces(&mut parts, base, base + 100, base + 20_000, 5, 6, None);
        assert_eq!(p, [(base + 100, base + 20_000)]);
        assert_eq!(held, 0);
        let (p, held) = pieces(&mut parts, base, base + 50, base + 20_000, 5, 7, None);
        assert_eq!(p, [(base + 50, base + 100)]);
        assert_eq!(held, 20_000 - 100);
    }
}
