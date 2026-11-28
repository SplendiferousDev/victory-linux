//! Headroom under the card's memory budget (`VICTORY_VRAM_HEADROOM_MB=<n>`;
//! off while unset or 0).
//!
//! Every NVIDIA "Failed to insert new mapping node" so far (runs of
//! 2026-11-26/27, an 8 GB card under vkd3d-proton) came while the card's
//! own memory in use stood at the budget the system reports: the driver
//! then moves allocations out to the PC's memory and remaps them. The
//! budget is not fixed: it is the card's memory less what other users of
//! the card hold, and in those runs it fell by some 700 MB in the first two
//! minutes of the world (7,011 to 6,276 MB) while ours stayed flat, after
//! the texture queue, the card copies of title buffers and the windows had
//! already been placed against the higher number.
//!
//! With the switch, two things:
//! - Planning: the rules that place our copies on the card (the texture
//!   queue's room, `crate::resource::room`; card copies of title buffers
//!   and windows, `crate::shadow`; the idle time of host textures) see the
//!   budget less `n` ([`planned`]), so what they place leaves `n` free.
//! - Holding: at each memory line (every five seconds, `crate::host`) the
//!   governor ([`Governor`]) compares the card's most in use with the
//!   budget's least less `n`. Over that line it holds back one more kind of
//!   placement, in a fixed order of least cost: (1) windows on the card or
//!   in GPU upload heaps (new ones stay in the PC's memory; the ones the
//!   sweep lets go come back there), (2) card copies of title buffers (a
//!   buffer turned down keeps its copy in the PC's memory), (3) host copies
//!   of textures the texture queue makes (they wait, as when the card is
//!   full; the idle sweep, at its shortest idle time, lets unused ones go).
//!   One level per [`STEP_MS`] at most; one level back after use stayed
//!   [`calm_gap`] under the line for [`CALM_MS`].
//!
//! Nothing is freed by the governor itself: the driver's trouble is in
//! remapping, so it sheds by not allocating and lets the sweeps that run
//! anyway give memory back. The scratch and staging pools are upload
//! memory in the PC's memory, not the card's: left alone.
//!
//! While a level is held, one line per memory line says the numbers and
//! what was held back since the last one.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// The least time between two steps up, in ms.
pub(crate) const STEP_MS: u64 = 10_000;
/// How long use must stay under the line less [`calm_gap`] before a step
/// down, in ms (and between two steps down).
pub(crate) const CALM_MS: u64 = 30_000;
/// The highest level.
pub(crate) const TOP: u8 = 3;

/// What a level holds back, by level from 1.
const HELD: [&str; 3] = ["windows on the card", "card copies of title buffers", "host copies of textures from the queue"];

/// The gap under the line use must reach to step down: a quarter of the
/// headroom, at least 128 MB.
pub(crate) fn calm_gap(headroom: u64) -> u64 {
    (headroom / 4).max(128 << 20)
}

/// The headroom in bytes from the switch's text: `None` off.
pub(crate) fn parse(text: Option<&str>) -> Option<u64> {
    let mb = text?.trim().parse::<u64>().ok()?;
    (mb > 0).then(|| mb.min(16_384) << 20)
}

/// `VICTORY_VRAM_HEADROOM_MB` in bytes (read once; `None` off).
pub(crate) fn headroom() -> Option<u64> {
    static N: OnceLock<Option<u64>> = OnceLock::new();
    *N.get_or_init(|| {
        let n = parse(std::env::var("VICTORY_VRAM_HEADROOM_MB").ok().as_deref());
        if let Some(n) = n {
            tracing::info!(
                "headroom under the card's budget (VICTORY_VRAM_HEADROOM_MB={}): placements plan against the budget less it; over it, \
                 held back in turn: {}",
                n >> 20,
                HELD.join(", then ")
            );
        }
        n
    })
}

/// The budget the placement rules plan against: `budget` less `headroom`,
/// never under half of it.
pub(crate) fn plan(budget: u64, headroom: u64) -> u64 {
    budget.saturating_sub(headroom).max(budget / 2)
}

/// `Host::video_memory`'s answer ([budget, in use, ...]) with the budget
/// the placement rules plan against ([`plan`]), when the switch is on.
pub(crate) fn planned(video_memory: Option<[u64; 4]>) -> Option<[u64; 4]> {
    match (video_memory, headroom()) {
        (Some([budget, used, a, b]), Some(n)) => Some([plan(budget, n), used, a, b]),
        (v, _) => v,
    }
}

/// The governor's state, pure: fed the memory line's numbers.
#[derive(Debug, Default, Clone)]
pub(crate) struct Governor {
    pub(crate) level: u8,
    /// When the level last changed (ms; `None`: never).
    changed: Option<u64>,
    /// Since when use is under the line less the calm gap (`None`: not).
    calm_since: Option<u64>,
}

impl Governor {
    /// One memory line: the card's most in use and the budget's least over
    /// it, at `now` ms. Returns the new level if it changed.
    pub(crate) fn observe(&mut self, headroom: u64, used: u64, budget: u64, now: u64) -> Option<u8> {
        if budget == 0 {
            return None;
        }
        let line = budget.saturating_sub(headroom);
        let since_change = self.changed.map_or(u64::MAX, |at| now.saturating_sub(at));
        if used > line {
            self.calm_since = None;
            if self.level < TOP && since_change >= STEP_MS {
                self.level += 1;
                self.changed = Some(now);
                return Some(self.level);
            }
            return None;
        }
        if used + calm_gap(headroom) > line {
            // Between the two lines: hold what is held.
            self.calm_since = None;
            return None;
        }
        let calm = now.saturating_sub(*self.calm_since.get_or_insert(now));
        if self.level > 0 && calm >= CALM_MS && since_change >= CALM_MS {
            self.level -= 1;
            self.changed = Some(now);
            self.calm_since = Some(now);
            return Some(self.level);
        }
        None
    }
}

/// The level in force, for the placement rules.
static LEVEL: AtomicU8 = AtomicU8::new(0);
static GOVERNOR: Mutex<Governor> = Mutex::new(Governor { level: 0, changed: None, calm_since: None });

/// The governor's level now (0: nothing held).
pub(crate) fn level() -> u8 {
    LEVEL.load(Ordering::Relaxed)
}

/// A kind of placement the governor may hold back (its level).
#[derive(Clone, Copy)]
pub(crate) enum Hold {
    Windows = 1,
    CardCopies = 2,
    Textures = 3,
}

/// Placements held back since the last line, by kind: count and bytes.
static HELD_BACK: [[AtomicU64; 2]; 3] = [const { [const { AtomicU64::new(0) }; 2] }; 3];

/// Whether placements of `kind` are held back now; if so, `bytes` are
/// counted for the line.
pub(crate) fn holds(kind: Hold, bytes: u64) -> bool {
    if LEVEL.load(Ordering::Relaxed) < kind as u8 {
        return false;
    }
    let i = kind as usize - 1;
    HELD_BACK[i][0].fetch_add(1, Ordering::Relaxed);
    HELD_BACK[i][1].fetch_add(bytes, Ordering::Relaxed);
    true
}

/// The use when the governor went from level 0 to 1 (for the line).
static USE_AT_HOLD: AtomicU64 = AtomicU64::new(0);

/// With every memory line (`crate::host`): the card's most in use and the
/// budget's least since the last one, at `now` ms.
pub(crate) fn memory_line(used: u64, budget: u64, now: u64) {
    let Some(n) = headroom() else { return };
    if budget == u64::MAX {
        return;
    }
    let (level, changed) = {
        let mut g = GOVERNOR.lock().unwrap_or_else(|e| e.into_inner());
        let before = g.level;
        let changed = g.observe(n, used, budget, now);
        if before == 0 && g.level > 0 {
            USE_AT_HOLD.store(used, Ordering::Relaxed);
        }
        (g.level, changed.is_some())
    };
    LEVEL.store(level, Ordering::Relaxed);
    let held: Vec<[u64; 2]> =
        HELD_BACK.iter().map(|h| [h[0].swap(0, Ordering::Relaxed), h[1].swap(0, Ordering::Relaxed)]).collect();
    if level == 0 && !changed {
        return;
    }
    let line = budget.saturating_sub(n);
    let mb = |b: u64| b >> 20;
    let state = if used > line {
        format!("{} MB over the line", mb(used - line))
    } else {
        format!("{} MB under the line", mb(line - used))
    };
    tracing::info!(
        "headroom (VICTORY_VRAM_HEADROOM_MB={}): level {level} of {TOP}{}{}; the card's own in use at most {} MB, budget at least {} MB, \
         line {} MB ({state}; back down after {} s under {} MB); held back since the last line: {} windows {} MB, {} card copies {} MB, \
         {} texture-queue asks; use since holding began {:+} MB",
        mb(n),
        if changed { " (changed now)" } else { "" },
        if level > 0 { format!(", holding {}", HELD[..level as usize].join(", ")) } else { String::new() },
        mb(used),
        mb(budget),
        mb(line),
        CALM_MS / 1000,
        mb(line.saturating_sub(calm_gap(n))),
        held[0][0],
        mb(held[0][1]),
        held[1][0],
        mb(held[1][1]),
        held[2][0],
        (mb(used) as i64) - (mb(USE_AT_HOLD.load(Ordering::Relaxed)) as i64),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    #[test]
    fn the_switch_reads_megabytes() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("alt")), None);
        assert_eq!(parse(Some(" 768 ")), Some(768 * MB));
        assert_eq!(parse(Some("99999")), Some(16_384 * MB));
    }

    #[test]
    fn planning_takes_the_headroom_off_but_not_below_half() {
        assert_eq!(plan(7011 * MB, 768 * MB), 6243 * MB);
        assert_eq!(plan(1000 * MB, 768 * MB), 500 * MB);
        assert_eq!(plan(0, 768 * MB), 0);
    }

    #[test]
    fn under_the_line_nothing_is_held() {
        let mut g = Governor::default();
        for t in 0..20 {
            assert_eq!(g.observe(768 * MB, 5000 * MB, 7000 * MB, t * 5000), None);
        }
        assert_eq!(g.level, 0);
    }

    #[test]
    fn over_the_line_it_steps_up_one_level_per_step_time() {
        let mut g = Governor::default();
        // Line 6276 - 300 = 5976 MB; use 6263 MB is over it.
        let (n, used, budget) = (300 * MB, 6263 * MB, 6276 * MB);
        assert_eq!(g.observe(n, used, budget, 0), Some(1));
        // The next memory line, 5 s later: too soon.
        assert_eq!(g.observe(n, used, budget, 5_000), None);
        assert_eq!(g.observe(n, used, budget, 10_000), Some(2));
        assert_eq!(g.observe(n, used, budget, 15_000), None);
        assert_eq!(g.observe(n, used, budget, 20_000), Some(3));
        // The top holds.
        for t in 5..40 {
            assert_eq!(g.observe(n, used, budget, t * 5_000), None);
        }
        assert_eq!(g.level, TOP);
    }

    #[test]
    fn a_falling_budget_alone_raises_the_level() {
        // Our use flat, the budget falling as other users of the card grow
        // (as in the runs that met it).
        let mut g = Governor::default();
        let n = 300 * MB;
        assert_eq!(g.observe(n, 6260 * MB, 7011 * MB, 0), None);
        assert_eq!(g.observe(n, 6260 * MB, 6650 * MB, 5_000), None);
        assert_eq!(g.observe(n, 6260 * MB, 6530 * MB, 10_000), Some(1));
    }

    #[test]
    fn between_the_lines_it_holds_and_does_not_step_down() {
        let mut g = Governor::default();
        let n = 512 * MB; // calm gap 128 MB
        g.observe(n, 6400 * MB, 6400 * MB, 0);
        assert_eq!(g.level, 1);
        // Line 5888; 5800 is under it but within the gap (5760).
        for t in 1..100 {
            assert_eq!(g.observe(n, 5800 * MB, 6400 * MB, t * 5_000), None);
        }
        assert_eq!(g.level, 1);
    }

    #[test]
    fn it_steps_down_only_after_calm_time_one_level_at_a_time() {
        let mut g = Governor::default();
        let n = 512 * MB;
        for t in 0..5 {
            g.observe(n, 6400 * MB, 6400 * MB, t * 10_000);
        }
        assert_eq!(g.level, 3);
        let t0 = 50_000;
        // Well under: 5000 < 5888 - 128.
        assert_eq!(g.observe(n, 5000 * MB, 6400 * MB, t0), None);
        assert_eq!(g.observe(n, 5000 * MB, 6400 * MB, t0 + 25_000), None);
        assert_eq!(g.observe(n, 5000 * MB, 6400 * MB, t0 + 30_000), Some(2));
        // The next step down needs another calm time.
        assert_eq!(g.observe(n, 5000 * MB, 6400 * MB, t0 + 35_000), None);
        assert_eq!(g.observe(n, 5000 * MB, 6400 * MB, t0 + 60_000), Some(1));
        assert_eq!(g.observe(n, 5000 * MB, 6400 * MB, t0 + 90_000), Some(0));
        assert_eq!(g.observe(n, 5000 * MB, 6400 * MB, t0 + 120_000), None);
    }

    #[test]
    fn a_spike_restarts_the_calm_time() {
        let mut g = Governor::default();
        let n = 512 * MB;
        g.observe(n, 6400 * MB, 6400 * MB, 0);
        assert_eq!(g.observe(n, 5000 * MB, 6400 * MB, 40_000), None);
        // Over again at 60 s: steps up (10 s since the last change passed).
        assert_eq!(g.observe(n, 6000 * MB, 6400 * MB, 60_000), Some(2));
        assert_eq!(g.observe(n, 5000 * MB, 6400 * MB, 65_000), None);
        assert_eq!(g.observe(n, 5000 * MB, 6400 * MB, 90_000), None);
        assert_eq!(g.observe(n, 5000 * MB, 6400 * MB, 95_000), Some(1));
    }

    #[test]
    fn alternating_lines_do_not_thrash() {
        // Use crossing the line every memory line: at most one change per
        // step time up, and never down (no calm time).
        let mut g = Governor::default();
        let n = 300 * MB;
        let mut changes = 0;
        for t in 0..60u64 {
            let used = if t % 2 == 0 { 6300 * MB } else { 5800 * MB };
            if g.observe(n, used, 6276 * MB, t * 5_000).is_some() {
                changes += 1;
            }
        }
        assert_eq!(changes, 3);
        assert_eq!(g.level, TOP);
    }

    #[test]
    fn no_budget_no_decision() {
        let mut g = Governor::default();
        assert_eq!(g.observe(300 * MB, 6000 * MB, 0, 0), None);
        assert_eq!(g.level, 0);
    }

    #[test]
    fn the_calm_gap_is_a_quarter_but_at_least_128_mb() {
        assert_eq!(calm_gap(256 * MB), 128 * MB);
        assert_eq!(calm_gap(1024 * MB), 256 * MB);
    }
}
