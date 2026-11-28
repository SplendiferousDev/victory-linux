//! List calls that are only written down by the title's thread and done by
//! threads of ours (`VICTORY_THIN=1`, off by default, read once).
//!
//! Why: the title's recording threads spend most of their recording time
//! inside our list methods (a timeline had 240 thread-milliseconds a frame
//! there, 85% of the recording window), while half the machine stands idle.
//! With the switch a draw, a dispatch, an indirect execution and the state
//! they need are appended to the list's [`Stream`] with copies of what their
//! pointers name, and threads of ours make the calls in order, beginning
//! while the title still records. One stream is run by one thread at a time,
//! so a list's calls keep their order; lists are independent of each other
//! until they are submitted, as they are for the title's own threads.
//!
//! Every other list call, the list's submission, its reset and its release
//! wait until the stream is empty and then go on as before
//! ([`Stream::drain`]): nothing outside sees a list half translated.
//!
//! The design is Maren Halvorsen's (VIctory-Fast, `list/thin.rs`, commits
//! dd83629 and b1c7f60; GPL-3.0-or-later, as this), written anew for this
//! tree: there every list call is written down and a line of submissions
//! waits for the translation; here, as a first stage, the title's
//! submission waits.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Calls a stream gathers before a thread of ours is woken for it (a wake
/// costs the title's thread more than the calls it writes down).
const WAKE_AT: usize = 24;

/// Whether list calls are written down (`VICTORY_THIN=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_THIN").is_some_and(|v| v == "1");
        if on {
            tracing::info!(
                threads = threads(),
                check = check(),
                "draws, dispatches and their state are written down by the title's threads and translated by threads of ours (VICTORY_THIN=1)"
            );
        }
        on
    })
}

/// Whether each draw notes what its descriptor tables begin with, to tell
/// whether the title changed them before the draw was translated
/// (`VICTORY_THIN_CHECK=1`).
pub(crate) fn check() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_THIN_CHECK").is_some_and(|v| v == "1"))
}

fn threads() -> usize {
    std::env::var("VICTORY_THIN_THREADS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(6usize).clamp(1, 16)
}

fn held<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

thread_local! {
    /// This thread is one of ours that translates.
    static TRANSLATES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// This thread lets go of an object only calls name ([`unnamed`]).
    static UNNAMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f`, which lets go of objects that no descriptor or view names
/// (pipelines, command signatures, descriptor heaps): every list that names
/// one in a call holds it itself, so freeing one waits for nothing
/// ([`before_free`]).
pub(crate) fn unnamed<R>(f: impl FnOnce() -> R) -> R {
    let was = UNNAMED.replace(true);
    let r = f();
    UNNAMED.set(was);
    r
}

/// Whether this thread is one that translates streams.
pub(crate) fn translating() -> bool {
    TRANSLATES.get()
}

/// Why the title's thread waited for a stream.
#[derive(Clone, Copy)]
pub(crate) enum Why {
    /// A list call that is not written down.
    Call = 0,
    /// The list's submission, reset or release.
    End = 1,
    /// An object is freed.
    Free = 2,
}

/// Since the last line: calls translated, words of arguments, nanoseconds
/// translating (all threads), pieces, the largest piece; waits and their
/// nanoseconds by [`Why`]; draws checked and tables found changed
/// ([`check`]).
static STATS: [AtomicU64; 13] = [const { AtomicU64::new(0) }; 13];
const CALLS: usize = 0;
const WORDS: usize = 1;
const BUSY: usize = 2;
const PIECES: usize = 3;
const LARGEST: usize = 4;
const WAITS: usize = 5;
const CHECKED: usize = 11;
const CHANGED: usize = 12;

/// A draw's tables were looked at again when it was translated.
pub(crate) fn checked(changed: bool) {
    STATS[CHECKED].fetch_add(1, Relaxed);
    if changed {
        STATS[CHANGED].fetch_add(1, Relaxed);
    }
}

fn waited(why: Why, time: Duration) {
    STATS[WAITS + 2 * why as usize].fetch_add(1, Relaxed);
    STATS[WAITS + 2 * why as usize + 1].fetch_add(time.as_nanos() as u64, Relaxed);
}

/// The line of the log, every ten seconds.
pub(crate) fn line(frames: f64) {
    if !on() {
        return;
    }
    let s: Vec<u64> = STATS.iter().map(|s| s.swap(0, Relaxed)).collect();
    let per = |n: u64| n as f64 / frames.max(1.0);
    let ms = |n: u64| n as f64 / 1e6 / frames.max(1.0);
    tracing::info!(
        "list calls written down and translated by threads of ours, per frame (VICTORY_THIN): {:.0} calls, {:.1} kB of \
         arguments, {:.1} ms translating (all threads), {:.0} pieces, the largest of {} calls; the title waited {:.1} \
         times {:.2} ms for a call that is not written down, {:.1} times {:.2} ms at a submission or reset, {:.1} \
         times {:.2} ms before an object was freed",
        per(s[CALLS]),
        per(s[WORDS]) * 8.0 / 1000.0,
        ms(s[BUSY]),
        per(s[PIECES]),
        s[LARGEST],
        per(s[WAITS]),
        ms(s[WAITS + 1]),
        per(s[WAITS + 2]),
        ms(s[WAITS + 3]),
        per(s[WAITS + 4]),
        ms(s[WAITS + 5]),
    );
    if check() {
        tracing::info!(
            "  draws whose descriptor tables were looked at again when translated (VICTORY_THIN_CHECK), per frame: \
             {:.0}, of them with a table that began otherwise than at the call {:.2} ({} since the last line)",
            per(s[CHECKED]),
            per(s[CHANGED]),
            s[CHANGED],
        );
    }
}

/// Makes the calls of a piece of a stream on its target, in order.
pub(crate) type Run<C> = fn(target: usize, calls: &[C], words: &[u64]);

/// The calls written down for one list and not made yet.
pub(crate) struct Stream<C: Send + 'static> {
    target: usize,
    run: Run<C>,
    /// What a panic of `run` does (the run ends, as a panic in a list method
    /// ends it).
    failed: fn(&str),
    inner: Mutex<Inner<C>>,
    moved: Condvar,
    /// `run` panicked: nothing more is written down or made.
    broken: AtomicBool,
}

struct Inner<C> {
    calls: Vec<C>,
    /// What the calls' pointers named ([`put_items`]).
    words: Vec<u64>,
    /// A thread of ours has it, or will.
    scheduled: bool,
    /// Memory to use again.
    spare: Option<(Vec<C>, Vec<u64>)>,
}

/// What the threads of ours take.
trait Job: Send + Sync {
    fn work(&self);
    /// Hands over what is written down and waits until it is done, for at
    /// most `most`; whether there was anything to wait for.
    fn settle(self: Arc<Self>, most: Duration) -> bool;
}

static READY: Mutex<VecDeque<Arc<dyn Job>>> = Mutex::new(VecDeque::new());
static WORK: Condvar = Condvar::new();
/// Every stream there is, for [`before_free`].
static STREAMS: Mutex<Vec<std::sync::Weak<dyn Job>>> = Mutex::new(Vec::new());
/// Streams that hold calls or are being run.
static BUSY_STREAMS: AtomicU64 = AtomicU64::new(0);

fn schedule(job: Arc<dyn Job>) {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        for i in 0..threads() {
            let worker = || {
                TRANSLATES.set(true);
                loop {
                    let job = {
                        let mut ready = held(&READY);
                        loop {
                            match ready.pop_front() {
                                Some(job) => break job,
                                None => ready = WORK.wait(ready).unwrap_or_else(|e| e.into_inner()),
                            }
                        }
                    };
                    job.work();
                }
            };
            if let Err(e) = std::thread::Builder::new().name(format!("victory-thin-{i}")).spawn(worker) {
                tracing::warn!(error = %e, "a thread less to translate list calls");
            }
        }
    });
    held(&READY).push_back(job);
    WORK.notify_one();
}

impl<C: Send + 'static> Stream<C> {
    pub(crate) fn new(target: usize, run: Run<C>, failed: fn(&str)) -> Arc<Self> {
        let stream = Arc::new(Stream {
            target,
            run,
            failed,
            inner: Mutex::new(Inner { calls: Vec::new(), words: Vec::new(), scheduled: false, spare: None }),
            moved: Condvar::new(),
            broken: AtomicBool::new(false),
        });
        let job: Arc<dyn Job> = stream.clone();
        let mut streams = held(&STREAMS);
        streams.retain(|s| s.strong_count() > 0);
        streams.push(Arc::downgrade(&job));
        stream
    }

    /// Writes a call down; `make` may add what its pointers name to the
    /// words.
    pub(crate) fn put(self: &Arc<Self>, make: impl FnOnce(&mut Vec<u64>) -> C) {
        if self.broken.load(Relaxed) {
            return;
        }
        let mut inner = held(&self.inner);
        let call = make(&mut inner.words);
        inner.calls.push(call);
        if inner.scheduled {
            return;
        }
        if inner.calls.len() == 1 {
            BUSY_STREAMS.fetch_add(1, Relaxed);
        }
        if inner.calls.len() >= WAKE_AT {
            inner.scheduled = true;
            drop(inner);
            schedule(self.clone());
        }
    }

    /// Hands what is written down to a thread of ours now.
    pub(crate) fn flush(self: &Arc<Self>) {
        let mut inner = held(&self.inner);
        if !inner.scheduled && !inner.calls.is_empty() {
            inner.scheduled = true;
            drop(inner);
            schedule(self.clone());
        }
    }

    /// Waits until every call written down is made; whether it had to.
    pub(crate) fn drain(self: &Arc<Self>, why: Why) -> bool {
        if translating() {
            // (The thread that runs a stream finds it as far as it is.)
            return false;
        }
        self.flush();
        match self.wait(Duration::MAX) {
            Some(time) => {
                waited(why, time);
                true
            }
            None => false,
        }
    }

    fn wait(&self, most: Duration) -> Option<Duration> {
        let mut inner = held(&self.inner);
        if !inner.scheduled {
            return None;
        }
        let began = Instant::now();
        while inner.scheduled {
            let left = most.saturating_sub(began.elapsed());
            if left.is_zero() {
                break;
            }
            inner = if most == Duration::MAX {
                self.moved.wait(inner).unwrap_or_else(|e| e.into_inner())
            } else {
                self.moved.wait_timeout(inner, left).unwrap_or_else(|e| e.into_inner()).0
            };
        }
        Some(began.elapsed())
    }

    /// Whether a panic ended it.
    #[cfg(test)]
    fn broken(&self) -> bool {
        self.broken.load(Relaxed)
    }
}

impl<C: Send + 'static> Job for Stream<C> {
    fn work(&self) {
        loop {
            let (mut calls, mut words) = {
                let mut inner = held(&self.inner);
                if inner.calls.is_empty() {
                    inner.scheduled = false;
                    BUSY_STREAMS.fetch_sub(1, Relaxed);
                    self.moved.notify_all();
                    return;
                }
                let (calls, words) = inner.spare.take().unwrap_or_default();
                (std::mem::replace(&mut inner.calls, calls), std::mem::replace(&mut inner.words, words))
            };
            let began = Instant::now();
            if !self.broken.load(Relaxed) {
                let run = std::panic::AssertUnwindSafe(|| (self.run)(self.target, &calls, &words));
                if std::panic::catch_unwind(run).is_err() {
                    // Nothing waits for it for ever: what is left is thrown
                    // away, and the run ends as it does for a panic in a
                    // list method.
                    self.broken.store(true, Relaxed);
                    (self.failed)("a thread that translates list calls panicked");
                }
            }
            STATS[CALLS].fetch_add(calls.len() as u64, Relaxed);
            STATS[WORDS].fetch_add(words.len() as u64, Relaxed);
            STATS[BUSY].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
            STATS[PIECES].fetch_add(1, Relaxed);
            STATS[LARGEST].fetch_max(calls.len() as u64, Relaxed);
            calls.clear();
            words.clear();
            if calls.capacity() <= 1 << 16 && words.capacity() <= 1 << 18 {
                held(&self.inner).spare = Some((calls, words));
            }
        }
    }

    fn settle(self: Arc<Self>, most: Duration) -> bool {
        self.flush();
        self.wait(most).is_some()
    }
}

/// An object of the title's is about to be freed: what is written down now
/// may name it (a descriptor, a render target view) and is translated
/// first. The wait is bounded, so a thread that frees while it holds what a
/// translating thread needs only loses time.
pub(crate) fn before_free() {
    static WAITS: OnceLock<bool> = OnceLock::new();
    if !on() || translating() || UNNAMED.get() || BUSY_STREAMS.load(Relaxed) == 0 {
        return;
    }
    // (`VICTORY_THIN_FREE_WAIT=0`: frees do not wait, to measure what the
    // waits cost; not safe to play with.)
    if !*WAITS.get_or_init(|| std::env::var_os("VICTORY_THIN_FREE_WAIT").is_none_or(|v| v != "0")) {
        return;
    }
    let most = Duration::from_millis(100);
    if let Some(time) = settle_all(most) {
        waited(Why::Free, time);
        if time >= most {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!("an object was freed before the list calls written down were all translated (waited 100 ms)")
            });
        }
    }
}

/// Hands over what every stream holds now and waits until it is done, for
/// at most `most` in all: how long, if there was anything to wait for.
fn settle_all(most: Duration) -> Option<Duration> {
    let streams: Vec<Arc<dyn Job>> = held(&STREAMS).iter().filter_map(std::sync::Weak::upgrade).collect();
    let began = Instant::now();
    let mut any = false;
    for stream in streams {
        any |= stream.settle(most.saturating_sub(began.elapsed()));
    }
    any.then(|| began.elapsed())
}

/// Adds `items` to the words of a stream: where they begin.
pub(crate) fn put_items<T: Copy>(words: &mut Vec<u64>, items: &[T]) -> u32 {
    const { assert!(align_of::<T>() <= 8) };
    let at = words.len();
    let bytes = size_of_val(items);
    words.resize(at + bytes.div_ceil(8), 0);
    // SAFETY: the words just added hold `bytes` bytes; plain data.
    unsafe { std::ptr::copy_nonoverlapping(items.as_ptr().cast::<u8>(), words.as_mut_ptr().add(at).cast::<u8>(), bytes) };
    at as u32
}

/// The `count` items [`put_items`] put at `at` (none if the words do not
/// hold them).
pub(crate) fn items<T: Copy>(words: &[u64], at: u32, count: u32) -> &[T] {
    const { assert!(align_of::<T>() <= 8) };
    let (at, count) = (at as usize, count as usize);
    let need = (count * size_of::<T>()).div_ceil(8);
    if count == 0 || at.checked_add(need).is_none_or(|end| end > words.len()) {
        return &[];
    }
    // SAFETY: the words from `at` hold `count` items, put there as bytes.
    unsafe { std::slice::from_raw_parts(words[at..].as_ptr().cast(), count) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the streams of the tests did, by target.
    static DONE: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());
    static FAILED: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Copy)]
    enum Call {
        Plain(u64),
        /// The sum of the items at `at`.
        Sum { at: u32, count: u32 },
        Slow(u64),
        Panic,
    }

    fn run(target: usize, calls: &[Call], words: &[u64]) {
        for call in calls {
            let value = match *call {
                Call::Plain(v) => v,
                Call::Sum { at, count } => items::<u32>(words, at, count).iter().map(|&v| u64::from(v)).sum(),
                Call::Slow(v) => {
                    std::thread::sleep(Duration::from_millis(2));
                    v
                }
                Call::Panic => panic!("a call of the test panics"),
            };
            held(&DONE).push((target, value));
        }
    }

    fn failed(_: &str) {
        FAILED.fetch_add(1, Relaxed);
    }

    fn done(target: usize) -> Vec<u64> {
        held(&DONE).iter().filter(|d| d.0 == target).map(|d| d.1).collect()
    }

    #[test]
    fn calls_are_made_in_the_order_they_were_written_down() {
        let stream = Stream::new(1, run, failed);
        for v in 0..5000 {
            stream.put(|_| Call::Plain(v));
        }
        stream.drain(Why::End);
        assert_eq!(done(1), (0..5000).collect::<Vec<_>>());
    }

    #[test]
    fn a_drain_returns_when_every_call_is_made_and_the_stream_goes_on() {
        let stream = Stream::new(2, run, failed);
        assert!(!stream.drain(Why::Call), "nothing written down, nothing to wait for");
        for v in 0..40 {
            stream.put(|_| Call::Slow(v));
        }
        assert!(stream.drain(Why::Call));
        assert_eq!(done(2).len(), 40, "the drain came back before the calls were made");
        // Fewer calls than wake a thread: the drain hands them over.
        stream.put(|_| Call::Plain(40));
        stream.put(|_| Call::Plain(41));
        assert!(done(2).len() <= 42);
        stream.drain(Why::End);
        assert_eq!(done(2), (0..42).collect::<Vec<_>>());
        assert!(!stream.drain(Why::End));
    }

    #[test]
    fn what_a_call_names_is_copied_when_it_is_written_down() {
        let stream = Stream::new(3, run, failed);
        let mut values = [1u32, 2, 3, 4, 5];
        for round in 0..100u32 {
            values[0] = round;
            stream.put(|words| Call::Sum { at: put_items(words, &values), count: 5 });
            // The title uses its memory again right after the call.
            values = [1000; 5];
            values = [1, 2, 3, 4, values[4] - 995];
        }
        values = [0; 5];
        stream.drain(Why::End);
        assert_eq!(values, [0; 5]);
        assert_eq!(done(3), (0..100).map(|round| round + 14).collect::<Vec<u64>>());
    }

    #[test]
    fn items_that_are_not_there_are_none() {
        let mut words = Vec::new();
        let at = put_items(&mut words, &[7u32, 8, 9]);
        assert_eq!(items::<u32>(&words, at, 3), &[7, 8, 9]);
        assert_eq!(words.len(), 2);
        assert!(items::<u32>(&words, at, 5).is_empty());
        assert!(items::<u32>(&words, 9, 1).is_empty());
        assert!(items::<u64>(&words, 0, 0).is_empty());
        let empty = put_items::<u64>(&mut words, &[]);
        assert!(items::<u64>(&words, empty, 0).is_empty());
    }

    #[test]
    fn a_panic_ends_the_stream_and_nobody_waits_for_ever() {
        let stream = Stream::new(4, run, failed);
        let before = FAILED.load(Relaxed);
        stream.put(|_| Call::Plain(1));
        stream.put(|_| Call::Panic);
        stream.put(|_| Call::Plain(2));
        stream.drain(Why::End);
        assert!(stream.broken());
        assert_eq!(FAILED.load(Relaxed), before + 1);
        assert_eq!(done(4), [1], "what follows a panic in its piece is not made");
        // Written down after it: thrown away, and a drain comes back.
        for v in 0..100 {
            stream.put(|_| Call::Plain(v));
        }
        stream.drain(Why::End);
        assert_eq!(done(4), [1]);
    }

    #[test]
    fn streams_run_beside_each_other_and_each_keeps_its_order() {
        let streams: Vec<_> = (0..12).map(|n| Stream::new(100 + n, run, failed)).collect();
        let writers: Vec<_> = streams
            .iter()
            .cloned()
            .map(|stream| {
                std::thread::spawn(move || {
                    for v in 0..2000 {
                        stream.put(|_| Call::Plain(v));
                        if v % 500 == 499 {
                            // A call that is not written down, in the middle.
                            stream.drain(Why::Call);
                        }
                    }
                })
            })
            .collect();
        writers.into_iter().for_each(|w| w.join().unwrap());
        for (n, stream) in streams.iter().enumerate() {
            stream.drain(Why::End);
            assert_eq!(done(100 + n), (0..2000).collect::<Vec<_>>());
        }
    }

    #[test]
    fn before_an_object_is_freed_every_stream_is_done_with_what_it_holds() {
        let stream = Stream::new(5, run, failed);
        // Fewer than wake a thread, and nobody drains this stream itself.
        for v in 0..5 {
            stream.put(|_| Call::Slow(v));
        }
        assert!(settle_all(Duration::from_secs(20)).is_some());
        assert_eq!(done(5), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_thread_that_translates_does_not_wait_for_streams() {
        fn nested(target: usize, calls: &[Call], _: &[u64]) {
            // Our own code asks for the list it is translating.
            assert!(translating());
            before_free();
            held(&DONE).push((target, calls.len() as u64));
        }
        let stream = Stream::new(6, nested, failed);
        stream.put(|_| Call::Plain(0));
        stream.drain(Why::End);
        assert_eq!(done(6), [1]);
        assert!(!translating());
    }
}
