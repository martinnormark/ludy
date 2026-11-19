//! A set of non-overlapping half-open byte ranges, kept merged.

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RangeSet {
    /// start -> end
    ranges: BTreeMap<u64, u64>,
}

impl RangeSet {
    pub fn insert(&mut self, mut start: u64, mut end: u64) {
        if start >= end {
            return;
        }
        // Absorb a range that begins at or before `start` and reaches it.
        if let Some((&s, &e)) = self.ranges.range(..=start).next_back() {
            if e >= start {
                start = s;
                end = end.max(e);
            }
        }
        // Absorb every range that begins inside the new one.
        let inside: Vec<u64> = self.ranges.range(start..=end).map(|(s, _)| *s).collect();
        for s in inside {
            if let Some(e) = self.ranges.remove(&s) {
                end = end.max(e);
            }
        }
        self.ranges.insert(start, end);
    }

    /// Drop everything at or beyond `len`.
    pub fn truncate(&mut self, len: u64) {
        let beyond: Vec<u64> = self.ranges.range(len..).map(|(s, _)| *s).collect();
        for s in beyond {
            self.ranges.remove(&s);
        }
        if let Some((_, e)) = self.ranges.iter_mut().next_back() {
            if *e > len {
                *e = len;
            }
        }
    }

    /// Every range as `(start, end)`, in order.
    pub fn ranges(&self) -> Vec<(u64, u64)> {
        self.ranges.iter().map(|(s, e)| (*s, *e)).collect()
    }

    pub fn from_ranges(ranges: &[(u64, u64)]) -> RangeSet {
        let mut set = RangeSet::default();
        for &(s, e) in ranges {
            set.insert(s, e);
        }
        set
    }

    pub fn total(&self) -> u64 {
        self.ranges.iter().map(|(s, e)| e - s).sum()
    }

    pub fn covers(&self, start: u64, end: u64) -> bool {
        if start >= end {
            return true;
        }
        matches!(self.ranges.range(..=start).next_back(), Some((_, &e)) if e >= end)
    }

    /// Split `[start, end)` into consecutive pieces, each flagged with
    /// whether it lies inside the set.
    pub fn segments(&self, start: u64, end: u64) -> Vec<(u64, u64, bool)> {
        let mut out = Vec::new();
        let mut pos = start;
        if let Some((_, &e)) = self.ranges.range(..=start).next_back() {
            if e > start {
                let to = e.min(end);
                out.push((pos, to, true));
                pos = to;
            }
        }
        for (&s, &e) in self.ranges.range(start + 1..end) {
            if pos >= end {
                break;
            }
            if s > pos {
                out.push((pos, s, false));
                pos = s;
            }
            let to = e.min(end);
            if to > pos {
                out.push((pos, to, true));
                pos = to;
            }
        }
        if pos < end {
            out.push((pos, end, false));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_touching_and_overlapping_ranges() {
        let mut r = RangeSet::default();
        r.insert(10, 20);
        r.insert(30, 40);
        r.insert(20, 30);
        assert_eq!(r.segments(0, 50), vec![(0, 10, false), (10, 40, true), (40, 50, false)]);
        r.insert(5, 45);
        assert_eq!(r.total(), 40);
        assert!(r.covers(5, 45));
        assert!(!r.covers(4, 45));
    }

    #[test]
    fn segments_inside_and_across() {
        let mut r = RangeSet::default();
        r.insert(100, 200);
        r.insert(300, 400);
        assert_eq!(r.segments(150, 350), vec![(150, 200, true), (200, 300, false), (300, 350, true)]);
        assert_eq!(r.segments(0, 50), vec![(0, 50, false)]);
        assert_eq!(r.segments(120, 130), vec![(120, 130, true)]);
        assert_eq!(r.segments(200, 300), vec![(200, 300, false)]);
    }

    #[test]
    fn truncate_clips() {
        let mut r = RangeSet::default();
        r.insert(0, 100);
        r.insert(200, 300);
        r.truncate(250);
        assert_eq!(r.total(), 150);
        r.truncate(50);
        assert_eq!(r.segments(0, 300), vec![(0, 50, true), (50, 300, false)]);
        r.truncate(0);
        assert_eq!(r.total(), 0);
    }
}
