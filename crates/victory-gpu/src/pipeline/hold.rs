//! Holding the title at its first screen until the early pipelines are
//! made (`VICTORY_PIPELINE_HOLD=1`, off by default).
//!
//! The title creates its pipelines while it boots, and the ones an earlier
//! run drew with are started at once ([`super::make::early`]). On a PC short
//! of cores and memory those take minutes; if the title loads its world
//! meanwhile, every pipeline waits behind them, draws are dropped by the
//! thousand and the picture stays black. While the hold is on, the input
//! devices report no buttons, sticks or keys (they stay connected): the
//! title goes on running and presenting its first interactive screen,
//! which waits for a button, so its main thread keeps making progress and
//! nothing of its own is delayed. Scripted pad steps wait as well.
//!
//! The hold ends when the queue is empty and every listed pipeline was
//! started, or when it has stayed empty for [`SETTLE`] after some were
//! started (the title may never create some listed ones before its world),
//! when there is no early list, or after the timeout
//! (`VICTORY_PIPELINE_HOLD_SECS`, default [`TIMEOUT`]).

use std::time::Duration;

/// The longest hold, unless `VICTORY_PIPELINE_HOLD_SECS` says otherwise.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(300);
/// How long the queue must stay empty when not every listed pipeline was
/// started.
pub(crate) const SETTLE: Duration = Duration::from_secs(10);

/// What the gate sees at a check.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Counts {
    /// Pipelines on the early list read at the start; `None` until the
    /// title created its first pipeline (the list is read then).
    pub(crate) listed: Option<u64>,
    /// Title pipelines started early so far.
    pub(crate) started: u64,
    /// Pipelines queued and not made yet.
    pub(crate) left: u64,
}

/// Why the hold ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Release {
    /// Every listed pipeline was started and all are made.
    AllMade,
    /// The queue stayed empty for [`SETTLE`].
    Settled,
    /// No early list (a first run, or no cache).
    NothingListed,
    TimedOut,
}

/// The gate's state; times are from the first check.
#[derive(Debug)]
pub(crate) struct Gate {
    timeout: Duration,
    /// When the queue was last seen becoming empty, while it still is.
    empty_since: Option<Duration>,
    released: Option<Release>,
}

impl Gate {
    pub(crate) fn new(timeout: Duration) -> Gate {
        Gate {
            timeout,
            empty_since: None,
            released: None,
        }
    }

    /// Whether the title is released at `now`; `Some` once it is (the
    /// reason given at the first such check stays).
    pub(crate) fn check(&mut self, now: Duration, counts: Counts) -> Option<Release> {
        if self.released.is_some() {
            return self.released;
        }
        if counts.left == 0 {
            self.empty_since.get_or_insert(now);
        } else {
            self.empty_since = None;
        }
        let released = match counts.listed {
            Some(0) => Some(Release::NothingListed),
            Some(listed) if counts.left == 0 && counts.started >= listed => Some(Release::AllMade),
            _ if counts.started > 0
                && self
                    .empty_since
                    .is_some_and(|since| now.saturating_sub(since) >= SETTLE) =>
            {
                Some(Release::Settled)
            }
            _ if now >= self.timeout => Some(Release::TimedOut),
            _ => None,
        };
        self.released = released;
        released
    }
}

#[cfg(windows)]
mod global {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use super::{Counts, Gate, Release, TIMEOUT};

    const REPORT_EVERY: Duration = Duration::from_secs(10);

    fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_HOLD").is_some_and(|v| v == "1"))
    }

    fn timeout() -> Duration {
        std::env::var("VICTORY_PIPELINE_HOLD_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(TIMEOUT, Duration::from_secs)
    }

    /// Set once the hold is over (or never was): the input path's fast way out.
    static RELEASED: AtomicBool = AtomicBool::new(false);
    /// Pipelines left as last reported, for the frame-rate box; `u64::MAX`
    /// when not holding.
    static SHOWN: AtomicU64 = AtomicU64::new(u64::MAX);

    struct Held {
        gate: Gate,
        start: Instant,
        reported: Option<Instant>,
    }

    fn counts() -> Counts {
        use super::super::library::{EARLY, LISTED};
        let listed = LISTED.load(Ordering::Relaxed);
        Counts {
            listed: (listed != u64::MAX).then_some(listed),
            started: EARLY.load(Ordering::Relaxed),
            left: super::super::make::waiting(),
        }
    }

    /// Whether the title's input is held now (called by every input poll).
    pub(crate) fn input_held() -> bool {
        if RELEASED.load(Ordering::Relaxed) {
            return false;
        }
        if !on() {
            RELEASED.store(true, Ordering::Relaxed);
            return false;
        }
        static HELD: Mutex<Option<Held>> = Mutex::new(None);
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        let held = held.get_or_insert_with(|| {
            let timeout = timeout();
            tracing::info!(
                timeout_s = timeout.as_secs(),
                "holding the title's input until the early pipelines are made (VICTORY_PIPELINE_HOLD)"
            );
            Held { gate: Gate::new(timeout), start: Instant::now(), reported: None }
        });
        let counts = counts();
        let waited = held.start.elapsed();
        if let Some(why) = held.gate.check(waited, counts) {
            RELEASED.store(true, Ordering::Relaxed);
            SHOWN.store(u64::MAX, Ordering::Relaxed);
            let why = match why {
                Release::AllMade => "early pipelines all made",
                Release::Settled => "pipeline queue empty",
                Release::NothingListed => "no early pipeline list",
                Release::TimedOut => "timed out",
            };
            tracing::info!(
                why,
                waited_s = waited.as_secs(),
                left = counts.left,
                started_early = counts.started,
                listed = counts.listed.unwrap_or(0),
                "title's input released"
            );
            return false;
        }
        if held.reported.is_none_or(|t| t.elapsed() >= REPORT_EVERY) {
            held.reported = Some(Instant::now());
            SHOWN.store(counts.left, Ordering::Relaxed);
            tracing::info!(
                "holding the title's input: {} pipelines left; {} of {} listed started early; {} s of at most {} s",
                counts.left,
                counts.started,
                counts
                    .listed
                    .map_or_else(|| "?".to_owned(), |n| n.to_string()),
                waited.as_secs(),
                held.gate.timeout.as_secs()
            );
        }
        true
    }

    /// Pipelines left while the hold is on (updated with the log line).
    pub(crate) fn shown_left() -> Option<u64> {
        let left = SHOWN.load(Ordering::Relaxed);
        (left != u64::MAX && !RELEASED.load(Ordering::Relaxed)).then_some(left)
    }
}

#[cfg(windows)]
pub(crate) use global::{input_held, shown_left};

#[cfg(test)]
mod tests {
    use super::*;

    const fn s(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    fn counts(listed: Option<u64>, started: u64, left: u64) -> Counts {
        Counts {
            listed,
            started,
            left,
        }
    }

    #[test]
    fn holds_while_pipelines_are_left_and_releases_when_all_are_made() {
        let mut gate = Gate::new(TIMEOUT);
        // Before the title created a pipeline, the list is not read yet.
        assert_eq!(gate.check(s(0), counts(None, 0, 0)), None);
        assert_eq!(gate.check(s(5), counts(Some(2000), 300, 290)), None);
        assert_eq!(gate.check(s(60), counts(Some(2000), 2000, 1200)), None);
        assert_eq!(gate.check(s(199), counts(Some(2000), 2000, 1)), None);
        assert_eq!(
            gate.check(s(200), counts(Some(2000), 2000, 0)),
            Some(Release::AllMade)
        );
        // It stays released, whatever comes later.
        assert_eq!(
            gate.check(s(201), counts(Some(2000), 2100, 50)),
            Some(Release::AllMade)
        );
    }

    #[test]
    fn a_short_empty_queue_while_pipelines_are_still_created_does_not_release() {
        let mut gate = Gate::new(TIMEOUT);
        assert_eq!(gate.check(s(10), counts(Some(2000), 100, 0)), None);
        assert_eq!(gate.check(s(19), counts(Some(2000), 100, 0)), None);
        // New ones queued: the empty stretch starts again.
        assert_eq!(gate.check(s(19), counts(Some(2000), 500, 400)), None);
        assert_eq!(gate.check(s(40), counts(Some(2000), 500, 0)), None);
        assert_eq!(gate.check(s(49), counts(Some(2000), 500, 0)), None);
        // Empty for SETTLE although not every listed one was started.
        assert_eq!(
            gate.check(s(50), counts(Some(2000), 500, 0)),
            Some(Release::Settled)
        );
    }

    #[test]
    fn an_empty_queue_before_any_was_started_waits_for_the_timeout() {
        let mut gate = Gate::new(s(120));
        assert_eq!(gate.check(s(0), counts(None, 0, 0)), None);
        assert_eq!(gate.check(s(60), counts(Some(2000), 0, 0)), None);
        assert_eq!(gate.check(s(119), counts(Some(2000), 0, 0)), None);
        assert_eq!(
            gate.check(s(120), counts(Some(2000), 0, 0)),
            Some(Release::TimedOut)
        );
    }

    #[test]
    fn times_out_with_pipelines_still_left() {
        let mut gate = Gate::new(s(30));
        assert_eq!(gate.check(s(29), counts(Some(10), 10, 3)), None);
        assert_eq!(
            gate.check(s(30), counts(Some(10), 10, 3)),
            Some(Release::TimedOut)
        );
    }

    #[test]
    fn nothing_listed_releases_at_once() {
        let mut gate = Gate::new(TIMEOUT);
        assert_eq!(
            gate.check(s(1), counts(Some(0), 0, 5)),
            Some(Release::NothingListed)
        );
    }
}
