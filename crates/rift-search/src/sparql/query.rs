//! Parsing and admission of one SPARQL query.

use rift_error::{RiftError, errors};
use spargebra::{Query, SparqlParser};

use super::admission::admit;
use super::limits::GraphLimits;
use super::query_thread;
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
    separators: Vec<String>,
}

impl GraphQuery {
    /// Parses `text` and refuses what the graph does not serve.
    ///
    /// Parsing runs on the query thread, after a count of every opening bracket and operator
    /// character bounds how deep the parser can recurse.
    ///
    /// # Errors
    ///
    /// - `query_too_large` when `text` passes the query byte bound, checked before parsing.
    /// - `query_structure_limit` when the text holds more opening brackets or operator
    ///   characters than allowed, or the query joins more patterns than allowed.
    /// - `query_syntax_invalid` when `text` is neither a query nor an update.
    /// - `query_feature_refused` naming the feature for an update, `CONSTRUCT`, `DESCRIBE`,
    ///   `FROM`, `FROM NAMED`, any `SERVICE`, `SERVICE SILENT` included, and a join of patterns
    ///   sharing no variable.
    /// - `query_answer_limit` when the product of every `VALUES` table's rows passes the
    ///   solution bound.
    /// - `query_evaluation_failed` when the query thread cannot start.
    pub fn parse(text: &str, limits: &GraphLimits) -> Result<Self, RiftError> {
        if text.len() > limits.query_bytes_max() {
            return errors::search::query_too_large()
                .limit(limits.query_bytes_max())
                .observed(text.len())
                .fail();
        }
        Structure::of(text.as_bytes()).admit(limits)?;
        query_thread::run(None, || Self::parsed(text, limits))?
    }

    /// The parsed query, for evaluation.
    pub(super) const fn query(&self) -> &Query {
        &self.query
    }

    /// The separator of each bounded `GROUP_CONCAT`, by its index.
    pub(super) fn separators(&self) -> &[String] {
        &self.separators
    }

    fn parsed(text: &str, limits: &GraphLimits) -> Result<Self, RiftError> {
        let parser = predeclared_parser();
        match parser.parse_query(text) {
            Ok(mut query) => {
                let separators = admit(&mut query, limits)?;
                Ok(Self { query, separators })
            }
            Err(_) if parser.parse_update(text).is_ok() => refused("an update"),
            Err(error) => errors::search::query_syntax_invalid()
                .detail(error.to_string())
                .fail(),
        }
    }
}

/// Refuses `feature`, which the read-only graph does not serve.
pub(super) fn refused<T>(feature: &str) -> Result<T, RiftError> {
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

/// Bytes that open a bracket, each a level the parser may recurse into.
const BRACKETS: &[u8] = b"({[";
/// Bytes that can chain operators, each a level the parser may recurse into.
const OPERATORS: &[u8] = b"+-*/&|^!";

/// What the scan counted: opening brackets and operator characters, wherever they stand.
///
/// The scan reads every byte, string literals, IRIs, escapes and comments included, so it never
/// depends on where a lexer would draw a token boundary: the counts can only overestimate the
/// recursion the parser performs. One pass, no allocation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Structure {
    brackets: usize,
    operators: usize,
}

impl Structure {
    fn of(text: &[u8]) -> Self {
        let count = |set: &[u8]| text.iter().filter(|byte| set.contains(byte)).count();
        Self {
            brackets: count(BRACKETS),
            operators: count(OPERATORS),
        }
    }

    /// Refuses a count past its bound, before the parser recurses into it.
    fn admit(self, limits: &GraphLimits) -> Result<(), RiftError> {
        let counts = [
            ("opening brackets", self.brackets, limits.brackets_max()),
            (
                "operator characters",
                self.operators,
                limits.operators_max(),
            ),
        ];
        match counts.into_iter().find(|(_, count, limit)| count > limit) {
            Some((bound, count, limit)) => errors::search::query_structure_limit()
                .bound(bound)
                .count(count)
                .limit(limit)
                .fail(),
            None => Ok(()),
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
            "SELECT * { BIND(1 AS ?x) ?s ?p ?o }",
            "SELECT * { VALUES ?k { \"function\" } ?s :kind ?k . ?s :calls ?t . ?t :kind ?k2 }",
            "SELECT * { { SELECT (COUNT(*) AS ?n) { ?a ?b ?c } } ?s ?p ?o }",
            "SELECT * { { SELECT ?a { ?a ?b ?c } LIMIT 1 } ?s ?p ?o }",
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
            "SELECT * { ?a :calls ?b . ?c :calls ?d }",
            "SELECT * { { ?a :calls ?b } { ?c :kind ?k } }",
            "SELECT * { ?a :calls ?b OPTIONAL { ?c :kind ?k } }",
            "SELECT * { VALUES ?x { 1 2 } ?a :calls ?b }",
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
