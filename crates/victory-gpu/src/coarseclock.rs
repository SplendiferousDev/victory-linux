//! The milliseconds since the start that [`crate::host::now_ms`] and [`crate::host::moment`] give,
//! read from a counter a thread of ours keeps (`VICTORY_COARSE_CLOCK=1`; `alt`: twenty seconds
//! with, twenty without, in turn, each change said in the log). Off by default.
//!
//! Why: under Wine `QueryPerformanceCounter` is a system call (the dispatcher saves and restores
//! the thread's whole register state on the way), and the record path asks for the time on
//! nearly every use of a host copy or host texture, to note when it was last used
//! (`moment`, in tenths of a second). In a profile of the title's five recording threads, 6 to
//! 7.5% of their time was that call and its dispatch. Those stamps want tenths of a second;
//! the counter here is at most about a millisecond behind.
//!
//! The counter only ever grows: the thread raises it to the precise time each millisecond, and
//! a precise reading taken while the switch is off in turn raises it too, so a reader never sees
//! the time go back when the switch turns on.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// The start of the counts: the first call of either clock.
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// The counter: milliseconds since [`EPOCH`], never less than any reading given so far.
static COARSE: AtomicU64 = AtomicU64::new(0);

/// Whether the counter is used now (the switch's state, kept by the counting thread).
static NOW_ON: AtomicBool = AtomicBool::new(false);

/// Whether the counting thread runs.
static STARTED: OnceLock<bool> = OnceLock::new();

static SWITCH: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_COARSE_CLOCK",
    "the time of the record path from a counter a thread of ours keeps instead of a system call each",
);

/// `0` off, `1` on, `2` in turn.
fn mode() -> u8 {
    SWITCH.mode()
}

/// Milliseconds since the start, asked of the system.
pub(crate) fn precise_ms() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Raises the counter to `ms` if it is lower (one write at most each millisecond).
fn raise(ms: u64) {
    raise_in(&COARSE, ms);
}

fn raise_in(counter: &AtomicU64, ms: u64) {
    if counter.load(Relaxed) < ms {
        counter.fetch_max(ms, Relaxed);
    }
}

/// The counting thread: the precise time each millisecond, and the switch's state.
fn count() {
    loop {
        raise(precise_ms());
        NOW_ON.store(SWITCH.on(), Relaxed);
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Starts the counting thread once; false if it could not be started.
fn started() -> bool {
    *STARTED.get_or_init(|| {
        raise(precise_ms());
        match std::thread::Builder::new().name("victory-clock".into()).spawn(count) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(error = %e, "the clock thread could not be started: the time is asked of the system");
                false
            }
        }
    })
}

/// Milliseconds since the start: the counter with the switch on, else asked of the system.
pub(crate) fn now_ms() -> u64 {
    match mode() {
        0 => precise_ms(),
        _ if !started() => precise_ms(),
        _ if NOW_ON.load(Relaxed) => COARSE.load(Relaxed),
        _ => {
            let ms = precise_ms();
            raise(ms);
            ms
        }
    }
}

/// Notes `moment` in `stamp` (a last-used time, [`crate::host::moment`]): written only when it
/// changes, so that threads noting the same moment on a shared object do not each take its cache
/// line (the moment changes once in a tenth of a second).
pub(crate) fn stamp(stamp: &AtomicU64, moment: u64) {
    if stamp.load(Relaxed) != moment {
        stamp.store(moment, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counter_follows_the_precise_time_and_never_goes_back() {
        assert!(started());
        let mut last = 0;
        let first = precise_ms();
        for _ in 0..200 {
            let coarse = COARSE.load(Relaxed);
            assert!(coarse >= last, "the counter went back: {coarse} after {last}");
            last = coarse;
            std::thread::sleep(Duration::from_micros(200));
        }
        // The thread keeps it close behind (a generous bound: a loaded test machine).
        std::thread::sleep(Duration::from_millis(20));
        let precise = precise_ms();
        let coarse = COARSE.load(Relaxed);
        assert!(coarse <= precise, "ahead of the precise time: {coarse} > {precise}");
        assert!(precise - coarse < 250, "far behind: {coarse} vs {precise}");
        assert!(coarse >= first);
    }

    #[test]
    fn a_stamp_holds_the_last_moment() {
        let s = AtomicU64::new(3);
        stamp(&s, 3);
        assert_eq!(s.load(Relaxed), 3);
        stamp(&s, 7);
        assert_eq!(s.load(Relaxed), 7);
    }

    #[test]
    fn raising_keeps_the_highest() {
        let counter = AtomicU64::new(5);
        raise_in(&counter, 1_000_000);
        raise_in(&counter, 10);
        assert_eq!(counter.load(Relaxed), 1_000_000);
    }

    #[test]
    fn unset_the_clock_is_the_systems() {
        // The test environment does not set the switch.
        if std::env::var_os("VICTORY_COARSE_CLOCK").is_none() {
            assert_eq!(mode(), 0);
            let a = precise_ms();
            let b = now_ms();
            assert!(b >= a);
        }
    }
}
