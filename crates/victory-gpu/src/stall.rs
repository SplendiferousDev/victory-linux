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
    fn ago_says_never_for_nothing() {
        assert_eq!(ago(5_000_000, 0), "never");
        assert_eq!(ago(5_000_000, 3_500_000), "1.5 s ago");
    }
}
