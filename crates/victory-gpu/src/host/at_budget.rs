//! The memory line's at-budget and buffer-churn warnings (`at_budget_note`),
//! split out of `host` to keep that file under the size limit.

use super::*;

/// Said with the memory line: the card's memory at its budget (the first
/// line of a streak, then every sixth, ~30 s), and buffers made per second
/// past `VICTORY_MAPPING_CHURN_WARN` (MB a second of upload + shared + card
/// buffers made, default 150; 0 off). `VICTORY_AT_BUDGET_EXIT=<lines>`
/// ends the run (as a broken driver does, `crate::hostfault`) after that
/// many lines in a row at the budget: for unattended soaks.
pub(super) fn at_budget_note(most: u64, least: u64, now: [u64; 6], then: [u64; 6], ms: u64) {
    static MARGIN: OnceLock<u64> = OnceLock::new();
    static CHURN: OnceLock<u64> = OnceLock::new();
    static EXIT: OnceLock<u64> = OnceLock::new();
    let env = |name: &str, default: u64| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    let margin = *MARGIN.get_or_init(|| env("VICTORY_AT_BUDGET_MARGIN_MB", 32)) << 20;
    let churn_mb = *CHURN.get_or_init(|| env("VICTORY_MAPPING_CHURN_WARN", 150));
    let exit_after = *EXIT.get_or_init(|| env("VICTORY_AT_BUDGET_EXIT", 0));
    let made_mb = (0..3).map(|i| now[i].saturating_sub(then[i])).sum::<u64>() >> 20;
    if churn_mb != 0 && made_mb * 1000 / ms.max(1) > churn_mb {
        tracing::warn!(
            "buffers made in the last {ms} ms: {made_mb} MB (upload + shared + card), more than VICTORY_MAPPING_CHURN_WARN={churn_mb} MB a second: \
             each one a mapping the card's driver makes and takes down"
        );
    }
    if least != u64::MAX && most + margin >= least {
        let n = AT_BUDGET_LINES.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n.is_multiple_of(6) {
            tracing::warn!(
                "the card's memory is at its budget ({} MB in use, budget {} MB; {n} memory lines in a row): the driver moves allocations \
                 out of it, the state every NVIDIA mapping-record failure so far came in",
                most >> 20,
                least >> 20
            );
        }
        if exit_after != 0 && n >= exit_after {
            tracing::error!("the card's memory stood at its budget for {n} memory lines (VICTORY_AT_BUDGET_EXIT={exit_after}): the run ends now");
            std::thread::sleep(std::time::Duration::from_millis(100));
            // SAFETY: ends this process, as `crate::hostfault` does.
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(windows_sys::Win32::System::Threading::GetCurrentProcess(), 0xB0D6);
            }
            std::process::abort();
        }
    } else if AT_BUDGET_LINES.swap(0, Ordering::Relaxed) > 0 {
        tracing::info!("the card's memory is under its budget again ({} MB in use, budget {} MB)", most >> 20, least >> 20);
    }
}
