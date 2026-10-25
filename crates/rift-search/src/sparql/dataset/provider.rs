//! RDF resources for captured normalized provider records.

use super::*;
use crate::query::{QueryProviderEdge, QueryProviderEndpoint};
use rift_provider::NormalizedTarget;

pub(super) struct ProviderIndex {
    pub(super) symbols: Vec<Term>,
    targets: HashMap<Vec<u8>, Term>,
    edges: Vec<IndexedEdge>,
    by_subject: HashMap<Term, Vec<usize>>,
}

#[derive(Clone, Copy)]
enum EdgeSource {
    Acquired(usize),
    Reference {
        provider: usize,
        reference: usize,
        target: usize,
    },
    Relationship {
        provider: usize,
        relationship: usize,
    },
}

struct IndexedEdge {
    source: EdgeSource,
    from: Option<Term>,
    to: Term,
    relationship: Term,
    usage: Option<Term>,
}

impl ProviderIndex {
    pub(super) fn new(
        facts: &QueryFacts,
        limits: QueryLimits,
        cancellation: &CancellationToken,
    ) -> Result<Self, QueryError> {
        facts.validate_providers()?;
        let mut result = Self {
            symbols: Vec::new(),
            targets: HashMap::new(),
            edges: Vec::new(),
            by_subject: HashMap::new(),
        };
        let mut bytes = 0usize;
        for symbol in &facts.symbols {
            if cancellation.is_cancelled() {
                return Err(failure::cancelled());
            }
            let resource = match &symbol.symbol.id {
                Some(identity) => iri(identity.as_str())?,
                None => blank(&serde_json::to_vec(symbol).map_err(invalid)?)?,
            };
            bytes = bytes
                .checked_add(term_bytes(&resource))
                .ok_or_else(|| failure::bound("rdf_bytes_max"))?;
            if bytes > limits.rdf_bytes_max {
                return Err(failure::bound("rdf_bytes_max"));
            }
            result.symbols.push(resource);
        }
        for (provider, snapshot) in facts.providers.iter().enumerate() {
            for record in &snapshot.records {
                if cancellation.is_cancelled() {
                    return Err(failure::cancelled());
                }
                let symbol = facts
                    .symbols
                    .get(record.symbol_index)
                    .ok_or_else(|| failure::invalid("captured provider symbol is absent"))?;
                let resource = match &symbol.symbol.id {
                    Some(identity) => iri(identity.as_str())?,
                    None => blank(&record.key(&snapshot.context).map_err(invalid)?)?,
                };
                result.symbols[record.symbol_index] = resource.clone();
                let targets = record
                    .identity
                    .as_ref()
                    .map(|identity| NormalizedTarget::Symbol(identity.clone()))
                    .into_iter()
                    .chain(
                        record
                            .contributions
                            .iter()
                            .map(|key| NormalizedTarget::Contribution(key.reference().clone())),
                    );
                for target in targets {
                    let key = serde_json::to_vec(&QueryProviderEndpoint {
                        provider: Some(provider),
                        target,
                    })
                    .map_err(invalid)?;
                    bytes = bytes
                        .checked_add(key.len())
                        .and_then(|bytes| bytes.checked_add(term_bytes(&resource)))
                        .ok_or_else(|| failure::bound("rdf_bytes_max"))?;
                    if bytes > limits.rdf_bytes_max || result.targets.len() >= limits.quads_max {
                        return Err(failure::bound("rdf_bytes_max"));
                    }
                    if result.targets.insert(key, resource.clone()).is_some() {
                        return Err(failure::invalid(
                            "captured provider target repeats within its original source",
                        ));
                    }
                }
            }
        }
        result.build_edges(facts, limits, cancellation, &mut bytes)?;
        Ok(result)
    }

    fn resource(
        &mut self,
        facts: &QueryFacts,
        target: &QueryProviderEndpoint,
        limits: QueryLimits,
        bytes: &mut usize,
    ) -> Result<Term, QueryError> {
        if let Some(identity) = target.canonical_symbol() {
            return iri(identity.as_str());
        }
        let key = serde_json::to_vec(target).map_err(invalid)?;
        match self.targets.get(&key) {
            Some(resource) => Ok(resource.clone()),
            None => {
                let provider = target.provider.ok_or_else(|| {
                    failure::invalid("unscoped provider target has no canonical identity")
                })?;
                let snapshot = facts
                    .providers
                    .get(provider)
                    .ok_or_else(|| failure::invalid("captured provider target is absent"))?;
                let resource = blank(&snapshot.context.target_key(&target.target)?)?;
                *bytes = bytes
                    .checked_add(key.len())
                    .and_then(|bytes| bytes.checked_add(term_bytes(&resource)))
                    .ok_or_else(|| failure::bound("rdf_bytes_max"))?;
                if *bytes > limits.rdf_bytes_max {
                    return Err(failure::bound("rdf_bytes_max"));
                }
                self.targets.insert(key, resource.clone());
                Ok(resource)
            }
        }
    }

    fn build_edges(
        &mut self,
        facts: &QueryFacts,
        limits: QueryLimits,
        cancellation: &CancellationToken,
        bytes: &mut usize,
    ) -> Result<(), QueryError> {
        for index in 0..facts.provider_edges.len() {
            self.index_edge(
                facts,
                EdgeSource::Acquired(index),
                limits,
                cancellation,
                bytes,
            )?;
        }
        for (provider, snapshot) in facts.providers.iter().enumerate() {
            for (reference, item) in snapshot.references.iter().enumerate() {
                for target in 0..item.targets.len() {
                    self.index_edge(
                        facts,
                        EdgeSource::Reference {
                            provider,
                            reference,
                            target,
                        },
                        limits,
                        cancellation,
                        bytes,
                    )?;
                }
            }
            for relationship in 0..snapshot.relationships.len() {
                self.index_edge(
                    facts,
                    EdgeSource::Relationship {
                        provider,
                        relationship,
                    },
                    limits,
                    cancellation,
                    bytes,
                )?;
            }
        }
        Ok(())
    }

    fn index_edge(
        &mut self,
        facts: &QueryFacts,
        source: EdgeSource,
        limits: QueryLimits,
        cancellation: &CancellationToken,
        bytes: &mut usize,
    ) -> Result<(), QueryError> {
        if cancellation.is_cancelled() {
            return Err(failure::cancelled());
        }
        if self.edges.len() >= limits.quads_max {
            return Err(failure::bound("quads_max"));
        }
        let edge = source.edge(facts);
        let from = edge
            .from
            .as_ref()
            .map(|target| self.resource(facts, target, limits, bytes))
            .transpose()?;
        let to = self.resource(facts, &edge.to, limits, bytes)?;
        let relationship = blank(
            &serde_json::to_vec(&(
                "Relationship",
                from.as_ref().map(resource_key),
                resource_key(&to),
                edge.facet,
                edge.derivation,
                &edge.occurrence,
            ))
            .map_err(invalid)?,
        )?;
        let usage = edge
            .occurrence
            .as_ref()
            .map(|occurrence| {
                // Same physical key as the existing canonical occurrence owner; facet is a fact.
                blank(
                    &serde_json::to_vec(&(
                        resource_key(&to),
                        from.as_ref().map(resource_key),
                        occurrence,
                    ))
                    .map_err(invalid)?,
                )
            })
            .transpose()?;
        let index = self.edges.len();
        let mut subjects = vec![to.clone(), relationship.clone()];
        subjects.extend(from.iter().cloned());
        subjects.extend(usage.iter().cloned());
        let mut distinct = HashSet::new();
        subjects.retain(|subject| distinct.insert(subject.clone()));
        let material = subjects
            .iter()
            .try_fold(std::mem::size_of::<IndexedEdge>(), |bytes, term| {
                bytes
                    .checked_add(term_bytes(term))
                    .and_then(|bytes| bytes.checked_add(std::mem::size_of::<usize>()))
            })
            .ok_or_else(|| failure::bound("rdf_bytes_max"))?;
        *bytes = bytes
            .checked_add(material)
            .ok_or_else(|| failure::bound("rdf_bytes_max"))?;
        if *bytes > limits.rdf_bytes_max {
            return Err(failure::bound("rdf_bytes_max"));
        }
        for subject in subjects {
            self.by_subject.entry(subject).or_default().push(index);
        }
        self.edges.push(IndexedEdge {
            source,
            from,
            to,
            relationship,
            usage,
        });
        Ok(())
    }

    pub(super) fn project(
        &self,
        facts: &QueryFacts,
        output: &mut Projection,
    ) -> Result<(), QueryError> {
        let usages = vocabulary("usages")?;
        let selected = output
            .pattern
            .subject
            .as_ref()
            .map(|subject| self.by_subject.get(subject).map_or(&[][..], Vec::as_slice))
            .or_else(|| {
                (output.pattern.predicate.as_ref() == Some(&usages))
                    .then_some(output.pattern.object.as_ref())
                    .flatten()
                    .map(|usage| self.by_subject.get(usage).map_or(&[][..], Vec::as_slice))
            });
        let all = if selected.is_none() {
            0..self.edges.len()
        } else {
            0..0
        };
        for index in selected.unwrap_or(&[]).iter().copied().chain(all) {
            let indexed = &self.edges[index];
            let edge = indexed.source.edge(facts);
            if let Some(from) = &indexed.from {
                output.push(
                    from.clone(),
                    vocabulary(edge.facet.as_ref())?,
                    indexed.to.clone(),
                )?;
            }
            if output.relationship_predicate() {
                continue;
            }
            let resource = indexed.relationship.clone();
            output.class(resource.clone(), "Relationship")?;
            if let Some(from) = &indexed.from {
                output.push(resource.clone(), vocabulary("from")?, from.clone())?;
            }
            output.push(resource.clone(), vocabulary("to")?, indexed.to.clone())?;
            output.push(
                resource.clone(),
                vocabulary("facets")?,
                vocabulary(edge.facet.as_ref())?,
            )?;
            if let Some(derivation) = edge.derivation {
                output.fields_at(resource.clone(), "derivation", &derivation)?;
            }
            if let (Some(usage), Some(occurrence)) = (&indexed.usage, &edge.occurrence) {
                output.push(resource, vocabulary("occurrence")?, usage.clone())?;
                output.push(indexed.to.clone(), vocabulary("usages")?, usage.clone())?;
                output.usage(usage.clone(), occurrence, edge.facet, indexed.from.clone())?;
            }
        }
        Ok(())
    }
}

impl EdgeSource {
    fn edge(self, facts: &QueryFacts) -> QueryProviderEdge {
        match self {
            Self::Acquired(index) => facts.provider_edges[index].clone(),
            Self::Reference {
                provider,
                reference,
                target,
            } => {
                let reference = &facts.providers[provider].references[reference];
                QueryProviderEdge {
                    from: Some(QueryProviderEndpoint {
                        provider: Some(provider),
                        target: NormalizedTarget::Contribution(
                            reference.source.reference().clone(),
                        ),
                    }),
                    to: QueryProviderEndpoint {
                        provider: Some(provider),
                        target: reference.targets[target].clone(),
                    },
                    facet: reference.facet,
                    occurrence: Some(span(&reference.binding)),
                    derivation: None,
                }
            }
            Self::Relationship {
                provider,
                relationship,
            } => {
                let relationship = &facts.providers[provider].relationships[relationship];
                QueryProviderEdge {
                    from: Some(QueryProviderEndpoint {
                        provider: Some(provider),
                        target: NormalizedTarget::Contribution(
                            relationship.source.reference().clone(),
                        ),
                    }),
                    to: QueryProviderEndpoint {
                        provider: Some(provider),
                        target: relationship.target.clone(),
                    },
                    facet: relationship.facet,
                    occurrence: relationship.occurrence.as_ref().map(span),
                    derivation: relationship.derivation,
                }
            }
        }
    }
}

fn span(binding: &rift_core::DeclarationBinding) -> rift_protocol::read::SourceUnitSpan {
    rift_protocol::read::SourceUnitSpan {
        unit: rift_protocol::read::SourceUnitId(binding.unit().to_string()),
        range: rift_protocol::read::TextRange {
            start: binding.range().start(),
            end: binding.range().end(),
        },
    }
}

fn resource_key(resource: &Term) -> &str {
    match resource {
        Term::NamedNode(node) => node.as_str(),
        Term::BlankNode(node) => node.as_str(),
        Term::Literal(value) => value.value(),
    }
}
