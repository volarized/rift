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

/// Characters an IRI written as `<...>` never holds, so a `<` followed by one is an operator.
const IRI_EXCLUDED: &[u8] = b"<\"{}|^`\\";

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
    /// Parsing runs on the query thread, after a lexical scan bounds bracket nesting and
    /// operators: the parser recurses once per bracket level and once per chained operator.
    ///
    /// # Errors
    ///
    /// - `query_too_large` when `text` passes the query byte bound, checked before parsing.
    /// - `query_nesting_limit` when brackets outside literals and IRIs nest past the bound.
    /// - `query_operator_limit` when operators outside literals and IRIs pass the bound.
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
        let shape = shape(text.as_bytes());
        if shape.depth > limits.nesting_depth_max() {
            return errors::search::query_nesting_limit()
                .depth(shape.depth)
                .limit(limits.nesting_depth_max())
                .fail();
        }
        if shape.operators > limits.operators_max() {
            return errors::search::query_operator_limit()
                .count(shape.operators)
                .limit(limits.operators_max())
                .fail();
        }
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
                let separators = admit(&mut query, limits.solutions_max())?;
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

/// What the lexical scan measured: the parser recurses once per bracket level and once per
/// operator of a chain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Shape {
    /// The deepest nesting of `(`, `{` and `[`.
    depth: usize,
    /// The count of `+`, `-`, `*`, `/`, `&` and `|`.
    operators: usize,
}

/// Measures `text` outside string literals, IRIs and comments.
///
/// One pass over the bytes, no allocation. A closing bracket with nothing open counts nothing,
/// so an unbalanced query reaches the parser and its syntax refusal. A `-` inside a prefixed
/// name and the `*` of `SELECT *` count as operators too: the count only ever overestimates.
fn shape(text: &[u8]) -> Shape {
    let (mut shape, mut depth, mut index) = (Shape::default(), 0_usize, 0_usize);
    while let Some(&byte) = text.get(index) {
        index = match byte {
            b'(' | b'{' | b'[' => {
                depth += 1;
                shape.depth = shape.depth.max(depth);
                index + 1
            }
            b')' | b'}' | b']' => {
                depth = depth.saturating_sub(1);
                index + 1
            }
            b'+' | b'-' | b'*' | b'/' | b'&' | b'|' => {
                shape.operators += 1;
                index + 1
            }
            b'"' | b'\'' => skip_string(text, index),
            b'<' => skip_iri(text, index),
            b'#' => skip_comment(text, index),
            _ => index + 1,
        };
    }
    shape
}

/// The index past the string literal starting at `start`, long forms and escapes included.
fn skip_string(text: &[u8], start: usize) -> usize {
    let quote = text[start];
    let long = text.get(start..start + 3) == Some(&[quote; 3][..]);
    let mut index = if long { start + 3 } else { start + 1 };
    while let Some(&byte) = text.get(index) {
        match byte {
            b'\\' => index += 2,
            _ if long && text.get(index..index + 3) == Some(&[quote; 3][..]) => return index + 3,
            _ if !long && byte == quote => return index + 1,
            _ => index += 1,
        }
    }
    text.len()
}

/// The index past the IRI starting at `start`, or past `<` when it is an operator.
fn skip_iri(text: &[u8], start: usize) -> usize {
    let body = text[start + 1..].iter().position(|byte| {
        byte.is_ascii_whitespace() || IRI_EXCLUDED.contains(byte) || *byte == b'>'
    });
    match body.map(|offset| start + 1 + offset) {
        Some(end) if text[end] == b'>' => end + 1,
        Some(_) | None => start + 1,
    }
}

/// The index past the comment starting at `start`.
fn skip_comment(text: &[u8], start: usize) -> usize {
    text[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(text.len(), |offset| start + offset + 1)
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
