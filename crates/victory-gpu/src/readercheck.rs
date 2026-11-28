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

/// Since the last line (`crate::slotguard::line`) and since the start:
/// dynamic slots a list made a table in that were written again before
/// the list gave them back; of them, the ones that then held another kind;
/// stages that read a table at the title's own place past the host heap's
/// title slots.
static LANDING: [[AtomicU64; 3]; 2] = [const { [const { AtomicU64::new(0) }; 3] }; 2];

/// Whether a line may be said now ([`LINES_A_SECOND`]).
fn say_now() -> bool {
    let now = EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs();
    let mut said = SAID.load(Ordering::Relaxed);
    loop {
        let (say, next) = may_say(said, now);
        if !say {
            return false;
        }
        match SAID.compare_exchange_weak(said, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(seen) => said = seen,
        }
    }
}

/// Dynamic slot `index`, entry `entry` of a table of `n` slots that a list
/// of title pipeline `pipeline` made, was written again before the list
/// gave it back: its stamp (`HostHeap::stamp_of`: writes, and the kind in
/// the two low bits) was `was` when made and is `now`. The list's draws
/// read the slot as it is now. `when`: what looked.
pub(crate) fn rewritten(index: u32, entry: u32, n: u32, was: u32, now: u32, when: &'static str, pipeline: usize) {
    let (kind_was, kind_now) = ((was & 3) as u8, (now & 3) as u8);
    for c in &LANDING {
        c[0].fetch_add(1, Ordering::Relaxed);
        if kind_was != kind_now {
            c[1].fetch_add(1, Ordering::Relaxed);
        }
    }
    if !say_now() {
        return;
    }
    tracing::warn!(
        index,
        entry,
        table_slots = n,
        made_as = kind_name(kind_was),
        holds = kind_name(kind_now),
        writes_since = (now >> 2).wrapping_sub(was >> 2),
        pipeline = format_args!("{pipeline:#x}"),
        when,
        "reader check: a per-draw view heap slot was written again while the list that made a table in it still read it          (VICTORY_READER_CHECK)"
    );
}

/// A stage reads the table in root parameter `slot` at the title's own
/// place, title slot `index`, which lies past the host heap's `count`
/// title slots: its reads land in per-draw slots made for other tables, or
/// in the null slot at the heap's end.
pub(crate) fn past(index: u32, count: u32, slot: u8, pipeline: usize) {
    for c in &LANDING {
        c[2].fetch_add(1, Ordering::Relaxed);
    }
    if !say_now() {
        return;
    }
    SHADER.with_borrow(|shader| {
        tracing::warn!(
            index,
            count,
            root_parameter = slot,
            pipeline = format_args!("{pipeline:#x}"),
            shader = shader.as_str(),
            "reader check: a stage reads a table at the title's own place past the host heap's title slots: what it reads              there are per-draw slots made for other tables, or the null slot (VICTORY_READER_CHECK)"
        );
    });
}

/// For `crate::slotguard::line`: [`LANDING`] since the last call.
pub(crate) fn take_landing() -> [u64; 3] {
    [0, 1, 2].map(|i| LANDING[0][i].swap(0, Ordering::Relaxed))
}

/// For the reports: [`LANDING`] since the start.
pub(crate) fn landing_totals() -> [u64; 3] {
    [0, 1, 2].map(|i| LANDING[1][i].load(Ordering::Relaxed))
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

    /// Synthetic stamps: slot 9 made as a constant buffer view (kind 1,
    /// 4 writes), found a texture view (kind 2) after 2 more writes; slot
    /// 10 written again with its kind kept.
    #[test]
    fn slots_written_under_their_list_are_counted_by_whether_the_kind_moved() {
        let before = landing_totals();
        rewritten(9, 1, 4, 4 << 2 | 1, 6 << 2 | 2, "test", 0);
        rewritten(10, 2, 4, 4 << 2 | 1, 5 << 2 | 1, "test", 0);
        past(800_000, 737_855, 3, 0);
        let after = landing_totals();
        assert!(after[0] >= before[0] + 2);
        assert!(after[1] >= before[1] + 1);
        assert!(after[2] >= before[2] + 1);
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
