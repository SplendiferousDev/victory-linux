//! The stall report's sections of the flights and of the host device
//! (`crate::stall`), and what they read without taking locks. Apart from
//! `host.rs` for the asset policy's size limit of a source file.

use super::*;

/// [`late_met_known`] for the stall report: None if the line is locked.
pub(crate) fn late_met_tried(wait: &crate::list::ValueWait) -> Option<bool> {
    let (address, bytes) = wait.place();
    let line = LINE.try_lock().ok()?;
    let writes = line.in_hand.iter().map(|(at, what, n)| (at, what, n)).chain(line.line.iter().filter_map(|item| match item {
        Late::Write(at, what, n) => Some((at, what, n)),
        _ => None,
    }));
    for (at, what, n) in writes {
        if let crate::queue::Written::Value(v) = what
            && *at == address
            && *n >= bytes
            && wait.holds_for(*v)
        {
            return Some(true);
        }
    }
    Some(false)
}

/// Reads `dst.len()` bytes at `src` through the kernel: false if a page is
/// not readable (never faults; for reports from another thread).
pub(crate) fn peek(src: *const u8, dst: &mut [u8]) -> bool {
    read(src, dst.as_mut_ptr(), dst.len())
}

/// The stall report's section of the flights (`crate::stall`): what the
/// submitting thread is doing, how far the card got against what it was
/// given, the line of flights and what stands behind each, the locks a stuck
/// thread may hold, objects kept for running submissions, and the newest
/// entries of the trail. Only tries the locks.
pub(crate) fn stall_section(out: &mut String) {
    use std::fmt::Write;
    let now = now_us();
    let ms = |at: u64| now.saturating_sub(at) as f64 / 1000.0;
    let phase = match SUBMITTING[0].load(Ordering::Relaxed) {
        0 => "between submissions",
        1 => "preparing a submission",
        2 => "waiting for the card to run a submission",
        _ => "copying back what a submission wrote",
    };
    let _ = write!(
        out,
        "the submitting thread {phase} ({} lists) for {:.1} ms; submissions started {} finished {}; ",
        SUBMITTING[1].load(Ordering::Relaxed),
        ms(SUBMITTING[2].load(Ordering::Relaxed)),
        STARTED.load(Ordering::Relaxed),
        FINISHED.load(Ordering::Relaxed),
    );
    let Some(host) = host() else {
        out.push_str("no host device");
        return;
    };
    // SAFETY: a plain query of a free-threaded fence.
    let completed = unsafe { host.fence.GetCompletedValue() };
    let _ = write!(
        out,
        "the card's fence at {completed}, signalled up to {}; ",
        match host.submitted.try_lock() {
            Ok(s) => s.to_string(),
            Err(_) => "? (the submission lock `submitted` is held)".to_owned(),
        }
    );
    match LINE.try_lock() {
        Ok(line) => {
            let (mut runs, mut writes, mut signals) = (0, 0, 0);
            let mut first: Option<(u64, u64)> = None;
            let mut said = String::new();
            for item in &line.line {
                match item {
                    Late::Run(f) => {
                        runs += 1;
                        first.get_or_insert((f.number, f.fence));
                    }
                    Late::Write(at, what, bytes) => {
                        writes += 1;
                        if writes <= 4 {
                            let _ = write!(said, " write {what:?} ({bytes} bytes) to {at:#x};");
                        }
                    }
                    Late::Signal(fence, value) => {
                        signals += 1;
                        if signals <= 4 {
                            let _ = write!(
                                said,
                                " signal fence {fence:#x} to {value} (at {});",
                                crate::fence::completed(*fence as crate::object::This)
                            );
                        }
                    }
                }
            }
            let _ = write!(
                out,
                "line of flights: {} things, {runs} flights ({} lists){}, {writes} bottom-of-pipe writes, {signals} fence signals behind them;{said} \
                 a landing under way {}, {} writes in hand, {} landed lists to reset, the helper poked {}, the last flight's fence {}",
                line.line.len(),
                line.lists,
                match first {
                    Some((number, fence)) => format!(
                        ", the first number {number} behind card fence {fence}{}",
                        if fence <= completed { " (the card HAS run it: not landed)" } else { " (the card has not run it)" }
                    ),
                    None => String::new(),
                },
                line.settling,
                line.in_hand.len(),
                line.landed.len(),
                line.poke,
                line.target,
            );
        }
        Err(_) => out.push_str("line of flights: locked (lock LINE)"),
    }
    let _ = write!(
        out,
        "; lines run because a held head was met by memory alone (VICTORY_HELD_POLL_MS): {} times",
        HELD_POLLED.load(Ordering::Relaxed)
    );
    // The merge on the card (`crate::shadow::merge_on_card`) never waits:
    // a submission that finds no mask buffer free sends its bytes up as
    // copies. Said so that a line held behind our sync lists can be read.
    if crate::shadow::merge_on_card_wanted() {
        match CARD_WORK.try_lock() {
            Ok(work) => match work.as_ref().and_then(|w| w.merge.as_ref()) {
                Some(merge) => {
                    let busy = merge.masks.iter().filter(|m| m.3 > completed).count();
                    let _ = write!(
                        out,
                        "; the merge on the card: {} mask buffers ({} MB), {busy} read by submissions the card has not run (of {} at most)",
                        merge.masks.len(),
                        merge.masks.iter().map(|m| m.2).sum::<u64>() >> 20,
                        CardMerge::MOST
                    );
                }
                None => out.push_str("; the merge on the card: not made"),
            },
            Err(_) => out.push_str("; the merge on the card: locked (lock CARD_WORK: copies in the card's memory being brought up to date)"),
        }
    }
    let _ = write!(
        out,
        "; objects kept until a submission finishes: {}",
        match RETIRED.try_lock() {
            Ok(r) => r.len().to_string(),
            Err(_) => "locked (lock RETIRED)".to_owned(),
        }
    );
    match TRAIL.try_lock() {
        Ok(trail) => {
            out.push_str("; the newest of the trail:");
            for (at, thread, what) in trail.iter().rev().take(8) {
                let _ = write!(out, " [{:.1} s ago, thread {thread}] {what};", now.saturating_sub(*at) as f64 / 1e6);
            }
        }
        Err(_) => out.push_str("; the trail is locked (lock TRAIL)"),
    }
}

/// The stall report's section of the host device: whether it is removed
/// and why. A card that stopped on a fault of its own (the driver's "MMU
/// Fault" in the kernel's log) may leave the device unremoved and the fence
/// standing for good; `VKD3D_CONFIG=fault` makes the host's runtime give up
/// on such a wait after 5 s and say what faulted.
pub(crate) fn device_section(out: &mut String) {
    use std::fmt::Write;
    let Some(host) = host() else {
        out.push_str("none");
        return;
    };
    // SAFETY: a plain query of a free-threaded device.
    let removed = unsafe { host.device.GetDeviceRemovedReason() };
    let _ = match removed {
        Ok(()) => write!(out, "not removed (a card stopped by a fault may still say so: look for \"NVRM: Xid\" in the kernel's log at this time)"),
        Err(e) => write!(out, "REMOVED: {e}"),
    };
}
