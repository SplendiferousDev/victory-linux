//! `VICTORY_SLOT_GUARD` (off unless `1`; `alt` in turn): the per-draw slots
//! of a view heap (`crate::descriptor::host::DYNAMIC`) are used again only
//! by a table of the same kinds, and only once every submission that could
//! read them has finished.
//!
//! A translated shader reads a table entry at the heap index its user data
//! gives it, as the kind its code says. A constant buffer read through a
//! slot that holds another kind of view makes NVIDIA's constant cache read
//! an address decoded from that view's bits (`crate::slotkind`). Tables
//! with constant buffers are made afresh in per-draw slots, and those slots
//! were handed out by a walk round one ring: a run given back was taken by
//! whichever table came next, of whatever layout, so a slot went from a
//! constant buffer view to a texture view and back some 16,000 times a
//! frame. That is harmless only while nothing in the air reads the slot any
//! more: every reader that outlives the list that held the slot (a list run
//! again, a slot given back early by any path, a table entry read past the
//! slots made for it) then reads another kind and faults.
//!
//! Two parts (`VICTORY_SLOT_GUARD_PARTS=stable` or `hold` for one alone):
//!
//! - **stable**: a run given back is kept for tables of the same shape (the
//!   slot count and which entries are constant buffer views,
//!   [`shape`]). A slot then holds the same kind for as long as the pools
//!   stand: a late reader finds other constants, never another kind.
//! - **hold**: a run given back carries the number of the last submission
//!   started by then (`crate::host::started`) and is handed out again only
//!   when that submission has finished: nothing that was in the air when
//!   the slots were given back can still read them.
//!
//! The ring is still what new runs are cut from. When it has no room, runs
//! whose readers have finished go back to it (their kinds may change then,
//! with nothing reading them); only if that is not enough is a run taken
//! before its readers have finished, of the same shape first (counted: the
//! old way did this every time).
//!
//! `VICTORY_SLOT_FAULT_TEST=early` (with `VICTORY_SLOT_FAULT_TEST_CONFIRM=1`,
//! refused without it): a test that can fault the card on purpose. Lists
//! give their per-draw slots back when their submission is sent, not when
//! it has run, and the ring hands the slots just given back out first. With
//! the guard off the tables of the next draws land on slots the flight in
//! the air still reads; with it on they must not.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::fasthash::FastMap;

static GUARD: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_SLOT_GUARD",
    "per-draw view heap slots are used again only by a table of the same kinds, and only once the submissions that could read them have finished",
);

/// Part of [`parts`]: runs are kept by shape.
pub(crate) const STABLE: u8 = 1;
/// Part of [`parts`]: runs wait for the submissions started before they
/// were given back.
pub(crate) const HOLD: u8 = 2;

/// Which parts are on now: 0 with the switch off.
pub(crate) fn parts() -> u8 {
    if !GUARD.on() {
        return 0;
    }
    static PARTS: OnceLock<u8> = OnceLock::new();
    *PARTS.get_or_init(|| parts_of(std::env::var("VICTORY_SLOT_GUARD_PARTS").ok().as_deref()))
}

/// The parts `VICTORY_SLOT_GUARD_PARTS` names: both unless it names one.
fn parts_of(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("stable") => STABLE,
        Some("hold") => HOLD,
        _ => STABLE | HOLD,
    }
}

/// Whether the switch is set at all (`1` or `alt`).
pub(crate) fn set() -> bool {
    GUARD.set()
}

/// The fault test (`VICTORY_SLOT_FAULT_TEST=early`): slots are given back
/// when their submission is sent and handed out again at once. The process
/// ends at the first look if the confirmation is missing: this can fault
/// the card.
pub(crate) fn fault_test() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let asked = std::env::var("VICTORY_SLOT_FAULT_TEST").ok();
        let confirmed = std::env::var_os("VICTORY_SLOT_FAULT_TEST_CONFIRM").is_some_and(|v| v == "1");
        match fault_test_of(asked.as_deref(), confirmed) {
            Ok(false) => false,
            Ok(true) => {
                tracing::warn!(
                    "FAULT TEST: lists give their per-draw view heap slots back when their submission is sent, and the slots \
                     are handed out again at once (VICTORY_SLOT_FAULT_TEST=early): without VICTORY_SLOT_GUARD this is meant \
                     to fault the card"
                );
                true
            }
            Err(why) => {
                tracing::error!("{why}");
                eprintln!("{why}");
                std::process::exit(78);
            }
        }
    })
}

/// What the fault test's two variables ask for: on, off, or a refusal.
fn fault_test_of(asked: Option<&str>, confirmed: bool) -> Result<bool, &'static str> {
    match asked.map(str::trim) {
        None | Some("" | "0") => Ok(false),
        Some("early") if confirmed => Ok(true),
        Some("early") => Err(
            "VICTORY_SLOT_FAULT_TEST=early can fault the card and take the desktop with it: not run without \
             VICTORY_SLOT_FAULT_TEST_CONFIRM=1",
        ),
        Some(_) => Err("VICTORY_SLOT_FAULT_TEST: the only test is `early`"),
    }
}

/// The shape of a one-view table of a root view (never a constant buffer).
pub(crate) const SHAPE_VIEW: u64 = 1;

/// The shape of a table from which of its entries, in order, are made
/// constant buffer views: tables of one shape hold the same kind at every
/// slot. Never 0 (the shape of every run with the stable part off) nor
/// [`SHAPE_VIEW`] for a table with a constant buffer view.
pub(crate) fn shape(constant: impl Iterator<Item = bool>) -> u64 {
    let mut h: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut any = false;
    for (k, c) in constant.enumerate() {
        if c {
            any = true;
            h = (h.rotate_left(7) ^ (k as u64 + 1)).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
    if !any {
        return SHAPE_VIEW;
    }
    h | 2
}

/// The runs of one heap's per-draw slots that are out with a shape, and the
/// ones given back and kept for their shape. Slots count from the first
/// per-draw slot.
#[derive(Default)]
pub(crate) struct Pools {
    /// By slot count and shape: runs given back (first slot, the number of
    /// the last submission started by then), oldest first.
    free: FastMap<(u32, u64), VecDeque<(u32, u64)>>,
    /// Runs out: first slot to slot count and shape.
    out: FastMap<u32, (u32, u64)>,
    /// Slots in `free`.
    pooled: u32,
}

impl Pools {
    /// A run of `n` slots of `shape` given back before, whose readers have
    /// finished (given back with at most `finished` started).
    fn pop(&mut self, n: u32, shape: u64, finished: u64) -> Option<u32> {
        let runs = self.free.get_mut(&(n, shape))?;
        let &(first, tag) = runs.front()?;
        if tag > finished {
            return None;
        }
        runs.pop_front();
        self.pooled -= n;
        Some(first)
    }

    /// A run of `n` slots kept for any other shape whose readers have
    /// finished: for a table the ring has no room for. Its slots change
    /// kind, with nothing reading them.
    fn pop_other(&mut self, n: u32, finished: u64) -> Option<u32> {
        let runs = self
            .free
            .iter_mut()
            .filter(|(key, _)| key.0 == n)
            .find_map(|(_, runs)| runs.front().is_some_and(|&(_, tag)| tag <= finished).then_some(runs))?;
        let (first, _) = runs.pop_front()?;
        self.pooled -= n;
        Some(first)
    }

    /// Whether a run of that shape is kept at all (its readers may fly).
    fn has(&self, n: u32, shape: u64) -> bool {
        self.free.get(&(n, shape)).is_some_and(|runs| !runs.is_empty())
    }

    /// The run from `first` is out with `shape`.
    fn note_out(&mut self, first: u32, n: u32, shape: u64) {
        self.out.insert(first, (n, shape));
    }

    /// The run from `first` comes back with `started` submissions started.
    /// `Ok` if it is kept for its shape; else the slots the ring gets back.
    fn give(&mut self, first: u32, n: u32, started: u64, keep: bool) -> Result<(), u32> {
        match self.out.remove(&first) {
            Some((m, shape)) if m == n && keep => {
                self.free.entry((n, shape)).or_default().push_back((first, started));
                self.pooled += n;
                Ok(())
            }
            Some((m, _)) => Err(m.max(n)),
            None => Err(n),
        }
    }

    /// Takes out of the pools every run whose readers have finished (given
    /// back with at most `finished` started), for the ring: (first, count).
    fn drain(&mut self, finished: u64) -> Vec<(u32, u32)> {
        let mut back = Vec::new();
        for (&(n, _), runs) in &mut self.free {
            while let Some(&(first, tag)) = runs.front() {
                if tag > finished {
                    break;
                }
                runs.pop_front();
                back.push((first, n));
            }
        }
        self.free.retain(|_, runs| !runs.is_empty());
        self.pooled -= back.iter().map(|b| b.1).sum::<u32>();
        back
    }
}

/// Counts since the last line ([`line`]) and since the start.
/// 0: tables given a run kept for their shape; 1: the slots of those;
/// 2: tables whose shape had a run kept, but one a submission in flight may
/// still read: they got a run of their own instead; 3: runs cut from the
/// ring; 4: slots that went to another shape or back to the ring because
/// the ring had no room (their readers had finished); 5: runs taken although a submission in flight may
/// still read them (the ring and the pools were out); 6: slots that went
/// back to the ring before their readers had finished (likewise); 7: tables
/// refused; 8: runs given back and kept; 9: runs given back to the ring.
static COUNTS: [[AtomicU64; 10]; 2] = [const { [const { AtomicU64::new(0) }; 10] }; 2];
/// Slots kept in pools, runs out with a shape, now (all heaps).
static POOLED: AtomicU64 = AtomicU64::new(0);
static OUT: AtomicU64 = AtomicU64::new(0);

fn count(i: usize, n: u64) {
    for c in &COUNTS {
        c[i].fetch_add(n, Ordering::Relaxed);
    }
}

fn lock(pools: &Mutex<Pools>) -> std::sync::MutexGuard<'_, Pools> {
    crate::lockmeter::lock("dh:guard (per-draw slot pools)", pools)
}

/// Whether any heap keeps runs or has runs out with a shape: what a heap
/// asks before it takes the old way with the switch off (in turn).
pub(crate) fn any() -> bool {
    POOLED.load(Ordering::Relaxed) != 0 || OUT.load(Ordering::Relaxed) != 0
}

/// `n` slots for a table of `shape` with the guard's `parts` on, `finished`
/// submissions finished: the first slot. `ring_take` cuts a run from the
/// ring, `ring_give` gives one back to it.
pub(crate) fn take(
    pools: &Mutex<Pools>,
    n: u32,
    shape: u64,
    parts: u8,
    finished: u64,
    mut ring_take: impl FnMut(u32) -> Option<u32>,
    mut ring_give: impl FnMut(u32, u32),
) -> Option<u32> {
    let shape = if parts & STABLE != 0 { shape } else { 0 };
    let finished = if parts & HOLD != 0 { finished } else { u64::MAX };
    let mut p = lock(pools);
    let before = p.pooled;
    let first = take_in(&mut p, n, shape, finished, &mut ring_take, &mut ring_give);
    if first.is_some() {
        OUT.fetch_add(1, Ordering::Relaxed);
    } else {
        count(7, 1);
    }
    let after = p.pooled;
    drop(p);
    gauge(&POOLED, before, after);
    first
}

fn gauge(g: &AtomicU64, before: u32, after: u32) {
    if after >= before {
        g.fetch_add(u64::from(after - before), Ordering::Relaxed);
    } else {
        g.fetch_sub(u64::from(before - after), Ordering::Relaxed);
    }
}

fn take_in(
    p: &mut Pools,
    n: u32,
    shape: u64,
    finished: u64,
    ring_take: &mut impl FnMut(u32) -> Option<u32>,
    ring_give: &mut impl FnMut(u32, u32),
) -> Option<u32> {
    if let Some(first) = p.pop(n, shape, finished) {
        count(0, 1);
        count(1, u64::from(n));
        p.note_out(first, n, shape);
        return Some(first);
    }
    if p.has(n, shape) {
        count(2, 1);
    }
    if let Some(first) = ring_take(n) {
        count(3, 1);
        p.note_out(first, n, shape);
        return Some(first);
    }
    // The ring has no room. A run of that many slots kept for another
    // shape, which nothing in the air reads, is taken as it is; else all
    // that nothing in the air reads goes back to the ring.
    if let Some(first) = p.pop_other(n, finished) {
        count(4, u64::from(n));
        p.note_out(first, n, shape);
        return Some(first);
    }
    let back = p.drain(finished);
    if !back.is_empty() {
        count(4, back.iter().map(|b| u64::from(b.1)).sum());
        for (first, m) in back {
            ring_give(first, m);
        }
        if let Some(first) = ring_take(n) {
            count(3, 1);
            p.note_out(first, n, shape);
            return Some(first);
        }
    }
    // Before its readers have finished, as without the guard: a run of the
    // same shape first (its kinds stay), then everything back to the ring.
    if let Some(first) = p.pop(n, shape, u64::MAX) {
        count(5, 1);
        p.note_out(first, n, shape);
        return Some(first);
    }
    let back = p.drain(u64::MAX);
    if back.is_empty() {
        return None;
    }
    count(6, back.iter().map(|b| u64::from(b.1)).sum());
    for (first, m) in back {
        ring_give(first, m);
    }
    let first = ring_take(n)?;
    count(3, 1);
    p.note_out(first, n, shape);
    Some(first)
}

/// The run of `n` slots from `first` is given back with `started`
/// submissions started: kept for its shape with any of `parts` on, else
/// (or if it was not taken with a shape) given to the ring.
pub(crate) fn give(pools: &Mutex<Pools>, first: u32, n: u32, parts: u8, started: u64, mut ring_give: impl FnMut(u32, u32)) {
    let mut p = lock(pools);
    let was_out = p.out.contains_key(&first);
    let kept = p.give(first, n, started, parts != 0);
    drop(p);
    if was_out {
        OUT.fetch_sub(1, Ordering::Relaxed);
    }
    match kept {
        Ok(()) => {
            POOLED.fetch_add(u64::from(n), Ordering::Relaxed);
            count(8, 1);
        }
        Err(slots) => {
            count(9, 1);
            ring_give(first, slots);
        }
    }
}

/// Everything kept goes back to the ring: the switch went off (in turn).
pub(crate) fn flush(pools: &Mutex<Pools>, mut ring_give: impl FnMut(u32, u32)) {
    let mut p = lock(pools);
    let before = p.pooled;
    let back = p.drain(u64::MAX);
    drop(p);
    gauge(&POOLED, before, 0);
    for (first, m) in back {
        ring_give(first, m);
    }
}

/// With the frame line: what the guard did since the last one, per frame
/// over `frames`, and what the look at lists' tables found
/// (`crate::readercheck`). Nothing while the switch is unset and nothing
/// was found.
pub(crate) fn line(frames: f64) {
    let c: [u64; 10] = std::array::from_fn(|i| COUNTS[0][i].swap(0, Ordering::Relaxed));
    let found = crate::readercheck::take_landing();
    if !set() && found.iter().all(|&n| n == 0) {
        return;
    }
    let f = |n: u64| n as f64 / frames;
    tracing::info!(
        "per-draw view heap slots per frame (VICTORY_SLOT_GUARD, parts {} now): {:.0} tables took a run kept for their shape \
         (slots recycled without a change of kind: {:.0}), {:.0} recycles deferred because a submission in flight may still \
         read the run (a run of their own instead), {:.0} runs cut from the ring; {:.0} runs given back and kept, {:.0} given \
         back to the ring; the ring out of room: {:.0} slots went to another shape or back to it with their readers finished, \
         {:.1} runs taken and {:.0} slots sent back before their readers had finished, {:.1} tables refused; now {} slots kept and {} runs \
         out; tables looked at when given back (VICTORY_READER_CHECK): {:.1} slots a per-draw table was made in were \
         written again while its list held them ({:.1} of them now of another kind), {:.2} stages read a table at the \
         title's own place past the host's title slots; binds of stages that read a constant buffer at a table place \
         their used-resource list does not name as one: {:.1} at a fixed place, {:.1} at a place worked out as they run",
        parts(),
        f(c[0]),
        f(c[1]),
        f(c[2]),
        f(c[3]),
        f(c[8]),
        f(c[9]),
        f(c[4]),
        f(c[5]),
        f(c[6]),
        f(c[7]),
        POOLED.load(Ordering::Relaxed),
        OUT.load(Ordering::Relaxed),
        f(found[0]),
        f(found[1]),
        f(found[2]),
        f(found[3]),
        f(found[4]),
    );
}

/// For the stall and removal reports and the fault line: the counts since
/// the start.
pub(crate) fn section(out: &mut String) {
    use std::fmt::Write;
    let c: [u64; 10] = std::array::from_fn(|i| COUNTS[1][i].load(Ordering::Relaxed));
    let found = crate::readercheck::landing_totals();
    let _ = write!(
        out,
        "; per-draw slot guard (VICTORY_SLOT_GUARD parts {}{}): {} tables took a kept run ({} slots), {} recycles deferred for \
         a submission in flight, {} runs cut from the ring, {} slots back to the ring with readers finished, {} runs taken and \
         {} slots sent back before their readers had finished, {} tables refused; tables looked at when given back: {} slots \
         written again while their list held them ({} of another kind now), {} stages read a table at the title's own place \
         past the host's title slots; binds of stages reading a constant buffer at a table place their used-resource list \
         does not name as one: {} at a fixed place, {} at a place worked out as they run",
        parts(),
        if fault_test() { ", FAULT TEST early" } else { "" },
        c[0],
        c[1],
        c[2],
        c[3],
        c[4],
        c[5],
        c[6],
        c[7],
        found[0],
        found[1],
        found[2],
        found[3],
        found[4],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ring of `size` slots handed out in order, never reused unless
    /// given back: (next free, given back runs).
    struct Ring {
        size: u32,
        next: u32,
        back: Vec<(u32, u32)>,
        writes: Vec<u8>,
    }

    impl Ring {
        fn new(size: u32) -> Ring {
            Ring { size, next: 0, back: Vec::new(), writes: vec![0; size as usize] }
        }

        fn take(&mut self, n: u32) -> Option<u32> {
            if let Some(at) = self.back.iter().position(|b| b.1 >= n) {
                let (first, m) = self.back.swap_remove(at);
                if m > n {
                    self.back.push((first + n, m - n));
                }
                return Some(first);
            }
            (self.next + n <= self.size).then(|| {
                self.next += n;
                self.next - n
            })
        }
    }

    fn take(pools: &Mutex<Pools>, ring: &std::cell::RefCell<Ring>, n: u32, shape: u64, parts: u8, finished: u64) -> Option<u32> {
        super::take(pools, n, shape, parts, finished, |n| ring.borrow_mut().take(n), |first, m| ring.borrow_mut().back.push((first, m)))
    }

    fn give(pools: &Mutex<Pools>, ring: &std::cell::RefCell<Ring>, first: u32, n: u32, parts: u8, started: u64) {
        super::give(pools, first, n, parts, started, |first, m| ring.borrow_mut().back.push((first, m)));
    }

    /// Writes a table of the layout `constant` into the ring's slots and
    /// says how many slots changed kind (1 constant, 2 other).
    fn write(ring: &std::cell::RefCell<Ring>, first: u32, constant: &[bool]) -> usize {
        let mut ring = ring.borrow_mut();
        let mut changed = 0;
        for (k, &c) in constant.iter().enumerate() {
            let kind = if c { 1 } else { 2 };
            let slot = &mut ring.writes[first as usize + k];
            if *slot != 0 && *slot != kind {
                changed += 1;
            }
            *slot = kind;
        }
        changed
    }

    #[test]
    fn shapes_tell_layouts_apart_and_only_layouts() {
        let a = shape([true, false, false].into_iter());
        let b = shape([false, true, false].into_iter());
        let c = shape([true, false, false].into_iter());
        assert_eq!(a, c);
        assert_ne!(a, b);
        assert_eq!(shape([false, false].into_iter()), SHAPE_VIEW);
        assert!(a != 0 && a != SHAPE_VIEW && b != 0 && b != SHAPE_VIEW);
        // The slot count is in the pools' key, not in the shape.
        assert_eq!(shape([true].into_iter()), shape([true, false].into_iter()));
    }

    #[test]
    fn the_parts_and_the_fault_test_are_read_as_documented() {
        assert_eq!(parts_of(None), STABLE | HOLD);
        assert_eq!(parts_of(Some("stable")), STABLE);
        assert_eq!(parts_of(Some(" hold ")), HOLD);
        assert_eq!(fault_test_of(None, false), Ok(false));
        assert_eq!(fault_test_of(Some("0"), true), Ok(false));
        assert_eq!(fault_test_of(Some("early"), true), Ok(true));
        assert!(fault_test_of(Some("early"), false).is_err(), "refused without the confirmation");
        assert!(fault_test_of(Some("soon"), true).is_err());
    }

    /// Two layouts taking turns on a small ring. Without the guard (every
    /// run straight back to the ring) the slots change kind; with the
    /// stable part none does after the first round.
    #[test]
    fn a_recycled_slot_never_changes_kind() {
        let layouts: [&[bool]; 2] = [&[true, false, false, true], &[false, true, true, false]];
        for parts in [0, STABLE, STABLE | HOLD] {
            let (pools, ring) = (Mutex::new(Pools::default()), std::cell::RefCell::new(Ring::new(64)));
            let mut changed = 0;
            for round in 0..40u64 {
                let layout = layouts[(round % 2) as usize];
                let first = if parts == 0 {
                    ring.borrow_mut().take(4).unwrap()
                } else {
                    take(&pools, &ring, 4, shape(layout.iter().copied()), parts, round).unwrap()
                };
                changed += write(&ring, first, layout);
                if parts == 0 {
                    ring.borrow_mut().back.push((first, 4));
                } else {
                    // Its submission is `round + 1`, finished by the next round.
                    give(&pools, &ring, first, 4, parts, round + 1);
                }
            }
            if parts == 0 {
                assert!(changed > 100, "the old way changes kinds: {changed}");
            } else {
                assert_eq!(changed, 0, "parts {parts}");
                assert!(ring.borrow().next <= 16, "two shapes need few runs: {}", ring.borrow().next);
            }
        }
    }

    /// A run given back while submission 7 is in the air is not handed out
    /// until 7 has finished; a table of that shape gets a run of its own
    /// meanwhile.
    #[test]
    fn a_run_waits_for_the_submissions_started_before_it_was_given_back() {
        let (pools, ring) = (Mutex::new(Pools::default()), std::cell::RefCell::new(Ring::new(64)));
        let s = shape([true, false].into_iter());
        let a = take(&pools, &ring, 2, s, STABLE | HOLD, 5).unwrap();
        give(&pools, &ring, a, 2, STABLE | HOLD, 7);
        let b = take(&pools, &ring, 2, s, STABLE | HOLD, 6).unwrap();
        assert_ne!(a, b, "submission 7 may still read the run");
        give(&pools, &ring, b, 2, STABLE | HOLD, 8);
        assert_eq!(take(&pools, &ring, 2, s, STABLE | HOLD, 7), Some(a), "7 has finished");
        assert_ne!(take(&pools, &ring, 2, s, STABLE | HOLD, 7), Some(b), "8 has not");
        // Without the hold part the run is handed out at once.
        let (pools, ring) = (Mutex::new(Pools::default()), std::cell::RefCell::new(Ring::new(64)));
        let a = take(&pools, &ring, 2, s, STABLE, 5).unwrap();
        give(&pools, &ring, a, 2, STABLE, 7);
        assert_eq!(take(&pools, &ring, 2, s, STABLE, 5), Some(a));
    }

    /// The ring out of room: runs whose readers have finished go back to it
    /// for a table of another shape; a run whose readers have not is taken
    /// only when nothing else is left, of the same shape first.
    #[test]
    fn a_full_ring_takes_back_what_nothing_reads_first() {
        let (pools, ring) = (Mutex::new(Pools::default()), std::cell::RefCell::new(Ring::new(8)));
        let (s, t) = (shape([true, false, false, false].into_iter()), shape([false, true, false, false].into_iter()));
        let both = STABLE | HOLD;
        let a = take(&pools, &ring, 4, s, both, 0).unwrap();
        let b = take(&pools, &ring, 4, s, both, 0).unwrap();
        assert_eq!(take(&pools, &ring, 4, t, both, 0), None, "everything is out");
        give(&pools, &ring, a, 4, both, 3);
        give(&pools, &ring, b, 4, both, 9);
        // Shape `t`, submissions up to 3 finished: `a` (as many slots,
        // nothing reads it) is taken as it is; `b` (read until 9) stays.
        assert_eq!(take(&pools, &ring, 4, t, both, 3), Some(a));
        assert!(ring.borrow().back.is_empty(), "not by way of the ring");
        // Another `t`: only `b` is left, of another shape and still read.
        // It goes back to the ring before its time (counted), as the old
        // way would have done at once.
        let early = COUNTS[1][6].load(Ordering::Relaxed);
        assert_eq!(take(&pools, &ring, 4, t, both, 3), Some(b));
        assert!(COUNTS[1][6].load(Ordering::Relaxed) >= early + 4);
        assert_eq!(take(&pools, &ring, 4, t, both, 3), None);
        // A table of another length: everything nothing reads goes back to
        // the ring, where a run of its length is cut.
        give(&pools, &ring, a, 4, both, 3);
        give(&pools, &ring, b, 4, both, 3);
        let u = shape([true, true].into_iter());
        assert_eq!(take(&pools, &ring, 2, u, both, 3), Some(a));
        assert_eq!(lock(&pools).pooled, 0);
    }

    /// A run that was not taken with a shape (the old way, or the guard
    /// switched off meanwhile) goes to the ring; with the switch off a run
    /// taken with a shape does too.
    #[test]
    fn runs_go_to_the_ring_when_the_guard_does_not_keep_them() {
        let (pools, ring) = (Mutex::new(Pools::default()), std::cell::RefCell::new(Ring::new(16)));
        give(&pools, &ring, 4, 2, STABLE, 1);
        assert_eq!(ring.borrow().back, vec![(4, 2)]);
        let s = shape([true].into_iter());
        let a = take(&pools, &ring, 1, s, STABLE, 0).unwrap();
        give(&pools, &ring, a, 1, 0, 1);
        assert!(ring.borrow().back.contains(&(a, 1)));
        // What is kept goes back when asked (the switch went off).
        let b = take(&pools, &ring, 1, s, STABLE, 0).unwrap();
        give(&pools, &ring, b, 1, STABLE, 1);
        let before = ring.borrow().back.len();
        flush(&pools, |first, m| ring.borrow_mut().back.push((first, m)));
        assert_eq!(ring.borrow().back.len(), before + 1);
        assert_eq!(lock(&pools).pooled, 0);
    }
}
