//! `search` over commit messages: `target: "commit"` matches `query` against the messages
//! of the commits the history store holds, and answers each matched commit with its
//! message, author, time, and the paths it changed.

use std::collections::HashSet;

use rift_history_store::StoreReads;
use rift_protocol::read::{
    COMMIT_MESSAGE_BYTES_MAX, COMMIT_PATHS_MAX, CommitHit, ProjectPath, ReadWarning, RevisionId,
    SearchHit, SearchHitTarget, SearchParams, SearchParamsTarget, SearchResult, SearchScope,
};
use rift_ranking::{ParsedQuery, QueryPhase};

use super::{parsed_query, query_narrowing_warning, required_query, search_page_limit};
use crate::history::StoredHistory;
use crate::read::{ReadError, ReadFault, ReadService, page, results_truncation_warning};

/// The capability a commit search names when no history store answers it.
const COMMIT_SEARCH_CAPABILITY: &str = "commit search";

/// Why a commit search cannot take a field the request also names: each selects
/// declarations, file text, or another tree, and the global index holds no project
/// history. `None` for a request whose target is not `commit`, or one naming none of them.
pub(super) fn commit_conflict(params: &SearchParams) -> Option<ReadError> {
    if params.target != SearchParamsTarget::Commit {
        return None;
    }
    let conflicts = [
        (
            "pattern",
            params.pattern.is_some(),
            "a commit search matches messages by `query`, and `pattern` matches file text",
        ),
        (
            "traversal",
            params.traversal.is_some(),
            "a walk answers declarations, and a commit search answers commits",
        ),
        (
            "change",
            params.change.is_some(),
            "a comparison answers declarations, and a commit search answers commits",
        ),
        (
            "rev",
            params.rev.is_some(),
            "a commit search reads the history store, which holds every analyzed revision",
        ),
        (
            "packages",
            !params.packages.is_empty(),
            "the global index holds no project history",
        ),
        (
            "paths",
            params.paths.is_some(),
            "a commit search matches commit messages alone",
        ),
        (
            "scope",
            params.scope != SearchScope::Local,
            "the global index holds no project history",
        ),
    ];
    conflicts
        .into_iter()
        .find_map(|(field, present, reason)| present.then(|| ReadFault::invalid(field, reason)))
}

impl ReadService {
    /// Answers one commit search from the attached history store: the commits whose
    /// message carries every term of `query`, then the ones carrying some, each group
    /// newest first, bounded by the index's `results_max`. While the store lacks a commit
    /// the history task's latest fill selects, or the commit `HEAD` names, the answer
    /// carries `history_store_filling`.
    ///
    /// # Errors
    ///
    /// Returns `invalid_request` naming the field for a commit search beside `pattern`,
    /// `traversal`, `change`, `rev`, `packages`, or `paths`, or with a `scope` past
    /// `local`, and naming `query` when it is missing, empty, or refused by the bounded
    /// parser; `unsupported` when `[providers.history]` is disabled or no history store is
    /// attached; and the store fault when the store cannot be read.
    pub fn search_commits(&self, params: &SearchParams) -> Result<SearchResult, ReadError> {
        if let Some(conflict) = commit_conflict(params) {
            return Err(conflict);
        }
        let query = required_query(params)?;
        let limit = search_page_limit(params)?;
        let parsed = parsed_query(query)?;
        let stored = self.commit_store()?;
        let reads = stored.connect()?;
        let results_max = self.index().results_max();
        let matched = rift_core::traced!(component = "search", operation = "search.commits", {
            matched_commits(&reads, &parsed, results_max)
        })?;
        let (ids, pagination) = page(matched.ids, params.page_index, limit);
        let mut results = Vec::with_capacity(ids.len());
        for id in &ids {
            results.extend(commit_hit(&reads, id)?);
        }
        let mut warnings: Vec<ReadWarning> = stored
            .filling(&reads, self.index().root())?
            .into_iter()
            .collect();
        if parsed.is_narrowed() {
            warnings.push(query_narrowing_warning());
        }
        if matched.truncated {
            warnings.push(results_truncation_warning(results_max));
        }
        Ok(SearchResult {
            results,
            pagination,
            warnings,
        })
    }

    /// The attached history store a commit search reads.
    fn commit_store(&self) -> Result<&StoredHistory, ReadError> {
        if !self.history_configuration().enabled {
            return Err(ReadFault::unsupported(
                "commit search (providers.history disabled)",
            ));
        }
        self.stored_history()
            .ok_or_else(|| ReadFault::unsupported(COMMIT_SEARCH_CAPABILITY))
    }
}

/// The commits one search matched, in answer order, and whether more matched than the
/// bound kept.
#[derive(Debug)]
struct MatchedCommits {
    ids: Vec<String>,
    truncated: bool,
}

/// The ids of the commits whose message matches `query`: the precise phase's first, then
/// the broad phase's the precise one did not answer, each phase newest first. At most
/// `results_max` are kept; each phase reads one more, so a cut is observed rather than
/// guessed.
fn matched_commits(
    reads: &StoreReads,
    query: &ParsedQuery,
    results_max: usize,
) -> Result<MatchedCommits, ReadError> {
    let fetch = results_max.saturating_add(1);
    let precise = query.render(QueryPhase::Precise);
    let broad = query
        .has_broad_phase()
        .then(|| query.render(QueryPhase::Broad))
        .flatten();
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for expression in [precise, broad].into_iter().flatten() {
        let matched = reads
            .search_messages(&expression, fetch)
            .map_err(ReadFault::history_store)?;
        append_unseen(&mut ids, &mut seen, matched, fetch);
    }
    let truncated = ids.len() > results_max;
    ids.truncate(results_max);
    Ok(MatchedCommits { ids, truncated })
}

/// Appends each of `matched` not already in `ids`, until `ids` holds `fetch`.
fn append_unseen(
    ids: &mut Vec<String>,
    seen: &mut HashSet<String>,
    matched: Vec<String>,
    fetch: usize,
) {
    for id in matched {
        if ids.len() == fetch {
            return;
        }
        if seen.insert(id.clone()) {
            ids.push(id);
        }
    }
}

/// One matched commit as a hit, or `None` when a trim deleted it between the match and
/// this read.
fn commit_hit(reads: &StoreReads, id: &str) -> Result<Option<SearchHit>, ReadError> {
    let Some(commit) = reads.commit(id).map_err(ReadFault::history_store)? else {
        return Ok(None);
    };
    let mut paths = reads
        .changed_paths(&commit, COMMIT_PATHS_MAX.saturating_add(1))
        .map_err(ReadFault::history_store)?;
    let paths_truncated = paths.len() > COMMIT_PATHS_MAX;
    paths.truncate(COMMIT_PATHS_MAX);
    let (message, message_truncated) = bounded_message(&commit.message);
    let hit = CommitHit {
        revision: RevisionId(commit.id.clone()),
        message: message.to_owned(),
        message_truncated,
        author: commit.author.clone(),
        timestamp: commit.committed_at.clone(),
        paths: paths.into_iter().map(ProjectPath).collect(),
        paths_truncated,
    };
    Ok(Some(SearchHit {
        hit: SearchHitTarget::Commit {
            commit: Box::new(hit),
        },
        score: None,
        matched_by: Vec::new(),
        source: None,
        range: None,
        line: None,
        path: None,
        unit: None,
        traversal_path: None,
        distance: None,
        change: None,
    }))
}

/// `message` cut at the last character boundary within `COMMIT_MESSAGE_BYTES_MAX`, and
/// whether the cut dropped anything.
fn bounded_message(message: &str) -> (&str, bool) {
    let end = message.floor_char_boundary(COMMIT_MESSAGE_BYTES_MAX);
    (&message[..end], end < message.len())
}
