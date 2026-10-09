//! Project declaration alternatives for a symbol lookup that found no match.

use rapidfuzz::distance::levenshtein::{Args, BatchComparator};
use rift_index::IndexedFile;
use rift_protocol::read::{SYMBOL_ALTERNATIVES_MAX, SymbolId};
use rift_syntax::SyntaxSymbol;

/// One candidate retained while selecting the nearest declarations.
#[derive(Debug)]
struct Alternative<'source> {
    file: &'source IndexedFile,
    symbol: &'source SyntaxSymbol,
    distance: usize,
}

impl Alternative<'_> {
    /// Distance and the declaration's stable placement define one total order.
    fn order(&self) -> (usize, &str, &str) {
        (
            self.distance,
            &self.symbol.qualified_name,
            self.file.path().as_str(),
        )
    }

    /// Uses the same identity constructor as ordinary declaration reads.
    fn identity(&self) -> SymbolId {
        SymbolId(rift_core::symbol_identity(
            &self.file.syntax().language().identity_segment(),
            self.file.path().as_str(),
            &self.symbol.qualified_name,
        ))
    }
}

/// Maximum normalization, comparison, and traversal work for one optional ranking.
const WORK_MAX: usize = 16_777_216;
/// Rust lowercase mappings contain at most three characters, each at most four bytes.
/// Reserving this multiple before allocation also covers the source traversal.
const LOWERCASE_BYTES_PER_SOURCE_BYTE: usize = 12;
/// Explains why a named miss carries no closest declarations.
pub(crate) const UNAVAILABLE_DETAIL: &str = "no symbols available with this name; closest alternatives unavailable at the work bound; \
     select a language or use a shorter exact name";

/// Remaining work for one complete ranking; a failed reservation stops the pass.
struct Work {
    remaining: usize,
}

impl Work {
    fn claim(&mut self, amount: usize) -> Result<(), ()> {
        self.remaining = self.remaining.checked_sub(amount).ok_or(())?;
        Ok(())
    }

    /// Reserve the largest lowercase output before its allocation.
    fn lowercase(&mut self, value: &str) -> Result<String, ()> {
        let amount = value
            .len()
            .checked_mul(LOWERCASE_BYTES_PER_SOURCE_BYTE)
            .ok_or(())?;
        self.claim(amount)?;
        Ok(value.to_lowercase())
    }

    /// Actual lowercase character lengths bound the distance matrix and input copy.
    fn distance(
        &mut self,
        comparator: &BatchComparator<char>,
        query_characters: usize,
        name: &str,
        cutoff: usize,
    ) -> Result<Option<usize>, ()> {
        let name = self.lowercase(name)?;
        let amount = name
            .chars()
            .count()
            .checked_mul(query_characters + 1)
            .ok_or(())?;
        self.claim(amount)?;
        Ok(comparator.distance_with_args(name.chars(), &Args::default().score_cutoff(cutoff)))
    }
}

/// Returns nearest declarations only after comparing the entire selected set.
///
/// Every visited file and declaration costs one work unit, including language exclusions.
/// Normalization reserves twelve units per source byte before allocation. Each distance
/// reserves (Q + 1) * L units for actual lowercase query and candidate character lengths.
/// Stable insertion reserves three times the new qualified-name and path bytes. The fixed
/// `WORK_MAX` limits traversal to that many entries and distance cells to that many cells,
/// independently of publication size. Only three candidates survive. Exhaustion discards
/// every candidate, so a prefix never claims to be the closest set.
pub(crate) fn symbols<'source>(
    files: impl IntoIterator<Item = &'source IndexedFile>,
    name: &str,
    language: Option<&rift_protocol::read::Language>,
) -> Result<Vec<SymbolId>, ()> {
    symbols_with_work(files, name, language, WORK_MAX)
}

/// The same complete ranking with an explicit allowance for boundary tests.
fn symbols_with_work<'source>(
    files: impl IntoIterator<Item = &'source IndexedFile>,
    name: &str,
    language: Option<&rift_protocol::read::Language>,
    allowance: usize,
) -> Result<Vec<SymbolId>, ()> {
    let mut work = Work {
        remaining: allowance,
    };
    let name = work.lowercase(name)?;
    let query_characters = name.chars().count();
    work.claim(query_characters)?;
    let comparator = BatchComparator::new(name.chars());
    let mut nearest: Vec<Alternative<'_>> = Vec::with_capacity(SYMBOL_ALTERNATIVES_MAX + 1);
    for file in files {
        select_file(
            file,
            language,
            &mut work,
            &comparator,
            query_characters,
            &mut nearest,
        )?;
    }
    Ok(nearest.iter().map(Alternative::identity).collect())
}

/// Charges one file and its declarations before selecting its language and names.
fn select_file<'source>(
    file: &'source IndexedFile,
    language: Option<&rift_protocol::read::Language>,
    work: &mut Work,
    comparator: &BatchComparator<char>,
    query_characters: usize,
    nearest: &mut Vec<Alternative<'source>>,
) -> Result<(), ()> {
    work.claim(1)?;
    let selected = language
        .is_none_or(|language| crate::read::language_selects(language, file.syntax().language()));
    for symbol in file.syntax().symbols() {
        work.claim(1)?;
        if !selected {
            continue;
        }
        let cutoff = nearest
            .last()
            .filter(|_| nearest.len() == SYMBOL_ALTERNATIVES_MAX)
            .map_or(usize::MAX, |candidate| candidate.distance);
        let Some(distance) = distance(work, comparator, query_characters, symbol, cutoff)? else {
            continue;
        };
        let candidate = Alternative {
            file,
            symbol,
            distance,
        };
        let ordering = symbol
            .qualified_name
            .len()
            .checked_add(file.path().as_str().len())
            .and_then(|bytes| bytes.checked_add(1))
            .and_then(|bytes| bytes.checked_mul(SYMBOL_ALTERNATIVES_MAX))
            .ok_or(())?;
        work.claim(ordering)?;
        let position = nearest
            .iter()
            .position(|other| candidate.order() < other.order())
            .unwrap_or(nearest.len());
        nearest.insert(position, candidate);
        nearest.truncate(SYMBOL_ALTERNATIVES_MAX);
    }
    Ok(())
}

/// Short and qualified names share the smaller accepted distance.
fn distance(
    work: &mut Work,
    comparator: &BatchComparator<char>,
    query_characters: usize,
    symbol: &SyntaxSymbol,
    cutoff: usize,
) -> Result<Option<usize>, ()> {
    let short = work.distance(comparator, query_characters, &symbol.name, cutoff)?;
    if symbol.name == symbol.qualified_name {
        return Ok(short);
    }
    let qualified = work.distance(
        comparator,
        query_characters,
        &symbol.qualified_name,
        short.unwrap_or(cutoff),
    )?;
    Ok(qualified.or(short))
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use rift_core::{SourceVisibility, TextFileInclusion};
    use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};

    use super::{UNAVAILABLE_DETAIL, Work, symbols_with_work};

    type TestResult = Result<(), Box<dyn Error>>;

    fn index(root: &std::path::Path) -> Result<WorkspaceIndex, rift_error::RiftError> {
        WorkspaceIndex::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
    }

    #[test]
    fn complete_ranking_accepts_exact_allowance_and_refuses_one_more_unit() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let index = index(directory.path())?;
        // Query normalization 13, file/declaration 2, candidate normalization 72,
        // distance 12, stable insertion 39: the complete ranking costs 138.
        let complete = symbols_with_work(index.files(), "x", None, 138)
            .map_err(|()| "exact allowance refused")?;
        assert_eq!(complete.len(), 1);
        assert!(symbols_with_work(index.files(), "x", None, 137).is_err());
        // The next file is inspected even if it contains no selected declaration.
        std::fs::write(directory.path().join("z.rs"), "")?;
        let index = super::tests::index(directory.path())?;
        assert!(symbols_with_work(index.files(), "x", None, 138).is_err());
        assert_eq!(
            symbols_with_work(index.files(), "x", None, 139).ok(),
            Some(complete)
        );
        Ok(())
    }

    #[test]
    fn language_exclusions_charge_file_and_declaration_traversal() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let index = index(directory.path())?;
        let language = rift_protocol::read::Language {
            name: "go".to_owned(),
            dialect: None,
        };
        assert_eq!(
            symbols_with_work(index.files(), "x", Some(&language), 15),
            Ok(Vec::new())
        );
        assert!(symbols_with_work(index.files(), "x", Some(&language), 14).is_err());
        Ok(())
    }

    #[test]
    fn unicode_expansion_is_charged_before_distance() {
        let query = "İ".to_lowercase();
        assert_eq!(query.chars().count(), 2);
        let comparator = rapidfuzz::distance::levenshtein::BatchComparator::new(query.chars());
        let mut exact = Work { remaining: 30 };
        assert_eq!(exact.distance(&comparator, 2, "İ", usize::MAX), Ok(Some(0)));
        assert_eq!(exact.remaining, 0);
        let mut short = Work { remaining: 29 };
        assert!(short.distance(&comparator, 2, "İ", usize::MAX).is_err());
        assert!(UNAVAILABLE_DETAIL.chars().count() <= 4096);
    }

    #[test]
    fn exhaustion_discards_candidates_already_compared() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("a.rs"), "pub fn beacon() {}\n")?;
        std::fs::write(directory.path().join("z.rs"), "pub fn x() {}\n")?;
        let index = index(directory.path())?;
        assert!(symbols_with_work(index.files(), "x", None, 132).is_err());
        let complete = symbols_with_work(index.files(), "x", None, 1_000)
            .map_err(|()| "complete ranking refused")?;
        assert!(complete[0].0.ends_with("/x"));
        Ok(())
    }
}
