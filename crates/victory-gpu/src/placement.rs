//! Where the title's pinned threads run on this host.
//!
//! The title pins threads with `SetThreadGroupAffinity` (its media library
//! with `SetThreadAffinityMask`) to masks of the console's sixteen logical
//! processors. The host applies them literally to its own numbering. Under
//! Wine a mask naming a processor the host does not have is refused as a
//! whole (`ERROR_INVALID_PARAMETER`) and the thread keeps all processors,
//! while a mask within them is applied as it stands: on a host with twelve
//! logical processors its recording threads (on a Windows PC pinned to
//! `0x1f00` with some job threads, see `crate::spread`) run anywhere, but
//! eight job threads are held to host processors 0-7 (the first 6 cores
//! and the second threads of two of them) and wait runnable 57-82 ms a
//! second, against 30-42 for five unpinned job threads of the same kind
//! (a 5 s sample of a world run). Host numbering is not the console's
//! either: Linux numbers a core's second thread six or more apart, the
//! console's pairs are neighbours.
//!
//! Every pinning the title asks for is said once in the log with what the
//! host made of it. With `VICTORY_AFFINITY_FREE=1` none is applied: the
//! call succeeds, the title reads back the mask it asked for, and the
//! thread runs wherever the host scheduler puts it (the title cannot tell:
//! it asks no processor number). `alt`: twenty seconds as asked, twenty
//! free, in turn; the threads pinned so far are moved at each change.
//! Off by default.
#![allow(non_snake_case)]

use std::collections::HashMap;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, SetLastError};
use windows_sys::Win32::System::SystemInformation::{
    GROUP_AFFINITY, GetLogicalProcessorInformation, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessAffinityMask, GetThreadId, OpenThread, THREAD_QUERY_INFORMATION, THREAD_SET_INFORMATION,
};

static FREE: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_AFFINITY_FREE",
    "the title's thread pinnings are not applied: the host scheduler places its threads",
);

/// A pinning the title asked for: the mask, and whether it came through
/// `SetThreadAffinityMask` (the host keeps what it has of such a mask).
#[derive(Clone, Copy)]
struct Asked {
    mask: usize,
    partial: bool,
}

/// The pinnings the title asked for, by thread id.
static ASKED: Mutex<Option<HashMap<u32, Asked>>> = Mutex::new(None);

fn asked() -> std::sync::MutexGuard<'static, Option<HashMap<u32, Asked>>> {
    ASKED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The process's processors (all the host has, here).
fn process_mask() -> usize {
    let (mut process, mut system) = (0usize, 0usize);
    // SAFETY: our own process and valid out pointers.
    if unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) } == 0 || process == 0 {
        return usize::MAX;
    }
    process
}

/// The masks of the host's cores, lowest processor first.
fn cores() -> Vec<usize> {
    let mut length = 0u32;
    // SAFETY: a size query with no buffer.
    unsafe { GetLogicalProcessorInformation(std::ptr::null_mut(), &mut length) };
    let count = length as usize / size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION>();
    if count == 0 {
        return Vec::new();
    }
    // SAFETY: plain data, filled in by the call below.
    let mut info: Vec<SYSTEM_LOGICAL_PROCESSOR_INFORMATION> = vec![unsafe { std::mem::zeroed() }; count];
    // SAFETY: a buffer of `length` bytes.
    if unsafe { GetLogicalProcessorInformation(info.as_mut_ptr(), &mut length) } == 0 {
        return Vec::new();
    }
    let mut cores: Vec<usize> = info.iter().filter(|i| i.Relationship == RelationProcessorCore).map(|i| i.ProcessorMask).collect();
    cores.sort_by_key(|m| m.trailing_zeros());
    cores
}

/// What the host does with a pinning while the process has `all`: `None`
/// if it refuses it. Wine: the group call refuses a mask naming any
/// processor beyond `all`; `SetThreadAffinityMask` keeps the part within.
fn host_applies(asked: Asked, all: usize) -> Option<usize> {
    let mask = if asked.partial { asked.mask & all } else { asked.mask };
    (mask != 0 && mask & !all == 0).then_some(mask)
}

/// The processors as a list, `0-7,9`.
fn list(mask: usize) -> String {
    let mut parts = Vec::new();
    let mut bit = 0;
    while bit < usize::BITS {
        if mask >> bit & 1 == 1 {
            let start = bit;
            while bit + 1 < usize::BITS && mask >> (bit + 1) & 1 == 1 {
                bit += 1;
            }
            parts.push(if start == bit { format!("{start}") } else { format!("{start}-{bit}") });
        }
        bit += 1;
    }
    parts.join(",")
}

/// The first pinning: the host's processors and what is done with them.
fn say_start(all: usize) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let cores = cores();
        let text = cores.iter().map(|&m| list(m)).collect::<Vec<_>>().join(" ");
        tracing::info!(
            "thread placement: host has {} logical processors on {} cores ({text}); the title's pinnings name the console's 16 and are {}",
            all.count_ones(),
            cores.len(),
            match FREE.mode() {
                0 => "applied as asked, as the host does (VICTORY_AFFINITY_FREE=1: not applied)",
                1 => "NOT applied: the host scheduler places the title's threads (VICTORY_AFFINITY_FREE=1)",
                _ => "applied for twenty seconds and not for twenty, in turn (VICTORY_AFFINITY_FREE=alt)",
            }
        );
        if FREE.mode() == 2 {
            start_turns();
        }
    });
}

/// Says each mask the title pins to once, with the host's answer.
fn say_mask(asked: Asked, all: usize) {
    static SAID: Mutex<Vec<(usize, bool)>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.contains(&(asked.mask, asked.partial)) {
        return;
    }
    said.push((asked.mask, asked.partial));
    let (mask, api) = (asked.mask, if asked.partial { "SetThreadAffinityMask" } else { "SetThreadGroupAffinity" });
    match host_applies(asked, all) {
        Some(m) => tracing::info!("thread placement: the title pins a thread to {mask:#x} ({api}); the host holds it to processors {}", list(m)),
        None => tracing::info!(
            "thread placement: the title pins a thread to {mask:#x} ({api}); the host refuses it (processors beyond its {}), the thread keeps all",
            all.count_ones()
        ),
    }
}

/// Sets a thread's processors on the host; the host's result.
fn host_set(thread: HANDLE, mask: usize, previous: *mut GROUP_AFFINITY) -> i32 {
    let affinity = GROUP_AFFINITY { Mask: mask, Group: 0, Reserved: [0; 3] };
    // SAFETY: the caller's thread handle and out pointer (may be null).
    unsafe { windows_sys::Win32::System::Threading::SetThreadGroupAffinity(thread, &affinity, previous) }
}

/// Notes the title's pinning of `thread`; whether to apply it.
fn note(thread: HANDLE, asked: Asked) -> (bool, Option<Asked>) {
    let all = process_mask();
    say_start(all);
    say_mask(asked, all);
    // SAFETY: the title's handle.
    let id = unsafe { GetThreadId(thread) };
    let before = self::asked().get_or_insert_with(HashMap::new).insert(id, asked);
    (!FREE.on(), before)
}

/// `SetThreadGroupAffinity`, for the title.
///
/// # Safety
/// As the host function: a thread handle and valid (or null `previous`) pointers.
pub unsafe extern "system" fn SetThreadGroupAffinity(thread: HANDLE, affinity: *const GROUP_AFFINITY, previous: *mut GROUP_AFFINITY) -> i32 {
    // SAFETY: the title's pointer.
    if let Some(a) = unsafe { affinity.as_ref() }.filter(|a| a.Group == 0 && a.Reserved == [0; 3] && a.Mask != 0) {
        let (apply, before) = note(thread, Asked { mask: a.Mask, partial: false });
        if !apply {
            if !previous.is_null() {
                let mask = before.map_or(process_mask(), |b| b.mask);
                // SAFETY: the title's out pointer.
                unsafe { *previous = GROUP_AFFINITY { Mask: mask, Group: 0, Reserved: [0; 3] } };
            }
            return 1;
        }
    }
    // SAFETY: the title's arguments.
    unsafe { windows_sys::Win32::System::Threading::SetThreadGroupAffinity(thread, affinity, previous) }
}

/// `SetThreadAffinityMask`, for the title: the previous mask, 0 on failure.
pub extern "system" fn SetThreadAffinityMask(thread: HANDLE, mask: usize) -> usize {
    if mask != 0 {
        let (apply, before) = note(thread, Asked { mask, partial: true });
        if !apply {
            return before.map_or(process_mask(), |b| b.mask);
        }
    }
    // SAFETY: the title's arguments.
    unsafe { windows_sys::Win32::System::Threading::SetThreadAffinityMask(thread, mask) }
}

/// `GetThreadGroupAffinity`, for the title: what it asked for while its
/// pinnings are not applied.
///
/// # Safety
/// As the host function.
pub unsafe extern "system" fn GetThreadGroupAffinity(thread: HANDLE, affinity: *mut GROUP_AFFINITY) -> i32 {
    if FREE.set() && !affinity.is_null() {
        // SAFETY: the title's handle.
        let id = unsafe { GetThreadId(thread) };
        if let Some(asked) = asked().as_ref().and_then(|a| a.get(&id).copied()) {
            // SAFETY: the title's out pointer.
            unsafe { *affinity = GROUP_AFFINITY { Mask: asked.mask, Group: 0, Reserved: [0; 3] } };
            // SAFETY: plain.
            unsafe { SetLastError(0) };
            return 1;
        }
    }
    // SAFETY: the title's arguments.
    unsafe { windows_sys::Win32::System::Threading::GetThreadGroupAffinity(thread, affinity) }
}

/// The mask a thread the title pinned gets now.
fn wanted(asked: Asked, all: usize, free: bool) -> usize {
    if free { all } else { host_applies(asked, all).unwrap_or(all) }
}

/// In turn: moves the pinned threads at each change of phase.
fn start_turns() {
    let made = std::thread::Builder::new().name("victory-placement".into()).spawn(|| {
        let mut last = None;
        loop {
            let free = FREE.on();
            if last != Some(free) {
                last = Some(free);
                let all = process_mask();
                let threads: Vec<(u32, Asked)> = asked().as_ref().map(|a| a.iter().map(|(&k, &v)| (k, v)).collect()).unwrap_or_default();
                let mut moved = 0;
                for (id, mask) in threads {
                    // SAFETY: a thread id; the handle is closed below.
                    let handle = unsafe { OpenThread(THREAD_QUERY_INFORMATION | THREAD_SET_INFORMATION, 0, id) };
                    if handle.is_null() {
                        continue;
                    }
                    if host_set(handle, wanted(mask, all, free), std::ptr::null_mut()) != 0 {
                        moved += 1;
                    } else {
                        // SAFETY: plain.
                        tracing::debug!(id, error = unsafe { GetLastError() }, "a title thread's processors could not be set");
                    }
                    // SAFETY: the handle opened above.
                    unsafe { CloseHandle(handle) };
                }
                tracing::info!("thread placement: {moved} pinned title threads moved ({})", if free { "free" } else { "as asked" });
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
    if let Err(error) = made {
        tracing::warn!(%error, "the thread placement thread could not be started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_refuses_a_mask_beyond_its_processors() {
        let all = 0xFFF;
        let group = |mask| Asked { mask, partial: false };
        let old = |mask| Asked { mask, partial: true };
        assert_eq!(host_applies(group(0xFF), all), Some(0xFF));
        assert_eq!(host_applies(group(0x1F00), all), None);
        assert_eq!(host_applies(group(0), all), None);
        // SetThreadAffinityMask keeps the part the host has
        assert_eq!(host_applies(old(0x1F00), all), Some(0xF00));
        assert_eq!(host_applies(old(0xF000), all), None);
        assert_eq!(wanted(group(0x1F00), all, false), all);
        assert_eq!(wanted(group(0xFF), all, false), 0xFF);
        assert_eq!(wanted(group(0xFF), all, true), all);
    }

    #[test]
    fn processors_read_as_ranges() {
        assert_eq!(list(0xFF), "0-7");
        assert_eq!(list(0x41), "0,6");
        assert_eq!(list(0xE38), "3-5,9-11");
        assert_eq!(list(0), "");
    }

    #[test]
    fn this_host_has_cores() {
        let cores = cores();
        assert!(!cores.is_empty());
        let all = process_mask();
        assert_eq!(cores.iter().fold(0, |m, c| m | c) & all, all & cores.iter().fold(0, |m, c| m | c));
    }
}
