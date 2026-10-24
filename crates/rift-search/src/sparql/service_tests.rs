//! Existing producer and retained-answer services through library evaluation.

use std::collections::BTreeMap;
use std::error::Error;

use rift_protocol::query::{QueryAnswer, RdfTerm, RdfTriple};
use rift_protocol::read::{Pagination, SearchHit, SearchHitTarget, SearchResult};

use super::{
    PreparedQuery, QueryCancellation, QueryFacts, QueryLimits, QueryServiceData, QueryServices,
    RetainedResult,
};

type TestResult = Result<(), Box<dyn Error>>;

fn limits() -> QueryLimits {
    QueryLimits {
        query_bytes_max: 16_384,
        algebra_nodes_max: 4096,
        quads_max: 100_000,
        rdf_bytes_max: 16_777_216,
        result_items_max: 20_000,
        result_bytes_max: 16_777_216,
    }
}

fn hit(start: u64) -> Result<SearchHit, serde_json::Error> {
    serde_json::from_value(serde_json::json!({
        "hit": {"target":"symbol", "symbol": {"id":"rift://symbol/local/rust/app/config", "name":"config", "kind":"function", "language":"rust"}},
        "score":0.5, "range":{"start":start,"end":start+3},"line":1,"path":"src/lib.rs"
    }))
}

fn service(query: &PreparedQuery, hits: Vec<SearchHit>) -> QueryServices {
    let result = SearchResult {
        view: None,
        results: hits,
        pagination: Pagination {
            page_index: 0,
            total_pages: 1,
        },
        warnings: Vec::new(),
    };
    QueryServices {
        retrievals: vec![QueryServiceData {
            request: query.retrievals()[0].clone(),
            result,
        }],
        bindings: Vec::new(),
    }
}

fn rows(answer: QueryAnswer) -> Result<Vec<BTreeMap<String, RdfTerm>>, Box<dyn Error>> {
    match answer {
        QueryAnswer::Bindings { bindings, .. } => Ok(bindings),
        _ => Err("expected bindings".into()),
    }
}

#[test]
fn test_ranked_service_reuses_existing_hit_scores_and_symbol_identity() -> TestResult {
    let query = PreparedQuery::parse(
        "SELECT ?symbol ?score { SERVICE :search { ?hit :query 'config'; :symbol ?symbol; :score ?score } }",
        limits(),
    )?;
    assert!(!query.requirements().domain);
    assert_eq!(query.retrievals().len(), 1);
    assert_eq!(query.retrievals()[0].query.as_deref(), Some("config"));
    let services = service(&query, vec![hit(0)?]);
    let answer = query.evaluate_with_services(
        QueryFacts::default(),
        limits(),
        QueryCancellation::default(),
        services,
    )?;
    let answer = rows(answer)?;
    assert_eq!(answer.len(), 1);
    assert_eq!(
        answer[0]["symbol"],
        RdfTerm::Iri {
            value: "rift://symbol/local/rust/app/config".to_owned()
        }
    );
    assert!(matches!(&answer[0]["score"], RdfTerm::Literal { value, .. } if value == "0.5"));
    Ok(())
}

#[test]
fn test_regex_service_preserves_every_match_range() -> TestResult {
    let query = PreparedQuery::parse(
        "SELECT ?start ?end { SERVICE :search { ?hit :pattern 'config'; :start ?start; :end ?end } } ORDER BY ?start",
        limits(),
    )?;
    let mut first = hit(2)?;
    first.hit = SearchHitTarget::File {
        size: 40,
        languages: Vec::new(),
    };
    first.score = None;
    let mut second = first.clone();
    second.range = Some(rift_protocol::read::TextRange { start: 10, end: 13 });
    let services = service(&query, vec![first, second]);
    let answer = rows(query.evaluate_with_services(
        QueryFacts::default(),
        limits(),
        QueryCancellation::default(),
        services,
    )?)?;
    assert_eq!(answer.len(), 2);
    assert_ne!(answer[0]["start"], answer[1]["start"]);
    Ok(())
}

#[test]
fn test_service_request_preserves_scope_revision_and_source_admission() -> TestResult {
    let query = PreparedQuery::parse(
        "SELECT * { SERVICE :search { ?hit :query 'config'; :scope :all; :paths '{\"force_include\":[\"ignored/**\"]}'^^rdf:JSON; :source ?source } }",
        limits(),
    )?;
    let request = &query.retrievals()[0];
    assert_eq!(
        serde_json::to_value(&request.scope)?,
        serde_json::json!("all")
    );
    assert_eq!(
        request.paths.as_ref().ok_or("missing paths")?.force_include[0].0,
        "ignored/**"
    );
    assert!(
        request
            .include
            .as_ref()
            .is_some_and(|include| include.contains(&rift_protocol::read::SearchInclude::Source))
    );
    let revision = PreparedQuery::parse(
        "SELECT * { SERVICE :search { ?hit :query 'config'; :rev 'HEAD~1' } }",
        limits(),
    )?;
    assert_eq!(
        revision.retrievals()[0]
            .rev
            .as_ref()
            .map(|revision| revision.0.as_str()),
        Some("HEAD~1")
    );
    Ok(())
}

#[test]
fn test_required_service_is_checked_even_when_silent() -> TestResult {
    let query = PreparedQuery::parse(
        "ASK { SERVICE SILENT <urn:missing:service> { ?s ?p ?o } }",
        limits(),
    )?;
    assert!(
        query
            .evaluate(
                QueryFacts::default(),
                limits(),
                QueryCancellation::default()
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn test_retained_table_preserves_duplicates_blank_nodes_and_inner_filter() -> TestResult {
    let row = BTreeMap::from([
        (
            "number".to_owned(),
            RdfTerm::Literal {
                value: "2".to_owned(),
                datatype: "http://www.w3.org/2001/XMLSchema#integer".to_owned(),
            },
        ),
        (
            "node".to_owned(),
            RdfTerm::BlankNode {
                value: "retained".to_owned(),
            },
        ),
    ]);
    let retained = RetainedResult {
        service: "urn:rift:result:12345678".to_owned(),
        answer: QueryAnswer::Bindings {
            variables: vec!["number".to_owned(), "node".to_owned()],
            bindings: vec![row.clone(), row.clone()],
        },
    };
    let services = QueryServices {
        retrievals: Vec::new(),
        bindings: vec![retained],
    };
    let query = PreparedQuery::parse(
        "SELECT ?number ?node { SERVICE <urn:rift:result:12345678> { FILTER(?number > 1) } }",
        limits(),
    )?;
    let answer = rows(query.evaluate_with_services(
        QueryFacts::default(),
        limits(),
        QueryCancellation::default(),
        services,
    )?)?;
    assert_eq!(answer, vec![row.clone(), row]);
    Ok(())
}

#[test]
fn test_retained_graph_uses_rdf_graph_matching() -> TestResult {
    let triple = RdfTriple {
        subject: RdfTerm::BlankNode {
            value: "retained".to_owned(),
        },
        predicate: RdfTerm::Iri {
            value: "urn:predicate:name".to_owned(),
        },
        object: RdfTerm::LanguageLiteral {
            value: "bonjour".to_owned(),
            language: "fr".to_owned(),
        },
    };
    let services = QueryServices {
        retrievals: Vec::new(),
        bindings: vec![RetainedResult {
            service: "urn:rift:result:12345678".to_owned(),
            answer: QueryAnswer::Graph {
                triples: vec![triple.clone()],
            },
        }],
    };
    let query = PreparedQuery::parse(
        "SELECT * { SERVICE <urn:rift:result:12345678> { ?s ?p ?o } }",
        limits(),
    )?;
    let answer = rows(query.evaluate_with_services(
        QueryFacts::default(),
        limits(),
        QueryCancellation::default(),
        services,
    )?)?;
    assert_eq!(answer.len(), 1);
    assert_eq!(answer[0]["s"], triple.subject);
    assert_eq!(answer[0]["o"], triple.object);
    Ok(())
}

#[test]
fn test_retained_boolean_uses_one_typed_value() -> TestResult {
    let services = QueryServices {
        retrievals: Vec::new(),
        bindings: vec![RetainedResult {
            service: "urn:rift:result:12345678".to_owned(),
            answer: QueryAnswer::Boolean { value: false },
        }],
    };
    let query = PreparedQuery::parse(
        "SELECT ?value { SERVICE <urn:rift:result:12345678> {} }",
        limits(),
    )?;
    let answer = rows(query.evaluate_with_services(
        QueryFacts::default(),
        limits(),
        QueryCancellation::default(),
        services,
    )?)?;
    assert_eq!(
        answer[0]["value"],
        RdfTerm::Literal {
            value: "false".to_owned(),
            datatype: "http://www.w3.org/2001/XMLSchema#boolean".to_owned()
        }
    );
    Ok(())
}
