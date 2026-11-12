//! Symbol history edges: what a symbol history answer and a query over the history axis
//! read from the store's rows.
//!
//! A symbol's commit edges come from the declaration changes the store holds per commit,
//! its `co_changes` edges from the other declarations the same commits changed, and the
//! served tree's `uses` edges from its dependency context. The store keys a declaration
//! change by the path and qualified name the commit itself held: which logical symbol a
//! declaration belongs to depends on the module layout of the tree holding it, so the
//! served tree binds those keys to its symbols when it reads them. Nothing here reads the
//! store, the repository, or the clock.

use std::collections::{BTreeMap, BTreeSet};

use rift_error::RiftError;
use rift_protocol::dependencies::PackageContextEntry;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{
    CO_CHANGE_COMMIT_DECLARATIONS_MAX, CO_CHANGE_SYMBOLS_MAX, CoChangeTruncation, HistoryChange,
    HistoryCommit, HistoryEdge, ProjectPath, SymbolId, SymbolVersionKind,
};
use rift_protocol::revision::Revision;

/// The commit edge one stored declaration change answers. A `moved` change answers
/// `renamed_from` the path the declaration came from, and nothing when no such path is
/// known.
#[must_use]
pub fn commit_edge(
    change: SymbolVersionKind,
    commit: HistoryCommit,
    from: Option<ProjectPath>,
) -> Option<HistoryEdge> {
    let change = match change {
        SymbolVersionKind::Introduced => return Some(HistoryEdge::IntroducedIn { commit }),
        SymbolVersionKind::Removed => return Some(HistoryEdge::RemovedIn { commit }),
        SymbolVersionKind::Moved => {
            return from.map(|from| HistoryEdge::RenamedFrom { commit, from });
        }
        SymbolVersionKind::SignatureChanged => HistoryChange::SignatureChanged,
        SymbolVersionKind::BodyChanged => HistoryChange::BodyChanged,
        SymbolVersionKind::DecoratorsChanged => HistoryChange::DecoratorsChanged,
    };
    Some(HistoryEdge::ChangedIn { commit, change })
}

/// The commit a commit edge names; `None` for a `co_changes` edge.
#[must_use]
pub const fn edge_commit(edge: &HistoryEdge) -> Option<&HistoryCommit> {
    match edge {
        HistoryEdge::IntroducedIn { commit }
        | HistoryEdge::ChangedIn { commit, .. }
        | HistoryEdge::RemovedIn { commit }
        | HistoryEdge::RenamedFrom { commit, .. } => Some(commit),
        HistoryEdge::CoChanges { .. } => None,
    }
}

/// One declaration a commit changed, keyed the way the store keys it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeclarationKey {
    /// The path the declaration lived at in the commit.
    pub path: String,
    /// The provider's qualified name for the declaration.
    pub qualified_name: String,
}

/// The `co_changes` edges of one symbol, counted commit by commit.
///
/// Each counted commit adds one to every other symbol it changed. A commit changing more
/// than [`CO_CHANGE_COMMIT_DECLARATIONS_MAX`] declarations counts toward none, so one
/// sweeping commit cannot tie the symbol to everything it touched; the answer keeps the
/// [`CO_CHANGE_SYMBOLS_MAX`] symbols sharing the most commits. Both cuts are counted in
/// the [`CoChangeTruncation`] the tally answers.
#[derive(Debug)]
pub struct CoChangeTally {
    symbol: SymbolId,
    counts: BTreeMap<SymbolId, u64>,
    commits_past_bound: u64,
}

impl CoChangeTally {
    /// An empty tally for `symbol`.
    #[must_use]
    pub const fn new(symbol: SymbolId) -> Self {
        Self {
            symbol,
            counts: BTreeMap::new(),
            commits_past_bound: 0,
        }
    }

    /// Counts one commit that changed the symbol and `changed`, the declarations the store
    /// lists for it. `bound` resolves a declaration key to the served tree's symbol; a key
    /// it binds to no symbol, or to this one, counts toward nothing. `changed` past
    /// [`CO_CHANGE_COMMIT_DECLARATIONS_MAX`] entries counts the commit past the bound and
    /// resolves no key, so the store reads one entry more than the bound to tell.
    ///
    /// # Errors
    ///
    /// Returns the error `bound` returns.
    pub fn count_commit(
        &mut self,
        changed: &[DeclarationKey],
        bound: &mut dyn FnMut(&DeclarationKey) -> Result<Option<SymbolId>, RiftError>,
    ) -> Result<(), RiftError> {
        if changed.len() > CO_CHANGE_COMMIT_DECLARATIONS_MAX {
            self.commits_past_bound = self.commits_past_bound.saturating_add(1);
            return Ok(());
        }
        let mut partners = BTreeSet::new();
        for key in changed {
            if let Some(partner) = bound(key)?
                && partner != self.symbol
            {
                partners.insert(partner);
            }
        }
        for partner in partners {
            let count = self.counts.entry(partner).or_default();
            *count = count.saturating_add(1);
        }
        Ok(())
    }

    /// The `co_changes` edges, most shared commits first and then by symbol, and what the
    /// bounds left out; the truncation is `None` when they left out nothing.
    #[must_use]
    pub fn into_edges(self) -> (Vec<HistoryEdge>, Option<CoChangeTruncation>) {
        let mut ranked: Vec<(SymbolId, u64)> = self.counts.into_iter().collect();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        let symbols_past_bound = ranked.len().saturating_sub(CO_CHANGE_SYMBOLS_MAX);
        ranked.truncate(CO_CHANGE_SYMBOLS_MAX);
        let edges = ranked
            .into_iter()
            .map(|(symbol, commits)| HistoryEdge::CoChanges { symbol, commits })
            .collect();
        let truncation = CoChangeTruncation {
            commits_past_bound: self.commits_past_bound,
            symbols_past_bound: u64::try_from(symbols_past_bound).unwrap_or(u64::MAX),
        };
        let truncated = truncation != CoChangeTruncation::default();
        (edges, truncated.then_some(truncation))
    }
}

/// One `uses` edge: the served workspace revision builds against one package release.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionUse {
    /// The served workspace revision.
    pub workspace: Revision,
    /// The package release it uses, as `rift://revision/<manager>/<registry>/<name>@<version>`.
    pub release: Revision,
}

/// The `uses` edges of the `workspace` revision: one per exact package release its
/// dependency context `packages` pins, in release order, each release once. An entry
/// naming no registry or no exact version, or a version its ecosystem does not spell
/// canonically, pins no release and answers no edge.
#[must_use]
pub fn revision_uses(workspace: &Revision, packages: &[PackageContextEntry]) -> Vec<RevisionUse> {
    let releases: BTreeMap<String, Revision> = packages
        .iter()
        .filter_map(pinned_release)
        .map(|release| (release.to_string(), release))
        .collect();
    releases
        .into_values()
        .map(|release| RevisionUse {
            workspace: workspace.clone(),
            release,
        })
        .collect()
}

/// The release one dependency context entry pins, when it pins one exactly.
fn pinned_release(entry: &PackageContextEntry) -> Option<Revision> {
    let owner = SymbolOwner::Package {
        manager: entry.manager.clone(),
        registry: entry.registry.clone()?,
        name: entry.name.clone(),
    };
    let release = Revision::release(&owner, entry.version.as_deref()?).ok()?;
    let spelled = Revision::parse(&release.to_string()).ok()?;
    (spelled == release).then_some(release)
}

#[cfg(test)]
mod tests;
