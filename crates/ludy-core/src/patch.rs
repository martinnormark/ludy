//! Planning an upload as a patch against the version underneath.
//!
//! A file written through the mount is a journal of byte ranges over a cloud
//! version. Uploading it should move only what changed: segments of the old
//! manifest that no write touched are kept by reference, and only the
//! stretches around the writes are read, cut into shards and sent.

use ludy_cloud::shard::Segment;

/// One stretch of the new content, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// Unchanged: reuse this segment of the old manifest as it is.
    Keep(Segment),
    /// Changed or new: bytes `start..end` of the new file have to be read
    /// and cut into shards.
    Run { start: u64, end: u64 },
}

impl Piece {
    pub fn len(&self) -> u64 {
        match self {
            Piece::Keep(s) => s.len(),
            Piece::Run { start, end } => end - start,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Plan the new content of a file.
///
/// * `base`: segments of the version underneath, in order.
/// * `valid`: how many leading bytes of that version still stand. A truncate
///   lowers it for good, so growing the file again gives zeros, not old bytes.
/// * `size`: the new size.
/// * `dirty`: byte ranges written locally, sorted and non-overlapping.
///
/// A stored shard is kept only if it is untouched from end to end; a touched
/// one becomes part of a run, along with whatever was written into it. A
/// synthetic segment can be split anywhere, so only the written bytes inside
/// it become a run. The pieces returned cover `0..size` exactly.
pub fn plan(base: &[Segment], valid: u64, size: u64, dirty: &[(u64, u64)]) -> Vec<Piece> {
    let valid = valid.min(size);

    // What must be read: local writes, and the hole past the valid base.
    let mut runs: Vec<(u64, u64)> = dirty.iter().map(|&(a, b)| (a.min(size), b.min(size))).filter(|(a, b)| a < b).collect();
    if valid < size {
        runs.push((valid, size));
    }
    runs.sort_unstable();
    let runs = merge(runs);

    // Walk the base: keep what no run touches.
    let mut keeps: Vec<(u64, Segment)> = Vec::new();
    let mut extra: Vec<(u64, u64)> = Vec::new();
    let mut pos = 0u64;
    for seg in base {
        let (start, end) = (pos, pos + seg.len());
        pos = end;
        if start >= valid {
            break;
        }
        match seg {
            Segment::Shard { .. } => {
                if end <= valid && !overlaps(&runs, start, end) {
                    keeps.push((start, seg.clone()));
                } else {
                    extra.push((start, end.min(valid)));
                }
            }
            Segment::Synth { seed, off, .. } => {
                for (a, b) in subtract(start, end.min(valid), &runs) {
                    keeps.push((a, Segment::Synth { seed: *seed, off: off + (a - start), n: b - a }));
                }
            }
        }
    }
    // The base may be shorter than `valid` claims (it should not be); read the rest.
    if pos < valid {
        extra.push((pos, valid));
    }

    let mut all = runs;
    all.extend(extra);
    all.sort_unstable();
    let runs = merge(all);

    let mut pieces: Vec<(u64, Piece)> = keeps.into_iter().map(|(at, s)| (at, Piece::Keep(s))).collect();
    pieces.extend(runs.into_iter().map(|(start, end)| (start, Piece::Run { start, end })));
    pieces.sort_by_key(|(at, _)| *at);
    pieces.into_iter().map(|(_, p)| p).collect()
}

/// Merge sorted ranges that overlap or touch.
fn merge(sorted: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(sorted.len());
    for (a, b) in sorted {
        match out.last_mut() {
            Some((_, end)) if a <= *end => *end = (*end).max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

fn overlaps(runs: &[(u64, u64)], start: u64, end: u64) -> bool {
    // The first run ending after `start`, if it begins before `end`.
    let i = runs.partition_point(|&(_, b)| b <= start);
    runs.get(i).is_some_and(|&(a, _)| a < end)
}

/// The parts of `start..end` that no run covers.
fn subtract(start: u64, end: u64, runs: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut pos = start;
    let first = runs.partition_point(|&(_, b)| b <= start);
    for &(a, b) in &runs[first..] {
        if a >= end {
            break;
        }
        if a > pos {
            out.push((pos, a));
        }
        pos = pos.max(b);
    }
    if pos < end {
        out.push((pos, end));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    fn shard(tag: u8, n: u64) -> Segment {
        Segment::Shard { h: format!("{tag:02x}").repeat(32), n }
    }

    /// Four 2 MiB shards: 0..2, 2..4, 4..6, 6..8 MiB.
    fn four() -> Vec<Segment> {
        (0..4).map(|i| shard(i, 2 * MIB)).collect()
    }

    fn run_bytes(pieces: &[Piece]) -> u64 {
        pieces.iter().map(|p| if matches!(p, Piece::Run { .. }) { p.len() } else { 0 }).sum()
    }

    /// Pieces must tile `0..size` with no gap or overlap.
    fn assert_tiles(pieces: &[Piece], size: u64) {
        let mut pos = 0;
        for p in pieces {
            if let Piece::Run { start, end } = p {
                assert_eq!(*start, pos, "run starts where the previous piece ended");
                assert!(end > start);
            }
            assert!(!p.is_empty());
            pos += p.len();
        }
        assert_eq!(pos, size);
    }

    #[test]
    fn nothing_written_keeps_everything() {
        let base = four();
        let pieces = plan(&base, 8 * MIB, 8 * MIB, &[]);
        assert_eq!(pieces, base.iter().cloned().map(Piece::Keep).collect::<Vec<_>>());
    }

    #[test]
    fn a_patch_inside_one_shard_rereads_only_that_shard() {
        let base = four();
        let pieces = plan(&base, 8 * MIB, 8 * MIB, &[(3 * MIB, 3 * MIB + 4096)]);
        assert_tiles(&pieces, 8 * MIB);
        assert_eq!(
            pieces,
            vec![Piece::Keep(base[0].clone()), Piece::Run { start: 2 * MIB, end: 4 * MIB }, Piece::Keep(base[2].clone()), Piece::Keep(base[3].clone())]
        );
    }

    #[test]
    fn a_patch_straddling_two_shards_rereads_both_as_one_run() {
        let base = four();
        let pieces = plan(&base, 8 * MIB, 8 * MIB, &[(4 * MIB - 10, 4 * MIB + 10)]);
        assert_tiles(&pieces, 8 * MIB);
        assert_eq!(pieces, vec![Piece::Keep(base[0].clone()), Piece::Run { start: 2 * MIB, end: 6 * MIB }, Piece::Keep(base[3].clone())]);
    }

    #[test]
    fn a_patch_ending_exactly_on_a_boundary_leaves_the_next_shard_alone() {
        let base = four();
        let pieces = plan(&base, 8 * MIB, 8 * MIB, &[(2 * MIB, 4 * MIB)]);
        assert_eq!(pieces[0], Piece::Keep(base[0].clone()));
        assert_eq!(pieces[1], Piece::Run { start: 2 * MIB, end: 4 * MIB });
        assert_eq!(pieces[2], Piece::Keep(base[2].clone()));
    }

    #[test]
    fn an_unaligned_patch_in_a_synthetic_object_splits_it_exactly() {
        let size = 50 * GIB;
        let base = vec![Segment::Synth { seed: 7, off: 0, n: size }];
        let at = 25 * GIB + 3;
        let pieces = plan(&base, size, size, &[(at, at + 5000)]);
        assert_tiles(&pieces, size);
        assert_eq!(
            pieces,
            vec![
                Piece::Keep(Segment::Synth { seed: 7, off: 0, n: at }),
                Piece::Run { start: at, end: at + 5000 },
                Piece::Keep(Segment::Synth { seed: 7, off: at + 5000, n: size - at - 5000 }),
            ]
        );
        // The whole point: 5000 bytes are read, not 50 GB.
        assert_eq!(run_bytes(&pieces), 5000);
    }

    #[test]
    fn several_patches_in_a_synthetic_object_stay_small() {
        let size = 50 * GIB;
        let base = vec![Segment::Synth { seed: 7, off: 100, n: size }];
        let dirty = [(0, 10), (GIB, GIB + 4096), (size - 1, size)];
        let pieces = plan(&base, size, size, &dirty);
        assert_tiles(&pieces, size);
        assert_eq!(run_bytes(&pieces), 10 + 4096 + 1);
        // Offsets into the synthetic stream follow the split.
        assert_eq!(pieces[1], Piece::Keep(Segment::Synth { seed: 7, off: 110, n: GIB - 10 }));
    }

    #[test]
    fn an_append_keeps_the_whole_base() {
        let base = four();
        let pieces = plan(&base, 8 * MIB, 8 * MIB + 1000, &[(8 * MIB, 8 * MIB + 1000)]);
        assert_tiles(&pieces, 8 * MIB + 1000);
        assert_eq!(pieces.len(), 5);
        assert_eq!(pieces[4], Piece::Run { start: 8 * MIB, end: 8 * MIB + 1000 });
    }

    #[test]
    fn a_truncate_mid_shard_rereads_the_cut_shard() {
        let base = four();
        let size = 5 * MIB;
        let pieces = plan(&base, size, size, &[]);
        assert_tiles(&pieces, size);
        assert_eq!(pieces, vec![Piece::Keep(base[0].clone()), Piece::Keep(base[1].clone()), Piece::Run { start: 4 * MIB, end: 5 * MIB }]);
    }

    #[test]
    fn a_truncate_on_a_boundary_reads_nothing() {
        let base = four();
        let pieces = plan(&base, 4 * MIB, 4 * MIB, &[]);
        assert_eq!(pieces, vec![Piece::Keep(base[0].clone()), Piece::Keep(base[1].clone())]);
    }

    #[test]
    fn truncate_then_extend_does_not_bring_old_bytes_back() {
        // Cut to 3 MiB, then grown to 7 MiB: 3..7 MiB is a hole of zeros and
        // must be read as such, never taken from the old shards.
        let base = four();
        let pieces = plan(&base, 3 * MIB, 7 * MIB, &[]);
        assert_tiles(&pieces, 7 * MIB);
        assert_eq!(pieces, vec![Piece::Keep(base[0].clone()), Piece::Run { start: 2 * MIB, end: 7 * MIB }]);
    }

    #[test]
    fn truncate_then_extend_a_synthetic_object() {
        let base = vec![Segment::Synth { seed: 1, off: 0, n: 10 * GIB }];
        let pieces = plan(&base, GIB, GIB + 100, &[]);
        assert_tiles(&pieces, GIB + 100);
        assert_eq!(pieces, vec![Piece::Keep(Segment::Synth { seed: 1, off: 0, n: GIB }), Piece::Run { start: GIB, end: GIB + 100 }]);
    }

    #[test]
    fn a_new_file_is_one_run() {
        assert_eq!(plan(&[], 0, 5000, &[(0, 5000)]), vec![Piece::Run { start: 0, end: 5000 }]);
        // Written sparsely: the gaps are zeros and belong to the run too.
        assert_eq!(plan(&[], 0, 5000, &[(100, 200)]), vec![Piece::Run { start: 0, end: 5000 }]);
        assert!(plan(&[], 0, 0, &[]).is_empty());
    }

    #[test]
    fn a_fully_rewritten_file_keeps_nothing() {
        let base = four();
        assert_eq!(plan(&base, 8 * MIB, 8 * MIB, &[(0, 8 * MIB)]), vec![Piece::Run { start: 0, end: 8 * MIB }]);
    }

    #[test]
    fn mixed_base_with_shards_and_synthetic_ranges() {
        // A synthetic object that was patched before: [synth 0..4][shard 4..6][synth 6..10] MiB.
        let base =
            vec![Segment::Synth { seed: 9, off: 0, n: 4 * MIB }, shard(1, 2 * MIB), Segment::Synth { seed: 9, off: 6 * MIB, n: 4 * MIB }];
        // A write that starts in the shard and runs into the synthetic tail.
        let pieces = plan(&base, 10 * MIB, 10 * MIB, &[(5 * MIB, 7 * MIB)]);
        assert_tiles(&pieces, 10 * MIB);
        assert_eq!(
            pieces,
            vec![
                Piece::Keep(base[0].clone()),
                Piece::Run { start: 4 * MIB, end: 7 * MIB },
                Piece::Keep(Segment::Synth { seed: 9, off: 7 * MIB, n: 3 * MIB }),
            ]
        );
    }

    #[test]
    fn dirty_ranges_past_the_new_size_are_ignored() {
        let base = four();
        let pieces = plan(&base, 2 * MIB, 2 * MIB, &[(6 * MIB, 7 * MIB)]);
        assert_eq!(pieces, vec![Piece::Keep(base[0].clone())]);
    }
}
