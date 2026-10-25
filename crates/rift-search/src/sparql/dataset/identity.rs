//! Exact public RDF identities backed by one original selected owner.

use std::collections::HashMap;

use oxigraph::model::{BlankNode, Term};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner, parse_source_unit_identity};

use super::{Pattern, QueryError, iri};
use crate::sparql::{QueryIdentities, failure};

#[derive(Clone, Default)]
pub(super) struct IdentityMap {
    forward: HashMap<Term, Term>,
    backward: HashMap<Term, Term>,
    blank_prefix: String,
}

impl IdentityMap {
    pub(super) fn source_unit(
        &self,
        source: &rift_protocol::query::QuerySource,
    ) -> Result<rift_protocol::read::SourceUnitId, QueryError> {
        let Term::NamedNode(unit) = self.public(iri(source.unit.as_str())?)? else {
            return Err(failure::invalid("source identity must be an IRI"));
        };
        rift_protocol::read::SourceUnitId::parse(unit.as_str())
    }

    pub(super) fn new(identities: QueryIdentities) -> Result<Self, QueryError> {
        let mut result = Self {
            blank_prefix: format!("{}_", identities.blank_prefix.as_str()),
            ..Self::default()
        };
        for (original, public) in identities.symbols {
            let owner = SymbolIdentity::parse(original.as_str())?.owner().clone();
            SymbolIdentity::parse(public.as_str())?;
            if !matches!(owner, SymbolOwner::Local) && original != public {
                return Err(failure::invalid(
                    "canonical symbol ownership must not change",
                ));
            }
            result.insert(iri(original.as_str())?, iri(public.as_str())?)?;
        }
        for (original, public) in identities.sources {
            let (owner, _) = parse_source_unit_identity(original.as_str())?;
            parse_source_unit_identity(public.as_str())?;
            if owner.is_some() && original != public {
                return Err(failure::invalid(
                    "canonical source ownership must not change",
                ));
            }
            result.insert(iri(original.as_str())?, iri(public.as_str())?)?;
        }
        for (original, public) in &result.forward {
            if original != public && result.forward.contains_key(public) {
                return Err(failure::invalid("advertised query identities collide"));
            }
        }
        Ok(result)
    }

    fn insert(&mut self, original: Term, public: Term) -> Result<(), QueryError> {
        if self
            .forward
            .get(&original)
            .is_some_and(|value| value != &public)
            || self
                .backward
                .get(&public)
                .is_some_and(|value| value != &original)
        {
            return Err(failure::invalid("advertised query identities collide"));
        }
        self.backward.insert(public.clone(), original.clone());
        self.forward.insert(original, public);
        Ok(())
    }

    pub(super) fn lookup_pattern(&self, pattern: &Pattern) -> Option<Pattern> {
        let lookup = |value: &Option<Term>| match value {
            Some(value) => self.original(value).map(Some),
            None => Some(None),
        };
        Some(Pattern {
            subject: lookup(&pattern.subject)?,
            predicate: lookup(&pattern.predicate)?,
            object: lookup(&pattern.object)?,
            graph_name: pattern.graph_name.clone(),
        })
    }

    fn original(&self, term: &Term) -> Option<Term> {
        match term {
            Term::BlankNode(value) if !self.blank_prefix.is_empty() => {
                let label = value.as_str().strip_prefix(&self.blank_prefix)?;
                BlankNode::new(label).ok().map(Term::from)
            }
            _ => self
                .backward
                .get(term)
                .cloned()
                .or_else(|| (!self.forward.contains_key(term)).then(|| term.clone())),
        }
    }

    pub(super) fn public(&self, term: Term) -> Result<Term, QueryError> {
        if let Some(public) = self.forward.get(&term) {
            return Ok(public.clone());
        }
        if let Term::BlankNode(value) = &term
            && !self.blank_prefix.is_empty()
        {
            return BlankNode::new(format!("{}{}", self.blank_prefix, value.as_str()))
                .map(Term::from)
                .map_err(failure::invalid);
        }
        Ok(term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sparql::{
        PreparedQuery, QueryAnswer, QueryCancellation, QueryDataset, QueryFacts, QueryLimits,
        QueryServices,
    };
    use rift_protocol::identity::SourceDigest;
    use rift_protocol::query::QuerySource;
    use rift_protocol::read::SourceUnitId;

    fn limits() -> QueryLimits {
        QueryLimits {
            query_bytes_max: 16_384,
            algebra_nodes_max: 4096,
            quads_max: 10_000,
            rdf_bytes_max: 1 << 20,
            result_items_max: 100,
            result_bytes_max: 1 << 20,
            input_bytes_max: None,
            output_bytes_max: None,
            material_bytes_max: None,
            heap_bytes_max: None,
            diagnostic_bytes_max: None,
        }
    }

    fn units() -> Result<(SourceUnitId, SourceUnitId), QueryError> {
        let original = SourceUnitId::parse("rift://source/project/src/lib.rs")?;
        let public = rift_core::SourceUnitId::new(
            rift_core::SourceResolverId::new("rift.sources.project")?,
            rift_core::SourcePath::new("profile/src/lib.rs")?,
        )?;
        Ok((original, SourceUnitId::parse(&public.to_string())?))
    }

    fn facts(text: &str) -> Result<QueryFacts, QueryError> {
        Ok(QueryFacts {
            sources: vec![QuerySource {
                unit: units()?.0,
                digest: None,
                text: Some(text.to_owned()),
            }],
            ..QueryFacts::default()
        })
    }

    fn services() -> Result<QueryServices, QueryError> {
        let (original, public) = units()?;
        Ok(QueryServices {
            datasets: vec![QueryDataset {
                graph: format!("{}docs", crate::sparql::EXTRA_PROJECTS),
                facts: facts("profile bytes")?,
                identities: QueryIdentities {
                    symbols: Vec::new(),
                    sources: vec![(original, public)],
                    blank_prefix: SourceDigest::parse(&"a".repeat(64))?,
                },
                default: true,
            }],
            ..QueryServices::default()
        })
    }

    fn evaluate(statement: &str) -> Result<QueryAnswer, QueryError> {
        PreparedQuery::parse(statement, limits())?.evaluate_with_services(
            facts("primary bytes")?,
            limits(),
            QueryCancellation::default(),
            services()?,
        )
    }

    #[test]
    fn owner_graphs_preserve_absolute_source_constants_and_library_datasets()
    -> Result<(), QueryError> {
        let (primary, profile) = units()?;
        let (primary, profile) = (primary.as_str(), profile.as_str());
        for (statement, expected) in [
            (
                format!("ASK {{ <{primary}> :source 'primary bytes' }}"),
                true,
            ),
            (
                format!("ASK {{ GRAPH extra:docs {{ <{primary}> :source ?text }} }}"),
                false,
            ),
            (
                format!("ASK {{ GRAPH extra:docs {{ <{profile}> :source 'profile bytes' }} }}"),
                true,
            ),
            (
                format!("ASK FROM extra:docs {{ <{primary}> :source ?text }}"),
                false,
            ),
            (
                format!("ASK FROM extra:docs {{ <{profile}> :source 'profile bytes' }}"),
                true,
            ),
            (
                format!(
                    "ASK FROM NAMED extra:docs {{ GRAPH extra:docs {{ <{profile}> :source 'profile bytes' }} }}"
                ),
                true,
            ),
            (
                format!(
                    "ASK {{ GRAPH <{}> {{ ?unit :source ?a }} GRAPH extra:docs {{ ?unit :source ?b }} }}",
                    crate::sparql::LOCAL_GRAPH
                ),
                false,
            ),
            (
                format!(
                    "ASK {{ BIND(<{primary}> AS ?a) BIND(<{profile}> AS ?b) FILTER(sameTerm(?a, ?b)) }}"
                ),
                false,
            ),
        ] {
            assert_eq!(
                evaluate(&statement)?,
                QueryAnswer::Boolean { value: expected },
                "{statement}"
            );
        }
        let QueryAnswer::Bindings { bindings, .. } = evaluate(
            "SELECT ?a ?b { GRAPH <urn:rift:graph:local> { ?a :source ?x } GRAPH extra:docs { ?b :source ?y } }",
        )?
        else {
            unreachable!()
        };
        assert_eq!(bindings.len(), 1);
        assert_ne!(bindings[0]["a"], bindings[0]["b"]);
        let QueryAnswer::Bindings { bindings, .. } =
            evaluate("SELECT (COUNT(?unit) AS ?count) { ?unit :source ?text }")?
        else {
            unreachable!()
        };
        assert_eq!(
            bindings[0]["count"],
            rift_protocol::query::RdfTerm::Literal {
                value: "2".to_owned(),
                datatype: "http://www.w3.org/2001/XMLSchema#integer".to_owned()
            }
        );
        Ok(())
    }

    #[test]
    fn owner_dictionary_keeps_bound_blank_nodes_and_rejects_collisions() -> Result<(), QueryError> {
        let (original, public) = units()?;
        let prefix = SourceDigest::parse(&"b".repeat(64))?;
        let map = IdentityMap::new(QueryIdentities {
            symbols: Vec::new(),
            sources: vec![(original.clone(), public.clone())],
            blank_prefix: prefix.clone(),
        })?;
        let node = Term::BlankNode(BlankNode::new("record").map_err(failure::invalid)?);
        let advertised = map.public(node.clone())?;
        assert_eq!(map.original(&advertised), Some(node));
        assert!(
            map.original(&Term::BlankNode(
                BlankNode::new("foreign").map_err(failure::invalid)?
            ))
            .is_none()
        );
        assert!(map.original(&iri(original.as_str())?).is_none());
        assert_eq!(
            map.original(&iri(public.as_str())?),
            Some(iri(original.as_str())?)
        );
        assert!(
            IdentityMap::new(QueryIdentities {
                symbols: Vec::new(),
                sources: vec![(original.clone(), public.clone()), (public, original)],
                blank_prefix: prefix
            })
            .is_err()
        );
        Ok(())
    }
}
