use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use rift_core::{
    Contribution, ContributionKey, IndexRevision, SourceApplicability, SourceRange, SourceRevision,
    SourceUnitId, SymbolRecord, SymbolResolution, TreeRevision,
};
use rift_error::RiftError;
use rift_protocol::identity::SymbolIdentity;

use super::{NormalizedGraph, Normalizer, normalize_edges};
use crate::{ProviderPublication, PublicationSet};

/// Ordered admitted rows of one modified project source.
#[derive(Debug)]
pub struct ModifiedContributionRun {
    /// Physical source unit that owns both runs.
    pub unit: SourceUnitId,
    /// Positions in the predecessor's syntax publication.
    pub previous: Range<usize>,
    /// Positions in the current syntax publication.
    pub current: Range<usize>,
}

impl Normalizer {
    /// Normalizes a complete captured project and retains its ordinary record roots.
    ///
    /// # Errors
    /// Returns the graph validation errors of ordinary normalization.
    pub fn normalize_project(
        revisions: (IndexRevision, SourceRevision, TreeRevision),
        publications: &Arc<PublicationSet>,
    ) -> Result<NormalizedGraph, RiftError> {
        Self::normalize_publications(revisions, publications, &|_| None, true)
    }

    /// Carries record membership over one isolated declaration insertion or removal.
    ///
    /// Selects a single admitted provider publication on both sides. The caller supplies
    /// every changed document in capture order with its old and current admitted runs.
    ///
    /// # Errors
    /// Returns an error if current record construction refuses projected membership.
    pub fn project_records_after_edit(
        revisions: (IndexRevision, SourceRevision, TreeRevision),
        publications: &Arc<PublicationSet>,
        previous: &NormalizedGraph,
        edits: &[ModifiedContributionRun],
    ) -> Result<Option<NormalizedGraph>, RiftError> {
        let Some(proof) = ProjectMembership::prove(revisions, publications, previous, edits) else {
            return Ok(None);
        };
        proof.project(revisions, publications, previous, edits)
    }
}

#[derive(Debug)]
struct ProjectMembership<'a> {
    current: &'a ProviderPublication,
    previous_roots: &'a [u32],
    change: MembershipChange<'a>,
    changed_rank: usize,
    old_rows: &'a [Contribution],
    earlier: SourceApplicability,
    now: SourceApplicability,
}

#[derive(Debug)]
enum MembershipChange<'a> {
    Inserted {
        position: usize,
        row: &'a Contribution,
    },
    Removed {
        position: usize,
        row: &'a Contribution,
    },
}

impl MembershipChange<'_> {
    const fn position(&self) -> usize {
        match self {
            Self::Inserted { position, .. } | Self::Removed { position, .. } => *position,
        }
    }

    const fn row(&self) -> &Contribution {
        match self {
            Self::Inserted { row, .. } | Self::Removed { row, .. } => row,
        }
    }

    fn current_root(&self, position: u32) -> Option<u32> {
        let position = position as usize;
        let changed = self.position();
        match self {
            Self::Inserted { .. } if position >= changed => u32::try_from(position + 1).ok(),
            Self::Removed { .. } if position == changed => None,
            Self::Removed { .. } if position > changed => u32::try_from(position - 1).ok(),
            _ => u32::try_from(position).ok(),
        }
    }

    const fn current_rank(&self, rank: usize, changed: usize) -> Option<usize> {
        match self {
            Self::Inserted { .. } if rank >= changed => Some(rank + 1),
            Self::Removed { .. } if rank == changed => None,
            Self::Removed { .. } if rank > changed => Some(rank - 1),
            _ => Some(rank),
        }
    }
}

impl<'a> ProjectMembership<'a> {
    fn prove(
        (_, source, tree): (IndexRevision, SourceRevision, TreeRevision),
        publications: &'a PublicationSet,
        previous: &'a NormalizedGraph,
        edits: &[ModifiedContributionRun],
    ) -> Option<Self> {
        const SELECTED_ROWS_MAX: usize = 8_192;
        const EDITS_MAX: usize = 64;
        let previous_roots = previous.project_roots.as_deref()?;
        if edits.is_empty()
            || edits.len() > EDITS_MAX
            || publications.provider_count() != 1
            || previous.publications.provider_count() != 1
            || publications.limits() != previous.publications.limits()
        {
            return None;
        }
        let current = publications.publications().next()?;
        let old = previous.publications.provider(current.provider())?;
        let (old_rows, current_rows) = (old.contributions(), current.contributions());
        if old_rows.len().abs_diff(current_rows.len()) != 1
            || u32::try_from(current_rows.len()).is_err()
            || previous_roots.len() != previous.records.len()
        {
            return None;
        }
        let insert = current_rows.len() > old_rows.len();
        let (earlier, now) = (
            SourceApplicability::Exact {
                source_revision: previous.source_revision,
                tree_revision: previous.tree_revision,
            },
            SourceApplicability::Exact {
                source_revision: source,
                tree_revision: tree,
            },
        );

        let mut visited = 0_usize;
        let mut change = None;
        let mut old_end = 0;
        let mut current_end = 0;
        for edit in edits {
            let previous_run = old_rows.get(edit.previous.clone())?;
            let current_run = current_rows.get(edit.current.clone())?;
            visited = visited
                .checked_add(previous_run.len())?
                .checked_add(current_run.len())?;
            if visited > SELECTED_ROWS_MAX
                || edit.previous.start.checked_sub(old_end)?
                    != edit.current.start.checked_sub(current_end)?
            {
                return None;
            }
            if let Some(edit_change) = compare_run(edit, previous_run, current_run, insert)? {
                if change.replace(edit_change).is_some() {
                    return None;
                }
            }
            old_end = edit.previous.end;
            current_end = edit.current.end;
        }
        if old_rows.len() - old_end != current_rows.len() - current_end {
            return None;
        }
        let change = change?;
        let changed = u32::try_from(change.position()).ok()?;
        let changed_rank = previous_roots.partition_point(|&root| root < changed);
        if let MembershipChange::Removed { row, .. } = change
            && (previous_roots.get(changed_rank) != Some(&changed)
                || previous.records.get(changed_rank)?.contributions() != [row.key().clone()])
        {
            return None;
        }

        if change
            .row()
            .identity_anchor()
            .is_some_and(|id| SymbolIdentity::parse(id.as_str()).is_err())
        {
            return None;
        }
        if change.row().applicability() != if insert { now } else { earlier } {
            return None;
        }
        Some(Self {
            current,
            previous_roots,
            change,
            changed_rank,
            old_rows,
            earlier,
            now,
        })
    }

    fn project(
        &self,
        (index_revision, source_revision, tree_revision): (
            IndexRevision,
            SourceRevision,
            TreeRevision,
        ),
        publications: &Arc<PublicationSet>,
        previous: &NormalizedGraph,
        edits: &[ModifiedContributionRun],
    ) -> Result<Option<NormalizedGraph>, RiftError> {
        let count = match self.change {
            MembershipChange::Inserted { .. } => previous.records.len() + 1,
            MembershipChange::Removed { .. } => previous.records.len() - 1,
        };
        let (mut records, mut roots) = (Vec::with_capacity(count), Vec::with_capacity(count));
        let revision = self.current.revision();
        for (rank, (record, root)) in previous.records.iter().zip(self.previous_roots).enumerate() {
            if rank == self.changed_rank {
                self.add_inserted(index_revision, &mut records, &mut roots)?;
            }
            if let Some(current_root) = self.change.current_root(*root) {
                let keys = record
                    .contributions()
                    .iter()
                    .map(|key| {
                        let reference = key.reference();
                        ContributionKey::new(
                            reference.provider().clone(),
                            revision,
                            reference.symbol().clone(),
                        )
                    })
                    .collect();
                records.push(SymbolRecord::new(
                    index_revision,
                    record.identity().cloned(),
                    record.resolution(),
                    keys,
                )?);
                roots.push(current_root);
            }
        }
        if self.changed_rank == previous.records.len() {
            self.add_inserted(index_revision, &mut records, &mut roots)?;
        }
        let added = match self.change {
            MembershipChange::Inserted { row, .. } => {
                Some((row.key().reference().clone(), self.changed_rank))
            }
            MembershipChange::Removed { .. } => None,
        };
        let records_by_contribution = previous
            .records_by_contribution
            .iter()
            .filter_map(|(reference, rank)| {
                self.change
                    .current_rank(*rank, self.changed_rank)
                    .map(|rank| (reference.clone(), rank))
            })
            .chain(added)
            .collect();
        // Fold survivor and anchor validation into the required current relationship pass.
        // The record projection stays private until that pass has verified every row.
        let units: BTreeSet<_> = edits.iter().map(|run| &run.unit).collect();
        if units.len() != edits.len() {
            return Ok(None);
        }
        let insert = matches!(self.change, MembershipChange::Inserted { .. });
        let changed_position = self.change.position();
        let anchor = self.change.row().identity_anchor();
        let mut old = self
            .old_rows
            .iter()
            .enumerate()
            .filter(|(position, _)| insert || *position != changed_position);
        let mut pending = edits.iter().peekable();
        let mut unit = None;
        let mut selected = false;
        let mut checked = 0;
        let verified =
            self.current
                .contributions()
                .iter()
                .enumerate()
                .scan((), |(), (position, row)| {
                    if insert && position == changed_position {
                        checked += 1;
                        return Some(row);
                    }
                    let (_, earlier_row) = old.next()?;
                    while pending
                        .peek()
                        .is_some_and(|edit| edit.current.end <= position)
                    {
                        pending.next();
                    }
                    let expected_unit = pending
                        .peek()
                        .filter(|edit| edit.current.contains(&position))
                        .map(|edit| &edit.unit);
                    let binding = row.source()?;
                    let earlier_binding = earlier_row.source()?;
                    if unit != Some(binding.unit()) {
                        unit = Some(binding.unit());
                        selected = units.contains(binding.unit());
                    }
                    if selected != expected_unit.is_some()
                        || expected_unit.is_some_and(|expected| expected != binding.unit())
                        || earlier_binding.unit() != binding.unit()
                        || (!selected && earlier_binding != binding)
                        || row.key().reference() != earlier_row.key().reference()
                        || row.identity_anchor() != earlier_row.identity_anchor()
                        || anchor.is_some_and(|anchor| row.identity_anchor() == Some(anchor))
                        || !row.equivalence().is_empty()
                        || !earlier_row.equivalence().is_empty()
                        || row.applicability() != self.now
                        || earlier_row.applicability() != self.earlier
                    {
                        return None;
                    }
                    checked += 1;
                    Some(row)
                });
        let (references, edges) = normalize_edges(
            index_revision,
            verified,
            &records,
            &records_by_contribution,
            &|_| None,
        );
        if checked != self.current.contributions().len() {
            return Ok(None);
        }
        Ok(Some(NormalizedGraph {
            index_revision,
            source_revision,
            tree_revision,
            publications: Arc::clone(publications),
            records,
            records_by_contribution,
            candidates: previous.candidates.clone(),
            references,
            edges,
            project_roots: Some(roots.into_boxed_slice()),
        }))
    }

    fn add_inserted(
        &self,
        revision: IndexRevision,
        records: &mut Vec<SymbolRecord>,
        roots: &mut Vec<u32>,
    ) -> Result<(), RiftError> {
        if let MembershipChange::Inserted { row, position } = self.change {
            let identity = row.identity_anchor().cloned();
            let resolution = if identity.is_some() {
                SymbolResolution::Established
            } else {
                SymbolResolution::Unresolved
            };
            records.push(SymbolRecord::new(
                revision,
                identity,
                resolution,
                vec![row.key().clone()],
            )?);
            roots.push(u32::try_from(position).expect("validated root position"));
        }
        Ok(())
    }
}

/// Equal ranges within a unit convey physical equivalence. The two maps require the
/// same partition of the surviving keys without requiring the ranges to stay fixed.
fn compare_run<'a>(
    edit: &ModifiedContributionRun,
    previous: &'a [Contribution],
    current: &'a [Contribution],
    insert: bool,
) -> Option<Option<MembershipChange<'a>>> {
    let (smaller, larger) = if insert {
        (previous, current)
    } else {
        (current, previous)
    };
    if smaller.len() > larger.len() || larger.len() - smaller.len() > 1 {
        return None;
    }
    for row in smaller.iter().chain(larger) {
        if row.source()?.unit() != &edit.unit || !row.equivalence().is_empty() {
            return None;
        }
    }
    let extra = if smaller.len() == larger.len() {
        None
    } else {
        let position = smaller
            .iter()
            .zip(larger)
            .take_while(|(smaller, larger)| smaller.key().reference() == larger.key().reference())
            .count();
        let row = larger.get(position)?;
        let binding = row.source()?;
        if larger
            .iter()
            .filter(|item| item.source() == Some(binding))
            .count()
            != 1
        {
            return None;
        }
        Some((position, row))
    };
    let mut forward = BTreeMap::<SourceRange, SourceRange>::new();
    let mut reverse = BTreeMap::<SourceRange, SourceRange>::new();
    let survivors = larger
        .iter()
        .enumerate()
        .filter(|(position, _)| extra.is_none_or(|(added, _)| *position != added));
    for (first, (_, second)) in smaller.iter().zip(survivors) {
        if first.key().reference() != second.key().reference()
            || first.identity_anchor() != second.identity_anchor()
        {
            return None;
        }
        let (left, right) = (first.source()?.range(), second.source()?.range());
        if forward.insert(left, right).is_some_and(|old| old != right)
            || reverse.insert(right, left).is_some_and(|old| old != left)
        {
            return None;
        }
    }
    Some(extra.map(|(offset, row)| {
        if insert {
            MembershipChange::Inserted {
                position: edit.current.start + offset,
                row,
            }
        } else {
            MembershipChange::Removed {
                position: edit.previous.start + offset,
                row,
            }
        }
    }))
}

#[cfg(test)]
mod tests;
