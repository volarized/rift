//! Body matches on the read path: the declarations inside the first file rows of one
//! ranked list, answered in place of those rows.
//!
//! A symbol row stores no source, so a query word only a declaration's body holds ranks
//! the row holding its file's text. For each of the first
//! [`BODY_MATCH_FILE_ROWS_MAX`] file rows, this finds where the query's terms sit in the
//! bytes the row holds, maps each position to its smallest containing declaration through
//! [`IndexedFile::enclosing_symbol`], and scores every declaration found together
//! ([`BodyMatchPool`]). [`RankedCandidates::with_body_matches`] then places them at the
//! first file row's place.

use std::collections::BTreeMap;

use rift_index::{IndexedFile, WorkspaceIndex, declaration_identity};
use rift_protocol::read::SearchParamsTarget;
use rift_ranking::{
    BODY_MATCH_FILE_ROWS_MAX, BodyMatchPool, BodyTerms, DocumentIdentity, FileRowAnswer,
    FileRowFrequencies, FusedCandidate, ParsedQuery, RankedCandidates,
};
use rift_syntax::SyntaxSymbol;

use super::{ResolvedCandidate, StoreAnswer, declared, resolve_file};

/// How often one declaration holds each query term.
type TermCounts<'term> = BTreeMap<&'term str, u32>;

/// What body matching needs for one request: the query's terms, the store's document
/// frequencies for them, and whether the answer keeps its file rows.
pub(super) struct BodyMatching<'store> {
    terms: BodyTerms,
    frequencies: &'store FileRowFrequencies,
    file_rows: FileRowAnswer,
}

impl<'store> BodyMatching<'store> {
    /// Body matching for one request, or `None` when the request's `target` never answers
    /// a declaration from a file row, the query carries no term, or the store read no
    /// document frequencies beside its ranking.
    pub(super) fn for_request(
        target: SearchParamsTarget,
        store: &'store StoreAnswer,
        query: &ParsedQuery,
    ) -> Option<Self> {
        let file_rows = match target {
            SearchParamsTarget::Symbol => FileRowAnswer::Dropped,
            SearchParamsTarget::All => FileRowAnswer::Kept,
            SearchParamsTarget::File
            | SearchParamsTarget::Documentation
            | SearchParamsTarget::Commit => return None,
        };
        let frequencies = store.file_rows()?;
        let terms = BodyTerms::of(query);
        (!terms.is_empty()).then_some(Self {
            terms,
            frequencies,
            file_rows,
        })
    }

    /// `ranked` with the declarations found inside its first file rows placed at the first
    /// file row's place, and its file rows dropped when the request names declarations
    /// alone. The joined answer stays within `keep_max`.
    pub(super) fn placed(
        &self,
        index: &WorkspaceIndex,
        ranked: RankedCandidates,
        keep_max: usize,
    ) -> RankedCandidates {
        let mut pool = BodyMatchPool::default();
        let file_rows = ranked
            .candidates()
            .iter()
            .filter(|candidate| is_file_row(candidate))
            .take(BODY_MATCH_FILE_ROWS_MAX);
        for row in file_rows {
            self.pool_row(index, row, &mut pool);
        }
        ranked.with_body_matches(pool.into_ranked(), is_file_row, self.file_rows, keep_max)
    }

    /// Adds every declaration holding a query term inside `row`'s bytes to `pool`. A text
    /// file declares nothing, and a row whose bytes no longer sit inside its file answers
    /// nothing: the store and the index disagree about it, and the row stays unmapped.
    fn pool_row(
        &self,
        index: &WorkspaceIndex,
        row: &FusedCandidate,
        pool: &mut BodyMatchPool<DocumentIdentity>,
    ) {
        let Some(ResolvedCandidate::SourceFile(file)) =
            resolve_file(index, row.identity().as_str())
        else {
            return;
        };
        let Some(range) = row.file_range() else {
            return;
        };
        let bounds = usize::try_from(range.start)
            .ok()
            .zip(usize::try_from(range.end).ok());
        let Some(text) = bounds.and_then(|(start, end)| file.source().get(start..end)) else {
            return;
        };
        for (symbol, counts) in self.declarations_holding(file, range.start, text) {
            let length = symbol.range.end.saturating_sub(symbol.range.start);
            pool.add(
                self.frequencies,
                declaration_identity(declared(file, symbol)),
                &counts,
                length,
            );
        }
    }

    /// Each declaration of `file` holding a query term inside `text`, which starts at byte
    /// `start` of the file, with how often it holds each term, in the order the file
    /// declares them. Work is one pass over `text` plus one lookup per term occurrence.
    fn declarations_holding<'file>(
        &self,
        file: &'file IndexedFile,
        start: u64,
        text: &str,
    ) -> Vec<(&'file SyntaxSymbol, TermCounts<'_>)> {
        let mut held: BTreeMap<_, (&SyntaxSymbol, TermCounts<'_>)> = BTreeMap::new();
        for (offset, term) in self.terms.occurrences(text) {
            let at = start.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
            let end = at.saturating_add(u64::try_from(term.len()).unwrap_or(u64::MAX));
            let Some(symbol) = file.enclosing_symbol(at, end) else {
                continue;
            };
            let key = (
                symbol.range.start,
                symbol.range.end,
                symbol.qualified_name.as_str(),
            );
            let (_, counts) = held.entry(key).or_insert_with(|| (symbol, BTreeMap::new()));
            *counts.entry(term).or_default() += 1;
        }
        held.into_values().collect()
    }
}

/// Whether one fused candidate is a row holding file text: the full-text store states
/// which bytes of its file every such row holds, and no other row carries that.
fn is_file_row(candidate: &FusedCandidate) -> bool {
    candidate.file_range().is_some()
}
