//! The stall report: when the title has not presented for a while (its
//! frames stopped, the process alive), one detailed line of what the layer
//! still holds and waits for, repeated at most once a minute while the stall
//! lasts.
//!
//! Why: two runs of a batch froze seconds after the title entered its world.
//! Frames stopped, nothing was thrashing, and the title's main thread spun in
//! its `monitorx` / `mwaitx` wait for a value in memory that never came. The
//! periodic lines of the layer are written by presents and submissions, so
//! they stopped too: nothing in the log said which wait was pending, what the
//! queues held, or whether the card had run what it was given.
//!
//! `VICTORY_STALL_REPORT`: seconds without a present before the report
//! (default 15); `0` or `off`: no report and no thread.
//!
//! Everything is read from another thread while the stuck threads may hold
//! their locks for good: locks are only tried, and one that is held is said
//! as "locked" (named), which is evidence in itself. Other crates add their
//! own sections ([`add_source`]): the loader the title threads inside an
//! emulated `mwaitx` wait, the runtime its DirectStorage queues.

use std::fmt::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The default seconds without a present before the report.
const DEFAULT_SECONDS: u64 = 15;
/// The least time between two reports of one stall.
const REPEAT_US: u64 = 60_000_000;

/// When the title last presented and last submitted, and when our line last
/// sent a submission to the card ([`crate::host::now_us`]; 0: never).
static LAST_PRESENT_US: AtomicU64 = AtomicU64::new(0);
static LAST_SUBMIT_US: AtomicU64 = AtomicU64::new(0);
static LAST_SENT_US: AtomicU64 = AtomicU64::new(0);
/// Presents and title submissions since the start.
static PRESENTS: AtomicU64 = AtomicU64::new(0);
static SUBMITS: AtomicU64 = AtomicU64::new(0);

/// A section another crate adds to the report: its name and what writes it.
pub type Source = fn(&mut String);

static SOURCES: Mutex<Vec<(&'static str, Source)>> = Mutex::new(Vec::new());

/// Adds a section to the report (once per name). It is called from the
/// report's thread and must not block: try its locks, say "locked".
pub fn add_source(name: &'static str, source: Source) {
    let mut sources = SOURCES.lock().unwrap_or_else(|e| e.into_inner());
    if !sources.iter().any(|(n, _)| *n == name) {
        sources.push((name, source));
    }
}

/// The seconds of [`DEFAULT_SECONDS`], or the switch's; None: off.
fn threshold_for(asked: Option<&str>) -> Option<u64> {
    match asked.map(str::trim) {
        None | Some("") => Some(DEFAULT_SECONDS),
        Some("off") | Some("0") => None,
        Some(v) => Some(v.parse::<u64>().ok().filter(|&n| n > 0).unwrap_or(DEFAULT_SECONDS)),
    }
}

fn threshold() -> Option<u64> {
    static ON: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *ON.get_or_init(|| threshold_for(std::env::var("VICTORY_STALL_REPORT").ok().as_deref()))
}

/// What the watch does at `now`: nothing, a report, or the line that frames
/// came again. `last_present` is when the title last presented,
/// `reported` when this stall was last reported (0: not yet) and
/// `reported_present` the present it was reported after.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Nothing,
    Report,
    Resumed,
}

fn step(now: u64, last_present: u64, threshold_us: u64, reported: u64, reported_present: u64) -> Step {
    if last_present == 0 {
        return Step::Nothing;
    }
    if reported != 0 && last_present != reported_present {
        return Step::Resumed;
    }
    if now.saturating_sub(last_present) < threshold_us {
        return Step::Nothing;
    }
    if reported == 0 || now.saturating_sub(reported) >= REPEAT_US {
        Step::Report
    } else {
        Step::Nothing
    }
}

/// Presents of the title since the start.
#[cfg(windows)]
pub(crate) fn presents() -> u64 {
    PRESENTS.load(Ordering::Relaxed)
}

/// A present of the title (called once per present): starts the watch on the
/// first one.
#[cfg(windows)]
pub(crate) fn presented() {
    LAST_PRESENT_US.store(crate::host::now_us().max(1), Ordering::Relaxed);
    PRESENTS.fetch_add(1, Ordering::Relaxed);
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let Some(seconds) = threshold() else {
            return;
        };
        match std::thread::Builder::new().name("victory-stall-watch".into()).spawn(move || watch(seconds)) {
            Ok(_) => tracing::info!(seconds, "stall report on: one when no frame was presented for this long (VICTORY_STALL_REPORT; 0: off)"),
            Err(e) => tracing::warn!(error = %e, "no thread for the stall report"),
        }
    });
}

/// A submission of the title (its `ExecuteCommandLists`).
pub(crate) fn submitted() {
    #[cfg(windows)]
    LAST_SUBMIT_US.store(crate::host::now_us().max(1), Ordering::Relaxed);
    SUBMITS.fetch_add(1, Ordering::Relaxed);
}

/// A submission of our line sent to the card.
#[cfg(windows)]
pub(crate) fn sent() {
    LAST_SENT_US.store(crate::host::now_us().max(1), Ordering::Relaxed);
}

#[cfg(windows)]
fn watch(seconds: u64) {
    let threshold_us = seconds * 1_000_000;
    let (mut reported, mut reported_present) = (0u64, 0u64);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let now = crate::host::now_us();
        let last = LAST_PRESENT_US.load(Ordering::Relaxed);
        match step(now, last, threshold_us, reported, reported_present) {
            Step::Nothing => {}
            Step::Resumed => {
                tracing::warn!(
                    "stall over: the title presented again, {:.1} s after the present the stall report was about",
                    last.saturating_sub(reported_present) as f64 / 1e6
                );
                reported = 0;
            }
            Step::Report => {
                let started = std::time::Instant::now();
                let text = report(now);
                tracing::warn!("STALL REPORT ({:.1} ms to write): {text}", started.elapsed().as_secs_f64() * 1e3);
                reported = now.max(1);
                reported_present = last;
            }
        }
    }
}

/// What the thread preparing or settling a submission does now, finer than
/// its phase ([`doing`]). A run froze with the submitting thread "preparing
/// a submission" for minutes and nothing said where inside that it stood.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Doing {
    /// Nothing marked since the last submission ended.
    Nothing = 0,
    /// Waiting for flights in the air to be settled (detail: the `WHY_`
    /// place), on this thread.
    Settle,
    /// Waiting for the thread that lands flights (detail: the `WHY_` place).
    RelayWait,
    /// Descriptors the title copied into placed heaps.
    SyncHeaps,
    /// The sweep of host copies held by nothing.
    Sweep,
    /// Slots that waited for a texture's host copy.
    MakeWaiting,
    /// The one write-watch asking of the submission's refreshes.
    AskOnce,
    /// A copy of title memory a list recorded (address, bytes).
    Copy,
    /// A fill of upload memory from title memory (source address, bytes).
    Fill,
    /// Work a list left for submission time.
    Run,
    /// The snapshot of a range shaders may write (address, bytes).
    GpuWrite,
    /// A texture's host copy refreshed.
    Texture,
    /// The refreshes of host copies the lists read, all at once.
    Flush,
    /// The snapshots left for after the refreshes.
    Snapshots,
    /// Copies in the card's memory: what changed goes up first.
    CardBefore,
    /// Handing the lists to the host queue.
    Execute,
}

const DOING_NAMES: [&str; 16] = [
    "nothing marked",
    "settling flights in the air itself",
    "waiting for the thread that lands flights",
    "syncing descriptor heaps",
    "the sweep",
    "making slots that waited for textures",
    "asking the write watch once",
    "copying title memory",
    "filling upload memory from title memory",
    "running work a list left",
    "snapshotting a range shaders may write",
    "refreshing a texture's host copy",
    "refreshing host copies (the batch)",
    "taking snapshots",
    "sending changes up to copies in the card's memory",
    "handing the lists to the host queue",
];

/// [`Doing`], its detail, address and bytes, since when, and the Windows
/// thread that marked it.
static DOING_NOW: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Marks what this thread does now inside a submission ([`Doing`]): a few
/// relaxed stores, read only by the report.
#[cfg(windows)]
pub(crate) fn doing(what: Doing, detail: u64, address: usize, bytes: usize) {
    // SAFETY: a plain query of the calling thread.
    let thread = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
    DOING_NOW[1].store(detail, Ordering::Relaxed);
    DOING_NOW[2].store(address as u64, Ordering::Relaxed);
    DOING_NOW[3].store(bytes as u64, Ordering::Relaxed);
    DOING_NOW[4].store(crate::host::now_us(), Ordering::Relaxed);
    DOING_NOW[5].store(u64::from(thread), Ordering::Relaxed);
    DOING_NOW[0].store(what as u64, Ordering::Relaxed);
}

/// The report's words for the last [`doing`] mark.
#[cfg(windows)]
fn describe_doing(now: u64, out: &mut String) {
    let what = DOING_NOW[0].load(Ordering::Relaxed) as usize;
    let _ = write!(
        out,
        "{} (detail {}, at {:#x}, {} bytes) for {:.1} ms, marked by thread {}",
        DOING_NAMES.get(what).copied().unwrap_or("?"),
        DOING_NOW[1].load(Ordering::Relaxed),
        DOING_NOW[2].load(Ordering::Relaxed),
        DOING_NOW[3].load(Ordering::Relaxed),
        now.saturating_sub(DOING_NOW[4].load(Ordering::Relaxed)) as f64 / 1e3,
        DOING_NOW[5].load(Ordering::Relaxed),
    );
}

/// One thread of the process as Linux sees it: its id, name, state letter
/// and the kernel function it sleeps in (`wchan`; "0" when running).
#[derive(Debug, Clone, PartialEq)]
struct KernelThread {
    tid: u32,
    comm: String,
    state: char,
    wchan: String,
}

/// The name and state from a `/proc/<pid>/task/<tid>/stat` line.
fn parse_stat(line: &str) -> Option<(String, char)> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let comm = line.get(open + 1..close)?.to_owned();
    let state = line.get(close + 1..)?.trim_start().chars().next()?;
    Some((comm, state))
}

/// Whether a thread's name is one of ours or the host's graphics stack's,
/// said in the report whatever its state.
fn named_ours(comm: &str) -> bool {
    ["victory", "vkd3d", "dstorage", "card accounting"].iter().any(|p| comm.starts_with(p))
}

/// The kernel section's words: threads by state, every thread in an
/// uninterruptible wait (`D`: the kernel, usually disk I/O, writeback or
/// memory reclaim, holds it) with where it waits, then our own threads.
fn describe_threads(threads: &[KernelThread], out: &mut String) {
    let mut states: Vec<(char, usize)> = Vec::new();
    for t in threads {
        match states.iter_mut().find(|s| s.0 == t.state) {
            Some(s) => s.1 += 1,
            None => states.push((t.state, 1)),
        }
    }
    states.sort();
    let _ = write!(out, "{} threads (", threads.len());
    for (i, (state, n)) in states.iter().enumerate() {
        let _ = write!(out, "{}{state} {n}", if i > 0 { ", " } else { "" });
    }
    out.push_str("); in an uninterruptible wait (D): ");
    let d: Vec<String> =
        threads.iter().filter(|t| t.state == 'D').map(|t| format!("{} '{}' in {}", t.tid, t.comm, t.wchan)).collect();
    out.push_str(if d.is_empty() { "none" } else { "" });
    out.push_str(&d.join(", "));
    let ours: Vec<String> =
        threads.iter().filter(|t| named_ours(&t.comm)).map(|t| format!("'{}' {} {}", t.comm, t.state, t.wchan)).collect();
    let _ = write!(out, "; ours: {}", ours.join(", "));
}

/// The process's Linux id, under Wine (`ProcessWineUnixPid`), else None.
#[cfg(windows)]
fn unix_pid() -> Option<u32> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    /// Wine's information class for the process's Linux id.
    const PROCESS_WINE_UNIX_PID: u32 = 1101;
    type Query = unsafe extern "system" fn(isize, u32, *mut core::ffi::c_void, u32, *mut u32) -> i32;
    // SAFETY: a lookup in ntdll, always loaded; the routine has this type,
    // and Wine fills an int for this class (other hosts refuse the class).
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        if ntdll.is_null() {
            return None;
        }
        let query: Query = std::mem::transmute(GetProcAddress(ntdll, c"NtQueryInformationProcess".as_ptr().cast())?);
        let mut pid: i32 = 0;
        let status = query(-1, PROCESS_WINE_UNIX_PID, (&raw mut pid).cast(), 4, std::ptr::null_mut());
        (status == 0 && pid > 0).then_some(pid as u32)
    }
}

/// The report's kernel section: what Linux says of each of the process's
/// threads (under Wine, through the `Z:` drive; nothing elsewhere).
#[cfg(windows)]
fn kernel_section(out: &mut String) {
    let Some(pid) = unix_pid() else {
        out.push_str("not under Wine");
        return;
    };
    let dir = format!("Z:\\proc\\{pid}\\task");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        let _ = write!(out, "{dir} not readable");
        return;
    };
    let mut threads = Vec::new();
    for entry in entries.flatten() {
        let Some(tid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let read = |what: &str| std::fs::read_to_string(format!("{dir}\\{tid}\\{what}")).unwrap_or_default();
        let Some((comm, state)) = parse_stat(&read("stat")) else {
            continue;
        };
        let wchan = read("wchan").trim().to_owned();
        threads.push(KernelThread { tid, comm, state, wchan: if wchan.is_empty() { "?".to_owned() } else { wchan } });
    }
    threads.sort_by_key(|t| t.tid);
    describe_threads(&threads, out);
}

/// Seconds from `at` to `now` (or "never").
fn ago(now: u64, at: u64) -> String {
    if at == 0 { "never".to_owned() } else { format!("{:.1} s ago", now.saturating_sub(at) as f64 / 1e6) }
}

/// The report: the times, then each section, `" || "` between them.
#[cfg(windows)]
fn report(now: u64) -> String {
    let mut out = String::with_capacity(4096);
    let _ = write!(
        out,
        "no present for {:.1} s (presents so far {}); the title's last submission {} ({} so far); our last submission to the card {}",
        now.saturating_sub(LAST_PRESENT_US.load(Ordering::Relaxed)) as f64 / 1e6,
        PRESENTS.load(Ordering::Relaxed),
        ago(now, LAST_SUBMIT_US.load(Ordering::Relaxed)),
        SUBMITS.load(Ordering::Relaxed),
        ago(now, LAST_SENT_US.load(Ordering::Relaxed)),
    );
    let mut section = |name: &str, f: &dyn Fn(&mut String)| {
        let _ = write!(out, " || {name}: ");
        f(&mut out);
    };
    section("queues", &crate::queue::stall_section);
    section("flights", &crate::host::stall_section);
    section("inside the submission", &|out: &mut String| describe_doing(now, out));
    section("copy queues", &crate::queue2::stall_section);
    section("host queues", &crate::hostq::stall_section);
    section("host device", &crate::host::device_section);
    section("GPU addresses of our host copies", &crate::shadow::gpu_map);
    section("root views", &crate::gpuwatch::roots_section);
    section("kernel", &kernel_section);
    section("pipelines", &|out: &mut String| {
        let _ = write!(
            out,
            "{} waiting to be made; the title's input {}",
            crate::pipeline::making(),
            match crate::pipeline::hold_shown_left() {
                Some(left) => format!("held ({left} left)"),
                None => "not held".to_owned(),
            }
        );
    });
    let sources: Vec<(&'static str, Source)> = match SOURCES.try_lock() {
        Ok(s) => s.clone(),
        Err(_) => {
            out.push_str(" || other sections: locked (the list of sections)");
            Vec::new()
        }
    };
    for (name, source) in sources {
        let _ = write!(out, " || {name}: ");
        source(&mut out);
    }
    out
}

/// One thing a queue's line holds, as the report says it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HeldView {
    /// A wait for a fence (its address): wanted, the fence's value now.
    Fence { fence: usize, wanted: u64, now: u64 },
    /// A wait for a memory value: where, the wait as recorded, the value
    /// there now (None: not readable), whether it is met now, and whether a
    /// flight in the air is known to write a value that meets it (None: the
    /// line was locked).
    Value { address: usize, wait: String, now: Option<u64>, met: bool, flight_writes: Option<bool> },
    /// A submission held: its lists.
    Run { lists: usize },
    /// A fence signal due behind what comes before it: fence, value, the
    /// fence's value now.
    Signal { fence: usize, value: u64, now: u64 },
    /// Our thread is sending the head of the line.
    Running,
    /// A present held behind the line: its frame.
    Present { frame: u64 },
}

/// The line `items` of one queue, the head in full and the rest counted:
/// what stands at the head is why the queue does not move.
pub(crate) fn describe_line(items: &[HeldView], out: &mut String) {
    let Some(head) = items.first() else {
        out.push_str("empty");
        return;
    };
    out.push_str("head ");
    describe_item(head, out);
    // The waits and signals behind the head, each (they say who waits for
    // whom), the rest counted.
    let (mut runs, mut lists, mut presents) = (0, 0, 0);
    let mut said = 0;
    for item in &items[1..] {
        match item {
            HeldView::Run { lists: n } => {
                runs += 1;
                lists += n;
            }
            HeldView::Present { .. } => presents += 1,
            HeldView::Running => {}
            other => {
                if said < 6 {
                    out.push_str("; then ");
                    describe_item(other, out);
                }
                said += 1;
            }
        }
    }
    if said > 6 {
        let _ = write!(out, "; and {} more waits and signals", said - 6);
    }
    let _ = write!(out, "; behind it {runs} submissions ({lists} lists), {presents} presents, {} things in all", items.len());
}

fn describe_item(item: &HeldView, out: &mut String) {
    let _ = match item {
        HeldView::Fence { fence, wanted, now } => {
            write!(out, "wait for fence {fence:#x} to reach {wanted} (it is at {now}{})", if now >= wanted { ": MET, not released" } else { "" })
        }
        HeldView::Value { address, wait, now, met, flight_writes } => write!(
            out,
            "wait for the memory value at {address:#x} ({wait}; holds {}; {}; {})",
            now.map_or_else(|| "not readable".to_owned(), |v| format!("{v:#x}")),
            if *met { "MET now, not released" } else { "not met" },
            match flight_writes {
                Some(true) => "a flight in the air writes a value that meets it",
                Some(false) => "no flight in the air is known to write it",
                None => "the flights' line was locked",
            }
        ),
        HeldView::Run { lists } => write!(out, "a submission of {lists} lists"),
        HeldView::Signal { fence, value, now } => write!(out, "signal of fence {fence:#x} to {value} (it is at {now})"),
        HeldView::Running => write!(out, "our thread sending the head"),
        HeldView::Present { frame } => write!(out, "the present of frame {frame}"),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_sets_the_seconds_or_turns_the_report_off() {
        assert_eq!(threshold_for(None), Some(15));
        assert_eq!(threshold_for(Some("")), Some(15));
        assert_eq!(threshold_for(Some(" 40 ")), Some(40));
        assert_eq!(threshold_for(Some("0")), None);
        assert_eq!(threshold_for(Some("off")), None);
        assert_eq!(threshold_for(Some("soon")), Some(15));
    }

    #[test]
    fn a_stall_is_reported_once_a_minute_and_its_end_once() {
        let s = |n: u64| n * 1_000_000;
        // No present yet (loading): nothing, however long.
        assert_eq!(step(s(500), 0, s(15), 0, 0), Step::Nothing);
        // Presents going on.
        assert_eq!(step(s(100), s(99), s(15), 0, 0), Step::Nothing);
        assert_eq!(step(s(114) - 1, s(99), s(15), 0, 0), Step::Nothing);
        // 15 s without one: a report.
        assert_eq!(step(s(114), s(99), s(15), 0, 0), Step::Report);
        // Not again for a minute, then again.
        assert_eq!(step(s(150), s(99), s(15), s(115), s(99)), Step::Nothing);
        assert_eq!(step(s(175), s(99), s(15), s(115), s(99)), Step::Report);
        // Frames again: said once (the caller forgets the report).
        assert_eq!(step(s(180), s(179), s(15), s(175), s(99)), Step::Resumed);
        assert_eq!(step(s(181), s(179), s(15), 0, 0), Step::Nothing);
    }

    #[test]
    fn a_line_says_its_head_in_full_and_counts_the_rest() {
        let mut out = String::new();
        describe_line(&[], &mut out);
        assert_eq!(out, "empty");

        let line = [
            HeldView::Value { address: 0xd0001000, wait: "64-bit >= 0x2a".into(), now: Some(0x29), met: false, flight_writes: Some(false) },
            HeldView::Run { lists: 3 },
            HeldView::Signal { fence: 0x7000, value: 9, now: 8 },
            HeldView::Run { lists: 2 },
            HeldView::Present { frame: 1346 },
        ];
        let mut out = String::new();
        describe_line(&line, &mut out);
        assert_eq!(
            out,
            "head wait for the memory value at 0xd0001000 (64-bit >= 0x2a; holds 0x29; not met; no flight in the air is known to write it); \
             then signal of fence 0x7000 to 9 (it is at 8); behind it 2 submissions (5 lists), 1 presents, 5 things in all"
        );
    }

    #[test]
    fn a_head_that_is_met_but_still_held_is_said_so() {
        let mut out = String::new();
        describe_line(&[HeldView::Fence { fence: 0x10, wanted: 5, now: 6 }, HeldView::Run { lists: 1 }], &mut out);
        assert!(out.starts_with("head wait for fence 0x10 to reach 5 (it is at 6: MET, not released)"), "{out}");
        let mut out = String::new();
        describe_line(&[HeldView::Value { address: 0x20, wait: "w".into(), now: None, met: true, flight_writes: None }], &mut out);
        assert!(out.contains("holds not readable; MET now, not released; the flights' line was locked"), "{out}");
    }

    #[test]
    fn many_waits_behind_the_head_are_cut_short() {
        let mut line = vec![HeldView::Running];
        line.extend((0..9).map(|i| HeldView::Signal { fence: 0x100, value: i, now: 0 }));
        let mut out = String::new();
        describe_line(&line, &mut out);
        assert_eq!(out.matches("; then ").count(), 6);
        assert!(out.contains("; and 3 more waits and signals"), "{out}");
        assert!(out.ends_with("10 things in all"), "{out}");
    }

    #[test]
    fn a_thread_stat_line_gives_its_name_and_state() {
        assert_eq!(parse_stat("4242 (victory-submit) D 1 2 3"), Some(("victory-submit".to_owned(), 'D')));
        // A name may hold spaces and parentheses: the last ')' ends it.
        assert_eq!(parse_stat("7 (card (x) y) S 1"), Some(("card (x) y".to_owned(), 'S')));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn threads_in_the_kernel_and_ours_are_named() {
        let t = |tid, comm: &str, state, wchan: &str| KernelThread { tid, comm: comm.to_owned(), state, wchan: wchan.to_owned() };
        let threads = [
            t(1, "title", 'S', "futex_wait_queue"),
            t(2, "victory-submit", 'D', "folio_wait_bit_common"),
            t(3, "vkd3d_queue", 'S', "futex_wait_queue"),
            t(4, "title", 'R', "0"),
        ];
        let mut out = String::new();
        describe_threads(&threads, &mut out);
        assert!(out.starts_with("4 threads (D 1, R 1, S 2)"), "{out}");
        assert!(out.contains("2 'victory-submit' in folio_wait_bit_common"), "{out}");
        assert!(out.contains("'vkd3d_queue' S futex_wait_queue"), "{out}");
        assert!(!out.contains("'title'"), "{out}");
        let mut none = String::new();
        describe_threads(&threads[..1], &mut none);
        assert!(none.contains("(D): none"), "{none}");
    }

    /// Under Wine the section reads this process's threads from Linux;
    /// elsewhere it says it is not under Wine.
    #[cfg(windows)]
    #[test]
    fn the_kernel_section_reads_this_process() {
        let mut out = String::new();
        kernel_section(&mut out);
        if unix_pid().is_some() {
            assert!(out.contains(" threads ("), "{out}");
            assert!(!out.contains("(D 0"), "{out}");
        } else {
            assert_eq!(out, "not under Wine");
        }
        let mut mark = String::new();
        doing(Doing::Copy, 1, 0x1000, 64);
        describe_doing(crate::host::now_us(), &mut mark);
        assert!(mark.starts_with("copying title memory (detail 1, at 0x1000, 64 bytes)"), "{mark}");
    }

    #[test]
    fn ago_says_never_for_nothing() {
        assert_eq!(ago(5_000_000, 0), "never");
        assert_eq!(ago(5_000_000, 3_500_000), "1.5 s ago");
    }
}
