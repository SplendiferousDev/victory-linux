//! The three-way merge on the card (`VICTORY_MERGE_THREEWAY=1`; off by
//! default, and only with the relay, `crate::shadow::relay`).
//!
//! The merge on the card (`crate::shadow::merge_on_card`) puts the title's
//! changed bytes over the card's side of a copy. A byte that a flight's
//! shader wrote on the card AND the title changed on the CPU meanwhile
//! takes the title's value there, and the shader's write is lost: the
//! landing takes such a byte for the CPU side's (`Shadow::card_merge_relay`).
//! Settled instead (the flights landed first, then the copy refreshed), the
//! same byte keeps the shader's value: a landing puts every byte the card
//! changed into title memory.
//!
//! With the switch each masked byte also keeps the CPU side's value from
//! before the title's change (`Masked::old`), which goes up beside the
//! masks, and the pass takes the title's byte only where the card still
//! holds that old value ([`pick_dword`]); where a shader wrote the byte, the
//! card's stays, as a settle would leave it. The landing applies the same
//! rule on the CPU to the bytes of merges the card had not run when the
//! fetch ran, so title memory and the CPU side take the shader's byte too.
//!
//! Conflicts (always counted while the merge is on): masked bytes whose
//! card value at a landing is not the old one, i.e. a shader wrote a byte
//! the title changed. Without the switch the title's value wins at those;
//! with it the shader's. Only merges not yet run at the fetch show them
//! without the switch, so the count is then a lower bound.
//!
//! `alt` is not offered: a landing would have to know which rule each send
//! used, and a byte noted under one rule may go up under the other.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::shadow::{MASK_BLOCK, MaskBlocks, MaskStretch};

/// Whether the merge on the card is three-way (read once).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let value = std::env::var("VICTORY_MERGE_THREEWAY").ok();
        let on = value.as_deref().map(str::trim) == Some("1") && crate::shadow::relay();
        match value.as_deref().map(str::trim) {
            Some("1") if on => tracing::info!(
                "the merge on the card is three-way: a byte a shader wrote keeps the shader's value (VICTORY_MERGE_THREEWAY=1)"
            ),
            Some("1") => tracing::warn!("VICTORY_MERGE_THREEWAY=1 ignored: it needs the relay (VICTORY_RELAY)"),
            Some("alt") => tracing::warn!("VICTORY_MERGE_THREEWAY=alt is not offered (the rule cannot change between sends): off"),
            _ => {}
        }
        on
    })
}

/// Since the last frame-rate line: conflicts seen at landings for merges
/// the card had not run at the fetch, bytes a three-way merge left to the
/// shader that a landing found, and bytes of slot tables and old bytes
/// sent up.
static CONFLICTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Counts `n` conflicts of kind `kind` ([`CONFLICTS`]).
pub(crate) fn count(kind: usize, n: u64) {
    if n != 0 {
        CONFLICTS[kind].fetch_add(n, Ordering::Relaxed);
    }
}

/// [`CONFLICTS`] since the last asking, per frame of `frames`.
pub(crate) fn taken(frames: f64) -> [f64; 3] {
    [0, 1, 2].map(|i| CONFLICTS[i].swap(0, Ordering::Relaxed) as f64 / frames)
}

/// `0xff` in each byte of `x` that is not zero.
#[cfg(test)]
pub(crate) fn nonzero_bytes32(x: u32) -> u32 {
    let mut d = x;
    d |= d >> 4;
    d |= d >> 2;
    d |= d >> 1;
    (d & 0x0101_0101) * 255
}

/// One dword of the three-way pass: `card` as the card holds it, `old` the
/// CPU side's bytes from before the title's change, `title` the title's,
/// `bytes` `0xff` in each masked byte. A masked byte takes the title's
/// value where the card still holds the old one; every other byte stays.
/// (The rule of `crate::host::CARD_MERGE3`, for the tests.)
#[cfg(test)]
pub(crate) fn pick_dword(card: u32, old: u32, title: u32, bytes: u32) -> u32 {
    let take = bytes & !nonzero_bytes32(card ^ old);
    (card & !take) | (title & take)
}

/// The dwords of mask that are not zero in `blocks`: each takes a slot of
/// 32 old bytes.
pub(crate) fn slots(blocks: &MaskBlocks) -> usize {
    blocks.values().map(|mask| mask.chunks_exact(4).filter(|d| *d != [0u8; 4]).count()).sum()
}

/// Lays out the old bytes of `blocks` for the pass: `table` holds a dword
/// for each dword of mask, at the mask's own offsets (as
/// `shadow::mask_fill` lays the masks out for `stretches`): the slot of a
/// dword that is not zero, `u32::MAX` for the others; `slots_out` takes
/// the 32 old bytes of each such dword in its slot, the first `first`.
/// Returns the next free slot.
pub(crate) fn lay_old(table: &mut [u8], slots_out: &mut [u8], stretches: &[MaskStretch], blocks: &MaskBlocks, old: &MaskBlocks, first: usize) -> usize {
    let mut slot = first;
    let mut k = 0;
    for (&block, bits) in blocks {
        let at = block * MASK_BLOCK;
        while stretches[k].at + stretches[k].bytes <= at {
            k += 1;
        }
        let to = stretches[k].mask_at + (at - stretches[k].at) / 8;
        let olds = old.get(&block);
        for (w, dword) in bits.chunks_exact(4).enumerate() {
            let entry = if dword == [0u8; 4] {
                u32::MAX
            } else {
                let into = &mut slots_out[slot * 32..slot * 32 + 32];
                match olds {
                    Some(olds) => into.copy_from_slice(&olds[w * 32..w * 32 + 32]),
                    None => into.fill(0),
                }
                slot += 1;
                (slot - 1) as u32
            };
            table[to + w * 4..to + w * 4 + 4].copy_from_slice(&entry.to_le_bytes());
        }
    }
    slot
}

/// The CPU side's value of byte `at` of the copy from before the title's
/// change, if `old` has it.
pub(crate) fn old_at(old: &MaskBlocks, at: usize) -> Option<u8> {
    old.get(&(at / MASK_BLOCK)).map(|b| b[at % MASK_BLOCK])
}

/// Notes the old values of the bytes of `bits` (bit `j`: byte `bit0 + j`
/// of the block) whose bit is not set yet in `mask`: `held` holds them
/// (byte `j` for bit `j`), before the title's bytes go over it.
pub(crate) fn note_old(mask: &[u8], old: &mut [u8], bit0: usize, bits: u8, held: &[u8]) {
    for j in 0..8 {
        if bits >> j & 1 == 0 {
            continue;
        }
        let bit = bit0 + j;
        if mask[bit / 8] >> (bit % 8) & 1 == 0 {
            old[bit] = held[j];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_byte_the_shader_wrote_keeps_the_shaders_value() {
        // Bytes 0 and 1 masked (the title changed them from 0x11, 0x22 to
        // 0xaa, 0xbb); the shader wrote byte 1 (0x22 -> 0x99) and byte 3
        // (not masked).
        let old = u32::from_le_bytes([0x11, 0x22, 0x33, 0x44]);
        let card = u32::from_le_bytes([0x11, 0x99, 0x33, 0x77]);
        let title = u32::from_le_bytes([0xaa, 0xbb, 0x33, 0x44]);
        let bytes = 0x0000_ffff;
        assert_eq!(pick_dword(card, old, title, bytes).to_le_bytes(), [0xaa, 0x99, 0x33, 0x77]);
        // The two-way pass for comparison: the title wins byte 1.
        let two = (card & !bytes) | (title & bytes);
        assert_eq!(two.to_le_bytes(), [0xaa, 0xbb, 0x33, 0x77]);
    }

    #[test]
    fn nonzero_bytes_look_at_each_byte_alone() {
        assert_eq!(nonzero_bytes32(0), 0);
        assert_eq!(nonzero_bytes32(0x8000_0001), 0xff00_00ff);
        assert_eq!(nonzero_bytes32(0x0010_0100), 0x00ff_ff00);
        assert_eq!(nonzero_bytes32(0xffff_ffff), 0xffff_ffff);
    }

    #[test]
    fn old_bytes_are_laid_out_by_the_masks_offsets() {
        // Two blocks next to each other: one stretch. Block 0 has bytes 3
        // and 40 masked (dwords 0 and 1), block 1 byte 0 (its dword 0).
        let mut blocks = MaskBlocks::new();
        let mut m0 = vec![0u8; MASK_BLOCK / 8].into_boxed_slice();
        m0[0] = 1 << 3;
        m0[5] = 1;
        blocks.insert(0, m0);
        let mut m1 = vec![0u8; MASK_BLOCK / 8].into_boxed_slice();
        m1[0] = 1;
        blocks.insert(1, m1);
        let mut old = MaskBlocks::new();
        let mut o0 = vec![0u8; MASK_BLOCK].into_boxed_slice();
        o0[3] = 0x33;
        o0[40] = 0x40;
        old.insert(0, o0);
        let mut o1 = vec![0u8; MASK_BLOCK].into_boxed_slice();
        o1[0] = 0x77;
        old.insert(1, o1);
        let (stretches, end) = crate::shadow::mask_stretches(&blocks, 0);
        assert_eq!(slots(&blocks), 3);
        let mut table = vec![0u8; end];
        let mut out = vec![0u8; 3 * 32];
        assert_eq!(lay_old(&mut table, &mut out, &stretches, &blocks, &old, 0), 3);
        let entry = |w: usize| u32::from_le_bytes(table[w * 4..w * 4 + 4].try_into().unwrap());
        assert_eq!((entry(0), entry(1), entry(2)), (0, 1, u32::MAX));
        // Block 1's first dword of mask is dword MASK_BLOCK / 32 of the stretch.
        assert_eq!(entry(MASK_BLOCK / 32), 2);
        assert_eq!((out[3], out[32 + 8], out[64]), (0x33, 0x40, 0x77));
    }

    #[test]
    fn an_old_byte_is_noted_only_the_first_time() {
        let mut mask = vec![0u8; 2];
        let mut old = vec![0u8; 16];
        note_old(&mask, &mut old, 0, 0b101, &[1, 2, 3, 4, 5, 6, 7, 8]);
        mask[0] = 0b101;
        // Byte 0 again (already masked) and byte 1 for the first time.
        note_old(&mask, &mut old, 0, 0b011, &[9, 9, 9, 9, 9, 9, 9, 9]);
        assert_eq!(&old[..3], &[1, 9, 3]);
        assert_eq!(old_at(&MaskBlocks::new(), 5), None);
    }
}
