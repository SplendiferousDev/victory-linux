//! Checks of the merge on the card (`crate::shadow::merge_on_card`):
//!
//! - `VICTORY_MERGE_CAP` (off by default; `1` on, `alt` in turn): the
//!   title's bytes put in under flights' shader writes go through the
//!   merge on the card only as far as the cap of changed runs a submission
//!   (`VICTORY_MERGE_RUNS`) leaves, as the copies of the runs do without
//!   it; past the cap the flights are settled. With the cap the merge on
//!   the card is only another way up for the same bytes; without it it
//!   also takes the pages the cap used to settle for (hundreds of
//!   thousands of changed runs a frame where the copies take a few
//!   hundred). A run with and one without tell which of the two a fault
//!   with the merge on comes from.
//! - Torn words, counted whenever the merge on the card is wanted: when a
//!   flight lands (`Shadow::card_merge_relay`), the aligned words of 4, 8
//!   and 16 bytes that hold both a byte the merge takes from the title (a
//!   mask bit) and a byte a shader wrote on the card (the card's side
//!   differs from the CPU's there, outside the title's bytes still on
//!   their way up). Such a word ends up on the card part title, part
//!   shader: a value no one wrote whole, if the word is one value. The
//!   title's bytes looked at are those of merges sent after the landing
//!   flight's own send (they ran over its shader writes) and those still
//!   to go up: the notes of sends are kept until the flights before them
//!   have landed ([`crate::shadow::relay_landed`]).
//! - `VICTORY_MERGE_QSYNC` (off by default; `1` on, `alt` in turn): with
//!   fetches on a copy queue (`VICTORY_QUEUE2` bit 1) a fetch of the
//!   card's copies there never runs beside a merge on the card on the
//!   direct queue: a fetch waits for the last merge sent before it
//!   (`Second::order`'s value behind that merge's send), and the direct
//!   queue waits for the fetches sent before a merge goes to it.
//! - `VICTORY_MERGE_BARRIER` (off by default; `1` on, `alt` in turn): a
//!   barrier for every unordered access ahead of and behind each copy's
//!   merge, and the copy leaves its send list in the state the copies of
//!   the runs leave it in when nothing hashes it after the merge.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

static QSYNC: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_MERGE_QSYNC",
    "fetches on the copy queue and merges on the card never run beside each other",
);

static BARRIER: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_MERGE_BARRIER",
    "barriers for unordered access ahead of and behind each merge on the card",
);

/// Whether fetches on the copy queue are kept apart from merges now.
pub(crate) fn qsync_on() -> bool {
    QSYNC.on()
}

/// The number of the last send with a merge on the card (`Second::order`
/// reaches it behind that send's list), 0: none yet. Under [`ORDERING`].
static LAST_MERGE: AtomicU64 = AtomicU64::new(0);

/// Held from the reading of [`LAST_MERGE`] until the fetch that waits for
/// it is sent, and from its writing until the direct queue waits for the
/// fetches sent so far: no fetch is sent between, so each fetch is either
/// behind a merge or waited for by it.
static ORDERING: Mutex<()> = Mutex::new(());

/// The send numbered `number` has merges on the card and its list goes to
/// `direct` next: the direct queue waits for the fetches sent so far on
/// the copy queue first, and later fetches wait for this send.
pub(crate) fn before_merge_send(direct: &ID3D12CommandQueue, number: u64) -> windows::core::Result<()> {
    let Some(second) = crate::queue2::second().filter(|s| s.fetches()) else {
        return Ok(());
    };
    if number == 0 {
        return Ok(());
    }
    let _order = ORDERING.lock().unwrap_or_else(|e| e.into_inner());
    LAST_MERGE.fetch_max(number, Ordering::AcqRel);
    let job = second.down.last_sent();
    if job != 0 && !second.down.done(job) {
        QSYNC_STATS[0].fetch_add(1, Ordering::Relaxed);
        crate::hostq::wait(direct, second.down.fence(), job)?;
    }
    Ok(())
}

/// A fetch on the copy queue: `send` sends it with the waits it is given,
/// to which the last merge's send is added.
pub(crate) fn fetch_after_merges<T>(
    second: &crate::queue2::Second,
    send: impl FnOnce(Option<(&ID3D12Fence, u64)>) -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    let _order = ORDERING.lock().unwrap_or_else(|e| e.into_inner());
    let last = LAST_MERGE.load(Ordering::Acquire);
    if last != 0 && second.order_done() < last {
        QSYNC_STATS[1].fetch_add(1, Ordering::Relaxed);
    }
    send((last != 0).then(|| (second.order_fence(), last)))
}

/// Since the last frame-rate line: merges whose send waited for fetches
/// in the air; fetches that waited for a merge not run yet.
static QSYNC_STATS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// [`QSYNC_STATS`] since the last asking, per frame of `frames`.
pub(crate) fn qsync_taken(frames: f64) -> [f64; 2] {
    [0, 1].map(|i| QSYNC_STATS[i].swap(0, Ordering::Relaxed) as f64 / frames)
}

/// Whether the merge's barriers are recorded now.
pub(crate) fn barrier_on() -> bool {
    BARRIER.on()
}

/// A barrier for every unordered access.
fn uav_all() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    }
}

/// Ahead of a copy's merge, with [`barrier_on`].
///
/// # Safety
/// `list` is open.
pub(crate) unsafe fn ahead(list: &ID3D12GraphicsCommandList) {
    // SAFETY: an open list (the caller's).
    unsafe { list.ResourceBarrier(&[uav_all()]) };
}

/// Behind a copy's merge on `buffer`, with [`barrier_on`]: a barrier for
/// its writes and, if nothing hashes the copy behind it (`hashed`), the
/// state the copies of the runs leave it in.
///
/// # Safety
/// `list` is open and `buffer` is in the unordered-access state in it.
pub(crate) unsafe fn behind(list: &ID3D12GraphicsCommandList, buffer: &ID3D12Resource, hashed: bool) {
    // SAFETY: an open list (the caller's); a live buffer.
    unsafe {
        if hashed {
            list.ResourceBarrier(&[uav_all()]);
        } else {
            list.ResourceBarrier(&[
                uav_all(),
                crate::host::transition(buffer, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_DEST),
            ]);
        }
    }
}

static CAP: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_MERGE_CAP",
    "the merge on the card takes the title's changed runs only up to the cap of VICTORY_MERGE_RUNS a submission; past it the flights are settled",
);

/// Whether the merge on the card keeps to the cap of changed runs now.
pub(crate) fn cap_on() -> bool {
    CAP.on()
}

/// The cap's state for the log.
pub(crate) fn cap_said() -> &'static str {
    match CAP.mode() {
        0 => "off",
        1 => "on",
        _ => "alt",
    }
}

/// Since the last frame-rate line: landed pieces that held a byte of the
/// merge on the card, and torn words of 4, 8 and 16 bytes in them.
static TORN: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Counts one landed piece's torn words ([`torn_words`]).
pub(crate) fn count(torn: [u64; 3]) {
    TORN[0].fetch_add(1, Ordering::Relaxed);
    for (i, n) in torn.into_iter().enumerate() {
        TORN[i + 1].fetch_add(n, Ordering::Relaxed);
    }
}

/// [`TORN`] since the last asking, per frame of `frames`.
pub(crate) fn taken(frames: f64) -> [f64; 4] {
    [0, 1, 2, 3].map(|i| TORN[i].swap(0, Ordering::Relaxed) as f64 / frames)
}

/// Of `len` bytes from address `base` on: the aligned words of 4, 8 and
/// 16 bytes that hold a byte of `masked` (offsets from `base`, in order)
/// and another byte `i`, not in `masked`, for which `wrote(i)`. Bytes
/// outside `[0, len)` are neither.
pub(crate) fn torn_words(base: usize, len: usize, masked: &[usize], wrote: impl Fn(usize) -> bool) -> [u64; 3] {
    let mut out = [0u64; 3];
    if masked.is_empty() {
        return out;
    }
    let mut is_masked = vec![false; len];
    for &i in masked {
        if i < len {
            is_masked[i] = true;
        }
    }
    for (k, size) in [4usize, 8, 16].into_iter().enumerate() {
        let mut last_word = usize::MAX;
        for &i in masked {
            if i >= len {
                continue;
            }
            let word = (base + i) / size;
            if word == last_word {
                continue;
            }
            last_word = word;
            let lo = (word * size).saturating_sub(base);
            let hi = ((word + 1) * size - base).min(len);
            if (lo..hi).any(|j| !is_masked[j] && wrote(j)) {
                out[k] += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::torn_words;

    #[test]
    fn a_word_is_torn_only_with_both_a_title_byte_and_a_shader_byte() {
        // Title bytes at 0 and 9; the shader wrote 2 and 20.
        let wrote = |i: usize| i == 2 || i == 20;
        // 4: [0,4) torn, [8,12) not; 8: [0,8) torn; 16: [0,16) torn once.
        assert_eq!(torn_words(0, 32, &[0, 9], wrote), [1, 1, 1]);
        // Nothing written by the shader: nothing torn.
        assert_eq!(torn_words(0, 32, &[0, 9], |_| false), [0, 0, 0]);
        // No title bytes: nothing torn.
        assert_eq!(torn_words(0, 32, &[], wrote), [0, 0, 0]);
    }

    #[test]
    fn a_masked_byte_is_no_shader_byte() {
        // Every byte of the 4-byte word is the title's: not torn; the 8-
        // and 16-byte words hold shader bytes beside them.
        assert_eq!(torn_words(0, 8, &[0, 1, 2, 3], |_| true), [0, 1, 1]);
    }

    #[test]
    fn words_are_aligned_on_the_address() {
        // From address 6: offset 1 is address 7 (words 4..8, 0..8, 0..16),
        // offset 2 is address 8 (words 8..12, 8..16, 0..16).
        let wrote = |i: usize| i == 2;
        assert_eq!(torn_words(6, 16, &[1], wrote), [0, 0, 1]);
        // A word cut by the piece's start or end looks only inside.
        assert_eq!(torn_words(6, 3, &[0], |i| i == 5), [0, 0, 0]);
    }
}
