//! Small changes to the copies in the card's memory taken from other forks,
//! each behind a switch of its own (off unless `1`; `alt` in turn,
//! `crate::alt`). Apart from `host.rs` for the asset policy's size limit of
//! a source file.

use super::*;

static ONE_BARRIER: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_CARD_ONE_BARRIER",
    "the state changes after the copies up to the card's copies are one a buffer, however many copies lie in it",
);

/// State changes left out because their buffer had one already in the same
/// call (since the start).
pub(crate) static ONE_BARRIER_SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether the state change of `buffer` after the copies up to it is the
/// first of that buffer in the call being made (`seen`: the buffers of
/// those before). A second change of the same buffer in one
/// `ResourceBarrier` call starts from a state the first already left.
/// Always true with `VICTORY_CARD_ONE_BARRIER` off, as before.
/// (After codec-d/cool_fox 641835a4, by way of iLucasRT-VInAMD 773d902f.)
pub(super) fn first_of_buffer(seen: &mut Vec<usize>, buffer: &ID3D12Resource) -> bool {
    if !ONE_BARRIER.on() {
        return true;
    }
    let raw = buffer.as_raw() as usize;
    if seen.contains(&raw) {
        if ONE_BARRIER_SKIPPED.fetch_add(1, Ordering::Relaxed) == 0 {
            tracing::info!("a card copy's buffer had two state changes in one call: one left out (VICTORY_CARD_ONE_BARRIER)");
        }
        return false;
    }
    seen.push(raw);
    true
}

static EIGHT: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_CARD_COMPARE_EIGHT",
    "the card's hashes of copies' pieces (a group of 64 threads a piece) load eight places at once",
);

/// The loop of [`CARD_COMPARE_WIDE`] that [`card_compare_eight_source`]
/// replaces.
const WIDE_LOOP: &str = "    [loop]
    for (uint at = lo + t * 16u; at < hi; at += 1024u) {
        h = (h ^ now.Load4(at)) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
    }
";

/// The same steps eight at a time: the eight loads are made before any is
/// folded, so the card waits for memory once for the eight and not eight
/// times one after the other. Each lane folds the same 16 bytes in the same
/// order as [`WIDE_LOOP`]: the same hash, so stored and compared hashes of
/// both forms agree and `alt` may change form between any two submissions.
const EIGHT_LOOP: &str = "    uint at = lo + t * 16u;
    [loop]
    for (; at + 7168u < hi; at += 8192u) {
        uint4 v0 = now.Load4(at + 0u);
        uint4 v1 = now.Load4(at + 1024u);
        uint4 v2 = now.Load4(at + 2048u);
        uint4 v3 = now.Load4(at + 3072u);
        uint4 v4 = now.Load4(at + 4096u);
        uint4 v5 = now.Load4(at + 5120u);
        uint4 v6 = now.Load4(at + 6144u);
        uint4 v7 = now.Load4(at + 7168u);
        h = (h ^ v0) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v1) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v2) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v3) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v4) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v5) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v6) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v7) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
    }
    [loop]
    for (; at < hi; at += 1024u) {
        h = (h ^ now.Load4(at)) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
    }
";

/// [`CARD_COMPARE_WIDE`] with its loop of loads eight at once
/// (`VICTORY_CARD_COMPARE_EIGHT`); `None` if that loop is not in it as
/// expected (the wide form then stays as it is).
pub(super) fn card_compare_eight_source() -> Option<String> {
    CARD_COMPARE_WIDE.contains(WIDE_LOOP).then(|| CARD_COMPARE_WIDE.replacen(WIDE_LOOP, EIGHT_LOOP, 1))
}

/// The pipeline of a submission's compare: `wide`'s, or with
/// `VICTORY_CARD_COMPARE_EIGHT` on now (and made) the eight-load form of it.
/// (After iLucasRT-VInAMD 9b90ad11: 1.93 -> 1.69 ms of a frame's 423 passes
/// on its card.)
pub(super) fn compare_or_eight<'a>(
    wide: (&'a ID3D12PipelineState, bool),
    eight: &'a Option<ID3D12PipelineState>,
) -> (&'a ID3D12PipelineState, bool) {
    match eight {
        Some(eight) if wide.1 && EIGHT.on() => (eight, true),
        _ => wide,
    }
}

impl Host {
    /// The eight-load wide compare with the compare's root signature `root`
    /// if `VICTORY_CARD_COMPARE_EIGHT` is set and the wide form is in use at
    /// all; `None`, said in the log, if it cannot be made.
    pub(super) fn make_card_compare_eight(&self, root: &ID3D12RootSignature) -> Option<ID3D12PipelineState> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        if !EIGHT.set() || card_hash_mode() == 0 {
            return None;
        }
        let Some(source) = card_compare_eight_source() else {
            tracing::warn!("VICTORY_CARD_COMPARE_EIGHT: the wide compare's loop is not the one expected; left as it is");
            return None;
        };
        let made = (|| -> windows::core::Result<ID3D12PipelineState> {
            let mut code: Option<ID3DBlob> = None;
            // SAFETY: the source and names live through the call; the out pointer is ours.
            unsafe {
                D3DCompile(
                    source.as_ptr().cast(),
                    source.len(),
                    windows::core::s!("card compare eight"),
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
            // SAFETY: the blob's own bytes; a root signature the shader declares as well.
            unsafe {
                let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
                self.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: bytes.as_ptr().cast(),
                        BytecodeLength: bytes.len(),
                    },
                    ..Default::default()
                })
            }
        })();
        match made {
            Ok(pso) => Some(pso),
            Err(e) => {
                tracing::warn!(error = %e, "VICTORY_CARD_COMPARE_EIGHT: the eight-load compare could not be made; one load a step throughout");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 16-byte steps one lane folds, in order: as the wide loop walks
    /// them, and as the eight-load loop does.
    fn steps_one(lo: u32, hi: u32, t: u32) -> Vec<u32> {
        (0..).map(|k| lo + t * 16 + k * 1024).take_while(|&at| at < hi).collect()
    }

    fn steps_eight(lo: u32, hi: u32, t: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut at = lo + t * 16;
        while at + 7168 < hi {
            out.extend((0..8).map(|k| at + k * 1024));
            at += 8192;
        }
        while at < hi {
            out.push(at);
            at += 1024;
        }
        out
    }

    #[test]
    fn eight_loads_fold_the_same_steps_in_the_same_order() {
        for (lo, hi) in [(0, 65536), (16, 65536), (0, 16), (4096, 12288), (0, 8192), (0, 8208), (32, 40000), (0, 0)] {
            for t in [0, 1, 31, 63] {
                assert_eq!(steps_one(lo, hi, t), steps_eight(lo, hi, t), "lo {lo} hi {hi} lane {t}");
            }
        }
    }

    #[test]
    fn the_eight_load_source_replaces_the_wide_loop_once() {
        let source = card_compare_eight_source().expect("the wide compare's loop as expected");
        assert!(source.contains("now.Load4(at + 7168u)"));
        assert!(!source.contains(WIDE_LOOP));
        assert_eq!(source.matches("void main").count(), 1);
    }

    /// The eight-load form compiles (the compiler only: no device).
    #[test]
    fn the_eight_load_source_compiles() {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let source = card_compare_eight_source().expect("the wide compare's loop as expected");
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; the out pointers are ours.
        let made = unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                windows::core::s!("card compare eight"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        let said = errors
            // SAFETY: the blob's own bytes.
            .map(|e| unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(e.GetBufferPointer().cast::<u8>(), e.GetBufferSize())).into_owned() })
            .unwrap_or_default();
        assert!(made.is_ok() && code.is_some(), "{said}");
    }
}
