//! Static samplers made in one kept order, before any root signature of a
//! pipeline that has them (`VICTORY_SAMPLER_ORDER=1`; unset: not, as before).
//!
//! Ported from the base fork's commit 9e907466 (fxceless), which describes
//! the cause: the card's driver (NVIDIA) gives each static sampler a place
//! in a table of its own when the first root signature holding it is
//! created, and a compiled pipeline carries the places. The driver's cache
//! on disk answers only where the places are what they were when it
//! compiled. The pipeline workers reach their root signatures in an order
//! that differs from start to start, so a start after warm ones compiled
//! hundreds of compute pipelines with several static samplers again (3 to
//! 10 s each in the base fork's measurements; on this side the "cold" later
//! starts with compute pipelines of 8 to 14 s each). Under vkd3d-proton a
//! static sampler becomes an immutable sampler of the set layout, made when
//! the root signature is created, so the same order question arises there.
//!
//! Here every static sampler a pipeline has had is listed in a file beside
//! the pipeline library, in the order it was first met. As soon as the host
//! device exists (and at the latest before the first root signature with a
//! static sampler) a root signature of its own is made for each listed one,
//! in that order, and kept for the run (a place is the sampler's while a
//! root signature holds it). A sampler not listed yet is made the same way,
//! one at a time under one lock, before its pipeline's root signature, and
//! the file is written again with it at the end (temporary file + rename:
//! a start never reads half a file; damaged lines are skipped). So a
//! sampler's place is its line in the file, in every start.
//!
//! The first start with this compiles what the driver's cache holds under
//! other places once more; from the second on the cache answers.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;

/// The list, in the pipeline cache's folder: a line a sampler, thirteen
/// hexadecimal words ([`Key`]).
const FILE: &str = "static-samplers.txt";

/// The most samplers listed and made early. (The base fork measured that
/// the card's table holds 2,032 distinct ones; heap samplers share it.)
const MOST: usize = 1024;

/// A static sampler as the host is told of it, whole: filter, the three
/// address modes, the bias' bits, the anisotropy, the comparison, the border
/// colour, the bits of both level bounds, register, space and visibility.
type Key = [u32; 13];

fn key(s: &D3D12_STATIC_SAMPLER_DESC) -> Key {
    [
        s.Filter.0 as u32,
        s.AddressU.0 as u32,
        s.AddressV.0 as u32,
        s.AddressW.0 as u32,
        s.MipLODBias.to_bits(),
        s.MaxAnisotropy,
        s.ComparisonFunc.0 as u32,
        s.BorderColor.0 as u32,
        s.MinLOD.to_bits(),
        s.MaxLOD.to_bits(),
        s.ShaderRegister,
        s.RegisterSpace,
        s.ShaderVisibility.0 as u32,
    ]
}

fn description(k: &Key) -> D3D12_STATIC_SAMPLER_DESC {
    D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER(k[0] as i32),
        AddressU: D3D12_TEXTURE_ADDRESS_MODE(k[1] as i32),
        AddressV: D3D12_TEXTURE_ADDRESS_MODE(k[2] as i32),
        AddressW: D3D12_TEXTURE_ADDRESS_MODE(k[3] as i32),
        MipLODBias: f32::from_bits(k[4]),
        MaxAnisotropy: k[5],
        ComparisonFunc: D3D12_COMPARISON_FUNC(k[6] as i32),
        BorderColor: D3D12_STATIC_BORDER_COLOR(k[7] as i32),
        MinLOD: f32::from_bits(k[8]),
        MaxLOD: f32::from_bits(k[9]),
        ShaderRegister: k[10],
        RegisterSpace: k[11],
        ShaderVisibility: D3D12_SHADER_VISIBILITY(k[12] as i32),
    }
}

fn line(k: &Key) -> String {
    k.iter().map(|w| format!("{w:x}")).collect::<Vec<_>>().join(" ")
}

/// A line of the file, or nothing for one that is not thirteen words.
fn parse(line: &str) -> Option<Key> {
    let mut k = [0u32; 13];
    let mut words = line.split_ascii_whitespace();
    for w in &mut k {
        *w = u32::from_str_radix(words.next()?, 16).ok()?;
    }
    words.next().is_none().then_some(k)
}

/// The samplers in their kept order.
#[derive(Default)]
struct Listed {
    order: Vec<Key>,
    known: HashSet<Key>,
}

impl Listed {
    /// The lines of the file, each sampler once, at most [`MOST`]; bytes
    /// that are not text and lines that are not whole are left out.
    fn of(bytes: &[u8]) -> Listed {
        let mut listed = Listed::default();
        for k in String::from_utf8_lossy(bytes).lines().filter_map(parse) {
            if listed.order.len() >= MOST {
                break;
            }
            listed.place(k);
        }
        listed
    }

    /// The place `k` takes at the end if it has none yet (and the list has
    /// room).
    fn place(&mut self, k: Key) -> Option<usize> {
        if self.order.len() >= MOST || !self.known.insert(k) {
            return None;
        }
        self.order.push(k);
        Some(self.order.len() - 1)
    }

    /// The file's text for this list.
    fn text(&self) -> String {
        self.order.iter().map(|k| line(k) + "\n").collect()
    }
}

/// A root signature kept so that its sampler keeps its place.
struct Kept(#[allow(dead_code)] ID3D12RootSignature);
// SAFETY: D3D12 objects are free-threaded; this one is only held.
unsafe impl Send for Kept {}

struct Order {
    listed: Listed,
    kept: Vec<Kept>,
    file: Option<PathBuf>,
    /// Samplers met with the list full: counted, not made early.
    over: u64,
}

static ORDER: Mutex<Option<Order>> = Mutex::new(None);

/// Samplers added to the list in this run.
pub(crate) static NEW: AtomicU64 = AtomicU64::new(0);
/// Pipelines that took a second or longer to make in this run (with the
/// driver's cache answering, a handful).
pub(crate) static SLOW_NOW: AtomicU64 = AtomicU64::new(0);

/// Counts a pipeline that took `ms` to make.
pub(crate) fn note_time(ms: u128) {
    if ms >= 1000 {
        SLOW_NOW.fetch_add(1, Ordering::Relaxed);
    }
}

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SAMPLER_ORDER").is_some_and(|v| v == "1"))
}

/// A root signature that holds the one static sampler `k` and nothing else.
fn root_of(device: &ID3D12Device, k: &Key) -> windows::core::Result<ID3D12RootSignature> {
    let sampler = description(k);
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumStaticSamplers: 1,
                pStaticSamplers: &sampler,
                ..Default::default()
            },
        },
    };
    // SAFETY: the description points at `sampler`, alive through the call;
    // the blob is read in full before it is released.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        D3D12SerializeVersionedRootSignature(&desc, &mut blob, None)?;
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes = std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        device.CreateRootSignature(0, bytes)
    }
}

impl Order {
    /// The file's samplers, each made in its turn.
    fn begin(device: &ID3D12Device, file: Option<PathBuf>) -> Order {
        let began = std::time::Instant::now();
        let bytes = file.as_ref().and_then(|f| std::fs::read(f).ok()).unwrap_or_default();
        let mut order = Order { listed: Listed::of(&bytes), kept: Vec::new(), file, over: 0 };
        let mut refused = 0u32;
        for k in &order.listed.order {
            match root_of(device, k) {
                Ok(root) => order.kept.push(Kept(root)),
                Err(_) => refused += 1,
            }
        }
        tracing::info!(
            listed = order.listed.order.len(),
            refused,
            ms = began.elapsed().as_secs_f64() * 1e3,
            "static samplers made in their kept order (VICTORY_SAMPLER_ORDER=1)"
        );
        order
    }

    /// `k`, met for the first time: made, kept, and the file written again
    /// with it at the end.
    fn add(&mut self, device: &ID3D12Device, k: Key) {
        let Some(place) = self.listed.place(k) else {
            if self.listed.order.len() >= MOST && self.listed.known.insert(k) {
                self.over += 1;
                if self.over == 1 {
                    tracing::warn!(most = MOST, "more static samplers than are listed: the rest take their places as they come");
                }
            }
            return;
        };
        match root_of(device, &k) {
            Ok(root) => self.kept.push(Kept(root)),
            Err(e) => tracing::warn!(error = %e, place, "a static sampler's own root signature was refused"),
        }
        if let Some(file) = &self.file {
            let written = file
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| super::library::write_atomically(file, self.listed.text().as_bytes()));
            if let Err(e) = written {
                tracing::warn!(error = %e, "the list of static samplers was not written");
            }
        }
        NEW.fetch_add(1, Ordering::Relaxed);
        tracing::info!(place, sampler = %line(&k), "a static sampler met for the first time takes the next place");
    }
}

/// The list in its place for this run, made with `device` if it is not yet
/// (never before the pipeline cache's folder is known: without it the list
/// could not be kept).
fn order<'a>(device: &ID3D12Device, held: &'a mut Option<Order>) -> Option<&'a mut Order> {
    if held.is_none() {
        let file = super::library::folder().map(|d| d.join(FILE));
        file.as_ref()?;
        *held = Some(Order::begin(device, file));
    }
    held.as_mut()
}

/// Called when the host device is made: the listed samplers take their
/// places before the title's own samplers can take any.
pub(crate) fn early(device: &ID3D12Device) {
    if on() {
        let mut held = ORDER.lock().unwrap_or_else(|e| e.into_inner());
        let _ = order(device, &mut held);
    }
}

/// Called before a root signature with the static samplers `samplers` is
/// created: the listed ones have their places by then, and each of these
/// that is new takes the next one.
pub(crate) fn before(device: &ID3D12Device, samplers: &[D3D12_STATIC_SAMPLER_DESC]) {
    if samplers.is_empty() || !on() {
        return;
    }
    let mut held = ORDER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(order) = order(device, &mut held) else { return };
    for s in samplers {
        let k = key(s);
        if !order.listed.known.contains(&k) {
            order.add(device, k);
        }
    }
}

/// [`before`] for a root signature serialised inside a shader of ours (its
/// static samplers read back from the bytes).
pub(crate) fn before_serialised(device: &ID3D12Device, bytes: &[u8]) {
    if !on() {
        return;
    }
    // SAFETY: `bytes` is a whole serialised root signature, alive through
    // the calls; the description is read while the deserializer lives.
    unsafe {
        let mut out: *mut core::ffi::c_void = std::ptr::null_mut();
        let made = D3D12CreateVersionedRootSignatureDeserializer(
            bytes.as_ptr().cast(),
            bytes.len(),
            &<ID3D12VersionedRootSignatureDeserializer as windows::core::Interface>::IID,
            &mut out,
        );
        if made.is_err() || out.is_null() {
            return;
        }
        let de = <ID3D12VersionedRootSignatureDeserializer as windows::core::Interface>::from_raw(out);
        let Ok(desc) = de.GetRootSignatureDescAtVersion(D3D_ROOT_SIGNATURE_VERSION_1_1) else { return };
        let d = &(*desc).Anonymous.Desc_1_1;
        if d.NumStaticSamplers > 0 && !d.pStaticSamplers.is_null() {
            let samplers = std::slice::from_raw_parts(d.pStaticSamplers, d.NumStaticSamplers as usize).to_vec();
            before(device, &samplers);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sampler(filter: i32, register: u32) -> D3D12_STATIC_SAMPLER_DESC {
        D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER(filter),
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_MIRROR,
            MipLODBias: -0.75,
            MaxAnisotropy: 8,
            ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
            BorderColor: D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE,
            MinLOD: 0.5,
            MaxLOD: f32::MAX,
            ShaderRegister: register,
            RegisterSpace: 3,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        }
    }

    /// One start as `before` runs it, without a device: the samplers made,
    /// in the order they are made, and the file it leaves.
    fn start(file: &[u8], met: &[Vec<Key>]) -> (Vec<Key>, String) {
        let mut listed = Listed::of(file);
        let mut made = listed.order.clone();
        for pipeline in met {
            for &k in pipeline {
                if !listed.known.contains(&k) && listed.place(k).is_some() {
                    made.push(k);
                }
            }
        }
        (made, listed.text())
    }

    #[test]
    fn two_starts_meeting_samplers_in_other_orders_give_them_the_same_places() {
        let s: Vec<Key> = (0..6).map(|n| key(&sampler(0x15, n))).collect();
        // The first start: three pipelines' samplers, as one worker order had it.
        let (first, file) = start(b"", &[vec![s[0], s[1]], vec![s[2]], vec![s[1], s[3]]]);
        assert_eq!(first, [s[0], s[1], s[2], s[3]]);
        // Later starts: the workers reach the pipelines in other orders.
        for met in [
            vec![vec![s[1], s[3]], vec![s[2]], vec![s[0], s[1]]],
            vec![vec![s[3]], vec![s[2], s[0]]],
            vec![],
        ] {
            let (made, again) = start(file.as_bytes(), &met);
            assert_eq!(made, first, "places of the listed samplers");
            assert_eq!(again, file, "the file is unchanged");
        }
        // A new sampler takes the next place, behind every listed one, in
        // whichever order the others were met.
        let (made, file2) = start(file.as_bytes(), &[vec![s[4], s[3]], vec![s[0]]]);
        assert_eq!(made, [s[0], s[1], s[2], s[3], s[4]]);
        let (made, _) = start(file2.as_bytes(), &[vec![s[5]], vec![s[4], s[2]]]);
        assert_eq!(made, [s[0], s[1], s[2], s[3], s[4], s[5]]);
    }

    #[test]
    fn a_sampler_comes_back_from_its_line_whole() {
        let s = sampler(0x15, 4);
        let k = key(&s);
        assert_eq!(parse(&line(&k)), Some(k));
        let back = description(&k);
        assert_eq!(key(&back), k);
        assert_eq!(back.MipLODBias, -0.75);
        assert_eq!(back.MaxLOD, f32::MAX);
        assert_eq!(back.ShaderVisibility, D3D12_SHADER_VISIBILITY_PIXEL);
    }

    #[test]
    fn lines_cut_short_or_damaged_are_left_out() {
        let k = key(&sampler(0, 0));
        let whole = line(&k);
        assert_ne!(parse(&whole[..whole.len() - 3]), Some(k));
        assert_eq!(parse("1 2 3"), None);
        assert_eq!(parse(&format!("{whole} 7")), None);
        assert_eq!(parse("zz 0 0 0 0 0 0 0 0 0 0 0 0"), None);
        assert_eq!(parse(""), None);
        // A damaged file: bytes that are not text, a cut line, a line twice.
        let b = key(&sampler(0x15, 1));
        let mut file = format!("{whole}\n").into_bytes();
        file.extend_from_slice(&[0xff, 0xfe, 0x00, b'\n']);
        file.extend_from_slice(format!("{}\n{whole}\n1 2", line(&b)).as_bytes());
        assert_eq!(Listed::of(&file).order, [k, b]);
    }

    #[test]
    fn places_are_the_order_of_first_meeting_and_never_change() {
        let (a, b, c) = (key(&sampler(0, 0)), key(&sampler(0x15, 0)), key(&sampler(0x15, 1)));
        let mut listed = Listed::default();
        assert_eq!(listed.place(a), Some(0));
        assert_eq!(listed.place(b), Some(1));
        assert_eq!(listed.place(a), None);
        // The same sampler at another register is one of its own.
        assert_eq!(listed.place(c), Some(2));
        let again = Listed::of(format!("{}{}\n1 2\n", listed.text(), line(&a)).as_bytes());
        assert_eq!(again.order, [a, b, c]);
        let mut again = again;
        assert_eq!(again.place(b), None);
        assert_eq!(again.place(key(&sampler(1, 2))), Some(3));
    }

    #[test]
    fn the_list_takes_no_more_than_its_most() {
        let mut listed = Listed::default();
        for n in 0..MOST as u32 + 5 {
            listed.place(key(&sampler(0, n)));
        }
        assert_eq!(listed.order.len(), MOST);
        let text: String = (0..MOST as u32 + 5).map(|n| line(&key(&sampler(0, n))) + "\n").collect();
        assert_eq!(Listed::of(text.as_bytes()).order.len(), MOST);
    }
}
