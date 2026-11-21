//! External targets at authored reference, type, and inheritance sites.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use lsp_types::{Location, Position};
use rift_core::{ProjectPath, SymbolId as CoreSymbolId};
use rift_error::RiftError;
use rift_lsp::{EngineSession, LineIndex, PositionEncoding};
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::{
    Edge, EdgeKind, EdgeProvider, ReadWarning, SourceUnitSpan, SymbolId, TextRange,
    TraversalDirection,
};

use super::{EngineInventorySelection, InventoryHeritage, InventoryReport, graph_call_coverage};
use crate::{CalleeRoots, PackageCallee, ReadService};

/// One external engine target awaiting the same package placement as a call target.
#[derive(Clone, Debug)]
pub struct PackageEdge {
    /// The exact file, owner and position answered by the engine.
    pub target: PackageCallee,
    /// The requested connection to that declaration.
    pub kind: EdgeKind,
    /// Whether the external declaration is the source or target of this edge.
    pub direction: TraversalDirection,
    /// Authored occurrence when this read addressed a source token.
    pub source: Option<SourceUnitSpan>,
    /// The dependency or standard-library target the static pass stored at the same source
    /// site under the same owner, when exactly one such target exists there.
    pub static_target: Option<SymbolId>,
}

impl PackageEdge {
    /// Names this target without changing its analysis, direction or source occurrence.
    #[must_use]
    pub fn named(&self, identity: SymbolId) -> Edge {
        let seed = SymbolId(self.target.caller().as_str().to_owned());
        let (from, to) = match self.direction {
            TraversalDirection::Outgoing => (seed, identity),
            TraversalDirection::Incoming => (identity, seed),
        };
        super::super::graph_edge(
            &from,
            to,
            self.kind,
            self.source.clone(),
            self.target.edge_provider(),
        )
    }
}

#[derive(Clone, Debug)]
pub(super) struct Site {
    caller: SymbolId,
    source: SourceUnitSpan,
    name: String,
    utf8: Position,
    utf16: Position,
    kinds: BTreeSet<EdgeKind>,
}

#[derive(Debug)]
pub(super) struct Answer {
    site: Site,
    locations: Vec<Location>,
}

pub(super) fn sites(
    reads: &ReadService,
    file: &rift_index::IndexedFile,
    seeds: &BTreeSet<SymbolId>,
    selection: EngineInventorySelection,
) -> Result<Vec<Site>, RiftError> {
    if !selection.references
        && !selection.types.definitions
        && !selection.types.extends
        && !selection.types.implements
    {
        return Ok(Vec::new());
    }
    let Some(facts) = file.syntax().static_facts() else {
        return Ok(Vec::new());
    };
    let mut sites = BTreeMap::new();
    let lines = LineIndex::new(file.source());
    for reference in facts.references() {
        let mut kinds = Vec::new();
        if selection.references && reference.role != rift_core::ReferenceRole::Definition {
            kinds.push(EdgeKind::References);
        }
        if selection.types.definitions && reference.role == rift_core::ReferenceRole::Type {
            kinds.push(EdgeKind::HasType);
        }
        if kinds.is_empty() {
            continue;
        }
        if let Some(caller) = super::super::edge_enclosing_symbol(
            reads,
            file,
            reference.target.start,
            reference.target.end,
        )? && seeds.contains(&caller)
        {
            insert(
                &mut sites,
                file,
                &lines,
                caller,
                reference.target,
                kinds,
                selection.records_max,
            )?;
        }
    }
    if selection.references {
        reference_sites(
            reads,
            file,
            seeds,
            selection.records_max,
            &lines,
            &mut sites,
        )?;
    }
    let mut headers = BTreeMap::new();
    for seed in seeds {
        for edge in reads.edges().outgoing(&CoreSymbolId::new(seed.as_str())?) {
            if matches!(edge.kind(), EdgeKind::Extends | EdgeKind::Implements)
                && edge.occurrence().unit().key().as_str() == file.path().as_str()
            {
                let range = edge.occurrence().range();
                headers.insert((edge.kind(), range.start(), range.end()), seed.clone());
            }
        }
    }
    for header in facts.heritage_edges() {
        let selected = match header.kind {
            EdgeKind::Extends => selection.types.extends,
            EdgeKind::Implements => selection.types.implements,
            _ => false,
        };
        if !selected {
            continue;
        }
        let caller = headers
            .get(&(header.kind, header.target.start, header.target.end))
            .cloned();
        let caller = if caller.is_some() || file.syntax().language().name == "rust" {
            caller
        } else {
            super::super::edge_enclosing_symbol(reads, file, header.from.start, header.from.end)?
        };
        if let Some(caller) = caller
            && seeds.contains(&caller)
        {
            insert(
                &mut sites,
                file,
                &lines,
                caller,
                header.target,
                [header.kind],
                selection.records_max,
            )?;
        }
    }
    Ok(sites.into_values().collect())
}

fn reference_sites(
    reads: &ReadService,
    file: &rift_index::IndexedFile,
    seeds: &BTreeSet<SymbolId>,
    maximum: usize,
    lines: &LineIndex,
    sites: &mut BTreeMap<(SymbolId, u64, u64), Site>,
) -> Result<(), RiftError> {
    let Some(facts) = file.syntax().static_facts() else {
        return Ok(());
    };
    let imports = facts
        .bindings()
        .iter()
        .filter(|binding| {
            matches!(
                binding.kind,
                rift_syntax::StaticBindingKind::PythonImport { .. }
                    | rift_syntax::StaticBindingKind::JavaScriptImport { .. }
                    | rift_syntax::StaticBindingKind::JavaScriptEdgeNamespace { .. }
                    | rift_syntax::StaticBindingKind::JavaScriptRequire { .. }
                    | rift_syntax::StaticBindingKind::RustUse { .. }
            )
        })
        .map(|binding| binding.name);
    for range in facts
        .calls()
        .iter()
        .map(|call| call.callee_range)
        .chain(imports)
    {
        if let Some(caller) =
            super::super::edge_enclosing_symbol(reads, file, range.start, range.end)?
            && seeds.contains(&caller)
        {
            insert(
                sites,
                file,
                lines,
                caller,
                range,
                [EdgeKind::References],
                maximum,
            )?;
        }
    }
    Ok(())
}

fn insert(
    sites: &mut BTreeMap<(SymbolId, u64, u64), Site>,
    file: &rift_index::IndexedFile,
    lines: &LineIndex,
    caller: SymbolId,
    range: rift_syntax::ByteRange,
    kinds: impl IntoIterator<Item = EdgeKind>,
    maximum: usize,
) -> Result<(), RiftError> {
    let key = (caller.clone(), range.start, range.end);
    if let Some(site) = sites.get_mut(&key) {
        site.kinds.extend(kinds);
        return Ok(());
    }
    if sites.len() == maximum {
        return Err(graph_call_coverage());
    }
    let start = usize::try_from(range.start).map_err(|_| graph_call_coverage())?;
    let end = usize::try_from(range.end).map_err(|_| graph_call_coverage())?;
    let name = file
        .source()
        .get(start..end)
        .ok_or_else(graph_call_coverage)?;
    let at = name
        .char_indices()
        .next_back()
        .map(|(offset, _)| start + offset)
        .ok_or_else(graph_call_coverage)?;
    sites.insert(
        key,
        Site {
            caller,
            name: name.to_owned(),
            source: SourceUnitSpan {
                unit: crate::read::source_unit_id(file.path()),
                range: TextRange {
                    start: range.start,
                    end: range.end,
                },
            },
            utf8: lines.position(PositionEncoding::Utf8, at)?,
            utf16: lines.position(PositionEncoding::Utf16, at)?,
            kinds: kinds.into_iter().collect(),
        },
    );
    Ok(())
}

pub(super) async fn read(
    session: &mut EngineSession,
    path: &ProjectPath,
    sites: &[Site],
    maximum: usize,
    report: &mut InventoryReport,
) -> Result<(), RiftError> {
    if !session.capabilities().definitions.definition {
        return Ok(());
    }
    let encoding = session.capabilities().position_encoding;
    for chunk in sites.chunks(maximum.clamp(1, rift_lsp::correlation::PENDING_REQUESTS_MAX)) {
        let positions = chunk
            .iter()
            .map(|site| {
                (
                    path.clone(),
                    match encoding {
                        PositionEncoding::Utf8 => site.utf8,
                        PositionEncoding::Utf16 => site.utf16,
                    },
                )
            })
            .collect::<Vec<_>>();
        let answers = session.definition_batch(&positions).await?;
        for (site, locations) in chunk.iter().zip(answers) {
            report.charge(locations.len().saturating_add(site.kinds.len()))?;
            report.targets.push(Answer {
                site: site.clone(),
                locations,
            });
        }
    }
    Ok(())
}

pub(super) struct Mapping<'read> {
    pub(super) reads: &'read ReadService,
    pub(super) roots: &'read CalleeRoots,
    pub(super) workspace: &'read Path,
    pub(super) encoding: PositionEncoding,
    pub(super) provider: EdgeProvider,
}

pub(super) struct Resolved {
    pub(super) packages: Vec<PackageEdge>,
    pub(super) types: Vec<(SymbolId, Vec<Location>)>,
    pub(super) heritage: Vec<InventoryHeritage>,
    pub(super) warnings: Vec<ReadWarning>,
}

impl Mapping<'_> {
    pub(super) fn resolve(
        &self,
        types: Vec<(SymbolId, Vec<Location>)>,
        heritage: Vec<InventoryHeritage>,
        targets: Vec<Answer>,
    ) -> Result<Resolved, RiftError> {
        let mut edges = Vec::new();
        let mut missing = BTreeSet::new();
        let mut local_types = Vec::new();
        let mut local_heritage = Vec::new();
        for (seed, locations) in types {
            let mut local = Vec::new();
            for location in locations {
                if self.target(
                    &seed,
                    &location,
                    (EdgeKind::HasType, TraversalDirection::Outgoing),
                    ("", None),
                    &mut edges,
                    &mut missing,
                )? {
                    local.push(location);
                }
            }
            local_types.push((seed, local));
        }
        for item in heritage {
            if self.target(
                &item.seed,
                &item.location,
                (item.kind, item.direction),
                ("", None),
                &mut edges,
                &mut missing,
            )? {
                local_heritage.push(item);
            }
        }
        for answer in targets {
            for location in answer.locations {
                for &kind in &answer.site.kinds {
                    if self.target(
                        &answer.site.caller,
                        &location,
                        (kind, TraversalDirection::Outgoing),
                        (&answer.site.name, Some(&answer.site.source)),
                        &mut edges,
                        &mut missing,
                    )? && kind == EdgeKind::HasType
                    {
                        local_types.push((answer.site.caller.clone(), vec![location.clone()]));
                    }
                }
            }
        }
        self.static_targets(&mut edges);
        let warnings = if missing.is_empty() {
            Vec::new()
        } else {
            vec![ReadWarning::EdgeCoverageMissing {
            kinds: missing.into_iter().collect(),
            detail: "the engine could not establish targets within the admitted project, package and runtime roots, so this answer omits those edges".to_owned(),
        }]
        };
        Ok(Resolved {
            packages: edges,
            types: local_types,
            heritage: local_heritage,
            warnings,
        })
    }

    /// Names each external engine target after the dependency or standard-library target
    /// the static pass stored at the same source site, when exactly one stored target there
    /// has the engine target's owner: the static admission rule named that site, and the
    /// engine placed it in that owner's sources. A call site matches by its callee range.
    /// Each declaration's stored edges are read once.
    fn static_targets(&self, edges: &mut [PackageEdge]) {
        let mut sites = BTreeMap::<CoreSymbolId, BTreeMap<(u64, u64), Vec<usize>>>::new();
        for (index, edge) in edges.iter().enumerate() {
            if let Some(source) = &edge.source {
                sites
                    .entry(edge.target.caller().clone())
                    .or_default()
                    .entry((source.range.start, source.range.end))
                    .or_default()
                    .push(index);
            }
        }
        for (seed, ranges) in sites {
            let mut named = BTreeMap::<usize, StaticName<'_>>::new();
            for stored in self.reads.edges().outgoing(&seed) {
                let Ok(identity) = SymbolIdentity::parse(stored.to().as_str()) else {
                    continue;
                };
                let occurrence = stored.occurrence();
                let callee = stored
                    .static_callee()
                    .map(|callee| (callee.start, callee.end));
                let mut unit = None;
                for key in std::iter::once((occurrence.range().start(), occurrence.range().end()))
                    .chain(callee)
                {
                    for &index in ranges.get(&key).into_iter().flatten() {
                        let edge = &edges[index];
                        let unit = unit.get_or_insert_with(|| occurrence.unit().to_string());
                        if edge
                            .source
                            .as_ref()
                            .is_none_or(|source| source.unit.0 != *unit)
                            || edge.target.package().owner().as_ref() != Some(identity.owner())
                        {
                            continue;
                        }
                        named
                            .entry(index)
                            .and_modify(|name| name.add(stored.to()))
                            .or_insert(StaticName::One(stored.to()));
                    }
                }
            }
            for (index, name) in named {
                if let StaticName::One(target) = name {
                    edges[index].static_target = Some(SymbolId(target.as_str().to_owned()));
                }
            }
        }
    }

    fn target(
        &self,
        seed: &SymbolId,
        location: &Location,
        (kind, direction): (EdgeKind, TraversalDirection),
        (name, source): (&str, Option<&SourceUnitSpan>),
        edges: &mut Vec<PackageEdge>,
        missing: &mut BTreeSet<EdgeKind>,
    ) -> Result<bool, RiftError> {
        let caller = CoreSymbolId::new(seed.as_str())?;
        match super::super::callee_file(
            self.roots,
            &[self.workspace, self.reads.index().root()],
            &caller,
            super::super::NamedCallee {
                provider: self.provider,
                uri: &location.uri,
                name,
                position: location.range.start,
                encoding: self.encoding,
            },
        )? {
            super::super::CalleeFile::Package(target) => {
                if location.range.start == Position::new(0, 0)
                    && location.range.end == location.range.start
                {
                    missing.insert(kind);
                } else {
                    edges.push(PackageEdge {
                        target,
                        kind,
                        direction,
                        source: source.cloned(),
                        static_target: None,
                    });
                }
            }
            super::super::CalleeFile::Project(_) => return Ok(true),
            super::super::CalleeFile::Unaddressed => {
                missing.insert(kind);
            }
        }
        Ok(false)
    }
}

/// The stored static targets at one engine site under the engine target's owner.
enum StaticName<'store> {
    One(&'store CoreSymbolId),
    /// More than one stored target: the site names none of them.
    Ambiguous,
}

impl<'store> StaticName<'store> {
    fn add(&mut self, target: &'store CoreSymbolId) {
        match self {
            Self::One(held) if *held == target => {}
            _ => *self = Self::Ambiguous,
        }
    }
}

#[cfg(test)]
mod tests;
