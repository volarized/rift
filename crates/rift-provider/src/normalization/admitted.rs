//! Carries record membership across exact syntax publications when their associations agree.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_core::{
    Contribution, ContributionKey, ContributionReference, DeclarationBinding, IndexRevision,
    SourceApplicability, SourceRevision, SourceUnitId, SymbolRecord, SymbolResolution,
    TreeRevision,
};
use rift_error::RiftError;

use super::{NormalizedGraph, normalize_edges};
use crate::{ProviderPublication, PublicationSet};

#[derive(Debug)]
pub(super) struct RecordOrdinals {
    /// Provider ordinal of the union root, in emitted record order.
    pub(super) roots: Vec<usize>,
    /// Record position of each provider row.
    pub(super) records: Vec<usize>,
}

impl RecordOrdinals {
    pub(super) fn for_publication(set: &PublicationSet, count: usize) -> Option<Self> {
        let provider = sole_provider(set)?;
        (provider.contributions().len() == count).then(|| Self {
            roots: Vec::new(),
            records: vec![0; count],
        })
    }

    pub(super) fn position(
        &self,
        set: &PublicationSet,
        reference: &ContributionReference,
    ) -> Option<usize> {
        let index = set
            .provider(reference.provider())?
            .ordinal(reference.symbol())?;
        self.records.get(index).copied()
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.roots
            .capacity()
            .saturating_add(self.records.capacity())
            .saturating_mul(std::mem::size_of::<usize>())
    }
}

fn sole_provider(set: &PublicationSet) -> Option<&ProviderPublication> {
    let mut providers = set.publications();
    let first = providers.next()?;
    (providers.next().is_none() && first.provider().as_str() == "syntax").then_some(first)
}

struct Run {
    before: usize,
    after: usize,
    len: usize,
}

#[derive(Default)]
struct Layout {
    matched: Vec<Run>,
    added: Vec<usize>,
    removed: Vec<usize>,
    moved_units: BTreeSet<SourceUnitId>,
}

impl Layout {
    fn current_ordinal(&self, old: usize) -> Option<usize> {
        let i = self
            .matched
            .partition_point(|run| run.before <= old)
            .checked_sub(1)?;
        let run = &self.matched[i];
        let offset = old - run.before;
        (offset < run.len).then_some(run.after + offset)
    }

    fn align(held: &ProviderPublication, current: &ProviderPublication) -> Option<Self> {
        let old = held.contributions();
        let now = current.contributions();
        let mut layout = Self::default();
        let (mut i, mut j) = (0, 0);
        while i < old.len() || j < now.len() {
            let prior = old.get(i);
            let next = now.get(j);
            if let (Some(a), Some(b)) = (prior, next)
                && a.key().reference() == b.key().reference()
            {
                if a.identity_anchor() != b.identity_anchor() {
                    return None;
                }
                if a.source() != b.source() {
                    let (a, b) = (a.source()?, b.source()?);
                    if a.unit() != b.unit() {
                        return None;
                    }
                    layout.moved_units.insert(a.unit().clone());
                }
                match layout.matched.last_mut() {
                    Some(run) if i == run.before + run.len && j == run.after + run.len => {
                        run.len += 1
                    }
                    _ => layout.matched.push(Run {
                        before: i,
                        after: j,
                        len: 1,
                    }),
                }
                i += 1;
                j += 1;
            } else if let Some(b) = next
                && held.ordinal(b.key().reference().symbol()).is_none()
            {
                layout.moved_units.insert(b.source()?.unit().clone());
                layout.added.push(j);
                j += 1;
            } else if let Some(a) = prior
                && current.ordinal(a.key().reference().symbol()).is_none()
            {
                layout.moved_units.insert(a.source()?.unit().clone());
                layout.removed.push(i);
                i += 1;
            } else {
                return None;
            }
        }
        Some(layout)
    }

    /// Each old physical equality class must map bijectively to a current class.
    /// Added and removed rows must each own a distinct binding and an isolated anchor.
    fn same_associations(&self, old: &[Contribution], now: &[Contribution]) -> bool {
        let mut additions = BTreeSet::new();
        let mut removals = BTreeSet::new();
        let mut changed_anchors = BTreeSet::new();
        for (rows, positions, bindings) in [
            (old, &self.removed, &mut removals),
            (now, &self.added, &mut additions),
        ] {
            for &i in positions {
                let row = &rows[i];
                let Some(binding) = row.source() else {
                    return false;
                };
                if !bindings.insert(coordinates(binding))
                    || row
                        .identity_anchor()
                        .is_some_and(|anchor| !changed_anchors.insert(anchor))
                {
                    return false;
                }
            }
        }
        let mut forward = BTreeMap::new();
        let mut reverse = BTreeMap::new();
        for run in &self.matched {
            for (a, b) in old[run.before..run.before + run.len]
                .iter()
                .zip(&now[run.after..run.after + run.len])
            {
                if a.identity_anchor()
                    .is_some_and(|anchor| changed_anchors.contains(anchor))
                {
                    return false;
                }
                let Some(a) = a
                    .source()
                    .filter(|binding| self.moved_units.contains(binding.unit()))
                else {
                    continue;
                };
                let Some(b) = b.source() else { return false };
                let (a, b) = (coordinates(a), coordinates(b));
                if removals.contains(&a)
                    || additions.contains(&b)
                    || forward.insert(a, b).is_some_and(|prior| prior != b)
                    || reverse.insert(b, a).is_some_and(|prior| prior != a)
                {
                    return false;
                }
            }
        }
        true
    }
}

fn coordinates(binding: &DeclarationBinding) -> (&SourceUnitId, u64, u64) {
    (
        binding.unit(),
        binding.range().start(),
        binding.range().end(),
    )
}

fn exact(rows: &[Contribution], source: SourceRevision, tree: TreeRevision) -> bool {
    let scope = SourceApplicability::Exact {
        source_revision: source,
        tree_revision: tree,
    };
    rows.iter()
        .all(|row| row.applicability() == scope && row.equivalence().is_empty())
}

pub(super) type Rebuild = Result<NormalizedGraph, &'static str>;

pub(super) fn rebuild(
    held: &NormalizedGraph,
    publications: &Arc<PublicationSet>,
    index_revision: IndexRevision,
    source_revision: SourceRevision,
    tree_revision: TreeRevision,
) -> Result<Rebuild, RiftError> {
    let (Some(old), Some(now), Some(previous)) = (
        sole_provider(&held.publications),
        sole_provider(publications),
        &held.ordinals,
    ) else {
        return Ok(Err("provider"));
    };
    if !exact(
        old.contributions(),
        held.source_revision,
        held.tree_revision,
    ) || !exact(now.contributions(), source_revision, tree_revision)
    {
        return Ok(Err("scope_or_equivalence"));
    }
    let Some(layout) = Layout::align(old, now) else {
        return Ok(Err("declaration_order_or_anchor"));
    };
    if !layout.same_associations(old.contributions(), now.contributions()) {
        return Ok(Err("physical_binding_or_claimant"));
    }

    let count = now.contributions().len();
    let mut records = Vec::with_capacity(
        held.records
            .len()
            .saturating_sub(layout.removed.len())
            .saturating_add(layout.added.len()),
    );
    let mut ordinals = RecordOrdinals {
        roots: Vec::with_capacity(records.capacity()),
        records: vec![0; count],
    };
    let mut old_to_new = vec![usize::MAX; held.records.len()];
    let mut added = layout.added.iter().copied().peekable();
    for (old_position, (&root, record)) in previous.roots.iter().zip(&held.records).enumerate() {
        let Some(current_root) = layout.current_ordinal(root) else {
            if record.contributions().len() != 1 {
                return Ok(Err("removed_group"));
            }
            continue;
        };
        while added.peek().is_some_and(|&i| i < current_root) {
            let i = added.next().expect("peeked addition");
            singleton(&mut records, &mut ordinals, now, index_revision, i)?;
        }
        let keys = record
            .contributions()
            .iter()
            .map(|key| {
                ContributionKey::new(
                    now.provider().clone(),
                    now.revision(),
                    key.reference().symbol().clone(),
                )
            })
            .collect();
        old_to_new[old_position] = records.len();
        ordinals.roots.push(current_root);
        records.push(SymbolRecord::new(
            index_revision,
            record.identity().cloned(),
            record.resolution(),
            keys,
        )?);
    }
    for i in added {
        singleton(&mut records, &mut ordinals, now, index_revision, i)?;
    }
    for run in layout.matched {
        for offset in 0..run.len {
            let rank = old_to_new[previous.records[run.before + offset]];
            if rank == usize::MAX {
                return Ok(Err("removed_member"));
            }
            ordinals.records[run.after + offset] = rank;
        }
    }
    let rows = now.contributions().iter().collect::<Vec<_>>();
    let (references, edges) = normalize_edges(
        index_revision,
        &rows,
        &records,
        |reference| ordinals.position(publications, reference),
        &|_| None,
    );
    Ok(Ok(NormalizedGraph {
        index_revision,
        source_revision,
        tree_revision,
        publications: Arc::clone(publications),
        records,
        records_by_contribution: BTreeMap::new(),
        ordinals: Some(ordinals),
        group_work: ("carried", held.records.len() - layout.removed.len()),
        candidates: held.candidates.clone(),
        references,
        edges,
    }))
}

fn singleton(
    records: &mut Vec<SymbolRecord>,
    ordinals: &mut RecordOrdinals,
    provider: &ProviderPublication,
    revision: IndexRevision,
    index: usize,
) -> Result<(), RiftError> {
    let row = &provider.contributions()[index];
    let anchor = row
        .identity_anchor()
        .filter(|id| rift_protocol::identity::SymbolIdentity::parse(id.as_str()).is_ok())
        .cloned();
    let resolution = if anchor.is_some() {
        SymbolResolution::Established
    } else {
        SymbolResolution::Unresolved
    };
    ordinals.roots.push(index);
    ordinals.records[index] = records.len();
    records.push(SymbolRecord::new(
        revision,
        anchor,
        resolution,
        vec![row.key().clone()],
    )?);
    Ok(())
}
