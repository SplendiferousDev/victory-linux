//! The console upscaler's output, made by a plain stretch.
//!
//! The title in its Series S mode renders below its output size and has the
//! console's FidelityFX upscaler write the output. That upscaler's passes are
//! compute pipelines in a container the layer does not translate, so the
//! output stayed black. This pass stands in for it: each output texel is the
//! bilinear sample of the rendered part of the colour input.
//!
//! Hand-ported from ScarlettPort's vendored runtime (`crates/victory-gpu/src/over.rs`,
//! its stretch half; GPL-3.0-or-later). Differences: the root signature is
//! the shader's own, as for the other passes of ours (`crate::host::compile`),
//! and the sample position is clamped to the rendered part, so the last row
//! and column do not blend in texels past it.
//!
//! shortcut: no temporal accumulation, sharpening or use of the depth and
//! motion inputs: an image, not the upscaler's quality.

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::Interface;

/// The pass: `scale` the rendered part of the source in its texture
/// coordinates, `texel` one target texel, `limit` the last texture
/// coordinate whose bilinear footprint stays in the rendered part.
const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), DescriptorTable(SRV(t0), UAV(u0)), StaticSampler(s0, filter = FILTER_MIN_MAG_LINEAR_MIP_POINT, addressU = TEXTURE_ADDRESS_CLAMP, addressV = TEXTURE_ADDRESS_CLAMP, addressW = TEXTURE_ADDRESS_CLAMP)"
cbuffer Sizes : register(b0) { float2 scale; float2 texel; float2 limit; };
Texture2D<float4> source : register(t0);
RWTexture2D<float4> target : register(u0);
SamplerState bilinear : register(s0);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void stretch(uint3 id : SV_DispatchThreadID) {
    // Groups are 8 by 8: the last ones reach past a size that is not a
    // multiple of 8.
    uint width, height;
    target.GetDimensions(width, height);
    if (id.x >= width || id.y >= height) return;
    float2 at = min((float2(id.xy) + 0.5) * texel * scale, limit);
    target[id.xy] = source.SampleLevel(bilinear, at, 0);
}
"#;

/// Views of a stretch are kept this many stretches (two views each).
///
/// shortcut: a fixed count. The title dispatches the upscaler once a frame
/// and a list runs within the frames the title keeps in flight; 64 frames
/// is far more than it keeps.
const RING: u32 = 64;

struct Pass {
    /// The device the pass was made on (a test makes its own).
    device: usize,
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    step: u32,
    next: u32,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}

static PASS: Mutex<Option<Pass>> = Mutex::new(None);

fn make(device: &ID3D12Device) -> windows::core::Result<Pass> {
    use windows::core::s;
    let cs = crate::host::compile(SOURCE, s!("stretch"), s!("cs_5_1"))?;
    // SAFETY: the root signature is inside the shader; the description
    // points at live data and never releases its pointer.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &cs)?;
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: cs.as_ptr().cast(),
                BytecodeLength: cs.len(),
            },
            ..Default::default()
        };
        let pipeline: ID3D12PipelineState = device.CreateComputePipelineState(&desc)?;
        let views: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: RING * 2,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })?;
        Ok(Pass {
            device: device.as_raw() as usize,
            root,
            pipeline,
            step: device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
            views,
            next: 0,
        })
    }
}

/// A typeless colour format as the typed one shaders read and write.
pub(crate) fn typed(format: DXGI_FORMAT) -> DXGI_FORMAT {
    DXGI_FORMAT(match format.0 {
        1 => 2,
        9 => 10,
        23 => 24,
        27 => 28,
        87 | 90 => 87,
        f => f,
    })
}

/// Whether shaders can write `texture` (it allows unordered access).
pub(crate) fn writable(texture: &ID3D12Resource) -> bool {
    // SAFETY: a plain query of a live resource.
    let desc = unsafe { texture.GetDesc() };
    desc.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
        && desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
        && desc.SampleDesc.Count == 1
}

/// The pass's constants for a stretch of the top-left `render` texels of a
/// `from` (width, height) source over a `to` target: `scale`, `texel`,
/// `limit` as the shader names them. A zero `render` is the whole source.
fn constants(from: (u32, u32), to: (u32, u32), render: [u32; 2]) -> [f32; 6] {
    let part = |r: u32, whole: u32| if r == 0 { whole } else { r.min(whole) };
    let rendered = [part(render[0], from.0), part(render[1], from.1)];
    [
        rendered[0] as f32 / from.0 as f32,
        rendered[1] as f32 / from.1 as f32,
        1.0 / to.0 as f32,
        1.0 / to.1 as f32,
        (rendered[0] as f32 - 0.5) / from.0 as f32,
        (rendered[1] as f32 - 0.5) / from.1 as f32,
    ]
}

/// Records into `list` the stretch of the top-left `render` texels of
/// `source` (readable by compute shaders) over the whole of `target` (in
/// the unordered-access state, [`writable`]). An error says why nothing was
/// recorded.
///
/// The pass sets the list's descriptor heap, compute root signature and
/// pipeline and puts none of them back: the caller replays the title's.
pub(crate) fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    target: &ID3D12Resource,
    source: &ID3D12Resource,
    render: [u32; 2],
) -> Result<(), &'static str> {
    // SAFETY: live resources.
    let (to, from) = unsafe { (target.GetDesc(), source.GetDesc()) };
    if !writable(target) {
        return Err("the output is not a single-sample 2D texture shaders can write");
    }
    if to.Width == 0 || to.Height == 0 || from.Width == 0 || from.Height == 0 {
        return Err("the colour input or the output is empty");
    }
    let mut made = PASS.lock().unwrap_or_else(|e| e.into_inner());
    if made.as_ref().is_none_or(|p| p.device != device.as_raw() as usize) {
        // Tried again at the next dispatch if it fails: a failure need not
        // last.
        *made = match make(device) {
            Ok(p) => {
                tracing::info!("the upscaler's output is made by a plain stretch of its colour input");
                Some(p)
            }
            Err(e) => {
                static TOLD: std::sync::Once = std::sync::Once::new();
                TOLD.call_once(|| {
                    tracing::warn!(error = %e, "no pass to stretch the upscaler's input (its shader or pipeline was refused): its output stays as it is");
                });
                None
            }
        };
    }
    let Some(p) = made.as_mut() else {
        return Err("the stretch pass could not be made");
    };
    let slot = (p.next % RING) * 2;
    p.next = p.next.wrapping_add(1);
    let (width, height) = (to.Width as u32, to.Height);
    let sizes = constants((from.Width as u32, from.Height), (width, height), render);
    let read = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: typed(from.Format),
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
    let written = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: typed(to.Format),
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    // SAFETY: an open list; slots of our heap; live resources and objects
    // (the pass's own live as long as the process, or until a pass for
    // another device replaces it, which only a test does).
    unsafe {
        let cpu = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: p.views.GetCPUDescriptorHandleForHeapStart().ptr + ((slot + i) * p.step) as usize,
        };
        let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: p.views.GetGPUDescriptorHandleForHeapStart().ptr + u64::from(slot * p.step),
        };
        device.CreateShaderResourceView(source, Some(&read), cpu(0));
        device.CreateUnorderedAccessView(target, None, Some(&written), cpu(1));
        list.SetDescriptorHeaps(&[Some(p.views.clone())]);
        list.SetComputeRootSignature(&p.root);
        list.SetPipelineState(&p.pipeline);
        list.SetComputeRoot32BitConstants(0, sizes.len() as u32, sizes.as_ptr().cast(), 0);
        list.SetComputeRootDescriptorTable(1, gpu);
        list.Dispatch(width.div_ceil(8), height.div_ceil(8), 1);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Typeless colour formats are read and written as their typed ones;
    /// typed formats, and formats with no typeless form here, stay.
    #[test]
    fn typeless_formats_get_their_typed_one() {
        for (typeless, typed_one) in [(1, 2), (9, 10), (23, 24), (27, 28), (90, 87)] {
            assert_eq!(typed(DXGI_FORMAT(typeless)).0, typed_one);
            assert_eq!(typed(DXGI_FORMAT(typed_one)).0, typed_one);
        }
        // R11G11B10_FLOAT, R8G8B8A8_UNORM_SRGB, R16G16B16A16_FLOAT, unknown.
        for format in [26, 29, 10, 0] {
            assert_eq!(typed(DXGI_FORMAT(format)).0, format);
        }
    }

    /// The stretch's source guards its writes: groups of 8 reach past sizes
    /// that are not multiples of 8; and its root signature is its own.
    #[test]
    fn the_stretch_writes_inside_its_target_only() {
        assert!(SOURCE.contains("numthreads(8, 8, 1)"));
        assert!(SOURCE.contains("if (id.x >= width || id.y >= height) return;"));
        assert!(SOURCE.contains("[RootSignature(RS)]"));
        assert!(SOURCE.contains("num32BitConstants=6"));
    }

    /// A 960x540 part of a 1920x1080 input over a 1920x1080 output: half the
    /// input's coordinates, and the clamp half a texel inside the part.
    #[test]
    fn the_constants_cover_the_rendered_part() {
        let c = constants((1920, 1080), (1920, 1080), [960, 540]);
        assert_eq!(c[..2], [0.5, 0.5]);
        assert_eq!(c[2..4], [1.0 / 1920.0, 1.0 / 1080.0]);
        assert_eq!(c[4..], [959.5 / 1920.0, 539.5 / 1080.0]);
        // No size given: the whole input; a size past it: clamped to it.
        assert_eq!(constants((64, 32), (128, 64), [0, 0])[..2], [1.0, 1.0]);
        assert_eq!(constants((64, 32), (128, 64), [100, 100])[..2], [1.0, 1.0]);
    }

    /// On a device of the test's own (the software adapter if there is one,
    /// else the default one, under Wine the card through vkd3d-proton): the
    /// top-left 2x2 of a 4x4 input whose other texels are green, stretched
    /// over a 4x4 output. The corners are the part's corners, nothing green
    /// leaks in, and a texel between two input texels is between them.
    #[test]
    fn a_stretch_on_the_device_fills_the_output_from_the_rendered_part() {
        let Some(rig) = gpu::Rig::new() else {
            eprintln!("stretch test: skipped, no D3D12 device");
            return;
        };
        use windows::Win32::Graphics::Direct3D12::*;
        // RGBA8: red ramps across the part (0, 255), blue down it (0, 255);
        // texels outside the part are opaque green.
        let mut input = [[0u8, 255, 0, 255]; 16];
        input[0] = [0, 0, 0, 255];
        input[1] = [255, 0, 0, 255];
        input[4] = [0, 0, 255, 255];
        input[5] = [255, 0, 255, 255];
        let source = rig.texture(4, 4, D3D12_RESOURCE_FLAG_NONE);
        let target = rig.texture(4, 4, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        let out = rig.run(&[(&source, &input, 4, 4)], (&target, 4, 4), |l| {
            let transition = crate::host::transition;
            // SAFETY: an open list of the rig's; its own textures.
            unsafe {
                l.ResourceBarrier(&[
                    transition(&source, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE),
                    transition(&target, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS),
                ]);
                record(rig.device(), l, &target, &source, [2, 2]).expect("a stretch recorded");
                l.ResourceBarrier(&[transition(
                    &target,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
            }
        });
        let at = |x: usize, y: usize| out[y * 4 + x];
        for (i, texel) in out.iter().enumerate() {
            assert_eq!(texel[1], 0, "texel {i} has green from outside the rendered part: {texel:?}");
            assert_eq!(texel[3], 255, "texel {i}");
        }
        assert_eq!(at(0, 0), [0, 0, 0, 255]);
        assert_eq!(at(3, 0), [255, 0, 0, 255]);
        assert_eq!(at(0, 3), [0, 0, 255, 255]);
        assert_eq!(at(3, 3), [255, 0, 255, 255]);
        // Output column 1 samples input x 0.75: a quarter of the way.
        let red = at(1, 0)[0];
        assert!((60..=68).contains(&red), "a texel between two input texels: {red}");
        // Rows go the same way down.
        assert_eq!(at(1, 0)[0], at(1, 3)[0]);
        assert_eq!(at(0, 1)[2], at(3, 1)[2]);
    }

    /// A device of the tests' own and one list, for the passes of ours
    /// (also `crate::over`'s tests): RGBA8 textures filled by upload,
    /// one submission, one texture read back.
    pub(crate) mod gpu {
        use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
        use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
        use windows::core::Interface;

        pub(crate) struct Rig {
            device: ID3D12Device,
            queue: ID3D12CommandQueue,
            _allocator: ID3D12CommandAllocator,
            list: ID3D12GraphicsCommandList,
            fence: ID3D12Fence,
            event: HANDLE,
        }

        impl Drop for Rig {
            fn drop(&mut self) {
                // SAFETY: our own event.
                let _ = unsafe { CloseHandle(self.event) };
            }
        }

        /// Bytes from one row to the next in the copy buffers: textures of
        /// up to 64 texels a row.
        const PITCH: u32 = 256;

        impl Rig {
            /// The software adapter if there is one, else the default one
            /// (under Wine: the card through vkd3d-proton); `None` without a
            /// device.
            pub(crate) fn new() -> Option<Rig> {
                // SAFETY: plain object creation on a device of our own.
                unsafe {
                    let mut device: Option<ID3D12Device> = None;
                    let warp = crate::host::warp_adapter().ok();
                    match &warp {
                        Some(adapter) => D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?,
                        None => D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?,
                    }
                    let device = device?;
                    eprintln!(
                        "pass test: on {}",
                        if warp.is_some() { "the software adapter" } else { "the default adapter" }
                    );
                    let queue: ID3D12CommandQueue = device
                        .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                            ..Default::default()
                        })
                        .ok()?;
                    let allocator: ID3D12CommandAllocator =
                        device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
                    let list: ID3D12GraphicsCommandList =
                        device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).ok()?;
                    let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                    let event = CreateEventW(None, false, false, None).ok()?;
                    Some(Rig {
                        device,
                        queue,
                        _allocator: allocator,
                        list,
                        fence,
                        event,
                    })
                }
            }

            pub(crate) fn device(&self) -> &ID3D12Device {
                &self.device
            }

            fn buffer(&self, kind: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES, rows: u32) -> ID3D12Resource {
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(PITCH) * u64::from(rows),
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                self.committed(kind, &desc, state)
            }

            /// An RGBA8 texture in the copy-destination state.
            pub(crate) fn texture(&self, width: u32, height: u32, flags: D3D12_RESOURCE_FLAGS) -> ID3D12Resource {
                assert!(width * 4 <= PITCH);
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: u64::from(width),
                    Height: height,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Flags: flags,
                    ..Default::default()
                };
                self.committed(D3D12_HEAP_TYPE_DEFAULT, &desc, D3D12_RESOURCE_STATE_COPY_DEST)
            }

            fn committed(
                &self,
                kind: D3D12_HEAP_TYPE,
                desc: &D3D12_RESOURCE_DESC,
                state: D3D12_RESOURCE_STATES,
            ) -> ID3D12Resource {
                let heap = D3D12_HEAP_PROPERTIES {
                    Type: kind,
                    ..Default::default()
                };
                let mut out: Option<ID3D12Resource> = None;
                // SAFETY: plain resource creation.
                unsafe {
                    self.device
                        .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, desc, state, None, &mut out)
                        .expect("a resource");
                }
                out.expect("a resource")
            }

            fn footprint(buffer: &ID3D12Resource, width: u32, height: u32) -> D3D12_TEXTURE_COPY_LOCATION {
                D3D12_TEXTURE_COPY_LOCATION {
                    // SAFETY: a borrowed pointer, never released through the copy.
                    pResource: unsafe { std::mem::transmute_copy(buffer) },
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                                Width: width,
                                Height: height,
                                Depth: 1,
                                RowPitch: PITCH,
                            },
                        },
                    },
                }
            }

            fn whole(texture: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
                D3D12_TEXTURE_COPY_LOCATION {
                    // SAFETY: as above.
                    pResource: unsafe { std::mem::transmute_copy(texture) },
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                }
            }

            /// One submission: each input texture (in the copy-destination
            /// state) gets its texels, `record` records what is tested (it
            /// leaves `output` in the copy-source state), and `output` is
            /// read back.
            pub(crate) fn run(
                &self,
                inputs: &[(&ID3D12Resource, &[[u8; 4]], u32, u32)],
                (output, width, height): (&ID3D12Resource, u32, u32),
                record: impl FnOnce(&ID3D12GraphicsCommandList),
            ) -> Vec<[u8; 4]> {
                let back = self.buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST, height);
                let mut uploads = Vec::new();
                // SAFETY: our own resources, list and queue; the mapped
                // buffers are written and read inside their sizes, and read
                // after the card finished.
                unsafe {
                    let l = &self.list;
                    for &(texture, texels, w, h) in inputs {
                        let upload = self.buffer(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ, h);
                        let mut at = std::ptr::null_mut();
                        upload.Map(0, None, Some(&mut at)).expect("a mapped upload buffer");
                        for y in 0..h as usize {
                            let row = &texels[y * w as usize..][..w as usize];
                            std::ptr::copy_nonoverlapping(
                                row.as_ptr().cast::<u8>(),
                                at.cast::<u8>().add(y * PITCH as usize),
                                row.len() * 4,
                            );
                        }
                        upload.Unmap(0, None);
                        l.CopyTextureRegion(&Self::whole(texture), 0, 0, 0, &Self::footprint(&upload, w, h), None);
                        uploads.push(upload);
                    }
                    record(l);
                    l.CopyTextureRegion(&Self::footprint(&back, width, height), 0, 0, 0, &Self::whole(output), None);
                    l.Close().expect("a closed list");
                    self.queue.ExecuteCommandLists(&[Some(l.cast().expect("a command list"))]);
                    self.queue.Signal(&self.fence, 1).expect("a signal");
                    self.fence.SetEventOnCompletion(1, self.event).expect("an event");
                    assert_eq!(WaitForSingleObject(self.event, 10_000), WAIT_OBJECT_0, "the submission did not finish");
                    let removed = self.device.GetDeviceRemovedReason();
                    assert!(removed.is_ok(), "the device is lost: {removed:?}");
                    let mut at = std::ptr::null_mut();
                    back.Map(0, None, Some(&mut at)).expect("a mapped read-back buffer");
                    let mut out = Vec::new();
                    for y in 0..height as usize {
                        let row = at.cast::<[u8; 4]>().byte_add(y * PITCH as usize);
                        out.extend_from_slice(std::slice::from_raw_parts(row, width as usize));
                    }
                    back.Unmap(0, None);
                    out
                }
            }
        }
    }
}
