//! Parsing and admission of one SPARQL query.

use rift_error::{RiftError, errors};
use spargebra::algebra::{AggregateExpression, Expression, OrderExpression, QueryExpression};
use spargebra::term::NamedNodePattern;
use spargebra::{Query, SparqlParser};

use super::limits::GraphLimits;
use super::vocabulary::VOCABULARY_NAMESPACE;

/// Prefixes every query may use without declaring them.
const PREDECLARED_PREFIXES: [(&str, &str); 3] = [
    ("", VOCABULARY_NAMESPACE),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
];

/// One admitted SPARQL 1.1 `SELECT` or `ASK` query over the fixed graph.
///
/// The prefixes `:` (the vocabulary), `rdf:` and `xsd:` are predeclared; a query may redeclare
/// them.
#[derive(Clone, Debug)]
pub struct GraphQuery {
    query: Query,
}

impl GraphQuery {
    /// Parses `text` and refuses what the graph does not serve.
    ///
    /// # Errors
    ///
    /// - `query_too_large` when `text` passes the query byte bound, checked before parsing.
    /// - `query_syntax_invalid` when `text` is neither a query nor an update.
    /// - `query_feature_refused` naming the feature for an update, `CONSTRUCT`, `DESCRIBE`,
    ///   `FROM`, `FROM NAMED`, and any `SERVICE`, `SERVICE SILENT` included.
    pub fn parse(text: &str, limits: &GraphLimits) -> Result<Self, RiftError> {
        if text.len() > limits.query_bytes_max() {
            return errors::search::query_too_large()
                .limit(limits.query_bytes_max())
                .observed(text.len())
                .fail();
        }
        let parser = predeclared_parser();
        match parser.parse_query(text) {
            Ok(query) => Self::admitted(query),
            Err(_) if parser.parse_update(text).is_ok() => refused("an update"),
            Err(error) => errors::search::query_syntax_invalid()
                .detail(error.to_string())
                .fail(),
        }
    }

    /// The parsed query, for evaluation.
    pub(super) const fn query(&self) -> &Query {
        &self.query
    }

    /// Refuses a form, dataset clause or service the graph does not serve.
    fn admitted(query: Query) -> Result<Self, RiftError> {
        let (expression, dataset) = match &query {
            Query::Select(select) => (&select.expression, select.dataset.as_ref()),
            Query::Ask(ask) => (&ask.expression, ask.dataset.as_ref()),
            Query::Construct(_) => return refused("CONSTRUCT"),
            Query::Describe(_) => return refused("DESCRIBE"),
        };
        if dataset.is_some() {
            return refused("a FROM or FROM NAMED clause");
        }
        match first_service(expression) {
            Some(service) => refused(&format!("SERVICE {service}")),
            None => Ok(Self { query }),
        }
    }
}

/// Refuses `feature`, which the read-only graph does not serve.
fn refused(feature: &str) -> Result<GraphQuery, RiftError> {
    errors::search::query_feature_refused()
        .feature(feature)
        .fail()
}

/// The parser every query goes through, with the predeclared prefixes.
fn predeclared_parser() -> SparqlParser {
    PREDECLARED_PREFIXES
        .iter()
        .fold(SparqlParser::new(), |parser, (name, iri)| {
            match parser.with_prefix(name, iri) {
                Ok(declared) => declared,
                Err(error) => {
                    unreachable!("predeclared prefix {name}: {iri} must be an IRI: {error}")
                }
            }
        })
}

/// One algebra element still to inspect.
enum Visit<'a> {
    Pattern(&'a QueryExpression),
    Expression(&'a Expression),
}

/// The name of the first `SERVICE` anywhere in `root`, `EXISTS` filters included.
///
/// The walk keeps an explicit stack, so query nesting never deepens the call stack. Its work
/// is one step per algebra element, bounded by the query byte bound the parser ran under.
fn first_service(root: &QueryExpression) -> Option<&NamedNodePattern> {
    let mut pending = vec![Visit::Pattern(root)];
    while let Some(visit) = pending.pop() {
        match visit {
            Visit::Pattern(QueryExpression::Service { name, .. }) => return Some(name),
            Visit::Pattern(pattern) => push_pattern_children(pattern, &mut pending),
            Visit::Expression(expression) => push_expression_children(expression, &mut pending),
        }
    }
    None
}

/// Pushes the patterns and expressions one pattern holds.
fn push_pattern_children<'a>(pattern: &'a QueryExpression, pending: &mut Vec<Visit<'a>>) {
    match pattern {
        QueryExpression::Bgp { .. }
        | QueryExpression::Path { .. }
        | QueryExpression::Values { .. }
        | QueryExpression::Service { .. } => {}
        QueryExpression::Join { left, right }
        | QueryExpression::Union { left, right }
        | QueryExpression::Minus { left, right } => {
            pending.extend([Visit::Pattern(left), Visit::Pattern(right)]);
        }
        QueryExpression::LeftJoin {
            left,
            right,
            expression,
        } => {
            pending.extend([Visit::Pattern(left), Visit::Pattern(right)]);
            pending.extend(expression.iter().map(Visit::Expression));
        }
        QueryExpression::Filter { expr, inner } => {
            pending.extend([Visit::Expression(expr), Visit::Pattern(inner)]);
        }
        QueryExpression::Extend {
            inner, expression, ..
        } => pending.extend([Visit::Expression(expression), Visit::Pattern(inner)]),
        QueryExpression::OrderBy { inner, expression } => {
            pending.push(Visit::Pattern(inner));
            pending.extend(expression.iter().map(|order| match order {
                OrderExpression::Asc(expression) | OrderExpression::Desc(expression) => {
                    Visit::Expression(expression)
                }
            }));
        }
        QueryExpression::Group {
            inner, aggregates, ..
        } => {
            pending.push(Visit::Pattern(inner));
            pending.extend(
                aggregates
                    .iter()
                    .filter_map(|(_, aggregate)| match aggregate {
                        AggregateExpression::CountSolutions { .. } => None,
                        AggregateExpression::FunctionCall { expr, .. } => {
                            Some(Visit::Expression(expr))
                        }
                    }),
            );
        }
        QueryExpression::Graph { inner, .. }
        | QueryExpression::Project { inner, .. }
        | QueryExpression::Distinct { inner }
        | QueryExpression::Reduced { inner }
        | QueryExpression::Slice { inner, .. } => pending.push(Visit::Pattern(inner)),
    }
}

/// Pushes the subexpressions and `EXISTS` patterns one expression holds.
fn push_expression_children<'a>(expression: &'a Expression, pending: &mut Vec<Visit<'a>>) {
    match expression {
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_) => {}
        Expression::Or(left, right) | Expression::And(left, right) => {
            pending.extend([Visit::Expression(left), Visit::Expression(right)]);
        }
        Expression::In(needle, haystack) => {
            pending.push(Visit::Expression(needle));
            pending.extend(haystack.iter().map(Visit::Expression));
        }
        Expression::Exists(pattern) => pending.push(Visit::Pattern(pattern)),
        Expression::If(condition, then, otherwise) => pending.extend([
            Visit::Expression(condition),
            Visit::Expression(then),
            Visit::Expression(otherwise),
        ]),
        Expression::Coalesce(arguments) | Expression::FunctionCall(_, arguments) => {
            pending.extend(arguments.iter().map(Visit::Expression));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GraphQuery;
    use crate::sparql::limits::GraphLimits;
    use rift_error::{ErrorSlug, errors};

    fn refusal(text: &str) -> ErrorSlug {
        GraphQuery::parse(text, &GraphLimits::default())
            .expect_err("the query is refused")
            .slug()
    }

    #[test]
    fn test_graph_query_parse_accepts_select_and_ask_with_predeclared_prefixes() {
        let accepted = [
            "SELECT ?symbol WHERE { ?symbol a :Symbol ; :calls+ ?callee }",
            "ASK { ?node rdf:type :Node ; :start ?start FILTER(?start > \"0\"^^xsd:integer) }",
            "PREFIX : <urn:rift:vocabulary:> SELECT (COUNT(*) AS ?count) { ?s :uses ?r }",
            "SELECT * { ?s ?p ?o FILTER NOT EXISTS { ?s :removedIn ?r } } ORDER BY ?s LIMIT 5",
        ];
        for text in accepted {
            assert!(
                GraphQuery::parse(text, &GraphLimits::default()).is_ok(),
                "query must be admitted: {text}"
            );
        }
    }

    #[test]
    fn test_graph_query_parse_refuses_unserved_features() {
        let syntax = refusal("SELECT WHERE {");
        assert_eq!(syntax, errors::search::query_syntax_invalid::SLUG);
        let refused = [
            "INSERT DATA { :a :b :c }",
            "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }",
            "DESCRIBE <urn:x>",
            "SELECT * FROM <urn:g> { ?s ?p ?o }",
            "SELECT * FROM NAMED <urn:g> { ?s ?p ?o }",
            "SELECT * { SERVICE <https://example.org/sparql> { ?s ?p ?o } }",
            "SELECT * { ?s ?p ?o OPTIONAL { SERVICE SILENT ?endpoint { ?s ?q ?r } } }",
            "SELECT * { ?s ?p ?o FILTER EXISTS { SERVICE :search { ?s ?q ?r } } }",
            "SELECT (SUM(IF(EXISTS { SERVICE <urn:a> {} }, 1, 0)) AS ?n) { ?s ?p ?o }",
            "SELECT * { ?s ?p ?o } ORDER BY DESC(COALESCE(EXISTS { SERVICE <urn:a> {} }))",
            "SELECT * { BIND(?x IN (1, EXISTS { SERVICE <urn:a> {} }) AS ?y) }",
        ];
        for text in refused {
            let slug = refusal(text);
            assert_eq!(slug, errors::search::query_feature_refused::SLUG, "{text}");
        }
    }

    #[test]
    fn test_graph_query_parse_refuses_text_past_byte_bound() {
        let limits = GraphLimits::builder()
            .query_bytes_max(8)
            .build()
            .expect("bounds are nonzero");
        let error = GraphQuery::parse("ASK { ?s ?p ?o }", &limits).expect_err("too large");
        assert_eq!(error.slug(), errors::search::query_too_large::SLUG);
        assert!(
            GraphQuery::parse("ASK {}", &limits).is_ok(),
            "a query at the bound parses"
        );
    }
}
