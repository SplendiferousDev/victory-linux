//! Tests of `crate::patchspan` (`VICTORY_PATCH_SPANS`), inside
//! `crate::shadow`: the one-pass words against the run-by-run ones, the
//! spans against the runs' pieces, and a landing after a patch noted as a
//! span against one noted run by run. Synthetic bytes only.

use super::{PIECE, PatchWord, changed_runs, folded, joined, patch_words, piece_spans, pieces_of};
use crate::patchspan::{PageSpan, apply, page_runs, page_words, runs_in, strictly_rising};

/// A small xorshift, for pages of every shape.
struct Seed(u64);

impl Seed {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// A page's old bytes and new ones: `kind` 0 sparse single bytes, 1 runs of
/// a few bytes (the world's ~4), 2 long runs, 3 every byte, 4 none.
fn page(seed: &mut Seed, n: usize, kind: usize) -> (Vec<u8>, Vec<u8>) {
    let was: Vec<u8> = (0..n).map(|_| seed.below(256) as u8).collect();
    let mut now = was.clone();
    let mut i = seed.below(8);
    while i < n {
        let (len, gap) = match kind {
            0 => (1, 1 + seed.below(40)),
            1 => (1 + seed.below(7), 1 + seed.below(9)),
            2 => (1 + seed.below(3000), 1 + seed.below(500)),
            3 => (n, 0),
            _ => break,
        };
        for b in &mut now[i..(i + len).min(n)] {
            *b ^= 1 + seed.below(255) as u8;
        }
        i += len + gap;
    }
    (was, now)
}

/// The words made in one pass equal those made from the changed runs bit
/// for bit, folded or not, at every alignment of the page in the copy and
/// for pages that are not whole blocks of 64; the runs and bytes counted
/// are the runs' and their bytes; the runs read back off the words are
/// the changed runs.
#[test]
fn one_pass_words_equal_the_run_by_run_words() {
    let mut seed = Seed(0x9e37_79b9_7f4a_7c15);
    for at in [0usize, 1, 2, 3, 4, 65536, 65536 + 5, 3 * 65536 + 63] {
        for n in [65536usize, 4097, 100, 64, 63, 5, 3, 1] {
            for kind in 0..5 {
                let (was, now) = page(&mut seed, n, kind);
                let runs = changed_runs(&now, &was);
                let mut want: Vec<PatchWord> = Vec::new();
                patch_words(at, &now, &runs, &mut want);
                let mut got: Vec<PatchWord> = Vec::new();
                let (count, bytes) = page_words(at, &now, &was, &mut got);
                assert_eq!(got, want, "at {at} n {n} kind {kind}");
                assert_eq!(folded(got.clone()), want, "at {at} n {n} kind {kind}");
                assert!(strictly_rising(&got));
                assert_eq!(count, runs.len(), "at {at} n {n} kind {kind}");
                assert_eq!(bytes, runs.iter().map(|r| r.1).sum::<usize>());
                assert_eq!(page_runs(&got, at, n), runs, "at {at} n {n} kind {kind}");
                // The CPU side takes exactly the changed bytes.
                let mut held = was.clone();
                apply(&got, at, &mut held);
                assert_eq!(held, now);
            }
        }
    }
}

/// A page's span runs from its first changed byte to its last, lies in
/// the page, and holds the same pieces of title memory as the page's runs
/// (so the hash passes take the same pieces), whatever the copy's start;
/// spans of several pages, joined, give the pieces of all their runs.
#[test]
fn spans_cover_exactly_the_patched_pages() {
    let mut seed = Seed(0x2545_f491_4f6c_dd1d);
    for start in [0usize, 4, 65532, 0x1234_5678] {
        let mut all_runs = Vec::new();
        let mut all_spans = Vec::new();
        for page_no in [0usize, 1, 2, 5, 6, 40] {
            // A page of the copy as merge_under_writes cuts it: on 64 KiB
            // of title memory.
            let lo = (page_no * PIECE).saturating_sub(start % PIECE);
            let n = PIECE.min(PIECE - (start + lo) % PIECE);
            let (was, now) = page(&mut seed, n, page_no % 3);
            let mut words = Vec::new();
            page_words(lo, &now, &was, &mut words);
            let runs: Vec<(usize, usize)> = changed_runs(&now, &was).into_iter().map(|(r, m)| (lo + r, lo + r + m)).collect();
            if runs.is_empty() {
                continue;
            }
            let span = PageSpan::new(words);
            assert_eq!(span.from, runs[0].0);
            assert_eq!(span.to, runs[runs.len() - 1].1);
            assert!(span.from >= lo && span.to <= lo + n);
            assert_eq!(pieces_of(start, span.from, span.to), pieces_of(start, runs[0].0, runs[runs.len() - 1].1));
            assert_eq!(pieces_of(start, span.from, span.to).len(), 1, "a page is one piece");
            all_runs.extend(runs);
            all_spans.push((span.from, span.to));
        }
        let pieces = |ranges: &[(usize, usize)]| {
            let mut out: Vec<usize> = ranges.iter().flat_map(|&(a, b)| pieces_of(start, a, b)).collect();
            out.sort_unstable();
            out.dedup();
            out
        };
        let (runs, spans) = (joined(all_runs), joined(all_spans));
        assert_eq!(pieces(&spans), pieces(&runs));
        assert_eq!(pieces(&piece_spans(start, &spans)), pieces(&piece_spans(start, &runs)));
        assert!(spans.len() <= runs.len());
    }
}

/// What a landing skips of a span: exactly the changed bytes inside what it
/// lands, none between them.
#[test]
fn a_span_gives_a_landing_its_exact_bytes() {
    let was = vec![0u8; 256];
    let mut now = was.clone();
    for i in [3usize, 4, 5, 9, 64, 65, 100, 255] {
        now[i] = 1;
    }
    let mut words = Vec::new();
    page_words(1000, &now, &was, &mut words);
    let span = PageSpan::new(words);
    assert_eq!((span.from, span.to), (1003, 1256));
    let mut out = Vec::new();
    assert_eq!(runs_in(&span.words, 0, usize::MAX, &mut out), 5);
    assert_eq!(out, vec![(1003, 1006), (1009, 1010), (1064, 1066), (1100, 1101), (1255, 1256)]);
    out.clear();
    runs_in(&span.words, 1004, 1065, &mut out);
    assert_eq!(out, vec![(1004, 1006), (1009, 1010), (1064, 1065)]);
    out.clear();
    assert_eq!(runs_in(&span.words, 1010, 1064, &mut out), 0);
    assert!(span.overlaps(1255, 2000) && !span.overlaps(1256, 2000) && !span.overlaps(0, 1003));
}

/// A landing after a patch, on a real copy in the card's memory
/// (`Shadow::card_merge_relay`): the title rewrote a page in hundreds of
/// short runs (patched, so the CPU side holds them and the card's side
/// does not yet), and a flight's shader wrote bytes between those runs and
/// beside them. Noted as one span (pending, or sent in a send the fetch
/// came before) the landing keeps every title byte and lands every shader
/// byte, exactly as with the runs noted one by one; noted in a send the
/// fetch came after, the card's bytes win there, as with runs. A coarse
/// note taken as a range would lose the shader's bytes between the runs.
#[test]
fn a_landing_after_a_span_keeps_title_and_shader_bytes() {
    use super::card_merge_tests::warp_buffer;
    use super::{Card, Patch, Shadow, Side};
    use std::sync::Mutex;
    let (Some(buffer), Some(staging)) = (warp_buffer(), warp_buffer()) else {
        return;
    };
    const LEN: usize = 64 << 10;
    let content: Vec<u8> = (0..LEN).map(|i| (i * 7 + 3) as u8).collect();
    let mut changed: Vec<usize> = Vec::new();
    let mut i = 1000;
    while i < 9000 {
        changed.extend(i..i + 1 + i % 3);
        i += 8 + i % 5;
    }
    let mut new = content.clone();
    for &i in &changed {
        new[i] ^= 0x5a;
    }
    // Shader bytes: between two runs, before the first, after the last.
    let shader = [changed[0] - 1, changed[0] + 4, changed[changed.len() - 1] + 2, 20_000];
    for s in shader {
        assert!(!changed.contains(&s));
    }
    let mut words = Vec::new();
    page_words(0, &new, &content, &mut words);
    let exact = changed_runs(&new, &content).into_iter().map(|(r, m)| (r, r + m)).collect::<Vec<_>>();
    // `how`: 0 runs pending, 1 span pending, 2 span sent in send 7, 3 span
    // sent in send 6, 4 runs sent in send 6, 5 one coarse range pending.
    let land = |how: u8| -> Vec<u8> {
        let mut title = new.clone();
        let mut side = new.clone();
        let span = PageSpan::new(words.clone());
        let patch = match how {
            0 => Patch { ranges: exact.clone(), ..Default::default() },
            1 => Patch { spans: vec![span], ..Default::default() },
            2 => Patch { sent: vec![(span, 7)], ..Default::default() },
            3 => Patch { sent: vec![(span, 6)], ..Default::default() },
            5 => Patch { ranges: vec![(span.from, span.to)], ..Default::default() },
            _ => Patch::default(),
        };
        let sent = if how == 4 { exact.iter().map(|&(a, b)| (a, b, 6)).collect() } else { Vec::new() };
        let mut shadow = Shadow::new(buffer.clone(), side.as_mut_ptr(), 0, title.as_mut_ptr() as u64, LEN as u64);
        shadow.card = Some(Card {
            cpu: Side::Buffer(staging.clone()),
            before: Mutex::new(None),
            dirty: Mutex::default(),
            sent: Mutex::new(sent),
            masked: Mutex::default(),
            patch: Mutex::new(patch),
            size: 0,
            me: std::sync::OnceLock::new(),
        });
        // The flight's slot: the card's side before the patch ran, with the
        // shader's bytes.
        let mut slot = content.clone();
        for s in shader {
            slot[s] ^= 0xc3;
        }
        shadow.card_merge_relay(0, &slot, 6, 6, &mut Vec::new());
        drop(shadow);
        title
    };
    let mut want = new.clone();
    for s in shader {
        want[s] ^= 0xc3;
    }
    assert_eq!(land(0), want, "runs pending");
    for how in [1u8, 2] {
        let got = land(how);
        for &i in &changed {
            assert_eq!(got[i], new[i], "how {how}: the title's byte {i} was lost");
        }
        for s in shader {
            assert_eq!(got[s], want[s], "how {how}: the shader's byte {s} did not land");
        }
        assert_eq!(got, want, "how {how}");
    }
    // Sent before the fetch: the card's bytes are the newer ones, with
    // the runs as with the span.
    assert_eq!(land(3), land(4));
    assert!(changed.iter().all(|&i| land(3)[i] == content[i]));
    // The span as a coarse range: shader bytes inside it are lost.
    let coarse = land(5);
    assert_ne!(coarse[shader[1]], want[shader[1]]);
}
