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
//! The line also sets our count beside the driver's (what the system says
//! this process holds of the card's memory and of the PC's memory shared
//! with it, `QueryVideoMemoryInfo`): the difference is device memory that is
//! not an object of ours, chiefly vkd3d-proton's own. vkd3d-proton places a
//! buffer under 2 MiB in a 16 MiB block it shares with others of the same
//! heap and a texture under 8 MiB in a 32 MiB one, and frees a block only
//! once all in it are gone, so the line counts our objects below those
//! sizes apart (the room a block keeps is in the difference, not in ours),
//! and the host pipelines made and loaded (their code is the driver's).
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
    /// Buffers as [`Kind::Shared`] that are windows of title memory outside
    /// buffer objects (`crate::shadow`'s windows), apart from the title
    /// buffers' copies and the CPU sides of copies on the card.
    SharedWindow,
}

const KINDS: usize = 13;

const KIND_NAMES: [&str; KINDS] = [
    "upload", "shared", "gpu-upload", "read-back", "card buffers", "textures", "reserved", "resource heaps",
    "view heaps", "query heaps", "allocators", "lists", "shared windows",
];

/// The most sites told apart; later ones share the last.
const SITES: usize = 1024;

/// Per site: alive, bytes alive, made since the start, made untracked.
static ALIVE: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static BYTES: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static MADE: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static UNTRACKED: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
/// Per kind: alive and bytes alive of the objects small enough for
/// vkd3d-proton to place in a shared block ([`small`]).
static SMALL_ALIVE: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
static SMALL_BYTES: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];

/// Whether vkd3d-proton places an object of `kind` and `bytes` in a block it
/// shares with others (`memory.c`: buffers under `VKD3D_VA_BLOCK_SIZE`, 2 MiB,
/// in 16 MiB chunks; textures under 8 MiB in 32 MiB ones). Heaps of views,
/// queries and command objects are not resources.
fn small(kind: Kind, bytes: u64) -> bool {
    match kind {
        Kind::Upload | Kind::Shared | Kind::GpuUpload | Kind::ReadBack | Kind::Card | Kind::SharedWindow => {
            bytes < (2 << 20)
        }
        Kind::Texture => bytes < (8 << 20),
        _ => false,
    }
}

/// Which of the system's two memory groups a kind's memory is in: 0 the
/// card's own, 1 the PC's shared with the card, `None` not known here
/// (resource heaps of either, reserved resources, command objects).
fn group(kind: Kind) -> Option<usize> {
    match kind {
        Kind::Card | Kind::Texture | Kind::GpuUpload | Kind::ViewHeap | Kind::QueryHeap => Some(0),
        Kind::Upload | Kind::Shared | Kind::ReadBack | Kind::SharedWindow => Some(1),
        _ => None,
    }
}

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

/// A site's file as the line names it: its path below the crate's `src`
/// (`descriptor/host.rs`, not `host.rs`: several files share a name).
fn place_of(file: &'static str) -> &'static str {
    let at = ["/src/", "\\src\\"].iter().filter_map(|s| file.rfind(s).map(|at| at + s.len())).max();
    match at {
        Some(at) => &file[at..],
        None => file.rsplit(['/', '\\']).next().unwrap_or(file),
    }
}

/// The destruction callback's data: the site in the low bits, then the
/// kind's number plus one for a small object ([`small`]) or 0, the bytes in
/// 64 KiB units above. No allocation, nothing to free, no lock.
const SITE_BITS: u32 = 10;
const KIND_BITS: u32 = 4;
const UNITS_AT: u32 = SITE_BITS + KIND_BITS;

fn pack(site: usize, bytes: u64, small: Option<Kind>) -> usize {
    let units = bytes.div_ceil(64 << 10);
    let small = small.map_or(0, |k| k as u64 + 1);
    ((units << UNITS_AT) | (small << SITE_BITS) | site as u64) as usize
}

fn unpack(data: usize) -> (usize, u64, Option<Kind>) {
    let data = data as u64;
    let small = ((data >> SITE_BITS) & ((1 << KIND_BITS) - 1)) as usize;
    let kind = small.checked_sub(1).and_then(|k| KIND_OF.get(k).copied());
    ((data & ((1 << SITE_BITS) - 1)) as usize, (data >> UNITS_AT) << 16, kind)
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
    let n = site(kind, place_of(at.file()), at.line());
    MADE[n].fetch_add(1, Relaxed);
    let Ok(notifier) = object.cast::<ID3DDestructionNotifier>() else {
        UNTRACKED[n].fetch_add(1, Relaxed);
        return;
    };
    let tiny = small(kind, bytes);
    let data = pack(n, bytes, tiny.then_some(kind));
    let (_, rounded, _) = unpack(data);
    ALIVE[n].fetch_add(1, Relaxed);
    BYTES[n].fetch_add(rounded, Relaxed);
    if tiny {
        SMALL_ALIVE[kind as usize].fetch_add(1, Relaxed);
        SMALL_BYTES[kind as usize].fetch_add(rounded, Relaxed);
    }
    // SAFETY: a live object; the callback's data is a plain number.
    if unsafe { notifier.RegisterDestructionCallback(Some(destroyed), data as *const _) }.is_err() {
        ALIVE[n].fetch_sub(1, Relaxed);
        BYTES[n].fetch_sub(rounded, Relaxed);
        if tiny {
            SMALL_ALIVE[kind as usize].fetch_sub(1, Relaxed);
            SMALL_BYTES[kind as usize].fetch_sub(rounded, Relaxed);
        }
        UNTRACKED[n].fetch_add(1, Relaxed);
    }
}

/// The device's call as a counted object is destroyed (on whatever thread
/// let it go last).
unsafe extern "system" fn destroyed(data: *mut core::ffi::c_void) {
    let (n, bytes, tiny) = unpack(data as usize);
    if n < SITES {
        ALIVE[n].fetch_sub(1, Relaxed);
        BYTES[n].fetch_sub(bytes, Relaxed);
        if let Some(k) = tiny {
            SMALL_ALIVE[k as usize].fetch_sub(1, Relaxed);
            SMALL_BYTES[k as usize].fetch_sub(bytes, Relaxed);
        }
    }
}

/// Our side beside the driver's, for the line: per group (the card's own,
/// the PC's shared) the driver's use (`None` unknown), our bytes alive, and
/// of them the objects in shared blocks (alive, bytes); then the host
/// pipelines made and loaded since the start.
fn driver_text(rows: &[Row], driver: Option<[u64; 2]>, small: &[(u64, u64); KINDS], pipelines: (u64, u64)) -> String {
    let mb = |b: u64| b as f64 / f64::from(1 << 20);
    let mut ours = [0u64; 2];
    for r in rows {
        if let Some(g) = group(r.kind) {
            ours[g] += r.bytes;
        }
    }
    let mut tiny = [(0u64, 0u64); 2];
    for (k, &(n, b)) in small.iter().enumerate() {
        let Some(g) = KIND_OF.get(k).and_then(|&kind| group(kind)) else { continue };
        tiny[g].0 += n;
        tiny[g].1 += b;
    }
    let names = ["the card's own", "the PC's shared"];
    let sides: Vec<String> = (0..2)
        .map(|g| {
            let theirs = match driver {
                Some(d) => format!(
                    "the driver counts {:.0} MB, ours {:.0} MB ({:.0} MB not ours)",
                    mb(d[g]),
                    mb(ours[g]),
                    mb(d[g]) - mb(ours[g])
                ),
                None => format!("ours {:.0} MB (the driver's count not known)", mb(ours[g])),
            };
            format!("{}: {theirs}, of ours {} objects {:.0} MB in vkd3d-proton's shared blocks", names[g], tiny[g].0, mb(tiny[g].1))
        })
        .collect();
    format!(
        "; device memory beside the driver's count: {}; host pipelines made {} and loaded {} since the start",
        sides.join("; "),
        pipelines.0,
        pipelines.1
    )
}

/// The kinds by their number.
const KIND_OF: [Kind; KINDS] = [
    Kind::Upload,
    Kind::Shared,
    Kind::GpuUpload,
    Kind::ReadBack,
    Kind::Card,
    Kind::Texture,
    Kind::Reserved,
    Kind::Heap,
    Kind::ViewHeap,
    Kind::QueryHeap,
    Kind::Allocator,
    Kind::List,
    Kind::SharedWindow,
];

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
    let rows = rows();
    let driver = crate::host::host().and_then(|h| h.video_memory_groups()).map(|[own, shared]| [own[1], shared[1]]);
    let small = std::array::from_fn(|k| (SMALL_ALIVE[k].load(Relaxed), SMALL_BYTES[k].load(Relaxed)));
    let extra = crate::host::prelude_pool_text()
        + &crate::host::allocator_trim_text()
        + &crate::shadow::window_use_text()
        + &driver_text(&rows, driver, &small, crate::pipeline::frame_log_counts());
    tracing::info!("{}", text(&rows, lists, &extra));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_callback_data_carries_site_and_whole_64k_units() {
        for (site, bytes, back) in [(0, 0, 0), (3, 1, 64 << 10), (1023, 64 << 10, 64 << 10), (7, 5 << 30, 5 << 30)] {
            for tiny in [None, Some(Kind::Upload), Some(Kind::SharedWindow), Some(Kind::Texture)] {
                assert_eq!(unpack(pack(site, bytes, tiny)), (site, back, tiny));
            }
        }
    }

    #[test]
    fn small_follows_vkd3d_protons_block_sizes() {
        assert!(small(Kind::Shared, (2 << 20) - 1));
        assert!(!small(Kind::Shared, 2 << 20));
        assert!(small(Kind::Texture, (8 << 20) - 1));
        assert!(!small(Kind::Texture, 8 << 20));
        assert!(!small(Kind::ViewHeap, 1));
        assert!(!small(Kind::Allocator, 0));
    }

    #[test]
    fn kinds_are_listed_in_their_order() {
        for (i, k) in KIND_OF.iter().enumerate() {
            assert_eq!(*k as usize, i);
        }
    }

    #[test]
    fn the_driver_part_sets_ours_beside_theirs() {
        let row = |kind, bytes| Row { kind, place: "a.rs:1".into(), alive: 1, bytes, made: 1, untracked: 0 };
        let rows = [row(Kind::Card, 100 << 20), row(Kind::Texture, 200 << 20), row(Kind::SharedWindow, 300 << 20)];
        let mut small = [(0, 0); KINDS];
        small[Kind::Card as usize] = (5, 10 << 20);
        small[Kind::SharedWindow as usize] = (7, 20 << 20);
        let t = driver_text(&rows, Some([500 << 20, 450 << 20]), &small, (3, 9));
        assert!(t.contains("the card's own: the driver counts 500 MB, ours 300 MB (200 MB not ours), of ours 5 objects 10 MB"), "{t}");
        assert!(t.contains("the PC's shared: the driver counts 450 MB, ours 300 MB (150 MB not ours), of ours 7 objects 20 MB"), "{t}");
        assert!(t.contains("host pipelines made 3 and loaded 9"), "{t}");
        let t = driver_text(&rows, None, &small, (0, 0));
        assert!(t.contains("ours 300 MB (the driver's count not known)"), "{t}");
    }

    #[test]
    fn a_site_is_named_by_its_path_below_src() {
        assert_eq!(place_of("crates/victory-gpu/src/descriptor/host.rs"), "descriptor/host.rs");
        assert_eq!(place_of("crates\\victory-gpu\\src\\host.rs"), "host.rs");
        assert_eq!(place_of("crates/victory-gpu/src/host.rs"), "host.rs");
        assert_eq!(place_of("elsewhere/lib.rs"), "lib.rs");
        assert_eq!(place_of("lib.rs"), "lib.rs");
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
