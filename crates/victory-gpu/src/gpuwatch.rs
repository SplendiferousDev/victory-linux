//! Guards and notes for GPU page faults on our host copies.
//!
//! A run under `VKD3D_CONFIG=fault` lost the device to a compute shader
//! reading the first page of a 4 MiB window of title memory that the idle
//! sweep (`crate::shadow::let_go_idle`) had let go a moment before: the
//! host runtime called it "very likely use-after-free", and our address map
//! named the range as a window let go. The sweep lets a window go when no
//! list, slot or submission holds it, and frees it at once when nothing is
//! running on the card; something read it by its GPU address all the same.
//!
//! Two things here:
//! - **the hold** ([`hold`]): what the sweep lets go is freed only once a
//!   submission started after the sweep has finished and
//!   `VICTORY_SWEEP_HOLD_MS` (2,000 by default; 0: freed as before) have
//!   passed. A list recorded before the sweep that names the copy by its
//!   GPU address alone then runs while the copy is still there.
//! - **the ring of root views** ([`note_root`]): the last root views bound
//!   (the title's address, the host GPU address, constant or shader view),
//!   said in the stall report and when the host device is removed, each
//!   marked when its host address lies in a range let go since. A fault
//!   address from the kernel's log (`Xid 31 ... faulted @`) or from the host
//!   runtime can be looked up there and in the address map of our copies.
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// How long what the sweep lets go is held before it is freed, at least
/// (`VICTORY_SWEEP_HOLD_MS`; 0: freed at once when nothing runs).
pub(crate) fn hold_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        std::env::var("VICTORY_SWEEP_HOLD_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(2_000)
    })
}

struct Held {
    /// [`crate::host::now_us`] when it was let go.
    at: u64,
    /// The submission that must have finished: the first one started after
    /// the sweep.
    after: u64,
    bytes: u64,
    _object: Box<dyn std::any::Any>,
}

// SAFETY: what is held is our host copies (D3D12 objects, free-threaded,
// in reference-counted wrappers), dropped once, as `crate::host::retire`
// does with the same objects.
unsafe impl Send for Held {}

static HELD: Mutex<std::collections::VecDeque<Held>> = Mutex::new(std::collections::VecDeque::new());
/// Objects held and bytes held now, and objects held since the start.
static HELD_COUNT: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Holds `object` (`bytes` of host memory) until the submission after
/// `started` (the count of submissions started at the sweep) has finished
/// and [`hold_ms`] have passed (the caller asks [`hold_ms`] first: with 0
/// it frees as before).
pub(crate) fn hold<T: 'static>(object: T, bytes: u64, started: u64) {
    HELD_COUNT[0].fetch_add(1, Ordering::Relaxed);
    HELD_COUNT[1].fetch_add(bytes, Ordering::Relaxed);
    HELD_COUNT[2].fetch_add(1, Ordering::Relaxed);
    HELD.lock().unwrap_or_else(|e| e.into_inner()).push_back(Held {
        at: crate::host::now_us(),
        after: started + 1,
        bytes,
        _object: Box::new(object),
    });
}

/// Frees what was held long enough, now that submissions up to `finished`
/// have finished. Called where finished submissions bury what they kept.
pub(crate) fn release(finished: u64) {
    let now = crate::host::now_us();
    let due = |h: &Held| ready(h.after, h.at, finished, now, hold_ms());
    let done: Vec<Held> = {
        let Ok(mut held) = HELD.try_lock() else {
            return;
        };
        // In the order held: `after` and `at` only grow along the queue.
        let mut done = Vec::new();
        while held.front().is_some_and(due) {
            done.extend(held.pop_front());
        }
        done
    };
    for h in &done {
        HELD_COUNT[0].fetch_sub(1, Ordering::Relaxed);
        HELD_COUNT[1].fetch_sub(h.bytes, Ordering::Relaxed);
    }
    drop(done);
}

/// Whether a thing held at `at` until submission `after` finished may go,
/// with submissions up to `finished` finished at `now`.
fn ready(after: u64, at: u64, finished: u64, now: u64, hold_ms: u64) -> bool {
    finished >= after && now.saturating_sub(at) >= hold_ms * 1000
}

/// For the stall report and the memory line: objects and bytes held now,
/// and objects held since the start.
pub(crate) fn held() -> [u64; 3] {
    [0, 1, 2].map(|i| HELD_COUNT[i].load(Ordering::Relaxed))
}

/// The root views kept ([`note_root`]).
const RING: usize = 256;
/// Per entry: the title's address, the host GPU address, and the moment
/// ([`crate::host::now_us`]) with the kind in its low bits (bit 0 a shader
/// view, bit 1 compute).
static ROOTS: [[AtomicU64; 3]; RING] = [const { [const { AtomicU64::new(0) }; 3] }; RING];
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Notes a root view bound: `title` the title's address, `gpu` the host
/// GPU address it was bound as. A plain store or three: no lock.
pub(crate) fn note_root(compute: bool, srv: bool, title: u64, gpu: u64) {
    let i = NEXT.fetch_add(1, Ordering::Relaxed) % RING;
    let kind = u64::from(srv) | u64::from(compute) << 1;
    ROOTS[i][0].store(title, Ordering::Relaxed);
    ROOTS[i][1].store(gpu, Ordering::Relaxed);
    ROOTS[i][2].store(crate::host::now_us() << 2 | kind, Ordering::Relaxed);
    // A host address that is not one: the title's own (never translated)
    // or past the 48 bits of a GPU address.
    if !plausible(gpu, title) {
        static SAID: AtomicU64 = AtomicU64::new(0);
        if SAID.fetch_add(1, Ordering::Relaxed) < 8 {
            tracing::error!(
                title = format_args!("{title:#x}"),
                gpu = format_args!("{gpu:#x}"),
                srv,
                compute,
                "a root view bound at a host address that cannot be a copy of ours (the title's own address, or none)"
            );
        }
    }
}

/// Host GPU addresses asked of a copy for a title address outside it
/// ([`outside`]), since the start.
static OUTSIDE: AtomicU64 = AtomicU64::new(0);

/// Notes a host GPU address asked of a copy (title memory `start`, `len`
/// bytes, at host GPU address `gpu`) for a title `address` outside it: the
/// card would read or write memory that is not the copy's. Said at once
/// (the first 8), and counted for the stall report.
pub(crate) fn outside(address: u64, start: u64, len: u64, gpu: u64) {
    if OUTSIDE.fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::error!(
            title = format_args!("{address:#x}"),
            copy = format_args!("{start:#x}+{len:#x}"),
            host = format_args!("{gpu:#x}"),
            gives = format_args!("{:#x}", gpu.wrapping_add(address.wrapping_sub(start))),
            "a host GPU address asked of a copy for a title address outside it"
        );
    }
}

/// Views made that reach past the end of their copy ([`view_past_end`]),
/// since the start.
static PAST_END: AtomicU64 = AtomicU64::new(0);

/// Notes a view of `bytes` bytes made at title `address` whose copy has
/// only `left` bytes from there: the card may read past the copy. Said at
/// once (the first 8) and counted for the stall report.
pub(crate) fn view_past_end(what: &str, address: u64, left: u64, bytes: u64) {
    if left >= bytes {
        return;
    }
    if PAST_END.fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::error!(
            title = format_args!("{address:#x}"),
            left,
            bytes,
            "a {what} reaches past the end of its host copy"
        );
    }
}

/// View heap slots whose kind changed between a constant buffer view and
/// another view ([`slot_kind_changed`]), since the start: while submissions
/// were in flight, title slots and dynamic ones; with none in flight.
static KIND_CHANGES: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Notes that view heap slot `index` (`dynamic`: one of our per-draw
/// slots past the title's) changed between a constant buffer view and
/// another kind (`constant`: what it is now), with `in_flight` submissions
/// started and not finished.
///
/// What the card then does is known (measured on this PC with a small
/// program and NVIDIA's driver under vkd3d-proton with `VK_EXT_descriptor_heap`):
/// a shader that reads, with an index the same for all its threads, a
/// constant buffer through a slot holding a texture view takes `Xid 31
/// ... GPCCLIENT_GCC faulted @ 0x5_8d249000 ... FAULT_PDE`; through a slot
/// holding a buffer view, `@ 0x6_01492000` (addresses decoded from the
/// other view's bits, the same in every run). The title's runs show those
/// two addresses (and neighbours) mid-run and at their end. A list sent
/// before the change that reads the slot as a constant buffer, run after
/// it, is one way to get there; this says how often that can happen.
pub(crate) fn slot_kind_changed(index: u32, dynamic: bool, constant: bool, in_flight: u64) {
    let i = if in_flight == 0 {
        2
    } else {
        usize::from(dynamic)
    };
    let n = KIND_CHANGES[i].fetch_add(1, Ordering::Relaxed);
    crate::slotkind::changed(dynamic, in_flight);
    if in_flight > 0 && n < 16 {
        tracing::warn!(
            index,
            dynamic,
            now = if constant { "constant buffer view" } else { "another view" },
            in_flight,
            "a view heap slot changed between a constant buffer view and another kind while submissions are in flight"
        );
    }
}

/// Whether a host GPU address `gpu` can be one of a live copy of ours or
/// of the host runtime: not none, within the 48 bits of a GPU address, and
/// not the title's own `title` address passed on untranslated.
pub(crate) fn plausible(gpu: u64, title: u64) -> bool {
    gpu != title && gpu >> 48 == 0 && gpu >= 0x1_0000
}

/// The newest root views bound, newest first, each with the range let go
/// since that holds its host address, if one does.
pub(crate) fn roots_section(out: &mut String) {
    use std::fmt::Write;
    let now = crate::host::now_us();
    let next = NEXT.load(Ordering::Relaxed);
    let n = next.min(RING);
    let _ = write!(out, "{} bound since the start, the newest {n}, newest first:", next);
    let mut gone_hits = 0;
    for k in 1..=n {
        let i = (next - k) % RING;
        let title = ROOTS[i][0].load(Ordering::Relaxed);
        let gpu = ROOTS[i][1].load(Ordering::Relaxed);
        let stamp = ROOTS[i][2].load(Ordering::Relaxed);
        let (at, kind) = (stamp >> 2, stamp & 3);
        let ago = now.saturating_sub(at) as f64 / 1e6;
        let what = match kind {
            0 => "graphics constant",
            1 => "graphics shader view",
            2 => "compute constant",
            _ => "compute shader view",
        };
        let gone = crate::shadow::gone_holding(gpu, at);
        if gone.is_some() {
            gone_hits += 1;
        }
        // All that hit a range let go; of the others, the newest 48.
        if gone.is_none() && k > 48 {
            continue;
        }
        let _ = write!(out, " {title:#x}->{gpu:#x} {what} {ago:.2} s ago");
        if let Some(gone) = gone {
            let _ = write!(out, " (IN A RANGE LET GO AFTER IT WAS BOUND: {gone})");
        }
        out.push(';');
    }
    let _ = write!(out, " {gone_hits} of them in ranges let go after they were bound");
    let _ = write!(
        out,
        "; host addresses asked of a copy for a title address outside it: {}; views reaching past their copy's end: {}; \
         view slots changed between a constant buffer view and another kind: {} title slots and {} dynamic ones while submissions were in flight, {} with none",
        OUTSIDE.load(Ordering::Relaxed),
        PAST_END.load(Ordering::Relaxed),
        KIND_CHANGES[0].load(Ordering::Relaxed),
        KIND_CHANGES[1].load(Ordering::Relaxed),
        KIND_CHANGES[2].load(Ordering::Relaxed)
    );
    crate::slotkind::section(out);
    let [objects, bytes, ever] = held();
    let _ = write!(
        out,
        "; held after the sweep (VICTORY_SWEEP_HOLD_MS={}): {objects} now ({} MB), {ever} since the start",
        hold_ms(),
        bytes >> 20
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_copy_waits_for_a_later_submission_and_the_time() {
        // Let go at 1 s with 10 submissions started: the 11th must finish.
        let (after, at) = (11, 1_000_000);
        assert!(!ready(after, at, 10, at + 5_000_000, 2_000));
        assert!(!ready(after, at, 11, at + 1_999_999, 2_000));
        assert!(ready(after, at, 11, at + 2_000_000, 2_000));
        assert!(ready(after, at, 12, at + 3_000_000, 2_000));
        // With no time asked, the submission alone.
        assert!(ready(after, at, 11, at, 0));
    }

    #[test]
    fn a_host_address_with_bits_past_48_or_the_titles_own_is_not_one() {
        // A sign-extended one, as some fault addresses read.
        assert!(!plausible(0xffff_c005_8d24_9000, 0));
        assert!(!plausible(0x5_8d24_9000, 0x5_8d24_9000));
        assert!(!plausible(0, 0x5_8d24_9000));
        assert!(plausible(0x2_a6c0_0000, 0x5_8d24_9000));
    }

    #[test]
    fn a_slot_kind_change_is_counted_by_whether_flights_were_in_the_air() {
        let before = [0, 1, 2].map(|i| KIND_CHANGES[i].load(Ordering::Relaxed));
        slot_kind_changed(7, false, true, 3);
        slot_kind_changed(800_000, true, false, 1);
        slot_kind_changed(7, false, false, 0);
        let after = [0, 1, 2].map(|i| KIND_CHANGES[i].load(Ordering::Relaxed));
        assert_eq!([after[0] - before[0], after[1] - before[1], after[2] - before[2]], [1, 1, 1]);
    }
}
