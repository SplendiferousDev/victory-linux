//! How many buffers the layer makes, and which of them the indirect
//! window path makes (`crate::draw` windows, `crate::indirect::gather`).
//!
//! Every buffer made is memory the card's driver maps into the card's
//! address space, and every buffer let go is a mapping it takes down. On
//! Linux the NVIDIA kernel driver once reported a mapping it could not
//! insert ("intermapRegisterDmaMapping: Failed to insert new mapping node")
//! and then hung, and the same lines came again in a run with
//! `VICTORY_INDIRECT_WINDOW=1`. The window path makes no memory of its own
//! per draw or per frame: a list's indirect scratch buffer grows by
//! doubling and is kept, its gathered-record buffers (at most eight a list)
//! are taken again from the list's spares, and its budget word is made
//! once. This line lets a run show that: the buffers made per frame by
//! kind, and those the window path made, beside the memory line every five
//! seconds. If the window path's numbers stay near zero while the line's
//! totals do not move with the switch, the windows add no mappings.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Since the start: indirect scratch buffers made (`HostList::indirect_scratch`)
/// and their bytes; buffers for gathered records made, and taken again from
/// a list's spares; budget words made for gathered records.
static MADE: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// A list's indirect scratch buffer of `bytes` was made (the first, or a
/// bigger one in place of the last).
pub(crate) fn scratch_made(bytes: u64) {
    MADE[0].fetch_add(1, Relaxed);
    MADE[1].fetch_add(bytes, Relaxed);
}

/// A buffer for a list's gathered records was made (`fresh`) or taken again
/// from the list's spares.
pub(crate) fn gather_set(fresh: bool) {
    MADE[if fresh { 2 } else { 3 }].fetch_add(1, Relaxed);
}

/// A list's budget word for gathered records was made.
pub(crate) fn budget_made() {
    MADE[4].fetch_add(1, Relaxed);
}

/// What [`line`] last said: the counts, `crate::host::MADE`'s buffer counts
/// (upload, shared, card), and the presents.
static MARK: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// The counts now: [`MADE`], the host's buffers made by kind, presents.
fn now() -> [u64; 9] {
    let host = |i: usize| crate::host::MADE[i].load(Relaxed);
    [
        MADE[0].load(Relaxed),
        MADE[1].load(Relaxed),
        MADE[2].load(Relaxed),
        MADE[3].load(Relaxed),
        MADE[4].load(Relaxed),
        host(0),
        host(2),
        host(4),
        crate::stall::presents(),
    ]
}

/// Per frame since the last line, from the counts `then` and `now`.
fn per_frame(then: &[u64; 9], now: &[u64; 9]) -> [f64; 8] {
    let frames = now[8].saturating_sub(then[8]).max(1) as f64;
    std::array::from_fn(|i| now[i].saturating_sub(then[i]) as f64 / frames)
}

/// Said with the memory line (every five seconds).
pub(crate) fn line() {
    let now = now();
    let then: [u64; 9] = std::array::from_fn(|i| MARK[i].swap(now[i], Relaxed));
    if now[8] == then[8] {
        return;
    }
    let f = per_frame(&then, &now);
    tracing::info!(
        "buffers made per frame (each one a mapping the card's driver makes, and takes down when it is let go): upload {:.2}, \
         shared {:.2}, card {:.2}; of the card's, by the indirect path: scratch buffers {:.3} ({:.1} KiB), buffers for gathered \
         records {:.3} made and {:.3} taken again, budget words {:.3}; since the start: {} scratch buffers, {} buffers for gathered \
         records made and {} taken again, {} budget words",
        f[5],
        f[6],
        f[7],
        f[0],
        f[1] / 1024.0,
        f[2],
        f[3],
        f[4],
        now[0],
        now[2],
        now[3],
        now[4],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_per_frame_since_the_last_line() {
        let then = [1, 1024, 2, 0, 1, 10, 20, 30, 100];
        let now = [3, 5120, 2, 8, 1, 30, 20, 40, 110];
        let f = per_frame(&then, &now);
        assert_eq!(f[0], 0.2);
        assert_eq!(f[1], 409.6);
        assert_eq!(f[3], 0.8);
        assert_eq!(f[5], 2.0);
        assert_eq!(f[7], 1.0);
        // No present since: counted over one frame, never divided by zero.
        let f = per_frame(&now, &now);
        assert!(f.iter().all(|v| *v == 0.0));
    }
}
