//! Our host objects alive, by the code that made them (`VICTORY_MEM_SITES=1`;
//! off by default).
//!
//! Some 1.8 GB of the card's memory in use and an unknown share of the PC's
//! pinned memory are nobody's in the memory line: the driver keeps host
//! copies and command memory in pages it pins, and the PC's available memory
//! falls to a few hundred MB at peaks. This counts what we ask the device
//! for: buffers, textures, heaps of views and queries, command allocators and
//! command lists, each by its creating call site (`file:line`, through
//! `#[track_caller]` on the factories) and by a small set of kinds
//! ([`Kind`]). A count stays exact while the object lives: it is taken back
//! by the device's own destruction callback (`ID3DDestructionNotifier`,
//! which vkd3d-proton gives for resources, heaps, allocators and lists), so
//! a reference kept anywhere (a retired object, a descriptor slot) keeps it
//! counted. An object that cannot tell of its destruction is counted as made
//! only ("untracked").
//!
//! Bytes are what the device reports for a resource
//! (`GetResourceAllocationInfo`; buffers by their width, rounded up to
//! 64 KiB), descriptors times their size for a heap of views, eight bytes a
//! query; allocators and lists have none to report (the driver's command
//! memory grows behind them unseen), so they are counted by number, and the
//! line names host lists, their preludes and the idle ones.
//!
//! Every 10 s, with the memory line: one line, the kinds' totals and the
//! largest sites. With the switch off a factory pays one relaxed load.
//!
//! Idea from skornuta, victory-memory-stack (call-site counting of buffers).

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D::ID3DDestructionNotifier;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// What an object is, for the totals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum Kind {
    /// Buffers in the upload heap (CPU memory the driver pins).
    Upload = 0,
    /// Buffers in custom CPU-visible heaps shaders write (pinned too).
    Shared,
    /// Buffers in GPU upload heaps (the card's memory through the BAR).
    GpuUpload,
    /// Read-back buffers (CPU memory).
    ReadBack,
    /// Buffers in the card's own memory.
    Card,
    /// Committed textures (the card's memory).
    Texture,
    /// Reserved resources (memory comes from heaps mapped into them).
    Reserved,
    /// Heaps that resources are placed in or tiles mapped from.
    Heap,
    /// Heaps of views, samplers, render targets.
    ViewHeap,
    /// Heaps of queries.
    QueryHeap,
    /// Command allocators.
    Allocator,
    /// Command lists.
    List,
}

const KINDS: usize = 12;

const KIND_NAMES: [&str; KINDS] = [
    "upload", "shared", "gpu-upload", "read-back", "card buffers", "textures", "reserved", "resource heaps",
    "view heaps", "query heaps", "allocators", "lists",
];

/// The most sites told apart; later ones share the last.
const SITES: usize = 1024;

/// Per site: alive, bytes alive, made since the start, made untracked.
static ALIVE: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static BYTES: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static MADE: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static UNTRACKED: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];

/// The sites in the order first seen: kind, file, line.
static NAMES: Mutex<Vec<(Kind, &'static str, u32)>> = Mutex::new(Vec::new());

/// 0 unknown, 1 off, 2 on.
static ON: AtomicU8 = AtomicU8::new(0);

/// Whether objects are counted by site (`VICTORY_MEM_SITES=1`).
pub(crate) fn on() -> bool {
    match ON.load(Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = std::env::var_os("VICTORY_MEM_SITES").is_some_and(|v| v == "1");
            ON.store(if on { 2 } else { 1 }, Relaxed);
            if on {
                tracing::info!(
                    "host objects are counted by the code that made them (VICTORY_MEM_SITES=1): one line every 10 s"
                );
            }
            on
        }
    }
}

/// The site's number, added if new.
fn site(kind: Kind, file: &'static str, line: u32) -> usize {
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(at) = names.iter().position(|&(k, f, l)| k == kind && l == line && f == file) {
        return at;
    }
    if names.len() < SITES {
        names.push((kind, file, line));
    }
    names.len() - 1
}

/// The destruction callback's data: the site in the low bits, the bytes in
/// 64 KiB units above. No allocation, nothing to free.
const SITE_BITS: u32 = 10;

fn pack(site: usize, bytes: u64) -> usize {
    let units = bytes.div_ceil(64 << 10);
    ((units << SITE_BITS) | site as u64) as usize
}

fn unpack(data: usize) -> (usize, u64) {
    let data = data as u64;
    ((data & ((1 << SITE_BITS) - 1)) as usize, (data >> SITE_BITS) << 16)
}

/// Counts `object`, made at the caller's place, as `kind` of `bytes` until
/// the device destroys it. Nothing with the switch off.
#[track_caller]
pub(crate) fn note<T: Interface>(object: &T, kind: Kind, bytes: u64) {
    if !on() {
        return;
    }
    let at = std::panic::Location::caller();
    count(object, kind, bytes, at);
}

/// [`note`] for a resource: bytes as `device` reports them for its
/// description, its kind from its heap where the caller does not say.
#[track_caller]
pub(crate) fn note_resource(device: &ID3D12Device, resource: &ID3D12Resource, kind: Kind) {
    if !on() {
        return;
    }
    let at = std::panic::Location::caller();
    // SAFETY: plain queries of live objects.
    let bytes = unsafe {
        let desc = resource.GetDesc();
        if kind == Kind::Reserved {
            0
        } else if desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
            desc.Width
        } else {
            device.GetResourceAllocationInfo(0, &[desc]).SizeInBytes
        }
    };
    count(resource, kind, bytes, at);
}

/// [`note`] for a descriptor heap: its descriptors times their size.
#[track_caller]
pub(crate) fn note_view_heap(device: &ID3D12Device, heap: &ID3D12DescriptorHeap) {
    if !on() {
        return;
    }
    let at = std::panic::Location::caller();
    // SAFETY: plain queries of live objects.
    let bytes = unsafe {
        let desc = heap.GetDesc();
        u64::from(desc.NumDescriptors) * u64::from(device.GetDescriptorHandleIncrementSize(desc.Type))
    };
    count(heap, Kind::ViewHeap, bytes, at);
}

/// [`note`] for a command allocator and the list made on it.
#[track_caller]
pub(crate) fn note_pair(allocator: &ID3D12CommandAllocator, list: &ID3D12GraphicsCommandList) {
    if !on() {
        return;
    }
    let at = std::panic::Location::caller();
    count(allocator, Kind::Allocator, 0, at);
    count(list, Kind::List, 0, at);
}

fn count<T: Interface>(object: &T, kind: Kind, bytes: u64, at: &'static std::panic::Location<'static>) {
    let file = at.file().rsplit(['/', '\\']).next().unwrap_or(at.file());
    let n = site(kind, file, at.line());
    MADE[n].fetch_add(1, Relaxed);
    let Ok(notifier) = object.cast::<ID3DDestructionNotifier>() else {
        UNTRACKED[n].fetch_add(1, Relaxed);
        return;
    };
    let data = pack(n, bytes);
    let (_, rounded) = unpack(data);
    ALIVE[n].fetch_add(1, Relaxed);
    BYTES[n].fetch_add(rounded, Relaxed);
    // SAFETY: a live object; the callback's data is a plain number.
    if unsafe { notifier.RegisterDestructionCallback(Some(destroyed), data as *const _) }.is_err() {
        ALIVE[n].fetch_sub(1, Relaxed);
        BYTES[n].fetch_sub(rounded, Relaxed);
        UNTRACKED[n].fetch_add(1, Relaxed);
    }
}

/// The device's call as a counted object is destroyed (on whatever thread
/// let it go last).
unsafe extern "system" fn destroyed(data: *mut core::ffi::c_void) {
    let (n, bytes) = unpack(data as usize);
    if n < SITES {
        ALIVE[n].fetch_sub(1, Relaxed);
        BYTES[n].fetch_sub(bytes, Relaxed);
    }
}

/// One site's figures.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    kind: Kind,
    place: String,
    alive: u64,
    bytes: u64,
    made: u64,
    untracked: u64,
}

fn rows() -> Vec<Row> {
    let names = NAMES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    names
        .iter()
        .enumerate()
        .map(|(n, &(kind, file, line))| Row {
            kind,
            place: format!("{file}:{line}"),
            alive: ALIVE[n].load(Relaxed),
            bytes: BYTES[n].load(Relaxed),
            made: MADE[n].load(Relaxed),
            untracked: UNTRACKED[n].load(Relaxed),
        })
        .collect()
}

/// The line's text from the sites' rows and the host lists' figures
/// (host lists alive, of them idle, preludes made since the start).
fn text(rows: &[Row], lists: [u64; 3], extra: &str) -> String {
    let mb = |b: u64| b as f64 / f64::from(1 << 20);
    let mut kinds = [(0u64, 0u64); KINDS];
    for r in rows {
        kinds[r.kind as usize].0 += r.alive;
        kinds[r.kind as usize].1 += r.bytes;
    }
    let totals: Vec<String> = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| k.0 > 0)
        .map(|(i, k)| format!("{} {} {:.0} MB", KIND_NAMES[i], k.0, mb(k.1)))
        .collect();
    let mut by_bytes: Vec<&Row> = rows.iter().filter(|r| r.bytes > 0).collect();
    by_bytes.sort_by_key(|r| std::cmp::Reverse(r.bytes));
    let largest: Vec<String> = by_bytes
        .iter()
        .take(20)
        .map(|r| format!("{} {} {}/{:.0} MB", KIND_NAMES[r.kind as usize], r.place, r.alive, mb(r.bytes)))
        .collect();
    let mut by_count: Vec<&Row> =
        rows.iter().filter(|r| matches!(r.kind, Kind::Allocator | Kind::List) && r.alive > 0).collect();
    by_count.sort_by_key(|r| std::cmp::Reverse(r.alive));
    let commands: Vec<String> = by_count
        .iter()
        .take(10)
        .map(|r| format!("{} {} {} (made {})", KIND_NAMES[r.kind as usize], r.place, r.alive, r.made))
        .collect();
    let untracked: u64 = rows.iter().map(|r| r.untracked).sum();
    format!(
        "memory by site (VICTORY_MEM_SITES=1): alive by kind: {}; largest sites (alive/MB): {}; command objects by \
         site: {}; host lists alive {} (idle {}), preludes made {}{}; untracked (no destruction callback) {}",
        if totals.is_empty() { "none".into() } else { totals.join(", ") },
        if largest.is_empty() { "none".into() } else { largest.join(", ") },
        if commands.is_empty() { "none".into() } else { commands.join(", ") },
        lists[0],
        lists[1],
        lists[2],
        extra,
        untracked,
    )
}

/// Every 10 s, with the memory line.
pub(crate) fn line() {
    if !on() {
        return;
    }
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let last = LAST.load(Relaxed);
    if (last != 0 && now < last + 10_000) || LAST.compare_exchange(last, now.max(1), Relaxed, Relaxed).is_err() {
        return;
    }
    let lists = [
        crate::host::ALIVE[0].load(Relaxed),
        crate::list::idle_count() as u64,
        crate::host::LISTS[1].load(Relaxed),
    ];
    let extra = crate::host::prelude_pool_text() + &crate::host::allocator_trim_text();
    tracing::info!("{}", text(&rows(), lists, &extra));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_callback_data_carries_site_and_whole_64k_units() {
        for (site, bytes, back) in [(0, 0, 0), (3, 1, 64 << 10), (1023, 64 << 10, 64 << 10), (7, 5 << 30, 5 << 30)] {
            assert_eq!(unpack(pack(site, bytes)), (site, back));
        }
    }

    #[test]
    fn a_site_is_known_by_kind_file_and_line() {
        let a = site(Kind::Upload, "x.rs", 10);
        assert_eq!(site(Kind::Upload, "x.rs", 10), a);
        assert_ne!(site(Kind::Shared, "x.rs", 10), a);
        assert_ne!(site(Kind::Upload, "x.rs", 11), a);
    }

    #[test]
    fn the_line_sums_kinds_and_ranks_sites() {
        let row = |kind, place: &str, alive, bytes| Row {
            kind,
            place: place.into(),
            alive,
            bytes,
            made: alive,
            untracked: 0,
        };
        let rows = [
            row(Kind::Upload, "a.rs:1", 2, 3 << 20),
            row(Kind::Upload, "b.rs:2", 1, 10 << 20),
            row(Kind::Allocator, "host.rs:9", 500, 0),
        ];
        let t = text(&rows, [600, 40, 300], "");
        assert!(t.contains("upload 3 13 MB"), "{t}");
        assert!(t.find("b.rs:2").unwrap() < t.find("a.rs:1").unwrap(), "{t}");
        assert!(t.contains("allocators host.rs:9 500 (made 500)"), "{t}");
        assert!(t.contains("host lists alive 600 (idle 40), preludes made 300"), "{t}");
    }
}
