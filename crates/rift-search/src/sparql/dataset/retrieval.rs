//! RDF projections of retained retrieval results and edges.

use oxrdf::{BlankNode, Literal, NamedNode, NamedOrBlankNode, Term, Triple, vocab::rdf};
use rift_error::{RiftError, errors};
use rift_protocol::read::{SearchHitTarget, SearchResult};
use serde_json::Value;

use super::{GraphDatasetBuilder, content_blank_node};
use crate::sparql::{RetrievalSelection, VOCABULARY_NAMESPACE, retrieval::RDF_JSON};

impl GraphDatasetBuilder {
    /// Adds one edge with its analysis and physical occurrence.
    /// An original record identity keeps equal records separate; local edges carry none.
    ///
    /// # Errors
    /// Refuses an edge that exceeds the dataset bounds or fails its RDF projection.
    pub fn insert_edge(
        &mut self,
        edge: &rift_protocol::read::Edge,
        record: Option<&rift_protocol::edge_read::EdgeRecordId>,
    ) -> Result<(), RiftError> {
        let fields = serde_json::to_value(edge).map_err(|_| projection_failure())?;
        let encoded = fields.to_string();
        let key = record.map_or("", rift_protocol::edge_read::EdgeRecordId::as_str);
        let id = content_blank_node(&["edge", key, &encoded]);
        let mut triples = vec![Triple::new(
            id.clone(),
            rdf::TYPE,
            super::super::vocabulary::VocabularyClass::Edge.iri(),
        )];
        project_fields(id.into(), fields, &mut triples, self.limits.triples_max())?;
        self.insert_triples(triples, oxrdf::GraphNameRef::DefaultGraph)
    }

    /// Adds the original producer rows to the graph selected by this service.
    ///
    /// # Errors
    /// Refuses a result that exceeds the shared dataset bounds or fails its RDF projection.
    pub fn insert_retrieval(
        &mut self,
        selection: &RetrievalSelection,
        result: &SearchResult,
    ) -> Result<(), RiftError> {
        // `page` is 1-based: the first hit of page `p` is hit `(p - 1) * max_results`.
        let offset = selection
            .request
            .page
            .saturating_sub(1)
            .checked_mul(selection.request.max_results.unwrap_or(0))
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
                let view_syntax_element = child(&id.clone().into(), "view", 0);
                triples.push(Triple::new(
                    id.clone(),
                    vocabulary("view"),
                    view_syntax_element.clone(),
                ));
                triples.push(Triple::new(
                    view_syntax_element.clone(),
                    vocabulary("id"),
                    Literal::from(view.id.as_str().to_owned()),
                ));
                triples.push(Triple::new(
                    view_syntax_element,
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
                        if matches!(key.as_str(), "kinds" | "evidence" | "modifiers") {
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
        Value::String(value) if matches!(key, "kinds" | "derivation") => vocabulary(&value).into(),
        Value::String(value)
            if value.starts_with("rift://")
                && matches!(
                    key,
                    "id" | "unit"
                        | "container"
                        | "symbol"
                        | "syntax_element"
                        | "evidence"
                        | "from"
                        | "to"
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

#[cfg(test)]
mod tests {
    use rift_protocol::query::QueryAnswer;
    use rift_protocol::read::SearchResult;
    use serde_json::json;

    use crate::sparql::{GraphDataset, GraphLimits, GraphQuery, QueryCancellation};

    #[test]
    fn retrieval_index_counts_hits_from_the_first_page() -> Result<(), Box<dyn std::error::Error>> {
        for (page, expected) in [(1, ["0", "1"]), (3, ["4", "5"])] {
            let limits = GraphLimits::default();
            let query = GraphQuery::parse(
                &format!(
                    "SELECT ?index {{ SERVICE :search {{ ?hit :pattern 'beacon'; \
                     :max_results 2; :page {page}; :index ?index }} }} ORDER BY ?index"
                ),
                &limits,
            )?;
            let [selection] = query.retrieval_requests() else {
                return Err("one retrieval service".into());
            };
            let hit = |path: &str| json!({"hit": {"target": "file", "size": 1}, "path": path});
            let result: SearchResult = serde_json::from_value(json!({
                "results": [hit("a.rs"), hit("b.rs")],
                "pagination": {"page": page, "pages": 3, "max_results": 2, "total": 6},
            }))?;
            let mut builder = GraphDataset::builder(limits);
            builder.insert_retrieval(selection, &result)?;
            let answer: QueryAnswer = builder
                .build()
                .evaluate(&query, &QueryCancellation::new())?
                .into();
            let QueryAnswer::Bindings { bindings, .. } = answer else {
                return Err("SELECT answers bindings".into());
            };
            let rows = serde_json::to_value(&bindings)?;
            let indexes = rows
                .as_array()
                .ok_or("bindings encode as an array")?
                .iter()
                .map(|row| {
                    row["index"]["value"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect::<Vec<_>>();
            assert_eq!(indexes, expected, "page {page}");
        }
        Ok(())
    }
}
