//! The coordinates of one spreadsheet axis a sheet hides, kept as runs: a
//! range of any length is one entry, and a lookup, a scan for the first shown
//! coordinate and a count of the shown ones cost a binary search.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    start: u64,
    end: u64,
    /// The coordinates the runs before this one hide.
    before: u64,
}

/// Sorted runs of hidden coordinates, none empty, overlapping or adjacent to
/// another, so the coordinate after a run is always shown.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct HiddenRuns {
    runs: Vec<Run>,
}

impl HiddenRuns {
    /// The runs that cover exactly the coordinates of `spans`.
    pub(crate) fn of(mut spans: Vec<Range<u64>>) -> Self {
        spans.sort_unstable_by_key(|span| span.start);
        let mut runs: Vec<Run> = Vec::new();
        let mut hidden = 0u64;
        for span in spans.into_iter().filter(|span| span.start < span.end) {
            match runs.last_mut() {
                Some(last) if span.start <= last.end => {
                    let grown = span.end.saturating_sub(last.end);
                    last.end = last.end.max(span.end);
                    hidden = hidden.saturating_add(grown);
                }
                _ => {
                    runs.push(Run { start: span.start, end: span.end, before: hidden });
                    hidden = hidden.saturating_add(span.end - span.start);
                }
            }
        }
        Self { runs }
    }

    pub(crate) fn contains(&self, at: u64) -> bool {
        self.runs.get(self.first_ending_after(at)).is_some_and(|run| run.start <= at)
    }

    /// How many coordinates the runs hide.
    pub(crate) fn count(&self) -> u64 {
        self.runs.last().map_or(0, |run| run.before.saturating_add(run.end - run.start))
    }

    /// How many coordinates of `span` no run hides.
    pub(crate) fn shown_in(&self, span: Range<u64>) -> u64 {
        if span.start >= span.end {
            return 0;
        }
        let hidden = self.hidden_below(span.end) - self.hidden_below(span.start);
        span.end - span.start - hidden
    }

    /// The first coordinate of `span` no run hides.
    pub(crate) fn first_shown(&self, span: Range<u64>) -> Option<u64> {
        let first = match self.runs.get(self.first_ending_after(span.start)) {
            Some(run) if run.start <= span.start => run.end,
            _ => span.start,
        };
        (first < span.end).then_some(first)
    }

    /// The parts of the runs inside `span`.
    pub(crate) fn hidden_in(&self, span: Range<u64>) -> impl Iterator<Item = Range<u64>> {
        let from = self.first_ending_after(span.start);
        self.runs
            .get(from..)
            .unwrap_or_default()
            .iter()
            .take_while(move |run| run.start < span.end)
            .map(move |run| run.start.max(span.start)..run.end.min(span.end))
    }

    fn first_ending_after(&self, at: u64) -> usize {
        self.runs.partition_point(|run| run.end <= at)
    }

    fn hidden_below(&self, at: u64) -> u64 {
        match self.runs.get(self.first_ending_after(at)) {
            Some(run) => run.before.saturating_add(at.saturating_sub(run.start)),
            None => self.count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(spans: &[(u64, u64)]) -> HiddenRuns {
        HiddenRuns::of(spans.iter().map(|&(start, end)| start..end).collect())
    }

    fn within(hidden: &HiddenRuns, start: u64, end: u64) -> Vec<(u64, u64)> {
        hidden.hidden_in(start..end).map(|run| (run.start, run.end)).collect()
    }

    #[test]
    fn overlapping_adjacent_nested_and_empty_spans_join_into_sorted_runs() {
        let joined =
            runs(&[(9, 9), (20, 30), (5, 7), (7, 8), (22, 25), (8, 9), (12, 12), (40, 41)]);
        assert_eq!(joined, runs(&[(5, 9), (20, 30), (40, 41)]));
        assert_eq!(joined.count(), 15);
        assert_eq!(HiddenRuns::of(Vec::new()), HiddenRuns::default());
    }

    #[test]
    fn a_coordinate_is_hidden_exactly_inside_a_run() {
        let hidden = runs(&[(5, 9), (20, 30)]);
        let at: Vec<u64> = (0..32).filter(|&at| hidden.contains(at)).collect();
        assert_eq!(at, [5, 6, 7, 8, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29]);
        assert!(!runs(&[]).contains(0));
        let all = runs(&[(0, u64::MAX)]);
        assert!(all.contains(u64::MAX - 1));
        assert!(!all.contains(u64::MAX));
    }

    #[test]
    fn the_first_shown_coordinate_skips_the_run_that_holds_the_start() {
        let hidden = runs(&[(5, 9), (20, 30)]);
        for (start, end, first) in [
            (0, 3, Some(0)),
            (5, 20, Some(9)),
            (6, 10, Some(9)),
            (6, 9, None),
            (20, 30, None),
            (20, 31, Some(30)),
            (9, 9, None),
            (30, u64::MAX, Some(30)),
        ] {
            assert_eq!(hidden.first_shown(start..end), first, "{start}..{end}");
        }
        assert_eq!(runs(&[(0, u64::MAX)]).first_shown(0..u64::MAX), None);
    }

    #[test]
    fn the_shown_coordinates_of_a_span_are_counted_without_visiting_them() {
        let hidden = runs(&[(5, 9), (20, 30)]);
        for (start, end, shown) in [
            (0, 40, 26),
            (0, 5, 5),
            (3, 7, 2),
            (6, 8, 0),
            (8, 21, 11),
            (25, 35, 5),
            (30, 30, 0),
            (35, 30, 0),
            (0, 0, 0),
        ] {
            assert_eq!(hidden.shown_in(start..end), shown, "{start}..{end}");
        }
        let huge = runs(&[(1, u64::MAX - 1)]);
        assert_eq!(huge.shown_in(0..u64::MAX), 2);
        assert_eq!(huge.count(), u64::MAX - 2);
    }

    #[test]
    fn the_hidden_parts_of_a_span_are_cut_to_it() {
        let hidden = runs(&[(5, 9), (20, 30)]);
        assert_eq!(within(&hidden, 0, 40), [(5, 9), (20, 30)]);
        assert_eq!(within(&hidden, 7, 22), [(7, 9), (20, 22)]);
        assert_eq!(within(&hidden, 9, 20), []);
        assert_eq!(within(&hidden, 25, 26), [(25, 26)]);
        assert_eq!(within(&hidden, 30, 40), []);
    }
}
