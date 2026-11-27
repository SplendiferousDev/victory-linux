//! The title's recording threads inside our list calls, per thread and per kind of call
//! (with `VICTORY_FRAME_TIMELINE=1`, as part of [`crate::timeline`]).
//!
//! With the lock waits gone the frame stayed at 110 ms and the recording threads still paced
//! it, each about half busy. This says whether one thread is the long pole (its share of our
//! calls, and when in the frame its first and last call returned) and which calls cost: each
//! list call is known by the place in the graphics crate that took it (the list method), so
//! the line names `file:line` with its milliseconds and calls per frame, over all threads and
//! for the thread that spent the most in them.

use std::panic::Location;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

/// Threads are known by the slot [`crate::timeline`] gave them (at most this many).
pub const THREADS: usize = 32;

static FIRST: [AtomicU64; THREADS] = [const { AtomicU64::new(u64::MAX) }; THREADS];
static LAST: [AtomicU64; THREADS] = [const { AtomicU64::new(0) }; THREADS];

const SITES: usize = 1024;

/// A (place, thread) pair: the place's address, the thread's slot plus one, calls and
/// nanoseconds since the last line.
struct Site {
    at: AtomicUsize,
    thread: AtomicUsize,
    calls: AtomicU64,
    ns: AtomicU64,
}

static TABLE: [Site; SITES] = [const {
    Site {
        at: AtomicUsize::new(0),
        thread: AtomicUsize::new(0),
        calls: AtomicU64::new(0),
        ns: AtomicU64::new(0),
    }
}; SITES];

fn site(at: &'static Location<'static>, thread: usize) -> Option<&'static Site> {
    let key = std::ptr::from_ref(at) as usize;
    let start = ((key >> 3) ^ thread.wrapping_mul(0x51)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 54;
    for i in 0..SITES {
        let s = &TABLE[(start + i) % SITES];
        match s.at.load(Relaxed) {
            0 => {
                if s.at.compare_exchange(0, key, Relaxed, Relaxed).is_ok() {
                    s.thread.store(thread + 1, Relaxed);
                    return Some(s);
                }
                // Taken meanwhile: look at it again.
                if s.at.load(Relaxed) == key && wait_thread(s) == thread + 1 {
                    return Some(s);
                }
            }
            k if k == key && wait_thread(s) == thread + 1 => return Some(s),
            _ => {}
        }
    }
    None
}

/// A slot's thread, once its taker has written it.
fn wait_thread(s: &Site) -> usize {
    loop {
        let t = s.thread.load(Relaxed);
        if t != 0 {
            return t;
        }
        std::hint::spin_loop();
    }
}

/// A list call of thread `thread` at place `at` took `ns` nanoseconds and returned at `now`
/// (microseconds of the timeline's clock).
pub fn call(thread: usize, at: &'static Location<'static>, ns: u64, now: u64) {
    if thread < THREADS {
        FIRST[thread].fetch_min(now, Relaxed);
        LAST[thread].fetch_max(now, Relaxed);
    }
    if let Some(s) = site(at, thread.min(THREADS)) {
        s.calls.fetch_add(1, Relaxed);
        s.ns.fetch_add(ns, Relaxed);
    }
}

/// Per thread for one frame: milliseconds in our calls, first and last call's return in ms
/// from the frame's start (`None`: no call in the frame).
pub type ThreadFrame = (f64, Option<f64>, Option<f64>);

/// The frame from `start` to `end` (microseconds) ended: each thread's figures, given its
/// nanoseconds in our calls this frame.
pub fn frame(start: u64, end: u64, ns: &[u64; THREADS]) -> [ThreadFrame; THREADS] {
    let off = |at: u64| {
        (at != 0 && at != u64::MAX && at >= start && at <= end).then(|| (at - start) as f64 / 1e3)
    };
    std::array::from_fn(|i| {
        let (first, last) = (FIRST[i].swap(u64::MAX, Relaxed), LAST[i].swap(0, Relaxed));
        (ns[i] as f64 / 1e6, off(first), off(last))
    })
}

/// The frames' per-thread figures since the last line.
static WINDOW: Mutex<Vec<[ThreadFrame; THREADS]>> = Mutex::new(Vec::new());

/// Keeps one frame's per-thread figures for the next line.
pub fn keep(frame: [ThreadFrame; THREADS]) {
    WINDOW.lock().unwrap_or_else(|e| e.into_inner()).push(frame);
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

/// The per-thread part of the line: for each thread with calls, its median ms in our calls,
/// its first and last call; in thread order. Returns the text and the busiest thread.
pub fn threads_text(frames: &[[ThreadFrame; THREADS]]) -> (String, Option<usize>) {
    let mut parts = Vec::new();
    let mut busiest = None;
    let mut most = 0.0;
    for t in 0..THREADS {
        let ms: Vec<f64> = frames.iter().map(|f| f[t].0).filter(|&v| v > 0.0).collect();
        if ms.is_empty() {
            continue;
        }
        let total: f64 = ms.iter().sum::<f64>() / frames.len().max(1) as f64;
        if total > most {
            most = total;
            busiest = Some(t);
        }
        parts.push(format!(
            "thread {t}: {} ms in our calls (in {} of {} frames), first {} last {}",
            show(median(ms.clone())),
            ms.len(),
            frames.len(),
            show(median(frames.iter().filter_map(|f| f[t].1).collect())),
            show(median(frames.iter().filter_map(|f| f[t].2).collect())),
        ));
    }
    (parts.join("; "), busiest)
}

/// Places since the last line: (file:line, thread, calls, ns), counts reset.
fn take_sites() -> Vec<(String, usize, u64, u64)> {
    let mut v = Vec::new();
    for s in &TABLE {
        let key = s.at.load(Relaxed);
        if key == 0 {
            continue;
        }
        let (calls, ns) = (s.calls.swap(0, Relaxed), s.ns.swap(0, Relaxed));
        if calls == 0 {
            continue;
        }
        // SAFETY: `key` is a `&'static Location`'s address ([`site`]).
        let at = unsafe { &*(key as *const Location<'static>) };
        let file = at.file().rsplit(['/', '\\']).next().unwrap_or(at.file());
        v.push((
            format!("{file}:{}", at.line()),
            wait_thread(s) - 1,
            calls,
            ns,
        ));
    }
    v
}

/// The places' part: the ten costliest over all threads and the six of `busiest`, per frame.
pub fn sites_text(
    sites: Vec<(String, usize, u64, u64)>,
    frames: f64,
    busiest: Option<usize>,
) -> String {
    let frames = frames.max(1.0);
    let mut all: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
    for (at, _, calls, ns) in &sites {
        let e = all.entry(at.clone()).or_default();
        e.0 += calls;
        e.1 += ns;
    }
    let show = |mut v: Vec<(String, u64, u64)>, n: usize| {
        v.sort_by(|a, b| b.2.cmp(&a.2));
        v.iter()
            .take(n)
            .map(|(at, calls, ns)| {
                format!(
                    "{at} {:.1} ms {:.0} calls",
                    *ns as f64 / 1e6 / frames,
                    *calls as f64 / frames
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let all_ms: f64 = all.values().map(|v| v.1 as f64).sum::<f64>() / 1e6 / frames;
    let top = show(all.into_iter().map(|(k, v)| (k, v.0, v.1)).collect(), 10);
    let mine = busiest.map_or_else(String::new, |b| {
        show(
            sites
                .iter()
                .filter(|s| s.1 == b)
                .map(|s| (s.0.clone(), s.2, s.3))
                .collect(),
            6,
        )
    });
    format!(
        "our list calls by place, per frame ({all_ms:.1} ms in all): {top} | the busiest thread ({}): {mine}",
        busiest.map_or_else(|| "-".to_string(), |b| format!("thread {b}"))
    )
}

/// The whole line since the last one (resets what it reads).
pub fn line() -> String {
    let frames = std::mem::take(&mut *WINDOW.lock().unwrap_or_else(|e| e.into_inner()));
    let (threads, busiest) = threads_text(&frames);
    format!(
        "recording threads (VICTORY_FRAME_TIMELINE), medians over {} frames: {threads} | {}",
        frames.len(),
        sites_text(take_sites(), frames.len() as f64, busiest)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_busiest_thread_is_found_and_its_places_shown() {
        let none: ThreadFrame = (0.0, None, None);
        let mut f = [none; THREADS];
        f[0] = (10.0, Some(1.0), Some(90.0));
        f[3] = (70.0, Some(5.0), Some(100.0));
        let (text, busiest) = threads_text(&[f, f]);
        assert_eq!(busiest, Some(3));
        assert!(
            text.contains(
                "thread 3: 70.0 ms in our calls (in 2 of 2 frames), first 5.0 last 100.0"
            ),
            "{text}"
        );
        let sites = vec![
            ("list.rs:10".to_string(), 3, 100, 60_000_000),
            ("list.rs:20".to_string(), 0, 400, 20_000_000),
            ("list.rs:20".to_string(), 3, 100, 10_000_000),
        ];
        let text = sites_text(sites, 2.0, busiest);
        assert!(
            text.contains(
                "(45.0 ms in all): list.rs:10 30.0 ms 50 calls, list.rs:20 15.0 ms 250 calls"
            ),
            "{text}"
        );
        assert!(text.contains("the busiest thread (thread 3): list.rs:10 30.0 ms 50 calls, list.rs:20 5.0 ms 50 calls"), "{text}");
    }

    #[test]
    fn places_are_kept_per_thread() {
        #[track_caller]
        fn here() -> &'static Location<'static> {
            Location::caller()
        }
        let at = here();
        call(THREADS - 1, at, 1_000, 5);
        call(THREADS - 1, at, 2_000, 6);
        call(THREADS - 2, at, 4_000, 7);
        let a = site(at, THREADS - 1).expect("a slot");
        let b = site(at, THREADS - 2).expect("a slot");
        assert!(!std::ptr::eq(a, b));
        assert_eq!((a.calls.load(Relaxed), a.ns.load(Relaxed)), (2, 3_000));
        let f = frame(0, 10, &[0; THREADS]);
        assert_eq!(f[THREADS - 1].1, Some(0.005));
        assert_eq!(f[THREADS - 1].2, Some(0.006));
    }
}
