//! Waits for values in the title's memory, and the wakes that end them.
//!
//! The title waits for some values (fences, the GPU's bottom-of-pipe writes) with the console
//! CPU's `monitorx` / `mwaitx`, which the loader emulates; with `VICTORY_MWAITX=wait` a long
//! wait blocks in [`wait`] instead of spinning. The runtime's own writes of such values (in the
//! graphics and system-API crates, which do not see the loader) call [`written`], which wakes a
//! waiter on the same 64-byte line, as the processor's monitor would. A write that does not
//! call it (the title's own threads, say) is noticed when the wait's timeout runs out.
//!
//! The primitive is ntdll's `RtlWaitOnAddress`: on Windows and under Wine it parks the thread on
//! its per-thread alert (a futex under Wine, no wineserver call) and returns early on a wake or
//! when the value differs; `RtlWakeAddressAll` with nobody waiting is a short walk under a spin
//! lock, and [`written`] skips even that while no thread waits.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Threads that may wait at once with their address registered; one more still waits but
/// is woken by its timeout only.
const SLOTS: usize = 64;

/// The addresses waited on now (0: a free slot).
static AT: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
/// Waits under way (registered or not).
static WAITING: AtomicUsize = AtomicUsize::new(0);
/// Since the start: waits woken (or found the value changed), waits that ran to their
/// timeout, and the wakes [`written`] sent.
static WOKEN: AtomicU64 = AtomicU64::new(0);
static TIMED_OUT: AtomicU64 = AtomicU64::new(0);
static WAKES: AtomicU64 = AtomicU64::new(0);

/// How a [`wait`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// A wake, or the value was not (or no longer) the one given.
    Woken,
    /// The timeout ran out.
    TimedOut,
    /// The size was not 1, 2, 4 or 8: nothing waited.
    Refused,
}

/// Waits counted since the start: (woken, timed out, wakes sent).
pub fn counts() -> (u64, u64, u64) {
    (
        WOKEN.load(Ordering::Relaxed),
        TIMED_OUT.load(Ordering::Relaxed),
        WAKES.load(Ordering::Relaxed),
    )
}

/// Whether a write of `bytes` at `address` touches the line of a wait at `waited`.
fn touches(address: usize, bytes: usize, waited: usize) -> bool {
    let last = address.saturating_add(bytes.max(1) - 1);
    (address & !63) <= (waited & !63) && (waited & !63) <= (last & !63)
}

/// Blocks while the `size` bytes at `address` hold `current` (its low bytes), for at most
/// `timeout_us` microseconds, or until [`written`] names that line.
///
/// # Safety
/// The `size` bytes at `address` are readable for the whole call.
pub unsafe fn wait(address: usize, current: u64, size: usize, timeout_us: u64) -> Ended {
    if !matches!(size, 1 | 2 | 4 | 8) {
        return Ended::Refused;
    }
    // Registered before the value is compared (inside the wait): a writer either sees the
    // slot and wakes it, or wrote before, and the compare sees the new value.
    WAITING.fetch_add(1, Ordering::SeqCst);
    let slot = AT.iter().find(|s| {
        s.compare_exchange(0, address, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
    });
    // SAFETY: the caller's readable bytes, our own compare value of the same size.
    let ended = unsafe { sys::wait(address, current, size, timeout_us) };
    if let Some(slot) = slot {
        slot.store(0, Ordering::Release);
    }
    WAITING.fetch_sub(1, Ordering::Release);
    match ended {
        Ended::Woken => WOKEN.fetch_add(1, Ordering::Relaxed),
        Ended::TimedOut => TIMED_OUT.fetch_add(1, Ordering::Relaxed),
        Ended::Refused => 0,
    };
    ended
}

/// The runtime wrote `bytes` at `address` (a fence's value, a bottom-of-pipe write): a
/// [`wait`] on that line is woken. Call it after the store; cheap while nobody waits.
pub fn written(address: usize, bytes: usize) {
    // The store before this against the waiter's registration (see [`wait`]).
    std::sync::atomic::fence(Ordering::SeqCst);
    if WAITING.load(Ordering::Relaxed) == 0 {
        return;
    }
    for slot in &AT {
        let waited = slot.load(Ordering::Relaxed);
        if waited != 0 && touches(address, bytes, waited) {
            WAKES.fetch_add(1, Ordering::Relaxed);
            sys::wake(waited);
        }
    }
}

#[cfg(windows)]
mod sys {
    use super::Ended;

    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        /// `(address, compare, size, timeout)`: STATUS_SUCCESS on a wake or a value that
        /// differs, STATUS_TIMEOUT; the timeout in 100 ns units, negative: relative.
        fn RtlWaitOnAddress(
            address: *const u8,
            compare: *const u8,
            size: usize,
            timeout: *const i64,
        ) -> i32;
        fn RtlWakeAddressAll(address: *const u8);
    }

    const STATUS_TIMEOUT: i32 = 0x102;

    /// # Safety
    /// As [`super::wait`].
    pub(super) unsafe fn wait(address: usize, current: u64, size: usize, timeout_us: u64) -> Ended {
        // Little-endian: the low `size` bytes of `current` are its first ones.
        let compare = current.to_le_bytes();
        let timeout = -(timeout_us.min(i64::MAX as u64 / 10) as i64 * 10);
        // SAFETY: the caller's readable bytes; ours, of the same size.
        let status =
            unsafe { RtlWaitOnAddress(address as *const u8, compare.as_ptr(), size, &timeout) };
        if status == STATUS_TIMEOUT {
            Ended::TimedOut
        } else {
            Ended::Woken
        }
    }

    pub(super) fn wake(address: usize) {
        // SAFETY: the address is only a key to the waiters' table.
        unsafe { RtlWakeAddressAll(address as *const u8) }
    }
}

#[cfg(not(windows))]
mod sys {
    use super::Ended;

    /// # Safety
    /// As [`super::wait`].
    pub(super) unsafe fn wait(address: usize, current: u64, size: usize, timeout_us: u64) -> Ended {
        // A poll of the value: there is no title to wait for off Windows.
        let start = std::time::Instant::now();
        let mask = if size == 8 {
            u64::MAX
        } else {
            (1u64 << (size * 8)) - 1
        };
        loop {
            // SAFETY: readable, as the caller says.
            let now = unsafe {
                match size {
                    1 => u64::from(std::ptr::read_volatile(address as *const u8)),
                    2 => u64::from(std::ptr::read_volatile(address as *const u16)),
                    4 => u64::from(std::ptr::read_volatile(address as *const u32)),
                    _ => std::ptr::read_volatile(address as *const u64),
                }
            };
            if now != current & mask {
                return Ended::Woken;
            }
            if start.elapsed().as_micros() as u64 >= timeout_us {
                return Ended::TimedOut;
            }
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
    }

    pub(super) fn wake(_: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn a_write_wakes_waits_on_its_line_only() {
        assert!(touches(0x1000, 4, 0x1000));
        assert!(touches(0x1008, 8, 0x1030), "the same line");
        assert!(touches(0x103C, 8, 0x1040), "a write across two lines");
        assert!(!touches(0x1040, 8, 0x1000), "the next line");
        assert!(!touches(0x0FF8, 8, 0x1000), "the line before");
    }

    #[test]
    fn a_wait_on_a_value_that_differs_returns_at_once() {
        let value = AtomicU64::new(5);
        let start = Instant::now();
        // SAFETY: our own value, alive for the call.
        let ended = unsafe { wait(value.as_ptr() as usize, 4, 8, 2_000_000) };
        assert_eq!(ended, Ended::Woken);
        assert!(start.elapsed() < Duration::from_secs(1));
        // SAFETY: as above.
        assert_eq!(
            unsafe { wait(value.as_ptr() as usize, 5, 3, 1_000) },
            Ended::Refused
        );
    }

    #[test]
    fn a_wait_ends_at_its_timeout() {
        let value = AtomicU64::new(7);
        let start = Instant::now();
        // SAFETY: our own value, alive for the call.
        let ended = unsafe { wait(value.as_ptr() as usize, 7, 8, 20_000) };
        assert_eq!(ended, Ended::TimedOut);
        assert!(
            start.elapsed() >= Duration::from_millis(15),
            "{:?}",
            start.elapsed()
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    #[repr(align(64))]
    struct Line([AtomicU64; 8]);

    /// A thread waiting on the first word of `line` (holding 0) for up to 10 s, started once
    /// its wait is registered; it returns how its wait ended and how long it took.
    fn waiter(line: &Arc<Line>) -> std::thread::JoinHandle<(Ended, Duration)> {
        let address = line.0[0].as_ptr() as usize;
        let handle = {
            let line = Arc::clone(line);
            std::thread::spawn(move || {
                let start = Instant::now();
                // SAFETY: the line lives as long as the Arc this thread holds.
                let ended = unsafe { wait(line.0[0].as_ptr() as usize, 0, 8, 10_000_000) };
                (ended, start.elapsed())
            })
        };
        let start = Instant::now();
        while !AT.iter().any(|s| s.load(Ordering::SeqCst) == address)
            && start.elapsed() < Duration::from_secs(5)
        {
            std::thread::yield_now();
        }
        // Registered just before it compares and parks: a little time to get there (a wake
        // before it parks is lost, which the test would show as a timeout).
        std::thread::sleep(Duration::from_millis(100));
        handle
    }

    #[test]
    fn a_changed_value_and_its_wake_end_a_long_wait_early() {
        let line = Arc::new(Line([const { AtomicU64::new(0) }; 8]));
        let waiting = waiter(&line);
        line.0[0].store(1, Ordering::Release);
        // Another word of the same line, as a fence beside the monitored address.
        written(line.0[3].as_ptr() as usize, 8);
        let (ended, took) = waiting.join().unwrap();
        assert_eq!(ended, Ended::Woken);
        assert!(
            took < Duration::from_secs(5),
            "woken, not timed out: {took:?}"
        );
    }

    /// The wake alone (the value unchanged) ends the wait: it is the wake, not a poll.
    #[cfg(windows)]
    #[test]
    fn a_wake_alone_ends_a_wait() {
        let line = Arc::new(Line([const { AtomicU64::new(0) }; 8]));
        let waiting = waiter(&line);
        // Another line: nobody is woken.
        written(line.0[0].as_ptr() as usize + 64, 8);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !waiting.is_finished(),
            "a write on another line wakes nobody"
        );
        written(line.0[0].as_ptr() as usize, 4);
        let (ended, took) = waiting.join().unwrap();
        assert_eq!(ended, Ended::Woken);
        assert!(
            took < Duration::from_secs(5),
            "woken, not timed out: {took:?}"
        );
        let (woken, _, wakes) = counts();
        assert!(woken >= 1 && wakes >= 1);
    }
}
