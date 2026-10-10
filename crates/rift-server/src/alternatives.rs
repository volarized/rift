//! Project declaration alternatives for a symbol lookup that found no match.

use rapidfuzz::distance::levenshtein::{Args, BatchComparator};
use rift_error::{RiftError, errors};
use rift_index::{IndexedFile, WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::{SYMBOL_ALTERNATIVES_MAX, SymbolId};
use rift_syntax::SyntaxSymbol;

/// One candidate retained while selecting the nearest declarations.
#[derive(Debug)]
struct Alternative<'source> {
    file: &'source IndexedFile,
    symbol: &'source SyntaxSymbol,
    identity: SymbolId,
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
}

/// Remaining work for one complete ranking; a failed reservation stops the pass.
///
/// Reservation arithmetic casts before addition and multiplication. On supported 32-bit
/// and 64-bit targets, two `usize` lengths and a query length plus one fit each `u128` product.
/// Stable insertion sums two lengths and one before multiplying by three. Required work
/// therefore renders exactly, in at most 39 decimal digits, even beyond `u64`.
struct Work {
    remaining: u128,
    allowance: usize,
    lowercase_weight: usize,
}

impl Work {
    fn new(allowance: usize, lowercase_work: usize) -> Self {
        Self {
            remaining: allowance as u128,
            allowance,
            lowercase_weight: lowercase_work,
        }
    }

    fn unavailable(&self, required: u128) -> RiftError {
        errors::server::read_symbol_alternatives_unavailable()
            .work(self.allowance)
            .lowercase_work(self.lowercase_weight)
            .remaining(self.remaining)
            .required(required)
            .error()
    }

    fn claim(&mut self, amount: u128) -> Result<(), RiftError> {
        self.remaining = self
            .remaining
            .checked_sub(amount)
            .ok_or_else(|| self.unavailable(amount))?;
        Ok(())
    }

    /// Reserve the largest lowercase output before its allocation.
    fn lowercase(&mut self, value: &str) -> Result<String, RiftError> {
        let amount = (value.len() as u128) * (self.lowercase_weight as u128);
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
    ) -> Result<Option<usize>, RiftError> {
        let name = self.lowercase(name)?;
        let amount = (name.chars().count() as u128) * (query_characters as u128 + 1);
        self.claim(amount)?;
        Ok(comparator.distance_with_args(name.chars(), &Args::default().score_cutoff(cutoff)))
    }
}

/// Returns nearest declarations only after comparing the entire selected set.
///
/// Every visited file and declaration costs one work unit, including language exclusions.
/// Normalization reserves the configured lowercase work per source byte before allocation. Each distance
/// reserves (Q + 1) * L units for actual lowercase query and candidate character lengths.
/// Stable insertion reserves three times the new qualified-name and path bytes. The
/// configured work limits traversal to that many entries and distance cells to that many cells,
/// independently of publication size. Only three candidates survive. Exhaustion discards
/// every candidate, so a prefix never claims to be the closest set.
pub(crate) fn symbols<'source>(
    index: &WorkspaceIndex,
    files: impl IntoIterator<Item = &'source IndexedFile>,
    name: &str,
    language: Option<&rift_protocol::read::Language>,
    limits: WorkspaceIndexLimits,
) -> Result<Vec<SymbolId>, RiftError> {
    rank(
        index,
        files,
        name,
        language,
        Work::new(
            limits.symbol_alternatives_work_max(),
            limits.symbol_alternatives_lowercase_work(),
        ),
    )
}

/// Compares the complete selected set under the accepted work policy.
fn rank<'source>(
    index: &WorkspaceIndex,
    files: impl IntoIterator<Item = &'source IndexedFile>,
    name: &str,
    language: Option<&rift_protocol::read::Language>,
    mut work: Work,
) -> Result<Vec<SymbolId>, RiftError> {
    let name = work.lowercase(name)?;
    let query_characters = name.chars().count();
    work.claim(query_characters as u128)?;
    let comparator = BatchComparator::new(name.chars());
    let mut nearest: Vec<Alternative<'_>> = Vec::with_capacity(SYMBOL_ALTERNATIVES_MAX + 1);
    for file in files {
        select_file(
            index,
            file,
            language,
            &mut work,
            &comparator,
            query_characters,
            &mut nearest,
        )?;
    }
    Ok(nearest
        .into_iter()
        .map(|candidate| candidate.identity)
        .collect())
}

/// Charges one file and its declarations before selecting its language and names.
fn select_file<'source>(
    index: &WorkspaceIndex,
    file: &'source IndexedFile,
    language: Option<&rift_protocol::read::Language>,
    work: &mut Work,
    comparator: &BatchComparator<char>,
    query_characters: usize,
    nearest: &mut Vec<Alternative<'source>>,
) -> Result<(), RiftError> {
    work.claim(1)?;
    let selected = language
        .is_none_or(|language| crate::read::language_selects(language, file.syntax().language()));
    for symbol in file.syntax().symbols() {
        work.claim(1)?;
        if !selected {
            continue;
        }
        let assembled = index.assembled_symbol(crate::search::declared(file, symbol))?;
        let Some(identity) = assembled.identity() else {
            continue;
        };
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
            identity: SymbolId(identity.as_str().to_owned()),
            distance,
        };
        let ordering =
            (symbol.qualified_name.len() as u128 + file.path().as_str().len() as u128 + 1)
                * SYMBOL_ALTERNATIVES_MAX as u128;
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
) -> Result<Option<usize>, RiftError> {
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

    use super::Work;

    fn symbols_with_work<'source>(
        index: &WorkspaceIndex,
        files: impl IntoIterator<Item = &'source rift_index::IndexedFile>,
        name: &str,
        language: Option<&rift_protocol::read::Language>,
        allowance: usize,
    ) -> Result<Vec<rift_protocol::read::SymbolId>, rift_error::RiftError> {
        super::rank(index, files, name, language, Work::new(allowance, 12))
    }

    type TestResult = Result<(), Box<dyn Error>>;

    fn index(root: &std::path::Path) -> Result<WorkspaceIndex, Box<dyn Error>> {
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"beacon\"\nversion = \"1.0.0\"\n[lib]\npath = \"lib.rs\"\n",
        )?;
        if !root.join("lib.rs").exists() {
            std::fs::write(root.join("lib.rs"), "mod a;\nmod z;\n")?;
        }
        Ok(WorkspaceIndex::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?)
    }

    fn rust_files(index: &WorkspaceIndex) -> impl Iterator<Item = &rift_index::IndexedFile> {
        index
            .files()
            .filter(|file| file.syntax().language().name == "rust")
    }

    #[test]
    fn complete_ranking_accepts_exact_allowance_and_refuses_one_more_unit() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let index = index(directory.path())?;
        // Query normalization 13, file/declaration 2, candidate normalization 72,
        // distance 12, stable insertion 39: the complete ranking costs 138.
        let complete = symbols_with_work(&index, rust_files(&index), "x", None, 138)
            .map_err(|_| "exact allowance refused")?;
        assert_eq!(complete.len(), 1);
        assert!(symbols_with_work(&index, rust_files(&index), "x", None, 137).is_err());
        // The next file is inspected even if it contains no selected declaration.
        std::fs::write(directory.path().join("z.rs"), "")?;
        let index = super::tests::index(directory.path())?;
        assert!(symbols_with_work(&index, rust_files(&index), "x", None, 138).is_err());
        assert_eq!(
            symbols_with_work(&index, rust_files(&index), "x", None, 139).ok(),
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
            symbols_with_work(&index, rust_files(&index), "x", Some(&language), 15).ok(),
            Some(Vec::new())
        );
        assert!(symbols_with_work(&index, rust_files(&index), "x", Some(&language), 14).is_err());
        Ok(())
    }

    #[test]
    fn structural_facts_without_namespace_supply_no_symbol_alternatives() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("facts.json"), "{\"beacon\": 1}")?;
        let index = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        let matches = index.symbols("beacon", 1)?;
        assert_eq!(matches.len(), 1);
        assert!(index.assembled_symbol(matches[0])?.identity().is_none());
        assert!(super::symbols(&index, index.files(), "beaco", None, index.limits(),)?.is_empty());
        assert_eq!(matches[0].file.path().as_str(), "facts.json");
        let range = matches[0].symbol.item_range;
        let start = usize::try_from(range.start)?;
        let end = usize::try_from(range.end)?;
        assert!(matches[0].file.source()[start..end].contains("beacon"));
        Ok(())
    }

    #[test]
    fn unicode_expansion_is_charged_before_distance() {
        let query = "İ".to_lowercase();
        assert_eq!(query.chars().count(), 2);
        let comparator = rapidfuzz::distance::levenshtein::BatchComparator::new(query.chars());
        let mut exact = Work::new(30, 12);
        assert_eq!(
            exact.distance(&comparator, 2, "İ", usize::MAX).ok(),
            Some(Some(0))
        );
        assert_eq!(exact.remaining, 0);
        let mut short = Work::new(29, 12);
        assert!(short.distance(&comparator, 2, "İ", usize::MAX).is_err());
        let error = short.unavailable(6);
        assert!(error.to_string().chars().count() <= 4096);
        assert!(
            error
                .action()
                .contains("search.symbol_alternatives_lowercase_work")
        );
    }

    #[test]
    fn configured_lowercase_weight_reserves_before_unicode_allocation() {
        use rift_protocol::configuration::SEARCH_SYMBOL_ALTERNATIVES_LOWERCASE_WORK_MIN;
        // core Unicode mappings return at most three chars, each at most four UTF-8 bytes.
        assert_eq!(SEARCH_SYMBOL_ALTERNATIVES_LOWERCASE_WORK_MIN, 3 * 4);
        let mut exact = Work::new(48, 24);
        assert_eq!(exact.lowercase("İ").ok().as_deref(), Some("i\u{307}"));
        assert_eq!(exact.remaining, 0);
        let mut exhausted = Work::new(47, 24);
        let error = exhausted
            .lowercase("İ")
            .expect_err("allocation reservation refused");
        assert_eq!(exhausted.remaining, 47);
        assert_eq!(
            error.slug(),
            rift_error::errors::server::read_symbol_alternatives_unavailable::SLUG
        );
        assert!(error.message().contains("work bound 47"));
        assert!(error.message().contains("required 48"));
        assert!(
            error
                .action()
                .contains("reserves 24 work units per input byte")
        );
        assert!(error.to_string().contains(error.message()));
        assert!(error.to_string().contains(error.action()));
    }

    #[test]
    fn large_reservations_report_exact_required_work_without_overflow() {
        let mut work = Work::new(1, usize::MAX);
        let normalization = work
            .lowercase("xx")
            .expect_err("large normalization refused");
        assert!(
            normalization
                .message()
                .contains(&format!("required {}", 2 * usize::MAX as u128))
        );
        assert_eq!(work.remaining, 1);
        assert_eq!(
            normalization.slug(),
            rift_error::errors::server::read_symbol_alternatives_unavailable::SLUG
        );
        let comparator = rapidfuzz::distance::levenshtein::BatchComparator::new("x".chars());
        let mut work = Work::new(100, 12);
        let addition = work
            .distance(&comparator, usize::MAX, "x", usize::MAX)
            .expect_err("large query refused");
        assert!(
            addition
                .message()
                .contains(&format!("required {}", usize::MAX as u128 + 1))
        );
        assert_eq!(work.remaining, 88);
        assert_eq!(addition.slug(), normalization.slug());
        let mut work = Work::new(100, 12);
        let multiplication = work
            .distance(&comparator, usize::MAX / 2, "xx", usize::MAX)
            .expect_err("large distance refused");
        assert!(
            multiplication
                .message()
                .contains(&format!("required {}", (usize::MAX as u128 / 2 + 1) * 2))
        );
        assert_eq!(work.remaining, 76);
        assert_eq!(multiplication.slug(), normalization.slug());
    }

    #[test]
    fn index_limits_accept_and_refuse_symbol_alternatives_configuration() -> TestResult {
        use rift_protocol::configuration::SearchConfiguration;
        let search = SearchConfiguration {
            symbol_alternatives_work: 138,
            symbol_alternatives_lowercase_work: 24,
            ..Default::default()
        };
        let limits =
            WorkspaceIndexLimits::default().with_symbol_alternatives_configuration(&search)?;
        assert_eq!(limits.symbol_alternatives_work_max(), 138);
        assert_eq!(limits.symbol_alternatives_lowercase_work(), 24);
        for work in [0, (1 << 30) + 1, u64::MAX] {
            let invalid = SearchConfiguration {
                symbol_alternatives_work: work,
                ..Default::default()
            };
            let error = limits
                .with_symbol_alternatives_configuration(&invalid)
                .expect_err("work out of range");
            assert!(
                error
                    .to_string()
                    .contains("search.symbol_alternatives_work")
            );
        }
        for weight in [0, 11, 1025, u64::MAX] {
            let invalid = SearchConfiguration {
                symbol_alternatives_lowercase_work: weight,
                ..Default::default()
            };
            let error = limits
                .with_symbol_alternatives_configuration(&invalid)
                .expect_err("weight out of range");
            assert!(
                error
                    .to_string()
                    .contains("search.symbol_alternatives_lowercase_work")
            );
        }
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let index = index(directory.path())?;
        let higher_weight = super::symbols(&index, rust_files(&index), "x", None, limits)
            .expect_err("higher weight exhausts allowance");
        assert_eq!(
            higher_weight.slug(),
            rift_error::errors::server::read_symbol_alternatives_unavailable::SLUG
        );
        let accepted = SearchConfiguration {
            symbol_alternatives_work: 138,
            ..Default::default()
        };
        let accepted = limits.with_symbol_alternatives_configuration(&accepted)?;
        let complete = super::symbols(&index, rust_files(&index), "x", None, accepted)?;
        assert_eq!(complete.len(), 1);
        assert!(complete[0].0.ends_with("/beacon"));
        Ok(())
    }

    #[test]
    fn exhaustion_discards_candidates_already_compared() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("a.rs"), "pub fn beacon() {}\n")?;
        std::fs::write(directory.path().join("z.rs"), "pub fn x() {}\n")?;
        let index = index(directory.path())?;
        assert!(symbols_with_work(&index, rust_files(&index), "x", None, 132).is_err());
        let complete = symbols_with_work(&index, rust_files(&index), "x", None, 1_000)
            .map_err(|_| "complete ranking refused")?;
        assert!(complete[0].0.ends_with("/x"));
        Ok(())
    }

    #[test]
    fn qualified_name_exhaustion_discards_short_match_and_retained_candidates() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("lib.rs"),
            "pub mod parent { pub fn x() {} }\n",
        )?;
        let index = index(directory.path())?;
        let file = index
            .file(&rift_core::ProjectPath::new("lib.rs")?)
            .ok_or("parsed fixture absent")?;
        let names = file
            .syntax()
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.qualified_name.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(names, [("parent", "parent"), ("x", "parent::x")]);
        let symbol = &file.syntax().symbols()[1];
        let comparator = rapidfuzz::distance::levenshtein::BatchComparator::new("y".chars());
        // Short x costs 14; qualified parent::x costs 126, including its distance.
        let mut exact = Work::new(140, 12);
        assert_eq!(
            super::distance(&mut exact, &comparator, 1, symbol, usize::MAX).ok(),
            Some(Some(1))
        );
        assert_eq!(exact.remaining, 0);
        let mut exhausted = Work::new(139, 12);
        let error = super::distance(&mut exhausted, &comparator, 1, symbol, usize::MAX)
            .expect_err("qualified distance refused");
        assert_eq!(
            error.slug().to_string(),
            "rift.server.read_symbol_alternatives_unavailable"
        );
        assert_eq!(
            exhausted.remaining, 17,
            "short comparison and qualified normalization succeed before 18 distance units refuse"
        );
        // Query/file/parent cost 138. After x's declaration unit, short comparison,
        // and qualified normalization, 278 is one below the qualified distance bound.
        assert!(
            symbols_with_work(&index, rust_files(&index), "y", None, 278).is_err(),
            "the retained parent candidate must not escape an incomplete ranking"
        );
        // Complete ranking also reserves x's 48 insertion units, ending at 327.
        let complete = symbols_with_work(&index, rust_files(&index), "y", None, 327)
            .map_err(|_| "complete qualified ranking refused")?;
        assert_eq!(complete.len(), 2);
        let matched = index.symbols("parent::x", 1)?;
        let assembled = index.assembled_symbol(matched[0])?;
        assert_eq!(
            complete[0].0,
            assembled
                .identity()
                .ok_or("fixture identity absent")?
                .as_str()
        );
        assert!(
            error
                .message()
                .contains("closest alternatives unavailable at the work bound 139")
        );
        Ok(())
    }
}
