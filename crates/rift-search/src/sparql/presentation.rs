//! Identifies directly projected retrieval fields in an admitted `SELECT`.

use std::collections::BTreeMap;

use oxrdf::{Variable, vocab::rdf};
use serde::{Deserialize, Serialize};
use spargebra::Query;
use spargebra::algebra::QueryExpression;
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};

use super::{GraphQuery, VOCABULARY_NAMESPACE};

/// The fields that give a retrieval answer its text layout.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryField {
    /// Canonical symbol.
    Symbol,
    /// Symbol name.
    Name,
    /// Project-relative path.
    Path,
    /// Source line.
    Line,
    /// Source excerpt.
    Source,
    /// Start byte offset.
    Start,
    /// End byte offset.
    End,
    /// Retrieval or flow score.
    Score,
    /// The identity of a call path.
    Flow,
    /// A step's zero-based index within a call path.
    Step,
}

/// Text layout selected by direct bindings to one retrieval service's fields.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryPresentation {
    /// Display the projected terms in their query result form.
    #[default]
    Raw,
    /// Lead with the match name or location and any selected source.
    Matches(BTreeMap<QueryField, String>),
    /// Display each call path's selected steps.
    Flows(BTreeMap<QueryField, String>),
}

impl GraphQuery {
    /// Identifies direct, projected retrieval bindings, independent of the variable names.
    /// A query with joins or computed bindings outside the retrieval graph uses the raw layout.
    #[must_use]
    pub fn presentation(&self) -> QueryPresentation {
        classify(self).unwrap_or_default()
    }
}

fn classify(query: &GraphQuery) -> Option<QueryPresentation> {
    let Query::Select(select) = query.query() else {
        return None;
    };
    let [service] = query.retrieval_requests() else {
        return None;
    };
    let mut expression = &select.expression;
    let mut projection = None;
    // Admission bounds algebra depth before presentation inspects it.
    loop {
        expression = match expression {
            QueryExpression::Project { inner, variables } if projection.is_none() => {
                projection = Some(variables.as_slice());
                inner
            }
            QueryExpression::OrderBy { inner, .. }
            | QueryExpression::Distinct { inner, .. }
            | QueryExpression::Reduced { inner, .. }
            | QueryExpression::Slice { inner, .. }
            | QueryExpression::Filter { inner, .. } => inner,
            _ => break,
        };
    }
    let QueryExpression::Graph {
        name: NamedNodePattern::NamedNode(graph),
        inner,
    } = expression
    else {
        return None;
    };
    if graph != &service.graph {
        return None;
    }
    let QueryExpression::Bgp { patterns } = inner.as_ref() else {
        return None;
    };
    let patterns = Projection {
        patterns,
        projected: projection?,
    };
    if service.flow_limit.is_some() {
        patterns.flows()
    } else {
        patterns.matches()
    }
}

struct Projection<'a> {
    patterns: &'a [TriplePattern],
    projected: &'a [Variable],
}

impl Projection<'_> {
    fn root(&self, class: &str) -> Option<&TermPattern> {
        let mut roots = self.patterns.iter().filter_map(|pattern| {
            matches!((&pattern.predicate, &pattern.object),
                (NamedNodePattern::NamedNode(predicate), TermPattern::NamedNode(object))
                    if *predicate == rdf::TYPE && object.as_str().strip_prefix(VOCABULARY_NAMESPACE) == Some(class))
                .then_some(&pattern.subject)
        });
        let first = roots.next()?;
        roots.all(|other| first == other).then_some(first)
    }

    fn object(&self, subject: &TermPattern, field: &str) -> Option<&TermPattern> {
        let mut objects = self.patterns.iter().filter_map(|pattern| {
            (pattern.subject == *subject
                && matches!(&pattern.predicate, NamedNodePattern::NamedNode(predicate)
                    if predicate.as_str().strip_prefix(VOCABULARY_NAMESPACE) == Some(field)))
            .then_some(&pattern.object)
        });
        let first = objects.next()?;
        objects.all(|other| first == other).then_some(first)
    }

    fn fields<const N: usize>(
        &self,
        bindings: [(QueryField, Option<&TermPattern>); N],
    ) -> BTreeMap<QueryField, String> {
        bindings
            .into_iter()
            .filter_map(|(field, binding)| match binding? {
                TermPattern::Variable(variable) if self.projected.contains(variable) => {
                    Some((field, variable.as_str().to_owned()))
                }
                _ => None,
            })
            .collect()
    }

    fn matches(&self) -> Option<QueryPresentation> {
        use QueryField::{End, Line, Name, Path, Score, Source, Start, Symbol};
        let hit = self.root("SearchHit")?;
        let symbol = self.object(hit, "symbol");
        let fields = self.fields([
            (Symbol, symbol),
            (Name, symbol.and_then(|symbol| self.object(symbol, "name"))),
            (Path, self.object(hit, "path")),
            (Line, self.object(hit, "line")),
            (Source, self.object(hit, "source")),
            (Start, self.object(hit, "start")),
            (End, self.object(hit, "end")),
            (Score, self.object(hit, "score")),
        ]);
        [Symbol, Name, Path, Source]
            .iter()
            .any(|field| fields.contains_key(field))
            .then_some(QueryPresentation::Matches(fields))
    }

    fn flows(&self) -> Option<QueryPresentation> {
        use QueryField::{Flow, Name, Score, Step, Symbol};
        let flow = self.root("Flow")?;
        let step = self.object(flow, "steps")?;
        let symbol = self.object(step, "value")?;
        let fields = self.fields([
            (Flow, Some(flow)),
            (Step, self.object(step, "index")),
            (Symbol, Some(symbol)),
            (Name, self.object(symbol, "name")),
            (Score, self.object(flow, "score")),
        ]);
        [Flow, Step, Symbol]
            .iter()
            .all(|field| fields.contains_key(field))
            .then_some(QueryPresentation::Flows(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sparql::GraphLimits;

    fn presentation(query: &str) -> Result<QueryPresentation, rift_error::RiftError> {
        Ok(GraphQuery::parse(query, &GraphLimits::default())?.presentation())
    }

    #[test]
    fn presentation_uses_retrieval_patterns_and_projected_variables()
    -> Result<(), rift_error::RiftError> {
        use QueryField::{Name, Path, Source, Symbol};
        assert_eq!(
            presentation(
                "SELECT ?place ?id ?label { SERVICE :search { ?hit :query 'ReadService';
                 :path ?place; :symbol ?id; :source ?omitted . ?id :name ?label } }
                 ORDER BY ?label LIMIT 2"
            )?,
            QueryPresentation::Matches(BTreeMap::from([
                (Path, "place".to_owned()),
                (Symbol, "id".to_owned()),
                (Name, "label".to_owned()),
            ]))
        );
        assert_eq!(
            presentation(
                "SELECT ?file ?evidence { SERVICE :search { ?hit :pattern 'impl ReadService';
                 :target 'file'; :path ?file; :source ?evidence } }"
            )?,
            QueryPresentation::Matches(BTreeMap::from([
                (Path, "file".to_owned()),
                (Source, "evidence".to_owned()),
            ]))
        );
        Ok(())
    }

    #[test]
    fn flow_presentation_requires_projected_flow_index_and_symbol()
    -> Result<(), rift_error::RiftError> {
        use QueryField::{Flow, Score, Step, Symbol};
        assert_eq!(
            presentation(
                "SELECT ?route ?i ?callee ?rank { SERVICE :flows { ?route :query 'ReadService';
                 :limit 1; :score ?rank; :steps ?step . ?step :index ?i; :value ?callee } }
                 ORDER BY DESC(?rank) ?route ?i OFFSET 1"
            )?,
            QueryPresentation::Flows(BTreeMap::from([
                (Flow, "route".to_owned()),
                (Step, "i".to_owned()),
                (Symbol, "callee".to_owned()),
                (Score, "rank".to_owned()),
            ]))
        );
        for query in [
            "SELECT ?flow ?index ?symbol { VALUES (?flow ?index ?symbol) {
             (:Flow 2 <rift://symbol/local/rust/app/ReadService>) } }",
            "SELECT ?flow ?index ?symbol { SERVICE :flows { ?flow :query 'ReadService';
             :steps ?step . ?step :index ?index; :evidence ?symbol } }",
            "SELECT ?index ?symbol { SERVICE :flows { ?flow :query 'ReadService';
             :steps ?step . ?step :index ?index; :value ?symbol } }",
        ] {
            assert_eq!(presentation(query)?, QueryPresentation::Raw, "{query}");
        }
        Ok(())
    }
}
