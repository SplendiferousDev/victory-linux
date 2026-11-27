//! Waits for our locks, per lock and by kind of thread (with `VICTORY_FRAME_TIMELINE=1`).
//!
//! The title's recording threads were found inside our list calls for 76 of 112 ms a frame
//! at about half a processor each, blocking some 1,200 times a frame each: a lock convoy is the
//! suspect. Here the locks of the record path, the submitting thread and the relay are taken
//! through [`lock`] (and [`read`] / [`write`] for read-write locks): with the switch off it is
//! the plain call; with it on, a `try_lock` first, and only an acquisition that found the lock
//! held is timed. Each lock is known by a name given where it is taken (one per lock, or one
//! per kind of lock held by many objects, such as a host copy's flights). The timeline's line
//! is followed by one line of the eight locks waited for longest, per frame.
//!
//! Under Wine a lock that is found held parks the thread in the kernel (a futex): each such
//! wait is a system call there and back, and the holder's release one more.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::time::Instant;

/// Kinds of thread, as they say of themselves ([`set_class`]); a thread that made a list call
/// is a recorder from then on (`crate::list`).
pub(crate) const RECORDER: u8 = 0;
pub(crate) const SUBMIT: u8 = 1;
pub(crate) const RELAY: u8 = 2;
const OTHER: u8 = 3;
const CLASSES: usize = 4;
const CLASS_NAMES: [&str; CLASSES] = ["recorders", "submit", "relay", "other"];

thread_local! {
    static CLASS: Cell<u8> = const { Cell::new(OTHER) };
}

/// This thread is of kind `class` (the first say is kept, but a recorder stays one).
pub(crate) fn set_class(class: u8) {
    if on() {
        CLASS.with(|c| {
            if c.get() == OTHER {
                c.set(class);
            }
        });
    }
}

fn on() -> bool {
    victory_core::timeline::on()
}

const SLOTS: usize = 256;

/// A lock's counts: acquisitions, those that found it held, and the nanoseconds they waited,
/// by kind of thread.
struct Slot {
    name: AtomicUsize,
    len: AtomicUsize,
    counts: [[AtomicU64; CLASSES]; 3],
}

static TABLE: [Slot; SLOTS] = [const {
    Slot {
        name: AtomicUsize::new(0),
        len: AtomicUsize::new(0),
        counts: [const { [const { AtomicU64::new(0) }; CLASSES] }; 3],
    }
}; SLOTS];
/// The places locks are taken from (`#[track_caller]` through [`lock`], [`read`], [`write`]
/// and the helpers that call them): the place, the lock's name, and its acquisitions, those
/// that found the lock held and the nanoseconds they waited, all threads.
struct Site {
    at: AtomicUsize,
    name: AtomicUsize,
    len: AtomicUsize,
    counts: [AtomicU64; 3],
}

static SITES: [Site; SLOTS] = [const {
    Site {
        at: AtomicUsize::new(0),
        name: AtomicUsize::new(0),
        len: AtomicUsize::new(0),
        counts: [const { AtomicU64::new(0) }; 3],
    }
}; SLOTS];

fn site(at: &'static std::panic::Location<'static>, name: &'static str) -> Option<&'static Site> {
    let key = std::ptr::from_ref(at) as usize;
    let start = (key >> 3).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56;
    for i in 0..SLOTS {
        let s = &SITES[(start + i) % SLOTS];
        match s.at.compare_exchange(0, key, Relaxed, Relaxed) {
            Ok(_) => {
                s.name.store(name.as_ptr() as usize, Relaxed);
                s.len.store(name.len(), Relaxed);
                return Some(s);
            }
            Err(was) if was == key => return Some(s),
            Err(_) => {}
        }
    }
    None
}

/// Frames since the last line.
static FRAMES: AtomicU64 = AtomicU64::new(0);

/// The slot of the lock named `name` (by the address of the name: one per place it is
/// written), taken at the first ask; `None` when the table is full.
fn slot(name: &'static str) -> Option<&'static Slot> {
    let key = name.as_ptr() as usize;
    let start = (key >> 3).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56;
    for i in 0..SLOTS {
        let s = &TABLE[(start + i) % SLOTS];
        match s.name.compare_exchange(0, key, Relaxed, Relaxed) {
            Ok(_) => {
                s.len.store(name.len(), Relaxed);
                return Some(s);
            }
            Err(was) if was == key => return Some(s),
            Err(_) => {}
        }
    }
    None
}

#[track_caller]
fn note(name: &'static str, waited: Option<Instant>) {
    let waited_ns = waited.map(|since| since.elapsed().as_nanos() as u64);
    if let Some(s) = site(std::panic::Location::caller(), name) {
        s.counts[0].fetch_add(1, Relaxed);
        if let Some(ns) = waited_ns {
            s.counts[1].fetch_add(1, Relaxed);
            s.counts[2].fetch_add(ns, Relaxed);
        }
    }
    let Some(s) = slot(name) else { return };
    let class = CLASS.with(Cell::get) as usize;
    s.counts[0][class].fetch_add(1, Relaxed);
    if let Some(ns) = waited_ns {
        s.counts[1][class].fetch_add(1, Relaxed);
        s.counts[2][class].fetch_add(ns, Relaxed);
    }
}

/// `m.lock()`, poison ignored; metered with the switch on.
#[track_caller]
pub(crate) fn lock<'a, T>(name: &'static str, m: &'a Mutex<T>) -> MutexGuard<'a, T> {
    if !on() {
        return m.lock().unwrap_or_else(|e| e.into_inner());
    }
    match m.try_lock() {
        Ok(g) => {
            note(name, None);
            g
        }
        Err(TryLockError::Poisoned(e)) => {
            note(name, None);
            e.into_inner()
        }
        Err(TryLockError::WouldBlock) => {
            let since = Instant::now();
            let g = m.lock().unwrap_or_else(|e| e.into_inner());
            note(name, Some(since));
            g
        }
    }
}

/// `m.read()`, poison ignored; metered with the switch on.
#[track_caller]
pub(crate) fn read<'a, T>(name: &'static str, m: &'a RwLock<T>) -> RwLockReadGuard<'a, T> {
    if !on() {
        return m.read().unwrap_or_else(|e| e.into_inner());
    }
    match m.try_read() {
        Ok(g) => {
            note(name, None);
            g
        }
        Err(TryLockError::Poisoned(e)) => {
            note(name, None);
            e.into_inner()
        }
        Err(TryLockError::WouldBlock) => {
            let since = Instant::now();
            let g = m.read().unwrap_or_else(|e| e.into_inner());
            note(name, Some(since));
            g
        }
    }
}

/// `m.write()`, poison ignored; metered with the switch on.
#[track_caller]
pub(crate) fn write<'a, T>(name: &'static str, m: &'a RwLock<T>) -> RwLockWriteGuard<'a, T> {
    if !on() {
        return m.write().unwrap_or_else(|e| e.into_inner());
    }
    match m.try_write() {
        Ok(g) => {
            note(name, None);
            g
        }
        Err(TryLockError::Poisoned(e)) => {
            note(name, None);
            e.into_inner()
        }
        Err(TryLockError::WouldBlock) => {
            let since = Instant::now();
            let g = m.write().unwrap_or_else(|e| e.into_inner());
            note(name, Some(since));
            g
        }
    }
}

/// A frame ended (the title's present returned).
pub(crate) fn frame() {
    if on() {
        FRAMES.fetch_add(1, Relaxed);
    }
}

/// One lock's figures since the last line, per frame.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    name: String,
    taken: [f64; CLASSES],
    contended: [f64; CLASSES],
    waited_ms: [f64; CLASSES],
}

fn take(frames: f64) -> Vec<Row> {
    let mut rows = Vec::new();
    for s in &TABLE {
        let key = s.name.load(Relaxed);
        if key == 0 {
            continue;
        }
        let len = s.len.load(Relaxed);
        // SAFETY: `key` and `len` are a `&'static str`'s address and length ([`slot`]); the
        // length is stored after the address, so a slot just taken may say 0 bytes for a moment.
        let name = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(key as *const u8, len)) };
        let per = |i: usize, scale: f64| std::array::from_fn(|c| s.counts[i][c].swap(0, Relaxed) as f64 / scale / frames);
        let row = Row { name: name.to_string(), taken: per(0, 1.0), contended: per(1, 1.0), waited_ms: per(2, 1e6) };
        if row.taken.iter().sum::<f64>() > 0.0 {
            rows.push(row);
        }
    }
    rows
}

fn text(mut rows: Vec<Row>, frames: u64) -> String {
    let total = |r: &Row| r.waited_ms.iter().sum::<f64>();
    rows.sort_by(|a, b| total(b).total_cmp(&total(a)));
    let all_waited: f64 = rows.iter().map(total).sum();
    let all_contended: f64 = rows.iter().map(|r| r.contended.iter().sum::<f64>()).sum();
    let shown: Vec<String> = rows
        .iter()
        .take(8)
        .map(|r| {
            let by: Vec<String> = (0..CLASSES)
                .filter(|&c| r.taken[c] > 0.0)
                .map(|c| {
                    format!("{} {:.0} taken {:.0} held {:.1} ms", CLASS_NAMES[c], r.taken[c], r.contended[c], r.waited_ms[c])
                })
                .collect();
            format!("{} {:.1} ms ({})", r.name, total(r), by.join(", "))
        })
        .collect();
    format!(
        "lock waits per frame over {frames} frames (VICTORY_FRAME_TIMELINE): {all_contended:.0} acquisitions found a lock held, \
         {all_waited:.1} ms waited in all; longest: {}",
        shown.join("; ")
    )
}

/// The line to log beside the timeline's, from the counts since the last one.
pub(crate) fn line() -> Option<String> {
    if !on() {
        return None;
    }
    let frames = FRAMES.swap(0, Relaxed);
    if frames == 0 {
        return None;
    }
    let sites = take_sites(frames as f64);
    Some(format!("{} | {}", text(take(frames as f64), frames), sites_text(sites)))
}

/// One place's figures since the last line, per frame: where, which lock, taken, found held,
/// ms waited.
#[derive(Debug, Clone, PartialEq)]
struct SiteRow {
    at: String,
    name: String,
    taken: f64,
    contended: f64,
    waited_ms: f64,
}

fn take_sites(frames: f64) -> Vec<SiteRow> {
    let mut rows = Vec::new();
    for s in &SITES {
        let key = s.at.load(Relaxed);
        let (name, len) = (s.name.load(Relaxed), s.len.load(Relaxed));
        if key == 0 || name == 0 {
            continue;
        }
        // SAFETY: `key` is a `&'static Location`'s address and `name`/`len` a `&'static str`'s
        // ([`site`]).
        let (at, name) = unsafe {
            (&*(key as *const std::panic::Location<'static>), std::str::from_utf8_unchecked(std::slice::from_raw_parts(name as *const u8, len)))
        };
        let [taken, contended, waited] = std::array::from_fn(|i| s.counts[i].swap(0, Relaxed) as f64 / frames);
        if taken > 0.0 {
            let file = at.file().rsplit(['/', '\\']).next().unwrap_or(at.file());
            rows.push(SiteRow { at: format!("{file}:{}", at.line()), name: name.to_string(), taken, contended, waited_ms: waited / 1e6 });
        }
    }
    rows
}

fn sites_text(mut rows: Vec<SiteRow>) -> String {
    rows.sort_by(|a, b| b.taken.total_cmp(&a.taken));
    let most: Vec<String> = rows
        .iter()
        .take(8)
        .map(|r| format!("{} ({}) {:.0} taken {:.0} held {:.1} ms", r.at, r.name, r.taken, r.contended, r.waited_ms))
        .collect();
    rows.sort_by(|a, b| b.waited_ms.total_cmp(&a.waited_ms));
    let waited: Vec<String> = rows
        .iter()
        .take(8)
        .filter(|r| r.waited_ms > 0.0)
        .map(|r| format!("{} ({}) {:.1} ms", r.at, r.name, r.waited_ms))
        .collect();
    format!("places taken most per frame: {}; waited longest: {}", most.join("; "), waited.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_sorted_by_wait_and_shown_by_class() {
        let row = |name: &str, waited: f64| Row {
            name: name.into(),
            taken: [10.0, 2.0, 0.0, 0.0],
            contended: [3.0, 1.0, 0.0, 0.0],
            waited_ms: [waited, 0.5, 0.0, 0.0],
        };
        let line = text(vec![row("small", 1.0), row("big", 9.0)], 5);
        assert!(line.contains("over 5 frames"), "{line}");
        assert!(line.contains("8 acquisitions found a lock held, 11.0 ms waited in all"), "{line}");
        let big = line.find("big 9.5 ms").expect("big shown");
        assert!(big < line.find("small 1.5 ms").expect("small shown"), "{line}");
        assert!(line.contains("recorders 10 taken 3 held 9.0 ms, submit 2 taken 1 held 0.5 ms)"), "{line}");
        assert!(!line.contains("relay 0"), "{line}");
    }

    #[test]
    fn places_are_sorted_by_takes_and_by_waits() {
        let row = |at: &str, taken: f64, waited_ms: f64| SiteRow {
            at: at.into(),
            name: "sh:BUFFERS".into(),
            taken,
            contended: 1.0,
            waited_ms,
        };
        let text = sites_text(vec![row("a.rs:1", 5.0, 9.0), row("b.rs:2", 50.0, 1.0), row("c.rs:3", 1.0, 0.0)]);
        let (most, waited) = text.split_once("; waited longest: ").expect("two parts");
        assert!(most.find("b.rs:2").unwrap() < most.find("a.rs:1").unwrap(), "{text}");
        assert!(waited.find("a.rs:1").unwrap() < waited.find("b.rs:2").unwrap(), "{text}");
        assert!(!waited.contains("c.rs:3"), "{text}");
    }

    #[test]
    fn a_name_keeps_its_slot_and_locks_work_with_the_meter_off() {
        static NAME: &str = "lockmeter test";
        let a = slot(NAME).map(|s| s as *const Slot);
        assert_eq!(a, slot(NAME).map(|s| s as *const Slot));
        let m = Mutex::new(1);
        *lock(NAME, &m) += 1;
        assert_eq!(*lock(NAME, &m), 2);
        let r = RwLock::new(3);
        *write(NAME, &r) += 1;
        assert_eq!(*read(NAME, &r), 4);
    }
}
