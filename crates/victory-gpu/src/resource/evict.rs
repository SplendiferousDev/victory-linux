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
//! texture through a table makes it at once, as before. A slot made null
//! that the title touches again (writes a view into it, maps or writes the
//! texture's memory) or that a bound table covers is made again at once,
//! past the governor's hold, up to [`refills_per_frame`] a frame
//! (`HostHeap::texture_slot_refill`); the room it takes is paid for by
//! colder copies at the next rounds.
//!
//! What is seen of a texture shaders pick by index: the title's writes of
//! its slot, the runtime's mapping, unmapping and writes of its memory (the
//! title's streaming), tables that cover its slot, and whether all its
//! memory is mapped. A copy whose memory is all mapped is one the title's
//! streaming keeps whole; such copies are left alone unless
//! `VICTORY_TEXTURE_EVICT_WHOLE=1`. Not seen: which slots a shader that
//! picks by index reads (the index comes from the title's own data on the
//! card); only a mark written by the translated shaders would tell.
//!
//! Against a loop of letting go and making again: a copy let go is not
//! picked again for [`AGAIN_MS`], and once over the line rounds go on until
//! the card is `VICTORY_TEXTURE_EVICT_UNDER_MB` (128) under it, then stop
//! until it is over again.
//!
//! With slot marks (`crate::slotuse`, `VICTORY_SLOT_FEEDBACK=1`) the
//! translated shaders say which slots they read: a copy with a slot a
//! shader read within the marks' idle time is never picked, and its slots
//! are never made null. While the marks are relied on besides
//! (`crate::slotuse::certain`): a copy as old as that time none of whose
//! slots was read in it may go although all its memory is mapped
//! ([`Facts::unread`]), before the others; the slots of a copy let go leave
//! the queue and stay null until a shader is seen reading one, the title
//! touches it or a table covers it ([`went_unread`]), and are then made at
//! once like the others.
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
/// A copy let go is not picked again for this long.
pub(crate) const AGAIN_MS: u64 = 120_000;
/// A slot the title wrote, or whose memory was mapped or written, within
/// this many frames is not made null (`HostHeap::texture_slots_evicted`).
pub(crate) const TOUCH_FRAMES: u64 = 1_200;
/// Most title memory lookups (is all of a texture mapped?) a round makes.
pub(crate) const MOST_LOOKS: usize = 4_096;

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
    /// Let go by eviction less than [`AGAIN_MS`] ago.
    pub recent: bool,
    /// All its title memory is mapped (or not looked at).
    pub whole: bool,
    /// The slot marks say no shader read any of its slots for their whole
    /// idle time, and nothing asked for it in that time either.
    pub unread: bool,
}

/// Whether a copy may be picked at `now` (a `crate::host::moment`).
pub(crate) fn eligible(f: &Facts, now: u64) -> bool {
    f.filled
        && f.made
        && !f.listed
        && f.held
        && !f.pending
        && !f.recent
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
    /// Copies whose memory is all mapped may be picked too.
    pub whole: bool,
}

/// Of `all` (an id and its facts), the ids a round picks to free `want`
/// bytes: eligible ones ([`eligible`]; whole ones only with `caps.whole`
/// or unread by the slot marks), never made again before the others, the
/// ones the marks say unread first, partly mapped before whole, least
/// recently asked for first, until `want` or a cap is reached (the
/// copy that crosses `want` is taken; none past the byte cap).
pub(crate) fn choose<T: Copy>(all: &[(T, Facts)], want: u64, now: u64, caps: Caps) -> Vec<T> {
    if want == 0 || caps.count == 0 {
        return Vec::new();
    }
    let mut order: Vec<&(T, Facts)> =
        all.iter().filter(|(_, f)| eligible(f, now) && (caps.whole || !f.whole || f.unread)).collect();
    // By when asked for: the frame count (high half) first, then the time.
    order.sort_by_key(|(_, f)| (f.remade, !f.unread, f.whole, f.used >> 32, f.used & 0xffff_ffff));
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

/// The bytes to free and whether rounds go on: over the line (`budget`
/// less `headroom`, as the governor draws it) they start; once started
/// (`active`) they go on down to `under` below it; then stop. Less what was
/// picked and has not gone yet.
pub(crate) fn want(used: u64, budget: u64, headroom: u64, under: u64, active: bool, pending: u64) -> (u64, bool) {
    let line = budget.saturating_sub(headroom);
    let floor = line.saturating_sub(under);
    let active = used > line || (active && used > floor);
    if !active {
        return (0, false);
    }
    (used.saturating_sub(floor).saturating_sub(pending), true)
}

/// Whether a slot made null in frame stamp `evicted` (frame + 1) is to be
/// made again past the hold: the title touched it (`touched`, frame + 1 of
/// its last write or memory change; 0 never) or a table covered it
/// (`tabled`, likewise) since.
pub(crate) fn refill_wanted(evicted: u64, touched: u64, tabled: u64) -> bool {
    touched > evicted || tabled > evicted
}

/// A per-frame budget: whether one more may go in `frame` with `most` a
/// frame (`state`: the frame and how many went in it).
pub(crate) fn take_one(state: &mut (u64, u32), frame: u64, most: u32) -> bool {
    if state.0 != frame {
        *state = (frame, 0);
    }
    if state.1 >= most {
        return false;
    }
    state.1 += 1;
    true
}

/// Whether the slots that show a picked texture may all be made null: there
/// is at least one, each is a title slot (under `count`; a slot past it is a
/// list's copy of a table) and none was read through a table lately.
pub(crate) fn slots_free(slots: &[u32], count: u32, tabled_lately: impl Fn(u32) -> bool) -> bool {
    !slots.is_empty() && slots.iter().all(|&s| s < count && !tabled_lately(s))
}

/// The round's caps: `VICTORY_TEXTURE_EVICT_MB` (128, at most 1024) and
/// `VICTORY_TEXTURE_EVICT_COUNT` (256, at most 4096).
pub(crate) fn caps_from(mb: Option<&str>, count: Option<&str>, whole: Option<&str>) -> Caps {
    let n = |v: Option<&str>| v.and_then(|v| v.trim().parse::<u64>().ok());
    Caps {
        bytes: n(mb).map_or(128, |mb| mb.clamp(1, 1024)) << 20,
        count: n(count).map_or(256, |c| c.clamp(1, 4096)) as usize,
        whole: whole.is_some_and(|v| v.trim() == "1"),
    }
}

/// A number switch in MB (`usual` while unset), in bytes.
fn mb_var(name: &str, usual: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(usual).min(4096) << 20
}

/// Slots made null that may be made again past the hold in one frame:
/// `VICTORY_TEXTURE_EVICT_REFILLS` (32, at most 1024).
pub(crate) fn refills_per_frame() -> u32 {
    static N: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_EVICT_REFILLS").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(32).min(1024)
    })
}

/// Title slots made null by eviction, with the frame stamp (frame + 1) in
/// which they were, until made again past the hold.
#[cfg(windows)]
static EVICTED: std::sync::Mutex<std::collections::BTreeMap<u32, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());
#[cfg(windows)]
static EVICTED_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The heap made `slots` null in `frame`.
#[cfg(windows)]
pub(crate) fn note_evicted(slots: &[u32], frame: u64) {
    let mut e = EVICTED.lock().unwrap_or_else(|e| e.into_inner());
    for &s in slots {
        e.insert(s, frame + 1);
    }
    EVICTED_N.store(e.len(), Relaxed);
}

/// Slot marks: title slot `slot` stays null for a texture that went unread
/// at frame stamp `stamp` ([`went_unread`]), as if made null then.
#[cfg(windows)]
pub(crate) fn note_parked(slot: u32, stamp: u64) {
    let mut e = EVICTED.lock().unwrap_or_else(|e| e.into_inner());
    e.entry(slot).or_insert(stamp);
    EVICTED_N.store(e.len(), Relaxed);
    BY_MARKS[5].fetch_add(1, Relaxed);
}

/// Whether any title slot is null by eviction now, and how many.
#[cfg(windows)]
pub(crate) fn any_null() -> bool {
    EVICTED_N.load(Relaxed) != 0
}

/// See [`any_null`].
#[cfg(windows)]
pub(crate) fn null_now() -> usize {
    EVICTED_N.load(Relaxed)
}

/// Title textures (by address) whose host copy eviction let go while the
/// slot marks said no shader read it, with the frame stamp (frame + 1) at
/// which it was found gone; until the copy is made again.
#[cfg(windows)]
static UNREAD: std::sync::Mutex<Option<std::collections::HashMap<usize, u64>>> = std::sync::Mutex::new(None);
#[cfg(windows)]
static UNREAD_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Whether any texture's copy went unread and has not been made again.
#[cfg(windows)]
pub(crate) fn any_unread() -> bool {
    UNREAD_N.load(Relaxed) != 0
}

/// The frame stamp at which the copy of title texture `texture` went
/// unread, if it did and has not been made again.
#[cfg(windows)]
pub(crate) fn went_unread(texture: usize) -> Option<u64> {
    if UNREAD_N.load(Relaxed) == 0 {
        return None;
    }
    UNREAD.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|u| u.get(&texture).copied())
}

/// Since the start, by the slot marks: copies not picked for a slot a
/// shader read; copies picked as unread; of those let go, and their bytes;
/// of those made again; slots left null and out of the queue for a texture
/// that went unread.
#[cfg(windows)]
static BY_MARKS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// [`BY_MARKS`]'s first five since the last call (for `crate::slotuse`'s line).
#[cfg(windows)]
pub(crate) fn by_marks() -> [u64; 5] {
    static BEFORE: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
    std::array::from_fn(|i| {
        let n = BY_MARKS[i].load(Relaxed);
        n - BEFORE[i].swap(n, Relaxed)
    })
}

/// The frame stamp in which title slot `slot` was made null by eviction.
#[cfg(windows)]
pub(crate) fn evicted_stamp(slot: u32) -> Option<u64> {
    if EVICTED_N.load(Relaxed) == 0 {
        return None;
    }
    EVICTED.lock().unwrap_or_else(|e| e.into_inner()).get(&slot).copied()
}

/// Slot `slot`, made null by eviction, was touched since: made again now
/// if this frame's refills allow (counted either way).
#[cfg(windows)]
pub(crate) fn refill_take(slot: u32, frame: u64) -> bool {
    static BUDGET: std::sync::Mutex<(u64, u32)> = std::sync::Mutex::new((0, 0));
    STATS[11].fetch_add(1, Relaxed);
    if !take_one(&mut BUDGET.lock().unwrap_or_else(|e| e.into_inner()), frame, refills_per_frame()) {
        STATS[13].fetch_add(1, Relaxed);
        return false;
    }
    STATS[12].fetch_add(1, Relaxed);
    let mut e = EVICTED.lock().unwrap_or_else(|e| e.into_inner());
    e.remove(&slot);
    EVICTED_N.store(e.len(), Relaxed);
    true
}

/// Since the start: rounds that picked, copies picked and their bytes;
/// picked copies let go and their bytes; let go and made again (re-fills)
/// and their bytes; given up (not gone in time, or asked for again);
/// textures whose slots the heap made null, and those slots; textures it
/// left (a slot not ours to null).
#[cfg(windows)]
/// Then: slots made null found touched again (read while evicted), of them
/// made again past the hold, and those the frame's refills turned away;
/// copies left alone for all their memory being mapped, and for being let
/// go lately.
static STATS: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];

/// Whether rounds go on ([`want`]'s `active`).
#[cfg(windows)]
static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Copies let go by eviction, by title texture, and when (ms).
#[cfg(windows)]
static RECENT: std::sync::Mutex<Vec<(usize, u64)>> = std::sync::Mutex::new(Vec::new());

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
    /// Picked as unread by the slot marks ([`Facts::unread`]).
    unread: bool,
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
                if p.unread {
                    BY_MARKS[2].fetch_add(1, Relaxed);
                    BY_MARKS[3].fetch_add(p.bytes, Relaxed);
                    let mut went = UNREAD.lock().unwrap_or_else(|e| e.into_inner());
                    let map = went.get_or_insert_with(Default::default);
                    map.insert(p.at, crate::frame::current() + 1);
                    UNREAD_N.store(map.len(), Relaxed);
                }
                RECENT.lock().unwrap_or_else(|e| e.into_inner()).push((p.at, now_ms));
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
                if p.unread {
                    BY_MARKS[4].fetch_add(1, Relaxed);
                }
                return false;
            }
            true
        });
        // The textures that went unread: until made again, or gone.
        if UNREAD_N.load(Relaxed) != 0 {
            let unread = |f: &mut dyn FnMut(&mut std::collections::HashMap<usize, u64>)| {
                if let Some(map) = UNREAD.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    f(map);
                    UNREAD_N.store(map.len(), Relaxed);
                }
            };
            let mut keys = Vec::new();
            unread(&mut |map| keys = map.keys().copied().collect());
            // (Asked outside the list's lock.)
            keys.retain(|at| {
                !all.contains(at) || {
                    // SAFETY: as above.
                    let r = unsafe { &*(*at as *const super::Resource) };
                    crate::lockmeter::lock("rs:made", &r.host.made).is_some()
                }
            });
            unread(&mut |map| map.retain(|at, _| !keys.contains(at)));
        }
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
    static UNDER: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let under = *UNDER.get_or_init(|| mb_var("VICTORY_TEXTURE_EVICT_UNDER_MB", 128));
    let want = if crate::headroom::level() >= crate::headroom::TOP {
        let (want, active) = want(used, budget, headroom, under, ACTIVE.load(Relaxed), pending);
        ACTIVE.store(active, Relaxed);
        want
    } else {
        ACTIVE.store(false, Relaxed);
        0
    };
    // Slots and copies let go long ago are forgotten.
    {
        let mut e = EVICTED.lock().unwrap_or_else(|e| e.into_inner());
        let frame = crate::frame::current() + 1;
        e.retain(|_, &mut at| frame.saturating_sub(at) < 36_000);
        EVICTED_N.store(e.len(), Relaxed);
    }
    let recent: std::collections::HashSet<usize> = {
        let mut r = RECENT.lock().unwrap_or_else(|e| e.into_inner());
        r.retain(|&(_, when)| now_ms.saturating_sub(when) < AGAIN_MS);
        r.iter().map(|&(at, _)| at).collect()
    };
    if want > 0 {
        static CAPS: std::sync::OnceLock<Caps> = std::sync::OnceLock::new();
        let caps = *CAPS.get_or_init(|| {
            let var = |name| std::env::var(name).ok();
            caps_from(
                var("VICTORY_TEXTURE_EVICT_MB").as_deref(),
                var("VICTORY_TEXTURE_EVICT_COUNT").as_deref(),
                var("VICTORY_TEXTURE_EVICT_WHOLE").as_deref(),
            )
        });
        let pending_at: std::collections::HashSet<usize> =
            PICKED.lock().unwrap_or_else(|e| e.into_inner()).0.iter().map(|p| p.at).collect();
        // Slot marks (`crate::slotuse`): the host textures shown by a slot
        // a shader read within the marks' idle time are not picked, and,
        // while the marks are relied on, a copy as old as that time may go
        // as unread.
        let marks = crate::slotuse::certain();
        let read = crate::slotuse::textures_read();
        let marks_idle = crate::slotuse::idle_ms();
        let all = super::textures();
        let mut facts: Vec<(usize, Facts)> = Vec::new();
        // Where each copy's title memory is, to ask whether all is mapped.
        let mut memory: Vec<(u64, u64)> = Vec::new();
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
            if read.as_ref().is_some_and(|r| r.contains(&(windows::core::Interface::as_raw(&host.texture) as usize))) {
                BY_MARKS[0].fetch_add(1, Relaxed);
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
                recent: recent.contains(&at),
                whole: true,
                unread: marks.is_some() && idle_since(used, now, marks_idle, MIN_IDLE_FRAMES),
            };
            if f.recent && eligible(&Facts { recent: false, ..f }, now) {
                STATS[15].fetch_add(1, Relaxed);
            }
            facts.push((at, f));
            memory.push((r.data() as u64, super::footprint(&r.desc).map_or(0, |f| f.size)));
        }
        drop(all);
        // Whether all their memory is mapped: asked outside the table's
        // lock (an address is only asked about, never read), oldest first,
        // at most `MOST_LOOKS`; one not asked counts as whole.
        if !caps.whole {
            let mut order: Vec<usize> = (0..facts.len()).filter(|&i| eligible(&facts[i].1, now)).collect();
            order.sort_by_key(|&i| (facts[i].1.remade, facts[i].1.used >> 32, facts[i].1.used & 0xffff_ffff));
            for &i in order.iter().take(MOST_LOOKS) {
                let (at, len) = memory[i];
                let whole = len == 0 || crate::texture::mapped_bytes(at as *const u8, len as usize) >= len;
                facts[i].1.whole = whole;
                if whole {
                    STATS[14].fetch_add(1, Relaxed);
                }
            }
        }
        let chosen = choose(&facts, want, now, caps);
        let unread: std::collections::HashSet<usize> =
            facts.iter().filter(|(_, f)| f.unread).map(|&(at, _)| at).collect();
        let all = super::textures();
        if !chosen.is_empty() {
            let mut wanted = Vec::with_capacity(chosen.len());
            let mut picked = Vec::with_capacity(chosen.len());
            for &at in &chosen {
                if !all.contains(&at) {
                    continue;
                }
                // SAFETY: as above (in the table, under its lock).
                let r = unsafe { &*(at as *const super::Resource) };
                let made = crate::lockmeter::lock("rs:made", &r.host.made);
                let Some((host, _)) = made.as_ref() else { continue };
                wanted.push(Wanted(host.texture.clone()));
                let unread = unread.contains(&at);
                BY_MARKS[1].fetch_add(u64::from(unread), Relaxed);
                picked.push(Picked { at, bytes: host.bytes, when: now_ms, moment: now, unread });
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
    static BEFORE: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];
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
         textures (targets, written by the host) {} MB; {} slots made null read while evicted (touched again), {} of them made \
         again past the hold, {} turned away by the frame's refills, {} slots made null now; left alone: {} looks found all memory \
         mapped, {} let go lately; rounds {}; longest round {} us",
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
        s[11],
        s[12],
        s[13],
        EVICTED_N.load(Relaxed),
        s[14],
        s[15],
        if ACTIVE.load(Relaxed) { "going on" } else { "stopped" },
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
    const WIDE: Caps = Caps { bytes: 1 << 40, count: 1 << 20, whole: false };

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
        let by_count = Caps { bytes: 1 << 40, count: 3, whole: false };
        assert_eq!(choose(&all, 1 << 40, NOW, by_count), vec![0, 1, 2]);
        let by_bytes = Caps { bytes: 100 * MB, count: 100, whole: false };
        assert_eq!(choose(&all, 1 << 40, NOW, by_bytes), vec![0, 1]);
        // A copy too big for what is left of the cap is passed over for a
        // smaller one.
        let mixed = [(1, copy(90 * MB, at(1, 1))), (2, copy(20 * MB, at(2, 1))), (3, copy(5 * MB, at(3, 1)))];
        assert_eq!(choose(&mixed, 1 << 40, NOW, by_bytes), vec![1, 3]);
        assert!(choose(&mixed, 1 << 40, NOW, Caps { bytes: 1 << 40, count: 0, whole: false }).is_empty());
    }

    #[test]
    fn rounds_start_over_the_line_and_stop_some_way_under_it() {
        // Budget 6208, headroom 768: line 5440; under 128: floor 5312.
        let (b, h, u) = (6208 * MB, 768 * MB, 128 * MB);
        assert_eq!(want(5837 * MB, b, h, u, false, 0), (525 * MB, true));
        assert_eq!(want(5837 * MB, b, h, u, true, 300 * MB), (225 * MB, true));
        assert_eq!(want(5837 * MB, b, h, u, false, 600 * MB), (0, true));
        // Under the line, not started: nothing.
        assert_eq!(want(5400 * MB, b, h, u, false, 0), (0, false));
        // Under the line, started: on down to the floor.
        assert_eq!(want(5400 * MB, b, h, u, true, 0), (88 * MB, true));
        // At the floor: stopped, and not started again until over the line.
        assert_eq!(want(5312 * MB, b, h, u, true, 0), (0, false));
        assert_eq!(want(5430 * MB, b, h, u, false, 0), (0, false));
    }

    #[test]
    fn copies_let_go_lately_are_not_picked_again() {
        let recent = Facts { recent: true, ..copy(MB, at(100, 1_000)) };
        assert!(!eligible(&recent, NOW));
        assert!(choose(&[(1, recent)], 1 << 40, NOW, WIDE).is_empty());
    }

    #[test]
    fn copies_with_all_memory_mapped_are_left_unless_allowed_and_go_last() {
        let whole = Facts { whole: true, ..copy(10 * MB, at(100, 1_000)) };
        let part = copy(10 * MB, at(900, 9_000));
        let all = [(1, whole), (2, part)];
        assert_eq!(choose(&all, 1 << 40, NOW, WIDE), vec![2]);
        assert_eq!(choose(&all, 1 << 40, NOW, Caps { whole: true, ..WIDE }), vec![2, 1]);
    }

    #[test]
    fn copies_the_marks_say_unread_go_first_and_although_whole() {
        // Whole and unread by the marks: picked without the whole switch,
        // before a partly mapped copy asked for longer ago.
        let unread = Facts { whole: true, unread: true, ..copy(10 * MB, at(900, 9_000)) };
        let whole = Facts { whole: true, ..copy(10 * MB, at(100, 1_000)) };
        let part = copy(10 * MB, at(200, 2_000));
        let all = [(1, whole), (2, part), (3, unread)];
        assert_eq!(choose(&all, 1 << 40, NOW, WIDE), vec![3, 2]);
        assert_eq!(choose(&all, 1, NOW, WIDE), vec![3]);
        // One made again goes after the others all the same, and one in a
        // list or asked for lately not at all.
        let again = Facts { remade: true, ..unread };
        assert_eq!(choose(&[(1, again), (2, part)], 1 << 40, NOW, WIDE), vec![2, 1]);
        let listed = Facts { listed: true, ..unread };
        let lately = Facts { used: at(999, 9_999), ..unread };
        assert!(choose(&[(1, listed), (2, lately)], 1 << 40, NOW, WIDE).is_empty());
    }

    #[test]
    fn a_slot_made_null_comes_back_once_touched_or_tabled_since() {
        assert!(!refill_wanted(500, 0, 0));
        assert!(!refill_wanted(500, 499, 500));
        assert!(refill_wanted(500, 501, 0));
        assert!(refill_wanted(500, 0, 501));
    }

    #[test]
    fn refills_are_bounded_per_frame() {
        let mut state = (0, 0);
        assert!(take_one(&mut state, 7, 2));
        assert!(take_one(&mut state, 7, 2));
        assert!(!take_one(&mut state, 7, 2));
        assert!(take_one(&mut state, 8, 2));
        assert!(!take_one(&mut state, 9, 0));
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
        assert_eq!(caps_from(None, None, None), Caps { bytes: 128 * MB, count: 256, whole: false });
        assert_eq!(caps_from(Some(" 64 "), Some("16"), Some("1")), Caps { bytes: 64 * MB, count: 16, whole: true });
        assert_eq!(caps_from(Some("99999"), Some("0"), Some("0")), Caps { bytes: 1024 * MB, count: 1, whole: false });
        assert_eq!(caps_from(Some("x"), Some("y"), None), Caps { bytes: 128 * MB, count: 256, whole: false });
    }
}
