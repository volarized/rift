//! Virtual RDF projection over the selected domain records.

mod identity;
mod nodes;
mod provider;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use oxigraph::model::{BlankNode, GraphName, Literal, NamedNode, NamedOrBlankNode, Term};
use oxigraph::sparql::CancellationToken;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use spareval::{InternalQuad, QueryableDataset};

use super::failure;
use super::{QueryError, QueryFacts, QueryLimits, QuerySymbol, VOCABULARY};

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

#[derive(Clone)]
pub(super) struct VirtualDataset {
    facts: Arc<QueryFacts>,
    occurrences: Arc<OccurrenceIndex>,
    providers: Arc<provider::ProviderIndex>,
    limits: QueryLimits,
    cancellation: CancellationToken,
    retrieval: Option<Arc<super::services::QueryServiceData>>,
    retained_graph: Option<Arc<Vec<rift_protocol::query::RdfTriple>>>,
    datasets: Arc<Vec<(Term, Self)>>,
    identities: Arc<identity::IdentityMap>,
    default_graph: bool,
}

#[derive(Default)]
struct OccurrenceIndex {
    resources: Vec<Term>,
    by_resource: HashMap<Term, Vec<usize>>,
}

impl OccurrenceIndex {
    fn new(
        facts: &QueryFacts,
        limits: QueryLimits,
        cancellation: &CancellationToken,
    ) -> Result<Self, QueryError> {
        let mut result = Self::default();
        let mut bytes = 0usize;
        for (index, occurrence) in facts
            .occurrences
            .iter()
            .chain(
                facts
                    .occurrence_records
                    .iter()
                    .map(|record| &record.occurrence),
            )
            .enumerate()
        {
            if cancellation.is_cancelled() {
                return Err(failure::cancelled());
            }
            let resource = blank(
                &serde_json::to_vec(&(
                    &occurrence.target,
                    &occurrence.enclosing,
                    &occurrence.source,
                ))
                .map_err(invalid)?,
            )?;
            let copies = if result.by_resource.contains_key(&resource) {
                1
            } else {
                2
            };
            bytes = term_bytes(&resource)
                .checked_mul(copies)
                .and_then(|count| bytes.checked_add(count))
                .ok_or_else(|| failure::bound("rdf_bytes_max"))?;
            if bytes > limits.rdf_bytes_max {
                return Err(failure::bound("rdf_bytes_max"));
            }
            result
                .by_resource
                .entry(resource.clone())
                .or_default()
                .push(index);
            result.resources.push(resource);
        }
        Ok(result)
    }
}

impl VirtualDataset {
    pub(super) fn new(
        facts: QueryFacts,
        limits: QueryLimits,
        cancellation: CancellationToken,
    ) -> Result<Self, QueryError> {
        let count = fact_count(&facts)?;
        if count > limits.quads_max {
            return Err(failure::bound("quads_max"));
        }
        nodes::validate(&facts)?;
        let mut records = HashSet::new();
        for record in &facts.relationship_records {
            if !records.insert(record.id.as_str()) {
                return Err(failure::invalid(
                    "a selected relationship record identity repeats",
                ));
            }
        }
        let occurrences = Arc::new(OccurrenceIndex::new(&facts, limits, &cancellation)?);
        let providers = Arc::new(provider::ProviderIndex::new(&facts, limits, &cancellation)?);
        Ok(Self {
            facts: Arc::new(facts),
            occurrences,
            providers,
            limits,
            cancellation,
            retrieval: None,
            retained_graph: None,
            datasets: Arc::new(Vec::new()),
            identities: Arc::new(identity::IdentityMap::default()),
            default_graph: true,
        })
    }

    fn project(&self, pattern: Pattern) -> Result<Vec<InternalQuad<Term>>, QueryError> {
        let mut output = Projection::new(pattern.clone(), self.limits, self.cancellation.clone())?;
        self.project_owner(&mut output, &pattern, iri(super::LOCAL_GRAPH)?)?;
        for (graph, selected) in self.datasets.iter() {
            if pattern.graph_name != Some(GraphName::DefaultGraph) || selected.default_graph {
                selected.project_owner(&mut output, &pattern, graph.clone())?;
            }
        }
        Ok(output.quads)
    }

    fn project_owner(
        &self,
        output: &mut Projection,
        pattern: &Pattern,
        graph: Term,
    ) -> Result<(), QueryError> {
        let Some(pattern) = self.identities.lookup_pattern(pattern) else {
            return Ok(());
        };
        output.pattern = pattern;
        output.graph = graph;
        output.identities = Arc::clone(&self.identities);
        if output.graph_selected() {
            self.project_into(output)?;
        }
        Ok(())
    }

    fn project_into(&self, output: &mut Projection) -> Result<(), QueryError> {
        self.symbols(output)?;
        self.relationships(output)?;
        self.providers.project(&self.facts, output)?;
        self.occurrences(output)?;
        self.occurrence_evidence(output)?;
        self.sources(output)?;
        self.other_records(output)?;
        self.retrieval(output)?;
        self.retained_graph(output)
    }

    pub(super) fn with_datasets(
        mut self,
        datasets: Vec<super::services::QueryDataset>,
    ) -> Result<Self, QueryError> {
        if datasets.len() > self.limits.quads_max {
            return Err(failure::bound("quads_max"));
        }
        let mut names = HashSet::from([iri(super::LOCAL_GRAPH)?]);
        let mut selected = Vec::with_capacity(datasets.len());
        let mut count = fact_count(&self.facts)?;
        for dataset in datasets {
            if self.cancellation.is_cancelled() {
                return Err(failure::cancelled());
            }
            let graph = iri(&dataset.graph)?;
            if !names.insert(graph.clone()) {
                return Err(failure::invalid(
                    "selected graph identities must be distinct",
                ));
            }
            count = count
                .checked_add(fact_count(&dataset.facts)?)
                .ok_or_else(|| failure::bound("quads_max"))?;
            if count > self.limits.quads_max {
                return Err(failure::bound("quads_max"));
            }
            let mut owner = Self::new(dataset.facts, self.limits, self.cancellation.clone())?;
            owner.identities = Arc::new(identity::IdentityMap::new(dataset.identities)?);
            owner.default_graph = dataset.default;
            selected.push((graph, owner));
        }
        crate::query::validate_source_selections_by_unit(
            std::iter::once(&self)
                .chain(selected.iter().map(|(_, owner)| owner))
                .flat_map(|owner| {
                    owner.facts.sources.iter().map(move |source| {
                        owner
                            .identities
                            .source_unit(source)
                            .map(|unit| (unit, source))
                    })
                }),
        )?;
        self.datasets = Arc::new(selected);
        Ok(self)
    }

    pub(super) fn with_retrieval(
        mut self,
        retrieval: Arc<super::services::QueryServiceData>,
    ) -> Self {
        self.retrieval = Some(retrieval);
        self
    }

    pub(super) fn with_retained_graph(
        mut self,
        triples: Arc<Vec<rift_protocol::query::RdfTriple>>,
    ) -> Self {
        self.retained_graph = Some(triples);
        self
    }

    fn retrieval(&self, output: &mut Projection) -> Result<(), QueryError> {
        let Some(retrieval) = &self.retrieval else {
            return Ok(());
        };
        let request = serde_json::to_vec(&retrieval.request).map_err(invalid)?;
        let request_digest = Sha256::digest(request);
        for (index, hit) in retrieval.result.results.iter().enumerate() {
            let resource =
                blank(&serde_json::to_vec(&(request_digest.as_slice(), index)).map_err(invalid)?)?;
            output.class(resource.clone(), "SearchHit")?;
            output.fields(resource.clone(), hit)?;
            output.push(
                resource.clone(),
                vocabulary("index")?,
                Literal::from(u64::try_from(index).map_err(invalid)?).into(),
            )?;
            if let Some(range) = &hit.range {
                output.push(
                    resource.clone(),
                    vocabulary("start")?,
                    Literal::from(range.start).into(),
                )?;
                output.push(
                    resource.clone(),
                    vocabulary("end")?,
                    Literal::from(range.end).into(),
                )?;
            }
            if let rift_protocol::read::SearchHitTarget::Symbol { symbol } = &hit.hit {
                let subject = match &symbol.id {
                    Some(identity) => iri(identity.as_str())?,
                    None => child(&resource, "symbol", 0)?,
                };
                output.push(resource, vocabulary("symbol")?, subject.clone())?;
                output.symbol(
                    &subject,
                    &QuerySymbol {
                        symbol: symbol.as_ref().clone(),
                        declarations: Vec::new(),
                    },
                )?;
            }
        }
        Ok(())
    }

    fn retained_graph(&self, output: &mut Projection) -> Result<(), QueryError> {
        let Some(triples) = &self.retained_graph else {
            return Ok(());
        };
        for triple in triples.iter() {
            output.push(
                super::services::rdf_term(&triple.subject)?,
                super::services::rdf_term(&triple.predicate)?,
                super::services::rdf_term(&triple.object)?,
            )?;
        }
        Ok(())
    }

    fn symbols(&self, output: &mut Projection) -> Result<(), QueryError> {
        if output.relationship_predicate() {
            return Ok(());
        }
        for (symbol, subject) in self.facts.symbols.iter().zip(&self.providers.symbols) {
            output.symbol(&subject, symbol)?;
        }
        Ok(())
    }

    fn relationships(&self, output: &mut Projection) -> Result<(), QueryError> {
        for (relationship, record) in self
            .facts
            .relationships
            .iter()
            .map(|relationship| (relationship, None))
            .chain(
                self.facts
                    .relationship_records
                    .iter()
                    .map(|record| (&record.relationship, Some(&record.id))),
            )
        {
            let subject = iri(relationship.from.as_str())?;
            let target = iri(relationship.to.as_str())?;
            for facet in &relationship.facets {
                output.push(subject.clone(), vocabulary(facet.as_ref())?, target.clone())?;
            }
            if output.relationship_predicate() {
                continue;
            }
            let resource = match record {
                Some(identity) => blank(identity.as_str().as_bytes())?,
                None => blank(&serde_json::to_vec(relationship).map_err(invalid)?)?,
            };
            output.class(resource.clone(), "Relationship")?;
            if let Some(identity) = record {
                output.push(
                    resource.clone(),
                    vocabulary("id")?,
                    Literal::from(identity.as_str()).into(),
                )?;
            }
            output.fields(resource, relationship)?;
        }
        Ok(())
    }

    fn occurrences(&self, output: &mut Projection) -> Result<(), QueryError> {
        if output.relationship_predicate() {
            return Ok(());
        }
        let usages = vocabulary("usages")?;
        let resource = output
            .pattern
            .subject
            .as_ref()
            .filter(|subject| matches!(subject, Term::BlankNode(_)))
            .or_else(|| {
                (output.pattern.predicate.as_ref() == Some(&usages))
                    .then_some(output.pattern.object.as_ref())
                    .flatten()
                    .filter(|object| matches!(object, Term::BlankNode(_)))
            });
        let selected = resource.map(|resource| {
            self.occurrences
                .by_resource
                .get(resource)
                .map_or(&[][..], Vec::as_slice)
        });
        let all = if selected.is_none() {
            0..self.occurrences.resources.len()
        } else {
            0..0
        };
        for index in selected.unwrap_or(&[]).iter().copied().chain(all) {
            let occurrence = if index < self.facts.occurrences.len() {
                &self.facts.occurrences[index]
            } else {
                &self.facts.occurrence_records[index - self.facts.occurrences.len()].occurrence
            };
            if let Some(Term::NamedNode(subject)) = &output.pattern.subject
                && subject.as_str() != occurrence.target.as_str()
            {
                continue;
            }
            let resource = self.occurrences.resources[index].clone();
            output.push(
                iri(occurrence.target.as_str())?,
                vocabulary("usages")?,
                resource.clone(),
            )?;
            output.usage(
                resource,
                &occurrence.source,
                occurrence.facet,
                occurrence
                    .enclosing
                    .as_ref()
                    .map(|identity| iri(identity.as_str()))
                    .transpose()?,
            )?;
        }
        Ok(())
    }

    fn occurrence_evidence(&self, output: &mut Projection) -> Result<(), QueryError> {
        if output.relationship_predicate() {
            return Ok(());
        }
        for (index, record) in self.facts.occurrence_records.iter().enumerate() {
            let usage = self.occurrences.resources[self.facts.occurrences.len() + index].clone();
            let evidence = blank(record.id.as_str().as_bytes())?;
            output.push(usage, vocabulary("evidence")?, evidence.clone())?;
            output.push(
                evidence.clone(),
                vocabulary("id")?,
                Literal::from(record.id.as_str()).into(),
            )?;
            if let Some(relationship) = &record.relationship {
                output.push(
                    evidence,
                    vocabulary("relationship")?,
                    blank(relationship.as_str().as_bytes())?,
                )?;
            }
        }
        Ok(())
    }

    fn sources(&self, output: &mut Projection) -> Result<(), QueryError> {
        if output.relationship_predicate() {
            return Ok(());
        }
        for source in &self.facts.sources {
            let subject = iri(source.unit.as_str())?;
            output.class(subject.clone(), "SourceUnitId")?;
            let (_, path) =
                rift_protocol::identity::parse_source_unit_identity(source.unit.as_str())
                    .map_err(invalid)?;
            output.push(
                subject.clone(),
                vocabulary("path")?,
                Literal::from(path).into(),
            )?;
            if let Some(digest) = &source.digest {
                output.push(
                    subject.clone(),
                    vocabulary("digest")?,
                    Literal::from(digest.as_str()).into(),
                )?;
            }
            if let Some(text) = &source.text {
                output.push(
                    subject,
                    vocabulary("source")?,
                    Literal::from(text.as_str()).into(),
                )?;
            }
        }
        Ok(())
    }

    fn other_records(&self, output: &mut Projection) -> Result<(), QueryError> {
        if output.relationship_predicate() {
            return Ok(());
        }
        nodes::project(&self.facts, output)?;
        for history in &self.facts.histories {
            let resource = child(&iri(history.symbol.as_str())?, "history", 0)?;
            output.push(
                iri(history.symbol.as_str())?,
                vocabulary("history")?,
                resource.clone(),
            )?;
            output.class(resource.clone(), "SymbolHistory")?;
            output.fields(resource, history)?;
        }
        output.records("DocumentationSource", &self.facts.documentation_sources)?;
        output.records("DocumentationBlock", &self.facts.documentation_blocks)?;
        output.records("DocumentationLink", &self.facts.documentation_links)?;
        output.records(
            "DocumentationReference",
            &self.facts.documentation_references,
        )?;
        output.records(
            "DocumentationReferenceCandidate",
            &self.facts.documentation_reference_candidates,
        )
    }
}

#[derive(Clone)]
struct Pattern {
    subject: Option<Term>,
    predicate: Option<Term>,
    object: Option<Term>,
    graph_name: Option<GraphName>,
}

struct Projection {
    pattern: Pattern,
    graph: Term,
    limits: QueryLimits,
    bytes: usize,
    distinct: HashSet<(Option<Term>, Term, Term, Term)>,
    quads: Vec<InternalQuad<Term>>,
    cancellation: CancellationToken,
    identities: Arc<identity::IdentityMap>,
}

impl Projection {
    fn new(
        pattern: Pattern,
        limits: QueryLimits,
        cancellation: CancellationToken,
    ) -> Result<Self, QueryError> {
        Ok(Self {
            pattern,
            graph: iri(super::LOCAL_GRAPH)?,
            limits,
            bytes: 0,
            distinct: HashSet::new(),
            quads: Vec::new(),
            cancellation,
            identities: Arc::new(identity::IdentityMap::default()),
        })
    }

    fn relationship_predicate(&self) -> bool {
        let Some(Term::NamedNode(predicate)) = &self.pattern.predicate else {
            return false;
        };
        predicate
            .as_str()
            .strip_prefix(VOCABULARY)
            .is_some_and(|value| {
                serde_json::from_value::<rift_protocol::read::RelationshipFacet>(Value::String(
                    value.to_owned(),
                ))
                .is_ok()
            })
    }

    fn push(&mut self, subject: Term, predicate: Term, object: Term) -> Result<(), QueryError> {
        if self.cancellation.is_cancelled() {
            return Err(failure::cancelled());
        }
        if !self.matches(&subject, &predicate, &object) {
            return Ok(());
        }
        let subject = self.identities.public(subject)?;
        let predicate = self.identities.public(predicate)?;
        let object = self.identities.public(object)?;
        let graph_name = if self.pattern.graph_name == Some(GraphName::DefaultGraph) {
            None
        } else {
            Some(self.graph.clone())
        };
        let key = (
            graph_name.clone(),
            subject.clone(),
            predicate.clone(),
            object.clone(),
        );
        if self.distinct.contains(&key) {
            return Ok(());
        }
        if self.quads.len() >= self.limits.quads_max {
            return Err(failure::bound("quads_max"));
        }
        let bytes = term_bytes(&subject)
            .checked_add(term_bytes(&predicate))
            .and_then(|bytes| bytes.checked_add(term_bytes(&object)))
            .and_then(|bytes| bytes.checked_add(graph_name.as_ref().map_or(0, term_bytes)))
            .and_then(|bytes| self.bytes.checked_add(bytes))
            .ok_or_else(|| failure::bound("rdf_bytes_max"))?;
        if bytes > self.limits.rdf_bytes_max {
            return Err(failure::bound("rdf_bytes_max"));
        }
        self.bytes = bytes;
        self.distinct.insert(key);
        self.quads.push(InternalQuad {
            subject,
            predicate,
            object,
            graph_name,
        });
        Ok(())
    }

    fn matches(&self, subject: &Term, predicate: &Term, object: &Term) -> bool {
        self.graph_selected()
            && self
                .pattern
                .subject
                .as_ref()
                .is_none_or(|value| value == subject)
            && self
                .pattern
                .predicate
                .as_ref()
                .is_none_or(|value| value == predicate)
            && self
                .pattern
                .object
                .as_ref()
                .is_none_or(|value| value == object)
    }

    fn graph_selected(&self) -> bool {
        self.pattern
            .graph_name
            .as_ref()
            .is_none_or(|graph| match graph {
                GraphName::DefaultGraph => true,
                GraphName::NamedNode(selected) => {
                    matches!(&self.graph, Term::NamedNode(graph) if selected == graph)
                }
                GraphName::BlankNode(selected) => {
                    matches!(&self.graph, Term::BlankNode(graph) if selected == graph)
                }
            })
    }

    fn class(&mut self, subject: Term, name: &str) -> Result<(), QueryError> {
        self.push(subject, iri(RDF_TYPE)?, vocabulary(name)?)
    }

    fn usage(
        &mut self,
        resource: Term,
        source: &rift_protocol::read::SourceUnitSpan,
        facet: rift_protocol::read::RelationshipFacet,
        enclosing: Option<Term>,
    ) -> Result<(), QueryError> {
        self.class(resource.clone(), "SourceUnitSpan")?;
        self.push(
            resource.clone(),
            vocabulary("unit")?,
            iri(source.unit.as_str())?,
        )?;
        self.push(
            resource.clone(),
            vocabulary("start")?,
            Literal::from(source.range.start).into(),
        )?;
        self.push(
            resource.clone(),
            vocabulary("end")?,
            Literal::from(source.range.end).into(),
        )?;
        self.push(
            resource.clone(),
            vocabulary("facets")?,
            vocabulary(facet.as_ref())?,
        )?;
        let (_, path) = rift_protocol::identity::parse_source_unit_identity(source.unit.as_str())
            .map_err(invalid)?;
        self.push(
            resource.clone(),
            vocabulary("path")?,
            Literal::from(path).into(),
        )?;
        if let Some(enclosing) = enclosing {
            self.push(resource, vocabulary("enclosing")?, enclosing)?;
        }
        Ok(())
    }

    fn records<T: Serialize>(&mut self, class: &str, records: &[T]) -> Result<(), QueryError> {
        for record in records {
            let subject = blank(&serde_json::to_vec(&(class, record)).map_err(invalid)?)?;
            self.class(subject.clone(), class)?;
            self.fields(subject, record)?;
        }
        Ok(())
    }

    fn symbol(&mut self, subject: &Term, symbol: &QuerySymbol) -> Result<(), QueryError> {
        self.class(subject.clone(), "Symbol")?;
        self.push(
            subject.clone(),
            vocabulary("name")?,
            Literal::from(symbol.symbol.name.as_str()).into(),
        )?;
        self.fields(subject.clone(), &symbol.symbol)?;
        // The wire omits common origin and false locality; the RDF model states both facts.
        self.fields_at(subject.clone(), "origin", &symbol.symbol.origin)?;
        self.push(
            subject.clone(),
            vocabulary("document_local")?,
            Literal::from(symbol.symbol.document_local).into(),
        )?;
        for (index, declaration) in symbol.declarations.iter().enumerate() {
            let resource = child(subject, "declarations", index)?;
            self.push(
                subject.clone(),
                vocabulary("declarations")?,
                resource.clone(),
            )?;
            self.class(resource.clone(), "SymbolDeclaration")?;
            self.fields(resource.clone(), declaration)?;
            self.declaration_entries(subject, &resource, symbol, declaration)?;
            if let Some(path) = &declaration.path {
                self.push(
                    subject.clone(),
                    vocabulary("path")?,
                    Literal::from(path.0.as_str()).into(),
                )?;
            }
        }
        Ok(())
    }

    fn declaration_entries(
        &mut self,
        symbol: &Term,
        declaration: &Term,
        facts: &QuerySymbol,
        binding: &rift_protocol::symbol_read::SymbolDeclaration,
    ) -> Result<(), QueryError> {
        for (key, indices, count) in [
            (
                "signatures",
                &binding.signature_indices,
                facts.symbol.signatures.len(),
            ),
            ("types", &binding.type_indices, facts.symbol.types.len()),
            (
                "documentation",
                &binding.documentation_indices,
                facts.symbol.documentation.len(),
            ),
        ] {
            for index in indices {
                let index = usize::try_from(*index).map_err(invalid)?;
                if index >= count {
                    return Err(failure::invalid(format!(
                        "declaration {key} index {index} is outside {count} entries"
                    )));
                }
                self.push(
                    declaration.clone(),
                    vocabulary(key)?,
                    child(symbol, key, index)?,
                )?;
            }
        }
        Ok(())
    }

    fn fields<T: Serialize>(&mut self, subject: Term, value: &T) -> Result<(), QueryError> {
        let value = serde_json::to_value(value).map_err(invalid)?;
        self.project_value(subject, value)
    }

    fn fields_at<T: Serialize>(
        &mut self,
        subject: Term,
        key: &str,
        value: &T,
    ) -> Result<(), QueryError> {
        let value = serde_json::to_value(value).map_err(invalid)?;
        self.project_value(subject, serde_json::json!({key: value}))
    }

    fn project_value(&mut self, subject: Term, value: Value) -> Result<(), QueryError> {
        let mut pending = vec![(subject, value)];
        let mut visited = 0usize;
        while let Some((subject, value)) = pending.pop() {
            visited = visited
                .checked_add(1)
                .ok_or_else(|| failure::bound("quads_max"))?;
            if visited > self.limits.quads_max || pending.len() > self.limits.quads_max {
                return Err(failure::bound("quads_max"));
            }
            let Value::Object(fields) = value else {
                continue;
            };
            for (key, value) in fields {
                self.field(&subject, &key, value, &mut pending)?;
            }
        }
        Ok(())
    }

    fn field(
        &mut self,
        subject: &Term,
        key: &str,
        value: Value,
        pending: &mut Vec<(Term, Value)>,
    ) -> Result<(), QueryError> {
        let predicate = vocabulary(key)?;
        match value {
            Value::Null => {}
            Value::Array(values) => {
                for (index, value) in values.into_iter().enumerate() {
                    self.array_entry(subject, predicate.clone(), key, index, value, pending)?;
                }
            }
            value => self.value(
                subject,
                predicate,
                key,
                value,
                child(subject, key, 0)?,
                pending,
            )?,
        }
        Ok(())
    }

    fn array_entry(
        &mut self,
        subject: &Term,
        predicate: Term,
        key: &str,
        index: usize,
        value: Value,
        pending: &mut Vec<(Term, Value)>,
    ) -> Result<(), QueryError> {
        let resource = child(subject, key, index)?;
        let is_record = value.is_object() || value.is_array();
        let is_set = ["facets", "evidence", "modifiers"].contains(&key);
        if is_record || is_set {
            self.value(subject, predicate, key, value, resource.clone(), pending)?;
        } else {
            self.push(subject.clone(), predicate, resource.clone())?;
            self.value(
                &resource,
                vocabulary("value")?,
                key,
                value,
                child(&resource, "value", 0)?,
                pending,
            )?;
        }
        if !is_set {
            self.push(
                resource.clone(),
                vocabulary("index")?,
                Literal::from(u64::try_from(index).map_err(invalid)?).into(),
            )?;
        }
        if let Some(class) = record_class(key) {
            self.class(resource, class)?;
        }
        Ok(())
    }

    fn value(
        &mut self,
        subject: &Term,
        predicate: Term,
        key: &str,
        value: Value,
        resource: Term,
        pending: &mut Vec<(Term, Value)>,
    ) -> Result<(), QueryError> {
        let term = match value {
            Value::Null => return Ok(()),
            Value::Object(_) if key == "extensions" => Literal::new_typed_literal(
                serde_json::to_string(&value).map_err(invalid)?,
                NamedNode::new("http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON")
                    .map_err(invalid)?,
            )
            .into(),
            Value::Object(_) | Value::Array(_) => {
                if let Some(class) = record_class(key) {
                    self.class(resource.clone(), class)?;
                }
                pending.push((resource.clone(), value));
                resource
            }
            Value::String(value) if key == "facets" || key == "derivation" => vocabulary(&value)?,
            Value::String(value)
                if value.starts_with("rift://")
                    && [
                        "id",
                        "unit",
                        "from",
                        "to",
                        "target",
                        "symbol",
                        "container",
                        "node",
                        "parent",
                        "evidence",
                    ]
                    .contains(&key) =>
            {
                iri(&value)?
            }
            Value::String(value) => Literal::from(value).into(),
            Value::Bool(value) => Literal::from(value).into(),
            Value::Number(value) => number(&value)?,
        };
        self.push(subject.clone(), predicate, term)
    }
}

impl<'a> QueryableDataset<'a> for VirtualDataset {
    type InternalTerm = Term;
    type Error = QueryError;

    fn internal_named_graphs(&self) -> impl Iterator<Item = Result<Term, QueryError>> + use<'a> {
        if self.cancellation.is_cancelled() {
            return vec![Err(failure::cancelled())].into_iter();
        }
        std::iter::once(iri(super::LOCAL_GRAPH))
            .chain(self.datasets.iter().map(|(graph, _)| Ok(graph.clone())))
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn contains_internal_graph_name(&self, graph: &Term) -> Result<bool, QueryError> {
        if self.cancellation.is_cancelled() {
            return Err(failure::cancelled());
        }
        Ok(
            matches!(graph, Term::NamedNode(value) if value.as_str() == super::LOCAL_GRAPH)
                || self.datasets.iter().any(|(name, _)| name == graph),
        )
    }

    fn internal_quads_for_pattern(
        &self,
        subject: Option<&Term>,
        predicate: Option<&Term>,
        object: Option<&Term>,
        graph_name: Option<Option<&Term>>,
    ) -> impl Iterator<Item = Result<InternalQuad<Term>, QueryError>> + use<'a> {
        let graph_name = match graph_name {
            None => None,
            Some(None) => Some(GraphName::DefaultGraph),
            Some(Some(term)) => {
                let Ok(graph) = NamedOrBlankNode::try_from(term.clone()) else {
                    return Vec::new().into_iter();
                };
                Some(GraphName::from(graph))
            }
        };
        let pattern = Pattern {
            subject: subject.cloned(),
            predicate: predicate.cloned(),
            object: object.cloned(),
            graph_name,
        };
        match self.project(pattern) {
            Ok(quads) => quads.into_iter().map(Ok).collect::<Vec<_>>().into_iter(),
            Err(error) => vec![Err(error)].into_iter(),
        }
    }

    fn internalize_term(&self, term: Term) -> Result<Term, QueryError> {
        Ok(term)
    }
    fn externalize_term(&self, term: Term) -> Result<Term, QueryError> {
        Ok(term)
    }
}

fn fact_count(facts: &QueryFacts) -> Result<usize, QueryError> {
    let nodes = nodes::count(facts)?;
    facts
        .symbols
        .len()
        .checked_add(facts.relationships.len())
        .and_then(|count| count.checked_add(facts.relationship_records.len()))
        .and_then(|count| count.checked_add(facts.occurrences.len()))
        .and_then(|count| count.checked_add(facts.occurrence_records.len()))
        .and_then(|count| count.checked_add(facts.sources.len()))
        .and_then(|count| count.checked_add(nodes))
        .and_then(|count| count.checked_add(facts.histories.len()))
        .and_then(|count| count.checked_add(facts.documentation_sources.len()))
        .and_then(|count| count.checked_add(facts.documentation_blocks.len()))
        .and_then(|count| count.checked_add(facts.documentation_links.len()))
        .and_then(|count| count.checked_add(facts.documentation_references.len()))
        .and_then(|count| count.checked_add(facts.documentation_reference_candidates.len()))
        .ok_or_else(|| failure::bound("quads_max"))
}

fn iri(value: &str) -> Result<Term, QueryError> {
    NamedNode::new(value).map(Term::from).map_err(invalid)
}

fn vocabulary(value: &str) -> Result<Term, QueryError> {
    iri(&format!("{VOCABULARY}{value}"))
}

fn blank(value: &[u8]) -> Result<Term, QueryError> {
    BlankNode::new(data_encoding::HEXLOWER.encode(&Sha256::digest(value)))
        .map(Term::from)
        .map_err(invalid)
}

fn child(subject: &Term, key: &str, index: usize) -> Result<Term, QueryError> {
    blank(&serde_json::to_vec(&(subject.to_string(), key, index)).map_err(invalid)?)
}

fn number(value: &serde_json::Number) -> Result<Term, QueryError> {
    if let Some(value) = value.as_u64() {
        return Ok(Literal::from(value).into());
    }
    if let Some(value) = value.as_i64() {
        return Ok(Literal::from(value).into());
    }
    value
        .as_f64()
        .map(Literal::from)
        .map(Term::from)
        .ok_or_else(|| failure::invalid(value))
}

fn term_bytes(term: &Term) -> usize {
    match term {
        Term::NamedNode(value) => value.as_str().len(),
        Term::BlankNode(value) => value.as_str().len(),
        Term::Literal(value) => value
            .value()
            .len()
            .saturating_add(value.datatype().as_str().len())
            .saturating_add(value.language().map_or(0, str::len)),
    }
}

fn invalid(error: impl std::fmt::Display) -> QueryError {
    failure::invalid(error)
}

fn record_class(key: &str) -> Option<&'static str> {
    match key {
        "signatures" => Some("Signature"),
        "parameters" | "receiver" => Some("Parameter"),
        "types" | "returns" => Some("TypeBinding"),
        "documentation" => Some("Documentation"),
        "versions" => Some("SymbolVersion"),
        "links" => Some("SignatureLink"),
        "expression" | "throws" => Some("TypeExpression"),
        "regions" => Some("NodeRegion"),
        "range" | "physical_ranges" => Some("TextRange"),
        "occurrence" => Some("SourceUnitSpan"),
        "origin" => Some("SymbolOrigin"),
        "package" => Some("PackageIdentity"),
        "runtime" => Some("RuntimeIdentity"),
        "author" => Some("CommitAuthor"),
        "heading_path" => Some("DocumentationHeading"),
        "chunks" => Some("DocumentationChunk"),
        "license" => Some("DocumentationLicense"),
        _ => None,
    }
}

#[cfg(test)]
#[path = "records.rs"]
mod records;
