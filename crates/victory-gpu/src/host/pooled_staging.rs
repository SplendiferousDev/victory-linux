//! Staging for the title's buffer-to-texture copies taken from the list's
//! pooled scratch chunks instead of a committed upload buffer made, mapped
//! and freed per copy (`VICTORY_POOLED_STAGING`; off unless `1`, `alt` in
//! turn, `crate::alt`). Apart from `host.rs` for the asset policy's size
//! limit of a source file.
//!
//! Ported by hand from VIctory-Belobug-AMD 5e08eee0 (romainhedouin).

use super::*;

static POOLED: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_POOLED_STAGING",
    "buffer-to-texture copies stage their bytes in the list's pooled scratch chunks, not an upload buffer each",
);

/// Where a placed footprint may start in a buffer
/// (`D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT`).
const PLACEMENT: u64 = 512;

impl HostList {
    /// `size` bytes of mapped upload memory for a copy's placed footprint,
    /// alive until the list is reset: the buffer, the offset of the bytes in
    /// it (a multiple of 512) and the CPU pointer. With the switch off, or
    /// past a scratch chunk's size, an upload buffer of its own as before.
    pub(crate) fn staging(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, u64, *mut u8)> {
        if size > SCRATCH_CHUNK || !POOLED.on() {
            let (buffer, data) = self.upload(host, size)?;
            return Some((buffer, 0, data));
        }
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        let len = size.max(1).next_multiple_of(256);
        // As `scratch`: the chunk in use while the bytes fit in what is left
        // of it (from a placement boundary), else the next one.
        while s.current < s.chunks.len() && s.used.next_multiple_of(PLACEMENT) + len > s.chunks[s.current].1 {
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let size = if scratch_pool() && s.chunks.is_empty() && len <= SCRATCH_FIRST {
                SCRATCH_FIRST
            } else {
                SCRATCH_CHUNK
            };
            let (chunk, size) = take_chunk(host, size)?;
            s.chunks.push((chunk, size));
            s.used = 0;
        }
        let at = s.used.next_multiple_of(PLACEMENT);
        let chunk = &s.chunks[s.current].0;
        let made = (chunk._buffer.clone(), at, (chunk.data + at as usize) as *mut u8);
        s.used = at + len;
        Some(made)
    }
}
