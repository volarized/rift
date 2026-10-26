//! Local services evaluate pre-acquired results through the same SPARQL library.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use oxigraph::model::{BlankNode, Literal, NamedNode, NamedNodeRef, Term, Variable};
use oxigraph::sparql::{QueryResults, QuerySolutionIter, ServiceHandler, SparqlEvaluator};
use oxiri::Iri;
use oxstr::OxString;
use rift_protocol::query::{QueryAnswer, RdfTerm};
use rift_protocol::read::{SearchParams, SearchResult};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use spargebra::Query;
use spargebra::algebra::QueryExpression;
use spargebra::term::{NamedNodePattern, TermPattern};

use super::dataset::VirtualDataset;
use super::{
    QueryCancellation, QueryError, QueryFacts, QueryLimits, SEARCH_SERVICE, VOCABULARY, failure,
};

/// One complete result produced by the existing retrieval owner.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryServiceData {
    /// The accepted request whose results were acquired.
    pub request: SearchParams,
    /// Complete selected hits and their original serving context.
    pub result: SearchResult,
    /// Original selected owner and provider endpoint for each result row.
    /// Owner zero selects primary facts; later owners select the additional bodies in order.
    pub targets: Vec<Option<(usize, crate::query::QueryProviderEndpoint)>>,
}

/// One retained table, addressed by its execution's service IRI.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RetainedResult {
    /// The execution IRI selected in the SPARQL statement.
    pub service: String,
    /// The original result, retaining duplicates, generated terms and unbound variables.
    pub answer: QueryAnswer,
}

/// Adapter-owned local service inputs; callbacks perform no acquisition or I/O.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryServices {
    /// Complete results from existing ranked, regex, revision and change producers.
    pub retrievals: Vec<QueryServiceData>,
    /// Previously retained binding tables.
    pub bindings: Vec<RetainedResult>,
    /// Complete facts for additional accepted named graphs.
    pub datasets: Vec<QueryDataset>,
}

/// One accepted named graph and its complete original selected facts.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryDataset {
    /// Expanded named graph IRI, or null when this owner has no named graph.
    #[serde(deserialize_with = "required_graph")]
    pub graph: Option<String>,
    /// Facts acquired from the graph's selected publication.
    pub facts: QueryFacts,
    /// Original identities and the exact public identities advertised for this owner.
    pub identities: QueryIdentities,
    /// Whether this owner contributes facts to the default graph.
    pub default: bool,
}

fn required_graph<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

/// Checked identity translation applied before RDF matching and joins.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryIdentities {
    /// Original and advertised canonical symbol identities.
    pub symbols: Vec<(rift_protocol::read::SymbolId, rift_protocol::read::SymbolId)>,
    /// Original and advertised physical source-unit identities.
    pub sources: Vec<(
        rift_protocol::read::SourceUnitId,
        rift_protocol::read::SourceUnitId,
    )>,
    /// Full selection digest used to scope generated blank-node labels.
    pub blank_prefix: rift_protocol::identity::SourceDigest,
}

impl QueryServices {
    pub(super) fn install(
        self,
        mut evaluator: SparqlEvaluator,
        limits: QueryLimits,
        cancellation: &QueryCancellation,
        dataset: VirtualDataset,
    ) -> Result<SparqlEvaluator, QueryError> {
        for record in &self.retrievals {
            if record.targets.len() != record.result.results.len() {
                return Err(failure::evaluation(
                    "retrieval record associations do not match selected rows",
                ));
            }
            for (hit, target) in record.result.results.iter().zip(&record.targets) {
                match (&hit.hit, target) {
                    (
                        rift_protocol::read::SearchHitTarget::Symbol { symbol },
                        Some((owner, endpoint)),
                    ) => {
                        let selected = dataset.selected_resource(*owner, endpoint)?;
                        if let Some(identity) = &symbol.id
                            && selected
                                != Term::NamedNode(
                                    NamedNodeRef::new(identity.as_str())
                                        .map_err(failure::invalid)?
                                        .into_owned(),
                                )
                        {
                            return Err(failure::evaluation(
                                "retrieval symbol differs from its original selected record",
                            ));
                        }
                    }
                    (rift_protocol::read::SearchHitTarget::Symbol { symbol }, None) => {
                        let identity = symbol.id.as_ref().ok_or_else(|| {
                            failure::evaluation("retrieval symbol has no original selected record")
                        })?;
                        crate::query::QueryProviderTarget::canonical(identity)?;
                    }
                    (_, None) => {}
                    (_, Some(_)) => {
                        return Err(failure::evaluation(
                            "retrieval record association has a different result target",
                        ));
                    }
                }
            }
        }
        if !self.retrievals.is_empty() {
            evaluator = evaluator.with_service_handler(
                NamedNode::new(SEARCH_SERVICE).map_err(failure::invalid)?,
                RetrievalHandler {
                    records: self.retrievals.into_iter().map(Arc::new).collect(),
                    limits,
                    cancellation: cancellation.clone(),
                    dataset: Arc::new(dataset),
                },
            );
        }
        let mut names = BTreeSet::new();
        for table in self.bindings {
            if !names.insert(table.service.clone()) {
                return Err(failure::parse(
                    "retained service identities must be distinct",
                ));
            }
            let handler = RetainedHandler::new(table.answer, limits, cancellation.clone());
            evaluator = evaluator.with_service_handler(
                NamedNode::new(table.service).map_err(failure::invalid)?,
                handler,
            );
        }
        Ok(evaluator)
    }

    pub(super) fn validate(
        &self,
        requirements: &crate::query::FactRequirements,
    ) -> Result<(), QueryError> {
        for service in &requirements.services {
            let available = if service == SEARCH_SERVICE {
                !self.retrievals.is_empty()
            } else {
                self.bindings.iter().any(|table| table.service == *service)
            };
            if !available {
                return Err(failure::evaluation(format!(
                    "required local service {service} is unavailable"
                )));
            }
        }
        for request in &requirements.retrievals {
            if !self
                .retrievals
                .iter()
                .any(|record| record.request == *request)
            {
                return Err(failure::evaluation(
                    "required retrieval result is unavailable",
                ));
            }
        }
        Ok(())
    }
}

struct RetrievalHandler {
    records: Vec<Arc<QueryServiceData>>,
    limits: QueryLimits,
    cancellation: QueryCancellation,
    dataset: Arc<VirtualDataset>,
}

impl ServiceHandler for RetrievalHandler {
    type Error = QueryError;

    fn handle(
        &self,
        pattern: &QueryExpression,
        base_iri: Option<&Iri<OxString>>,
    ) -> Result<QuerySolutionIter<'static>, QueryError> {
        let request = request(pattern, self.limits.algebra_nodes_max)?;
        let record = self
            .records
            .iter()
            .find(|record| record.request == request)
            .ok_or_else(|| failure::evaluation("required retrieval result is unavailable"))?;
        let dataset = VirtualDataset::new(
            QueryFacts::default(),
            self.limits,
            self.cancellation.0.clone(),
        )?
        .with_retrieval(Arc::clone(record), Arc::clone(&self.dataset));
        let query = Query::Select(spargebra::query::SelectQuery {
            dataset: None,
            expression: execution_pattern(pattern, self.limits.algebra_nodes_max)?,
            base_iri: base_iri.cloned(),
        });
        solutions(
            SparqlEvaluator::new()
                .with_cancellation_token(self.cancellation.0.clone())
                .for_query(query)
                .on_queryable_dataset(dataset)
                .execute()
                .map_err(super::result::evaluation_error)?,
        )
    }
}

struct BindingHandler {
    variables: Vec<String>,
    bindings: Arc<Vec<BTreeMap<String, RdfTerm>>>,
    limits: QueryLimits,
    cancellation: QueryCancellation,
}

enum RetainedHandler {
    Bindings(BindingHandler),
    Graph {
        triples: Arc<Vec<rift_protocol::query::RdfTriple>>,
        limits: QueryLimits,
        cancellation: QueryCancellation,
    },
}

impl RetainedHandler {
    fn new(answer: QueryAnswer, limits: QueryLimits, cancellation: QueryCancellation) -> Self {
        match answer {
            QueryAnswer::Bindings {
                variables,
                bindings,
            } => Self::Bindings(BindingHandler {
                variables,
                bindings: Arc::new(bindings),
                limits,
                cancellation,
            }),
            QueryAnswer::Graph { triples } => Self::Graph {
                triples: Arc::new(triples),
                limits,
                cancellation,
            },
            QueryAnswer::Boolean { value } => {
                let answer = BTreeMap::from([(
                    "value".to_owned(),
                    RdfTerm::Literal {
                        value: value.to_string(),
                        datatype: "http://www.w3.org/2001/XMLSchema#boolean".to_owned(),
                    },
                )]);
                Self::Bindings(BindingHandler {
                    variables: vec!["value".to_owned()],
                    bindings: Arc::new(vec![answer]),
                    limits,
                    cancellation,
                })
            }
        }
    }
}

impl ServiceHandler for RetainedHandler {
    type Error = QueryError;

    fn handle(
        &self,
        pattern: &QueryExpression,
        base_iri: Option<&Iri<OxString>>,
    ) -> Result<QuerySolutionIter<'static>, QueryError> {
        match self {
            Self::Bindings(handler) => handler.handle(pattern, base_iri),
            Self::Graph {
                triples,
                limits,
                cancellation,
            } => {
                let dataset =
                    VirtualDataset::new(QueryFacts::default(), *limits, cancellation.0.clone())?
                        .with_retained_graph(Arc::clone(triples));
                let query = Query::Select(spargebra::query::SelectQuery {
                    dataset: None,
                    expression: pattern.clone(),
                    base_iri: base_iri.cloned(),
                });
                solutions(
                    SparqlEvaluator::new()
                        .with_cancellation_token(cancellation.0.clone())
                        .for_query(query)
                        .on_queryable_dataset(dataset)
                        .execute()
                        .map_err(super::result::evaluation_error)?,
                )
            }
        }
    }
}

impl ServiceHandler for BindingHandler {
    type Error = QueryError;

    fn handle(
        &self,
        pattern: &QueryExpression,
        base_iri: Option<&Iri<OxString>>,
    ) -> Result<QuerySolutionIter<'static>, QueryError> {
        if self.bindings.len() > self.limits.result_items_max {
            return Err(failure::bound("result_items_max"));
        }
        let mut variables = self
            .variables
            .iter()
            .map(|name| Variable::new(name.clone()).map_err(failure::invalid))
            .collect::<Result<Vec<_>, _>>()?;
        pattern.on_in_scope_variable(|variable| {
            if !variables.contains(variable) {
                variables.push(variable.clone());
            }
        });
        let query = Query::Select(spargebra::query::SelectQuery {
            dataset: None,
            expression: QueryExpression::Project {
                inner: Box::new(pattern.clone()),
                variables: variables.clone(),
            },
            base_iri: base_iri.cloned(),
        });
        let evaluator = SparqlEvaluator::new()
            .with_cancellation_token(self.cancellation.0.clone())
            .for_query(query);
        let dataset = VirtualDataset::new(
            QueryFacts::default(),
            self.limits,
            self.cancellation.0.clone(),
        )?;
        let rows = Arc::clone(&self.bindings);
        let iterator = (0..rows.len()).flat_map(
            move |index| -> Box<
                dyn Iterator<
                    Item = Result<
                        oxigraph::sparql::QuerySolution,
                        oxigraph::sparql::QueryEvaluationError,
                    >,
                >,
            > {
                let mut query = evaluator.clone();
                for (name, value) in &rows[index] {
                    let substituted = Variable::new(name.clone())
                        .map_err(failure::invalid)
                        .and_then(|variable| rdf_term(value).map(|term| (variable, term)));
                    match substituted {
                        Ok((variable, term)) => query = query.substitute_variable(variable, term),
                        Err(error) => {
                            return Box::new(std::iter::once(Err(
                                oxigraph::sparql::QueryEvaluationError::Service(Box::new(error)),
                            )));
                        }
                    }
                }
                match query.on_queryable_dataset(dataset.clone()).execute() {
                    Ok(QueryResults::Solutions(values)) => Box::new(values),
                    Ok(_) => Box::new(std::iter::empty()),
                    Err(error) => Box::new(std::iter::once(Err(error))),
                }
            },
        );
        Ok(QuerySolutionIter::new(variables.into(), iterator))
    }
}

fn solutions(result: QueryResults<'static>) -> Result<QuerySolutionIter<'static>, QueryError> {
    match result {
        QueryResults::Solutions(values) => Ok(values),
        _ => Err(failure::evaluation("a query service must return bindings")),
    }
}

pub(super) fn request(
    pattern: &QueryExpression,
    nodes_max: usize,
) -> Result<SearchParams, QueryError> {
    let mut fields = Map::new();
    let mut pending = vec![pattern];
    let mut visited = 0usize;
    let mut include = BTreeSet::new();
    while let Some(pattern) = pending.pop() {
        visited += 1;
        if visited > nodes_max || pending.len() > nodes_max {
            return Err(failure::bound("algebra_nodes_max"));
        }
        match pattern {
            QueryExpression::Bgp { patterns } => {
                if patterns.len() > nodes_max {
                    return Err(failure::bound("algebra_nodes_max"));
                }
                for triple in patterns {
                    let NamedNodePattern::NamedNode(predicate) = &triple.predicate else {
                        continue;
                    };
                    let Some(key) = predicate.as_str().strip_prefix(VOCABULARY) else {
                        continue;
                    };
                    if ["score", "source"].contains(&key) {
                        include.insert(key.to_owned());
                    }
                    if INPUT_FIELDS.contains(&key) {
                        input(&mut fields, key, &triple.object)?;
                    }
                }
            }
            QueryExpression::Join { left, right }
            | QueryExpression::Union { left, right }
            | QueryExpression::Minus { left, right }
            | QueryExpression::LeftJoin { left, right, .. }
            | QueryExpression::Lateral { left, right } => {
                pending.extend([left.as_ref(), right.as_ref()]);
            }
            QueryExpression::Filter { inner, .. }
            | QueryExpression::Extend { inner, .. }
            | QueryExpression::Graph { inner, .. }
            | QueryExpression::OrderBy { inner, .. }
            | QueryExpression::Project { inner, .. }
            | QueryExpression::Distinct { inner }
            | QueryExpression::Reduced { inner }
            | QueryExpression::Slice { inner, .. }
            | QueryExpression::Group { inner, .. } => pending.push(inner),
            QueryExpression::Service { .. } => {
                return Err(failure::parse(
                    "retrieval services cannot acquire another service",
                ));
            }
            QueryExpression::Path { .. } | QueryExpression::Values { .. } => {}
        }
    }
    if !include.is_empty() && !fields.contains_key("include") {
        fields.insert(
            "include".into(),
            Value::Array(include.into_iter().map(Value::String).collect()),
        );
    }
    serde_json::from_value(Value::Object(fields)).map_err(failure::parse)
}

const INPUT_FIELDS: &[&str] = &[
    "view",
    "query",
    "pattern",
    "scope",
    "target",
    "order",
    "packages",
    "paths",
    "include",
    "limit",
    "page_index",
    "rev",
    "change",
];

fn input(
    fields: &mut Map<String, Value>,
    key: &str,
    object: &TermPattern,
) -> Result<(), QueryError> {
    if ["query", "pattern"].contains(&key)
        && match object {
            TermPattern::Literal(value) => {
                value.datatype().as_str() != "http://www.w3.org/2001/XMLSchema#string"
            }
            TermPattern::Variable(_) => false,
            TermPattern::NamedNode(_) => true,
        }
    {
        return Err(failure::parse(format!(
            "retrieval input {key} must be a string literal"
        )));
    }
    let value = match object {
        TermPattern::Literal(value)
            if value.datatype().as_str() == "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON" =>
        {
            serde_json::from_str(value.value()).map_err(failure::parse)?
        }
        TermPattern::Literal(value) if ["limit", "page_index"].contains(&key) => {
            Value::from(value.value().parse::<u64>().map_err(failure::parse)?)
        }
        TermPattern::Literal(value) => Value::String(value.value().to_owned()),
        TermPattern::NamedNode(value) => Value::String(
            value
                .as_str()
                .strip_prefix(VOCABULARY)
                .unwrap_or(value.as_str())
                .to_owned(),
        ),
        TermPattern::Variable(_) => return Ok(()),
    };
    if let Some(previous) = fields.insert(key.to_owned(), value.clone())
        && previous != value
    {
        return Err(failure::parse(format!(
            "retrieval input {key} carries conflicting values"
        )));
    }
    Ok(())
}

pub(super) fn rdf_term(value: &RdfTerm) -> Result<Term, QueryError> {
    match value {
        RdfTerm::Iri { value } => NamedNode::new(value.clone())
            .map(Term::from)
            .map_err(failure::invalid),
        RdfTerm::BlankNode { value } => BlankNode::new(value.clone())
            .map(Term::from)
            .map_err(failure::invalid),
        RdfTerm::Literal { value, datatype } => Ok(Literal::new_typed_literal(
            value.clone(),
            NamedNode::new(datatype.clone()).map_err(failure::invalid)?,
        )
        .into()),
        RdfTerm::LanguageLiteral { value, language } => {
            Literal::new_language_tagged_literal(value.clone(), language.clone())
                .map(Term::from)
                .map_err(failure::invalid)
        }
    }
}

fn execution_pattern(
    pattern: &QueryExpression,
    nodes_max: usize,
) -> Result<QueryExpression, QueryError> {
    let rdf_type =
        oxigraph::model::NamedNode::new("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
            .map_err(failure::parse)?;
    let hit_class = oxigraph::model::NamedNode::new(format!("{VOCABULARY}SearchHit"))
        .map_err(failure::parse)?;
    let mut pattern = pattern.clone();
    let mut pending = vec![&mut pattern];
    let mut visited = 0usize;
    while let Some(pattern) = pending.pop() {
        visited += 1;
        if visited > nodes_max || pending.len() > nodes_max {
            return Err(failure::bound("algebra_nodes_max"));
        }
        match pattern {
            QueryExpression::Bgp { patterns } => {
                let mut anchors = Vec::new();
                patterns.retain(|triple| {
                    let NamedNodePattern::NamedNode(predicate) = &triple.predicate else {
                        return true;
                    };
                    let Some(key) = predicate.as_str().strip_prefix(VOCABULARY) else {
                        return true;
                    };
                    if !INPUT_FIELDS.contains(&key)
                        || matches!(triple.object, TermPattern::Variable(_))
                    {
                        return true;
                    }
                    let mut anchor = triple.clone();
                    anchor.predicate = NamedNodePattern::NamedNode(rdf_type.clone());
                    anchor.object = TermPattern::NamedNode(hit_class.clone());
                    if !anchors.contains(&anchor) {
                        anchors.push(anchor);
                    }
                    false
                });
                patterns.extend(anchors);
            }
            QueryExpression::Join { left, right }
            | QueryExpression::Union { left, right }
            | QueryExpression::Minus { left, right }
            | QueryExpression::LeftJoin { left, right, .. }
            | QueryExpression::Lateral { left, right } => {
                pending.extend([left.as_mut(), right.as_mut()]);
            }
            QueryExpression::Filter { inner, .. }
            | QueryExpression::Extend { inner, .. }
            | QueryExpression::Graph { inner, .. }
            | QueryExpression::OrderBy { inner, .. }
            | QueryExpression::Project { inner, .. }
            | QueryExpression::Distinct { inner }
            | QueryExpression::Reduced { inner }
            | QueryExpression::Slice { inner, .. }
            | QueryExpression::Group { inner, .. } => pending.push(inner),
            QueryExpression::Service { .. }
            | QueryExpression::Path { .. }
            | QueryExpression::Values { .. } => {}
        }
    }
    Ok(pattern)
}

#[cfg(test)]
mod input_tests {
    use super::{Literal, Map, NamedNode, TermPattern, Value, input};

    #[test]
    fn textual_inputs_preserve_their_rdf_datatype() -> Result<(), Box<dyn std::error::Error>> {
        for key in ["query", "pattern"] {
            for value in [
                TermPattern::Literal(Literal::from(42u64)),
                TermPattern::Literal(Literal::new_language_tagged_literal("text", "en")?),
                TermPattern::Literal(Literal::new_typed_literal(
                    "42",
                    NamedNode::new("http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON")?,
                )),
                TermPattern::NamedNode(NamedNode::new("urn:rift:vocabulary:text")?),
            ] {
                assert!(input(&mut Map::new(), key, &value).is_err());
            }
            let mut fields = Map::new();
            let text = "42 \"quoted\" \\ unicode λ\nnext";
            input(
                &mut fields,
                key,
                &TermPattern::Literal(Literal::new_simple_literal(text)),
            )?;
            assert_eq!(fields.get(key), Some(&Value::String(text.to_owned())));
        }
        Ok(())
    }
}

#[cfg(test)]
mod dataset_owner_tests {
    use super::*;

    #[test]
    fn selected_owner_graph_requires_presence_and_accepts_explicit_null()
    -> Result<(), Box<dyn std::error::Error>> {
        let selected = QueryDataset {
            graph: None,
            facts: QueryFacts::default(),
            identities: QueryIdentities {
                symbols: Vec::new(),
                sources: Vec::new(),
                blank_prefix: rift_protocol::identity::SourceDigest::parse(&"a".repeat(64))?,
            },
            default: true,
        };
        let mut encoded = serde_json::to_value(selected)?;
        assert!(encoded["graph"].is_null());
        assert!(
            serde_json::from_value::<QueryDataset>(encoded.clone())?
                .graph
                .is_none()
        );
        encoded
            .as_object_mut()
            .ok_or("selected owner")?
            .remove("graph");
        assert!(serde_json::from_value::<QueryDataset>(encoded).is_err());
        Ok(())
    }
}
