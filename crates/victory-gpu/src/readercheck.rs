//! `VICTORY_READER_CHECK=1` (off unless set): at each bind, every table
//! entry a stage reads as a constant buffer has its host slot's recorded
//! kind (`HostHeap::kind_of`) looked at. A slot that holds another kind of
//! view there (a texture view, a buffer view, a null view of either, or
//! nothing written yet: the heap starts as null texture views) is what
//! makes NVIDIA's constant cache fault (`crate::slotkind`). Each such read
//! is counted by the way the table was bound and a few a second are said
//! in the log ("reader check: ..."); the counts go to the slot kind line
//! and to the stall and removal reports.
//!
//! Also counted, switch or not: a list whose submission ran again after
//! its descriptor tables were given back (`crate::draw::State::tables_done`):
//! its draws then read per-draw slots other lists may have made anew.
//!
//! Off, a bind pays one relaxed load and a branch.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// How the table holding the entry was bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Path {
    /// Made afresh for this stage in per-draw slots.
    Fresh = 0,
    /// Found among the tables this list made before (the list's cache).
    Found = 1,
    /// Read in the title's own slots (not made afresh after all: a sampler
    /// heap, a table not on 32 bytes, a table past the per-draw span).
    InPlace = 2,
}

const PATHS: [&str; 3] = ["fresh", "cache hit", "in place"];

/// 0 off, 1 on, anything else not read yet.
static ON: AtomicU8 = AtomicU8::new(u8::MAX);

/// Whether the check runs (`VICTORY_READER_CHECK=1`).
#[inline]
pub(crate) fn on() -> bool {
    match ON.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => read_switch(),
    }
}

#[cold]
fn read_switch() -> bool {
    let on = std::env::var_os("VICTORY_READER_CHECK").is_some_and(|v| v == "1");
    if on {
        tracing::info!(
            "table entries read as constant buffers have their host slot's kind checked at each bind (VICTORY_READER_CHECK=1)"
        );
    }
    ON.store(u8::from(on), Ordering::Relaxed);
    on
}

/// Since the last slot kind line and since the start: reads as a constant
/// buffer of a slot holding another kind, by [`Path`].
static MISMATCHES: [[AtomicU64; 3]; 2] = [const { [const { AtomicU64::new(0) }; 3] }; 2];

/// Since the last slot kind line and since the start: lists that ran again
/// after their tables were given back.
static RERUNS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Lines said in the current second, and that second (since the start).
static SAID: AtomicU64 = AtomicU64::new(0);
static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Lines a second at most.
const LINES_A_SECOND: u64 = 4;

/// Whether a line may be said in second `now`, given the packed state
/// `said` (second in the high 40 bits, lines in the low 24); the new state.
fn may_say(said: u64, now: u64) -> (bool, u64) {
    let (second, lines) = (said >> 24, said & 0xff_ffff);
    if second != now {
        return (true, now << 24 | 1);
    }
    if lines < LINES_A_SECOND {
        return (true, said + 1);
    }
    (false, said)
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        crate::slotkind::NONE => "nothing written (a null texture view)",
        crate::slotkind::CONSTANT => "a constant buffer view",
        _ => "another kind of view (texture or buffer, or its null)",
    }
}

thread_local! {
    /// The shader whose tables are walked on this thread, for the log.
    static SHADER: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Names the shader whose tables this thread walks next (with [`on`]).
pub(crate) fn shader(hash: &str) {
    SHADER.with_borrow_mut(|s| {
        s.clear();
        s.push_str(hash);
    });
}

/// Checks that slot `index`, of kind `found`, read as a constant buffer at
/// entry `entry` of the table bound by `path` (title pipeline `pipeline`),
/// holds a constant buffer view. False, counted and maybe said, if not.
pub(crate) fn check(found: u8, path: Path, index: u32, entry: u32, pipeline: usize) -> bool {
    if found == crate::slotkind::CONSTANT {
        return true;
    }
    for c in &MISMATCHES {
        c[path as usize].fetch_add(1, Ordering::Relaxed);
    }
    let now = EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs();
    let mut said = SAID.load(Ordering::Relaxed);
    loop {
        let (say, next) = may_say(said, now);
        if !say {
            return false;
        }
        match SAID.compare_exchange_weak(said, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(seen) => said = seen,
        }
    }
    SHADER.with_borrow(|shader| {
        tracing::warn!(
            index,
            found = kind_name(found),
            pipeline = format_args!("{pipeline:#x}"),
            shader = shader.as_str(),
            entry,
            path = PATHS[path as usize],
            "reader check: a table entry a shader reads as a constant buffer is a slot holding another kind of view (VICTORY_READER_CHECK)"
        );
    });
    false
}

/// A list's submission ran again after its tables were given back.
pub(crate) fn rerun() {
    for c in &RERUNS {
        c.fetch_add(1, Ordering::Relaxed);
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::warn!("a list ran again after its descriptor tables were given back: its draws read per-draw slots others may have made anew");
    });
}

/// For the slot kind line: mismatches by path, then reruns, since the last
/// call.
pub(crate) fn take() -> [u64; 4] {
    [
        MISMATCHES[0][0].swap(0, Ordering::Relaxed),
        MISMATCHES[0][1].swap(0, Ordering::Relaxed),
        MISMATCHES[0][2].swap(0, Ordering::Relaxed),
        RERUNS[0].swap(0, Ordering::Relaxed),
    ]
}

/// For the reports: as [`take`], since the start.
pub(crate) fn totals() -> [u64; 4] {
    [
        MISMATCHES[1][0].load(Ordering::Relaxed),
        MISMATCHES[1][1].load(Ordering::Relaxed),
        MISMATCHES[1][2].load(Ordering::Relaxed),
        RERUNS[1].load(Ordering::Relaxed),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_few_lines_a_second() {
        let mut said = 0;
        let mut lines = 0;
        for _ in 0..10 {
            let (say, next) = may_say(said, 7);
            lines += u64::from(say);
            said = next;
        }
        assert_eq!(lines, LINES_A_SECOND);
        assert!(may_say(said, 8).0, "a new second says again");
    }

    #[test]
    fn a_constant_slot_passes_and_others_are_counted() {
        let before = totals();
        assert!(check(crate::slotkind::CONSTANT, Path::Found, 5, 1, 0));
        assert!(!check(crate::slotkind::OTHER, Path::Found, 5, 1, 0));
        assert!(!check(crate::slotkind::NONE, Path::InPlace, 6, 0, 0));
        let after = totals();
        assert!(after[1] > before[1] && after[2] > before[2]);
    }
}
