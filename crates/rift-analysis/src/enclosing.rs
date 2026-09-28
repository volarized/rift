//! Which declaration contains a byte range: one answer for every caller that maps an
//! offset in a file to its smallest containing declaration.
//!
//! Declaration spans come from syntax trees, so they nest or stay apart.
//! [`EnclosingDefinitions`] sorts them once and links each span to the nearest span
//! containing it; a lookup bisects to the last span starting at or before the range and
//! climbs those links, so it costs the nesting depth rather than the declaration count. A
//! span list that overlaps without nesting falls back to a scan over the spans starting at
//! or before the range, which answers the same.

/// One unit's declaration spans, sorted for bisection, each carrying the caller's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnclosingDefinitions<T> {
    /// `(start, end, value)`, by start, then the longer span first, then insertion order
    /// reversed, so the last span at one start is the smallest and, among equal spans,
    /// the first inserted.
    spans: Vec<(u64, u64, T)>,
    /// For each span, the nearest earlier span containing it.
    parents: Vec<Option<usize>>,
    /// Whether every two spans nest or stay apart, so the climb answers exactly.
    nested: bool,
}

impl<T> EnclosingDefinitions<T> {
    /// Sorts `spans` and links each to its nearest container. A span whose end precedes
    /// its start is left out.
    #[must_use]
    pub fn new(spans: impl IntoIterator<Item = (u64, u64, T)>) -> Self {
        let mut ordered: Vec<(usize, (u64, u64, T))> = spans
            .into_iter()
            .filter(|(start, end, _)| start <= end)
            .enumerate()
            .collect();
        ordered.sort_by(|(left_order, left), (right_order, right)| {
            left.0
                .cmp(&right.0)
                .then(right.1.cmp(&left.1))
                .then(right_order.cmp(left_order))
        });
        let spans: Vec<(u64, u64, T)> = ordered.into_iter().map(|(_, span)| span).collect();
        let mut parents = Vec::with_capacity(spans.len());
        let mut open: Vec<usize> = Vec::new();
        let mut nested = true;
        for (index, (start, end, _)) in spans.iter().enumerate() {
            let (parent, contains) = nearest_open_container(&spans, &mut open, *start, *end);
            nested &= contains;
            parents.push(parent);
            open.push(index);
        }
        Self {
            spans,
            parents,
            nested,
        }
    }

    /// The smallest span containing `start..end`, `None` when no span does. Equal spans
    /// answer the first inserted.
    #[must_use]
    pub fn resolve(&self, start: u64, end: u64) -> Option<&T> {
        let prefix_end = self
            .spans
            .partition_point(|(span_start, ..)| *span_start <= start);
        if !self.nested {
            return self.spans[..prefix_end]
                .iter()
                .rev()
                .filter(|(_, span_end, _)| end <= *span_end)
                .min_by_key(|(span_start, span_end, _)| (span_end - span_start, *span_start))
                .map(|(.., value)| value);
        }
        let mut candidate = prefix_end.checked_sub(1);
        while let Some(index) = candidate {
            let (_, span_end, value) = &self.spans[index];
            if end <= *span_end {
                return Some(value);
            }
            candidate = self.parents[index];
        }
        None
    }

    /// How many spans this holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether this holds no span.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// Closes every open span that ends before `start..end` begins, and answers the nearest
/// span still open with whether it contains the whole range.
///
/// `open` holds, innermost last, the spans that start at or before `start` and have not
/// ended; spans arrive sorted by start, so a span the new one starts past can contain no
/// later span either. Each span is pushed and popped once over a whole build.
fn nearest_open_container<T>(
    spans: &[(u64, u64, T)],
    open: &mut Vec<usize>,
    start: u64,
    end: u64,
) -> (Option<usize>, bool) {
    while let Some(&top) = open.last() {
        let (top_start, top_end, _) = &spans[top];
        let begins_past_it = start >= *top_end && *top_start != start;
        if !begins_past_it {
            return (Some(top), end <= *top_end);
        }
        open.pop();
    }
    (None, true)
}

#[cfg(test)]
mod tests {
    use super::EnclosingDefinitions;

    /// The smallest containing span by a scan over every span, ties to the first inserted.
    fn scanned(spans: &[(u64, u64, usize)], start: u64, end: u64) -> Option<usize> {
        spans
            .iter()
            .filter(|(span_start, span_end, _)| *span_start <= start && end <= *span_end)
            .min_by_key(|(span_start, span_end, value)| {
                (span_end - span_start, *span_start, *value)
            })
            .map(|(.., value)| *value)
    }

    #[test]
    fn the_innermost_span_answers_and_a_gap_climbs_to_the_container() {
        let spans = [(0, 100, 0), (10, 40, 1), (12, 20, 2), (50, 90, 3)];
        let enclosing = EnclosingDefinitions::new(spans);
        assert_eq!(enclosing.resolve(15, 16), Some(&2));
        assert_eq!(enclosing.resolve(25, 30), Some(&1));
        assert_eq!(enclosing.resolve(45, 46), Some(&0));
        assert_eq!(enclosing.resolve(60, 95), Some(&0));
        assert_eq!(enclosing.resolve(100, 101), None);
        assert_eq!(EnclosingDefinitions::<usize>::new([]).resolve(0, 0), None);
    }

    #[test]
    fn equal_spans_answer_the_first_inserted() {
        let enclosing =
            EnclosingDefinitions::new([(5, 9, "variable"), (5, 9, "function"), (0, 20, "file")]);
        assert_eq!(enclosing.resolve(6, 7), Some(&"variable"));
    }

    #[test]
    fn a_span_list_that_overlaps_without_nesting_answers_like_a_scan() {
        let spans = [(0, 30, 0), (20, 50, 1), (25, 28, 2), (40, 45, 3)];
        let enclosing = EnclosingDefinitions::new(spans);
        for (start, end) in [(21, 22), (26, 27), (29, 31), (41, 42), (46, 47), (0, 1)] {
            assert_eq!(
                enclosing.resolve(start, end).copied(),
                scanned(&spans, start, end)
            );
        }
    }

    #[test]
    fn every_offset_of_a_generated_tree_answers_like_a_scan() {
        let mut spans = Vec::new();
        let mut value = 0;
        for outer in 0..20_u64 {
            let base = outer * 1_000;
            spans.push((base, base + 900, value));
            value += 1;
            for inner in 0..8_u64 {
                let start = base + 10 + inner * 110;
                spans.push((start, start + 100, value));
                value += 1;
                spans.push((start + 20, start + 60, value));
                value += 1;
            }
        }
        let enclosing = EnclosingDefinitions::new(spans.clone());
        assert_eq!(enclosing.len(), spans.len());
        for offset in 0..20_100 {
            assert_eq!(
                enclosing.resolve(offset, offset + 1).copied(),
                scanned(&spans, offset, offset + 1),
                "offset={offset}"
            );
        }
    }
}
