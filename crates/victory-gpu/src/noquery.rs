//! Sparse reads of title memory by the runtime's own record of its
//! mappings (`VICTORY_COPY_NOQUERY`), and what sparse reads cost.
//!
//! A copy of title memory into host memory is one kernel read
//! (`crate::host::copy_mapped`); when that fails (a page of the range is
//! not there: the title streams its memory in and out of placeholders, one
//! 64 KiB view per page) the range is read region by region, each region
//! found by a `VirtualQuery` (`crate::host::read_sparse`): one question per
//! view and per hole between views, in the submitting thread. Under Wine
//! such a question walks the process's views and, in a placeholder, its
//! pages.
//!
//! The runtime knows those regions: it reserves the title's placeholders
//! (`victory-hle` xmem, [`title_memory_placeholder`]) and maps and unmaps
//! every view in them (`crate::inplace`, told by
//! [`crate::title_memory_view`] and [`crate::title_memory_remapped`]).
//! `VICTORY_COPY_NOQUERY=1` (off by default; `VICTORY_COPY_NOQUERY=alt`:
//! twenty seconds off, twenty on, in turn) reads a range that lies in one
//! placeholder by that record instead: the views as one kernel read per
//! run of neighbouring views, zeros for the holes, no question to Windows.
//!
//! Exact as the questions were: the record is read with the runtime's
//! mapping changes held off ([`crate::watch::title_memory_reading_now`];
//! a change under way: the questions, as before); a view the record holds
//! that is not there fails its kernel read and is read piece by piece as
//! before; a hole is looked at once through the kernel (its first byte),
//! and one that reads is memory the record does not know (the title
//! released the reservation itself and something else lives there now):
//! that hole is read with the questions, and the reservation forgotten.
//!
//! Ported by hand from ScarlettPort's `regions.rs` (`noquery`,
//! `sparse_runs`; GPL-3.0-or-later, credit to the ScarlettPort authors).
//! There the record is a full map of the title's memory and the switch
//! also waits out busy chunks of the copies it makes; this tree copies
//! through the kernel and asks Windows only after a failed read, so only
//! that part is taken, on this tree's record of views.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static NOQUERY: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_COPY_NOQUERY",
    "sparse reads of title memory take their regions from the runtime's record of its mappings, not from VirtualQuery",
);

/// `VICTORY_COPY_NOQUERY`: 1 on, `alt` in turn.
pub(crate) fn on() -> bool {
    #[cfg(test)]
    if FORCE.load(Relaxed) {
        return true;
    }
    NOQUERY.on()
}

/// The title's placeholder reservations as the runtime made them: start,
/// bytes. One the title releases itself stays until one made over it
/// replaces it, or a hole in it is found readable.
static PLACEHOLDERS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

fn placeholders() -> std::sync::MutexGuard<'static, BTreeMap<usize, usize>> {
    PLACEHOLDERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The runtime reserved `[at, at + len)` as a placeholder for the title's
/// physical pages: holes until it maps views there. Reservations it
/// overlaps are gone (memory is in one reservation at a time).
pub fn title_memory_placeholder(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let mut p = placeholders();
    note(&mut p, at, len);
}

/// Whether `[at, at + len)` lies in a placeholder reservation the runtime
/// made ([`title_memory_placeholder`]): the stand-in for the kernel's
/// answer where that is not given (Wine).
pub(crate) fn recorded_placeholder(at: usize, len: usize) -> bool {
    within(&placeholders(), at, at.saturating_add(len)).is_some()
}

fn note(p: &mut BTreeMap<usize, usize>, at: usize, len: usize) {
    let end = at.saturating_add(len);
    let first = p
        .range(..at)
        .next_back()
        .filter(|&(&s, &l)| s + l > at)
        .map(|(&s, _)| s);
    let gone: Vec<usize> = first
        .into_iter()
        .chain(p.range(at..end).map(|(&s, _)| s))
        .collect();
    for s in gone {
        p.remove(&s);
    }
    p.insert(at, len);
}

/// The reservation that holds all of `[at, end)`, if one does.
fn within(p: &BTreeMap<usize, usize>, at: usize, end: usize) -> Option<(usize, usize)> {
    p.range(..=at)
        .next_back()
        .map(|(&s, &l)| (s, l))
        .filter(|&(s, l)| end <= s + l)
}

/// The pieces of `[start, end)` by the record: `(from, to, mapped)` in
/// order, each a run of neighbouring views (`mapped`) or a hole between
/// them. `views`: the views that overlap the range, in address order, as
/// `(address, bytes)`.
fn runs(start: usize, end: usize, views: &[(usize, usize)]) -> Vec<(usize, usize, bool)> {
    let mut out: Vec<(usize, usize, bool)> = Vec::new();
    let mut at = start;
    for &(base, bytes) in views {
        let (a, e) = (base.max(at), base.saturating_add(bytes).min(end));
        if a >= e {
            continue;
        }
        if a > at {
            out.push((at, a, false));
        }
        match out.last_mut() {
            Some(last) if last.2 && last.1 == a => last.1 = e,
            _ => out.push((a, e, true)),
        }
        at = e;
    }
    if at < end {
        out.push((at, end, false));
    }
    out
}

/// What [`read_sparse`] is given of `crate::host`: the kernel's read of
/// title memory into host memory (false if a page is not there), the
/// piece-by-piece read that fills zeros where pages are not there, and the
/// read region by region by `VirtualQuery` (as before the switch).
pub(crate) struct Reads<'a> {
    pub(crate) read: &'a dyn Fn(*const u8, *mut u8, usize) -> bool,
    pub(crate) pieces: &'a dyn Fn(*const u8, *mut u8, usize),
    pub(crate) queried: &'a dyn Fn(*const u8, *mut u8, usize),
}

/// With [`on`]: reads title memory `[src, src + len)` into host memory at
/// `dst` by the record; false with nothing done if the record cannot say
/// (the caller asks Windows, as before).
pub(crate) fn read_sparse(src: *const u8, dst: *mut u8, len: usize, reads: &Reads) -> bool {
    if len == 0 || !on() {
        return false;
    }
    let (start, end) = (src as usize, (src as usize).saturating_add(len));
    if placeholders().is_empty() {
        add(N::Unknown, 1);
        return false;
    }
    // No mapping of title memory changes while the record is read and its
    // views are read (a change under way: the questions, as before).
    let Some(_still) = crate::watch::title_memory_reading_now() else {
        add(N::Busy, 1);
        return false;
    };
    let Some(reservation) = within(&placeholders(), start, end) else {
        add(N::Unknown, 1);
        return false;
    };
    let views = crate::inplace::views_between(start, end);
    let to = |a: usize| dst.wrapping_add(a - start);
    let mut forget = false;
    for (a, e, mapped) in runs(start, end, &views) {
        if mapped {
            add(N::Avoided, 1);
            add(N::Reads, 1);
            if !(reads.read)(a as *const u8, to(a), e - a) {
                // A view the record holds that is not all there: piece by
                // piece, zeros where it is not (as after a question).
                add(N::ViewsFailed, 1);
                (reads.pieces)(a as *const u8, to(a), e - a);
            }
            add(N::MappedBytes, (e - a) as u64);
            continue;
        }
        // A hole: one look at its first byte.
        let mut byte = 0u8;
        add(N::Probes, 1);
        if (reads.read)(a as *const u8, &raw mut byte, 1) {
            add(N::Stale, 1);
            forget = true;
            (reads.queried)(a as *const u8, to(a), e - a);
            continue;
        }
        // SAFETY: inside the caller's host destination of `len` bytes.
        unsafe { to(a).write_bytes(0, e - a) };
        add(N::Avoided, 1);
        add(N::HoleBytes, (e - a) as u64);
    }
    if forget {
        placeholders().remove(&reservation.0);
    }
    add(N::Done, 1);
    true
}

/// Counters since the last line.
#[derive(Clone, Copy)]
pub(crate) enum N {
    /// Sparse reads (after a failed bulk read), `VirtualQuery` calls they
    /// made and the microseconds of those reads (all, either way).
    Sparse,
    Queries,
    SparseUs,
    /// Sparse reads done by the record; regions it answered (a question to
    /// Windows each, before); kernel reads it made of runs of views.
    Done,
    Avoided,
    Reads,
    /// Bytes read of views, written as zeros for holes.
    MappedBytes,
    HoleBytes,
    /// Holes looked at, found readable (memory the record did not know);
    /// views the record held that did not read whole.
    Probes,
    Stale,
    ViewsFailed,
    /// Sparse reads left to the questions: a mapping change was under way;
    /// the range is not in one known placeholder.
    Busy,
    Unknown,
}

const COUNTERS: usize = N::Unknown as usize + 1;
static COUNT: [AtomicU64; COUNTERS] = [const { AtomicU64::new(0) }; COUNTERS];

pub(crate) fn add(n: N, v: u64) {
    COUNT[n as usize].fetch_add(v, Relaxed);
}

/// With the frame line: per frame over `frames`.
pub(crate) fn line(frames: f64) {
    let c: Vec<f64> = COUNT
        .iter()
        .map(|a| a.swap(0, Relaxed) as f64 / frames)
        .collect();
    let g = |n: N| c[n as usize];
    // Nothing to say: no sparse read and the switch not set.
    if g(N::Sparse) == 0.0 && !NOQUERY.set() {
        return;
    }
    tracing::info!(
        "sparse reads of title memory per frame (a bulk read met a missing page): {:.1} reads {:.2} ms, {:.1} VirtualQuery calls; by the mapping record: {:.1} reads, {:.1} queries avoided, {:.1} kernel reads of views {:.2} MB, {:.2} MB holes as zeros, {:.1} holes looked at, {:.2} found readable, {:.2} views not all there; {:.2} left to the questions for a mapping change under way, {:.2} for memory not in a known placeholder (VICTORY_COPY_NOQUERY {})",
        g(N::Sparse),
        g(N::SparseUs) / 1e3,
        g(N::Queries),
        g(N::Done),
        g(N::Avoided),
        g(N::Reads),
        g(N::MappedBytes) / 1e6,
        g(N::HoleBytes) / 1e6,
        g(N::Probes),
        g(N::Stale),
        g(N::ViewsFailed),
        g(N::Busy),
        g(N::Unknown),
        said(&NOQUERY),
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

/// Tests: the switch on whatever the environment says.
#[cfg(test)]
static FORCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
mod tests {
    use super::*;

    const K64: usize = 64 << 10;

    #[test]
    fn neighbouring_views_are_one_run_and_holes_lie_between() {
        let b = 0x1000_0000usize;
        let views = [
            (b, K64),
            (b + K64, K64),
            (b + 3 * K64, K64),
            (b + 4 * K64, 2 * K64),
        ];
        assert_eq!(
            runs(b + 100, b + 7 * K64, &views),
            [
                (b + 100, b + 2 * K64, true),
                (b + 2 * K64, b + 3 * K64, false),
                (b + 3 * K64, b + 6 * K64, true),
                (b + 6 * K64, b + 7 * K64, false)
            ]
        );
        // No view: one hole; a view past the range: nothing of it.
        assert_eq!(runs(b, b + K64, &[]), [(b, b + K64, false)]);
        assert_eq!(
            runs(b, b + K64, &[(b - K64, K64), (b + K64, K64)]),
            [(b, b + K64, false)]
        );
        // A view holding the whole range.
        assert_eq!(runs(b + 5, b + 9, &[(b, K64)]), [(b + 5, b + 9, true)]);
    }

    #[test]
    fn a_new_reservation_replaces_those_it_overlaps() {
        let mut p = BTreeMap::new();
        note(&mut p, 0x1_0000, 0x4_0000);
        note(&mut p, 0x8_0000, 0x1_0000);
        assert_eq!(within(&p, 0x2_0000, 0x5_0000), Some((0x1_0000, 0x4_0000)));
        assert_eq!(within(&p, 0x2_0000, 0x5_0001), None);
        note(&mut p, 0x3_0000, 0x6_0000);
        assert_eq!(p.into_iter().collect::<Vec<_>>(), [(0x3_0000, 0x6_0000)]);
    }

    /// On real memory: a placeholder with views of a section in it, read by
    /// the record, gives what the questions give: the views' bytes, zeros
    /// for the holes; and a hole that is in fact memory (a reservation the
    /// record still holds after the title let it go) is read, not zeroed.
    #[test]
    fn the_record_reads_what_the_questions_read() {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE,
            MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER,
            MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS, PAGE_READWRITE,
            UnmapViewOfFile2, VirtualAlloc2, VirtualFree,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        const PAGES: usize = 6;
        // SAFETY: a placeholder of our own, a section of our own, views of
        // it in pages 1, 2 and 4; all let go at the end.
        let (base, section) = unsafe {
            let base = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                PAGES * K64,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(base, 0);
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (3 * K64) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            for (k, page) in [1usize, 2, 4].into_iter().enumerate() {
                let at = base + page * K64;
                // Split the page off its placeholder (fails harmlessly at
                // the end, where it is already one).
                VirtualFree(at as *mut _, K64, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
                let view = MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    at as *const _,
                    (k * K64) as u64,
                    K64,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                );
                assert_eq!(view.Value as usize, at);
                std::ptr::write_bytes(at as *mut u8, 0x10 + k as u8, K64);
            }
            (base, section)
        };
        title_memory_placeholder(base, PAGES * K64);
        for (k, page) in [1usize, 2, 4].into_iter().enumerate() {
            crate::inplace::title_memory_view(
                base + page * K64,
                K64,
                section as usize,
                (k * K64) as u64,
            );
        }
        // SAFETY: `d` is `n` bytes of the test's own buffers.
        let kernel = |s: *const u8, d: *mut u8, n: usize| {
            crate::host::read_into(s, unsafe { std::slice::from_raw_parts_mut(d, n) })
        };
        let pieces =
            |_: *const u8, _: *mut u8, _: usize| panic!("no view of the record is missing");
        let queried = |_: *const u8, _: *mut u8, _: usize| panic!("no hole is memory");
        let reads = Reads {
            read: &kernel,
            pieces: &pieces,
            queried: &queried,
        };
        let mut out = vec![0xEEu8; PAGES * K64 - 200];
        // (The switch is read from the environment: set on for the test.)
        assert!(by_record(base + 100, &mut out, &reads));
        let want = |at: usize| match (at - base) / K64 {
            1 => 0x10,
            2 => 0x11,
            4 => 0x12,
            _ => 0,
        };
        for (i, &b) in out.iter().enumerate() {
            assert_eq!(b, want(base + 100 + i), "byte {i}");
        }
        // Page 5's hole made memory behind the record's back: read.
        // SAFETY: the last page of our placeholder, made private memory.
        unsafe {
            VirtualFree(
                (base + 5 * K64) as *mut _,
                K64,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            );
            let at = VirtualAlloc2(
                GetCurrentProcess(),
                (base + 5 * K64) as *const _,
                K64,
                MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(at as usize, base + 5 * K64);
            std::ptr::write_bytes(at as *mut u8, 0x77, K64);
        }
        let asked = std::cell::Cell::new(0);
        let queried = |s: *const u8, d: *mut u8, n: usize| {
            asked.set(asked.get() + 1);
            assert!(kernel(s, d, n));
        };
        let reads = Reads {
            read: &kernel,
            pieces: &pieces,
            queried: &queried,
        };
        let mut out = vec![0xEEu8; 2 * K64];
        assert!(by_record(base + 4 * K64, &mut out, &reads));
        assert_eq!(asked.get(), 1);
        assert!(out[..K64].iter().all(|&b| b == 0x12) && out[K64..].iter().all(|&b| b == 0x77));
        // The reservation is forgotten: the record no longer answers there.
        assert!(!by_record(base + 4 * K64, &mut out, &reads));
        crate::inplace::views_gone(base, PAGES * K64);
        // SAFETY: what was made above.
        unsafe {
            for page in [1usize, 2, 4] {
                UnmapViewOfFile2(
                    GetCurrentProcess(),
                    MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: (base + page * K64) as *mut _,
                    },
                    0,
                );
            }
            for page in [0usize, 3, 5] {
                VirtualFree((base + page * K64) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
    }

    /// [`read_sparse`] with the switch on whatever the environment says.
    fn by_record(at: usize, out: &mut [u8], reads: &Reads) -> bool {
        FORCE.store(true, Relaxed);
        let done = read_sparse(at as *const u8, out.as_mut_ptr(), out.len(), reads);
        FORCE.store(false, Relaxed);
        done
    }
}
