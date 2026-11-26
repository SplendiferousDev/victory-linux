//! The merge on the card of the title's bytes put in under flights' shader
//! writes (`crate::shadow::merge_on_card`): its shader, its pipeline and
//! the PC memory of its masks. Apart from `host.rs` for the asset policy's
//! size limit of a source file.

use super::*;

/// What the merge on the card needs ([`CARD_MERGE`]).
pub(super) struct CardMerge {
    pub(super) root: ID3D12RootSignature,
    pub(super) pso: ID3D12PipelineState,
    /// PC memory, mapped, for the masks of a submission's merges (buffer,
    /// mapping, bytes), and the value of the host's fence that is reached
    /// when the card has run the merges that read it: not written again
    /// before then.
    pub(super) masks: Vec<(ID3D12Resource, usize, u64, u64)>,
}

impl CardMerge {
    /// The most mask buffers alive: more submissions than that are never
    /// in the air with merges of their own (a submission that finds none
    /// free sends its bytes up as copies).
    pub(super) const MOST: usize = 32;

    /// The index in `masks` of a buffer of `bytes` bytes or more that
    /// nothing in the air reads (`completed`: the value the host's fence
    /// has reached): the smallest such.
    fn free(&self, bytes: u64, completed: u64) -> Option<usize> {
        self.masks
            .iter()
            .enumerate()
            .filter(|(_, m)| m.3 <= completed && m.2 >= bytes)
            .min_by_key(|(_, m)| m.2)
            .map(|(at, _)| at)
    }
}

/// The merge on the card (`crate::shadow::merge_on_card`), run in the list
/// that brings the copies in the card's memory up to date, ahead of a
/// submission's lists and behind those of the flights in the air: the
/// bytes of a stretch of a copy whose mask bit is set are taken from the
/// CPU's side (`title`), every other byte stays as the card has it. One
/// thread per dword of mask, 32 bytes of the copy; a thread whose dword is
/// zero reads and writes nothing else, and no two threads write the same
/// dword of the copy. The stretch starts at a multiple of 4 (of 64 KiB, as
/// `shadow::mask_stretches` lays them out).
///
/// The title's byte for byte `i` of the stretch is byte `title_at + i` of
/// `title`: the CPU side itself (`title_at` = `at`) where it is a buffer,
/// or the stretch's bytes laid out after the masks
/// (`shadow::mask_title_fill`) where the CPU side is plain memory
/// (`shadow::plain_twins`). `title_at` is a multiple of 4.
pub(crate) const CARD_MERGE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), SRV(t0), SRV(t1), UAV(u0)"
cbuffer Stretch : register(b0) {
    uint at;       // the stretch's first byte in the copy
    uint mask_at;  // its first byte of mask
    uint words;    // its dwords of mask
    uint title_at; // its first byte in `title`
};
ByteAddressBuffer title : register(t0);
ByteAddressBuffer mask : register(t1);
RWByteAddressBuffer card : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= words) return;
    uint bits = mask.Load(mask_at + id.x * 4u);
    if (bits == 0u) return;
    uint from = at + id.x * 32u;
    uint from_title = title_at + id.x * 32u;
    [loop]
    for (uint k = 0u; k < 8u; k++) {
        uint four = (bits >> (k * 4u)) & 15u;
        if (four == 0u) continue;
        // 0xff in each byte whose bit is set.
        uint bytes = ((four & 1u) | ((four & 2u) << 7) | ((four & 4u) << 14) | ((four & 8u) << 21)) * 255u;
        uint to = from + k * 4u;
        card.Store(to, (card.Load(to) & ~bytes) | (title.Load(from_title + k * 4u) & bytes));
    }
}
"#;

/// The most bytes of a copy one dispatch of [`CARD_MERGE`] goes over (a
/// multiple of `shadow::MASK_BLOCK`): 32,768 groups, within a dispatch's
/// 65,535.
pub(super) const CARD_MERGE_MOST: usize = 64 << 20;

/// Since the last frame-rate line, of the merge on the card: submissions
/// that had one, its dispatches, the bytes of mask sent up, the
/// submissions whose bytes went up as copies for want of a mask buffer;
/// with plain CPU sides (`crate::shadow::plain_twins`) the title's bytes
/// sent up beside the masks, and the copies whose bytes went up as copies
/// because a submission's masks and bytes would have passed
/// [`card_merge_most`].
pub(super) static CARD_MERGES: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// The most bytes of masks and of plain CPU sides' title bytes one
/// submission's merges on the card take in PC memory: the
/// [`crate::shadow::twin_ring_most`] of the upload chunks plain CPU sides
/// go up through otherwise (`VICTORY_CARD_TWIN_RING_MB`, 64). Past it a
/// plain side's bytes go up as copies of the runs, through those chunks.
pub(super) fn card_merge_most() -> usize {
    crate::shadow::twin_ring_most() as usize
}

impl Host {
    /// [`CardMerge`]; `None` (and the title's bytes put in under flights'
    /// writes go up as copies, as without the switch) if the shader cannot
    /// be had.
    pub(super) fn make_card_merge(&self) -> Option<CardMerge> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let made = (|| -> windows::core::Result<CardMerge> {
            let mut code: Option<ID3DBlob> = None;
            // SAFETY: the source and names live through the call; the out
            // pointer is ours.
            unsafe {
                D3DCompile(
                    CARD_MERGE.as_ptr().cast(),
                    CARD_MERGE.len(),
                    windows::core::s!("card merge"),
                    None,
                    None,
                    windows::core::s!("main"),
                    windows::core::s!("cs_5_1"),
                    0,
                    0,
                    &mut code,
                    None,
                )?;
            }
            let code = code.ok_or_else(windows::core::Error::empty)?;
            // SAFETY: the blob's own bytes; the root signature is inside
            // the shader.
            unsafe {
                let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
                let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
                let pso: ID3D12PipelineState = self.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: bytes.as_ptr().cast(),
                        BytecodeLength: bytes.len(),
                    },
                    ..Default::default()
                })?;
                Ok(CardMerge {
                    root,
                    pso,
                    masks: Vec::new(),
                })
            }
        })();
        match made {
            Ok(merge) => {
                tracing::info!(
                    "title bytes put in under flights' shader writes reach copies in the card's memory through a merge on the card \
                     (VICTORY_MERGE_ON_CARD=1; unset: as copies of the changed runs)"
                );
                if crate::shadow::plain_twins() {
                    tracing::info!(
                        "the merge on the card with plain CPU sides (VICTORY_CARD_TWIN_PLAIN): the title's bytes go up beside the masks, \
                         at most {} MB of both a submission (VICTORY_CARD_TWIN_RING_MB); past that as copies of the runs",
                        card_merge_most() >> 20
                    );
                }
                Some(merge)
            }
            Err(e) => {
                tracing::warn!(error = %e, "no merge on the card (VICTORY_MERGE_ON_CARD): the bytes go up as copies");
                None
            }
        }
    }

    /// PC memory for `bytes` bytes of masks of the merge on the card, and
    /// its mapping: a buffer of `merge`'s that nothing in the air reads
    /// (`completed`: the value the host's fence has reached), or a new one.
    /// It is not given out again before the fence reaches `until`. `None`:
    /// [`CardMerge::MOST`] are in the air, or no memory.
    pub(super) fn card_mask(&self, merge: &mut CardMerge, bytes: u64, completed: u64, until: u64) -> Option<(ID3D12Resource, usize)> {
        if let Some(at) = merge.free(bytes, completed) {
            merge.masks[at].3 = until;
            return Some((merge.masks[at].0.clone(), merge.masks[at].1));
        }
        // With plain CPU sides (`crate::shadow::plain_twins`, the mode for
        // little memory) the buffers also hold the title's bytes, up to
        // [`card_merge_most`] each: free ones too small for this asking
        // are given back before a bigger one is made, so that PC memory
        // pinned for the merge stays near what the flights in the air use.
        if merge.masks.len() >= CardMerge::MOST || crate::shadow::plain_twins() {
            // Free ones that are too small make room.
            merge.masks.retain(|m| m.3 > completed);
            if merge.masks.len() >= CardMerge::MOST {
                return None;
            }
        }
        let size = bytes.next_multiple_of(1 << 20);
        let (buffer, data) = self.shared_buffer(size)?;
        name(&buffer, || "card merge masks".into());
        merge.masks.push((buffer.clone(), data as usize, size, until));
        Some((buffer, data as usize))
    }
}
