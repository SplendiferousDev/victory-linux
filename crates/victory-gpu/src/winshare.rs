//! Windows of title memory outside buffers that fit their asks and are
//! shared between them (`VICTORY_WINDOW_SHARE=1`; off by default, which
//! leaves every window as it was).
//!
//! A window (`crate::shadow`, `loose`) is a host buffer in the PC's memory
//! that holds a range of title memory for shaders. The driver pins it, all
//! of it, from the moment it is made, so every byte of a window nothing
//! reads is a byte less for the title. As they were, windows of asks with a
//! length are named by the ask alone, cut on a grid (`VICTORY_WINDOW_KB`):
//! an ask of 4 KiB holds a whole cell, an ask across a grid line holds two
//! cells that the cells' own windows hold as well, and an ask inside a
//! window made for another range (a longer ask, a root view's tail) gets a
//! window of its own all the same. A run's late memory line had 1,495 MB in
//! 4,460 such windows, 826 MB of it outside anything asked for, and 806 MB
//! of title memory held more than once.
//!
//! With the switch on, an ask with a length
//! - takes the OLDEST window there is that holds all of it and is no longer
//!   than the grid cell it would have had (or twice its own pages, for an
//!   ask longer than that; any length for a draw's own vertices, indices
//!   and arguments, which no slot shows): one copy for both, and no window
//!   made;
//! - else gets a window fitted to it ([`fit`]): its 64 KiB pages and no
//!   more, or, for a short ask across a page line, as many pages from the
//!   ask's own start (the host's allocator gives a buffer whole 64 KiB
//!   pages whatever it asks for, so nothing finer saves anything).
//!
//! What a window must keep giving, and why this does:
//! - A descriptor slot shows the buffer its view found when the slot was
//!   made, and every pass and draw that brings the slot's copy up to date
//!   finds the copy again by the same ask. Both must find the same window
//!   for as long as the slot lives. The oldest window that holds an ask is
//!   that: a window made later is younger, a window goes only when no slot
//!   and no list holds it (the idle sweep's rule, unchanged), and the limit
//!   on a taken window's length is the ask's own. So the answer to an ask
//!   changes only when its window has gone, which no slot can outlive.
//!   That needs nothing remembered: [`Index::memo`] only saves the search.
//!   (The window made for an ask is one it may always take, whatever its
//!   length. And a view made on a copy of its own bytes, because it does
//!   not begin on an element of its window, keeps that window:
//!   `Shadow::lean_on`.)
//! - `VICTORY_WINDOW_KB` no longer cuts the windows of asks with a length
//!   in this mode; it is the length a short ask may take of another's
//!   window (4096: a root view's whole tail).
//! - The switch is read once. Flipped in a run, an ask would name the grid's
//!   window in one phase and a fitted one in the other, and a slot made in
//!   one would have another copy brought up to date in the other: the same
//!   reason `VICTORY_WINDOW_KB` cannot change in a run.
//! - A thread's remembered asks (`VICTORY_LOOKUP_CACHE`) are from one count
//!   of table changes, and every window made or let go is counted as
//!   before.
//! - Nothing is let go here: windows leave through the idle sweep and its
//!   hold, as before.
//! - No work is added to a draw: the search runs only for an ask that
//!   reaches the tables (one in twelve with the lookup cache) and has no
//!   remembered answer, under the windows' lock that ask takes anyway.
//!
//! A taken window may begin anywhere on 256 bytes below the ask's address,
//! so an address keeps its offset from 16 and from 256 bytes in it, as in a
//! grid window (raw views and constant buffer views need those).
//!
//! shortcut: windows made before a longer one over the same memory stay
//! (their slots show them); the memory they share is held twice until their
//! slots go. Joining them needs slots pointed at the longer window, which
//! is not done here. Root views (asks without a length) keep their windows.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::fasthash::FastMap;

/// The host gives a buffer whole pages of this size.
const PAGE: u64 = 64 << 10;
/// The index keeps the windows by the cells of this size they lie in.
const CELL: u64 = 4 << 20;
/// The most remembered answers before the index starts again.
const MEMO_MOST: usize = 1 << 17;
/// A window begun at its ask's own start is at most this long.
const SLIDE_MOST: u64 = 4 * PAGE;

/// `VICTORY_WINDOW_SHARE=1` (read once: see the module).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_WINDOW_SHARE").is_ok_and(|v| v.trim() == "1");
        if on {
            tracing::info!(
                "windows of title memory outside buffers fit their asks and are shared (VICTORY_WINDOW_SHARE=1): an ask with a \
                 length takes the oldest window that holds it, or gets one of its own pages, cut on {} KiB \
                 (VICTORY_WINDOW_SHARE_KB)",
                unit() >> 10
            );
        }
        on
    })
}

/// What fitted windows are cut on, in bytes: 64 KiB unless
/// `VICTORY_WINDOW_SHARE_KB` names a larger power of two up to 4096. A
/// larger unit makes fewer, longer windows (fewer copies to bring up to
/// date each frame) that hold more memory nothing asked for.
pub(crate) fn unit() -> u64 {
    static UNIT: OnceLock<u64> = OnceLock::new();
    *UNIT.get_or_init(|| unit_of(crate::tight::number(std::env::var("VICTORY_WINDOW_SHARE_KB").ok().as_deref())))
}

/// The unit in bytes for a setting in KiB.
fn unit_of(kb: Option<u64>) -> u64 {
    match kb {
        Some(kb) if kb.is_power_of_two() && (64..=4096).contains(&kb) => kb << 10,
        _ => PAGE,
    }
}

/// The window (start, bytes) made for an ask of `[address, end)` that no
/// window holds: the ask's units, whole. On the 64 KiB unit a short ask
/// that lies across a page line begins at its own start (on 256 bytes) if
/// that takes a page less: 8 KiB across a line are one page, not two.
pub(crate) fn fit(address: u64, end: u64, unit: u64) -> (u64, u64) {
    let low = address & !(unit - 1);
    let whole = end.next_multiple_of(unit) - low;
    if unit == PAGE {
        let own = address & !255;
        let slid = (end - own).next_multiple_of(PAGE);
        if slid < whole && slid <= SLIDE_MOST {
            return (own, slid);
        }
    }
    (low, whole)
}

/// The longest window an ask of `[address, end)` may take: the grid cell
/// (`grid` bytes, `VICTORY_WINDOW_KB`) the ask would have had, or twice its
/// own pages if that is more. A pass over the heap brings each copy up to
/// date from its lowest view to its highest: a short ask in a window of
/// megabytes would stretch that over everything between. A rule of the ask
/// and the window alone, so the answer to an ask stays what it was. (The
/// window [`fit`] makes for an ask is one that ask may take whatever its
/// length: [`Index::holder`].)
pub(crate) fn most(address: u64, end: u64, grid: u64) -> u64 {
    let pages = end.next_multiple_of(PAGE) - (address & !(PAGE - 1));
    grid.max(2 * pages)
}

/// The longest window an ask may take in this run: [`most`] for an ask
/// whose window a descriptor slot may show; any length for one that is done
/// with its copy when its list has run (`any`: `crate::shadow::find_any`),
/// which brings up to date just the range it bound.
pub(crate) fn most_now(address: u64, end: u64, grid: u64, any: bool) -> u64 {
    if any { u64::MAX } else { most(address, end, grid) }
}

/// What [`Index::choose`] says of an ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    /// The window (start, bytes) that is there and holds it.
    There(u64, u64),
    /// None holds it: the window to make.
    Make(u64, u64),
}

impl Choice {
    /// The window's (start, bytes).
    pub(crate) fn key(self) -> (u64, u64) {
        match self {
            Choice::There(start, len) | Choice::Make(start, len) => (start, len),
        }
    }
}

/// The windows there are (every window of the table, root views' too), for
/// the search of the oldest that holds an ask. Kept in step with the table
/// of windows under that table's lock.
#[derive(Default)]
pub(crate) struct Index {
    /// By (start, bytes): the asks other than its first that took it.
    windows: FastMap<(u64, u64), u32>,
    /// Per cell of [`CELL`] bytes (its number): the windows that lie in it,
    /// as (start, bytes, number). The number is the copy's own
    /// (`Shadow::id`): a window made later has a higher one.
    cells: FastMap<u64, Vec<(u64, u64, u64)>>,
    /// What asks `[address, end)` found (apart for those that may take a
    /// window of any length). Only a shorter way to the same answer
    /// ([`Index::holder`]): emptied or dropped at any time, nothing
    /// changes. An answer goes with its window.
    memo: FastMap<(u64, u64, bool), (u64, u64)>,
}

/// The cells `[start, start + len)` lies in.
fn cells_of(start: u64, len: u64) -> std::ops::RangeInclusive<u64> {
    start / CELL..=(start + len.max(1) - 1) / CELL
}

impl Index {
    /// A window was put into the table.
    pub(crate) fn insert(&mut self, start: u64, len: u64, id: u64) {
        if self.windows.contains_key(&(start, len)) {
            self.remove(start, len);
        }
        self.windows.insert((start, len), 0);
        for cell in cells_of(start, len) {
            self.cells.entry(cell).or_default().push((start, len, id));
        }
    }

    /// A window left the table: no ask finds it from now on.
    pub(crate) fn remove(&mut self, start: u64, len: u64) {
        if self.windows.remove(&(start, len)).is_none() {
            return;
        }
        for cell in cells_of(start, len) {
            if let Some(there) = self.cells.get_mut(&cell) {
                there.retain(|&(s, l, _)| (s, l) != (start, len));
                if there.is_empty() {
                    self.cells.remove(&cell);
                }
            }
        }
        self.memo.retain(|_, window| *window != (start, len));
    }

    /// The oldest window that holds `[address, end)` and is no longer than
    /// `most` bytes, or is `fitted`, the window the ask itself would have
    /// made (so that the window made for an ask is always one it may take:
    /// nothing that could take its place is older).
    pub(crate) fn holder(&self, address: u64, end: u64, most: u64, fitted: (u64, u64)) -> Option<(u64, u64)> {
        // A window that holds the ask lies in the cell of its first byte.
        self.cells
            .get(&(address / CELL))?
            .iter()
            .filter(|&&(start, len, _)| start <= address && start + len >= end && (len <= most || (start, len) == fitted))
            .min_by_key(|&&(_, _, id)| id)
            .map(|&(start, len, _)| (start, len))
    }

    /// The window of an ask of `[address, end)` that may take one of up to
    /// `most` bytes: the one that is there for it ([`Index::holder`]), or
    /// the one to make ([`fit`]), which the caller makes and tells of
    /// ([`Index::made`]).
    pub(crate) fn choose(&mut self, address: u64, end: u64, unit: u64, most: u64) -> Choice {
        if let Some(&(start, len)) = self.memo.get(&(address, end, most == u64::MAX)) {
            return Choice::There(start, len);
        }
        let fitted = fit(address, end, unit);
        match self.holder(address, end, most, fitted) {
            Some((start, len)) => {
                if let Some(took) = self.windows.get_mut(&(start, len)) {
                    *took = took.saturating_add(1);
                }
                TOOK[0].fetch_add(1, Relaxed);
                TOOK[1].fetch_add(end - address, Relaxed);
                self.remember(address, end, most == u64::MAX, (start, len));
                Choice::There(start, len)
            }
            None => Choice::Make(fitted.0, fitted.1),
        }
    }

    /// The window `[start, start + len)` numbered `id` was made for the ask
    /// `[address, end)` (one that may take a window of any length: `any`)
    /// and put into the table.
    pub(crate) fn made(&mut self, address: u64, end: u64, any: bool, start: u64, len: u64, id: u64) {
        MADE[0].fetch_add(1, Relaxed);
        MADE[1].fetch_add(len, Relaxed);
        if start % PAGE != 0 {
            MADE[2].fetch_add(1, Relaxed);
        }
        MADE[3].fetch_add(self.overlap(start, len), Relaxed);
        self.insert(start, len, id);
        // (What a search would find: nothing older held the ask, or it
        // would not have been made.)
        self.remember(address, end, any, (start, len));
    }

    fn remember(&mut self, address: u64, end: u64, any: bool, window: (u64, u64)) {
        if self.memo.len() >= MEMO_MOST {
            self.memo.clear();
        }
        self.memo.insert((address, end, any), window);
    }

    /// The bytes of `[start, start + len)` that windows of the index lie
    /// over, each byte once: title memory a window made there holds again.
    pub(crate) fn overlap(&self, start: u64, len: u64) -> u64 {
        let end = start + len;
        let mut pieces: Vec<(u64, u64)> = cells_of(start, len)
            .filter_map(|cell| self.cells.get(&cell))
            .flatten()
            .filter(|&&(s, l, _)| s < end && s + l > start)
            .map(|&(s, l, _)| (s.max(start), (s + l).min(end)))
            .collect();
        pieces.sort_unstable();
        let (mut bytes, mut reached) = (0, start);
        for (from, to) in pieces {
            if to > reached {
                bytes += to - from.max(reached);
                reached = to;
            }
        }
        bytes
    }

    /// How many asks other than its first took the window (0 for one the
    /// index does not know).
    pub(crate) fn took(&self, start: u64, len: u64) -> u32 {
        self.windows.get(&(start, len)).copied().unwrap_or(0)
    }

    /// Windows known (tests).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.windows.len()
    }
}

/// Since the start: windows made to fit, their bytes, those begun at their
/// ask's own start, and the bytes of them that lay over windows there
/// already when they were made.
static MADE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Since the start: asks that found their window by the search (each ask
/// once until its window goes or the remembered answers start again), and
/// their bytes.
static TOOK: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The index. Locked inside the lock of the table of windows, never the
/// other way round.
pub(crate) fn index() -> MutexGuard<'static, Index> {
    static INDEX: OnceLock<Mutex<Index>> = OnceLock::new();
    crate::lockmeter::lock("ws:index", INDEX.get_or_init(Mutex::default))
}

/// Of the windows alive in the PC's memory, for the line: those asked for
/// with a length that more than one ask took and their bytes; the bytes
/// the grid's cells from each such window's lowest to its highest ask
/// would be; the windows no draw ever asked for (only the pass over the
/// heap, for shaders that pick views by index) and their bytes; and the
/// bytes of root views' windows that lie under what was asked for with a
/// length ([`bytes_under`]): memory held once for each.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Alive {
    pub shared: [u64; 2],
    pub grid: u64,
    pub undrawn: [u64; 2],
    pub root_under: u64,
}

/// The bytes of `windows` (start, bytes) that lie under `spans` (start,
/// end; in any order, over one another or not), summed over the windows.
pub(crate) fn bytes_under(mut spans: Vec<(u64, u64)>, windows: &[(u64, u64)]) -> u64 {
    spans.sort_unstable();
    // Joined: in order, apart.
    let mut joined: Vec<(u64, u64)> = Vec::with_capacity(spans.len());
    for (from, to) in spans {
        match joined.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => joined.push((from, to)),
        }
    }
    windows
        .iter()
        .map(|&(start, len)| {
            let end = start + len;
            // The first span that ends above the window's start, and on.
            let first = joined.partition_point(|&(_, to)| to <= start);
            joined[first..]
                .iter()
                .take_while(|&&(from, _)| from < end)
                .map(|&(from, to)| to.min(end) - from.max(start))
                .sum::<u64>()
        })
        .sum()
}

/// The line's part on sharing, after the windows' own figures: `sized` is
/// the bytes of the windows asked for with a length.
pub(crate) fn text(alive: &Alive, sized: u64, on: bool, unit: u64) -> String {
    let mb = |b: u64| b >> 20;
    let counts = |c: &[AtomicU64]| -> Vec<u64> { c.iter().map(|v| v.load(Relaxed)).collect() };
    let (made, took) = (counts(&MADE), counts(&TOOK));
    let past = PAST.load(Relaxed);
    text_of(alive, sized, on.then_some(unit), [made[0], made[1], made[2], made[3]], [took[0], took[1], past], mb)
}

fn text_of(alive: &Alive, sized: u64, unit: Option<u64>, made: [u64; 4], took: [u64; 3], mb: impl Fn(u64) -> u64) -> String {
    let mut out = format!(
        "; the grid's cells over what was asked in those with a length: about {} MB (they hold {} MB); of the root views' \
         windows {} MB lie under what was asked with a length; windows only the pass over the heap asked for, no draw: {} {} \
         MB; views made that read past their memory's end into a window's spare room: {}",
        mb(alive.grid),
        mb(sized),
        mb(alive.root_under),
        alive.undrawn[0],
        mb(alive.undrawn[1]),
        took[2],
    );
    match unit {
        Some(unit) => out.push_str(&format!(
            "; VICTORY_WINDOW_SHARE on ({} KiB): {} alive that more than one ask took {} MB; since the start {} made to fit \
             {} MB ({} from their ask's own start, {} MB over windows there already), {} asks took a window that was \
             there ({} MB not held again)",
            unit >> 10,
            alive.shared[0],
            mb(alive.shared[1]),
            made[0],
            mb(made[1]),
            made[2],
            mb(made[3]),
            took[0],
            mb(took[1]),
        )),
        None => out.push_str("; VICTORY_WINDOW_SHARE off"),
    }
    out
}

/// Since the start: buffer views made on a window that read past the end
/// of the memory their ask named, and so only fit by the room the window
/// has after it ([`view_made`]).
static PAST: AtomicU64 = AtomicU64::new(0);

/// Whether a view of `count` elements of `element` bytes from element
/// `first` of a copy reads past the ask it was found by: `len` bytes that
/// begin `offset` bytes into the copy.
fn past(offset: u64, len: u64, first: u64, count: u32, element: u64) -> bool {
    first.saturating_add(u64::from(count)).saturating_mul(element) > offset.saturating_add(len)
}

/// A buffer view the host accepts was made on `shadow` (see [`past`] for
/// the rest). The console is lax and the title's heap holds views that
/// reach past their memory's end; on the grid a window's cell had room
/// after most asks, and such a view read what lay there. A fitted window
/// has less room (whole 64 KiB pages still): a view that does not fit is
/// left null and reads zeros ("buffer views left null as the host would
/// refuse them" in the frame line). Counted with the memory-by-site line
/// on, switch on or off, so that a run without the switch says how many
/// views lean on that room.
pub(crate) fn view_made(shadow: &crate::shadow::Shadow, offset: u64, len: u64, first: u64, count: u32, element: u64) {
    if crate::memsites::on() && shadow.is_window() && past(offset, len, first, count, element) {
        PAST.fetch_add(1, Relaxed);
    }
}

thread_local! {
    /// Whether this thread is in a pass over the heap ([`pass`]).
    static IN_PASS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Until dropped, this thread's asks are a pass's over the heap, not a
/// draw's ([`drawn`]).
pub(crate) struct Pass(bool);

/// A pass over the heap begins on this thread (it makes the views shaders
/// pick by index and asks for the copy of each).
pub(crate) fn pass() -> Pass {
    Pass(IN_PASS.with(|p| p.replace(true)))
}

impl Drop for Pass {
    fn drop(&mut self) {
        IN_PASS.with(|p| p.set(self.0));
    }
}

/// Whether an ask made now on this thread is to be noted as a draw's (or
/// anything else's that is not a pass over the heap): only with the
/// memory-by-site line on, which says the count.
pub(crate) fn drawn() -> bool {
    crate::memsites::on() && !IN_PASS.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1 << 10;
    const GRID: u64 = 256 * KIB;
    const BASE: u64 = 0xd_1200_0000;

    fn choose(index: &mut Index, address: u64, end: u64) -> Choice {
        index.choose(address, end, PAGE, most(address, end, GRID))
    }

    /// Makes what `choose` says to make, as the table's code does.
    fn ask(index: &mut Index, next: &mut u64, address: u64, end: u64) -> Choice {
        let choice = choose(index, address, end);
        if let Choice::Make(start, len) = choice {
            *next += 1;
            index.made(address, end, false, start, len, *next);
        }
        choice
    }

    #[test]
    fn a_window_is_the_asks_own_pages() {
        // Inside one page: that page, not the grid's cell.
        assert_eq!(fit(BASE + 0x1_2000, BASE + 0x1_3000, PAGE), (BASE + 0x1_0000, PAGE));
        // Whole pages from the page it begins in.
        assert_eq!(fit(BASE + 0x1_2000, BASE + 0x5_3000, PAGE), (BASE + 0x1_0000, 5 * PAGE));
        // On a page line at both ends: nothing added.
        assert_eq!(fit(BASE, BASE + 3 * PAGE, PAGE), (BASE, 3 * PAGE));
        // A megabyte and a bit from an odd address.
        let (start, len) = fit(BASE + 0x1_2340, BASE + 0x1_2340 + (1 << 20), PAGE);
        assert_eq!((start, len), (BASE + 0x1_0000, 17 * PAGE));
        for (a, e) in [(BASE + 1, BASE + 2), (BASE + 0xffff, BASE + 0x1_0001), (BASE + 0x3_0100, BASE + 0x9_0000)] {
            for unit in [PAGE, 256 * KIB, 4 << 20] {
                let (start, len) = fit(a, e, unit);
                assert!(start <= a && start + len >= e, "{a:#x} {e:#x} {unit:#x}");
                assert!(len % PAGE == 0 && start % 256 == 0, "{a:#x} {e:#x} {unit:#x}");
            }
        }
    }

    #[test]
    fn a_short_ask_across_a_page_line_begins_at_its_own_start() {
        // 8 KiB across a line: one page from the ask's 256 bytes, not two.
        let (a, e) = (BASE + 0xf_f010, BASE + 0x10_1010);
        assert_eq!(fit(a, e, PAGE), (BASE + 0xf_f000, PAGE));
        // The address keeps its offset from 16 and from 256 bytes.
        let (start, _) = fit(a, e, PAGE);
        assert_eq!((a - start) % 256, a % 256);
        // Nothing saved: whole pages (65 KiB from the middle of a page are
        // two pages either way).
        assert_eq!(fit(BASE + 0x8000, BASE + 0x1_8400, PAGE), (BASE, 2 * PAGE));
        // Too long to begin anywhere but on a page.
        let long = fit(BASE + 0xf000, BASE + 0xf000 + 5 * PAGE - 0x1000, PAGE);
        assert_eq!(long, (BASE, 6 * PAGE));
        // Not on a coarser unit: its cells, whole.
        assert_eq!(fit(a, e, GRID), (BASE + 0xc_0000, 2 * GRID));
    }

    #[test]
    fn an_ask_takes_the_oldest_window_that_holds_it() {
        let mut index = Index::default();
        let mut next = 0;
        // A megabyte, then asks inside it: no window made.
        assert_eq!(ask(&mut index, &mut next, BASE, BASE + (1 << 20)), Choice::Make(BASE, 1 << 20));
        // (A megabyte is more than the grid's cell: only asks of half of it
        // or more may take it.)
        assert_eq!(ask(&mut index, &mut next, BASE + 0x100, BASE + 0x8_0100), Choice::There(BASE, 1 << 20));
        assert_eq!(index.took(BASE, 1 << 20), 1);
        // A short ask inside gets its own page: the megabyte is too long
        // for it.
        assert_eq!(ask(&mut index, &mut next, BASE + 0x2_0000, BASE + 0x2_1000), Choice::Make(BASE + 0x2_0000, PAGE));
        // Another short ask in that page takes it, at its offset.
        assert_eq!(ask(&mut index, &mut next, BASE + 0x2_4000, BASE + 0x2_5000), Choice::There(BASE + 0x2_0000, PAGE));
        // A younger window over the same page changes no answer.
        assert_eq!(
            ask(&mut index, &mut next, BASE + 0x1_f000, BASE + 0x2_2000),
            Choice::Make(BASE + 0x1_f000, PAGE)
        );
        assert_eq!(choose(&mut index, BASE + 0x2_0000, BASE + 0x2_1000), Choice::There(BASE + 0x2_0000, PAGE));
        // ... also with nothing remembered.
        index.memo.clear();
        assert_eq!(choose(&mut index, BASE + 0x2_0000, BASE + 0x2_1000), Choice::There(BASE + 0x2_0000, PAGE));
        assert_eq!(choose(&mut index, BASE + 0x2_4000, BASE + 0x2_5000), Choice::There(BASE + 0x2_0000, PAGE));
        assert_eq!(choose(&mut index, BASE + 0x100, BASE + 0x8_0100), Choice::There(BASE, 1 << 20));
        assert_eq!(index.len(), 3);
    }

    #[test]
    fn asks_that_overlap_share_one_copy_at_their_offsets() {
        let mut index = Index::default();
        let mut next = 0;
        // Three views of one 200 KiB buffer and of parts of it.
        let whole = ask(&mut index, &mut next, BASE + 0x400, BASE + 0x400 + 200 * KIB);
        assert_eq!(whole, Choice::Make(BASE, 4 * PAGE));
        for (a, e) in [(BASE + 0x400, BASE + 0x400 + 200 * KIB), (BASE + 0x1_0400, BASE + 0x2_0400), (BASE + 0x3_0000, BASE + 0x3_2000)] {
            let Choice::There(start, len) = ask(&mut index, &mut next, a, e) else {
                panic!("{a:#x}: a window of its own");
            };
            // The view's offset in the copy is its address less the copy's start.
            assert_eq!((start, len), (BASE, 4 * PAGE));
            assert!(a - start < len && e - start <= len);
        }
        assert_eq!(index.len(), 1);
        // The first ask is the window's own; the two parts took it (the
        // whole, asked again, is the same ask).
        assert_eq!(index.took(BASE, 4 * PAGE), 2);
    }

    #[test]
    fn a_window_does_not_grow_a_longer_ask_gets_its_own() {
        let mut index = Index::default();
        let mut next = 0;
        assert_eq!(ask(&mut index, &mut next, BASE + 0x1000, BASE + 0x2000), Choice::Make(BASE, PAGE));
        // Grown past the window's end: a window of its own over the first
        // (the first stays for the slots that show it), counted as memory
        // held again.
        let before = MADE[3].load(Relaxed);
        assert_eq!(index.overlap(BASE, 3 * PAGE), PAGE);
        assert_eq!(ask(&mut index, &mut next, BASE + 0x1000, BASE + 0x2_8000), Choice::Make(BASE, 3 * PAGE));
        assert!(MADE[3].load(Relaxed) >= before + PAGE);
        // The first ask still names the first window; an ask only the
        // longer one holds names that.
        assert_eq!(choose(&mut index, BASE + 0x1000, BASE + 0x2000), Choice::There(BASE, PAGE));
        assert_eq!(choose(&mut index, BASE + 0x1_1000, BASE + 0x1_2000), Choice::There(BASE, 3 * PAGE));
        // An ask both hold names the older, whatever the order of asking.
        assert_eq!(choose(&mut index, BASE + 0x3000, BASE + 0x4000), Choice::There(BASE, PAGE));
    }

    #[test]
    fn an_answer_changes_only_when_its_window_has_gone() {
        let mut index = Index::default();
        let mut next = 0;
        let (a, e) = (BASE + 0x5_1000, BASE + 0x5_3000);
        assert_eq!(ask(&mut index, &mut next, a, e), Choice::Make(BASE + 0x5_0000, PAGE));
        // Windows come all around and over it: the ask's answer stays.
        for (x, y) in [(BASE, BASE + GRID), (BASE + 0x4_0000, BASE + 0x8_0000), (BASE + 0x5_0000, BASE + 0x5_0100)] {
            ask(&mut index, &mut next, x, y);
            assert_eq!(choose(&mut index, a, e), Choice::There(BASE + 0x5_0000, PAGE), "{x:#x}");
            index.memo.clear();
            assert_eq!(choose(&mut index, a, e), Choice::There(BASE + 0x5_0000, PAGE), "{x:#x}");
        }
        // Let go (no slot and no list held it): the ask finds the oldest of
        // the others that hold it, and that answer stays in turn.
        index.remove(BASE + 0x5_0000, PAGE);
        assert_eq!(choose(&mut index, a, e), Choice::There(BASE + 0x4_0000, GRID));
        // Every one let go, in any order: a window of its own again.
        index.remove(BASE + 0x4_0000, GRID);
        index.remove(BASE, GRID);
        assert_eq!(choose(&mut index, a, e), Choice::Make(BASE + 0x5_0000, PAGE));
        assert_eq!(index.len(), 0);
        assert!(index.cells.is_empty() && index.memo.is_empty());
        // Taken out twice, or never put in: nothing.
        index.remove(BASE, GRID);
    }

    #[test]
    fn a_short_ask_does_not_take_a_window_of_megabytes() {
        let mut index = Index::default();
        // A root view's tail of 3 MiB, there first.
        index.insert(BASE + (1 << 20), 3 << 20, 1);
        let (a, e) = (BASE + (2 << 20) + 0x100, BASE + (2 << 20) + 0x1100);
        // 4 KiB: the tail is longer than the grid's cell, so its own page.
        assert_eq!(most(a, e, GRID), GRID);
        assert_eq!(choose(&mut index, a, e), Choice::Make(BASE + (2 << 20), PAGE));
        // An ask of half the tail or more takes it.
        let long = (BASE + (1 << 20), BASE + (1 << 20) + (3 << 19));
        assert_eq!(most(long.0, long.1, GRID), 3 << 20);
        assert_eq!(choose(&mut index, long.0, long.1), Choice::There(BASE + (1 << 20), 3 << 20));
        // With the whole cell allowed (VICTORY_WINDOW_KB=4096) the short
        // one takes it too, and so does a draw's own range on any grid.
        assert_eq!(index.choose(a, e, PAGE, most(a, e, 4 << 20)), Choice::There(BASE + (1 << 20), 3 << 20));
        index.memo.clear();
        assert_eq!(index.choose(a, e, PAGE, u64::MAX), Choice::There(BASE + (1 << 20), 3 << 20));
        // The two kinds of ask keep their answers apart.
        assert_eq!(choose(&mut index, a, e), Choice::Make(BASE + (2 << 20), PAGE));
    }

    #[test]
    fn the_window_made_for_an_ask_is_one_it_may_take() {
        // On a 256 KiB unit a short ask across a unit line gets two units,
        // longer than the most it may take of others' windows.
        let mut index = Index::default();
        let (a, e) = (BASE + GRID - 0x800, BASE + GRID + 0x800);
        let limit = most(a, e, GRID);
        assert_eq!(limit, GRID);
        assert_eq!(index.choose(a, e, GRID, limit), Choice::Make(BASE, 2 * GRID));
        index.made(a, e, false, BASE, 2 * GRID, 1);
        // It is the ask's window from then on, searched for anew or not ...
        assert_eq!(index.choose(a, e, GRID, limit), Choice::There(BASE, 2 * GRID));
        assert_eq!(index.took(BASE, 2 * GRID), 0);
        index.memo.clear();
        // ... and whatever comes later that holds the ask and is short
        // enough (a draw's window from the ask's own start).
        index.insert(BASE + GRID - 0x800, PAGE, 2);
        assert_eq!(index.choose(a, e, GRID, limit), Choice::There(BASE, 2 * GRID));
        // Another short ask there, which would not have made it, may not
        // take it: the younger, shorter one is its window.
        let (b, f) = (BASE + GRID + 0x100, BASE + GRID + 0x400);
        assert_eq!(index.choose(b, f, GRID, most(b, f, GRID)), Choice::There(BASE + GRID - 0x800, PAGE));
    }

    #[test]
    fn a_window_is_found_from_every_cell_it_lies_in() {
        let mut index = Index::default();
        // 10 MiB from the middle of a cell: three cells.
        let (start, len) = (BASE + (2 << 20), 10 << 20);
        index.insert(start, len, 7);
        for at in [start, start + (3 << 20), start + len - (6 << 20)] {
            assert_eq!(index.holder(at, at + (6 << 20), u64::MAX, (0, 0)), Some((start, len)), "{at:#x}");
        }
        // Past its end, or begun below it: not held.
        assert_eq!(index.holder(start + len - 0x1000, start + len + 1, u64::MAX, (0, 0)), None);
        assert_eq!(index.holder(start - 1, start + 0x1000, u64::MAX, (0, 0)), None);
        assert_eq!(index.overlap(BASE, 4 << 20), 2 << 20);
        assert_eq!(index.overlap(start + len, 4 << 20), 0);
        index.remove(start, len);
        assert!(index.cells.is_empty());
    }

    #[test]
    fn memory_under_two_windows_is_counted_once() {
        let mut index = Index::default();
        index.insert(BASE, 2 * PAGE, 1);
        index.insert(BASE + PAGE, 2 * PAGE, 2);
        index.insert(BASE + 8 * PAGE, PAGE, 3);
        // Over the first two (three pages between them) and short of the third.
        assert_eq!(index.overlap(BASE, 8 * PAGE), 3 * PAGE);
        assert_eq!(index.overlap(BASE + PAGE / 2, PAGE), PAGE);
        assert_eq!(index.overlap(BASE + 3 * PAGE, 5 * PAGE), 0);
        assert_eq!(index.overlap(BASE + 3 * PAGE, 6 * PAGE), PAGE);
    }

    #[test]
    fn a_dense_run_of_small_buffers_holds_little_more_than_itself() {
        // 4 MiB of 8 KiB buffers at odd places, asked for in a scattered
        // order: on the 256 KiB grid each cell's window and a two-cell
        // window for every buffer across a cell line.
        let mut index = Index::default();
        let mut next = 0;
        let step = 8 * KIB + 0x300;
        let count = (4 << 20) / step;
        let mut made = 0;
        for i in 0..count {
            // (a step coprime with the count: every buffer once)
            let n = (i * 197) % count;
            let (a, e) = (BASE + n * step, BASE + n * step + 8 * KIB);
            if let Choice::Make(_, len) = ask(&mut index, &mut next, a, e) {
                made += len;
            }
        }
        // Every page once, and one more page for each buffer across a page
        // line (at most one a line).
        assert!(made <= 8 << 20, "{made:#x}");
        let grid: u64 = {
            let mut cells = std::collections::BTreeSet::new();
            for n in 0..count {
                let (a, e) = (BASE + n * step, BASE + n * step + 8 * KIB);
                let start = a & !(GRID - 1);
                cells.insert((start, e.next_multiple_of(GRID) - start));
            }
            cells.iter().map(|c| c.1).sum()
        };
        assert!(grid >= 11 << 20, "{grid:#x}");
    }

    #[test]
    fn the_unit_is_a_power_of_two_or_a_page() {
        assert_eq!(unit_of(None), PAGE);
        assert_eq!(unit_of(Some(64)), PAGE);
        assert_eq!(unit_of(Some(256)), 256 * KIB);
        assert_eq!(unit_of(Some(4096)), 4 << 20);
        for bad in [0, 32, 100, 8192] {
            assert_eq!(unit_of(Some(bad)), PAGE, "{bad}");
        }
    }

    #[test]
    fn the_line_says_what_sharing_holds_and_saved() {
        const MIB: u64 = 1 << 20;
        let alive = Alive { shared: [12, 30 * MIB], grid: 900 * MIB, undrawn: [40, 100 * MIB], root_under: 77 * MIB };
        let line = text_of(&alive, 500 * MIB, Some(PAGE), [70, 400 * MIB, 5, 9 * MIB], [300, 80 * MIB, 3], |b| b >> 20);
        assert!(line.contains("into a window's spare room: 3;"), "{line}");
        assert!(line.contains("about 900 MB (they hold 500 MB)"), "{line}");
        assert!(line.contains("no draw: 40 100 MB"), "{line}");
        assert!(line.contains("of the root views' windows 77 MB lie under what was asked with a length"), "{line}");
        assert!(line.contains("VICTORY_WINDOW_SHARE on (64 KiB): 12 alive that more than one ask took 30 MB"), "{line}");
        assert!(line.contains("70 made to fit 400 MB (5 from their ask's own start, 9 MB over windows there already)"), "{line}");
        assert!(line.contains("300 asks took a window that was there (80 MB not held again)"), "{line}");
        let off = text_of(&alive, 500 * MIB, None, [0; 4], [0; 3], |b| b >> 20);
        assert!(off.ends_with("VICTORY_WINDOW_SHARE off"), "{off}");
    }

    #[test]
    fn root_windows_are_measured_against_what_was_asked_with_a_length() {
        const MIB: u64 = 1 << 20;
        // Asks with a length: two that overlap, one apart, one that only touches.
        let spans = vec![(10 * MIB, 11 * MIB), (10 * MIB + 0x8_0000, 12 * MIB), (20 * MIB, 21 * MIB), (12 * MIB, 12 * MIB + 0x1000)];
        // A tail over the first run's upper half and beyond; one under
        // nothing; one inside an ask; one that only touches an ask's end.
        let windows = [(11 * MIB, 4 * MIB), (30 * MIB, MIB), (20 * MIB + 0x1000, 0x2000), (21 * MIB, MIB)];
        assert_eq!(bytes_under(spans.clone(), &windows[..1]), MIB + 0x1000);
        assert_eq!(bytes_under(spans.clone(), &windows[1..2]), 0);
        assert_eq!(bytes_under(spans.clone(), &windows[2..3]), 0x2000);
        assert_eq!(bytes_under(spans.clone(), &windows[3..]), 0);
        assert_eq!(bytes_under(spans, &windows), MIB + 0x3000);
        assert_eq!(bytes_under(Vec::new(), &windows), 0);
    }

    #[test]
    fn a_view_past_its_asks_end_is_told_apart() {
        // 16 elements of 16 bytes from element 4 of a copy the ask begins
        // 64 bytes into: bytes 64 to 320.
        assert!(!past(64, 256, 4, 16, 16));
        assert!(past(64, 255, 4, 16, 16));
        // One element more than the ask holds.
        assert!(past(0, 4096, 0, 1025, 4));
        assert!(!past(0, 4096, 0, 1024, 4));
        // Numbers the title's heap may hold: no overflow.
        assert!(past(0, 16, u64::MAX, u32::MAX, 2048));
    }

    #[test]
    fn a_pass_over_the_heap_is_not_a_draw() {
        assert!(!IN_PASS.with(std::cell::Cell::get));
        {
            let _pass = pass();
            assert!(IN_PASS.with(std::cell::Cell::get));
            {
                let _inner = pass();
            }
            // The inner one's end leaves the outer in force.
            assert!(IN_PASS.with(std::cell::Cell::get));
        }
        assert!(!IN_PASS.with(std::cell::Cell::get));
    }
}
