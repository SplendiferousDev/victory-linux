//! A least amount of indirect diffuse light, for titles whose indirect light
//! comes from ray-traced passes that do not run here yet. The pass is from
//! VIctory-Belobug-AMD (GPL-3.0-or-later), as ported to VItoriaBr; here it
//! names nothing of any title.
//!
//! The user names the compute program that resolves the indirect light
//! (`VICTORY_AMBIENT_SHADER`). Right after each dispatch of it, the image it
//! wrote is filled with a constant, shaded by occlusion worked out from the
//! scene's depth. The title's own lighting then applies it with the
//! surfaces' colours. Nothing runs unless a program is named.
//!
//! Which of the program's views are the light and the depth is found from
//! what they are: the light is the one image it writes as 32-bit integers
//! (shared-exponent RGB, `R9G9B9E5`), the depth the one depth-with-stencil
//! image (reversed 32-bit float depth) it reads of the same size.
//! `VICTORY_AMBIENT_LIGHT` and `VICTORY_AMBIENT_DEPTH` pin either one where
//! that is ambiguous.
//!
//! Environment:
//! - `VICTORY_AMBIENT_SHADER=<id>[,<id>...]`: the programs, each by the start
//!   of its hash as the log's `shader translated` lines print it, or as
//!   `name:<text>` by text in its container.
//! - `VICTORY_AMBIENT=<level>` (0.6 by default; 0 for none), or `off`, which
//!   turns the whole feature off.
//! - `VICTORY_AMBIENT_LIGHT=<root parameter>:<dword offset in its table>`,
//!   `VICTORY_AMBIENT_DEPTH=<root parameter>:<dword offset>`: the views to use.
//! - `VICTORY_AMBIENT_TINT=r,g,b`: the colour (warm white by default).
//! - `VICTORY_AMBIENT_SHADE=s,r,n,t`: occlusion strength (0 for none), radius
//!   looked at in metres, near plane distance, tangent of half the vertical
//!   field of view.
//! - `VICTORY_AMBIENT_SCAN=1`: logs each compute program that looks like
//!   such a pass (an integer image written, a depth image of its size read),
//!   once, with its hash and views.

use std::sync::OnceLock;

/// Where a view sits: the title's root parameter and the dword offset in
/// its table.
pub(crate) type Place = (u8, u32);

/// A program named by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Id {
    /// The start of its hash, lower case.
    Hash(String),
    /// Bytes its container holds.
    Name(Vec<u8>),
}

/// What the user set.
#[derive(Debug, Clone, PartialEq)]
struct Config {
    ids: Vec<Id>,
    level: f32,
    light: Option<Place>,
    depth: Option<Place>,
    tint: [f32; 3],
    shade: [f32; 4],
}

const LEVEL: f32 = 0.6;
const TINT: [f32; 3] = [1.0, 0.98, 0.94];
const SHADE: [f32; 4] = [1.0, 0.6, 0.1, 0.45];

fn parse_ids(set: &str) -> Vec<Id> {
    set.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s.strip_prefix("name:") {
            Some(name) => Id::Name(name.as_bytes().to_vec()),
            None => Id::Hash(s.to_ascii_lowercase()),
        })
        .filter(|id| !matches!(id, Id::Name(n) if n.is_empty()))
        .collect()
}

/// `<root parameter>:<dword offset>`.
fn parse_place(set: &str) -> Option<Place> {
    let (slot, offset) = set.trim().split_once(':')?;
    Some((slot.trim().parse().ok()?, offset.trim().parse().ok()?))
}

/// The level: `None` for `off`, the default when unset or not a level.
fn parse_level(set: Option<&str>) -> Option<f32> {
    let Some(set) = set.map(str::trim) else {
        return Some(LEVEL);
    };
    if set.eq_ignore_ascii_case("off") {
        return None;
    }
    Some(
        set.parse()
            .ok()
            .filter(|l: &f32| l.is_finite() && *l >= 0.0)
            .unwrap_or(LEVEL),
    )
}

/// Up to `N` numbers from `set`, the defaults for the rest.
fn parse_floats<const N: usize>(set: Option<&str>, mut to: [f32; N]) -> [f32; N] {
    for (to, v) in to.iter_mut().zip(set.unwrap_or_default().split(',')) {
        if let Ok(v) = v.trim().parse::<f32>()
            && v.is_finite()
        {
            *to = v;
        }
    }
    to
}

/// The configuration from `env`; why there is none otherwise (`None`: none
/// of its variables set, nothing to say).
fn configure(env: impl Fn(&str) -> Option<String>) -> Result<Config, Option<&'static str>> {
    let ids = env("VICTORY_AMBIENT_SHADER").map(|s| parse_ids(&s));
    let level = env("VICTORY_AMBIENT");
    let Some(level) = parse_level(level.as_deref()) else {
        return Err(ids.is_some().then_some("VICTORY_AMBIENT=off"));
    };
    let Some(ids) = ids.filter(|ids| !ids.is_empty()) else {
        let set = [
            "VICTORY_AMBIENT",
            "VICTORY_AMBIENT_SHADER",
            "VICTORY_AMBIENT_LIGHT",
        ]
        .iter()
        .any(|k| env(k).is_some());
        return Err(set.then_some("no program named: set VICTORY_AMBIENT_SHADER (VICTORY_AMBIENT_SCAN=1 lists candidates)"));
    };
    Ok(Config {
        ids,
        level,
        light: env("VICTORY_AMBIENT_LIGHT").and_then(|s| parse_place(&s)),
        depth: env("VICTORY_AMBIENT_DEPTH").and_then(|s| parse_place(&s)),
        tint: parse_floats(env("VICTORY_AMBIENT_TINT").as_deref(), TINT),
        shade: parse_floats(env("VICTORY_AMBIENT_SHADE").as_deref(), SHADE),
    })
}

fn config() -> Option<&'static Config> {
    static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
    CONFIG
        .get_or_init(|| match configure(|k| std::env::var(k).ok()) {
            Ok(c) => {
                tracing::info!(
                    programs = ?c.ids.iter().map(|id| match id {
                        Id::Hash(h) => h.clone(),
                        Id::Name(n) => format!("name:{}", String::from_utf8_lossy(n)),
                    }).collect::<Vec<_>>(),
                    level = c.level,
                    light = ?c.light,
                    depth = ?c.depth,
                    "ambient light: on for these programs"
                );
                Some(c)
            }
            Err(why) => {
                if let Some(why) = why {
                    tracing::info!("ambient light off: {why}");
                }
                None
            }
        })
        .as_ref()
}

fn scan() -> bool {
    static SCAN: OnceLock<bool> = OnceLock::new();
    *SCAN.get_or_init(|| std::env::var("VICTORY_AMBIENT_SCAN").is_ok_and(|v| v.trim() == "1"))
}

/// Whether the user named the program with this hash and container.
pub(crate) fn named(hash: &str, container: &[u8]) -> bool {
    config().is_some_and(|c| matches(&c.ids, hash, container))
}

fn matches(ids: &[Id], hash: &str, container: &[u8]) -> bool {
    ids.iter().any(|id| match id {
        Id::Hash(h) => hash.starts_with(h.as_str()),
        Id::Name(n) => container.windows(n.len()).any(|w| w == n.as_slice()),
    })
}

/// Whether a dispatch of a program is worth looking at after it runs: one
/// that was named, or any one not yet looked at while scanning.
pub(crate) fn wanted(named: bool, hash: &str) -> bool {
    named || (scan() && first_time(hash, "scan"))
}

/// Once per program and purpose.
fn first_time(hash: &str, what: &'static str) -> bool {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<(String, &'static str)>>> =
        std::sync::Mutex::new(None);
    SEEN.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert((hash.to_owned(), what))
}

/// What a view of the program shows, as far as the pass cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A 2D image of 32-bit integers.
    Integer,
    /// A 2D depth-with-stencil image (32-bit float depth).
    Depth,
    Other,
}

/// A view a program reads (`uav` false) or writes, with what it shows and
/// its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bound {
    pub(crate) uav: bool,
    pub(crate) at: Place,
    pub(crate) kind: Kind,
    pub(crate) size: (u64, u32),
}

/// Why a program's views give no light to replace.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Why {
    /// It writes no 32-bit-integer image.
    NoLight,
    /// The pinned place holds no written 32-bit-integer image.
    NotLight(Place),
    /// Several: these.
    Ambiguous(Vec<Place>),
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Why::NoLight => write!(f, "it writes no 32-bit-integer image"),
            Why::NotLight((s, o)) => {
                write!(
                    f,
                    "VICTORY_AMBIENT_LIGHT={s}:{o} is not a 32-bit-integer image it writes"
                )
            }
            Why::Ambiguous(places) => {
                write!(
                    f,
                    "it writes several 32-bit-integer images; pin one with VICTORY_AMBIENT_LIGHT="
                )?;
                let list: Vec<String> = places.iter().map(|(s, o)| format!("{s}:{o}")).collect();
                write!(f, "{}", list.join(" or "))
            }
        }
    }
}

/// The light to replace and the depth to shade it with, among `bound`, or
/// at the places pinned.
fn pick(
    bound: &[Bound],
    light: Option<Place>,
    depth: Option<Place>,
) -> Result<(Bound, Option<Bound>), Why> {
    let depths = |size: (u64, u32)| {
        bound
            .iter()
            .filter(move |b| !b.uav && b.kind == Kind::Depth && b.size == size)
    };
    let light = if let Some(at) = light {
        *bound
            .iter()
            .find(|b| b.uav && b.at == at && b.kind == Kind::Integer)
            .ok_or(Why::NotLight(at))?
    } else {
        let all: Vec<&Bound> = bound
            .iter()
            .filter(|b| b.uav && b.kind == Kind::Integer)
            .collect();
        let with_depth: Vec<&Bound> = all
            .iter()
            .copied()
            .filter(|b| depths(b.size).next().is_some())
            .collect();
        match (&all[..], &with_depth[..]) {
            ([], _) => return Err(Why::NoLight),
            ([one], _) | (_, [one]) => **one,
            _ => return Err(Why::Ambiguous(all.iter().map(|b| b.at).collect())),
        }
    };
    let depth = match depth {
        Some(at) => depths(light.size).find(|b| b.at == at).copied(),
        None => {
            let mut found = depths(light.size);
            match (found.next(), found.next()) {
                (Some(one), None) => Some(*one),
                _ => None,
            }
        }
    };
    Ok((light, depth))
}

/// Whether a program with these views looks like a light resolve: it
/// writes a 32-bit-integer image and reads a depth image of its size.
fn looks_like(bound: &[Bound]) -> bool {
    bound.iter().any(|l| {
        l.uav
            && l.kind == Kind::Integer
            && bound
                .iter()
                .any(|d| !d.uav && d.kind == Kind::Depth && d.size == l.size)
    })
}

fn places(bound: &[Bound], uav: bool, kind: Kind) -> String {
    bound
        .iter()
        .filter(|b| b.uav == uav && b.kind == kind)
        .map(|b| format!("{}:{} ({}x{})", b.at.0, b.at.1, b.size.0, b.size.1))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) use pass::after;

mod pass {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::s;

    use super::{Bound, Kind, Place, config, first_time, looks_like, pick, places};
    use crate::host::{HostList, host};
    use crate::object::This;

    /// What `this`, a title resource, shows: its kind and size, if its
    /// host copy is made.
    fn classify(this: This) -> Option<(Kind, (u64, u32))> {
        let t = crate::resource::host_made(this)?;
        // SAFETY: a live resource.
        let d = unsafe { t.GetDesc() };
        if d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D {
            return Some((Kind::Other, (d.Width, d.Height)));
        }
        let kind = match d.Format {
            DXGI_FORMAT_R32_UINT => Kind::Integer,
            DXGI_FORMAT_R32G8X24_TYPELESS => Kind::Depth,
            _ => Kind::Other,
        };
        Some((kind, (d.Width, d.Height)))
    }

    /// After a dispatch of the program with `hash` (`named` by the user, else
    /// looked at for the scan), whose views `views` lists as (written,
    /// place, title resource): the light raised, for a named one. Whether it
    /// recorded anything (the list's bindings are then ours).
    pub(crate) fn after(
        list: &HostList,
        hash: &str,
        named: bool,
        views: &[(bool, Place, This)],
    ) -> bool {
        let bound: Vec<Bound> = views
            .iter()
            .filter_map(|&(uav, at, this)| {
                let (kind, size) = classify(this)?;
                Some(Bound {
                    uav,
                    at,
                    kind,
                    size,
                })
            })
            .collect();
        if !named {
            if looks_like(&bound) {
                tracing::info!(
                    hash,
                    writes = places(&bound, true, Kind::Integer),
                    depth = places(&bound, false, Kind::Depth),
                    "ambient light: a candidate program (VICTORY_AMBIENT_SHADER=<its hash>)"
                );
            }
            return false;
        }
        let Some(c) = config() else {
            return false;
        };
        let (light, depth) = match pick(&bound, c.light, c.depth) {
            Ok(picked) => picked,
            Err(why) => {
                if first_time(hash, "no") {
                    tracing::warn!(
                        hash,
                        writes = places(&bound, true, Kind::Integer),
                        depth = places(&bound, false, Kind::Depth),
                        "no ambient light after this program: {why}"
                    );
                }
                return false;
            }
        };
        let this = |b: &Bound| {
            views
                .iter()
                .find(|v| (v.0, v.1) == (b.uav, b.at))
                .map(|v| v.2)
        };
        let (Some(light_this), depth_this) = (this(&light), depth.as_ref().and_then(this)) else {
            return false;
        };
        let raised = raise(list, c, light_this, depth_this);
        if raised && first_time(hash, "on") {
            tracing::info!(
                hash,
                light = format_args!("{}:{}", light.at.0, light.at.1),
                depth = ?depth.map(|d| format_args!("{}:{}", d.at.0, d.at.1).to_string()),
                width = light.size.0,
                height = light.size.1,
                level = c.level,
                "ambient light engaged"
            );
        }
        raised
    }

    const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=8, b0), DescriptorTable(UAV(u0), SRV(t0))"
cbuffer Floor : register(b0) {
    float level; float3 tint;
    float strength; float radius; float near; float tan_y;
};
RWTexture2D<uint> light : register(u0);
Texture2D<float> depth : register(t0);
float3 seen(int2 p, float2 size) {
    float d = depth.Load(int3(clamp(p, int2(0, 0), int2(size) - 1), 0));
    float z = near / max(d, 1e-7);
    float2 ndc = (float2(p) + 0.5) / size * 2.0 - 1.0;
    return float3(ndc.x * tan_y * size.x / size.y * z, -ndc.y * tan_y * z, z);
}
float open_to_light(int2 p, float2 size) {
    float3 c = seen(p, size);
    if (c.z > 400.0) return 1.0;
    float3 l = seen(p + int2(-1, 0), size), r = seen(p + int2(1, 0), size);
    float3 u = seen(p + int2(0, -1), size), d = seen(p + int2(0, 1), size);
    float3 dx = abs(l.z - c.z) < abs(r.z - c.z) ? c - l : r - c;
    float3 dy = abs(u.z - c.z) < abs(d.z - c.z) ? c - u : d - c;
    float3 n = normalize(cross(dy, dx));
    if (dot(n, c) > 0.0) n = -n;
    float px = radius * size.y / (2.0 * tan_y * c.z);
    if (px < 1.5) return 1.0;
    px = min(px, 128.0);
    float noise = frac(52.9829189 * frac(dot(float2(p), float2(0.06711056, 0.00583715))));
    float sum = 0.0;
    const int N = 16;
    for (int i = 0; i < N; i++) {
        float a = i * 2.399963 + noise * 6.2831853;
        float2 o = float2(cos(a), sin(a)) * (sqrt((i + 0.5) / N) * px);
        float3 v = seen(p + int2(round(o)), size) - c;
        float vv = dot(v, v);
        float over = dot(v, n) - 0.004 * c.z - 0.005;
        sum += max(over, 0.0) / sqrt(vv + 1e-6) * saturate(1.0 - vv / (radius * radius));
    }
    return saturate(1.0 - strength * 2.0 * sum / N);
}
uint pack(float3 c) {
    c = clamp(c, 0.0, 65408.0);
    float most = max(c.r, max(c.g, c.b));
    int e = clamp(int(floor(log2(max(most, 1e-9)))) + 16, 0, 31);
    float scale = exp2(float(24 - e));
    if (most * scale >= 511.5) { e = min(e + 1, 31); scale = exp2(float(24 - e)); }
    uint3 m = min(uint3(c * scale + 0.5), 511u);
    return m.r | (m.g << 9) | (m.b << 18) | (uint(e) << 27);
}
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    light.GetDimensions(w, h);
    if (at.x >= w || at.y >= h) return;
    float open = strength > 0.0 ? open_to_light(int2(at.xy), float2(w, h)) : 1.0;
    light[at.xy] = pack(tint * level * open);
}
"#;

    /// Views, used in turn: more than the passes of every list in flight.
    const SLOTS: u32 = 1024;

    pub(super) struct Pass {
        root: ID3D12RootSignature,
        pso: ID3D12PipelineState,
        views: ID3D12DescriptorHeap,
        step: u32,
        next: AtomicU32,
    }
    // SAFETY: D3D12 objects are free-threaded.
    unsafe impl Send for Pass {}
    // SAFETY: as above.
    unsafe impl Sync for Pass {}

    pub(super) fn make(device: &ID3D12Device) -> windows::core::Result<Pass> {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; out pointers ours.
        let compiled = unsafe {
            D3DCompile(
                SOURCE.as_ptr().cast(),
                SOURCE.len(),
                s!("ambient"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        let text = |b: &ID3DBlob| {
            // SAFETY: the blob's own bytes.
            unsafe {
                std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())
            }
            .to_vec()
        };
        if let Err(e) = compiled {
            if let Some(errors) = errors {
                tracing::warn!("ambient light: {}", String::from_utf8_lossy(&text(&errors)));
            }
            return Err(e);
        }
        let code = text(&code.ok_or_else(windows::core::Error::empty)?);
        // SAFETY: the root signature is inside the shader; plain creations.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &code)?;
            let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            })?;
            let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
            let views = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: SLOTS * 2,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })?;
            Ok(Pass {
                root,
                pso,
                views,
                step: device.GetDescriptorHandleIncrementSize(kind),
                next: AtomicU32::new(0),
            })
        }
    }

    fn uav_barrier(list: &ID3D12GraphicsCommandList) {
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            ..Default::default()
        };
        // SAFETY: an open list; the barrier names no resource.
        unsafe { list.ResourceBarrier(&[barrier]) };
    }

    /// Fills `light` (an R32_UINT image the program before wrote) with the
    /// floor, shaded by `depth` if given. Whether it recorded anything.
    fn raise(list: &HostList, c: &super::Config, light: This, depth: Option<This>) -> bool {
        let Some(host) = host() else {
            return false;
        };
        let Some(light) = crate::resource::host_made(light) else {
            return false;
        };
        static PASS: OnceLock<Option<Pass>> = OnceLock::new();
        let Some(pass) = PASS
            .get_or_init(|| {
                make(&host.device)
                    .inspect_err(
                        |e| tracing::warn!(error = %e, "no ambient light: its pass does not build"),
                    )
                    .ok()
            })
            .as_ref()
        else {
            return false;
        };
        // SAFETY: a live resource.
        let d = unsafe { light.GetDesc() };
        let slot = pass.next.fetch_add(1, Ordering::Relaxed) % SLOTS * 2;
        list.keep(light);
        let depth = depth.and_then(crate::resource::host_made);
        if let Some(depth) = depth {
            list.keep(depth);
        }
        let l = &list.list;
        uav_barrier(l);
        // SAFETY: an open list; a live texture the list keeps, in the
        // unordered-access state as the program before left it; the depth
        // in the state that program read it in; views in a slot no list in
        // flight reads.
        unsafe {
            let cpu = pass.views.GetCPUDescriptorHandleForHeapStart();
            let gpu = pass.views.GetGPUDescriptorHandleForHeapStart();
            let at = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: cpu.ptr + (slot * pass.step) as usize,
            };
            let view = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_UINT,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            host.device
                .CreateUnorderedAccessView(light, None::<&ID3D12Resource>, Some(&view), at);
            let seen = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            };
            let beside = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: at.ptr + pass.step as usize,
            };
            host.device
                .CreateShaderResourceView(depth, Some(&seen), beside);
            l.SetDescriptorHeaps(&[Some(pass.views.clone())]);
            l.SetComputeRootSignature(&pass.root);
            l.SetPipelineState(&pass.pso);
            let [r, g, b] = c.tint;
            let [strength, radius, near, tan_y] = c.shade;
            let strength = if depth.is_some() { strength } else { 0.0 };
            let constants = [c.level, r, g, b, strength, radius, near, tan_y];
            l.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
            l.SetComputeRootDescriptorTable(
                1,
                D3D12_GPU_DESCRIPTOR_HANDLE {
                    ptr: gpu.ptr + u64::from(slot * pass.step),
                },
            );
            l.Dispatch((d.Width as u32).div_ceil(8), d.Height.div_ceil(8), 1);
        }
        uav_barrier(l);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(set: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            set.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn off_unless_a_program_is_named() {
        assert_eq!(configure(env(&[])), Err(None));
        assert!(matches!(
            configure(env(&[("VICTORY_AMBIENT", "0.5")])),
            Err(Some(_))
        ));
        assert!(matches!(
            configure(env(&[("VICTORY_AMBIENT_SHADER", " , ")])),
            Err(Some(_))
        ));
        let on = configure(env(&[("VICTORY_AMBIENT_SHADER", "00ff")])).unwrap();
        assert_eq!(on.level, LEVEL);
        assert_eq!(
            (on.light, on.depth, on.tint, on.shade),
            (None, None, TINT, SHADE)
        );
        assert_eq!(
            configure(env(&[
                ("VICTORY_AMBIENT_SHADER", "00ff"),
                ("VICTORY_AMBIENT", "OFF")
            ])),
            Err(Some("VICTORY_AMBIENT=off"))
        );
    }

    #[test]
    fn reads_every_setting() {
        let c = configure(env(&[
            ("VICTORY_AMBIENT_SHADER", "ABcd12, name:Resolve,name:"),
            ("VICTORY_AMBIENT", "0.25"),
            ("VICTORY_AMBIENT_LIGHT", "1:20"),
            ("VICTORY_AMBIENT_DEPTH", " 0 : 8 "),
            ("VICTORY_AMBIENT_TINT", "1,0.5"),
            ("VICTORY_AMBIENT_SHADE", "0,x,2,nan"),
        ]))
        .unwrap();
        assert_eq!(
            c.ids,
            [Id::Hash("abcd12".into()), Id::Name(b"Resolve".to_vec())]
        );
        assert_eq!(c.level, 0.25);
        assert_eq!((c.light, c.depth), (Some((1, 20)), Some((0, 8))));
        assert_eq!(c.tint, [1.0, 0.5, TINT[2]]);
        assert_eq!(c.shade, [0.0, SHADE[1], 2.0, SHADE[3]]);
        assert_eq!(parse_level(Some("-1")), Some(LEVEL));
        assert_eq!(parse_level(Some("0")), Some(0.0));
        assert_eq!(parse_place("1"), None);
        assert_eq!(parse_place("300:1"), None);
    }

    #[test]
    fn programs_match_by_hash_start_or_text() {
        let ids = parse_ids("abcd,name:pass_b");
        assert!(matches(&ids, "abcdef0123456789", b""));
        assert!(!matches(&ids, "0abcdef123456789", b"pass_a"));
        assert!(matches(&ids, "0000000000000000", b"..pass_b.."));
    }

    fn b(uav: bool, at: Place, kind: Kind, size: (u64, u32)) -> Bound {
        Bound {
            uav,
            at,
            kind,
            size,
        }
    }

    #[test]
    fn picks_the_one_integer_image_and_its_depth() {
        let light = b(true, (1, 4), Kind::Integer, (64, 32));
        let depth = b(false, (0, 8), Kind::Depth, (64, 32));
        let bound = [
            b(false, (0, 0), Kind::Integer, (64, 32)),
            b(false, (0, 4), Kind::Depth, (32, 16)),
            depth,
            light,
            b(true, (1, 0), Kind::Other, (64, 32)),
        ];
        assert_eq!(pick(&bound, None, None), Ok((light, Some(depth))));
        assert!(looks_like(&bound));
        // Pinned depth of another size: no shading.
        assert_eq!(pick(&bound, None, Some((0, 4))), Ok((light, None)));
        // No depth of its size: the light alone.
        assert_eq!(pick(&bound[3..], None, None), Ok((light, None)));
        assert!(!looks_like(&bound[3..]));
        assert_eq!(pick(&bound[4..], None, None), Err(Why::NoLight));
    }

    #[test]
    fn several_integer_images_need_a_depth_or_a_pin() {
        let a = b(true, (1, 4), Kind::Integer, (64, 32));
        let c = b(true, (1, 8), Kind::Integer, (8, 8));
        let depth = b(false, (0, 0), Kind::Depth, (64, 32));
        assert_eq!(pick(&[a, c, depth], None, None), Ok((a, Some(depth))));
        let both = [a, b(true, (1, 12), Kind::Integer, (64, 32)), depth];
        assert_eq!(
            pick(&both, None, None),
            Err(Why::Ambiguous(vec![(1, 4), (1, 12)]))
        );
        assert_eq!(pick(&both, Some((1, 12)), None), Ok((both[1], Some(depth))));
        assert_eq!(pick(&both, Some((0, 0)), None), Err(Why::NotLight((0, 0))));
        // Two depths of its size: no shading unless one is pinned.
        let two = [a, depth, b(false, (0, 4), Kind::Depth, (64, 32))];
        assert_eq!(pick(&two, None, None), Ok((a, None)));
        assert_eq!(pick(&two, None, Some((0, 4))), Ok((a, Some(two[2]))));
    }

    /// The pass compiles with the system's compiler and the host takes it.
    #[test]
    fn ambient_pass_builds() {
        let Some(host) = crate::host::host() else {
            return;
        };
        pass::make(&host.device).expect("the ambient pass");
    }
}
