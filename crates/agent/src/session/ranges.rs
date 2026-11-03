//! Sets of inclusive integer ranges.
//!
//! Used to track which `source_seq` ranges of a remote session's history are
//! present locally. Ranges are kept sorted, non-overlapping and merged when
//! adjacent, so `[1, 5]` + `[6, 9]` is `[1, 9]`.

/// A merged set of inclusive `(start, end)` ranges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeSet(Vec<(i64, i64)>);

impl RangeSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_ranges(ranges: impl IntoIterator<Item = (i64, i64)>) -> Self {
        let mut set = Self::new();
        for (start, end) in ranges {
            set.insert(start, end);
        }
        set
    }

    /// Add `[start, end]`, merging with overlapping or adjacent ranges.
    /// Empty ranges (`start > end`) are ignored.
    pub fn insert(&mut self, start: i64, end: i64) {
        if start > end {
            return;
        }
        let (mut start, mut end) = (start, end);
        let mut merged = Vec::with_capacity(self.0.len() + 1);
        let mut placed = false;
        for &(a, b) in &self.0 {
            if b.saturating_add(1) < start {
                merged.push((a, b));
            } else if a > end.saturating_add(1) {
                if !placed {
                    merged.push((start, end));
                    placed = true;
                }
                merged.push((a, b));
            } else {
                start = start.min(a);
                end = end.max(b);
            }
        }
        if !placed {
            merged.push((start, end));
        }
        self.0 = merged;
    }

    /// Remove `[start, end]` from the set, splitting ranges as needed.
    pub fn remove(&mut self, start: i64, end: i64) {
        if start > end {
            return;
        }
        let mut kept = Vec::with_capacity(self.0.len() + 1);
        for &(a, b) in &self.0 {
            if b < start || a > end {
                kept.push((a, b));
                continue;
            }
            if a < start {
                kept.push((a, start - 1));
            }
            if b > end {
                kept.push((end + 1, b));
            }
        }
        self.0 = kept;
    }

    /// True when every value of `[start, end]` is covered.
    pub fn covers(&self, start: i64, end: i64) -> bool {
        start > end || self.0.iter().any(|&(a, b)| a <= start && end <= b)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn ranges(&self) -> &[(i64, i64)] {
        &self.0
    }

    pub fn contains(&self, value: i64) -> bool {
        self.0.iter().any(|&(a, b)| a <= value && value <= b)
    }

    /// Highest covered value.
    pub fn max_end(&self) -> Option<i64> {
        self.0.last().map(|&(_, b)| b)
    }

    /// End of the range that starts at (or before) 1: the highest value `n`
    /// such that `1..=n` is fully covered. `0` when `1` is not covered.
    pub fn contiguous_end(&self) -> i64 {
        match self.0.first() {
            Some(&(a, b)) if a <= 1 => b,
            _ => 0,
        }
    }

    /// The parts of `[start, end]` that are not covered, ascending.
    pub fn gaps(&self, start: i64, end: i64) -> Vec<(i64, i64)> {
        let mut gaps = Vec::new();
        if start > end {
            return gaps;
        }
        let mut next = start;
        for &(a, b) in &self.0 {
            if b < next {
                continue;
            }
            if a > end {
                break;
            }
            if a > next {
                gaps.push((next, a - 1));
            }
            if b >= end {
                return gaps;
            }
            next = b + 1;
        }
        if next <= end {
            gaps.push((next, end));
        }
        gaps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_merges_overlapping_and_adjacent_ranges() {
        let mut set = RangeSet::new();
        set.insert(10, 20);
        set.insert(30, 40);
        assert_eq!(set.ranges(), &[(10, 20), (30, 40)]);
        set.insert(21, 29);
        assert_eq!(set.ranges(), &[(10, 40)]);
        set.insert(5, 12);
        set.insert(38, 50);
        assert_eq!(set.ranges(), &[(5, 50)]);
        set.insert(60, 55);
        assert_eq!(set.ranges(), &[(5, 50)], "empty range ignored");
    }

    #[test]
    fn insert_bridges_several_ranges_and_keeps_order() {
        let mut set = RangeSet::from_ranges([(1, 3), (10, 12), (20, 22), (40, 41)]);
        set.insert(11, 21);
        assert_eq!(set.ranges(), &[(1, 3), (10, 22), (40, 41)]);
        set.insert(30, 31);
        assert_eq!(set.ranges(), &[(1, 3), (10, 22), (30, 31), (40, 41)]);
    }

    #[test]
    fn gaps_report_uncovered_parts_of_a_window() {
        let set = RangeSet::from_ranges([(5, 10), (20, 30)]);
        assert_eq!(set.gaps(1, 40), vec![(1, 4), (11, 19), (31, 40)]);
        assert_eq!(set.gaps(6, 9), vec![]);
        assert_eq!(set.gaps(8, 25), vec![(11, 19)]);
        assert_eq!(set.gaps(31, 31), vec![(31, 31)]);
        assert_eq!(RangeSet::new().gaps(1, 3), vec![(1, 3)]);
        assert_eq!(set.gaps(9, 3), vec![]);
    }

    #[test]
    fn contiguous_end_requires_coverage_from_one() {
        assert_eq!(RangeSet::new().contiguous_end(), 0);
        assert_eq!(RangeSet::from_ranges([(5, 9)]).contiguous_end(), 0);
        assert_eq!(
            RangeSet::from_ranges([(1, 9), (20, 30)]).contiguous_end(),
            9
        );
        assert_eq!(
            RangeSet::from_ranges([(1, 9), (10, 30)]).contiguous_end(),
            30
        );
        assert_eq!(RangeSet::from_ranges([(5, 9)]).max_end(), Some(9));
    }

    #[test]
    fn remove_splits_and_trims_ranges() {
        let mut set = RangeSet::from_ranges([(1, 10), (20, 30)]);
        set.remove(4, 6);
        assert_eq!(set.ranges(), &[(1, 3), (7, 10), (20, 30)]);
        set.remove(9, 25);
        assert_eq!(set.ranges(), &[(1, 3), (7, 8), (26, 30)]);
        set.remove(0, 100);
        assert!(set.is_empty());
        assert!(RangeSet::from_ranges([(1, 5)]).covers(2, 4));
        assert!(!RangeSet::from_ranges([(1, 5), (7, 9)]).covers(4, 8));
    }

    #[test]
    fn extreme_bounds_do_not_overflow() {
        let mut set = RangeSet::new();
        set.insert(i64::MAX - 1, i64::MAX);
        set.insert(1, 5);
        assert_eq!(set.gaps(1, i64::MAX), vec![(6, i64::MAX - 2)]);
    }
}
