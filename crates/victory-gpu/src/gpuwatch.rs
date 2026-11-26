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
    if gpu == title || gpu >> 48 != 0 || gpu < 0x1_0000 {
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
}
