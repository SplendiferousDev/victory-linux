//! What kind of view a slot of the shader-visible view heap holds, and the
//! rule that keeps a slot's kind still while submissions may read it.
//!
//! A shader that reads a constant buffer through a view heap slot holding
//! another kind of view (a texture view, a buffer view, or a null view of
//! either) makes NVIDIA's constant cache read an address decoded from that
//! view's bits: `Xid 31 ... GPCCLIENT_GCC faulted @ 0x5_8d249000` (a
//! texture view) or `@ 0x6_01492000` (a buffer view), measured with a
//! small program under vkd3d-proton (`crate::gpuwatch::slot_kind_changed`).
//! Two ways get there:
//!
//! - **A slot whose kind changes under a submission that reads it.** The
//!   title's descriptor writes reach the host heap at once (its view
//!   creation and copies on its own threads, the records it copies as
//!   bytes at the next submission, `crate::descriptor::sync_heaps`), while
//!   the submissions sent ahead (`crate::host::late`) still run. On the
//!   console the card reads a slot when it runs, and its card is never far
//!   behind; here flights stay in the air for a frame or more. Before the
//!   merge on the card (`VICTORY_MERGE_ON_CARD`) a submission found title
//!   memory changed under the flights' shader writes about 1.3 times a
//!   frame and settled them all, so the frame's first submission, which
//!   mirrors the next frame's records, mostly found nothing in the air.
//!   The merge takes that settle away. The rule here ([`before`]): a title
//!   slot that would change between a constant buffer view and another
//!   kind while submissions are in flight waits for them to land first
//!   (`crate::host::land_started`); a change within a kind (the console's
//!   case) does not wait. Our per-draw slots are not touched by the rule:
//!   a list holds its slots until its submission has landed.
//! - **A table entry read as a constant buffer that we made another kind.**
//!   The tables a shader reads constant buffers through are made afresh in
//!   our per-draw slots (`crate::draw::State::tables`); an entry with no
//!   view, a view of another kind, or no room for its constants was made a
//!   null texture view (or that view). Such an entry is now always a
//!   constant buffer view: of the other view's bytes if it is a buffer
//!   view, else a null one ([`table_constant`]).
//!
//! Nulls the host makes in a title slot (its view went away) keep the kind
//! the slot held (`HostHeap::make_null`).

use std::sync::atomic::{AtomicU64, Ordering};

/// Nothing written to the slot yet.
pub(crate) const NONE: u8 = 0;
/// A constant buffer view, or its null.
pub(crate) const CONSTANT: u8 = 1;
/// Any other view, or its null.
pub(crate) const OTHER: u8 = 2;

/// What a write of a view of kind `now` into a slot holding kind `was`
/// must do first: true to wait until the submissions in flight have
/// landed. Only for the title's slots (`dynamic`: one of our per-draw
/// slots, held by its list until its submission has landed), only for a
/// change of kind, and only with submissions in flight.
pub(crate) fn must_wait(on: bool, dynamic: bool, was: u8, now: u8, in_flight: u64) -> bool {
    on && !dynamic && was != NONE && now != NONE && was != now && in_flight > 0
}

/// Whether the rule is applied (`VICTORY_SLOT_KIND_WAIT`, on; `0` off:
/// such slots are written under the flights, as before, and counted).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_SLOT_KIND_WAIT").is_none_or(|v| v != "0");
        if !on {
            tracing::warn!("view heap slots of the title change kind under submissions in flight without a wait (VICTORY_SLOT_KIND_WAIT=0)");
        }
        on
    })
}

/// How a wait of [`gate`] ended (`crate::host::land_started`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Landed {
    /// Nothing was in the air any more.
    Clear,
    /// The flights in the air at the call landed.
    Waited,
    /// The flights could not be waited for here (no relay and the
    /// submission lock taken, or the thread that lands them): written
    /// under them.
    Unprotected,
    /// They had not landed after the longest wait: written under them.
    GaveUp,
}

/// Since the last frame line ([`line`]) and since the start: waits, of them
/// microseconds, writes left unprotected, waits given up.
static WAITS: [[AtomicU64; 4]; 2] = [const { [const { AtomicU64::new(0) }; 4] }; 2];

/// Since the last frame line and since the start: changes of kind of title
/// slots with submissions in flight, of our per-draw slots with
/// submissions in flight, of any slot with none.
static CHANGES: [[AtomicU64; 3]; 2] = [const { [const { AtomicU64::new(0) }; 3] }; 2];

/// Table entries read as constant buffers that were made a constant buffer
/// view all the same ([`table_constant`]), since the last frame line and
/// since the start, by what they held: no view, a buffer view (made a view
/// of its bytes), another view (a null), no room for the constants.
static TABLE: [[AtomicU64; 4]; 2] = [const { [const { AtomicU64::new(0) }; 4] }; 2];

fn add<const N: usize>(counts: &[[AtomicU64; N]; 2], i: usize, n: u64) {
    for c in counts {
        c[i].fetch_add(n, Ordering::Relaxed);
    }
}

/// Before title slot `index` (`dynamic`: one of our per-draw slots) that
/// holds kind `was` is written with kind `now`: waits for the submissions
/// in flight to land if [`must_wait`] says so.
pub(crate) fn gate(index: u32, dynamic: bool, was: u8, now: u8) {
    if !must_wait(on(), dynamic, was, now, crate::host::submissions_in_flight()) {
        return;
    }
    let started = std::time::Instant::now();
    let landed = crate::host::land_started();
    let us = started.elapsed().as_micros() as u64;
    match landed {
        Landed::Clear => {}
        Landed::Waited => {
            add(&WAITS, 0, 1);
            add(&WAITS, 1, us);
        }
        Landed::Unprotected => add(&WAITS, 2, 1),
        Landed::GaveUp => {
            add(&WAITS, 1, us);
            add(&WAITS, 3, 1);
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(index, us, "submissions in flight did not land before a view heap slot changed kind; written under them");
            });
        }
    }
}

/// Counts a slot's change of kind ([`crate::gpuwatch::slot_kind_changed`]).
pub(crate) fn changed(dynamic: bool, in_flight: u64) {
    add(&CHANGES, if in_flight == 0 { 2 } else { usize::from(dynamic) }, 1);
}

/// What a table entry read as a constant buffer held when it was made a
/// constant buffer view all the same.
#[derive(Clone, Copy)]
pub(crate) enum TableEntry {
    NoView = 0,
    BufferView = 1,
    OtherView = 2,
    NoRoom = 3,
}

/// Counts a table entry read as a constant buffer that held `what`.
pub(crate) fn table_constant(what: TableEntry) {
    add(&TABLE, what as usize, 1);
}

/// With the frame line: what happened since the last one, per frame over
/// `frames`. Nothing while nothing did.
pub(crate) fn line(frames: f64) {
    let waits = [0, 1, 2, 3].map(|i| WAITS[0][i].swap(0, Ordering::Relaxed));
    let changes = [0, 1, 2].map(|i| CHANGES[0][i].swap(0, Ordering::Relaxed));
    let table = [0, 1, 2, 3].map(|i| TABLE[0][i].swap(0, Ordering::Relaxed));
    if waits.iter().chain(&changes).chain(&table).all(|&n| n == 0) {
        return;
    }
    let f = |n: u64| n as f64 / frames;
    tracing::info!(
        "view heap slots per frame: changes between a constant buffer view and another kind: {:.1} of title slots and {:.1} of our \
         per-draw slots while submissions were in flight, {:.1} with none; waits for the flights before a title slot changed kind \
         (VICTORY_SLOT_KIND_WAIT; settles \"a view slot changed kind under flights\"): {:.2}, {:.2} ms waiting, {:.2} written under \
         flights all the same (not waitable there), {:.2} given up; table entries read as constant buffers made constant buffer \
         views (were a null texture view or their own kind): {:.1} without a view, {:.1} buffer views, {:.1} other views, {:.1} \
         without room for their constants",
        f(changes[0]),
        f(changes[1]),
        f(changes[2]),
        f(waits[0]),
        waits[1] as f64 / 1e3 / frames,
        f(waits[2]),
        f(waits[3]),
        f(table[0]),
        f(table[1]),
        f(table[2]),
        f(table[3]),
    );
}

/// For the stall and removal reports: the counts since the start.
pub(crate) fn section(out: &mut String) {
    use std::fmt::Write;
    let w = [0, 1, 2, 3].map(|i| WAITS[1][i].load(Ordering::Relaxed));
    let c = [0, 1, 2].map(|i| CHANGES[1][i].load(Ordering::Relaxed));
    let t = [0, 1, 2, 3].map(|i| TABLE[1][i].load(Ordering::Relaxed));
    let _ = write!(
        out,
        "; view slot kind (VICTORY_SLOT_KIND_WAIT={}): changes {} of title slots and {} of per-draw slots under flights, {} with none; \
         waits before a title slot changed kind {} ({} ms), {} written under flights all the same, {} given up; table entries read \
         as constant buffers made constant buffer views: {} without a view, {} buffer views, {} other views, {} without room",
        u8::from(on()),
        c[0],
        c[1],
        c[2],
        w[0],
        w[1] / 1000,
        w[2],
        w[3],
        t[0],
        t[1],
        t[2],
        t[3],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_slot_waits_only_when_its_kind_changes_under_flights() {
        // Constant buffer view -> another view, one submission in flight.
        assert!(must_wait(true, false, CONSTANT, OTHER, 1));
        // And back.
        assert!(must_wait(true, false, OTHER, CONSTANT, 3));
        // Nothing in flight: written at once.
        assert!(!must_wait(true, false, CONSTANT, OTHER, 0));
        // The same kind (the console's case: new contents, same kind).
        assert!(!must_wait(true, false, CONSTANT, CONSTANT, 5));
        assert!(!must_wait(true, false, OTHER, OTHER, 5));
        // A slot never written before.
        assert!(!must_wait(true, false, NONE, CONSTANT, 5));
        // Our per-draw slots: their list holds them until it has landed.
        assert!(!must_wait(true, true, CONSTANT, OTHER, 5));
        // The switch off.
        assert!(!must_wait(false, false, CONSTANT, OTHER, 5));
    }

    #[test]
    fn the_counts_go_to_the_line_and_to_the_report() {
        let before = TABLE[1][1].load(Ordering::Relaxed);
        table_constant(TableEntry::BufferView);
        assert_eq!(TABLE[1][1].load(Ordering::Relaxed) - before, 1);
        changed(false, 2);
        let mut out = String::new();
        section(&mut out);
        assert!(out.contains("view slot kind"), "{out}");
    }
}
