//! Inheritance edges from advertised LSP operations.

use std::collections::BTreeSet;

use lsp_types::{Location, Range};
use rift_core::{ProjectPath, SymbolId as CoreSymbolId};
use rift_error::{RiftError, errors};
use rift_lsp::{EngineSession, LineIndex, PositionEncoding, TreeRoot};
use rift_protocol::read::{
    Edge, EdgeDerivation, EdgeKind, EdgeProvider, ExactKind, Extensions, ReadWarning, SymbolId,
    TraversalDirection,
};
use tokio::time::Instant;

use super::{
    ReferenceTarget, graph_reference_coverage, project_declaration, readiness_unconfirmed_warning,
    walk_unsettled_warning,
};
use crate::ReadService;
use crate::engine::{EnginePool, EngineSlot, OutgoingAnswer};
use crate::traversal::TRAVERSAL_SYMBOLS_MAX;
use lsp_types::{SymbolKind, TypeHierarchyItem};

struct EdgeEngineReport {
    locations: Vec<Location>,
    encoding: PositionEncoding,
}

/// Reads engine heritage facts targeting the bounded captured type inventory.
///
/// # Errors
/// Refuses incomplete identities, unsupported kinds, exceeded bounds or engine failures.
///
/// # Cancel safety
/// Dropping the future cancels the current exchange and retains no inventory.
pub async fn graph_heritage_inventory(
    reads: &ReadService,
    engines: &EnginePool,
    kinds: &[EdgeKind],
    requests_max: usize,
    deadline: Instant,
) -> Result<(Vec<Edge>, Vec<ReadWarning>), RiftError> {
    if kinds
        .iter()
        .any(|kind| !matches!(kind, EdgeKind::Extends | EdgeKind::Implements))
    {
        return Err(graph_reference_coverage());
    }
    let maximum = requests_max / kinds.len().max(1);
    let view = reads.index().symbol_view()?;
    if !view.objects_complete() {
        return Err(graph_reference_coverage());
    }
    let mut targets = Vec::new();
    for (identity, object) in view.objects() {
        if object?
            .facts()
            .symbol_kinds()
            .contains(&rift_protocol::read::SymbolKind::Type)
        {
            if targets.len() == maximum {
                return Err(graph_reference_coverage());
            }
            targets.push(SymbolId(identity.as_str().to_owned()));
        }
    }
    let mut edges = Vec::new();
    let mut warnings = Vec::new();
    for target in targets {
        for &kind in kinds {
            let (found, reported) = Box::pin(graph_heritage_edges(
                reads,
                engines,
                &target,
                kind,
                TraversalDirection::Incoming,
                deadline,
            ))
            .await?;
            if edges.len().saturating_add(found.len()) > TRAVERSAL_SYMBOLS_MAX {
                return Err(graph_reference_coverage());
            }
            edges.extend(found);
            warnings.extend(reported);
        }
    }
    Ok((edges, warnings))
}

/// Reads inheritance edges from the capabilities the selected engine advertises.
///
/// # Errors
/// Returns an engine failure, an invalid source location, or an exceeded edge bound.
///
/// # Cancel safety
/// Dropping the future cancels the active exchange and retains no graph answer.
pub async fn graph_heritage_edges(
    reads: &ReadService,
    engines: &EnginePool,
    seed: &SymbolId,
    kind: EdgeKind,
    direction: TraversalDirection,
    deadline: Instant,
) -> Result<(Vec<Edge>, Vec<ReadWarning>), RiftError> {
    let identity = CoreSymbolId::new(seed.as_str())?;
    let Some(declaration) = super::edge_declarations(reads, &identity)?
        .into_iter()
        .next()
    else {
        return Ok((Vec::new(), Vec::new()));
    };
    let Some(slot) = engines.engine_for(declaration.file.syntax().language()) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let target = ReferenceTarget::new(
        slot.workspace_root(),
        reads.index().root(),
        declaration.file,
        declaration.symbol,
    )?;
    let implementation_kind = implementation_kind(declaration.symbol);
    let report = match Box::pin(edge_engine_report(
        slot,
        &target,
        kind,
        implementation_kind,
        direction,
        deadline,
    ))
    .await
    {
        Ok(report) => report,
        Err(error) if error.slug() == errors::lsp::engine_capability_absent::SLUG => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(error) => return Err(error),
    };
    let (report, warnings) = match report {
        OutgoingAnswer::Ready(report) => (report, Vec::new()),
        OutgoingAnswer::Unconfirmed(report) => (
            report,
            vec![readiness_unconfirmed_warning(&BTreeSet::from([slot
                .name()
                .to_owned()]))],
        ),
        OutgoingAnswer::Unsettled { attempts } => {
            return Ok((
                Vec::new(),
                vec![walk_unsettled_warning(
                    declaration.file.syntax().language().clone(),
                    attempts,
                )],
            ));
        }
        OutgoingAnswer::Unprepared => return Ok((Vec::new(), Vec::new())),
    };
    let edges = edge_locations(reads, slot, seed, (kind, direction), report)?;
    Ok((edges, warnings))
}

pub(super) fn implementation_kind(symbol: &rift_syntax::SyntaxSymbol) -> Option<EdgeKind> {
    match symbol.kind {
        "trait_item" | "interface_declaration" | "interface" | "trait" => {
            Some(EdgeKind::Implements)
        }
        "class_declaration"
        | "class_definition"
        | "abstract_class_declaration"
        | "class"
        | "abstract_class" => Some(EdgeKind::Extends),
        _ => None,
    }
}

fn edge_locations(
    reads: &ReadService,
    slot: &EngineSlot,
    seed: &SymbolId,
    (kind, direction): (EdgeKind, TraversalDirection),
    report: EdgeEngineReport,
) -> Result<Vec<Edge>, RiftError> {
    let identity = CoreSymbolId::new(seed.as_str())?;
    let roots = [
        TreeRoot::new(slot.workspace_root())?,
        TreeRoot::new(reads.index().root())?,
    ];
    let mut edges = Vec::new();
    for location in report.locations {
        let path = roots[0]
            .project_path(&location.uri)
            .or_else(|_| roots[1].project_path(&location.uri));
        let path = path.map_err(|_| graph_reference_coverage())?;
        let neighbor = project_declaration(
            reads,
            &path,
            (location.range, report.encoding),
            "engine edges",
        )?;
        if kind == EdgeKind::Implements && direction == TraversalDirection::Incoming {
            edge_implementation(
                reads,
                &identity,
                &path,
                (location.range, report.encoding),
                neighbor,
                slot.edge_provider(),
                &mut edges,
            )?;
            continue;
        }
        if let Some(neighbor) = neighbor {
            let (from, to) = match direction {
                TraversalDirection::Outgoing => (seed.clone(), neighbor),
                TraversalDirection::Incoming => (neighbor, seed.clone()),
            };
            edges.push(edge_protocol(from, to, kind, slot.edge_provider()));
        } else if direction == TraversalDirection::Incoming {
            let before = edges.len();
            edge_impl_header(
                reads,
                &identity,
                &path,
                (location.range, report.encoding),
                kind,
                slot.edge_provider(),
                &mut edges,
            )?;
            if edges.len() == before {
                return Err(graph_reference_coverage());
            }
        } else {
            return Err(graph_reference_coverage());
        }
    }
    Ok(edges)
}

pub(super) fn edge_protocol(
    from: SymbolId,
    to: SymbolId,
    kind: EdgeKind,
    provider: EdgeProvider,
) -> Edge {
    Edge {
        producer: None,
        from,
        to,
        kind: ExactKind(kind.as_ref().to_owned()),
        kinds: vec![kind],
        occurrence: None,
        provider,
        derivation: EdgeDerivation::Resolution,
        confidence: None,
        extensions: Extensions::default(),
    }
}

/// Keeps implementation replies only where the captured header establishes a direct edge.
pub(super) fn edge_implementation(
    reads: &ReadService,
    target: &CoreSymbolId,
    path: &ProjectPath,
    (range, encoding): (Range, PositionEncoding),
    neighbor: Option<SymbolId>,
    provider: EdgeProvider,
    edges: &mut Vec<Edge>,
) -> Result<(), RiftError> {
    let before = edges.len();
    edge_impl_header(
        reads,
        target,
        path,
        (range, encoding),
        EdgeKind::Implements,
        provider,
        edges,
    )?;
    if edges.len() != before {
        return Ok(());
    }
    let Some(from) = neighbor else { return Ok(()) };
    let identity = CoreSymbolId::new(from.as_str())?;
    let direct = reads
        .edges()
        .outgoing(&identity)
        .iter()
        .any(|edge| edge.kind() == EdgeKind::Implements && edge.to() == target);
    if direct {
        edges.push(edge_protocol(
            from,
            SymbolId(target.as_str().to_owned()),
            EdgeKind::Implements,
            provider,
        ));
    }
    Ok(())
}

pub(super) fn edge_impl_header(
    reads: &ReadService,
    target: &CoreSymbolId,
    path: &ProjectPath,
    (range, encoding): (Range, PositionEncoding),
    kind: EdgeKind,
    provider: EdgeProvider,
    edges: &mut Vec<Edge>,
) -> Result<(), RiftError> {
    let Some(file) = reads.index().file(path) else {
        return Ok(());
    };
    let index = LineIndex::new(file.source());
    let start = index.byte_offset(encoding, range.start)? as u64;
    let end = index.byte_offset(encoding, range.end)? as u64;
    let unit = rift_syntax::DocumentPlacement::project_path(path)?;
    for edge in reads
        .edges()
        .incoming(target)
        .iter()
        .filter(|edge| edge.kind() == kind && edge.occurrence().unit() == unit.unit())
    {
        let occurrence = edge.occurrence().range();
        if start <= occurrence.start() && occurrence.end() <= end {
            let mut edge = edge.to_protocol();
            edge.provider = provider;
            edge.derivation = EdgeDerivation::Resolution;
            edges.push(edge);
        }
    }
    Ok(())
}

async fn edge_engine_report(
    slot: &EngineSlot,
    target: &ReferenceTarget,
    kind: EdgeKind,
    implementation_kind: Option<EdgeKind>,
    direction: TraversalDirection,
    deadline: Instant,
) -> Result<OutgoingAnswer<EdgeEngineReport>, RiftError> {
    let path = target.path.clone();
    let (utf8, utf16) = (target.utf8, target.utf16);
    slot.request_outgoing(
        target.opening(),
        move |session| {
            let path = path.clone();
            Box::pin(async move {
                let capability = session.capabilities();
                let implementation = direction == TraversalDirection::Incoming
                    && kind == EdgeKind::Implements
                    && Some(kind) == implementation_kind
                    && capability.edges.implementation;
                if !capability.edges.type_hierarchy && !implementation {
                    return errors::lsp::engine_capability_absent()
                        .capability("textDocument/prepareTypeHierarchy")
                        .fail();
                }
                let encoding = session.capabilities().position_encoding;
                let position = match encoding {
                    PositionEncoding::Utf8 => utf8,
                    PositionEncoding::Utf16 => utf16,
                };
                let mut locations = Vec::new();
                if direction == TraversalDirection::Incoming
                    && kind == EdgeKind::Implements
                    && Some(kind) == implementation_kind
                    && session.capabilities().edges.implementation
                {
                    locations.extend(session.implementation_edges(&path, position).await?);
                }
                if session.capabilities().edges.type_hierarchy {
                    let prepared = session.prepare_type_edges(&path, position).await?;
                    if prepared.len() > TRAVERSAL_SYMBOLS_MAX {
                        return Err(graph_reference_coverage());
                    }
                    for item in prepared {
                        if item.uri != session.root().document_uri(&path)?
                            || !(item.selection_range.start <= position
                                && position < item.selection_range.end)
                        {
                            continue;
                        }
                        edge_hierarchy_locations(session, item, direction, kind, &mut locations)
                            .await?;
                    }
                }
                if locations.len() > TRAVERSAL_SYMBOLS_MAX {
                    return Err(graph_reference_coverage());
                }
                Ok(Some(EdgeEngineReport {
                    locations,
                    encoding,
                }))
            })
        },
        target.closing(),
        deadline,
    )
    .await
}

async fn edge_hierarchy_locations(
    session: &mut EngineSession,
    item: TypeHierarchyItem,
    direction: TraversalDirection,
    kind: EdgeKind,
    locations: &mut Vec<Location>,
) -> Result<(), RiftError> {
    let selected = item.kind;
    let neighbors = match direction {
        TraversalDirection::Outgoing => session.supertype_edges(item).await?,
        TraversalDirection::Incoming => session.subtype_edges(item).await?,
    };
    if neighbors.len() > TRAVERSAL_SYMBOLS_MAX {
        return Err(graph_reference_coverage());
    }
    for neighbor in neighbors {
        let (source, target) = match direction {
            TraversalDirection::Outgoing => (selected, neighbor.kind),
            TraversalDirection::Incoming => (neighbor.kind, selected),
        };
        let edge = if source != SymbolKind::INTERFACE && target == SymbolKind::INTERFACE {
            EdgeKind::Implements
        } else {
            EdgeKind::Extends
        };
        if edge == kind {
            locations.push(Location {
                uri: neighbor.uri,
                range: neighbor.selection_range,
            });
        }
        if locations.len() > TRAVERSAL_SYMBOLS_MAX {
            return Err(graph_reference_coverage());
        }
    }
    Ok(())
}
