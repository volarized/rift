//! RDF projections of the existing search producer's immutable response.

use oxrdf::{BlankNode, Literal, NamedNode, NamedOrBlankNode, Term, Triple, vocab::rdf};
use rift_error::{RiftError, errors};
use rift_protocol::read::{SearchHitTarget, SearchResult};
use serde_json::Value;

use super::{GraphDatasetBuilder, content_blank_node};
use crate::sparql::{RetrievalSelection, VOCABULARY_NAMESPACE, retrieval::RDF_JSON};

impl GraphDatasetBuilder {
    /// Adds the original producer rows to the graph selected by this service.
    ///
    /// # Errors
    /// Refuses a result that exceeds the shared dataset bounds or fails its RDF projection.
    pub fn insert_retrieval(
        &mut self,
        selection: &RetrievalSelection,
        result: &SearchResult,
    ) -> Result<(), RiftError> {
        let offset = selection
            .request
            .page_index
            .checked_mul(selection.request.limit.unwrap_or(0))
            .ok_or_else(projection_failure)?;
        for (index, hit) in result.results.iter().enumerate() {
            let index = offset
                .checked_add(u64::try_from(index).map_err(|_| projection_failure())?)
                .ok_or_else(projection_failure)?;
            let id = content_blank_node(&[selection.graph.as_str(), &index.to_string()]);
            let mut triples = vec![
                Triple::new(id.clone(), rdf::TYPE, vocabulary("SearchHit")),
                Triple::new(id.clone(), vocabulary("index"), Literal::from(index)),
            ];
            let fields = serde_json::to_value(hit).map_err(|_| projection_failure())?;
            project_fields(
                id.clone().into(),
                fields,
                &mut triples,
                self.limits.triples_max(),
            )?;
            if let Some(view) = &result.view {
                let view_node = child(&id.clone().into(), "view", 0);
                triples.push(Triple::new(
                    id.clone(),
                    vocabulary("view"),
                    view_node.clone(),
                ));
                triples.push(Triple::new(
                    view_node.clone(),
                    vocabulary("id"),
                    Literal::from(view.id.as_str().to_owned()),
                ));
                triples.push(Triple::new(
                    view_node,
                    vocabulary("expires_at"),
                    Literal::from(view.expires_at.clone()),
                ));
            }
            if let Some(range) = &hit.range {
                triples.push(Triple::new(
                    id.clone(),
                    vocabulary("start"),
                    Literal::from(range.start),
                ));
                triples.push(Triple::new(
                    id.clone(),
                    vocabulary("end"),
                    Literal::from(range.end),
                ));
            }
            match &hit.hit {
                SearchHitTarget::Symbol { symbol } => {
                    let symbol_id: NamedOrBlankNode = if let Some(identity) = &symbol.id {
                        named(identity.as_str())?.into()
                    } else {
                        content_blank_node(&[id.as_str(), "symbol"]).into()
                    };
                    triples.push(Triple::new(id, vocabulary("symbol"), symbol_id.clone()));
                    triples.push(Triple::new(
                        symbol_id.clone(),
                        rdf::TYPE,
                        vocabulary("Symbol"),
                    ));
                    let fields = serde_json::to_value(symbol).map_err(|_| projection_failure())?;
                    project_fields(symbol_id, fields, &mut triples, self.limits.triples_max())?;
                }
                SearchHitTarget::Commit { commit } => {
                    let commit_id = named(&format!(
                        "{}git/{}",
                        rift_protocol::revision::REVISION_URI_PREFIX,
                        commit.revision.0
                    ))?;
                    triples.push(Triple::new(id, vocabulary("commit"), commit_id.clone()));
                    let fields = serde_json::to_value(commit).map_err(|_| projection_failure())?;
                    project_fields(
                        commit_id.into(),
                        fields,
                        &mut triples,
                        self.limits.triples_max(),
                    )?;
                }
                _ => {}
            }
            self.insert_triples(triples, (&selection.graph).into())?;
        }
        Ok(())
    }
}

fn project_fields(
    subject: NamedOrBlankNode,
    fields: Value,
    triples: &mut Vec<Triple>,
    maximum: usize,
) -> Result<(), RiftError> {
    let mut pending = vec![(subject, fields)];
    while let Some((subject, value)) = pending.pop() {
        let Value::Object(fields) = value else {
            continue;
        };
        for (key, value) in fields {
            match value {
                Value::Array(values) => {
                    for (index, value) in values.into_iter().enumerate() {
                        let child = child(&subject, &key, index);
                        if matches!(key.as_str(), "facets" | "evidence" | "modifiers") {
                            emit(&subject, &key, value, child, &mut pending, triples)?;
                        } else {
                            triples.push(Triple::new(
                                subject.clone(),
                                vocabulary(&key),
                                child.clone(),
                            ));
                            triples.push(Triple::new(
                                child.clone(),
                                vocabulary("index"),
                                Literal::from(
                                    u64::try_from(index).map_err(|_| projection_failure())?,
                                ),
                            ));
                            emit(
                                &child.clone().into(),
                                "value",
                                value,
                                child,
                                &mut pending,
                                triples,
                            )?;
                        }
                        check_size(triples.len(), pending.len(), maximum)?;
                    }
                }
                value => {
                    emit(
                        &subject,
                        &key,
                        value,
                        child(&subject, &key, 0),
                        &mut pending,
                        triples,
                    )?;
                    check_size(triples.len(), pending.len(), maximum)?;
                }
            }
        }
    }
    Ok(())
}

fn check_size(triples: usize, pending: usize, maximum: usize) -> Result<(), RiftError> {
    let required = triples.saturating_add(pending);
    if required > maximum {
        return errors::search::graph_dataset_limit()
            .bound("retrieval triples")
            .limit(maximum)
            .required(required)
            .fail();
    }
    Ok(())
}

fn emit(
    subject: &NamedOrBlankNode,
    key: &str,
    value: Value,
    child: BlankNode,
    pending: &mut Vec<(NamedOrBlankNode, Value)>,
    triples: &mut Vec<Triple>,
) -> Result<(), RiftError> {
    let object: Term = match value {
        Value::Null => return Ok(()),
        Value::Object(_) if key != "extensions" => {
            pending.push((child.clone().into(), value));
            child.into()
        }
        Value::Object(_) | Value::Array(_) => {
            Literal::new_typed_literal(value.to_string(), named(RDF_JSON)?).into()
        }
        Value::String(value) if matches!(key, "facets" | "derivation") => vocabulary(&value).into(),
        Value::String(value)
            if value.starts_with("rift://")
                && matches!(
                    key,
                    "id" | "unit" | "container" | "symbol" | "node" | "evidence"
                ) =>
        {
            named(&value)?.into()
        }
        Value::String(value) => Literal::from(value).into(),
        Value::Bool(value) => Literal::from(value).into(),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                Literal::from(integer).into()
            } else if let Some(integer) = value.as_u64() {
                Literal::from(integer).into()
            } else {
                Literal::from(value.as_f64().ok_or_else(projection_failure)?).into()
            }
        }
    };
    triples.push(Triple::new(subject.clone(), vocabulary(key), object));
    Ok(())
}

fn child(subject: &NamedOrBlankNode, key: &str, index: usize) -> BlankNode {
    content_blank_node(&[&subject.to_string(), key, &index.to_string()])
}

fn vocabulary(key: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{VOCABULARY_NAMESPACE}{key}"))
}

fn named(iri: &str) -> Result<NamedNode, RiftError> {
    NamedNode::new(iri.to_owned()).map_err(|_| projection_failure())
}

fn projection_failure() -> RiftError {
    errors::search::query_evaluation_failed()
        .detail("could not project the retained retrieval result")
        .error()
}
