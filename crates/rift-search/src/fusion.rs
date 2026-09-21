//! Weighted reciprocal rank fusion, and the two per-file aggregations it feeds.
//!
//! The lexical and the vector ranking score in different units: one is a BM25
//! rank, the other a cosine. Neither is comparable to the other, and neither
//! becomes comparable by rescaling. What both tiers agree on is the position
//! they put a candidate in, so fusion reads positions alone:
//!
//! ```text
//! score(c) = sum over rankings i of  weight_i / (fusion_k + rank_i(c))
//! ```
//!
//! `rank_i(c)` is the 1-based position of `c` in ranking `i`, and a ranking
//! that never returned `c` contributes nothing. `fusion_k` is the operator's
//! constant: it flattens the head of each ranking, so a first place is worth
//! more than a second without being worth more than every other ranking's
//! opinion combined.
//!
//! Everything here is sans-I/O: the rankings arrive as values two tiers already
//! produced.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use rift_core::ProjectPath;

use crate::similarity::VectorMatch;

/// One vector match together with the file whose declaration produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct DeclarationMatch {
    file: ProjectPath,
    matched: VectorMatch,
}

impl DeclarationMatch {
    /// Pairs one vector match with the file its declaration lives in.
    #[must_use]
    pub const fn new(file: ProjectPath, matched: VectorMatch) -> Self {
        Self { file, matched }
    }

    /// The file the matched declaration lives in.
    #[must_use]
    pub const fn file(&self) -> &ProjectPath {
        &self.file
    }

    /// The match that declaration reached.
    #[must_use]
    pub const fn matched(&self) -> &VectorMatch {
        &self.matched
    }
}

/// Collapses `matches` to one entry per file, keeping the best similarity any
/// declaration in that file reached.
///
/// A file's vector rank is the best its declarations reached: a query that
/// matched one function has matched the file that holds it, and the file's
/// other declarations say nothing about that.
///
/// Files come back in descending best similarity, ties broken by path
/// ascending. The loop runs once over a slice whose length is already the
/// caller's own bound.
#[must_use]
pub fn best_per_file(matches: &[DeclarationMatch]) -> Vec<DeclarationMatch> {
    let mut best: BTreeMap<&ProjectPath, &DeclarationMatch> = BTreeMap::new();
    for entry in matches {
        keep_better(&mut best, entry);
    }
    let mut kept: Vec<DeclarationMatch> = best.into_values().cloned().collect();
    kept.sort_unstable_by(strongest_file_first);
    kept
}

/// Keeps `entry` when its file holds nothing better yet.
///
/// A tie keeps the entry already held, so one input order produces one answer.
fn keep_better<'a>(
    best: &mut BTreeMap<&'a ProjectPath, &'a DeclarationMatch>,
    entry: &'a DeclarationMatch,
) {
    let better = match best.get(entry.file()) {
        Some(held) => entry.matched().similarity() > held.matched().similarity(),
        None => true,
    };
    if better {
        best.insert(entry.file(), entry);
    }
}

/// Keeps at most `per_file_max` matches from any one file, in the order given.
///
/// Without the cap, one large file's declarations crowd every other file out of
/// a bounded candidate list, and the tier that follows never sees the file that
/// held the second-best answer.
///
/// The input order is preserved exactly: only the overflow past `per_file_max`
/// is dropped, and a cap of zero keeps nothing. The loop runs once over a slice
/// whose length is already the caller's own bound.
#[must_use]
pub fn spread_per_file(matches: &[DeclarationMatch], per_file_max: usize) -> Vec<DeclarationMatch> {
    if per_file_max == 0 {
        return Vec::new();
    }
    let mut taken: BTreeMap<&ProjectPath, usize> = BTreeMap::new();
    let mut kept: Vec<DeclarationMatch> = Vec::with_capacity(matches.len());
    for entry in matches {
        let count = taken.entry(entry.file()).or_insert(0);
        if *count < per_file_max {
            *count += 1;
            kept.push(entry.clone());
        }
    }
    kept
}

/// Descending similarity, then path ascending.
fn strongest_file_first(one: &DeclarationMatch, other: &DeclarationMatch) -> Ordering {
    other
        .matched()
        .similarity()
        .total_cmp(&one.matched().similarity())
        .then_with(|| one.file().cmp(other.file()))
}

#[cfg(test)]
mod tests {
    use super::{DeclarationMatch, keep_better, strongest_file_first};
    use crate::similarity::VectorMatch;
    use rift_core::ProjectPath;
    use std::cmp::Ordering;
    use std::collections::BTreeMap;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn declaration(
        path: &str,
        digest: &str,
        similarity: f32,
    ) -> Result<DeclarationMatch, Box<dyn std::error::Error>> {
        let file = ProjectPath::new(path.to_owned())?;
        Ok(DeclarationMatch::new(
            file,
            VectorMatch::new(digest.to_owned(), similarity),
        ))
    }

    #[test]
    fn test_the_file_order_is_similarity_then_path() -> TestResult {
        let strong = declaration("src/b.rs", "aaa", 0.9)?;
        let weak = declaration("src/a.rs", "bbb", 0.1)?;
        assert_eq!(strongest_file_first(&strong, &weak), Ordering::Less);
        let tied = declaration("src/c.rs", "ccc", 0.9)?;
        assert_eq!(strongest_file_first(&strong, &tied), Ordering::Less);
        assert_eq!(strongest_file_first(&tied, &strong), Ordering::Greater);
        Ok(())
    }

    #[test]
    fn test_a_file_keeps_the_first_of_two_equally_good_declarations() -> TestResult {
        let first = declaration("src/a.rs", "aaa", 0.5)?;
        let second = declaration("src/a.rs", "bbb", 0.5)?;
        let mut best: BTreeMap<&ProjectPath, &DeclarationMatch> = BTreeMap::new();
        keep_better(&mut best, &first);
        keep_better(&mut best, &second);
        assert_eq!(
            best.get(first.file()).map(|held| held.matched().digest()),
            Some("aaa")
        );
        Ok(())
    }
}
