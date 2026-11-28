//! Command allocators taken anew now and then
//! (`VICTORY_ALLOCATOR_TRIM_SECS=<s>`; off while unset or 0). Apart from
//! `host.rs` for the asset policy's size limit of a source file.
//!
//! A command allocator keeps the memory of the most its lists ever recorded
//! into it: vkd3d-proton resets its command pool without releasing that
//! memory, and our host lists are pooled and reset for the whole run, so a
//! list that once took a loading frame's commands keeps that much for good.
//! Nothing tells how much an allocator holds, so age stands in for growth:
//! with the switch an allocator older than `s` seconds is replaced by a new
//! one at the only moments it holds nothing that may run, and the old one
//! goes to [`retire`].
//!
//! Where: a host list's reset ([`HostList::reset`], called only after every
//! submission of the list has finished, as `Reset` itself requires), and a
//! prelude taken from the shared pool (`prelude_pool`, whose idle preludes
//! returned through [`retire`] after their submissions finished). Preludes
//! a list keeps of its own (the pool off) are not renewed. The list is then
//! reset onto the new allocator at once, before anything records; the old
//! allocator is not destroyed there but retired: it is dropped only once
//! every submission started by then has finished, the same path every
//! object a submission may still use takes. Nothing new is added to that
//! path.
//!
//! Idea from skornuta, victory-memory-stack (allocator trim).

use super::*;

/// Allocators renewed since the start: host lists', pooled preludes'; and
/// renewals that failed (the old allocator served on).
pub(crate) static STATS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Which allocator a renewal counts as.
pub(super) const LIST: usize = 0;
pub(super) const PRELUDE: usize = 1;

/// The age in seconds past which an allocator is renewed, from the switch's
/// text: `None` off.
fn parse(text: Option<&str>) -> Option<u64> {
    let secs: u64 = text?.trim().parse().ok()?;
    (secs > 0).then(|| secs.clamp(5, 3600))
}

/// The age in ms past which an allocator is renewed, or `None` (off).
pub(crate) fn after_ms() -> Option<u64> {
    static MS: OnceLock<Option<u64>> = OnceLock::new();
    *MS.get_or_init(|| {
        let secs = parse(std::env::var("VICTORY_ALLOCATOR_TRIM_SECS").ok().as_deref());
        if let Some(secs) = secs {
            tracing::info!(
                "command allocators older than {secs} s are taken anew at a host list's reset and a pooled prelude's reuse; the old ones are retired (VICTORY_ALLOCATOR_TRIM_SECS)"
            );
        }
        secs.map(|s| s * 1000)
    })
}

/// Whether an allocator made at `made` is due for renewal at `now`.
fn due(made: u64, now: u64, after: u64) -> bool {
    now.saturating_sub(made) >= after
}

/// Renews `slot` (a direct allocator and when it was made) if it is due:
/// the new one goes in, the old one to [`retire`]. To be called only where
/// the allocator holds nothing that may run, right before its list is reset
/// onto `slot`'s allocator.
pub(super) fn refresh(slot: &mut (ID3D12CommandAllocator, u64), which: usize) {
    let Some(after) = after_ms() else {
        return;
    };
    let now = now_ms();
    if !due(slot.1, now, after) {
        return;
    }
    let Some(host) = host() else {
        return;
    };
    // SAFETY: plain object creation.
    let made: windows::core::Result<ID3D12CommandAllocator> =
        unsafe { host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) };
    match made {
        Ok(fresh) => {
            crate::memsites::note(&fresh, crate::memsites::Kind::Allocator, 0);
            let old = std::mem::replace(slot, (fresh, now));
            retire(old.0);
            STATS[which].fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            // The old one serves on and is asked again after another while.
            slot.1 = now;
            STATS[2].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// For the memory-by-site line: the renewals, or nothing when off.
pub(crate) fn text() -> String {
    if after_ms().is_none() {
        return String::new();
    }
    format!(
        "; allocators renewed (VICTORY_ALLOCATOR_TRIM_SECS): {} of host lists, {} of pooled preludes, {} failed",
        STATS[LIST].load(Ordering::Relaxed),
        STATS[PRELUDE].load(Ordering::Relaxed),
        STATS[2].load(Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unset_or_zero_and_clamped() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("x")), None);
        assert_eq!(parse(Some("30")), Some(30));
        assert_eq!(parse(Some("1")), Some(5));
        assert_eq!(parse(Some("99999")), Some(3600));
    }

    #[test]
    fn an_allocator_is_due_once_old_enough() {
        assert!(!due(1000, 30_999, 30_000));
        assert!(due(1000, 31_000, 30_000));
        // A clock read before the making counts as no age.
        assert!(!due(5000, 4000, 30_000));
    }
}
