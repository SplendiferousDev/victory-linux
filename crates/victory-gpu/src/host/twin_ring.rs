//! The upload chunks a send of plain CPU sides (`crate::shadow::plain_twins`)
//! puts its bytes in on their way to the card's side.

use std::sync::atomic::Ordering;

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use super::{Chunk, Host, SCRATCH_CHUNK, give_chunks, take_chunk};

/// Upload chunks of the list scratch pool held by sends of plain CPU sides
/// (`crate::shadow::plain_twins`): bytes now, the most since the last
/// memory line, waits for a full ring since then.
pub(super) static TWIN_RING: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// For the memory line (`crate::tight::line`): [`TWIN_RING`], the most and
/// the waits taken.
pub(crate) fn twin_ring() -> [u64; 3] {
    let now = TWIN_RING[0].load(Ordering::Relaxed);
    [
        now,
        TWIN_RING[1].swap(now, Ordering::Relaxed).max(now),
        TWIN_RING[2].swap(0, Ordering::Relaxed),
    ]
}

/// Where a send of plain CPU sides puts its next bytes
/// ([`twin_ring_place`]).
#[derive(Debug, PartialEq, Eq)]
enum RingPlace {
    /// In chunk `chunk` from offset `at`, `n` bytes (at most those asked).
    Here { chunk: usize, at: u64, n: u64 },
    /// No room in the chunks held: take another.
    Take,
    /// No room, and `most` chunks are held: run what was recorded, wait
    /// for it, and fill the chunks again from the first.
    Full,
}

/// Where `len` more bytes of a send go in a ring of `held` chunks of
/// `chunk` bytes, of which chunk `current` is filled up to `used` (those
/// before it are full); at most `most` chunks. Pieces start on 256 bytes.
fn twin_ring_place(
    held: usize,
    most: usize,
    current: usize,
    used: u64,
    chunk: u64,
    len: u64,
) -> RingPlace {
    let at = used.next_multiple_of(256);
    if current < held && at < chunk {
        return RingPlace::Here {
            chunk: current,
            at,
            n: len.min(chunk - at),
        };
    }
    if current + 1 < held {
        return RingPlace::Here {
            chunk: current + 1,
            at: 0,
            n: len.min(chunk),
        };
    }
    if held < most.max(1) {
        RingPlace::Take
    } else {
        RingPlace::Full
    }
}

/// A send's upload chunks for plain CPU sides, held until its plan is done
/// with (its lists have run): then given back to the pool.
#[derive(Default)]
pub(super) struct TwinRing {
    chunks: Vec<(Chunk, u64)>,
    current: usize,
    used: u64,
}

impl TwinRing {
    /// Room for up to `len` bytes: the chunk's buffer, the offset in it,
    /// where it is mapped there, and the bytes. `Ok(None)`: the ring is
    /// full ([`RingPlace::Full`]); `Err`: no memory for a chunk.
    pub(super) fn place(
        &mut self,
        host: &Host,
        len: u64,
    ) -> Result<Option<(ID3D12Resource, u64, *mut u8, u64)>, ()> {
        let most = (crate::shadow::twin_ring_most() / SCRATCH_CHUNK).max(1) as usize;
        loop {
            match twin_ring_place(
                self.chunks.len(),
                most,
                self.current,
                self.used,
                SCRATCH_CHUNK,
                len,
            ) {
                RingPlace::Here { chunk, at, n } => {
                    (self.current, self.used) = (chunk, at + n);
                    let c = &self.chunks[chunk].0;
                    return Ok(Some((
                        c._buffer.clone(),
                        at,
                        (c.data + at as usize) as *mut u8,
                        n,
                    )));
                }
                RingPlace::Take => {
                    let chunk = take_chunk(host, SCRATCH_CHUNK).ok_or(())?;
                    let now =
                        TWIN_RING[0].fetch_add(SCRATCH_CHUNK, Ordering::Relaxed) + SCRATCH_CHUNK;
                    TWIN_RING[1].fetch_max(now, Ordering::Relaxed);
                    if self.chunks.is_empty() {
                        (self.current, self.used) = (0, 0);
                    }
                    self.chunks.push((chunk, SCRATCH_CHUNK));
                }
                RingPlace::Full => return Ok(None),
            }
        }
    }

    /// What was recorded from the chunks has run: they are filled again
    /// from the first.
    pub(super) fn rewind(&mut self) {
        (self.current, self.used) = (0, 0);
    }
}

impl Drop for TwinRing {
    fn drop(&mut self) {
        let bytes: u64 = self.chunks.iter().map(|c| c.1).sum();
        TWIN_RING[0].fetch_sub(bytes, Ordering::Relaxed);
        give_chunks(std::mem::take(&mut self.chunks));
    }
}

/// Where the send of plain CPU sides puts its bytes (`twin_ring_place`):
/// every byte of every range once, no piece across a chunk's end, pieces on
/// 256 bytes, and no more chunks than the most.
#[cfg(test)]
mod tests {
    use super::{RingPlace, twin_ring_place};

    /// Lays `ranges` into a ring as `TwinRing::place` does, without memory:
    /// the pieces (range, offset in it, chunk, offset in the chunk, bytes)
    /// in order, the chunks taken, and the times the ring was full.
    fn lay(
        ranges: &[(u64, u64)],
        chunk: u64,
        most: usize,
    ) -> (Vec<(usize, u64, usize, u64, u64)>, usize, usize) {
        let (mut held, mut current, mut used, mut full) = (0usize, 0usize, 0u64, 0usize);
        let mut pieces = Vec::new();
        for (r, &(a, b)) in ranges.iter().enumerate() {
            let mut from = a;
            while from < b {
                match twin_ring_place(held, most, current, used, chunk, b - from) {
                    RingPlace::Here { chunk: c, at, n } => {
                        assert!(n > 0);
                        (current, used) = (c, at + n);
                        pieces.push((r, from - a, c, at, n));
                        from += n;
                    }
                    RingPlace::Take => {
                        if held == 0 {
                            (current, used) = (0, 0);
                        }
                        held += 1;
                    }
                    RingPlace::Full => {
                        full += 1;
                        (current, used) = (0, 0);
                    }
                }
            }
        }
        (pieces, held, full)
    }

    #[test]
    fn every_byte_goes_once_and_no_piece_crosses_a_chunk() {
        let chunk = 4u64 << 20;
        let cases: [&[(u64, u64)]; 4] = [
            &[(0, 1)],
            &[(0, chunk)],
            &[
                (100, 200),
                (300, 3 * chunk + 17),
                (5 * chunk, 5 * chunk + 4),
            ],
            &[(0, 10 * chunk + 1)],
        ];
        for most in [1usize, 2, 3, 16] {
            for ranges in cases {
                let (pieces, held, _) = lay(ranges, chunk, most);
                assert!(
                    held <= most && held >= 1,
                    "{ranges:?} most {most}: {held} chunks"
                );
                for (r, &(a, b)) in ranges.iter().enumerate() {
                    let mut at = 0;
                    for &(_, off, _, _, n) in pieces.iter().filter(|p| p.0 == r) {
                        assert_eq!(off, at, "{ranges:?}: a gap or overlap in range {r}");
                        at += n;
                    }
                    assert_eq!(at, b - a, "{ranges:?}: range {r} not covered");
                }
                for &(_, _, c, at, n) in &pieces {
                    assert!(
                        c < held && at % 256 == 0 && at + n <= chunk,
                        "{ranges:?}: piece {c} {at} {n}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_full_ring_comes_round_only_at_its_most() {
        let chunk = 1u64 << 20;
        // 5 MiB through 2 chunks: full after 2 and 4 MiB.
        let (_, held, full) = lay(&[(0, 5 << 20)], chunk, 2);
        assert_eq!((held, full), (2, 2));
        // Within the most it never is.
        let (_, held, full) = lay(&[(0, 5 << 20)], chunk, 8);
        assert_eq!((held, full), (5, 0));
        // Small ranges share a chunk, each on 256 bytes.
        let (pieces, held, full) = lay(&[(0, 10), (20, 30), (1000, 1300)], chunk, 4);
        assert_eq!((held, full), (1, 0));
        let ats: Vec<u64> = pieces.iter().map(|p| p.3).collect();
        assert_eq!(ats, vec![0, 256, 512]);
        // A chunk filled to its end: the next piece goes to the next chunk.
        assert_eq!(
            twin_ring_place(2, 4, 0, chunk, chunk, 8),
            RingPlace::Here {
                chunk: 1,
                at: 0,
                n: 8
            }
        );
        assert_eq!(
            twin_ring_place(2, 4, 1, chunk - 100, chunk, 8),
            RingPlace::Take
        );
        assert_eq!(twin_ring_place(4, 4, 3, chunk, chunk, 8), RingPlace::Full);
        // Most 0 is taken as 1.
        assert_eq!(twin_ring_place(0, 0, 0, 0, chunk, 8), RingPlace::Take);
    }
}
