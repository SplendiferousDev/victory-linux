//! Instructions the console's CPU (Zen 2) has and an Intel PC's lacks, emulated: the title's
//! code raises STATUS_ILLEGAL_INSTRUCTION on them (the title zeroes memory with a `clzero` loop at
//! its first frames), and the vectored handler steps over them here.
//!
//! Ported from XCL (github.com/ivall/xcl, GPL-2.0-only): `insn.rs` (decoder, as
//! `console_insn_decode.rs`, unchanged) and `eh.rs` `emulate_console_only`.

#[path = "console_insn_decode.rs"]
mod decode;

use decode::ConsoleOnly;
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;

const CONTINUE_EXECUTION: i32 = -1;

/// Whether what Windows said about memory is remembered per thread (the regions it found
/// writable, the code bytes it found readable) and not asked again at every emulated
/// instruction. A query walks the pages of the region it answers for: asked from outside, it
/// took 2-9 ms in the title's heaps of 0.6-2.2 GB and 89 ms in its 6.2 GB page pool (run 30),
/// and the title zeroes blocks of them with `clzero` loops all through a frame: six of its
/// threads spent 25-78 % of their time inside that query (runs 29 and 30), about 27 s of
/// processor time in Windows per 9 s frame. On by default; `VICTORY_CLZERO_ASK=1` asks every
/// time, as before.
fn remembers() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CLZERO_ASK").is_none_or(|v| v != "1"))
}

/// Answers a remembered region gives before Windows is asked about it again: one freed or
/// protected meanwhile is forgotten then.
const ANSWERS: u32 = 4096;

thread_local! {
    /// The regions Windows last said are committed and writable (start, end), newest first,
    /// and the answers they gave since they were emptied.
    static WRITABLE: Cell<([(usize, usize); 4], u32)> = const { Cell::new(([(0, 0); 4], 0)) };
    /// Faulting addresses and the code bytes found readable there, newest first.
    static CODE: Cell<[(usize, usize); 8]> = const { Cell::new([(0, 0); 8]) };
}

/// Instructions emulated, the times Windows was asked about memory for them, and the bytes
/// `clzero` loops zeroed, since the start.
static EMULATED: AtomicU64 = AtomicU64::new(0);
static ASKED: AtomicU64 = AtomicU64::new(0);
static ZEROED: AtomicU64 = AtomicU64::new(0);

/// Exceptions taken by kind since the last line: a lone `clzero`, the 12-byte loop, the
/// 15-byte loop taken whole, `monitorx`, `mwaitx`, everything else; and the 64-byte lines the
/// 15-byte loops zeroed (each was an exception of its own before they were taken whole).
static KINDS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
const K_CLZERO: usize = 0;
const K_LOOP12: usize = 1;
const K_LOOP15: usize = 2;
const K_MONITORX: usize = 3;
const K_MWAITX: usize = 4;
const K_OTHER: usize = 5;
const K_LOOP15_LINES: usize = 6;
/// `mwaitx` that blocked ([`block`]), and exceptions on bytes we had patched meanwhile
/// ([`is_patched`]).
const K_MWAITX_BLOCKED: usize = 7;
const K_PATCHED: usize = 8;

/// Waits the title made with `monitorx` / `mwaitx` (it waits for a fence that way, reading
/// the fence's value in memory: its +0x4fb0ad0) that have ended since the last line: how
/// many, their microseconds, and both again for those of 1 ms and more, 5 ms and more,
/// 20 ms and more.
static WAITS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
/// Waits said in the log one by one.
static WAITS_SAID: AtomicU64 = AtomicU64::new(0);
/// When the last line was written, in milliseconds of [`clock_us`].
static LINE_AT: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The address and the value this thread last armed a monitor for (`monitorx`: rax, and
    /// the title's wanted value in rsi), and what that address held then, if it was read
    /// ([`armed_value`]).
    static MONITOR: Cell<(usize, u64, Option<u64>)> = const { Cell::new((0, 0, None)) };
    /// The page this thread's last [`block`] found readable, and until when (microseconds of
    /// [`clock_us`]) a `monitorx` may read it without asking Windows again.
    static VERIFIED: Cell<(usize, u64)> = const { Cell::new((0, 0)) };
    /// This thread's wait under way: when it began and when its last `mwaitx` was, in
    /// microseconds, what it waits for, and its `mwaitx` count.
    static WAIT: Cell<(u64, u64, usize, u64, u32)> = const { Cell::new((0, 0, 0, 0, 0)) };
}

/// Microseconds on the performance counter.
fn clock_us() -> u64 {
    static PER_SECOND: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    let per_second = *PER_SECOND.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut f) };
        f.max(1)
    });
    let mut now = 0i64;
    // SAFETY: a valid out pointer.
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut now) };
    (now as u128 * 1_000_000 / per_second as u128) as u64
}

/// Whether events are said one by one now: four seconds of every two minutes of the wall
/// clock, the same seconds in which the fence meter of the graphics side says its own
/// (`victory_gpu::fence::pace`), so that the two can be laid side by side.
fn detail_now() -> bool {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).is_ok_and(|d| d.as_secs() % 120 < 4)
}

/// Whether the 15-byte `clzero` loop is taken whole (`VICTORY_CLZERO_LOOP15=0`: line by
/// line, as before).
fn loop15() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CLZERO_LOOP15").is_none_or(|v| v != "0"))
}

/// A wait of this thread has ended: counted, and said if it is one to say.
fn wait_ended(begin: u64, last: u64, address: usize, wanted: u64, spins: u32) {
    let us = last.saturating_sub(begin);
    for (i, least) in [0u64, 1_000, 5_000, 20_000].into_iter().enumerate() {
        if us >= least {
            WAITS[2 * i].fetch_add(1, Relaxed);
            WAITS[2 * i + 1].fetch_add(us, Relaxed);
        }
    }
    if us >= 500 && (detail_now() || WAITS_SAID.fetch_add(1, Relaxed) < 40) {
        // SAFETY: no preconditions.
        let thread = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        tracing::info!(
            "mwait: thread {thread} waited {:.1} ms for the value at {address:#x} to reach {wanted} ({spins} mwaitx)",
            us as f64 / 1e3
        );
    }
}

/// A `mwaitx` of this thread at `rip`: part of the wait under way, or the first of a new one.
/// Returns how long the wait has lasted so far, in microseconds.
fn wait_step(rip: usize) -> u64 {
    let now = clock_us();
    let (address, wanted, _) = MONITOR.with(Cell::get);
    let (begin, last, was_at, was_for, spins) = WAIT.with(Cell::get);
    if begin != 0 && was_at == address && was_for == wanted && now.saturating_sub(last) < pacing().gap_us() {
        WAIT.with(|w| w.set((begin, now, address, wanted, spins + 1)));
        waiting::note(rip, address, wanted, begin, now, spins + 1);
        return now - begin;
    }
    if begin != 0 {
        wait_ended(begin, last, was_at, was_for, spins);
    }
    WAIT.with(|w| w.set((now, now, address, wanted, 1)));
    waiting::note(rip, address, wanted, now, now, 1);
    0
}

/// How an emulated `mwaitx` waits: `VICTORY_MWAITX=wait` spins as the
/// processor's early return would for [`Pacing::spin_us`] of a wait on the same address and
/// value, then blocks on the monitored address ([`block`]) for at most
/// [`Pacing::timeout_us`] at a time (`VICTORY_MWAITX_SPIN_US`, `VICTORY_MWAITX_TIMEOUT_US`);
/// `VICTORY_MWAITX=spin` (the default, also when unset) takes an exception at every `mwaitx`
/// all through the wait, as before. The blocking wait is off by default since a run with it on
/// faulted on the card and the fault hung the host's graphics driver; it is not known to be
/// the cause, but it stays a choice until the faults are understood.
/// Under Wine an exception costs about 4 us of the waiting thread, and a long wait took
/// 250,000 of them a second.
///
/// `mwaitx` may return early for any reason, and the title checks its condition again after
/// it, so a block that ends early is what the processor may do too; one that ends late is
/// bounded by the timeout and by the title's own timer in EBX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pacing {
    block: bool,
    spin_us: u64,
    timeout_us: u64,
}

impl Pacing {
    fn parse(mode: Option<&str>, spin_us: Option<&str>, timeout_us: Option<&str>) -> Pacing {
        let number = |v: Option<&str>, default: u64, least: u64, most: u64| v.and_then(|v| v.trim().parse().ok()).unwrap_or(default).clamp(least, most);
        Pacing {
            block: mode.is_some_and(|m| m.trim().eq_ignore_ascii_case("wait")),
            spin_us: number(spin_us, 200, 0, 1_000_000),
            timeout_us: number(timeout_us, 1_000, MIN_BLOCK_US, 100_000),
        }
    }

    /// The longest gap between two `mwaitx` of one wait: 2 ms, longer when a block lies
    /// between them.
    fn gap_us(&self) -> u64 {
        if self.block { 2_000.max(2 * self.timeout_us + 1_000) } else { 2_000 }
    }

    /// How the `mwaitx` of a wait that has lasted `waited_us` waits: None spins (one more
    /// exception), Some blocks for that many microseconds. With ECX bit 1 the title's timer
    /// in EBX caps it: the processor counts it at its P0 clock, taken here as
    /// [`TICKS_PER_US`], faster than the console's or this PC's, so that it is never
    /// overslept; a timer too short to block for spins.
    fn plan(&self, waited_us: u64, ecx: u64, ebx: u64) -> Option<u64> {
        if !self.block || waited_us < self.spin_us {
            return None;
        }
        let mut timeout = self.timeout_us;
        if ecx & 2 != 0 {
            timeout = timeout.min((ebx & 0xFFFF_FFFF) / TICKS_PER_US);
        }
        (timeout >= MIN_BLOCK_US).then_some(timeout)
    }
}

/// Timer ticks of `mwaitx` (EBX) a microsecond: a 4 GHz clock.
const TICKS_PER_US: u64 = 4_000;
/// The shortest block: below it a block costs more than the exceptions it saves.
const MIN_BLOCK_US: u64 = 20;

fn pacing() -> &'static Pacing {
    static PACING: std::sync::OnceLock<Pacing> = std::sync::OnceLock::new();
    PACING.get_or_init(|| {
        let var = |name: &str| std::env::var(name).ok();
        let mode = var("VICTORY_MWAITX");
        // The tests of the blocking wait run it unless told otherwise.
        #[cfg(test)]
        let mode = mode.or_else(|| Some("wait".to_owned()));
        let p = Pacing::parse(mode.as_deref(), var("VICTORY_MWAITX_SPIN_US").as_deref(), var("VICTORY_MWAITX_TIMEOUT_US").as_deref());
        tracing::info!(
            "mwaitx: {} (VICTORY_MWAITX=wait|spin; spin {} us, then blocks of at most {} us: VICTORY_MWAITX_SPIN_US, VICTORY_MWAITX_TIMEOUT_US)",
            if p.block { "spins, then blocks on the monitored address" } else { "spins" },
            p.spin_us,
            p.timeout_us
        );
        p
    })
}

/// The size a block compares at `address`: 8 bytes, or 4 at an address of 4 only; None at
/// any other (spun).
fn wait_size(address: usize) -> Option<usize> {
    match address & 7 {
        0 => Some(8),
        4 => Some(4),
        _ => None,
    }
}

/// What `size` bytes at `address` hold.
///
/// # Safety
/// They are readable.
unsafe fn read_value(address: usize, size: usize) -> u64 {
    // SAFETY: readable, as the caller says; aligned to their size ([`wait_size`]).
    unsafe {
        if size == 8 {
            std::ptr::read_volatile(address as *const u64)
        } else {
            u64::from(std::ptr::read_volatile(address as *const u32))
        }
    }
}

/// What the address a `monitorx` arms holds now, read only in the page this thread's last
/// [`block`] found readable, a moment ago: so that a write between the `monitorx` and the
/// `mwaitx` ends the block at once, as it ends the processor's wait. Elsewhere None: the
/// block reads the value itself, and a write just before it costs up to its timeout.
///
/// shortcut: a page freed within a block's timeout of being found readable faults here,
/// inside the handler; the title reads the same address right after its `monitorx`, so it
/// would fault there all the same.
fn armed_value(address: usize, now: u64) -> Option<u64> {
    let size = wait_size(address)?;
    let (page, until) = VERIFIED.with(Cell::get);
    if page == 0 || address & !0xFFF != page || now >= until {
        return None;
    }
    // SAFETY: in the page found readable a moment ago; aligned, so inside it.
    Some(unsafe { read_value(address, size) })
}

/// Blocks this thread's `mwaitx` on the address its `monitorx` armed for at most
/// `timeout_us`, until the value there differs from the one armed (or read now) or the
/// runtime's write wakes it (`victory_core::wake::written`). False when it did not block:
/// an address it cannot compare, or memory not readable (the `mwaitx` spins).
fn block(timeout_us: u64) -> bool {
    let (address, _, armed) = MONITOR.with(Cell::get);
    let Some(size) = wait_size(address) else { return false };
    if !readable(address, size) {
        VERIFIED.with(|v| v.set((0, 0)));
        return false;
    }
    let now = clock_us();
    VERIFIED.with(|v| v.set((address & !0xFFF, now + 2 * timeout_us + 1_000)));
    // SAFETY: committed and readable, just asked.
    let current = armed.unwrap_or_else(|| unsafe { read_value(address, size) });
    KINDS[K_MWAITX_BLOCKED].fetch_add(1, Relaxed);
    waiting::blocked();
    // SAFETY: readable, just asked (the title waits there for a value: it is not freed
    // while it waits).
    unsafe { victory_core::wake::wait(address, current, size, timeout_us) };
    true
}

/// The waits of the title's threads, readable from another thread: a table
/// of slots, one per thread that ever used `mwaitx` (the first [`waiting::SLOTS`]
/// of them), each with its newest wait. For the stall report
/// ([`stall_report`]): a thread that spins in an emulated wait for a value
/// that never comes says nothing else.
mod waiting {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

    pub(super) const SLOTS: usize = 64;

    /// A thread's newest wait: the thread, the `mwaitx`'s address, the
    /// monitored address and the value the title wants there (rsi), when
    /// the wait began and its last `mwaitx` (microseconds), its `mwaitx`
    /// count. Written by its thread alone; a reader may see a wait half
    /// changed, which a report can bear.
    pub(super) struct Slot {
        pub(super) thread: AtomicU32,
        pub(super) rip: AtomicU64,
        pub(super) address: AtomicU64,
        pub(super) wanted: AtomicU64,
        pub(super) begin: AtomicU64,
        pub(super) last: AtomicU64,
        pub(super) spins: AtomicU64,
        /// How many of its `mwaitx` blocked ([`super::block`]).
        pub(super) blocked: AtomicU64,
    }

    pub(super) static TABLE: [Slot; SLOTS] = [const {
        Slot {
            thread: AtomicU32::new(0),
            rip: AtomicU64::new(0),
            address: AtomicU64::new(0),
            wanted: AtomicU64::new(0),
            begin: AtomicU64::new(0),
            last: AtomicU64::new(0),
            spins: AtomicU64::new(0),
            blocked: AtomicU64::new(0),
        }
    }; SLOTS];

    thread_local! {
        /// This thread's slot: [`SLOTS`] + 1 not looked for yet, [`SLOTS`]
        /// the table was full.
        static MINE: Cell<usize> = const { Cell::new(SLOTS + 1) };
    }

    fn mine() -> Option<&'static Slot> {
        let mut i = MINE.with(Cell::get);
        if i > SLOTS {
            // SAFETY: no preconditions.
            let thread = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
            i = TABLE
                .iter()
                .position(|s| s.thread.load(Relaxed) == thread || s.thread.compare_exchange(0, thread, Relaxed, Relaxed).is_ok())
                .unwrap_or(SLOTS);
            MINE.with(|m| m.set(i));
        }
        TABLE.get(i)
    }

    pub(super) fn note(rip: usize, address: usize, wanted: u64, begin: u64, last: u64, spins: u32) {
        let Some(slot) = mine() else { return };
        if spins == 1 {
            slot.rip.store(rip as u64, Relaxed);
            slot.address.store(address as u64, Relaxed);
            slot.wanted.store(wanted, Relaxed);
            slot.begin.store(begin, Relaxed);
            slot.blocked.store(0, Relaxed);
        }
        slot.last.store(last, Relaxed);
        slot.spins.store(u64::from(spins), Relaxed);
    }

    /// This thread's newest `mwaitx` blocks.
    pub(super) fn blocked() {
        if let Some(slot) = mine() {
            slot.blocked.fetch_add(1, Relaxed);
        }
    }
}

/// One thread's wait as the report reads it: thread, `mwaitx` address,
/// monitored address, wanted value, began and last (microseconds), count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Waiting {
    thread: u32,
    rip: usize,
    address: usize,
    wanted: u64,
    begin: u64,
    last: u64,
    spins: u64,
    blocked: u64,
}

/// The waits still under way at `now` (a `mwaitx` in the last two seconds),
/// longest first, each with the value at its address now (`read`; None: not
/// readable) and where its code lies (`place`).
fn describe_waits(waits: &[Waiting], now: u64, read: impl Fn(usize) -> Option<u64>, place: impl Fn(usize) -> String, out: &mut String) {
    use std::fmt::Write;
    let mut under_way: Vec<&Waiting> = waits.iter().filter(|w| w.thread != 0 && w.last != 0 && now.saturating_sub(w.last) < 2_000_000).collect();
    under_way.sort_by_key(|w| w.begin);
    let _ = write!(out, "{} title threads in an emulated mwaitx wait now ({} ever used it)", under_way.len(), waits.iter().filter(|w| w.thread != 0).count());
    for w in under_way.iter().take(16) {
        let seconds = w.last.saturating_sub(w.begin) as f64 / 1e6;
        let _ = write!(
            out,
            "; thread {} at {} waits {seconds:.1} s for the value at {:#x} (wants {:#x}, holds {}), {} mwaitx ({:.0} a second{})",
            w.thread,
            place(w.rip),
            w.address,
            w.wanted,
            read(w.address).map_or_else(|| "not readable".to_owned(), |v| format!("{v:#x}")),
            w.spins,
            if seconds > 0.0 { w.spins as f64 / seconds } else { 0.0 },
            if w.blocked > 0 { format!(", {} of them blocked", w.blocked) } else { String::new() },
        );
    }
}

/// The stall report's section of the title's waits (`victory_gpu::stall`):
/// every thread inside an emulated `mwaitx` wait now, on which address and
/// value, for how long; and the exception counts since the last line.
pub fn stall_report(out: &mut String) {
    use std::fmt::Write;
    let waits: Vec<Waiting> = waiting::TABLE
        .iter()
        .map(|s| Waiting {
            thread: s.thread.load(Relaxed),
            rip: s.rip.load(Relaxed) as usize,
            address: s.address.load(Relaxed) as usize,
            wanted: s.wanted.load(Relaxed),
            begin: s.begin.load(Relaxed),
            last: s.last.load(Relaxed),
            spins: s.spins.load(Relaxed),
            blocked: s.blocked.load(Relaxed),
        })
        .collect();
    let read = |address: usize| -> Option<u64> {
        use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut value = 0u64;
        let mut done = 0;
        // SAFETY: our own 8 bytes; the kernel checks the source.
        let ok = unsafe { ReadProcessMemory(GetCurrentProcess(), address as *const _, (&raw mut value).cast(), 8, &mut done) } != 0;
        ok.then_some(value)
    };
    let place = |rip: usize| -> String {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: plain data filled in by the query.
        let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(rip as *const _, &mut m, size_of_val(&m)) } == 0 {
            return format!("{rip:#x}");
        }
        format!("{rip:#x} (image {:#x} +{:#x})", m.AllocationBase as usize, rip - m.AllocationBase as usize)
    };
    describe_waits(&waits, clock_us(), read, place, out);
    let _ = write!(
        out,
        "; since the last exceptions line: monitorx {}, mwaitx {} ({} blocked), clzero loops {}; emulated since the start {}",
        KINDS[K_MONITORX].load(Relaxed),
        KINDS[K_MWAITX].load(Relaxed),
        KINDS[K_MWAITX_BLOCKED].load(Relaxed),
        KINDS[K_LOOP12].load(Relaxed) + KINDS[K_LOOP15].load(Relaxed) + KINDS[K_CLZERO].load(Relaxed),
        EMULATED.load(Relaxed),
    );
}

/// A line every five seconds: exceptions a second by kind, and the waits that ended.
fn line() {
    let now = clock_us() / 1000;
    let last = LINE_AT.load(Relaxed);
    if now < last + 5_000 || LINE_AT.compare_exchange(last, now, Relaxed, Relaxed).is_err() {
        return;
    }
    if last == 0 {
        return;
    }
    let seconds = (now - last) as f64 / 1e3;
    let k: Vec<f64> = KINDS.iter().map(|k| k.swap(0, Relaxed) as f64 / seconds).collect();
    let w: Vec<f64> = WAITS.iter().map(|w| w.swap(0, Relaxed) as f64).collect();
    // The blocks' ends and the runtime's wakes, a second (counted since the start there).
    static WAKES_AT: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
    let (woken, timed_out, wakes) = victory_core::wake::counts();
    let b: Vec<f64> = [woken, timed_out, wakes].iter().zip(&WAKES_AT).map(|(&n, at)| n.saturating_sub(at.swap(n, Relaxed)) as f64 / seconds).collect();
    tracing::info!(
        "console CPU instructions, exceptions a second: clzero alone {:.0}, its 12-byte loop {:.0}, its 15-byte loop taken whole {:.0} ({:.0} lines a second, each an exception \
         before; VICTORY_CLZERO_LOOP15=0: again), monitorx {:.0}, mwaitx {:.0}, others {:.0} | mwaitx waits ended a second: {:.0}, {:.1} ms of thread time a second; of 1 ms and \
         more {:.1} ({:.1} ms), of 5 ms and more {:.1} ({:.1} ms), of 20 ms and more {:.1} ({:.1} ms) | mwaitx blocked {:.0} a second (woken or changed {:.0}, timed \
         out {:.0}; VICTORY_MWAITX=spin: none), wakes sent {:.0}, patched bytes met {:.0}",
        k[K_CLZERO],
        k[K_LOOP12],
        k[K_LOOP15],
        k[K_LOOP15_LINES],
        k[K_MONITORX],
        k[K_MWAITX],
        k[K_OTHER],
        w[0] / seconds,
        w[1] / seconds / 1e3,
        w[2] / seconds,
        w[3] / seconds / 1e3,
        w[4] / seconds,
        w[5] / seconds / 1e3,
        w[6] / seconds,
        w[7] / seconds / 1e3,
        k[K_MWAITX_BLOCKED],
        b[0],
        b[1],
        b[2],
        k[K_PATCHED],
    );
}

fn readable(va: usize, bytes: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let found = unsafe { VirtualQuery(va as *const _, &mut m, size_of_val(&m)) } != 0;
    ASKED.fetch_add(1, Relaxed);
    found && m.State == MEM_COMMIT && m.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0 && m.BaseAddress as usize + m.RegionSize >= va + bytes
}

fn writable(va: usize, bytes: usize) -> bool {
    writable_to(va, va + bytes) == Some(va + bytes)
}

/// Where the committed, writable memory that `va` lies in ends, at most `end`; None if `va`
/// is not in such memory. A region found is remembered ([`remembers`]).
/// shortcut: a remembered region that was freed or protected since is written all the same
/// for up to [`ANSWERS`] answers: the write then faults inside the handler, which the guard
/// on view memory opens as it does any write, and which is a crash otherwise (as the
/// unchecked single-line emulation behind this one already is).
fn writable_to(va: usize, end: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE, PAGE_READWRITE, VirtualQuery};
    let remember = remembers();
    if remember {
        let (regions, answers) = WRITABLE.with(Cell::get);
        if let Some(&(_, to)) = regions.iter().find(|&&(from, to)| from <= va && va < to) {
            let next = if answers >= ANSWERS { ([(0, 0); 4], 0) } else { (regions, answers + 1) };
            WRITABLE.with(|w| w.set(next));
            return Some(to.min(end));
        }
    }
    // SAFETY: plain data filled in by the query.
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let found = unsafe { VirtualQuery(va as *const _, &mut m, size_of_val(&m)) } != 0;
    ASKED.fetch_add(1, Relaxed);
    if !found || m.State != MEM_COMMIT || m.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) == 0 {
        return None;
    }
    let (from, to) = (m.BaseAddress as usize, m.BaseAddress as usize + m.RegionSize);
    if remember {
        WRITABLE.with(|w| {
            let (mut regions, answers) = w.get();
            regions.rotate_right(1);
            regions[0] = (from, to);
            w.set((regions, answers));
        });
    }
    Some(to.min(end))
}

/// The register `r` of `ctx` (CONTEXT order: rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8..r15).
fn register(ctx: &mut CONTEXT, r: u8) -> &mut u64 {
    match r & 15 {
        0 => &mut ctx.Rax,
        1 => &mut ctx.Rcx,
        2 => &mut ctx.Rdx,
        3 => &mut ctx.Rbx,
        4 => &mut ctx.Rsp,
        5 => &mut ctx.Rbp,
        6 => &mut ctx.Rsi,
        7 => &mut ctx.Rdi,
        8 => &mut ctx.R8,
        9 => &mut ctx.R9,
        10 => &mut ctx.R10,
        11 => &mut ctx.R11,
        12 => &mut ctx.R12,
        13 => &mut ctx.R13,
        14 => &mut ctx.R14,
        _ => &mut ctx.R15,
    }
}

/// Where the lines of the loop `mov rax, ptr; add ptr, 0x40; clzero; cmp ptr, end; jb` end,
/// from the state the loop has at its `clzero`: the line at `rax` is zeroed now, and one
/// more at `ptr` for every pass the `cmp` sends back. None if this is not the plain case
/// the loop makes (the line at `rax` on a 64-byte boundary, `ptr` the next line, under
/// 2 GiB in all): then every line is taken alone, as the instruction itself does it.
fn loop15_end(rax: usize, ptr: usize, end: usize) -> Option<usize> {
    if rax & 63 != 0 || ptr != rax.wrapping_add(64) || ptr < rax {
        return None;
    }
    let more = if ptr < end { (end - ptr).div_ceil(64) } else { 0 };
    let last = ptr.checked_add(more.checked_mul(64)?)?;
    (last - rax < 2 << 30).then_some(last)
}

/// The `clzero` at `rip` in the loop `mov rax, ptr; add ptr, 0x40; clzero; cmp ptr, end; jb`,
/// with all the lines the loop would still zero: `Some(true)` when they are zeroed and the
/// registers stand as the loop leaves them at its last `clzero` (the caller steps over the
/// instruction; the title's own `cmp` and `jb` then leave the loop), `Some(false)` when the
/// first line is not writable (the real instruction faults there too), and when only a
/// first part was writable the registers stand at the line after it, where the instruction
/// runs again. None if it is not that loop: the lone instruction is emulated.
///
/// # Safety
/// `rip` is the faulting instruction of `ctx`, whose bytes were read.
unsafe fn clzero_loop15(ctx: &mut CONTEXT, rip: usize, ptr_reg: u8, end_reg: u8) -> Option<bool> {
    // The seven bytes in front of it: in the same page as the instruction, which ran.
    if rip & 0xFFF < 7 && !readable(rip - 7, 7) {
        return None;
    }
    // SAFETY: readable, as checked or as the instruction's own page.
    let before = unsafe { std::slice::from_raw_parts((rip - 7) as *const u8, 7) };
    if !decode::loop15_head(before, ptr_reg) || end_reg == 0 || end_reg == ptr_reg {
        return None;
    }
    let rax = ctx.Rax as usize;
    let ptr = *register(ctx, ptr_reg) as usize;
    let end = *register(ctx, end_reg) as usize;
    let last = loop15_end(rax, ptr, end)?;
    // Region by region, as the 12-byte loop: a block may lie across several views.
    let mut at = rax;
    while at < last {
        let Some(to) = writable_to(at, last) else { break };
        // A region ends on a page boundary, so `to` is on a line boundary as `at` is.
        // SAFETY: committed and writable from `at` to `to`.
        unsafe { std::ptr::write_bytes(at as *mut u8, 0, to - at) };
        at = to;
    }
    if at == rax {
        return Some(false);
    }
    ZEROED.fetch_add((at - rax) as u64, Relaxed);
    KINDS[K_LOOP15_LINES].fetch_add(((at - rax) / 64) as u64, Relaxed);
    if at < last {
        // What follows is not writable: the loop stands at that line's `clzero` now, where
        // the real one faults too. The instruction runs again (the caller must not step).
        ctx.Rax = at as u64;
        *register(ctx, ptr_reg) = (at + 64) as u64;
        ctx.Rip = (rip as u64).wrapping_sub(3);
        return Some(true);
    }
    ctx.Rax = (last - 64) as u64;
    *register(ctx, ptr_reg) = last as u64;
    Some(true)
}

/// One NOP instruction of each length from 1 to 9 bytes, as AMD's and Intel's manuals
/// recommend them: what [`patch_nop`] writes over an instruction of that length.
const NOPS: [&[u8]; 9] = [
    &[0x90],
    &[0x66, 0x90],
    &[0x0F, 0x1F, 0x00],
    &[0x0F, 0x1F, 0x40, 0x00],
    &[0x0F, 0x1F, 0x44, 0x00, 0x00],
    &[0x66, 0x0F, 0x1F, 0x44, 0x00, 0x00],
    &[0x0F, 0x1F, 0x80, 0x00, 0x00, 0x00, 0x00],
    &[0x0F, 0x1F, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
    &[0x66, 0x0F, 0x1F, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
];

/// Whether `code`, the bytes at an illegal instruction's address, now begin with a NOP that
/// [`patch_nop`] writes: another thread patched the instruction after this one faulted on
/// it. A NOP never faults, so the bytes there changed since the fault.
fn is_patched(code: &[u8]) -> bool {
    NOPS.iter().any(|nop| code.starts_with(nop))
}

/// Where the instruction `bytes` at `rip` can be patched with one store: the aligned
/// 8-byte word that holds all of it, and its offset there. None for one across two words
/// or longer than the NOPs: another thread could run half old, half new bytes.
fn patch_word(rip: usize, len: usize) -> Option<(usize, usize)> {
    let word = rip & !7;
    ((1..=NOPS.len()).contains(&len) && rip + len <= word + 8).then_some((word, rip - word))
}

/// The word `before` with its `len` bytes from `offset` replaced by the NOP of that length.
fn with_nop(before: u64, offset: usize, len: usize) -> u64 {
    let mut bytes = before.to_le_bytes();
    bytes[offset..offset + len].copy_from_slice(NOPS[len - 1]);
    u64::from_le_bytes(bytes)
}

/// Replaces the instruction `old` at `rip` with one NOP of the same length ([`NOPS`]), in a
/// single 8-byte store, so that another thread runs either the old instruction (and faults,
/// which [`is_patched`] then steps) or the NOP, never a mix. The page is writable only around
/// the store, one patch at a time (two at once would restore each other's protection); an
/// instruction that cannot be patched so, or one met while another is patched, is stepped
/// over as before, at an exception each time.
///
/// # Safety
/// `old` are the bytes just read at `rip`, inside the instruction's own page.
unsafe fn patch_nop(rip: usize, old: &[u8]) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering::{Acquire, Release, SeqCst}};
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    static PATCHING: AtomicBool = AtomicBool::new(false);
    let Some((word, offset)) = patch_word(rip, old.len()) else { return false };
    if PATCHING.swap(true, Acquire) {
        return false;
    }
    let mut done = false;
    let mut was = 0u32;
    // SAFETY: the instruction's own word (aligned, so in its page), writable around the store
    // and restored; the store is atomic, so no thread fetches a torn instruction.
    unsafe {
        if VirtualProtect(word as *const _, 8, PAGE_EXECUTE_READWRITE, &mut was) != 0 {
            let cell = AtomicU64::from_ptr(word as *mut u64);
            let before = cell.load(SeqCst);
            // Only if the bytes are still the instruction decoded (not patched meanwhile).
            if before.to_le_bytes()[offset..offset + old.len()] == *old {
                done = cell.compare_exchange(before, with_nop(before, offset, old.len()), SeqCst, SeqCst).is_ok();
            }
            VirtualProtect(word as *const _, 8, was, &mut was);
            FlushInstructionCache(GetCurrentProcess(), rip as *const _, old.len());
        }
    }
    PATCHING.store(false, Release);
    done
}

/// An illegal instruction at `ctx.Rip`: a console-only one is emulated and stepped over
/// (`CONTINUE_EXECUTION`); anything else returns 0 (the crash report and the title's handlers
/// see it).
///
/// # Safety
/// `ctx` is the faulting thread's context, from the exception being dispatched.
pub(crate) unsafe fn emulate(ctx: &mut CONTEXT) -> i32 {
    let rip = ctx.Rip as usize;
    // The same few instructions fault again and again: their bytes are asked about once.
    let known = if remembers() { CODE.with(Cell::get) } else { [(0, 0); 8] };
    let n = match known.iter().find(|&&(at, n)| at == rip && n > 0) {
        Some(&(_, n)) => n,
        None => {
            if !readable(rip, 1) {
                return 0;
            }
            let n = (1..=16).rev().find(|&n| readable(rip, n)).unwrap_or(0);
            if remembers() {
                let mut known = known;
                known.rotate_right(1);
                known[0] = (rip, n);
                CODE.with(|c| c.set(known));
            }
            n
        }
    };
    // SAFETY: `n` bytes at `rip` are readable.
    let code = unsafe { std::slice::from_raw_parts(rip as *const u8, n) };
    let Some((what, len)) = decode::decode(code) else {
        if is_patched(code) {
            // Patched by another thread after this one faulted on the instruction: it runs
            // the bytes there now (a NOP never faults, so this cannot come back).
            KINDS[K_PATCHED].fetch_add(1, Relaxed);
            return CONTINUE_EXECUTION;
        }
        return 0;
    };
    static SEEN: std::sync::Mutex<Vec<std::mem::Discriminant<ConsoleOnly>>> = std::sync::Mutex::new(Vec::new());
    if let Ok(mut seen) = SEEN.lock()
        && !seen.contains(&std::mem::discriminant(&what))
    {
        seen.push(std::mem::discriminant(&what));
        if what != ConsoleOnly::Ud2 {
            tracing::warn!(instruction = ?what, at = format_args!("{rip:#x}"), "a console CPU instruction this CPU lacks: emulated (logged once)");
        }
    }
    match what {
        ConsoleOnly::Ud2 => return 0,
        ConsoleOnly::Clwb => {
            // nothing to write back on the PC: patched to a NOP, no exception next time
            // SAFETY: `code` holds the `len` bytes of the instruction at `rip`.
            unsafe { patch_nop(rip, &code[..len]) };
        }
        ConsoleOnly::Monitorx => {
            KINDS[K_MONITORX].fetch_add(1, Relaxed);
            let address = ctx.Rax as usize;
            let armed = if pacing().block { armed_value(address, clock_us()) } else { None };
            MONITOR.with(|m| m.set((address, ctx.Rsi, armed)));
        }
        ConsoleOnly::Mwaitx => {
            KINDS[K_MWAITX].fetch_add(1, Relaxed);
            let waited = wait_step(rip);
            let plan = pacing().plan(waited, ctx.Rcx, ctx.Rbx);
            if waited >= pacing().spin_us && ctx.Rcx & 2 != 0 {
                static SAID: AtomicU64 = AtomicU64::new(0);
                if SAID.fetch_add(1, Relaxed) < 4 {
                    tracing::info!(
                        "mwaitx: the title's timer is on (ecx {:#x}): ebx {:#x} ticks, {} us at 4 GHz; {}",
                        ctx.Rcx as u32,
                        ctx.Rbx as u32,
                        (ctx.Rbx & 0xFFFF_FFFF) / TICKS_PER_US,
                        plan.map_or_else(|| "spun".to_owned(), |t| format!("blocks {t} us"))
                    );
                }
            }
            if !plan.is_some_and(block) {
                std::hint::spin_loop()
            }
        }
        ConsoleOnly::Clzero | ConsoleOnly::ClzeroStep { .. } => {
            // The loop `mov rax, ptr; add ptr, 0x40; clzero; cmp ptr, end; jb` (the title's
            // +0x4e8ab4c and three more like it zero 896 to 7,040 bytes each, a line an
            // exception) is taken whole from the line it stands at.
            if let ConsoleOnly::ClzeroStep { ptr_reg, end_reg } = what
                && loop15()
                && let Some(done) = unsafe { clzero_loop15(ctx, rip, ptr_reg, end_reg) }
            {
                if !done {
                    return 0;
                }
                ctx.Rip += len as u64;
                EMULATED.fetch_add(1, Relaxed);
                KINDS[K_LOOP15].fetch_add(1, Relaxed);
                line();
                return CONTINUE_EXECUTION;
            }
            KINDS[K_CLZERO].fetch_add(1, Relaxed);
            // with an address-size prefix (0x67) the line is at EAX, zero-extended
            let a32 = code[..len - 3].contains(&0x67);
            let line = (if a32 { ctx.Rax & 0xFFFF_FFFF } else { ctx.Rax } as usize) & !63;
            if !writable(line, 64) {
                return 0; // the real instruction faults there too
            }
            // SAFETY: the 64-byte line is committed and writable.
            unsafe { std::ptr::write_bytes(line as *mut u8, 0, 64) };
        }
        ConsoleOnly::ClzeroLoop { end_reg } => {
            KINDS[K_LOOP12].fetch_add(1, Relaxed);
            // the whole `clzero; add rax,40h; cmp rax,end; jb` loop at once
            let regs: [u64; 16] = [
                ctx.Rax, ctx.Rcx, ctx.Rdx, ctx.Rbx, ctx.Rsp, ctx.Rbp, ctx.Rsi, ctx.Rdi, ctx.R8, ctx.R9, ctx.R10, ctx.R11, ctx.R12, ctx.R13, ctx.R14, ctx.R15,
            ];
            let end = regs[end_reg as usize & 15] as usize;
            let start = ctx.Rax as usize;
            if end > start && end - start < 2 << 30 {
                // Region by region: a block may lie across several (the title's pages are
                // mapped one view each), and one answer for all of it was a refusal, after
                // which every 64-byte line took an exception of its own.
                let mut at = start;
                while at < end {
                    let Some(to) = writable_to(at, end) else { break };
                    // SAFETY: committed and writable from `at` to `to`.
                    unsafe { std::ptr::write_bytes(at as *mut u8, 0, to - at) };
                    at = to;
                }
                if at == start {
                    return 0;
                }
                ZEROED.fetch_add((at - start) as u64, Relaxed);
                if at < end {
                    // What follows is not writable: the loop goes on there (the same
                    // instruction again), where the real one faults too.
                    ctx.Rax = at as u64;
                    return CONTINUE_EXECUTION;
                }
            }
            ctx.Rax = end as u64;
        }
        ConsoleOnly::Rdpru => {
            let mut v = 0i64;
            // SAFETY: a valid out pointer.
            unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut v) };
            let v = if ctx.Rcx as u32 <= 1 { v as u64 } else { 0 };
            ctx.Rax = v & 0xFFFF_FFFF;
            ctx.Rdx = v >> 32;
        }
        ConsoleOnly::Rdpid(r) => {
            // SAFETY: no preconditions.
            let cpu = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessorNumber() } as u64;
            let regs: [&mut u64; 16] = [
                &mut ctx.Rax, &mut ctx.Rcx, &mut ctx.Rdx, &mut ctx.Rbx, &mut ctx.Rsp, &mut ctx.Rbp, &mut ctx.Rsi, &mut ctx.Rdi, &mut ctx.R8, &mut ctx.R9, &mut ctx.R10, &mut ctx.R11, &mut ctx.R12, &mut ctx.R13,
                &mut ctx.R14, &mut ctx.R15,
            ];
            *regs[r as usize & 15] = cpu;
        }
        // the low quadword changes; the high one is zeroed, as a Zen 5 does
        ConsoleOnly::Extrq { dst, src, len, idx } => {
            // SAFETY: the context's XMM registers (CONTEXT_FLOATING_POINT is captured for exceptions).
            let x = unsafe { &mut ctx.Anonymous.FltSave.XmmRegisters };
            let (len, idx) = match src {
                Some(s) => {
                    let c = x[s as usize & 15].Low;
                    (c as u8, (c >> 8) as u8)
                }
                None => (len, idx),
            };
            let d = &mut x[dst as usize & 15];
            d.Low = decode::extrq(d.Low, len, idx);
            d.High = 0;
        }
        ConsoleOnly::Insertq { dst, src, reg, len, idx } => {
            // SAFETY: as above.
            let x = unsafe { &mut ctx.Anonymous.FltSave.XmmRegisters };
            let s = x[src as usize & 15];
            let (len, idx) = if reg { (s.High as u8, (s.High >> 8) as u8) } else { (len, idx) };
            let d = &mut x[dst as usize & 15];
            d.Low = decode::insertq(d.Low, s.Low, len, idx);
            d.High = 0;
        }
    }
    if !matches!(
        what,
        ConsoleOnly::Monitorx | ConsoleOnly::Mwaitx | ConsoleOnly::Clzero | ConsoleOnly::ClzeroStep { .. } | ConsoleOnly::ClzeroLoop { .. }
    ) {
        KINDS[K_OTHER].fetch_add(1, Relaxed);
    }
    line();
    ctx.Rip += len as u64;
    // How often this runs at all (each one is an exception: a trip into Windows and back).
    let emulated = EMULATED.fetch_add(1, Relaxed) + 1;
    if emulated >= 1 << 16 && emulated.is_power_of_two() {
        tracing::info!(
            emulated,
            asked = ASKED.load(Relaxed),
            zeroed_mb = ZEROED.load(Relaxed) >> 20,
            "console CPU instructions emulated so far, the times Windows was asked about memory for them, and what clzero loops zeroed"
        );
    }
    CONTINUE_EXECUTION
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the title's loop leaves, run line by line as the processor would: the lines it
    /// zeroed and the registers (rax, ptr) at the end, from the state at its first `clzero`.
    fn by_hand(mut rax: usize, mut ptr: usize, end: usize) -> (Vec<usize>, usize, usize) {
        let mut lines = vec![rax & !63];
        while ptr < end {
            rax = ptr;
            ptr += 0x40;
            lines.push(rax & !63);
        }
        (lines, rax, ptr)
    }

    #[test]
    fn the_report_names_the_waits_under_way_longest_first() {
        let w = |thread, address, begin, last, spins| Waiting { thread, rip: 0x5000, address, wanted: 7, begin, last, spins, blocked: if thread == 12 { 990 } else { 0 } };
        let waits = [
            w(0, 0, 0, 0, 0),
            // ended long ago
            w(11, 0x100, 1_000_000, 2_000_000, 5),
            // under way, the later one
            w(12, 0x200, 40_000_000, 49_500_000, 1_000),
            // under way, the longest
            w(13, 0x300, 10_000_000, 50_000_000, 10_000_000),
        ];
        let mut out = String::new();
        describe_waits(&waits, 50_000_000, |a| (a == 0x300).then_some(6), |rip| format!("{rip:#x}"), &mut out);
        assert_eq!(
            out,
            "2 title threads in an emulated mwaitx wait now (3 ever used it); \
             thread 13 at 0x5000 waits 40.0 s for the value at 0x300 (wants 0x7, holds 0x6), 10000000 mwaitx (250000 a second); \
             thread 12 at 0x5000 waits 9.5 s for the value at 0x200 (wants 0x7, holds not readable), 1000 mwaitx (105 a second, 990 of them blocked)"
        );
    }

    #[test]
    fn a_thread_keeps_its_slot_and_notes_its_newest_wait() {
        std::thread::spawn(|| {
            waiting::note(0x10, 0x1000, 3, 100, 100, 1);
            waiting::note(0x10, 0x1000, 3, 100, 900, 2);
            // SAFETY: no preconditions.
            let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
            let slots: Vec<_> = waiting::TABLE.iter().filter(|s| s.thread.load(Relaxed) == me).collect();
            assert_eq!(slots.len(), 1, "one slot per thread");
            let s = slots[0];
            assert_eq!((s.address.load(Relaxed), s.wanted.load(Relaxed)), (0x1000, 3));
            assert_eq!((s.begin.load(Relaxed), s.last.load(Relaxed), s.spins.load(Relaxed)), (100, 900, 2));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn mwaitx_spins_first_then_blocks_within_the_title_s_timer() {
        assert_eq!(Pacing::parse(None, None, None), Pacing { block: false, spin_us: 200, timeout_us: 1_000 }, "the defaults spin");
        assert_eq!(Pacing::parse(None, None, None).plan(10_000_000, 0, 0), None, "unset: spins all through");
        assert_eq!(Pacing::parse(Some("other"), None, None).plan(10_000_000, 0, 0), None, "anything but wait spins");
        let p = Pacing::parse(Some("wait"), None, None);
        assert_eq!(p, Pacing { block: true, spin_us: 200, timeout_us: 1_000 }, "VICTORY_MWAITX=wait");
        assert_eq!(Pacing::parse(Some("spin"), None, None).plan(10_000_000, 0, 0), None, "VICTORY_MWAITX=spin");
        assert_eq!(Pacing::parse(Some("wait"), Some("50"), Some("300")), Pacing { block: true, spin_us: 50, timeout_us: 300 });
        assert_eq!(Pacing::parse(Some("wait"), Some("x"), Some("1")).timeout_us, MIN_BLOCK_US, "nonsense and too small");
        // the spin phase, then blocks of the timeout
        assert_eq!(p.plan(0, 0, 0), None);
        assert_eq!(p.plan(199, 0, 0), None);
        assert_eq!(p.plan(200, 0, 0), Some(1_000));
        assert_eq!(p.plan(5_000_000, 0, 0), Some(1_000));
        // EBX without ECX bit 1 is no timer
        assert_eq!(p.plan(500, 1, 4_000), Some(1_000));
        // the title's timer: 400,000 ticks are 100 us at 4 GHz; a long one is capped
        assert_eq!(p.plan(500, 2, 400_000), Some(100));
        assert_eq!(p.plan(500, 3, 0xFFFF_FFFF), Some(1_000));
        assert_eq!(p.plan(500, 2, 0x1_0000_0000 + 400_000), Some(100), "EBX is 32 bits");
        // too short to block: spun
        assert_eq!(p.plan(500, 2, 4_000 * (MIN_BLOCK_US - 1)), None);
        assert_eq!(p.plan(500, 2, 0), None);
        // a wait goes on across a block
        assert!(p.gap_us() > p.timeout_us && p.gap_us() >= 2_000);
        assert_eq!(Pacing::parse(Some("wait"), None, Some("10000")).gap_us(), 21_000);
        assert_eq!(Pacing::parse(Some("spin"), None, Some("10000")).gap_us(), 2_000, "as before");
        assert_eq!((wait_size(0x1000), wait_size(0x1004), wait_size(0x1002), wait_size(0x1001)), (Some(8), Some(4), None, None));
    }

    /// The emulated `monitorx` / `mwaitx` pair on `value`, as the title runs it: `(rip after,
    /// microseconds the mwaitx took)`.
    fn monitor_and_wait(value: &AtomicU64, ecx: u64, ebx: u64) -> (u64, u64) {
        let (monitorx, mwaitx) = ([0x0Fu8, 0x01, 0xFA, 0x90], [0x0Fu8, 0x01, 0xFB, 0x90]);
        // SAFETY: plain data.
        let mut ctx: CONTEXT = unsafe { std::mem::zeroed() };
        ctx.Rip = monitorx.as_ptr() as u64;
        ctx.Rax = value.as_ptr() as u64;
        ctx.Rsi = 1;
        // SAFETY: the context points at the code above.
        assert_eq!(unsafe { emulate(&mut ctx) }, CONTINUE_EXECUTION);
        assert_eq!(ctx.Rip, monitorx.as_ptr() as u64 + 3);
        ctx.Rip = mwaitx.as_ptr() as u64;
        (ctx.Rcx, ctx.Rbx) = (ecx, ebx);
        let start = clock_us();
        // SAFETY: as above.
        assert_eq!(unsafe { emulate(&mut ctx) }, CONTINUE_EXECUTION);
        (ctx.Rip - mwaitx.as_ptr() as u64, clock_us() - start)
    }

    fn my_slot() -> &'static waiting::Slot {
        // SAFETY: no preconditions.
        let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        waiting::TABLE.iter().find(|s| s.thread.load(Relaxed) == me).expect("a slot")
    }

    #[test]
    fn a_long_mwaitx_wait_blocks_and_its_timeout_ends_each_block() {
        std::thread::spawn(|| {
            let value = AtomicU64::new(0);
            let start = clock_us();
            // the spin phase: no block
            let (step, _) = monitor_and_wait(&value, 0, 0);
            assert_eq!(step, 3, "stepped over");
            assert_eq!(my_slot().blocked.load(Relaxed), 0);
            while clock_us() - start < pacing().spin_us + 50 {
                monitor_and_wait(&value, 0, 0);
            }
            assert!(my_slot().spins.load(Relaxed) >= 2, "one wait");
            // then blocks of the timeout, the value unchanged (on a loaded machine a block may
            // overrun the gap that keeps a wait one wait: it begins anew, spins, blocks again)
            let mut blocks = 0;
            while blocks == 0 && clock_us() - start < 5_000_000 {
                let blocked = my_slot().blocked.load(Relaxed);
                let (step, took) = monitor_and_wait(&value, 0, 0);
                assert_eq!(step, 3);
                if my_slot().blocked.load(Relaxed) == blocked + 1 {
                    blocks += 1;
                    assert!(took >= pacing().timeout_us / 2, "until the timeout: {took} us");
                    assert!(took < 1_000_000, "{took} us");
                }
            }
            assert_eq!(blocks, 1, "it blocked");
            // the title's timer caps the next at 100 us (`plan`; the clock here is too coarse
            // to tell 100 from 1,000 us on a loaded machine)
            let (_, took) = monitor_and_wait(&value, 2, 400_000);
            assert!(took < 1_000_000, "the title's timer: {took} us");
            // the wait, in the stall report's table
            assert_eq!(my_slot().address.load(Relaxed), value.as_ptr() as u64);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_value_changed_after_monitorx_ends_the_block_at_once() {
        std::thread::spawn(|| {
            let value = AtomicU64::new(0);
            let start = clock_us();
            // into the wait phase (a block has found the page readable)
            while my_slot_blocked() == 0 && clock_us() - start < 5_000_000 {
                monitor_and_wait(&value, 0, 0);
            }
            assert!(my_slot_blocked() > 0);
            // monitorx reads 0; the value moves before the mwaitx: no wait at all
            let (monitorx, mwaitx) = ([0x0Fu8, 0x01, 0xFA], [0x0Fu8, 0x01, 0xFB]);
            // SAFETY: plain data.
            let mut ctx: CONTEXT = unsafe { std::mem::zeroed() };
            ctx.Rip = monitorx.as_ptr() as u64;
            ctx.Rax = value.as_ptr() as u64;
            ctx.Rsi = 1;
            // SAFETY: the context points at the code above.
            assert_eq!(unsafe { emulate(&mut ctx) }, CONTINUE_EXECUTION);
            value.store(1, Relaxed);
            ctx.Rip = mwaitx.as_ptr() as u64;
            let (woken, _, _) = victory_core::wake::counts();
            let at = clock_us();
            // SAFETY: as above.
            assert_eq!(unsafe { emulate(&mut ctx) }, CONTINUE_EXECUTION);
            let took = clock_us() - at;
            assert!(victory_core::wake::counts().0 > woken, "ended as changed");
            assert!(took < pacing().timeout_us, "{took} us");
        })
        .join()
        .unwrap();
    }

    fn my_slot_blocked() -> u64 {
        // SAFETY: no preconditions.
        let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        waiting::TABLE.iter().find(|s| s.thread.load(Relaxed) == me).map_or(0, |s| s.blocked.load(Relaxed))
    }

    #[test]
    fn a_waiter_blocked_in_mwaitx_goes_on_once_the_runtime_writes() {
        use std::sync::Arc;
        let value = Arc::new(AtomicU64::new(0));
        let waiter = {
            let value = Arc::clone(&value);
            std::thread::spawn(move || {
                let start = clock_us();
                // as the title: wait while the value is 0, at most 10 s
                let mut blocks = 0u64;
                while value.load(Relaxed) == 0 && clock_us() - start < 10_000_000 {
                    monitor_and_wait(&value, 0, 0);
                    blocks = my_slot_blocked();
                }
                (clock_us() - start, blocks)
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        // the runtime's write and wake, as a fence's
        value.store(5, Relaxed);
        victory_core::wake::written(value.as_ptr() as usize, 8);
        let (took, blocks) = waiter.join().unwrap();
        assert!(blocks > 0, "it blocked");
        assert!(took < 5_000_000, "released: {took} us");
    }

    #[test]
    fn an_instruction_patched_by_another_thread_runs_again_as_patched() {
        for (len, nop) in NOPS.iter().enumerate() {
            assert_eq!(nop.len(), len + 1);
            assert_eq!(decode::decode(nop), None, "{nop:02x?} is no console instruction");
            assert!(is_patched(nop));
        }
        assert!(!is_patched(&[0x66, 0x0F, 0xAE, 0x30]), "clwb [rax]");
        assert!(!is_patched(&[0x0F, 0x01, 0xFB]));
        assert!(!is_patched(&[0x0F, 0x1F]), "a NOP cut short");
        assert_eq!(patch_word(0x1000, 4), Some((0x1000, 0)));
        assert_eq!(patch_word(0x1003, 5), Some((0x1000, 3)));
        assert_eq!(patch_word(0x1005, 4), None, "across two words");
        assert_eq!(patch_word(0x1000, 10), None);
        assert_eq!(with_nop(0x1122_3344_5566_7788, 2, 4), u64::from_le_bytes([0x88, 0x77, 0x0F, 0x1F, 0x40, 0x00, 0x22, 0x11]));

        // clwb [rax] in a word of its own: emulated, patched, stepped over
        #[repr(align(8))]
        struct Code([u8; 24]);
        let mut code = Code([0xCC; 24]);
        code.0[8..12].copy_from_slice(&[0x66, 0x0F, 0xAE, 0x30]);
        let at = code.0.as_mut_ptr() as u64 + 8;
        // SAFETY: plain data.
        let mut ctx: CONTEXT = unsafe { std::mem::zeroed() };
        ctx.Rip = at;
        ctx.Rax = at;
        // SAFETY: the context points at the code above.
        assert_eq!(unsafe { emulate(&mut ctx) }, CONTINUE_EXECUTION);
        assert_eq!(ctx.Rip, at + 4);
        // SAFETY: our own bytes, written by the patch.
        let now = unsafe { std::ptr::read_volatile(&raw const code.0) };
        assert_eq!(&now[8..12], NOPS[3], "one 4-byte NOP");
        assert_eq!((&now[..8], &now[12..]), (&[0xCC; 8][..], &[0xCC; 12][..]), "nothing else");
        // another thread that faulted on it before the patch: it runs the NOP, unstepped
        ctx.Rip = at;
        // SAFETY: as above.
        assert_eq!(unsafe { emulate(&mut ctx) }, CONTINUE_EXECUTION);
        assert_eq!(ctx.Rip, at, "not stepped");
        // a clwb across two words is stepped over, not patched
        let mut code = Code([0xCC; 24]);
        code.0[6..10].copy_from_slice(&[0x66, 0x0F, 0xAE, 0x30]);
        let at = code.0.as_mut_ptr() as u64 + 6;
        ctx.Rip = at;
        // SAFETY: as above.
        assert_eq!(unsafe { emulate(&mut ctx) }, CONTINUE_EXECUTION);
        assert_eq!(ctx.Rip, at + 4);
        // SAFETY: our own bytes.
        let now = unsafe { std::ptr::read_volatile(&raw const code.0) };
        assert_eq!(&now[6..10], &[0x66, 0x0F, 0xAE, 0x30], "left as it was");
    }

    #[test]
    fn the_15_byte_loop_ends_where_the_processor_would_end_it() {
        for (rax, len) in [(0x1000usize, 7040usize), (0x1000, 3264), (0x1000, 3776), (0x1000, 896), (0x4000, 64), (0x4000, 65), (0x4000, 100), (0x4000, 1), (0x4000, 0), (0x8000, 128)] {
            let (ptr, end) = (rax + 64, rax + len);
            let (lines, last_rax, last_ptr) = by_hand(rax, ptr, end);
            let last = loop15_end(rax, ptr, end).expect("the plain case");
            assert_eq!(last, last_ptr, "rax {rax:#x} len {len}");
            assert_eq!(last - 64, last_rax, "rax {rax:#x} len {len}");
            assert_eq!((last - rax) / 64, lines.len(), "rax {rax:#x} len {len}");
            assert_eq!(*lines.last().unwrap(), last - 64);
        }
        // an end before the pointer: the line at rax alone
        assert_eq!(loop15_end(0x1000, 0x1040, 0x800), Some(0x1040));
        // not the plain case: a line not on a boundary, a pointer that is not the next line
        assert_eq!(loop15_end(0x1008, 0x1048, 0x2000), None);
        assert_eq!(loop15_end(0x1000, 0x1080, 0x2000), None);
        assert_eq!(loop15_end(0x1000, 0x1040, 0x1000 + (3 << 30)), None, "over 2 GiB");
    }

    /// The loop as the title has it at +0x4e8ab4c, for the given registers.
    fn code(ptr_reg: u8, end_reg: u8) -> Vec<u8> {
        let (ph, pl, eh, el) = (ptr_reg >> 3, ptr_reg & 7, end_reg >> 3, end_reg & 7);
        vec![
            0x48 | ph << 2, 0x89, 0xC0 | pl << 3, // mov rax, ptr
            0x48 | ph, 0x83, 0xC0 | pl, 0x40, // add ptr, 0x40
            0x0F, 0x01, 0xFC, // clzero
            0x48 | eh << 2 | ph, 0x39, 0xC0 | el << 3 | pl, // cmp ptr, end
            0x72, 0xF1, // jb -15
            0x90,
        ]
    }

    #[test]
    fn the_15_byte_loop_is_zeroed_whole_and_leaves_the_loop_s_registers() {
        for (ptr_reg, end_reg, len) in [(10u8, 11u8, 7040usize), (9, 10, 3264), (8, 9, 3776), (2, 8, 896), (10, 11, 64), (10, 11, 100)] {
            let code = code(ptr_reg, end_reg);
            // a block of 0xAA with room on both sides, its start on a line boundary
            let mut block = vec![0xAAu8; 16384];
            let base = (block.as_mut_ptr() as usize + 4096 + 63) & !63;
            // SAFETY: plain data.
            let mut ctx: CONTEXT = unsafe { std::mem::zeroed() };
            ctx.Rip = code.as_ptr() as u64 + 7;
            ctx.Rax = base as u64;
            *register(&mut ctx, ptr_reg) = base as u64 + 64;
            *register(&mut ctx, end_reg) = (base + len) as u64;
            // SAFETY: the context points at the code above.
            assert_eq!(unsafe { emulate(&mut ctx) }, CONTINUE_EXECUTION);
            let (lines, last_rax, last_ptr) = by_hand(base, base + 64, base + len);
            assert_eq!(ctx.Rip, code.as_ptr() as u64 + 10, "at the loop's own cmp");
            assert_eq!(ctx.Rax as usize, last_rax);
            assert_eq!(*register(&mut ctx, ptr_reg) as usize, last_ptr);
            assert_eq!(*register(&mut ctx, end_reg) as usize, base + len, "the end is left alone");
            let zeroed = lines.len() * 64;
            let at = base - block.as_ptr() as usize;
            assert!(block[at..at + zeroed].iter().all(|&b| b == 0), "{zeroed} bytes zeroed");
            assert!(block[..at].iter().all(|&b| b == 0xAA), "nothing before the block");
            assert!(block[at + zeroed..].iter().all(|&b| b == 0xAA), "nothing after its last line");
        }
    }

    #[test]
    fn a_clzero_that_only_looks_like_the_loop_is_one_line() {
        // the same bytes behind it, another head in front (add ptr, 0x80): one line, one step
        let mut code = code(10, 11);
        code[6] = 0x80;
        let mut block = vec![0xAAu8; 8192];
        let base = (block.as_mut_ptr() as usize + 2048 + 63) & !63;
        // SAFETY: plain data.
        let mut ctx: CONTEXT = unsafe { std::mem::zeroed() };
        ctx.Rip = code.as_ptr() as u64 + 7;
        ctx.Rax = base as u64;
        ctx.R10 = base as u64 + 64;
        ctx.R11 = base as u64 + 640;
        // SAFETY: the context points at the code above.
        assert_eq!(unsafe { emulate(&mut ctx) }, CONTINUE_EXECUTION);
        assert_eq!(ctx.Rip, code.as_ptr() as u64 + 10);
        assert_eq!((ctx.Rax, ctx.R10), (base as u64, base as u64 + 64), "registers untouched");
        let at = base - block.as_ptr() as usize;
        assert!(block[at..at + 64].iter().all(|&b| b == 0));
        assert!(block[at + 64..].iter().all(|&b| b == 0xAA), "one line only");
    }
}
