//! Optional producer facts in graph answers.

use rift_error::RiftError;
use rift_protocol::query::{QueryAnswer, QueryInclude, QuerySection, RdfTerm};
use rift_protocol::read::EdgeProvider;

impl super::GraphDataset {
    /// Attaches stored producer sets to answer terms for retention before projection.
    ///
    /// # Errors
    /// Returns a query evaluation failure for an invalid retained blank node identity.
    pub fn annotate_providers(&self, answer: &mut QueryAnswer) -> Result<(), RiftError> {
        let mut captured = std::collections::BTreeMap::new();
        for term in terms_mut(answer) {
            let RdfTerm::BlankNode { value, providers } = term else {
                continue;
            };
            let selected = match captured.entry(value.clone()) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(self.stored_providers(value)?)
                }
            };
            providers.clone_from(selected);
        }
        Ok(())
    }

    fn stored_providers(
        &self,
        value: &str,
    ) -> Result<std::collections::BTreeSet<EdgeProvider>, RiftError> {
        let subject = oxrdf::BlankNode::new(value.to_owned()).map_err(|_| projection_failure())?;
        let property = super::VocabularyProperty::Provider.iri();
        let mut providers = std::collections::BTreeSet::new();
        for quad in self.triples().quads_for_subject(&subject) {
            if quad.predicate != property {
                continue;
            }
            if let oxrdf::Term::Literal(value) = quad.object {
                providers.extend(
                    [
                        EdgeProvider::Engine,
                        EdgeProvider::Lsp,
                        EdgeProvider::Static,
                    ]
                    .into_iter()
                    .filter(|provider| provider.as_ref() == value.value()),
                );
            }
        }
        Ok(providers)
    }
}

/// Optional producer projection for an evaluated query answer.
pub trait QueryAnswerProjection {
    /// Projects requested producers on the selected answer page. The caller counts
    /// graph rows after omitting stored `:provider` triples.
    #[must_use]
    fn project(self, include: QueryInclude) -> Self;
}

impl QueryAnswerProjection for QueryAnswer {
    fn project(mut self, include: QueryInclude) -> Self {
        if let QueryAnswer::Graph { triples } = &mut self {
            triples.retain(|triple| !is_provider(&triple.predicate));
        }
        if !include.contains(&QuerySection::Providers) {
            for term in terms_mut(&mut self) {
                if let RdfTerm::BlankNode { providers, .. } = term {
                    providers.clear();
                }
            }
        }
        self
    }
}

fn terms_mut(answer: &mut QueryAnswer) -> Box<dyn Iterator<Item = &mut RdfTerm> + '_> {
    match answer {
        QueryAnswer::Bindings { bindings, .. } => Box::new(
            bindings
                .iter_mut()
                .flat_map(std::collections::BTreeMap::values_mut),
        ),
        QueryAnswer::Graph { triples } => Box::new(
            triples
                .iter_mut()
                .flat_map(|triple| [&mut triple.subject, &mut triple.object]),
        ),
        QueryAnswer::Boolean { .. } => Box::new(std::iter::empty()),
    }
}

fn is_provider(predicate: &RdfTerm) -> bool {
    matches!(predicate, RdfTerm::Iri { value } if value == "urn:rift:vocabulary:provider")
}

fn projection_failure() -> RiftError {
    rift_error::errors::search::query_evaluation_failed()
        .detail("could not project edge providers")
        .error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sparql::{GraphDataset, GraphLimits, GraphQuery, QueryCancellation};
    use rift_protocol::index::StoredEdge;
    use rift_protocol::read::{Edge, EdgeProvider};
    use serde_json::json;

    #[test]
    fn merged_edge_rows_keep_explicit_provider_filters_and_optional_graph_sets()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = GraphDataset::builder(GraphLimits::default());
        let fixture = json!({
            "from": "rift://symbol/local/rust/app/caller", "to": "rift://symbol/local/rust/app/callee",
            "kind": "calls", "kinds": ["calls"], "provider": "static", "derivation": "resolution",
            "occurrence": {"unit": "rift://source/project/src/lib.rs", "range": {"start": 10, "end": 16}}
        });
        let row: StoredEdge = serde_json::from_value(fixture)?;
        for provider in [
            EdgeProvider::Static,
            EdgeProvider::Lsp,
            EdgeProvider::Engine,
            EdgeProvider::Static,
        ] {
            let mut row = row.clone();
            row.provider = provider;
            if provider == EdgeProvider::Engine {
                row.derivation = rift_protocol::read::EdgeDerivation::Heuristic;
                row.confidence = Some(0.5);
            }
            let edge: Edge = row.into();
            builder.insert_edge(&edge, None)?;
        }
        let dataset = builder.build();
        for statement in [
            "SELECT ?edge { ?edge a :Edge; :from ?from; :to ?to }",
            "SELECT ?edge { ?edge a :Edge; :provider 'lsp' }",
        ] {
            let query = GraphQuery::parse(statement, &GraphLimits::default())?;
            let mut answer: QueryAnswer =
                dataset.evaluate(&query, &QueryCancellation::new())?.into();
            dataset.annotate_providers(&mut answer)?;
            let included = answer
                .clone()
                .project(QueryInclude::from([QuerySection::Providers]));
            let included = serde_json::to_value(included)?;
            assert_eq!(
                included["bindings"][0]["edge"]["providers"],
                json!(["engine", "lsp", "static"])
            );
            let answer = answer.clone().project(QueryInclude::default());
            assert!(
                serde_json::to_value(&answer)?["bindings"][0]["edge"]
                    .get("providers")
                    .is_none()
            );
            let QueryAnswer::Bindings { bindings, .. } = answer else {
                panic!("SELECT bindings")
            };
            assert_eq!(bindings.len(), 1, "{statement}");
        }
        let query = GraphQuery::parse(
            "SELECT ?derivation { ?edge a :Edge; :derivation ?derivation }",
            &GraphLimits::default(),
        )?;
        let evidence: QueryAnswer = dataset.evaluate(&query, &QueryCancellation::new())?.into();
        let QueryAnswer::Bindings { bindings, .. } = evidence else {
            panic!("SELECT bindings")
        };
        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings[0]["derivation"],
            RdfTerm::Iri {
                value: "urn:rift:vocabulary:resolution".to_owned()
            }
        );
        let query = GraphQuery::parse("DESCRIBE ?edge { ?edge a :Edge }", &GraphLimits::default())?;
        let mut answer: QueryAnswer = dataset.evaluate(&query, &QueryCancellation::new())?.into();
        dataset.annotate_providers(&mut answer)?;
        let default = answer.clone().project(QueryInclude::default());
        let included = answer
            .clone()
            .project(QueryInclude::from([QuerySection::Providers]));
        let QueryAnswer::Graph { triples } = &default else {
            panic!("graph answer")
        };
        assert!(!triples.iter().any(|triple| is_provider(&triple.predicate)));
        assert!(!serde_json::to_string(&default)?.contains("providers"));
        let value = serde_json::to_value(&included)?;
        assert!(
            value["triples"]
                .as_array()
                .is_some_and(|triples| triples.iter().any(|triple| {
                    triple["subject"]["providers"] == json!(["engine", "lsp", "static"])
                }))
        );
        let QueryAnswer::Graph { triples } = included else {
            panic!("graph answer")
        };
        assert!(!triples.iter().any(|triple| is_provider(&triple.predicate)));
        Ok(())
    }
}
