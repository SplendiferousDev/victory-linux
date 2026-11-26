//! What our host queues were asked to do, and where our threads stand
//! inside host calls, for the stall report (`crate::stall`).
//!
//! Why: in the frozen runs the card's fence stood one short of the first
//! flight in the air while the card was idle. Whether the host queue never
//! got the work (a submission thread of the host asleep), got it behind a
//! wait for a fence nobody signals, or the card stopped on it (a fault)
//! cannot be told from our own lines. Each host queue of ours now keeps,
//! per fence, the last value it was asked to signal and to wait for, and its
//! `ExecuteCommandLists` calls entered and returned; each thread of ours
//! inside a host call or a wait for a fence on the CPU is noted with since
//! when. The report reads each fence's completed value next to them.
//!
//! Every record is touched under short locks that nothing holds across a
//! host call; the report only tries them.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandList, ID3D12CommandQueue, ID3D12Fence};
use windows::core::Interface;

/// A fence one queue was asked about: the fence, the last value it was to
/// signal and to wait for (0: never).
struct FenceUse {
    fence: ID3D12Fence,
    signal: u64,
    wait: u64,
}

/// One host queue of ours.
struct Record {
    key: usize,
    name: &'static str,
    entered: AtomicU64,
    returned: AtomicU64,
    fences: Mutex<Vec<FenceUse>>,
}

// SAFETY: D3D12 fences are free-threaded.
unsafe impl Send for Record {}
// SAFETY: as above.
unsafe impl Sync for Record {}

static QUEUES: Mutex<Vec<Arc<Record>>> = Mutex::new(Vec::new());

/// A thread of ours inside a host call or a wait: the thread, its name,
/// what it does, on which queue, since when (`crate::host::now_us`).
struct Inside {
    thread: u32,
    name: String,
    what: &'static str,
    queue: &'static str,
    detail: String,
    since: u64,
}

static INSIDE: Mutex<Vec<Inside>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn record(queue: &ID3D12CommandQueue) -> Arc<Record> {
    let key = queue.as_raw() as usize;
    let mut all = lock(&QUEUES);
    if let Some(r) = all.iter().find(|r| r.key == key) {
        return r.clone();
    }
    let r = Arc::new(Record { key, name: "unnamed", entered: AtomicU64::new(0), returned: AtomicU64::new(0), fences: Mutex::new(Vec::new()) });
    all.push(r.clone());
    r
}

/// Names a host queue of ours for the report (once, when it is made).
pub(crate) fn name(queue: &ID3D12CommandQueue, name: &'static str) {
    let key = queue.as_raw() as usize;
    let mut all = lock(&QUEUES);
    all.retain(|r| r.key != key);
    all.push(Arc::new(Record { key, name, entered: AtomicU64::new(0), returned: AtomicU64::new(0), fences: Mutex::new(Vec::new()) }));
}

/// Marks this thread inside `what` until the guard goes.
pub(crate) struct Mark(u32);

impl Drop for Mark {
    fn drop(&mut self) {
        lock(&INSIDE).retain(|i| i.thread != self.0);
    }
}

/// Notes this thread inside `what` on `queue` (a name), with `detail`.
pub(crate) fn mark(what: &'static str, queue: &'static str, detail: impl FnOnce() -> String) -> Mark {
    let thread = crate::host::thread_id();
    let entry = Inside {
        thread,
        name: std::thread::current().name().unwrap_or("").to_owned(),
        what,
        queue,
        detail: detail(),
        since: crate::host::now_us(),
    };
    let mut inside = lock(&INSIDE);
    inside.retain(|i| i.thread != thread);
    inside.push(entry);
    Mark(thread)
}

fn note_fence(r: &Record, fence: &ID3D12Fence, signal: Option<u64>, wait: Option<u64>) {
    let mut fences = lock(&r.fences);
    let at = match fences.iter().position(|f| f.fence.as_raw() == fence.as_raw()) {
        Some(at) => at,
        None => {
            fences.push(FenceUse { fence: fence.clone(), signal: 0, wait: 0 });
            fences.len() - 1
        }
    };
    if let Some(v) = signal {
        fences[at].signal = v;
    }
    if let Some(v) = wait {
        fences[at].wait = v;
    }
}

/// `queue.ExecuteCommandLists(lists)`, counted and marked.
pub(crate) fn execute(queue: &ID3D12CommandQueue, lists: &[Option<ID3D12CommandList>]) {
    let r = record(queue);
    r.entered.fetch_add(1, Ordering::Relaxed);
    let _mark = mark("ExecuteCommandLists", r.name, || format!("{} lists", lists.len()));
    // SAFETY: the caller's closed lists of the queue's type.
    unsafe { queue.ExecuteCommandLists(lists) };
    r.returned.fetch_add(1, Ordering::Relaxed);
}

/// `queue.Signal(fence, value)`, noted.
pub(crate) fn signal(queue: &ID3D12CommandQueue, fence: &ID3D12Fence, value: u64) -> windows::core::Result<()> {
    let r = record(queue);
    note_fence(&r, fence, Some(value), None);
    let _mark = mark("Signal", r.name, || format!("to {value}"));
    // SAFETY: a live queue and fence of the same device.
    unsafe { queue.Signal(fence, value) }
}

/// `queue.Wait(fence, value)`, noted.
pub(crate) fn wait(queue: &ID3D12CommandQueue, fence: &ID3D12Fence, value: u64) -> windows::core::Result<()> {
    let r = record(queue);
    note_fence(&r, fence, None, Some(value));
    let _mark = mark("Wait", r.name, || format!("for {value}"));
    // SAFETY: a live queue and fence of the same device.
    unsafe { queue.Wait(fence, value) }
}

/// A queue's record as the report reads it.
pub(crate) struct QueueView {
    pub(crate) name: &'static str,
    pub(crate) entered: u64,
    pub(crate) returned: u64,
    /// Per fence: its address, last signal asked, last wait asked, its
    /// completed value now (None: the record was locked).
    pub(crate) fences: Option<Vec<(usize, u64, u64, u64)>>,
}

/// What the report says of the queues: per queue its calls entered and
/// returned and, per fence, signal and wait asked against completed (a
/// wait above completed with no signal of that value asked anywhere is a
/// wait nothing will meet).
pub(crate) fn describe_queues(queues: &[QueueView], out: &mut String) {
    let signalled = |fence: usize| -> u64 {
        queues.iter().filter_map(|q| q.fences.as_ref()).flatten().filter(|f| f.0 == fence).map(|f| f.1).max().unwrap_or(0)
    };
    for (i, q) in queues.iter().enumerate() {
        if i > 0 {
            out.push_str("; ");
        }
        let _ = write!(out, "{}: ExecuteCommandLists entered {} returned {}", q.name, q.entered, q.returned);
        if q.entered != q.returned {
            out.push_str(" (ONE NOT RETURNED)");
        }
        let Some(fences) = &q.fences else {
            out.push_str(", its fences locked");
            continue;
        };
        for &(fence, signal, wait, completed) in fences {
            let _ = write!(out, ", fence {fence:#x} at {completed}");
            if signal != 0 {
                let _ = write!(out, " signal asked {signal}{}", if completed < signal { " (NOT REACHED)" } else { "" });
            }
            if wait != 0 {
                let _ = write!(out, " wait asked {wait}{}", match () {
                    _ if completed >= wait => "",
                    _ if signalled(fence) < wait => " (NOT REACHED, and no queue was asked to signal it)",
                    _ => " (NOT REACHED)",
                });
            }
        }
    }
}

/// The report's section: the queues, then our threads inside host calls.
pub(crate) fn stall_section(out: &mut String) {
    let queues: Vec<Arc<Record>> = match QUEUES.try_lock() {
        Ok(q) => q.clone(),
        Err(_) => {
            out.push_str("locked (the list of host queues)");
            return;
        }
    };
    let views: Vec<QueueView> = queues
        .iter()
        .map(|r| QueueView {
            name: r.name,
            entered: r.entered.load(Ordering::Relaxed),
            returned: r.returned.load(Ordering::Relaxed),
            fences: r.fences.try_lock().ok().map(|f| {
                f.iter()
                    // SAFETY: a plain query of a live fence.
                    .map(|u| (u.fence.as_raw() as usize, u.signal, u.wait, unsafe { u.fence.GetCompletedValue() }))
                    .collect()
            }),
        })
        .collect();
    describe_queues(&views, out);
    let now = crate::host::now_us();
    match INSIDE.try_lock() {
        Ok(inside) if inside.is_empty() => out.push_str("; no thread of ours inside a host call"),
        Ok(inside) => {
            for i in inside.iter() {
                let _ = write!(
                    out,
                    "; thread {} '{}' inside {} on {} ({}) for {:.1} ms",
                    i.thread,
                    i.name,
                    i.what,
                    i.queue,
                    i.detail,
                    now.saturating_sub(i.since) as f64 / 1e3
                );
            }
        }
        Err(_) => out.push_str("; the threads inside host calls: locked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_nothing_signals_and_a_call_that_did_not_return_are_named() {
        let queues = [
            QueueView { name: "direct", entered: 10, returned: 10, fences: Some(vec![(0x100, 2845, 0, 2836)]) },
            QueueView { name: "copy down", entered: 5, returned: 4, fences: Some(vec![(0x100, 0, 2830, 2836), (0x200, 5, 0, 5)]) },
            QueueView { name: "copy up", entered: 0, returned: 0, fences: Some(vec![(0x300, 0, 7, 3)]) },
            QueueView { name: "other", entered: 1, returned: 1, fences: None },
        ];
        let mut out = String::new();
        describe_queues(&queues, &mut out);
        assert_eq!(
            out,
            "direct: ExecuteCommandLists entered 10 returned 10, fence 0x100 at 2836 signal asked 2845 (NOT REACHED); \
             copy down: ExecuteCommandLists entered 5 returned 4 (ONE NOT RETURNED), fence 0x100 at 2836 wait asked 2830, fence 0x200 at 5 signal asked 5; \
             copy up: ExecuteCommandLists entered 0 returned 0, fence 0x300 at 3 wait asked 7 (NOT REACHED, and no queue was asked to signal it); \
             other: ExecuteCommandLists entered 1 returned 1, its fences locked"
        );
    }

    #[test]
    fn a_mark_lasts_as_long_as_its_guard() {
        let me = crate::host::thread_id();
        {
            let _m = mark("Wait", "direct", || "for 3".into());
            assert!(lock(&INSIDE).iter().any(|i| i.thread == me && i.what == "Wait" && i.detail == "for 3"));
        }
        assert!(!lock(&INSIDE).iter().any(|i| i.thread == me));
    }
}
