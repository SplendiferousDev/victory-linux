//! Which view heap slots shaders read (`VICTORY_SLOT_FEEDBACK=1`; off by
//! default; not to be flipped in a run: the translations differ).
//!
//! Shaders take most of the title's textures from view heap slots they pick
//! by an index they compute; nothing on the CPU says which. With the switch
//! the translated programs store 1 into dword `index` of a buffer at the
//! slot below the null slot of every view heap whenever they take a texture
//! from a slot the host cannot know (`victory_shader::emit::slot_mark`).
//! Here: that buffer, its view in each view heap ([`install`]), and every
//! `VICTORY_SLOT_FEEDBACK_EVERY` frames (4; at least once a second) a list
//! of our own on the direct queue that copies it down and clears it
//! ([`tick`], ahead of a submission). A copy the card has run is read
//! ([`Seen::land`]): each slot found marked gets the frame the copy was
//! recorded in as the last frame a shader read it. A mark stored between
//! our copy and our clear is lost; a slot read in more than one frame is
//! seen all the same.
//!
//! Texture eviction (`crate::resource::evict`) uses it while the marks
//! cover every read (`VICTORY_SLOT_FEEDBACK_PIXEL` unset or `all`) and have
//! been watched for the whole idle time `VICTORY_SLOT_FEEDBACK_IDLE_S`
//! (45 s) with a copy landed in the last seconds ([`certain`]): a host copy
//! none of whose slots was read in that time, and that is that old itself,
//! may go although all its memory is mapped, and one with a slot read in
//! that time is never picked. Its slots stay null until a shader is seen
//! reading one (or the title touches it, as before), and are then made at
//! once. While the marks do not cover every read they only protect: a slot
//! marked within the idle time is never made null.
//! `VICTORY_SLOT_FEEDBACK_EVICT=0`: the marks are read and counted, and
//! eviction goes on by its own rules (for measuring the marks alone).
//!
//! Pixel programs with marks get the early depth and stencil test where
//! that is exact ([`early_wanted`]), so that a mark does not make the card
//! shade pixels the depth test throws away.
//!
//! One line every ten seconds says what the copies found ([`line`]).
//!
//! The idea (shaders note the heap slots they read; a texture none of
//! whose slots was read for a time may go, and its slots wait for a read)
//! is from skornuta's memory stack for VIctory (7c274de, 9c0c61b, 876456c,
//! 79dc3be); written anew for this tree.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;

/// Whether shaders store slot marks (the translator's own reading of the
/// switch: what it translates is what runs).
pub(crate) fn on() -> bool {
    victory_shader::emit::slot_mark::slot_feedback()
}

/// Whether the marks cover every read of a slot the host cannot know.
pub(crate) fn complete() -> bool {
    victory_shader::emit::slot_mark::slot_feedback_pixel() == victory_shader::emit::slot_mark::Pixel::All
}

fn number(name: &str, usual: u64, low: u64, high: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(usual).clamp(low, high)
}

/// Frames between copies (`VICTORY_SLOT_FEEDBACK_EVERY`, 4; 1 to 64).
fn every() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| number("VICTORY_SLOT_FEEDBACK_EVERY", 4, 1, 64))
}

/// How long no slot of a texture may have been read before it may go by
/// the marks (`VICTORY_SLOT_FEEDBACK_IDLE_S`, 45 s; 5 to 600), in
/// milliseconds.
pub(crate) fn idle_ms() -> u64 {
    static S: OnceLock<u64> = OnceLock::new();
    *S.get_or_init(|| number("VICTORY_SLOT_FEEDBACK_IDLE_S", 45, 5, 600)) * 1000
}

/// Whether texture eviction uses the marks (`VICTORY_SLOT_FEEDBACK_EVICT`
/// not `0`).
pub(crate) fn evicts() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| on() && std::env::var_os("VICTORY_SLOT_FEEDBACK_EVICT").is_none_or(|v| v != "0"))
}

/// A copy older than this says nothing of now: the marks are not relied on
/// until the next one lands.
pub(crate) const STALE_MS: u64 = 5_000;

/// What the copies found, per view heap slot.
pub(crate) struct Seen {
    /// The frame (plus one) of the copy that last found the slot marked;
    /// 0: never.
    last: Vec<AtomicU32>,
}

impl Seen {
    pub(crate) fn new(slots: u32) -> Seen {
        Seen { last: (0..slots).map(|_| AtomicU32::new(0)).collect() }
    }

    /// A copy recorded in `frame` holds `words` (a dword per slot, not 0:
    /// marked). Each marked slot gets the frame; `each` is told the ones
    /// under `title` (the title's own slots). Returns the marked slots, and
    /// those of them under `title`.
    pub(crate) fn land(&self, words: &[u32], frame: u64, title: u32, mut each: impl FnMut(u32)) -> (u64, u64) {
        let stamp = (frame + 1).min(u64::from(u32::MAX)) as u32;
        let (mut found, mut titles) = (0u64, 0u64);
        // Eight slots at a look: most are not marked.
        for (chunk, eight) in words.chunks(8).enumerate() {
            if eight.iter().all(|&w| w == 0) {
                continue;
            }
            for (k, _) in eight.iter().enumerate().filter(|(_, w)| **w != 0) {
                let slot = (chunk * 8 + k) as u32;
                found += 1;
                if let Some(last) = self.last.get(slot as usize) {
                    last.store(stamp, Relaxed);
                }
                if slot < title {
                    titles += 1;
                    each(slot);
                }
            }
        }
        (found, titles)
    }

    /// The frame (plus one) of the copy that last found `slot` marked.
    pub(crate) fn stamp(&self, slot: u32) -> u64 {
        self.last.get(slot as usize).map_or(0, |s| u64::from(s.load(Relaxed)))
    }

    /// Whether a copy recorded in frame `cutoff` or later found `slot`
    /// marked (such a copy may hold marks of a few frames before it: a
    /// slot is at most taken for read later than it was).
    pub(crate) fn read_since(&self, slot: u32, cutoff: u64) -> bool {
        self.stamp(slot) > cutoff
    }

    /// The slots under `title` read since `cutoff` ([`Seen::read_since`]).
    pub(crate) fn read_slots(&self, cutoff: u64, title: u32) -> Vec<u32> {
        let cut = cutoff.min(u64::from(u32::MAX)) as u32;
        self.last
            .iter()
            .take(title as usize)
            .enumerate()
            .filter(|(_, s)| s.load(Relaxed) > cut)
            .map(|(i, _)| i as u32)
            .collect()
    }
}

/// Frames by time: which frame it was some milliseconds ago.
#[derive(Default)]
pub(crate) struct Ring(VecDeque<(u64, u64)>);

impl Ring {
    /// Frame `frame` is under way at `ms`: a point every quarter of a
    /// second, a quarter of an hour of them.
    pub(crate) fn note(&mut self, frame: u64, ms: u64) {
        if self.0.back().is_some_and(|&(_, last)| ms < last + 250) {
            return;
        }
        self.0.push_back((frame, ms));
        while self.0.len() > 3_600 {
            self.0.pop_front();
        }
    }

    /// The frame under way at `ms`: that of the last point at or before
    /// it; `None` if the points do not reach back that far.
    pub(crate) fn frame_at(&self, ms: u64) -> Option<u64> {
        let after = self.0.partition_point(|&(_, at)| at <= ms);
        after.checked_sub(1).map(|i| self.0[i].0)
    }
}

/// Whether the marks say of the whole idle time, at `now`, which slots were
/// read: watched since `from` (ms; `None`: not yet) for at least `idle`,
/// and a copy recorded at `landed` (ms) not more than [`STALE_MS`] ago.
pub(crate) fn relied_on(from: Option<u64>, landed: Option<u64>, now: u64, idle: u64) -> bool {
    from.is_some_and(|f| f + idle <= now) && landed.is_some_and(|l| now.saturating_sub(l) <= STALE_MS)
}

/// Whether a pixel program with the slot mark bits `bits`
/// (`victory_shader::emit::slot_mark`) is to test depth and stencil before
/// it runs in a pipeline that writes depth or stencil if `writes` and
/// derives coverage from alpha if `alpha_to_coverage`: only a program that
/// marks and may ([`EARLY_OK`]), and only where nothing it does can change
/// what the test writes. A program that discards keeps a pixel's depth and
/// stencil from being written, so it tests early only where neither is
/// written at all; so does coverage from alpha.
///
/// [`EARLY_OK`]: victory_shader::emit::slot_mark::EARLY_OK
pub(crate) fn early_wanted(bits: u8, writes: bool, alpha_to_coverage: bool) -> bool {
    use victory_shader::emit::slot_mark::{DISCARDS, EARLY_OK, MARKS};
    bits & MARKS != 0 && bits & EARLY_OK != 0 && (!writes || (bits & DISCARDS == 0 && !alpha_to_coverage))
}

/// Whether a slot made null by eviction in frame stamp `evicted` is to be
/// made again because a copy recorded since found it marked (`marked`, the
/// copy's frame plus one).
pub(crate) fn read_again(evicted: u64, marked: u64) -> bool {
    marked > evicted
}

static SEEN: OnceLock<Seen> = OnceLock::new();
/// When the buffer was made (ms, plus one; 0: not), and when the last copy
/// landed was recorded (ms, plus one).
static FROM: AtomicU64 = AtomicU64::new(0);
static LANDED: AtomicU64 = AtomicU64::new(0);
/// The most title slots of a view heap with the buffer's view.
static TITLE: AtomicU32 = AtomicU32::new(0);
/// Since the start: copies landed; slots found marked in them (summed);
/// of those title slots; slots made null found read and queued again;
/// pixel pipelines made with the early test, and without it though their
/// program marks (it discards, or coverage comes from alpha, and depth or
/// stencil is written); copies recorded; copies left out for want of a
/// free read-back buffer.
pub(crate) static STATS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// The frame the idle time back, if the marks are relied on now for
/// texture eviction: a title slot no copy since found marked was not read
/// by any shader that marks.
#[cfg(windows)]
pub(crate) fn certain() -> Option<u64> {
    if !evicts() || !complete() {
        return None;
    }
    cutoff().filter(|_| {
        let ms = |a: &AtomicU64| a.load(Relaxed).checked_sub(1);
        relied_on(ms(&FROM), ms(&LANDED), crate::host::now_ms(), idle_ms())
    })
}

/// The frame the idle time back, if the frames noted reach that far.
#[cfg(windows)]
pub(crate) fn cutoff() -> Option<u64> {
    let ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    ring.frame_at(crate::host::now_ms().checked_sub(idle_ms())?)
}

/// Whether eviction leaves title slot `slot` alone for its mark: read
/// since frame `since` ([`cutoff`]; `None`, while the run is younger than
/// the idle time: ever).
#[cfg(windows)]
pub(crate) fn protects(slot: u32, since: Option<u64>) -> bool {
    evicts() && SEEN.get().is_some_and(|s| s.read_since(slot, since.unwrap_or(0)))
}

/// For eviction's choice: the host textures (by address) shown by a title
/// slot a shader read within the idle time; `None` while the marks are not
/// used.
#[cfg(windows)]
pub(crate) fn textures_read() -> Option<std::collections::HashSet<usize>> {
    (evicts() && SEEN.get().is_some()).then(|| crate::descriptor::textures_shown(&read_slots(cutoff().unwrap_or(0))))
}

/// The frame (plus one) of the copy that last found `slot` marked (0:
/// never, or the marks are not used).
#[cfg(windows)]
pub(crate) fn stamp(slot: u32) -> u64 {
    if !evicts() {
        return 0;
    }
    SEEN.get().map_or(0, |s| s.stamp(slot))
}

/// The title slots read since `cutoff`.
#[cfg(windows)]
pub(crate) fn read_slots(cutoff: u64) -> Vec<u32> {
    SEEN.get().map_or_else(Vec::new, |s| s.read_slots(cutoff, TITLE.load(Relaxed)))
}

#[cfg(windows)]
static RING: std::sync::Mutex<Ring> = std::sync::Mutex::new(Ring(VecDeque::new()));

#[cfg(windows)]
mod card {
    use super::*;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS;
    use windows::core::Interface;

    /// A read-back buffer with the list that copies into it.
    struct Back {
        buffer: ID3D12Resource,
        data: usize,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        /// Our fence's value behind its copy; 0: free.
        sent: u64,
        /// The frame and the time the copy was recorded at.
        at: (u64, u64),
    }

    struct State {
        buffer: ID3D12Resource,
        zeros: ID3D12Resource,
        /// Dwords of the buffer: a slot each, up to and with the null slot.
        slots: u32,
        backs: Vec<Back>,
        fence: ID3D12Fence,
        sent: u64,
        /// The frame and the time of the last copy recorded.
        last: (u64, u64),
        /// The device went: nothing more is read.
        dead: bool,
    }

    // SAFETY: D3D12 objects (free-threaded) and persistent mappings, used
    // under the lock.
    unsafe impl Send for State {}

    static STATE: std::sync::Mutex<Option<State>> = std::sync::Mutex::new(None);

    fn make(host: &crate::host::Host, slots: u32) -> Option<State> {
        let bytes = u64::from(slots) * 4;
        let buffer = host.uav_buffer(bytes)?;
        crate::host::name(&buffer, || "slot marks".into());
        let (zeros, data) = host.upload_buffer(bytes)?;
        // SAFETY: the upload buffer's mapping, `bytes` long.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
        crate::host::name(&zeros, || "slot marks, zeros".into());
        let mut backs = Vec::new();
        for _ in 0..2 {
            let back = host.read_back_buffer(bytes)?;
            crate::host::name(&back, || "slot marks read back".into());
            let mut data = std::ptr::null_mut();
            // SAFETY: plain creation; the mapping of a read-back buffer
            // stays for its life.
            let made = unsafe {
                (|| -> windows::core::Result<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
                    back.Map(0, None, Some(&mut data))?;
                    let allocator: ID3D12CommandAllocator = host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                    let list: ID3D12GraphicsCommandList =
                        host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                    list.Close()?;
                    Ok((allocator, list))
                })()
            };
            let (allocator, list) = made.inspect_err(|e| tracing::warn!(error = %e, "slot marks: no list for the copy")).ok()?;
            backs.push(Back { buffer: back, data: data as usize, allocator, list, sent: 0, at: (0, 0) });
        }
        // SAFETY: plain creation.
        let fence: ID3D12Fence = unsafe { host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.ok()?;
        Some(State { buffer, zeros, slots, backs, fence, sent: 0, last: (0, 0), dead: false })
    }

    /// For a view heap just made null with `title` title slots: writes the
    /// buffer's view (a raw UAV over all of it) into its slot `slot` at
    /// `at`. Whether it did.
    pub(crate) fn install(at: D3D12_CPU_DESCRIPTOR_HANDLE, slot: u32, title: u32) -> bool {
        let Some(host) = crate::host::host() else {
            return false;
        };
        // A dword per slot up to and with the null slot above this one.
        let slots = slot + 2;
        let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = make(host, slots);
            if guard.is_some() {
                let _ = SEEN.set(Seen::new(slots));
                FROM.store(crate::host::now_ms() + 1, Relaxed);
                tracing::info!(
                    "VICTORY_SLOT_FEEDBACK=1: shaders mark the view heap slots they take textures from ({} slots, the marks at slot \
                     {slot}, copied down every {} frames); {}",
                    slots,
                    every(),
                    match (evicts(), complete()) {
                        (false, _) => "texture eviction does not use them (VICTORY_SLOT_FEEDBACK_EVICT=0)".to_owned(),
                        (true, true) => format!(
                            "texture eviction lets go what no shader read for {} s and keeps what one read",
                            idle_ms() / 1000
                        ),
                        (true, false) =>
                            "not every pixel program marks (VICTORY_SLOT_FEEDBACK_PIXEL): texture eviction only keeps what a mark shows read".to_owned(),
                    }
                );
            }
        }
        let Some(state) = guard.as_ref().filter(|s| s.slots == slots) else {
            return false;
        };
        TITLE.fetch_max(title.min(slot), Relaxed);
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV { NumElements: slots, Flags: D3D12_BUFFER_UAV_FLAG_RAW, ..Default::default() },
            },
        };
        // SAFETY: a slot of a live view heap; a live buffer of `slots` dwords.
        unsafe { host.device.CreateUnorderedAccessView(&state.buffer, None, Some(&desc), at) };
        true
    }

    /// Ahead of a submission, on the thread that submits: reads the copies
    /// the card has run and, when one is due, records the next and sends it
    /// to the direct queue, where it runs between the title's submissions.
    pub(crate) fn tick() {
        if !on() {
            return;
        }
        let Some(host) = crate::host::host() else { return };
        let (frame, now) = (crate::frame::current(), crate::host::now_ms());
        RING.lock().unwrap_or_else(|e| e.into_inner()).note(frame, now);
        let mut read: Vec<u32> = Vec::new();
        {
            // (Another thread at it: its turn is as good.)
            let Ok(mut guard) = STATE.try_lock() else { return };
            let Some(state) = guard.as_mut().filter(|s| !s.dead) else { return };
            // SAFETY: a live fence.
            let done = unsafe { state.fence.GetCompletedValue() };
            if done == u64::MAX {
                state.dead = true;
                return;
            }
            let Some(seen) = SEEN.get() else { return };
            let title = TITLE.load(Relaxed);
            let slots = state.slots as usize;
            for back in state.backs.iter_mut().filter(|b| b.sent != 0 && b.sent <= done) {
                // SAFETY: the read-back buffer's mapping, `slots` dwords,
                // written by a copy the card has run.
                let words = unsafe { std::slice::from_raw_parts(back.data as *const u32, slots) };
                let (found, titles) = seen.land(words, back.at.0, title, |slot| read.push(slot));
                STATS[0].fetch_add(1, Relaxed);
                STATS[1].fetch_add(found, Relaxed);
                STATS[2].fetch_add(titles, Relaxed);
                LANDED.fetch_max(back.at.1 + 1, Relaxed);
                back.sent = 0;
            }
            let due = state.last.1 == 0 || frame >= state.last.0 + every() || now >= state.last.1 + 1000;
            if due {
                match state.backs.iter().position(|b| b.sent == 0) {
                    Some(at) => {
                        if let Err(e) = send(host, state, at, (frame, now)) {
                            tracing::warn!(error = %e, "slot marks: the copy could not be recorded or sent");
                            state.dead = true;
                        }
                    }
                    None => {
                        STATS[7].fetch_add(1, Relaxed);
                    }
                }
            }
        }
        // Slots made null whose texture went unread, read now: due again.
        if !read.is_empty() && evicts() && crate::resource::evict::any_null() {
            let n = crate::descriptor::slots_read_again(&read, frame);
            STATS[3].fetch_add(n, Relaxed);
        }
        line(now);
    }

    /// Records the copy into read-back buffer `at` and the clear, and sends
    /// them. The buffer rests in the common state between submissions (a
    /// buffer goes back to it after every call of the queue), which is what
    /// the title's shaders take it from as a UAV.
    fn send(host: &crate::host::Host, state: &mut State, at: usize, when: (u64, u64)) -> windows::core::Result<()> {
        let bytes = u64::from(state.slots) * 4;
        let back = &state.backs[at];
        let t = crate::host::transition;
        let (common, source, dest) = (D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
        // SAFETY: the allocator's last list has run (`sent` 0); live
        // buffers of `bytes` bytes each.
        unsafe {
            back.allocator.Reset()?;
            back.list.Reset(&back.allocator, None)?;
            back.list.ResourceBarrier(&[t(&state.buffer, common, source)]);
            back.list.CopyBufferRegion(&back.buffer, 0, &state.buffer, 0, bytes);
            back.list.ResourceBarrier(&[t(&state.buffer, source, dest)]);
            back.list.CopyBufferRegion(&state.buffer, 0, &state.zeros, 0, bytes);
            back.list.ResourceBarrier(&[t(&state.buffer, dest, common)]);
            back.list.Close()?;
        }
        let lists = [Some(back.list.cast::<ID3D12CommandList>()?)];
        crate::hostq::execute(host.queue(), &lists);
        state.sent += 1;
        crate::hostq::signal(host.queue(), &state.fence, state.sent)?;
        state.backs[at].sent = state.sent;
        state.backs[at].at = when;
        state.last = when;
        STATS[6].fetch_add(1, Relaxed);
        Ok(())
    }

    /// One line per ten seconds: what the copies found since the last.
    fn line(now: u64) {
        static LAST: AtomicU64 = AtomicU64::new(0);
        static BEFORE: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
        let last = LAST.load(Relaxed);
        if (last != 0 && now < last + 10_000) || LAST.compare_exchange(last, now.max(1), Relaxed, Relaxed).is_err() {
            return;
        }
        let s: Vec<u64> = STATS.iter().zip(&BEFORE).map(|(n, b)| {
            let n = n.load(Relaxed);
            n - b.swap(n, Relaxed)
        }).collect();
        let secs = (now.saturating_sub(last).max(1)) as f64 / 1000.0;
        let copies = s[0].max(1);
        let lately = cutoff().map(|c| read_slots(c).len());
        let e = crate::resource::evict::by_marks();
        tracing::info!(
            "slot marks (VICTORY_SLOT_FEEDBACK=1): since the last line {} copies read ({} sent, {} left out for want of a buffer), {} slots \
             marked per copy ({:.0} per second), of them {} title slots per copy; title slots read within the last {} s: {}; texture \
             eviction {}: {} copies kept for a slot a shader read, {} picked unread by the marks, {} of those let go {} MB, {} made \
             again; {} slots made null found read and due again, {} title slots made null now; pixel pipelines made with the early \
             depth test {} and without it though their program marks {} (since the start {} and {})",
            s[0],
            s[6],
            s[7],
            s[1] / copies,
            s[1] as f64 / secs,
            s[2] / copies,
            idle_ms() / 1000,
            lately.map_or_else(|| "not known yet".to_owned(), |n| n.to_string()),
            match (evicts(), certain().is_some()) {
                (false, _) => "does not use the marks",
                (true, true) => "goes by the marks",
                (true, false) if complete() => "is only kept from read slots (the marks do not cover the idle time yet)",
                (true, false) => "is only kept from read slots (not every pixel program marks)",
            },
            e[0],
            e[1],
            e[2],
            e[3] >> 20,
            e[4],
            s[3],
            crate::resource::evict::null_now(),
            s[4],
            s[5],
            STATS[4].load(Relaxed),
            STATS[5].load(Relaxed),
        );
    }
}

#[cfg(windows)]
pub(crate) use card::{install, tick};

#[cfg(test)]
mod tests {
    use super::*;
    use victory_shader::emit::slot_mark::{DISCARDS, EARLY_OK, MARKS};

    /// A copy's dwords with the slots `marked` set.
    fn copy(slots: usize, marked: &[usize]) -> Vec<u32> {
        let mut words = vec![0u32; slots];
        for &m in marked {
            words[m] = 1;
        }
        words
    }

    #[test]
    fn a_copy_stamps_the_slots_it_found_marked() {
        let seen = Seen::new(40);
        let mut told = Vec::new();
        // Title slots 0 to 29; 30 on are tables' copies and the null slot.
        let found = seen.land(&copy(40, &[0, 7, 8, 29, 30, 39]), 100, 30, |s| told.push(s));
        assert_eq!(found, (6, 4));
        assert_eq!(told, [0, 7, 8, 29]);
        assert_eq!(seen.stamp(7), 101);
        assert_eq!(seen.stamp(1), 0, "never marked");
        assert_eq!(seen.stamp(1_000), 0, "past the heap");
        // A later copy without slot 7 leaves its frame as it was.
        seen.land(&copy(40, &[8]), 104, 30, |_| {});
        assert_eq!((seen.stamp(7), seen.stamp(8)), (101, 105));
        // Read since frame 100 (the copy's own frame), not since 101.
        assert!(seen.read_since(7, 100) && !seen.read_since(7, 101));
        assert!(seen.read_since(8, 104) && !seen.read_since(1, 0));
        assert_eq!(seen.read_slots(0, 30), [0, 7, 8, 29]);
        assert_eq!(seen.read_slots(101, 30), [8]);
        assert_eq!(seen.read_slots(0, 8), [0, 7], "only the title's slots");
        // A copy shorter or longer than the heap does no harm.
        assert_eq!(seen.land(&copy(50, &[45]), 105, 30, |_| {}), (1, 0));
    }

    #[test]
    fn a_mark_lost_to_one_clear_is_seen_by_the_next_copy() {
        // Slot 3 is read every frame; the copy of frame 104 lost its mark
        // (stored between the copy and the clear of frame 100).
        let seen = Seen::new(8);
        for (frame, marked) in [(100, &[3][..]), (104, &[]), (108, &[3]), (112, &[3])] {
            seen.land(&copy(8, marked), frame, 8, |_| {});
        }
        // Whatever the idle time, as long as it spans more than one copy:
        assert!(seen.read_since(3, 105));
        // and a slot read only at frame 100 is unread since 101.
        seen.land(&copy(8, &[5]), 100, 8, |_| {});
        assert!(!seen.read_since(5, 101));
    }

    #[test]
    fn the_frame_some_time_ago_is_the_last_noted_by_then() {
        let mut ring = Ring::default();
        assert_eq!(ring.frame_at(1_000), None);
        for (frame, ms) in [(10, 1_000), (11, 1_100), (13, 1_300), (20, 2_000)] {
            ring.note(frame, ms);
        }
        // (The point at 1,100 came too soon after the first.)
        assert_eq!(ring.0.len(), 3);
        assert_eq!(ring.frame_at(999), None, "before the first point");
        assert_eq!(ring.frame_at(1_000), Some(10));
        assert_eq!(ring.frame_at(1_299), Some(10));
        assert_eq!(ring.frame_at(1_300), Some(13));
        assert_eq!(ring.frame_at(9_000), Some(20));
    }

    #[test]
    fn the_marks_are_relied_on_only_watched_long_enough_and_fresh() {
        let idle = 45_000;
        assert!(!relied_on(None, None, 100_000, idle), "no buffer yet");
        assert!(!relied_on(Some(1_000), None, 100_000, idle), "no copy landed");
        assert!(!relied_on(Some(60_000), Some(99_000), 100_000, idle), "watched 40 s only");
        assert!(relied_on(Some(55_000), Some(99_000), 100_000, idle));
        assert!(relied_on(Some(1_000), Some(95_000), 100_000, idle));
        assert!(!relied_on(Some(1_000), Some(94_999), 100_000, idle), "the last copy is too old");
    }

    #[test]
    fn a_pixel_program_tests_early_only_where_that_changes_nothing() {
        let plain = MARKS | EARLY_OK;
        let discards = plain | DISCARDS;
        // No discard: wherever coverage does not come from alpha while
        // depth or stencil is written.
        assert!(early_wanted(plain, true, false));
        assert!(early_wanted(plain, false, false));
        assert!(!early_wanted(plain, true, true));
        assert!(early_wanted(plain, false, true));
        // A discard: only where neither depth nor stencil is written.
        assert!(!early_wanted(discards, true, false));
        assert!(early_wanted(discards, false, false));
        assert!(early_wanted(discards, false, true));
        // Never a program without marks, or one that may not.
        assert!(!early_wanted(EARLY_OK, false, false));
        assert!(!early_wanted(MARKS, false, false));
        assert!(!early_wanted(MARKS | DISCARDS, false, false));
        assert!(!early_wanted(0, false, false));
    }

    #[test]
    fn a_slot_made_null_comes_back_when_a_later_copy_finds_it_read() {
        assert!(!read_again(500, 0), "never marked");
        assert!(!read_again(500, 500), "the copy of the frame before it went");
        assert!(read_again(500, 501));
    }
}
