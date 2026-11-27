//! A frame's timeline: when each stage of one frame happened, as milliseconds from the return
//! of the present before it (`VICTORY_FRAME_TIMELINE=1`; off unless set).
//!
//! Why: the frame-rate line sums each stage over seconds, and the sums do not say which stage
//! the next frame waited for. Taking 28 ms of waits for the card off the submitting thread left
//! the frame as long as before; something else set its length. Here the hand-offs of one frame
//! are stamped where they happen: the title's submit calls, the end of each submission's
//! preparing on our side, the moments the card ran out of work of ours, the ends of landings
//! (copy-backs), the title's own waits for a value in memory (its `mwaitx`, emulated by the
//! loader), and the title's present call and its return. Every five seconds one line gives the
//! medians, and counts of what each frame's present stood for and which stage ended last.
//!
//! Stamps are relaxed atomic minima, maxima and sums, swapped out at each present's return; an
//! event racing that swap may be counted in the frame beside its own. The graphics crate and the
//! loader both call in (neither sees the other); the graphics crate writes the line.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Instant;

/// Whether the timeline is kept (`VICTORY_FRAME_TIMELINE=1`).
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FRAME_TIMELINE").is_some_and(|v| v == "1"))
}

/// Microseconds since the first look, plus one (0 stays "never").
fn now_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64 + 1
}

/// Moments of a frame, each kept as its first and its last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Mark {
    /// The title's submit call returned.
    TitleSubmit,
    /// A submission's preparing ended on our side (it went to the card).
    Prepared,
    /// The card reached the newest signal of ours with nothing newer sent: it ran dry.
    CardDry,
    /// A landing (copy-back of what shaders wrote, bottom-of-pipe writes, fence signals) ended.
    Landed,
    /// A wait of the title for a value in memory (`mwaitx`) began; and ended.
    TitleWaitBegin,
    TitleWaitEnd,
    /// The title called present.
    PresentCall,
    /// A list call of the title (recording into our host lists) returned.
    Recorded,
    /// The pass over the view heap for shaders that pick buffers by index ended (it runs in
    /// a list call, on the title's thread, once a frame).
    HeapPass,
}
const MARKS: usize = 9;

/// Times of a frame, summed (microseconds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Span {
    /// Our preparing of submissions (waits for the card left out).
    Prepare,
    /// Whoever prepares standing for the card.
    CardWait,
    /// Our submit thread with nothing to do (waiting for the title to submit).
    Idle,
    /// Landings.
    Landing,
    /// The card with nothing of ours queued.
    CardIdle,
    /// The title's waits for values in memory.
    TitleWait,
    /// The title's threads inside our list calls (recording), all threads.
    Record,
    /// The pass over the view heap (inside a list call).
    HeapPass,
}
const SPANS: usize = 8;

/// Threads that made list calls: their id and their nanoseconds inside them this frame, and
/// calls this frame. A thread takes a slot at its first call; past the last slot it is counted
/// in the sums only.
const RECORDERS: usize = 32;
static RECORDER_ID: [AtomicU64; RECORDERS] = [const { AtomicU64::new(0) }; RECORDERS];
static RECORDER_NS: [AtomicU64; RECORDERS] = [const { AtomicU64::new(0) }; RECORDERS];
static RECORD_CALLS: AtomicU64 = AtomicU64::new(0);
static NEXT_RECORDER: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// This thread's slot in [`RECORDER_NS`] (`usize::MAX`: none yet; `RECORDERS`: none left).
    static MY_SLOT: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
}

/// A list call of the title took `ns` nanoseconds and returned now.
pub fn recorded(ns: u64) {
    if !on() {
        return;
    }
    let slot = MY_SLOT.with(|s| {
        if s.get() == usize::MAX {
            let n = NEXT_RECORDER.fetch_add(1, Relaxed) as usize;
            if n < RECORDERS {
                static NEXT_ID: AtomicU64 = AtomicU64::new(1);
                RECORDER_ID[n].store(NEXT_ID.fetch_add(1, Relaxed), Relaxed);
            }
            s.set(n.min(RECORDERS));
        }
        s.get()
    });
    if slot < RECORDERS {
        RECORDER_NS[slot].fetch_add(ns, Relaxed);
    }
    RECORD_CALLS.fetch_add(1, Relaxed);
    span(Span::Record, ns / 1_000);
}

/// This frame's list calls: calls, threads that made any, the busiest thread's milliseconds.
fn take_recorders() -> [f64; 3] {
    let mut threads = 0;
    let mut most = 0;
    for ns in &RECORDER_NS {
        let v = ns.swap(0, Relaxed);
        threads += u64::from(v > 0);
        most = most.max(v);
    }
    [
        RECORD_CALLS.swap(0, Relaxed) as f64,
        threads as f64,
        most as f64 / 1e6,
    ]
}

static FIRST: [AtomicU64; MARKS] = [const { AtomicU64::new(u64::MAX) }; MARKS];
static LAST: [AtomicU64; MARKS] = [const { AtomicU64::new(0) }; MARKS];
static SUM: [AtomicU64; SPANS] = [const { AtomicU64::new(0) }; SPANS];
/// Of [`SUM`], what fell between the present call and its return.
static IN_PRESENT: [AtomicU64; SPANS] = [const { AtomicU64::new(0) }; SPANS];
/// The present under way: when it was called (0: none); the last present's return.
static PRESENTING: AtomicU64 = AtomicU64::new(0);
static FRAME_START: AtomicU64 = AtomicU64::new(0);
/// A title's wait counts as met by a landing (or the card running dry) when it ended within
/// this many microseconds after one.
const NEAR_US: u64 = 2_000;

/// The latest landing ends and moments the card ran dry; the title's waits (begin, end, ended
/// right after a landing, right after the card ran dry) not yet given to a frame: the loader
/// learns of a wait's end only when the title's next wait begins, often a frame later; the
/// frames since the last line, and when that line was written.
struct Recent {
    landed: VecDeque<u64>,
    dry: VecDeque<u64>,
    waits: Vec<(u64, u64, bool, bool)>,
    frames: Vec<Frame>,
    line_at: u64,
}
static RECENT: Mutex<Recent> = Mutex::new(Recent {
    landed: VecDeque::new(),
    dry: VecDeque::new(),
    waits: Vec::new(),
    frames: Vec::new(),
    line_at: 0,
});
const LINE_US: u64 = 5_000_000;

fn recent() -> MutexGuard<'static, Recent> {
    RECENT.lock().unwrap_or_else(|e| e.into_inner())
}

fn keep(ring: &mut VecDeque<u64>, at: u64) {
    if ring.len() >= 256 {
        ring.pop_front();
    }
    ring.push_back(at);
}

fn stamp(m: Mark, at: u64) {
    FIRST[m as usize].fetch_min(at, Relaxed);
    LAST[m as usize].fetch_max(at, Relaxed);
}

/// Something happened now.
pub fn mark(m: Mark) {
    if !on() {
        return;
    }
    let now = now_us();
    stamp(m, now);
    match m {
        Mark::CardDry => keep(&mut recent().dry, now),
        Mark::Landed => keep(&mut recent().landed, now),
        _ => {}
    }
}

/// A stretch of `us` microseconds of kind `s` ended now. Preparing and landings also stamp
/// their end.
pub fn span(s: Span, us: u64) {
    if !on() {
        return;
    }
    let now = now_us();
    SUM[s as usize].fetch_add(us, Relaxed);
    let called = PRESENTING.load(Relaxed);
    if called != 0 {
        IN_PRESENT[s as usize].fetch_add(us.min(now.saturating_sub(called)), Relaxed);
    }
    match s {
        Span::Prepare => stamp(Mark::Prepared, now),
        Span::Record => stamp(Mark::Recorded, now),
        Span::HeapPass => stamp(Mark::HeapPass, now),
        Span::Landing => {
            stamp(Mark::Landed, now);
            keep(&mut recent().landed, now);
        }
        _ => {}
    }
}

/// A wait of the title for a value in memory, of `us` microseconds, ended `ago_us` microseconds
/// ago. It is given to the frame it ended in when the next line is made.
pub fn title_wait(us: u64, ago_us: u64) {
    if !on() {
        return;
    }
    let end = now_us().saturating_sub(ago_us).max(1);
    let begin = end.saturating_sub(us).max(1);
    let mut r = recent();
    let near = |ring: &VecDeque<u64>| {
        ring.iter()
            .rev()
            .any(|&at| at <= end && end - at <= NEAR_US)
    };
    let (landed, dry) = (near(&r.landed), near(&r.dry));
    if r.waits.len() < 65_536 {
        r.waits.push((begin, end, landed, dry));
    }
}

/// The title called present.
pub fn present_call() {
    if !on() {
        return;
    }
    let now = now_us();
    stamp(Mark::PresentCall, now);
    PRESENTING.store(now, Relaxed);
}

/// One frame, from the return of the present before it to its own present's return:
/// offsets in milliseconds from its start (None: did not happen in it), sums in milliseconds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    /// Its start and end, absolute microseconds.
    pub start: u64,
    pub end: u64,
    pub ms: f64,
    /// First and last of each [`Mark`].
    pub first: [Option<f64>; MARKS],
    pub last: [Option<f64>; MARKS],
    pub sum: [f64; SPANS],
    pub in_present: [f64; SPANS],
    /// The title's waits that ended in it: all, those met right after a landing, right after
    /// the card ran dry.
    pub waits: [u64; 3],
    /// Its list calls: calls, threads that made any, the busiest thread's milliseconds in them.
    pub recorders: [f64; 3],
}

impl Frame {
    /// The frame from its stamps (absolute microseconds; `first` u64::MAX and `last` 0 unset).
    pub fn of(
        start: u64,
        end: u64,
        first: [u64; MARKS],
        last: [u64; MARKS],
        sum: [u64; SPANS],
        in_present: [u64; SPANS],
    ) -> Frame {
        let mut f = Frame {
            start,
            end,
            ms: end.saturating_sub(start) as f64 / 1e3,
            sum: sum.map(|v| v as f64 / 1e3),
            in_present: in_present.map(|v| v as f64 / 1e3),
            ..Frame::default()
        };
        f.first = first.map(|at| f.offset(at));
        f.last = last.map(|at| f.offset(at));
        f
    }

    fn offset(&self, at: u64) -> Option<f64> {
        (at != 0 && at != u64::MAX && at >= self.start && at <= self.end)
            .then(|| (at - self.start) as f64 / 1e3)
    }

    /// Takes a title's wait (absolute microseconds) if it ended in this frame.
    pub fn take_wait(&mut self, begin: u64, end: u64, landed: bool, dry: bool) -> bool {
        if end <= self.start || end > self.end {
            return false;
        }
        let b = self.offset(begin.max(self.start));
        let e = self.offset(end);
        let (bi, ei) = (Mark::TitleWaitBegin as usize, Mark::TitleWaitEnd as usize);
        self.first[bi] = match (self.first[bi], b) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        };
        self.last[ei] = match (self.last[ei], e) {
            (Some(x), Some(y)) => Some(x.max(y)),
            (x, y) => x.or(y),
        };
        self.sum[Span::TitleWait as usize] += end.saturating_sub(begin) as f64 / 1e3;
        self.waits[0] += 1;
        self.waits[1] += u64::from(landed);
        self.waits[2] += u64::from(dry);
        true
    }

    /// How long the present call stood (its return less its call).
    pub fn present_ms(&self) -> Option<f64> {
        self.last[Mark::PresentCall as usize].map(|c| self.ms - c)
    }

    /// What the present stood for: 0 it did not stand (under 2 ms: the title came later than
    /// our side), 1 our preparing, 2 our waits for the card, 3 neither (the title's own work
    /// in the present, or our thread blocked on something not metered).
    pub fn present_stood_for(&self) -> usize {
        let Some(p) = self.present_ms() else {
            return 0;
        };
        if p < 2.0 {
            return 0;
        }
        let prep = self.in_present[Span::Prepare as usize];
        let wait = self.in_present[Span::CardWait as usize];
        if prep.max(wait) < p / 3.0 {
            3
        } else if prep >= wait {
            1
        } else {
            2
        }
    }

    /// Which of the title's last submit, our last preparing, the card's last running dry, the
    /// last landing and the title's last wait ended last in the frame (index into
    /// [`LAST_NAMES`]), if any did.
    pub fn last_stage(&self) -> Option<usize> {
        [
            Mark::TitleSubmit,
            Mark::Prepared,
            Mark::CardDry,
            Mark::Landed,
            Mark::TitleWaitEnd,
        ]
        .iter()
        .enumerate()
        .filter_map(|(i, &m)| self.last[m as usize].map(|t| (i, t)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
    }
}

pub const LAST_NAMES: [&str; 5] = [
    "title submit",
    "our preparing",
    "card ran dry",
    "landing",
    "title's wait",
];

/// The title's present returns: the frame ends. Every five seconds, the line to log.
pub fn present_return() -> Option<String> {
    if !on() {
        return None;
    }
    let now = now_us();
    let start = FRAME_START.swap(now, Relaxed);
    PRESENTING.store(0, Relaxed);
    let first = std::array::from_fn(|i| FIRST[i].swap(u64::MAX, Relaxed));
    let last = std::array::from_fn(|i| LAST[i].swap(0, Relaxed));
    let sum = std::array::from_fn(|i| SUM[i].swap(0, Relaxed));
    let in_present = std::array::from_fn(|i| IN_PRESENT[i].swap(0, Relaxed));
    let recorders = take_recorders();
    if start == 0 {
        return None;
    }
    let mut r = recent();
    r.frames.push(Frame {
        recorders,
        ..Frame::of(start, now, first, last, sum, in_present)
    });
    if r.line_at == 0 {
        r.line_at = now;
    }
    if now - r.line_at < LINE_US {
        return None;
    }
    r.line_at = now;
    let mut frames = std::mem::take(&mut r.frames);
    let waits = std::mem::take(&mut r.waits);
    drop(r);
    let later = give_waits(&mut frames, waits);
    recent().waits.extend(later);
    Some(line(&frames))
}

/// Gives each wait to the frame it ended in; returns those that ended after the last frame.
fn give_waits(
    frames: &mut [Frame],
    waits: Vec<(u64, u64, bool, bool)>,
) -> Vec<(u64, u64, bool, bool)> {
    let end = frames.last().map_or(0, |f| f.end);
    let mut later = Vec::new();
    for w in waits {
        if w.1 > end {
            later.push(w);
            continue;
        }
        let i = frames.partition_point(|f| f.end < w.1);
        if let Some(f) = frames.get_mut(i) {
            f.take_wait(w.0, w.1, w.2, w.3);
        }
    }
    later
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

fn show(v: Option<f64>) -> String {
    v.map_or_else(|| "-".to_string(), |v| format!("{v:.1}"))
}

/// The line for `frames`: medians over the frames in which each stage happened.
pub fn line(frames: &[Frame]) -> String {
    let n = frames.len();
    let med =
        |f: &dyn Fn(&Frame) -> Option<f64>| show(median(frames.iter().filter_map(f).collect()));
    let first = |m: Mark| med(&move |f: &Frame| f.first[m as usize]);
    let last = |m: Mark| med(&move |f: &Frame| f.last[m as usize]);
    let sum = |s: Span| med(&move |f: &Frame| Some(f.sum[s as usize]));
    let inp = |s: Span| med(&move |f: &Frame| Some(f.in_present[s as usize]));
    let mut stood = [0usize; 4];
    let mut lasts = [0usize; LAST_NAMES.len()];
    let mut waits = [0u64; 3];
    for f in frames {
        stood[f.present_stood_for()] += 1;
        if let Some(i) = f.last_stage() {
            lasts[i] += 1;
        }
        for (w, x) in waits.iter_mut().zip(f.waits) {
            *w += x;
        }
    }
    let lasts: Vec<String> = LAST_NAMES
        .iter()
        .zip(lasts)
        .map(|(name, k)| format!("{name} {k}"))
        .collect();
    format!(
        "frame timeline (VICTORY_FRAME_TIMELINE), medians of {n} frames in ms from the last present's return: frame {}; \
         title submits first {} last {}; present called {} and stood {} (of it our preparing {}, our waits for the card {}, \
         our submit thread idle {}); our preparing ends first {} last {}; card ran dry first {} last {}; landings end first {} \
         last {}; title's waits begin first {} end last {} | sums per frame: preparing {}, waits for the card {}, submit thread \
         idle {}, landing {}, card idle {}, title's waits {} | the present stood for: nothing (the title came later) {}, our \
         preparing {}, our waits for the card {}, neither {} | ended last in the frame: {} | title's waits {}, met within 2 ms \
         after a landing {}, after the card ran dry {} | the title's list calls (recording into ours): first returned {} last {}, \
         {} ms in all ({} in the present call) in {} calls on {} threads, the busiest thread {} ms in them; the view-heap pass \
         {} ms, ended {}",
        med(&|f: &Frame| Some(f.ms)),
        first(Mark::TitleSubmit),
        last(Mark::TitleSubmit),
        last(Mark::PresentCall),
        med(&|f: &Frame| f.present_ms()),
        inp(Span::Prepare),
        inp(Span::CardWait),
        inp(Span::Idle),
        first(Mark::Prepared),
        last(Mark::Prepared),
        first(Mark::CardDry),
        last(Mark::CardDry),
        first(Mark::Landed),
        last(Mark::Landed),
        first(Mark::TitleWaitBegin),
        last(Mark::TitleWaitEnd),
        sum(Span::Prepare),
        sum(Span::CardWait),
        sum(Span::Idle),
        sum(Span::Landing),
        sum(Span::CardIdle),
        sum(Span::TitleWait),
        stood[0],
        stood[1],
        stood[2],
        stood[3],
        lasts.join(", "),
        waits[0],
        waits[1],
        waits[2],
        first(Mark::Recorded),
        last(Mark::Recorded),
        sum(Span::Record),
        inp(Span::Record),
        med(&|f: &Frame| Some(f.recorders[0])),
        med(&|f: &Frame| Some(f.recorders[1])),
        med(&|f: &Frame| Some(f.recorders[2])),
        sum(Span::HeapPass),
        last(Mark::HeapPass),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(marks: &[(Mark, u64, u64)], present_in: [u64; SPANS]) -> Frame {
        let (mut first, mut last) = ([u64::MAX; MARKS], [0; MARKS]);
        for &(m, a, b) in marks {
            first[m as usize] = a;
            last[m as usize] = b;
        }
        Frame::of(1_000, 101_000, first, last, [0; SPANS], present_in)
    }

    #[test]
    fn offsets_are_from_the_frame_start_and_outside_ones_are_left_out() {
        let f = frame(
            &[
                (Mark::TitleSubmit, 11_000, 41_000),
                (Mark::CardDry, 500, 200_000),
            ],
            [0; SPANS],
        );
        assert_eq!(f.ms, 100.0);
        assert_eq!(f.first[Mark::TitleSubmit as usize], Some(10.0));
        assert_eq!(f.last[Mark::TitleSubmit as usize], Some(40.0));
        assert_eq!(f.first[Mark::CardDry as usize], None);
        assert_eq!(f.last[Mark::CardDry as usize], None);
        assert_eq!(f.first[Mark::Landed as usize], None);
    }

    #[test]
    fn what_the_present_stood_for() {
        // Called at 99 ms, returned at 100: it did not stand.
        let f = frame(&[(Mark::PresentCall, 100_000, 100_000)], [0; SPANS]);
        assert_eq!(f.present_ms(), Some(1.0));
        assert_eq!(f.present_stood_for(), 0);
        // Called at 50: stood 50 ms, 40 of them our preparing.
        let mut p = [0; SPANS];
        p[Span::Prepare as usize] = 40_000;
        p[Span::CardWait as usize] = 5_000;
        assert_eq!(
            frame(&[(Mark::PresentCall, 51_000, 51_000)], p).present_stood_for(),
            1
        );
        p[Span::CardWait as usize] = 45_000;
        assert_eq!(
            frame(&[(Mark::PresentCall, 51_000, 51_000)], p).present_stood_for(),
            2
        );
        assert_eq!(
            frame(&[(Mark::PresentCall, 51_000, 51_000)], [0; SPANS]).present_stood_for(),
            3
        );
    }

    #[test]
    fn the_last_stage_is_the_latest_end() {
        let f = frame(
            &[
                (Mark::TitleSubmit, 2_000, 30_000),
                (Mark::Prepared, 5_000, 90_000),
                (Mark::Landed, 3_000, 60_000),
            ],
            [0; SPANS],
        );
        assert_eq!(f.last_stage(), Some(1));
        assert_eq!(frame(&[], [0; SPANS]).last_stage(), None);
    }

    #[test]
    fn waits_go_to_the_frame_they_ended_in() {
        let mut frames = vec![
            Frame::of(
                0,
                100_000,
                [u64::MAX; MARKS],
                [0; MARKS],
                [0; SPANS],
                [0; SPANS],
            ),
            Frame::of(
                100_000,
                200_000,
                [u64::MAX; MARKS],
                [0; MARKS],
                [0; SPANS],
                [0; SPANS],
            ),
        ];
        let later = give_waits(
            &mut frames,
            vec![
                (90_000, 110_000, true, false),
                (120_000, 130_000, false, true),
                (150_000, 250_000, false, false),
            ],
        );
        assert_eq!(later, vec![(150_000, 250_000, false, false)]);
        assert_eq!(frames[0].waits, [0, 0, 0]);
        let f = &frames[1];
        assert_eq!(f.waits, [2, 1, 1]);
        assert_eq!(f.first[Mark::TitleWaitBegin as usize], Some(0.0));
        assert_eq!(f.last[Mark::TitleWaitEnd as usize], Some(30.0));
        assert_eq!(f.sum[Span::TitleWait as usize], 30.0);
    }

    #[test]
    fn the_line_has_medians_and_counts() {
        let frames: Vec<Frame> = [10_000u64, 30_000, 20_000]
            .iter()
            .map(|&t| {
                frame(
                    &[
                        (Mark::TitleSubmit, t, t),
                        (Mark::PresentCall, 100_000, 100_000),
                    ],
                    [0; SPANS],
                )
            })
            .collect();
        let line = line(&frames);
        assert!(line.contains("medians of 3 frames"), "{line}");
        assert!(
            line.contains("title submits first 19.0 last 19.0"),
            "{line}"
        );
        assert!(line.contains("nothing (the title came later) 3"), "{line}");
        assert!(line.contains("title submit 3"), "{line}");
        assert!(line.contains("landings end first - last -"), "{line}");
        assert!(
            line.contains("list calls (recording into ours): first returned - last -"),
            "{line}"
        );
    }
}

#[cfg(test)]
mod recorder_tests {
    use super::*;

    #[test]
    fn recorders_are_taken_per_frame() {
        // Without the switch nothing is kept.
        recorded(1_000_000);
        assert_eq!(RECORD_CALLS.load(Relaxed), u64::from(on()));
        // The table itself: two threads' nanoseconds, then taken and reset.
        RECORDER_NS[RECORDERS - 1].store(3_000_000, Relaxed);
        RECORDER_NS[RECORDERS - 2].store(1_000_000, Relaxed);
        RECORD_CALLS.store(7, Relaxed);
        let [calls, threads, most] = take_recorders();
        assert_eq!((calls, threads, most), (7.0, 2.0, 3.0));
        assert_eq!(take_recorders(), [0.0, 0.0, 0.0]);
    }
}
