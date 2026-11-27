//! Small changes to the copies in the card's memory taken from other forks,
//! each behind a switch of its own (off unless `1`; `alt` in turn,
//! `crate::alt`). Apart from `host.rs` for the asset policy's size limit of
//! a source file.

use super::*;

static ONE_BARRIER: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_CARD_ONE_BARRIER",
    "the state changes after the copies up to the card's copies are one a buffer, however many copies lie in it",
);

/// State changes left out because their buffer had one already in the same
/// call (since the start).
pub(crate) static ONE_BARRIER_SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether the state change of `buffer` after the copies up to it is the
/// first of that buffer in the call being made (`seen`: the buffers of
/// those before). A second change of the same buffer in one
/// `ResourceBarrier` call starts from a state the first already left.
/// Always true with `VICTORY_CARD_ONE_BARRIER` off, as before.
/// (After codec-d/cool_fox 641835a4, by way of iLucasRT-VInAMD 773d902f.)
pub(super) fn first_of_buffer(seen: &mut Vec<usize>, buffer: &ID3D12Resource) -> bool {
    if !ONE_BARRIER.on() {
        return true;
    }
    let raw = buffer.as_raw() as usize;
    if seen.contains(&raw) {
        if ONE_BARRIER_SKIPPED.fetch_add(1, Ordering::Relaxed) == 0 {
            tracing::info!("a card copy's buffer had two state changes in one call: one left out (VICTORY_CARD_ONE_BARRIER)");
        }
        return false;
    }
    seen.push(raw);
    true
}
