//! A present's second plane, drawn over the first in our window.
//!
//! The console's display composes the planes of a present. The title in
//! its Series S mode presents two: its game image at 1920x1080 and its
//! interface at 3840x2160, whose empty texels are zero with alpha zero. A
//! window showing the first plane alone lacks the interface, the menus and
//! loading screens included.
//!
//! The pass here draws the second plane over the back buffer once the first
//! has been copied there: one triangle, the plane sampled across the whole
//! buffer (stretched to it), blended by its alpha.
//!
//! Hand-ported from ScarlettPort's vendored runtime (`crates/victory-gpu/src/over.rs`,
//! its plane half; GPL-3.0-or-later). The root signature is the shader's
//! own, as for the other passes of ours (`crate::host::compile`).
//!
//! shortcut: the alpha is taken as premultiplied (what an interface drawn
//! onto a cleared target with the usual blending holds); planes after the
//! second, and the placement and scaling a plane's parameters can ask for,
//! are not read.

use std::sync::Mutex;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

const CORNERS: &str = r#"
struct Corner { float4 position : SV_Position; float2 at : TEXCOORD0; };
Corner corner(uint i : SV_VertexID) {
    Corner c;
    c.at = float2(float(i & 1) * 2.0, float(i >> 1) * 2.0);
    c.position = float4(c.at.x * 2.0 - 1.0, 1.0 - c.at.y * 2.0, 0.0, 1.0);
    return c;
}
"#;

const OVER: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL), StaticSampler(s0, filter = FILTER_MIN_MAG_LINEAR_MIP_POINT, addressU = TEXTURE_ADDRESS_CLAMP, addressV = TEXTURE_ADDRESS_CLAMP, addressW = TEXTURE_ADDRESS_CLAMP, visibility = SHADER_VISIBILITY_PIXEL)"
Texture2D<float4> plane : register(t0);
SamplerState bilinear : register(s0);
struct Corner { float4 position : SV_Position; float2 at : TEXCOORD0; };
[RootSignature(RS)]
float4 over(Corner c) : SV_Target { return plane.Sample(bilinear, c.at); }
"#;

/// Views kept until the list that uses them has run: a present can be sent
/// ahead of the card, so a view is not reused for this many presents.
///
/// shortcut: a fixed count, not derived from how many presents are in
/// flight. The swap chain has two buffers and a present waits for one, so
/// no more than a few lists are ever waiting; 16 is well past that.
const RING: u32 = 16;

struct Pass {
    device: usize,
    format: i32,
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    targets: ID3D12DescriptorHeap,
    views: ID3D12DescriptorHeap,
    target_step: usize,
    view_step: u32,
    next: u32,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}

static PASS: Mutex<Option<Pass>> = Mutex::new(None);

fn make(device: &ID3D12Device, format: DXGI_FORMAT) -> windows::core::Result<Pass> {
    use windows::core::{Interface, s};
    let vs = crate::host::compile(CORNERS, s!("corner"), s!("vs_5_1"))?;
    let ps = crate::host::compile(OVER, s!("over"), s!("ps_5_1"))?;
    let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
        pShaderBytecode: bytes.as_ptr().cast(),
        BytecodeLength: bytes.len(),
    };
    // SAFETY: the root signature is inside the pixel shader; the
    // description points at live data and never releases its pointer.
    unsafe {
        crate::pipeline::sampler_order::before_serialised(device, &ps);
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &ps)?;
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            VS: code(&vs[..]),
            PS: code(&ps[..]),
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        desc.RTVFormats[0] = format;
        desc.BlendState.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
            ..Default::default()
        };
        let pipeline: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc)?;
        let targets: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            NumDescriptors: RING,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        })?;
        let views: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: RING,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })?;
        Ok(Pass {
            device: device.as_raw() as usize,
            format: format.0,
            root,
            pipeline,
            target_step: device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize,
            view_step: device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
            targets,
            views,
            next: 0,
        })
    }
}

/// Records into `list` the draw of `plane` (readable by pixel shaders) over
/// `back` (in the render-target state, `width` by `height`, of `format`).
/// False if the pass could not be made: nothing was recorded then.
///
/// The pass sets the list's descriptor heap, root signature, pipeline,
/// target, viewport, scissor and topology and puts none of them back: it is
/// for a list of ours that records nothing of the title's.
pub(crate) fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    back: &ID3D12Resource,
    format: DXGI_FORMAT,
    (width, height): (u32, u32),
    plane: &ID3D12Resource,
) -> bool {
    use windows::core::Interface;
    let mut pass = PASS.lock().unwrap_or_else(|e| e.into_inner());
    if pass.as_ref().is_none_or(|p| p.format != format.0 || p.device != device.as_raw() as usize) {
        *pass = match make(device, format) {
            Ok(p) => {
                tracing::info!(format = format.0, "a present's second plane is drawn over the first");
                Some(p)
            }
            Err(e) => {
                static TOLD: std::sync::Once = std::sync::Once::new();
                TOLD.call_once(|| {
                    tracing::warn!(error = %e, "no pass to draw a present's second plane: it is not shown");
                });
                return false;
            }
        };
    }
    let Some(p) = pass.as_mut() else { return false };
    let slot = p.next % RING;
    p.next = p.next.wrapping_add(1);
    // SAFETY: a live resource.
    let plane_format = crate::stretch::typed(unsafe { plane.GetDesc() }.Format);
    let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: plane_format,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    let target_desc = D3D12_RENDER_TARGET_VIEW_DESC {
        Format: format,
        ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    // SAFETY: an open list; slots of our heaps; live resources and objects
    // (the pass's own live as long as the process or until remade, and a
    // list holds what it was recorded with).
    unsafe {
        let target = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: p.targets.GetCPUDescriptorHandleForHeapStart().ptr + slot as usize * p.target_step,
        };
        let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: p.views.GetCPUDescriptorHandleForHeapStart().ptr + (slot * p.view_step) as usize,
        };
        let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: p.views.GetGPUDescriptorHandleForHeapStart().ptr + u64::from(slot * p.view_step),
        };
        device.CreateRenderTargetView(back, Some(&target_desc), target);
        device.CreateShaderResourceView(plane, Some(&view), cpu);
        list.SetDescriptorHeaps(&[Some(p.views.clone())]);
        list.SetGraphicsRootSignature(&p.root);
        list.SetPipelineState(&p.pipeline);
        list.SetGraphicsRootDescriptorTable(0, gpu);
        list.OMSetRenderTargets(1, Some(&target), false, None);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]);
        list.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        }]);
        list.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4));
        list.DrawInstanced(3, 1, 0, 0);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pass's root signature is the pixel shader's own, as for our
    /// other passes; the vertex shader names none and needs none.
    #[test]
    fn the_root_signature_is_in_the_pixel_shader() {
        assert!(OVER.contains("[RootSignature(RS)]"));
        assert!(OVER.contains("StaticSampler(s0"));
        assert!(!CORNERS.contains("RootSignature"));
    }

    /// One triangle covers the target: its three corners, from the vertex
    /// ids as the shader computes them, are (-1, 1), (3, 1) and (-1, -3)
    /// in clip space, texture coordinates (0, 0), (2, 0), (0, 2).
    #[test]
    fn one_triangle_covers_the_target() {
        let corner = |i: u32| {
            let at = ((i & 1) as f32 * 2.0, (i >> 1) as f32 * 2.0);
            ((at.0 * 2.0 - 1.0, 1.0 - at.1 * 2.0), at)
        };
        assert_eq!(corner(0), ((-1.0, 1.0), (0.0, 0.0)));
        assert_eq!(corner(1), ((3.0, 1.0), (2.0, 0.0)));
        assert_eq!(corner(2), ((-1.0, -3.0), (0.0, 2.0)));
        assert!(CORNERS.contains("float(i & 1) * 2.0, float(i >> 1) * 2.0"));
        assert!(CORNERS.contains("c.at.x * 2.0 - 1.0, 1.0 - c.at.y * 2.0"));
    }

    /// On a device of the test's own (`crate::stretch`'s test rig): an 8x8
    /// plane over a 4x4 opaque blue buffer. Its left half is empty (zero),
    /// its right half opaque red at the top and half-covering red at the
    /// bottom (premultiplied). What is empty leaves the buffer as it was,
    /// what is opaque replaces it, what is half covering blends.
    #[test]
    fn a_plane_is_blended_over_the_buffer_by_its_alpha() {
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;
        let Some(rig) = crate::stretch::tests::gpu::Rig::new() else {
            eprintln!("over test: skipped, no D3D12 device");
            return;
        };
        let back = rig.texture(4, 4, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET);
        let plane = rig.texture(8, 8, D3D12_RESOURCE_FLAG_NONE);
        let blue = [[0u8, 0, 255, 255]; 16];
        let texels: Vec<[u8; 4]> = (0..64)
            .map(|i| match (i % 8 >= 4, i / 8 >= 4) {
                (false, _) => [0, 0, 0, 0],
                (true, false) => [255, 0, 0, 255],
                (true, true) => [128, 0, 0, 128],
            })
            .collect();
        let out = rig.run(&[(&back, &blue, 4, 4), (&plane, &texels, 8, 8)], (&back, 4, 4), |l| {
            let transition = crate::host::transition;
            // SAFETY: an open list of the rig's; its own textures.
            unsafe {
                l.ResourceBarrier(&[
                    transition(&back, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_RENDER_TARGET),
                    transition(&plane, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE),
                ]);
                assert!(record(rig.device(), l, &back, DXGI_FORMAT_R8G8B8A8_UNORM, (4, 4), &plane));
                l.ResourceBarrier(&[transition(
                    &back,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
            }
        });
        let at = |x: usize, y: usize| out[y * 4 + x];
        for y in 0..4 {
            assert_eq!(at(0, y), [0, 0, 255, 255], "empty plane, row {y}");
            assert_eq!(at(1, y), [0, 0, 255, 255], "empty plane, row {y}");
        }
        assert_eq!(at(2, 0), [255, 0, 0, 255]);
        assert_eq!(at(3, 1), [255, 0, 0, 255]);
        let half = at(3, 3);
        assert!(half[0].abs_diff(128) <= 1 && half[1] == 0 && half[2].abs_diff(127) <= 1 && half[3] >= 254, "half covering: {half:?}");
    }
}
