//! Texture eviction against the headroom plan
//! (`VICTORY_TEXTURE_BUDGET_EVICT`: unset or `0` off, `1` on, `alt` twenty
//! seconds off, twenty on).
//!
//! The headroom governor (`crate::headroom`, `VICTORY_VRAM_HEADROOM_MB`)
//! plans against the card's budget less the headroom and, over that line,
//! holds back new placements; it frees nothing. The idle sweep
//! ([`super::let_go_idle`]) lets go only the host copies of textures the
//! title fills that no list and no descriptor slot holds, and in the world
//! it finds none: every copy left is shown by a slot of the title's view
//! heap (a slot keeps what it views), so the card's use stays flat at the
//! line while the budget under it falls.
//!
//! With the switch, while the governor is at its top level (the texture
//! queue makes no new copies) and the card's use is over the line, once a
//! second (with the sweep):
//! 1. [`round`] picks host copies of textures the title fills, least
//!    recently asked for first ([`choose`]): only copies no list holds (no
//!    list not yet reset names them, so no submission recorded or in flight
//!    uses them but through a slot), that nothing asked for in the last
//!    [`MIN_IDLE_MS`] and [`MIN_IDLE_FRAMES`] frames, never render targets
//!    or other textures the host writes, until the bytes over the line (less
//!    those already picked and not yet gone) are reached or the round's caps
//!    ([`Caps`]).
//! 2. At the next pass of the texture queue, the view heap makes the title
//!    slots showing them null again, as it does for a texture whose memory
//!    was unmapped (`HostHeap::texture_slots_evicted`), and puts the slots
//!    back in the queue as due: a texture is taken only if every slot that
//!    shows it is a title slot no draw read through a table lately. The
//!    slots' old references are retired (`crate::host::retire`): a
//!    submission in flight that read the slot before keeps the texture.
//! 3. The next sweep then finds the copy held by its title texture alone and
//!    lets it go, retired again until the submissions started by then end.
//!
//! The next use of such a slot is that of a texture never uploaded: the
//! queue makes the copy anew and fills it from title memory when the card
//! has room again; until then the slot reads nothing. A draw that reads the
//! texture through a table makes it at once, as before.
//!
//! The idea of letting textures go against a byte budget, least recently
//! used first and copies already made again last, is from VIctory-Fast.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Nothing asked for a picked copy for this long ...
pub(crate) const MIN_IDLE_MS: u64 = 3_000;
/// ... and for this many frames (bound this frame or the last few: kept).
pub(crate) const MIN_IDLE_FRAMES: u64 = 8;
/// A picked copy not gone after this long is given up on (its slots are
/// not all ours to make null, or the title asked for it again).
pub(crate) const PENDING_MS: u64 = 10_000;
/// A copy let go is watched this long for being made again (a re-fill).
pub(crate) const WATCH_MS: u64 = 60_000;

/// The switch.
#[cfg(windows)]
static SWITCH: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_TEXTURE_BUDGET_EVICT",
    "host copies of textures the title fills are let go, least recently used first, while the card is over the headroom line",
);

/// What one host copy is, for the choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// The title fills it (from its memory) and the host never wrote it.
    pub filled: bool,
    /// It has a host copy now, of its own memory (not made level by level).
    pub made: bool,
    /// A list not yet reset holds it.
    pub listed: bool,
    /// Something besides its title texture holds the host texture (a slot):
    /// without that the sweep lets it go anyway.
    pub held: bool,
    /// It was picked already and is not gone yet.
    pub pending: bool,
    /// Its copy was let go once and made again.
    pub remade: bool,
    /// Its bytes.
    pub bytes: u64,
    /// When it was last asked for (`crate::host::moment`).
    pub used: u64,
}

/// Whether a copy may be picked at `now` (a `crate::host::moment`).
pub(crate) fn eligible(f: &Facts, now: u64) -> bool {
    f.filled
        && f.made
        && !f.listed
        && f.held
        && !f.pending
        && f.bytes > 0
        && f.used != 0
        && idle_since(f.used, now, MIN_IDLE_MS, MIN_IDLE_FRAMES)
}

/// `crate::host::long_ago` for a given `now`.
pub(crate) fn idle_since(moment: u64, now: u64, ms: u64, frames: u64) -> bool {
    let tenths = (now & 0xffff_ffff).saturating_sub(moment & 0xffff_ffff);
    (now >> 32).saturating_sub(moment >> 32) >= frames && tenths * 100 >= ms
}

/// The most one round picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Caps {
    pub bytes: u64,
    pub count: usize,
}

/// Of `all` (an id and its facts), the ids a round picks to free `want`
/// bytes: eligible ones ([`eligible`]), never made again before the others,
/// least recently asked for first, until `want` or a cap is reached (the
/// copy that crosses `want` is taken; none past the byte cap).
pub(crate) fn choose<T: Copy>(all: &[(T, Facts)], want: u64, now: u64, caps: Caps) -> Vec<T> {
    if want == 0 || caps.count == 0 {
        return Vec::new();
    }
    let mut order: Vec<&(T, Facts)> = all.iter().filter(|(_, f)| eligible(f, now)).collect();
    // By when asked for: the frame count (high half) first, then the time.
    order.sort_by_key(|(_, f)| (f.remade, f.used >> 32, f.used & 0xffff_ffff));
    let mut out = Vec::new();
    let mut bytes = 0u64;
    for (id, f) in order {
        if bytes >= want || out.len() >= caps.count {
            break;
        }
        if bytes + f.bytes > caps.bytes {
            continue;
        }
        bytes += f.bytes;
        out.push(*id);
    }
    out
}

/// The bytes to free: the card's use over the line (`budget` less
/// `headroom`, as the governor draws it) less what was picked and has not
/// gone yet.
pub(crate) fn want(used: u64, budget: u64, headroom: u64, pending: u64) -> u64 {
    let line = budget.saturating_sub(headroom);
    used.saturating_sub(line).saturating_sub(pending)
}

/// Whether the slots that show a picked texture may all be made null: there
/// is at least one, each is a title slot (under `count`; a slot past it is a
/// list's copy of a table) and none was read through a table lately.
pub(crate) fn slots_free(slots: &[u32], count: u32, tabled_lately: impl Fn(u32) -> bool) -> bool {
    !slots.is_empty() && slots.iter().all(|&s| s < count && !tabled_lately(s))
}

/// The round's caps: `VICTORY_TEXTURE_EVICT_MB` (128, at most 1024) and
/// `VICTORY_TEXTURE_EVICT_COUNT` (256, at most 4096).
pub(crate) fn caps_from(mb: Option<&str>, count: Option<&str>) -> Caps {
    let n = |v: Option<&str>| v.and_then(|v| v.trim().parse::<u64>().ok());
    Caps {
        bytes: n(mb).map_or(128, |mb| mb.clamp(1, 1024)) << 20,
        count: n(count).map_or(256, |c| c.clamp(1, 4096)) as usize,
    }
}

/// Since the start: rounds that picked, copies picked and their bytes;
/// picked copies let go and their bytes; let go and made again (re-fills)
/// and their bytes; given up (not gone in time, or asked for again);
/// textures whose slots the heap made null, and those slots; textures it
/// left (a slot not ours to null).
#[cfg(windows)]
static STATS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];

/// The most recent bytes over the plan, and the round's microseconds
/// (most since the last line).
#[cfg(windows)]
static OVER: AtomicU64 = AtomicU64::new(0);
#[cfg(windows)]
static ROUND_US: AtomicU64 = AtomicU64::new(0);

/// A picked copy: its title texture (address), bytes and when picked
/// (`crate::host::now_ms`).
#[cfg(windows)]
struct Picked {
    at: usize,
    bytes: u64,
    when: u64,
    moment: u64,
}

/// Picked copies not gone yet, and copies let go that are watched for
/// being made again.
#[cfg(windows)]
static PICKED: std::sync::Mutex<(Vec<Picked>, Vec<Picked>)> = std::sync::Mutex::new((Vec::new(), Vec::new()));

/// The host textures of the copies picked, for the view heap's next pass
/// (each held here so that its address names it until then; given back
/// through `crate::host::retire`).
#[cfg(windows)]
static WANTED: std::sync::Mutex<Vec<Wanted>> = std::sync::Mutex::new(Vec::new());

/// A host texture picked, held for the view heap's pass.
#[cfg(windows)]
pub(crate) struct Wanted(pub(crate) windows::Win32::Graphics::Direct3D12::ID3D12Resource);
// SAFETY: a D3D12 resource (free-threaded), only compared by address and
// given back through `crate::host::retire`.
#[cfg(windows)]
unsafe impl Send for Wanted {}

/// For the view heap's texture pass: the host textures whose slots are to
/// be made null (empty while the switch is off).
#[cfg(windows)]
pub(crate) fn take_wanted() -> Vec<Wanted> {
    std::mem::take(&mut *WANTED.lock().unwrap_or_else(|e| e.into_inner()))
}

/// The view heap's pass is done with `wanted`: it made null the slots of
/// `textures` of them (`slots` slots) and left `left`. The references held
/// for it are retired.
#[cfg(windows)]
pub(crate) fn slots_done(wanted: Vec<Wanted>, textures: u64, slots: u64, left: u64) {
    STATS[8].fetch_add(textures, Relaxed);
    STATS[9].fetch_add(slots, Relaxed);
    STATS[10].fetch_add(left, Relaxed);
    give_back(wanted);
}

/// Retires the references held for the heap's pass.
#[cfg(windows)]
fn give_back(wanted: Vec<Wanted>) {
    for texture in wanted {
        crate::host::retire(texture.0);
    }
}

/// Once a second, before the idle sweep (from [`super::let_go_idle`]).
#[cfg(windows)]
pub(crate) fn round() {
    use std::sync::Arc;
    if !SWITCH.on() {
        // (Picked ones the heap's pass did not take go back.)
        give_back(take_wanted());
        line(None);
        return;
    }
    let began = std::time::Instant::now();
    let Some(headroom) = crate::headroom::headroom() else {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                "VICTORY_TEXTURE_BUDGET_EVICT is set but VICTORY_VRAM_HEADROOM_MB is not: nothing is let go (eviction follows the headroom governor)"
            );
        });
        return;
    };
    let now_ms = crate::host::now_ms();
    let now = crate::host::moment();
    // The picked copies: gone (let go), asked for again, or given up on;
    // and the ones let go: made again?
    let pending = {
        let all = super::textures();
        let mut picked = PICKED.lock().unwrap_or_else(|e| e.into_inner());
        let (waiting, watched) = &mut *picked;
        let mut still = Vec::new();
        for p in waiting.drain(..) {
            if !all.contains(&p.at) {
                continue;
            }
            // SAFETY: in the table under its lock: alive (`Drop for Resource`).
            let r = unsafe { &*(p.at as *const super::Resource) };
            let made = crate::lockmeter::lock("rs:made", &r.host.made).is_some();
            if !made {
                STATS[3].fetch_add(1, Relaxed);
                STATS[4].fetch_add(p.bytes, Relaxed);
                watched.push(Picked { when: now_ms, ..p });
            } else if r.host.used.load(Relaxed) > p.moment || now_ms.saturating_sub(p.when) > PENDING_MS {
                STATS[7].fetch_add(1, Relaxed);
            } else {
                still.push(p);
            }
        }
        *waiting = still;
        watched.retain(|p| {
            if !all.contains(&p.at) || now_ms.saturating_sub(p.when) > WATCH_MS {
                return false;
            }
            // SAFETY: as above.
            let r = unsafe { &*(p.at as *const super::Resource) };
            if crate::lockmeter::lock("rs:made", &r.host.made).is_some() {
                STATS[5].fetch_add(1, Relaxed);
                STATS[6].fetch_add(p.bytes, Relaxed);
                return false;
            }
            true
        });
        waiting.iter().map(|p| p.bytes).sum::<u64>()
    };
    let memory = crate::host::host().and_then(|h| h.video_memory());
    let Some([budget, used, ..]) = memory else {
        line(Some((0, pending)));
        return;
    };
    let over = used.saturating_sub(budget.saturating_sub(headroom));
    OVER.store(over, Relaxed);
    // Only while the governor holds the texture queue back: below its top
    // level the queue would make the copies again at its next pass.
    let want = if crate::headroom::level() >= crate::headroom::TOP { want(used, budget, headroom, pending) } else { 0 };
    if want > 0 {
        static CAPS: std::sync::OnceLock<Caps> = std::sync::OnceLock::new();
        let caps = *CAPS.get_or_init(|| {
            let var = |name| std::env::var(name).ok();
            caps_from(var("VICTORY_TEXTURE_EVICT_MB").as_deref(), var("VICTORY_TEXTURE_EVICT_COUNT").as_deref())
        });
        let pending_at: std::collections::HashSet<usize> =
            PICKED.lock().unwrap_or_else(|e| e.into_inner()).0.iter().map(|p| p.at).collect();
        let all = super::textures();
        let mut facts: Vec<(usize, Facts)> = Vec::new();
        for &at in all.iter() {
            // SAFETY: as above.
            let r = unsafe { &*(at as *const super::Resource) };
            let used = r.host.used.load(Relaxed);
            // (Without the lock first: most were asked for lately.)
            if used == 0 || !super::fills(&r.desc) || !idle_since(used, now, MIN_IDLE_MS, MIN_IDLE_FRAMES) {
                continue;
            }
            if crate::texture::host_wrote(at) {
                continue;
            }
            let made = crate::lockmeter::lock("rs:made", &r.host.made);
            let Some((host, _)) = made.as_ref() else { continue };
            if host.tiled.is_some() {
                continue;
            }
            let f = Facts {
                filled: host.filled,
                made: true,
                listed: Arc::strong_count(host) > 1,
                held: crate::host::references(&host.texture) > 1,
                pending: pending_at.contains(&at),
                remade: r.host.gone.load(Relaxed),
                bytes: host.bytes,
                used,
            };
            facts.push((at, f));
        }
        let chosen = choose(&facts, want, now, caps);
        if !chosen.is_empty() {
            let mut wanted = Vec::with_capacity(chosen.len());
            let mut picked = Vec::with_capacity(chosen.len());
            for &at in &chosen {
                // SAFETY: as above (the table's lock is still held).
                let r = unsafe { &*(at as *const super::Resource) };
                let made = crate::lockmeter::lock("rs:made", &r.host.made);
                let Some((host, _)) = made.as_ref() else { continue };
                wanted.push(Wanted(host.texture.clone()));
                picked.push(Picked { at, bytes: host.bytes, when: now_ms, moment: now });
            }
            drop(all);
            STATS[0].fetch_add(1, Relaxed);
            STATS[1].fetch_add(picked.len() as u64, Relaxed);
            STATS[2].fetch_add(picked.iter().map(|p| p.bytes).sum::<u64>(), Relaxed);
            // Ones the heap's pass did not take (no pass ran: no shader
            // that picks textures by index was bound) go back; their picks
            // are given up on in time.
            let old = std::mem::replace(&mut *WANTED.lock().unwrap_or_else(|e| e.into_inner()), wanted);
            give_back(old);
            PICKED.lock().unwrap_or_else(|e| e.into_inner()).0.extend(picked);
        }
    }
    ROUND_US.fetch_max(began.elapsed().as_micros() as u64, Relaxed);
    line(Some((over, pending)));
}

/// One line per ten seconds while the switch is set: what went and what
/// came back since the last line. `now`: the bytes over the line and those
/// picked and not gone (`None`: off in this phase).
#[cfg(windows)]
fn line(now: Option<(u64, u64)>) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    static BEFORE: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
    if !SWITCH.set() {
        return;
    }
    let t = crate::host::now_ms();
    let last = LAST.load(Relaxed);
    if (last != 0 && t < last + 10_000) || LAST.compare_exchange(last, t.max(1), Relaxed, Relaxed).is_err() {
        return;
    }
    let s: Vec<u64> = STATS.iter().zip(&BEFORE).map(|(n, b)| {
        let n = n.load(Relaxed);
        n - b.swap(n, Relaxed)
    }).collect();
    let mb = |b: u64| b >> 20;
    let filled = super::KINDS[1].load(Relaxed);
    let all = super::HOST_TEXTURES[1].load(Relaxed);
    let (over, pending) = now.unwrap_or((OVER.load(Relaxed), 0));
    tracing::info!(
        "texture eviction (VICTORY_TEXTURE_BUDGET_EVICT{}): since the last line {} copies picked {} MB in {} rounds, {} let go {} MB, \
         {} made again (re-fills) {} MB, {} given up; the heap made null the slots of {} of them ({} slots) and left {}; now {} MB over \
         the headroom line, {} MB picked and not gone, governor level {}; host copies of textures the title fills {} MB, other host \
         textures (targets, written by the host) {} MB; longest round {} us",
        if now.is_none() { ", off in this phase" } else { "" },
        s[1],
        mb(s[2]),
        s[0],
        s[3],
        mb(s[4]),
        s[5],
        mb(s[6]),
        s[7],
        s[8],
        s[9],
        s[10],
        mb(over),
        mb(pending),
        crate::headroom::level(),
        mb(filled),
        mb(all.saturating_sub(filled)),
        ROUND_US.swap(0, Relaxed),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    /// A moment `frame` frames and `tenths` tenths of a second in.
    fn at(frame: u64, tenths: u64) -> u64 {
        (frame << 32) | tenths
    }

    fn copy(bytes: u64, used: u64) -> Facts {
        Facts { filled: true, made: true, held: true, bytes, used, ..Facts::default() }
    }

    const NOW: u64 = (1000 << 32) | 10_000;
    const WIDE: Caps = Caps { bytes: 1 << 40, count: 1 << 20 };

    #[test]
    fn least_recently_used_go_first_until_the_bytes_are_reached() {
        let all = [(1, copy(10 * MB, at(500, 9_000))), (2, copy(10 * MB, at(100, 1_000))), (3, copy(10 * MB, at(300, 5_000)))];
        assert_eq!(choose(&all, 15 * MB, NOW, WIDE), vec![2, 3]);
        assert_eq!(choose(&all, 10 * MB, NOW, WIDE), vec![2]);
        assert_eq!(choose(&all, 1, NOW, WIDE), vec![2]);
        assert_eq!(choose(&all, 100 * MB, NOW, WIDE), vec![2, 3, 1]);
        assert!(choose(&all, 0, NOW, WIDE).is_empty());
    }

    #[test]
    fn copies_made_again_go_last() {
        let mut again = copy(10 * MB, at(100, 1_000));
        again.remade = true;
        let all = [(1, again), (2, copy(10 * MB, at(900, 9_000)))];
        assert_eq!(choose(&all, 20 * MB, NOW, WIDE), vec![2, 1]);
    }

    #[test]
    fn copies_in_use_or_lately_asked_for_are_never_picked() {
        let old = at(100, 1_000);
        let listed = Facts { listed: true, ..copy(MB, old) };
        let unheld = Facts { held: false, ..copy(MB, old) };
        let pending = Facts { pending: true, ..copy(MB, old) };
        let target = Facts { filled: false, ..copy(MB, old) };
        let none = Facts { made: false, ..copy(MB, old) };
        let never = copy(MB, 0);
        // Asked for 2 frames ago (time long past): bound in the last few frames.
        let recent_frames = copy(MB, at(998, 1_000));
        // 900 frames ago but only a second ago.
        let recent_time = copy(MB, at(100, 9_990));
        let all = [listed, unheld, pending, target, none, never, recent_frames, recent_time].map(|f| (0, f));
        assert!(choose(&all, 1 << 40, NOW, WIDE).is_empty());
        assert!(eligible(&copy(MB, old), NOW));
        assert!(eligible(&copy(MB, at(1000 - MIN_IDLE_FRAMES, 10_000 - MIN_IDLE_MS / 100)), NOW));
        assert!(!eligible(&copy(MB, at(1001 - MIN_IDLE_FRAMES, 1_000)), NOW));
    }

    #[test]
    fn the_caps_bound_a_round() {
        let all: Vec<(u32, Facts)> = (0..10).map(|i| (i, copy(40 * MB, at(u64::from(i), 1)))).collect();
        let by_count = Caps { bytes: 1 << 40, count: 3 };
        assert_eq!(choose(&all, 1 << 40, NOW, by_count), vec![0, 1, 2]);
        let by_bytes = Caps { bytes: 100 * MB, count: 100 };
        assert_eq!(choose(&all, 1 << 40, NOW, by_bytes), vec![0, 1]);
        // A copy too big for what is left of the cap is passed over for a
        // smaller one.
        let mixed = [(1, copy(90 * MB, at(1, 1))), (2, copy(20 * MB, at(2, 1))), (3, copy(5 * MB, at(3, 1)))];
        assert_eq!(choose(&mixed, 1 << 40, NOW, by_bytes), vec![1, 3]);
        assert!(choose(&mixed, 1 << 40, NOW, Caps { bytes: 1 << 40, count: 0 }).is_empty());
    }

    #[test]
    fn the_bytes_wanted_are_those_over_the_line_less_the_picked() {
        // Budget 6208, headroom 768: line 5440.
        assert_eq!(want(5837 * MB, 6208 * MB, 768 * MB, 0), 397 * MB);
        assert_eq!(want(5837 * MB, 6208 * MB, 768 * MB, 300 * MB), 97 * MB);
        assert_eq!(want(5837 * MB, 6208 * MB, 768 * MB, 500 * MB), 0);
        assert_eq!(want(5000 * MB, 6208 * MB, 768 * MB, 0), 0);
        assert_eq!(want(5000 * MB, 0, 768 * MB, 0), 5000 * MB);
    }

    #[test]
    fn slots_are_made_null_only_if_all_are_free_title_slots() {
        let never = |_| false;
        assert!(slots_free(&[3, 9], 100, never));
        assert!(!slots_free(&[], 100, never));
        // A list's copy of a table past the title's slots.
        assert!(!slots_free(&[3, 150], 100, never));
        // One a draw read through a table lately.
        assert!(!slots_free(&[3, 9], 100, |s| s == 9));
    }

    #[test]
    fn the_caps_read_megabytes_and_a_count() {
        assert_eq!(caps_from(None, None), Caps { bytes: 128 * MB, count: 256 });
        assert_eq!(caps_from(Some(" 64 "), Some("16")), Caps { bytes: 64 * MB, count: 16 });
        assert_eq!(caps_from(Some("99999"), Some("0")), Caps { bytes: 1024 * MB, count: 1 });
        assert_eq!(caps_from(Some("x"), Some("y")), Caps { bytes: 128 * MB, count: 256 });
    }
}
