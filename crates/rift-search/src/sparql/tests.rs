//! SPARQL semantics and acquisition controls over independently enumerated facts.

use crate::query::FactRequirement;
use std::error::Error;

use rift_protocol::query::{QueryAnswer, QueryOccurrence, RdfTerm};
use rift_protocol::read::{Relationship, Symbol};

use super::{PreparedQuery, QueryCancellation, QueryFacts, QueryLimits, QuerySymbol, VOCABULARY};

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

fn identity(name: &str) -> String {
    format!("rift://symbol/local/rust/fixture/{name}")
}

fn edge(from: &str, to: &str, start: u64) -> Result<Relationship, serde_json::Error> {
    serde_json::from_value(serde_json::json!({
        "from": identity(from), "to": identity(to), "kind": "call", "facets": ["calls"], "derivation": "resolution",
        "occurrence": {"unit": "rift://source/project/src/lib.rs", "range": {"start": start, "end": start + 3}}
    }))
}

fn usage(target: &str, start: u64) -> Result<QueryOccurrence, serde_json::Error> {
    serde_json::from_value(serde_json::json!({
        "target": identity(target), "enclosing": identity("caller"), "facet": "references",
        "source": {"unit": "rift://source/project/src/lib.rs", "range": {"start": start, "end": start + 3}}
    }))
}

fn fixture() -> Result<QueryFacts, Box<dyn Error>> {
    let symbol: Symbol = serde_json::from_value(serde_json::json!({
        "id": identity("seed"), "name": "load_config", "kind": "function", "language": "rust"
    }))?;
    Ok(QueryFacts {
        symbols: vec![QuerySymbol {
            symbol,
            declarations: Vec::new(),
        }],
        relationships: vec![
            edge("seed", "first", 0)?,
            edge("first", "second", 10)?,
            edge("second", "third", 20)?,
            edge("seed", "third", 30)?,
            edge("third", "seed", 40)?,
        ],
        occurrences: vec![usage("seed", 50)?, usage("seed", 60)?],
        sources: Vec::new(),
        ..QueryFacts::default()
    })
}

fn evaluate(query: &str) -> Result<QueryAnswer, Box<dyn Error>> {
    Ok(PreparedQuery::parse(query, limits())?.evaluate(
        fixture()?,
        limits(),
        QueryCancellation::default(),
    )?)
}

fn rows(
    answer: QueryAnswer,
) -> Result<Vec<std::collections::BTreeMap<String, RdfTerm>>, Box<dyn Error>> {
    match answer {
        QueryAnswer::Bindings { bindings, .. } => Ok(bindings),
        _ => Err("expected projected bindings".into()),
    }
}

#[test]
fn test_name_query_uses_default_prefix_without_semantic_acquisition() -> TestResult {
    let query = "SELECT ?symbol { ?symbol :name \"load_config\" }";
    let prepared = PreparedQuery::parse(query, limits())?;
    assert!(
        !prepared
            .requirements()
            .requires(FactRequirement::CompleteDomain)
    );
    assert!(prepared.requirements().adjacency.is_empty());
    let answer = rows(evaluate(query)?)?;
    assert_eq!(answer.len(), 1);
    assert_eq!(
        answer[0]["symbol"],
        RdfTerm::Iri {
            value: identity("seed")
        }
    );
    Ok(())
}

#[test]
fn test_missing_node_family_refuses_negative_answers_but_allows_names() -> TestResult {
    let mut facts = fixture()?;
    facts
        .unavailable_families
        .push(super::QueryFactFamily::Nodes);
    for query in [
        "ASK { ?node a :Node }",
        "ASK { FILTER NOT EXISTS { ?node a :Node } }",
    ] {
        let prepared = PreparedQuery::parse(query, limits())?;
        assert!(prepared.requirements().requires(FactRequirement::Nodes));
        assert!(
            prepared
                .evaluate(facts.clone(), limits(), QueryCancellation::default())
                .is_err()
        );
    }
    let prepared = PreparedQuery::parse("ASK { ?symbol :name \"load_config\" }", limits())?;
    assert!(!prepared.requirements().requires(FactRequirement::Nodes));
    assert_eq!(
        prepared.evaluate(facts, limits(), QueryCancellation::default())?,
        QueryAnswer::Boolean { value: true }
    );
    Ok(())
}

#[test]
fn test_supplied_node_facts_preserve_identity_and_empty_available_family() -> TestResult {
    let prepared = PreparedQuery::parse("SELECT ?node { ?node a :Node }", limits())?;
    let mut facts = QueryFacts::default();
    assert!(
        rows(prepared.evaluate(facts.clone(), limits(), QueryCancellation::default())?)?.is_empty()
    );
    let identity = "rift://node/rust/src/lib.rs@0-3#ba7816bf";
    facts.nodes.push(serde_json::from_value(serde_json::json!({
        "unit": "rift://source/project/src/lib.rs", "bytes": 3,
        "nodes": [{"ordinal": 0, "parent": null, "node": {
            "id": identity, "unit": "rift://file/src/lib.rs", "language": "rust",
            "kind": "identifier", "range": {"start": 0, "end": 3}
        }}]
    }))?);
    facts
        .sources
        .push(serde_json::from_value(serde_json::json!({
            "unit": "rift://source/project/src/lib.rs", "text": "abc",
            "digest": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        }))?);
    let answer = rows(prepared.evaluate(facts, limits(), QueryCancellation::default())?)?;
    assert_eq!(answer.len(), 1);
    assert_ne!(
        answer[0]["node"],
        RdfTerm::Iri {
            value: identity.to_owned()
        }
    );
    Ok(())
}

#[test]
fn test_narrow_fact_selection_matches_complete_domain_and_rejects_unsafe_shapes() -> TestResult {
    let prepared = PreparedQuery::parse(
        "SELECT ?symbol { ?symbol a :Symbol; :name \"load_config\" }",
        limits(),
    )?;
    assert_eq!(
        prepared.requirements().symbol_name.as_deref(),
        Some("load_config")
    );
    let mut complete = fixture()?;
    let mut unrelated = complete.symbols[0].clone();
    unrelated.symbol.id = Some(serde_json::from_value(serde_json::json!(identity(
        "unrelated"
    )))?);
    unrelated.symbol.name = "unrelated".into();
    unrelated
        .symbol
        .signatures
        .push(serde_json::from_value(serde_json::json!({
            "display": "fn unrelated(load_config: u32)", "language": "rust",
            "parameters": [{"name":"load_config", "optional":false, "variadic":false}]
        }))?);
    complete.symbols.push(unrelated);
    let bare = PreparedQuery::parse(
        "SELECT ?subject { ?subject :name \"load_config\" }",
        limits(),
    )?;
    assert!(bare.requirements().symbol_name.is_none());
    assert_eq!(
        rows(bare.evaluate(complete.clone(), limits(), QueryCancellation::default())?)?.len(),
        2
    );
    let mut restricted = complete.clone();
    restricted.symbols.retain(|symbol| {
        Some(symbol.symbol.name.as_str()) == prepared.requirements().symbol_name.as_deref()
    });
    assert_eq!(
        prepared.evaluate(complete, limits(), QueryCancellation::default())?,
        prepared.evaluate(restricted, limits(), QueryCancellation::default())?
    );
    for query in [
        "SELECT ?symbol { ?symbol :name \"load_config\" }",
        "SELECT ?other { ?symbol :name \"load_config\" . ?other :name ?name }",
        "SELECT ?symbol { { ?symbol :name \"load_config\" } UNION { ?symbol :name \"unrelated\" } }",
        "SELECT ?symbol { ?symbol :name ?name FILTER(?name = \"load_config\") }",
    ] {
        assert!(
            PreparedQuery::parse(query, limits())?
                .requirements()
                .symbol_name
                .is_none()
        );
    }
    let query = format!(
        "SELECT ?usage ?start {{ <{}> :usages ?usage . ?usage :start ?start }}",
        identity("seed")
    );
    let prepared = PreparedQuery::parse(&query, limits())?;
    assert_eq!(
        prepared.requirements().symbol_ids.as_ref().map(Vec::len),
        Some(1)
    );
    let complete = fixture()?;
    let mut restricted = complete.clone();
    restricted.symbols.clear();
    assert_eq!(
        prepared.evaluate(complete, limits(), QueryCancellation::default())?,
        prepared.evaluate(restricted, limits(), QueryCancellation::default())?
    );
    let joined = format!(
        "SELECT ?name {{ <{}> :usages ?usage . ?usage :enclosing ?caller . ?caller :name ?name }}",
        identity("seed")
    );
    assert!(
        PreparedQuery::parse(&joined, limits())?
            .requirements()
            .symbol_ids
            .is_none()
    );
    Ok(())
}

#[test]
fn test_occurrence_query_retains_two_locations_and_one_caller() -> TestResult {
    let query = format!(
        "SELECT ?usage ?start {{ <{}> :usages ?usage . ?usage :start ?start }} ORDER BY ?start",
        identity("seed")
    );
    let prepared = PreparedQuery::parse(&query, limits())?;
    assert_eq!(prepared.requirements().occurrences.len(), 1);
    assert!(
        !prepared
            .requirements()
            .requires(FactRequirement::CompleteDomain)
    );
    let answer = rows(evaluate(&query)?)?;
    assert_eq!(answer.len(), 2);
    assert_ne!(answer[0]["usage"], answer[1]["usage"]);
    let query = format!(
        "SELECT DISTINCT ?caller {{ <{}> :usages ?usage . ?usage :enclosing ?caller }}",
        identity("seed")
    );
    assert_eq!(rows(evaluate(&query)?)?.len(), 1);
    Ok(())
}

#[test]
fn test_exact_three_calls_preserves_endpoint_also_reachable_directly() -> TestResult {
    let query = format!(
        "SELECT ?callee {{ <{}> :calls/:calls/:calls ?callee }}",
        identity("seed")
    );
    let answer = rows(evaluate(&query)?)?;
    assert_eq!(answer.len(), 3);
    assert_eq!(
        answer
            .iter()
            .filter(|row| row["callee"]
                == RdfTerm::Iri {
                    value: identity("third")
                })
            .count(),
        2
    );
    assert_eq!(
        answer
            .iter()
            .filter(|row| row["callee"]
                == RdfTerm::Iri {
                    value: identity("first")
                })
            .count(),
        1
    );
    Ok(())
}

#[test]
fn test_cycle_closure_reaches_seed_and_each_endpoint_once() -> TestResult {
    let query = format!(
        "SELECT ?callee {{ <{}> :calls+ ?callee }}",
        identity("seed")
    );
    let answer = rows(evaluate(&query)?)?;
    assert_eq!(answer.len(), 4);
    assert!(answer.iter().any(|row| row["callee"]
        == RdfTerm::Iri {
            value: identity("seed")
        }));
    assert_eq!(
        PreparedQuery::parse(&query, limits())?
            .requirements()
            .adjacency[0]
            .depth,
        None
    );
    Ok(())
}

#[test]
fn test_all_query_forms_use_library_results() -> TestResult {
    let described = PreparedQuery::parse(&format!("DESCRIBE <{}>", identity("seed")), limits())?;
    assert!(described.requirements().requires(FactRequirement::Domain));
    assert!(
        described
            .requirements()
            .requires(FactRequirement::Description)
    );
    assert!(
        !described
            .requirements()
            .requires(FactRequirement::CompleteDomain)
    );
    assert_eq!(
        evaluate("ASK { ?s :name \"load_config\" }")?,
        QueryAnswer::Boolean { value: true }
    );
    assert_eq!(
        evaluate("ASK { ?s :name \"absent\" }")?,
        QueryAnswer::Boolean { value: false }
    );
    let QueryAnswer::Graph { triples } =
        evaluate("CONSTRUCT { ?s :name ?name } WHERE { ?s :name ?name }")?
    else {
        return Err("expected constructed graph".into());
    };
    assert_eq!(triples.len(), 1);
    let QueryAnswer::Graph { triples } = evaluate(&format!("DESCRIBE <{}>", identity("seed")))?
    else {
        return Err("expected described graph".into());
    };
    assert!(!triples.is_empty());
    Ok(())
}

#[test]
fn test_typed_language_and_unbound_terms_preserve_projection() -> TestResult {
    let answer = rows(evaluate(
        "SELECT (\"bonjour\"@fr AS ?language) (42 AS ?number) ?absent WHERE {}",
    )?)?;
    assert_eq!(
        answer[0]["language"],
        RdfTerm::LanguageLiteral {
            value: "bonjour".into(),
            language: "fr".into()
        }
    );
    assert_eq!(
        answer[0]["number"],
        RdfTerm::Literal {
            value: "42".into(),
            datatype: "http://www.w3.org/2001/XMLSchema#integer".into()
        }
    );
    assert!(!answer[0].contains_key("absent"));
    Ok(())
}

#[test]
fn test_regex_substring_and_optional_projection_follow_standard_semantics() -> TestResult {
    let answer = rows(evaluate(
        "SELECT ?name ?missing { ?s :name ?name . FILTER(REGEX(?name, '^load_') && CONTAINS(?name, 'config')) OPTIONAL { ?s :absent ?missing } }",
    )?)?;
    assert_eq!(answer.len(), 1);
    assert!(!answer[0].contains_key("missing"));
    Ok(())
}

#[test]
fn test_explicit_prefix_and_base_override_inherited_context() -> TestResult {
    let answer = rows(evaluate(
        "PREFIX : <urn:custom:> BASE <https://example.org/> SELECT (<relative> AS ?iri) (:name AS ?predicate) WHERE {}",
    )?)?;
    assert_eq!(
        answer[0]["iri"],
        RdfTerm::Iri {
            value: "https://example.org/relative".into()
        }
    );
    assert_eq!(
        answer[0]["predicate"],
        RdfTerm::Iri {
            value: "urn:custom:name".into()
        }
    );
    Ok(())
}

#[test]
fn test_default_prefixes_resolve_equally() -> TestResult {
    let answer = rows(evaluate(
        "SELECT (:name AS ?empty) (r:name AS ?named) (rdf:type AS ?type) (rdfs:label AS ?label) (xsd:integer AS ?integer) WHERE {}",
    )?)?;
    assert_eq!(answer[0]["empty"], answer[0]["named"]);
    Ok(())
}

#[test]
fn test_negation_and_variable_predicates_require_semantic_domain() -> TestResult {
    let query = PreparedQuery::parse(
        "ASK { ?s :name 'load_config' FILTER NOT EXISTS { ?s :calls ?target } }",
        limits(),
    )?;
    assert!(
        query
            .requirements()
            .requires(FactRequirement::CompleteDomain)
    );
    assert!(!query.requirements().facets.is_empty());
    let query = PreparedQuery::parse("SELECT * { ?s ?p ?o }", limits())?;
    assert!(
        query
            .requirements()
            .requires(FactRequirement::CompleteDomain)
    );
    assert!(
        query
            .requirements()
            .requires(FactRequirement::AllOccurrences)
    );
    Ok(())
}

#[test]
fn test_parser_refuses_update_and_invalid_syntax() {
    assert!(PreparedQuery::parse("INSERT DATA { <urn:s> <urn:p> <urn:o> }", limits()).is_err());
    assert!(PreparedQuery::parse("SELECT", limits()).is_err());
}

#[test]
fn test_exact_query_bound_and_one_over() -> TestResult {
    let statement = "ASK {}";
    let mut accepted = limits();
    accepted.query_bytes_max = statement.len();
    PreparedQuery::parse(statement, accepted)?;
    accepted.query_bytes_max -= 1;
    let error = PreparedQuery::parse(statement, accepted)
        .err()
        .ok_or("expected query bound")?;
    assert!(error.to_string().contains("query_bytes_max"));
    Ok(())
}

#[test]
fn test_algebra_and_result_bounds_refuse_instead_of_truncating() -> TestResult {
    let mut accepted = limits();
    accepted.algebra_nodes_max = 0;
    let error = PreparedQuery::parse("ASK {}", accepted)
        .err()
        .ok_or("expected algebra bound")?;
    assert!(error.to_string().contains("algebra_nodes_max"));
    let query = PreparedQuery::parse("SELECT ?s { ?s :calls ?o }", limits())?;
    accepted = limits();
    accepted.result_items_max = 1;
    let error = query
        .evaluate(fixture()?, accepted, QueryCancellation::default())
        .err()
        .ok_or("expected result bound")?;
    assert!(error.to_string().contains("result_items_max"));
    accepted = limits();
    accepted.result_bytes_max = 1;
    let error = query
        .evaluate(fixture()?, accepted, QueryCancellation::default())
        .err()
        .ok_or("expected result byte bound")?;
    assert!(error.to_string().contains("result_bytes_max"));
    Ok(())
}

#[test]
fn test_cancellation_reaches_library_evaluation() -> TestResult {
    let cancellation = QueryCancellation::default();
    cancellation.cancel();
    let query = PreparedQuery::parse("SELECT * { ?s ?p ?o }", limits())?;
    let error = query
        .evaluate(fixture()?, limits(), cancellation)
        .err()
        .ok_or("expected cancellation")?;
    assert_eq!(
        error.slug(),
        rift_error::errors::server::read_cancelled::SLUG
    );
    Ok(())
}

#[test]
fn test_limit_offset_and_count_keep_distinct_meaning() -> TestResult {
    let query = format!(
        "SELECT (COUNT(?usage) AS ?count) {{ <{}> :usages ?usage }} LIMIT 1",
        identity("seed")
    );
    let answer = rows(evaluate(&query)?)?;
    assert_eq!(
        answer[0]["count"],
        RdfTerm::Literal {
            value: "2".into(),
            datatype: "http://www.w3.org/2001/XMLSchema#integer".into()
        }
    );
    assert_eq!(
        rows(evaluate(
            "SELECT ?s { ?s :calls ?o } ORDER BY ?s OFFSET 4 LIMIT 1"
        )?)?
        .len(),
        1
    );
    Ok(())
}

#[test]
fn test_inverse_and_mixed_paths_match_complete_graph_oracle() -> TestResult {
    for query in [
        format!("SELECT ?caller {{ ?caller :calls <{}> }}", identity("seed")),
        format!(
            "SELECT ?caller {{ <{}> ^:calls ?caller }}",
            identity("seed")
        ),
    ] {
        let answer = rows(evaluate(&query)?)?;
        assert_eq!(answer.len(), 1);
        assert_eq!(
            answer[0]["caller"],
            RdfTerm::Iri {
                value: identity("third")
            }
        );
    }
    let query = format!(
        "SELECT ?peer {{ <{}> :calls/^:calls ?peer }}",
        identity("seed")
    );
    let prepared = PreparedQuery::parse(&query, limits())?;
    assert!(
        prepared
            .requirements()
            .requires(FactRequirement::CompleteDomain)
    );
    let answer = rows(evaluate(&query)?)?;
    assert_eq!(answer.len(), 3);
    assert_eq!(
        answer
            .iter()
            .filter(|row| row["peer"]
                == RdfTerm::Iri {
                    value: identity("seed")
                })
            .count(),
        2
    );
    assert_eq!(
        answer
            .iter()
            .filter(|row| row["peer"]
                == RdfTerm::Iri {
                    value: identity("second")
                })
            .count(),
        1
    );
    Ok(())
}

#[test]
fn test_unestablished_symbol_has_blank_identity_and_extensions_remain_json() -> TestResult {
    let mut facts = fixture()?;
    let mut symbol = facts.symbols.remove(0);
    symbol.symbol.id = None;
    symbol.symbol.extensions = serde_json::from_value(serde_json::json!({
        "example.org.fact": {"version":1, "data":{"count":3,"name":"opaque"}}
    }))?;
    facts.symbols = vec![symbol];
    let prepared = PreparedQuery::parse(
        "SELECT ?symbol ?extension { ?symbol a :Symbol; :name \"load_config\"; :extensions ?extension }",
        limits(),
    )?;
    let answer = rows(prepared.evaluate(facts, limits(), QueryCancellation::default())?)?;
    assert_eq!(answer.len(), 1);
    assert!(matches!(answer[0]["symbol"], RdfTerm::BlankNode { .. }));
    let RdfTerm::Literal { value, datatype } = &answer[0]["extension"] else {
        return Err("extension must retain typed JSON".into());
    };
    assert_eq!(datatype, "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(value)?,
        serde_json::json!({"example.org.fact":{"version":1,"data":{"count":3,"name":"opaque"}}})
    );
    Ok(())
}

#[test]
fn test_physical_usage_identity_is_shared_across_relationship_facets() -> TestResult {
    let mut facts = fixture()?;
    let mut same_location = facts.occurrences[0].clone();
    same_location.facet = serde_json::from_value(serde_json::json!("calls"))?;
    facts.occurrences.push(same_location);
    let query = PreparedQuery::parse(
        &format!(
            "SELECT (COUNT(?usage) AS ?count) {{ <{}> :usages ?usage }}",
            identity("seed")
        ),
        limits(),
    )?;
    let answer = rows(query.evaluate(facts, limits(), QueryCancellation::default())?)?;
    let RdfTerm::Literal { value, .. } = &answer[0]["count"] else {
        return Err("usage count must be a typed number".into());
    };
    assert_eq!(value, "2");
    Ok(())
}

#[test]
fn test_ten_thousand_and_one_usage_fields_keep_exact_admission_bounds() -> TestResult {
    let mut facts = QueryFacts::default();
    for index in 0..10_001 {
        facts.occurrences.push(usage("seed", index * 3)?);
    }
    let query = format!(
        "SELECT ?usage ?unit ?start ?end {{ <{}> :usages ?usage . ?usage :unit ?unit; :start ?start; :end ?end }}",
        identity("seed")
    );
    let prepared = PreparedQuery::parse(&query, limits())?;
    let mut exact = limits();
    exact.quads_max = 10_001;
    exact.result_items_max = 10_001;
    assert_eq!(
        rows(prepared.evaluate(facts.clone(), exact, QueryCancellation::default())?)?.len(),
        10_001
    );
    let mut below = exact;
    below.quads_max = 10_002;
    below.result_items_max = 10_002;
    assert_eq!(
        rows(prepared.evaluate(facts.clone(), below, QueryCancellation::default())?)?.len(),
        10_001
    );
    let mut over = exact;
    over.quads_max = 10_000;
    assert!(
        prepared
            .evaluate(facts.clone(), over, QueryCancellation::default())
            .is_err()
    );
    over = exact;
    over.result_items_max = 10_000;
    assert!(
        prepared
            .evaluate(facts, over, QueryCancellation::default())
            .is_err()
    );
    Ok(())
}

#[test]
fn test_parsed_finite_call_and_inverse_reference_chains_acquire_only_the_anchored_frontier()
-> TestResult {
    for (path, direction) in [
        (":calls/:calls/:calls", "outgoing"),
        ("^:references/^:references", "incoming"),
    ] {
        let query = format!(
            "SELECT ?symbol ?name {{ <{}> {path} ?symbol . ?symbol :name ?name }}",
            identity("seed")
        );
        let prepared = PreparedQuery::parse(&query, limits())?;
        assert!(
            !prepared
                .requirements()
                .requires(FactRequirement::CompleteDomain),
            "{}",
            prepared.expanded()
        );
        assert_eq!(prepared.requirements().adjacency.len(), 1);
        let requirement = &prepared.requirements().adjacency[0];
        assert_eq!(requirement.seed.as_str(), identity("seed"));
        assert_eq!(
            serde_json::to_value(requirement.direction)?,
            serde_json::json!(direction)
        );
        assert_eq!(requirement.facets.len(), 1);
        assert_eq!(
            requirement.depth,
            Some(if direction == "outgoing" { 3 } else { 2 })
        );
    }
    Ok(())
}

#[test]
fn test_finite_chain_planning_preserves_branch_cycle_and_duplicate_constraints() -> TestResult {
    for steps in [2, 3] {
        let path = std::iter::repeat_n(":calls", steps)
            .collect::<Vec<_>>()
            .join("/");
        let statement = format!("SELECT ?callee {{ <{}> {path} ?callee }}", identity("seed"));
        let prepared = PreparedQuery::parse(&statement, limits())?;
        assert!(
            !prepared
                .requirements()
                .requires(FactRequirement::CompleteDomain)
        );
        assert_eq!(
            prepared.requirements().adjacency[0].depth,
            Some(u32::try_from(steps)?)
        );
        let selected = fixture()?;
        let mut complete = selected.clone();
        complete
            .relationships
            .push(edge("unrelated", "first", 1000)?);
        assert_eq!(
            prepared.evaluate(selected, limits(), QueryCancellation::default())?,
            prepared.evaluate(complete, limits(), QueryCancellation::default())?
        );
    }
    let seed = identity("seed");
    for body in [
        format!("<{seed}> :calls ?one . ?one :references ?two"),
        format!("<{seed}> :calls ?one . ?two :calls ?one"),
        format!("<{seed}> :calls ?one . ?one :calls ?two . ?one :calls ?two"),
        format!(
            "<{seed}> :calls ?one . ?one :calls ?two . <{}> :calls ?other",
            identity("unrelated")
        ),
        format!("<{seed}> :calls ?one . <{seed}> :calls ?other . ?one :calls ?two"),
    ] {
        let statement = format!("SELECT * {{ {body} }}");
        assert!(
            PreparedQuery::parse(&statement, limits())?
                .requirements()
                .requires(FactRequirement::CompleteDomain),
            "{statement}"
        );
    }
    let cycle = PreparedQuery::parse(
        &format!("SELECT ?middle {{ <{seed}> :calls ?middle . ?middle :calls <{seed}> }}"),
        limits(),
    )?;
    assert!(
        !cycle
            .requirements()
            .requires(FactRequirement::CompleteDomain)
    );
    assert_eq!(cycle.requirements().adjacency[0].depth, Some(2));
    assert_eq!(
        rows(cycle.evaluate(fixture()?, limits(), QueryCancellation::default())?)?.len(),
        1
    );
    Ok(())
}
#[test]
fn test_uniform_inverse_path_alternatives_preserve_direction_and_maximum_depth() -> TestResult {
    for (path, depth) in [
        ("(^:references|^:references/^:references)", Some(2)),
        (
            "^(:references|:references/:references/:references)",
            Some(3),
        ),
        ("(^:references|^:references+)", None),
    ] {
        let statement = format!("SELECT ?symbol {{ <{}> {path} ?symbol }}", identity("seed"));
        let prepared = PreparedQuery::parse(&statement, limits())?;
        let requirements = prepared.requirements();
        assert!(
            !requirements.requires(FactRequirement::CompleteDomain),
            "{}",
            prepared.expanded()
        );
        assert_eq!(requirements.adjacency.len(), 1);
        assert_eq!(requirements.adjacency[0].depth, depth);
        assert_eq!(
            serde_json::to_value(requirements.adjacency[0].direction)?,
            "incoming"
        );
        assert_eq!(
            serde_json::to_value(&requirements.adjacency[0].facets)?,
            serde_json::json!(["references"])
        );
    }
    for path in ["(:references|^:references)", "(^:references|^:calls)"] {
        let statement = format!("SELECT ?symbol {{ <{}> {path} ?symbol }}", identity("seed"));
        assert!(
            PreparedQuery::parse(&statement, limits())?
                .requirements()
                .requires(FactRequirement::CompleteDomain)
        );
    }
    Ok(())
}
#[test]
fn test_description_uses_captured_published_records_without_implicit_semantic_acquisition()
-> TestResult {
    let prepared = PreparedQuery::parse(&format!("DESCRIBE <{}>", identity("seed")), limits())?;
    let required = prepared.requirements();
    assert!(
        required.requires(FactRequirement::Description)
            && required.requires(FactRequirement::Domain)
            && required.requires(FactRequirement::Source)
    );
    assert!(
        !required.requires(FactRequirement::CompleteDomain)
            && !required.requires(FactRequirement::AllOccurrences)
    );
    assert!(required.adjacency.is_empty() && required.occurrences.is_empty());
    let answer = prepared.evaluate(fixture()?, limits(), QueryCancellation::default())?;
    let QueryAnswer::Graph { triples } = answer else {
        return Err("description must return graph triples".into());
    };
    assert!(triples.iter().any(|triple| triple.subject
        == RdfTerm::Iri {
            value: identity("seed")
        }
        && triple.predicate
            == RdfTerm::Iri {
                value: format!("{VOCABULARY}calls")
            }
        && triple.object
            == RdfTerm::Iri {
                value: identity("first")
            }));
    let semantic = PreparedQuery::parse(
        &format!(
            "DESCRIBE ?callee {{ <{}> :calls ?callee }}",
            identity("seed")
        ),
        limits(),
    )?;
    assert!(
        semantic
            .requirements()
            .requires(FactRequirement::Description)
    );
    assert_eq!(semantic.requirements().adjacency.len(), 1);
    let generic =
        PreparedQuery::parse("DESCRIBE ?symbol { ?symbol ?predicate ?object }", limits())?;
    assert!(
        generic
            .requirements()
            .requires(FactRequirement::CompleteDomain)
    );
    assert!(
        generic.requirements().requires(FactRequirement::Nodes)
            && generic.requirements().requires(FactRequirement::History)
            && generic
                .requirements()
                .requires(FactRequirement::Documentation)
    );
    Ok(())
}

#[test]
fn test_graph_selection_preserves_default_named_and_invalid_terms() -> TestResult {
    use oxigraph::model::{Literal, NamedNode, Term};
    use spareval::QueryableDataset;

    let dataset =
        super::dataset::VirtualDataset::new(fixture()?, limits(), QueryCancellation::default().0)?;
    let default = dataset
        .internal_quads_for_pattern(None, None, None, Some(None))
        .collect::<Result<Vec<_>, _>>()?;
    let named = dataset
        .internal_quads_for_pattern(None, None, None, None)
        .collect::<Result<Vec<_>, _>>()?;
    let graph = Term::NamedNode(NamedNode::new(super::LOCAL_GRAPH)?);
    assert!(!default.is_empty());
    assert!(default.iter().all(|quad| quad.graph_name.is_none()));
    assert_eq!(default.len(), named.len());
    assert!(
        named
            .iter()
            .all(|quad| quad.graph_name.as_ref() == Some(&graph))
    );

    let selected = dataset
        .internal_quads_for_pattern(None, None, None, Some(Some(&graph)))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(selected.len(), named.len());
    let invalid = Term::Literal(Literal::from("invalid graph"));
    assert_eq!(
        dataset
            .internal_quads_for_pattern(None, None, None, Some(Some(&invalid)))
            .count(),
        0,
    );
    Ok(())
}

#[test]
fn test_dataset_clause_and_variable_graph_keep_library_selection() -> TestResult {
    let graph = super::LOCAL_GRAPH;
    let statement = format!("SELECT ?symbol FROM <{graph}> {{ ?symbol :name \"load_config\" }}");
    let selected = rows(evaluate(&statement)?)?;
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0]["symbol"],
        RdfTerm::Iri {
            value: identity("seed")
        }
    );

    let selected = rows(evaluate(
        "SELECT ?symbol ?graph { GRAPH ?graph { ?symbol :name \"load_config\" } }",
    )?)?;
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0]["graph"],
        RdfTerm::Iri {
            value: graph.to_owned()
        }
    );
    assert!(rows(evaluate(
        "SELECT ?symbol { VALUES ?graph { \"invalid graph\" } GRAPH ?graph { ?symbol :name \"load_config\" } }",
    )?)?.is_empty());
    Ok(())
}
