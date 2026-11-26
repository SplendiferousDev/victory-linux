//! The wait before a view heap slot of the title changes kind under
//! submissions in flight (`crate::slotkind`). Apart from `host.rs` for the
//! asset policy's size limit of a source file.

use super::*;

/// The longest [`land_started`] waits: flights that have not landed by then
/// are not waited for further (said once by the caller).
const MOST: std::time::Duration = std::time::Duration::from_secs(10);

/// Waits until every submission started before the call has finished: the
/// flights in the air have landed (`crate::slotkind::gate`).
///
/// With the relay (the default with flights) the thread that lands flights
/// takes none of our locks, so any thread may wait for it here, the
/// submitting thread too (which holds the submission lock). Without it a
/// settle needs the submission lock: taken only if it is free, else the
/// write goes ahead unprotected (counted). The relay's own thread never
/// waits for itself.
pub(crate) fn land_started() -> crate::slotkind::Landed {
    use crate::slotkind::Landed;
    let target = STARTED.load(Ordering::Acquire);
    if target <= FINISHED.load(Ordering::Acquire) {
        return Landed::Clear;
    }
    if late() && relay() {
        if std::thread::current().name() == Some("victory-relay") {
            return Landed::Unprotected;
        }
        let begun = std::time::Instant::now();
        let mut line = line();
        while FINISHED.load(Ordering::Acquire) < target {
            if begun.elapsed() >= MOST {
                return Landed::GaveUp;
            }
            line = LINE_LANDED
                .wait_timeout(line, std::time::Duration::from_millis(20))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        return Landed::Waited;
    }
    let Some(host) = host() else {
        return Landed::Clear;
    };
    match host.submitted.try_lock() {
        Ok(mut submitted) => {
            if late() {
                host.settle(&mut submitted, WHY_RUN);
            }
            if FINISHED.load(Ordering::Acquire) >= target {
                Landed::Waited
            } else {
                Landed::Unprotected
            }
        }
        Err(_) => Landed::Unprotected,
    }
}
