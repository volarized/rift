//! RDF projection of acquired call paths and their call sites.

use std::sync::Arc;

use oxrdf::{BlankNode, Literal, NamedNode, Triple, vocab::rdf};
use rift_error::RiftError;
use rift_protocol::read::{RelationshipFacet, SourceUnitId, SourceUnitSpan, SymbolId, TextRange};
use rift_protocol::relationship_read::QueryOccurrence;

use super::{GraphDatasetBuilder, GraphFact, TripleWriter, content_blank_node};
use crate::flow::{FlowPath, FlowResult};
use crate::sparql::{
    RetrievalSelection, VOCABULARY_NAMESPACE, VocabularyProperty, retrieval::FLOW_CLASS,
};

/// The source unit and half-open byte offsets of one call site, when established.
pub type FlowCallSite = Option<(SourceUnitId, u64, u64)>;

/// Call paths and original call sites from one ranked retrieval.
pub type QueryFlows = FlowResult<Arc<SymbolId>, FlowCallSite>;

impl GraphDatasetBuilder {
    /// Projects complete flow records into their service's named graph.
    ///
    /// # Errors
    /// Refuses invalid endpoint identities or a projection past the dataset bounds.
    pub fn insert_flows(
        &mut self,
        selection: &RetrievalSelection,
        flows: &QueryFlows,
    ) -> Result<(), RiftError> {
        for path in &flows.paths {
            let mut writer = TripleWriter::default();
            write_path(&mut writer, flows, path)?;
            self.insert_triples(writer.0, (&selection.graph).into())?;
        }
        Ok(())
    }
}

fn write_path(
    writer: &mut TripleWriter,
    flows: &QueryFlows,
    path: &FlowPath<Arc<SymbolId>>,
) -> Result<(), RiftError> {
    let parts: Vec<_> = std::iter::once("flow")
        .chain(path.steps.iter().map(|symbol| symbol.as_str()))
        .collect();
    let flow = content_blank_node(&parts);
    writer.0.extend([
        Triple::new(flow.clone(), rdf::TYPE, term(FLOW_CLASS)),
        Triple::new(flow.clone(), term("score"), Literal::from(path.score)),
    ]);
    let mut previous = None;
    for (index, symbol) in (0_u64..).zip(&path.steps) {
        let step = content_blank_node(&[flow.as_str(), &index.to_string()]);
        let value = writer.symbol(symbol)?;
        writer.0.extend([
            Triple::new(flow.clone(), term("steps"), step.clone()),
            Triple::new(
                step.clone(),
                VocabularyProperty::Index.iri(),
                Literal::from(index),
            ),
            Triple::new(step.clone(), term("value"), value),
        ]);
        if let Some(caller) = previous {
            write_sites(writer, &step, flows, caller, symbol)?;
        }
        previous = Some(symbol);
    }
    Ok(())
}

fn write_sites(
    writer: &mut TripleWriter,
    step: &BlankNode,
    flows: &QueryFlows,
    caller: &Arc<SymbolId>,
    target: &Arc<SymbolId>,
) -> Result<(), RiftError> {
    let sites = flows.evidence(Arc::clone(caller), Arc::clone(target));
    for (unit, start, end) in sites.into_iter().flatten().flatten() {
        let key = format!("{} {} {} {}", target.as_str(), unit.as_str(), start, end);
        let occurrence = QueryOccurrence {
            target: target.as_ref().clone(),
            enclosing: Some(caller.as_ref().clone()),
            source: SourceUnitSpan {
                unit: unit.clone(),
                range: TextRange {
                    start: *start,
                    end: *end,
                },
            },
            facet: RelationshipFacet::Calls,
        };
        GraphFact::Occurrence {
            key: &key,
            occurrence: &occurrence,
            source: None,
        }
        .write_triples(writer)?;
        let site = content_blank_node(&["occurrence", &key]);
        writer
            .0
            .push(Triple::new(step.clone(), term("evidence"), site));
    }
    Ok(())
}

fn term(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{VOCABULARY_NAMESPACE}{local}"))
}
