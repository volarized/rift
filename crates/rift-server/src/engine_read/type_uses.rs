//! Type uses resolved by engines at captured type positions.

use rift_core::{ProjectPath, ReferenceRole};
use rift_error::RiftError;
use rift_protocol::read::{EdgeKind, SymbolId, SymbolKind};
use tokio::time::Instant;

use super::{GraphReferences, graph_reference_coverage, graph_references};
use crate::ReadService;
use crate::engine::EnginePool;
use crate::traversal::TRAVERSAL_SYMBOLS_MAX;

/// Resolves authored type positions against the bounded local type inventory.
///
/// # Errors
/// Returns incomplete identity coverage, an engine failure, or an exceeded occurrence bound.
///
/// # Cancel safety
/// Dropping the future cancels the current exchange and retains no answer.
pub async fn graph_type_uses(
    reads: &ReadService,
    engines: &EnginePool,
    deadline: Instant,
) -> Result<GraphReferences, RiftError> {
    let view = reads.index().symbol_view()?;
    let mut targets = Vec::new();
    for (identity, object) in view.objects() {
        let object = object?;
        if !object.facts().symbol_kinds().contains(&SymbolKind::Type) {
            continue;
        }
        if targets.len() == TRAVERSAL_SYMBOLS_MAX {
            return Err(graph_reference_coverage());
        }
        targets.push(SymbolId(identity.as_str().to_owned()));
    }
    let mut answer = GraphReferences {
        occurrences: Vec::new(),
        edges: Vec::new(),
        complete: view.objects_complete() && reads.edges().is_complete(),
        edge_warnings: Vec::new(),
    };
    for target in targets {
        let incoming = Box::pin(graph_references(reads, engines, &target, true, deadline)).await?;
        answer.admit_type_uses(reads, incoming)?;
    }
    Ok(answer)
}

impl GraphReferences {
    fn admit_type_uses(&mut self, reads: &ReadService, incoming: Self) -> Result<(), RiftError> {
        self.complete &= incoming.complete;
        self.edge_warnings.extend(incoming.edge_warnings);
        let mut admitted = std::collections::BTreeSet::new();
        for mut occurrence in incoming.occurrences {
            if !reads.is_type_position(&occurrence)? {
                continue;
            }
            if self.occurrences.len() == TRAVERSAL_SYMBOLS_MAX {
                return Err(graph_reference_coverage());
            }
            occurrence.kind = EdgeKind::HasType;
            admitted.insert((
                occurrence.target.clone(),
                occurrence.source.unit.as_str().to_owned(),
                occurrence.source.range.start,
                occurrence.source.range.end,
            ));
            self.occurrences.push(occurrence);
        }
        for mut edge in incoming.edges {
            let Some(span) = &edge.occurrence else {
                continue;
            };
            if admitted.contains(&(
                edge.to.clone(),
                span.unit.as_str().to_owned(),
                span.range.start,
                span.range.end,
            )) {
                if self.edges.len() == TRAVERSAL_SYMBOLS_MAX {
                    return Err(graph_reference_coverage());
                }
                edge.kind = rift_protocol::read::ExactKind(EdgeKind::HasType.as_ref().to_owned());
                edge.kinds = vec![EdgeKind::HasType];
                self.edges.push(edge);
            }
        }
        Ok(())
    }
}

impl ReadService {
    fn is_type_position(
        &self,
        occurrence: &rift_protocol::edge_read::QueryOccurrence,
    ) -> Result<bool, RiftError> {
        let path = ProjectPath::new(
            occurrence
                .source
                .unit
                .project_path()
                .ok_or_else(graph_reference_coverage)?,
        )?;
        let file = self
            .index()
            .file(&path)
            .ok_or_else(graph_reference_coverage)?;
        let range = rift_syntax::ByteRange {
            start: occurrence.source.range.start,
            end: occurrence.source.range.end,
        };
        // SyntaxFacts bounds this scan by the captured source's reference capacity.
        Ok(file.syntax().static_facts().is_some_and(|facts| {
            facts.references().iter().any(|reference| {
                reference.role == ReferenceRole::Type && reference.target.encloses(range)
            })
        }))
    }
}
