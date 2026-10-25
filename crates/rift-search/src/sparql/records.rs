//! Original records, graph adjacency and physical usages have separate identities.

use rift_protocol::query::{QueryAnswer, QueryOccurrence, RdfTerm};
use rift_protocol::relationship_read::{
    RelationshipOccurrence, RelationshipRecord, RelationshipRecordId,
};

use super::super::{PreparedQuery, QueryCancellation, QueryFacts, QueryLimits};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn limits() -> QueryLimits {
    QueryLimits {
        query_bytes_max: 4096,
        algebra_nodes_max: 1024,
        quads_max: 1000,
        rdf_bytes_max: 1 << 20,
        result_items_max: 1000,
        result_bytes_max: 1 << 20,
    }
}

fn facts() -> Result<QueryFacts, Box<dyn std::error::Error>> {
    let relationship: rift_protocol::read::Relationship = serde_json::from_value(
        serde_json::json!({
            "from":"rift://symbol/cargo/crates.io/demo@1.0.0/rust/caller",
            "to":"rift://symbol/cargo/crates.io/demo@1.0.0/rust/target",
            "kind":"call","facets":["calls"],"derivation":"resolution",
            "occurrence":{"unit":"rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs","range":{"start":0,"end":3}}
        }),
    )?;
    let mut result = QueryFacts::default();
    for suffix in ['a', 'b'] {
        result.relationship_records.push(RelationshipRecord {
            id: RelationshipRecordId::parse(&format!("12345678{}", suffix.to_string().repeat(56)))?,
            relationship: relationship.clone(),
        });
    }
    result.occurrences.push(QueryOccurrence {
        target: relationship.to.clone(),
        enclosing: Some(relationship.from.clone()),
        source: relationship.occurrence.expect("physical location"),
        facet: rift_protocol::read::RelationshipFacet::Calls,
    });
    Ok(result)
}

fn count(query: &str, facts: QueryFacts) -> Result<String, Box<dyn std::error::Error>> {
    let QueryAnswer::Bindings { bindings, .. } = PreparedQuery::parse(query, limits())?.evaluate(
        facts,
        limits(),
        QueryCancellation::default(),
    )?
    else {
        return Err("bindings required".into());
    };
    let RdfTerm::Literal { value, .. } = &bindings[0]["count"] else {
        return Err("count literal required".into());
    };
    Ok(value.clone())
}

#[test]
fn full_record_identity_preserves_parallel_records_without_duplicating_graph_edges_or_usages()
-> TestResult {
    let facts = facts()?;
    assert_eq!(
        count(
            "SELECT (COUNT(?record) AS ?count) { ?record a :Relationship }",
            facts.clone()
        )?,
        "2"
    );
    assert_eq!(
        count(
            "SELECT (COUNT(*) AS ?count) { ?from :calls ?to }",
            facts.clone()
        )?,
        "1"
    );
    assert_eq!(
        count(
            "SELECT (COUNT(?usage) AS ?count) { ?target :usages ?usage }",
            facts.clone()
        )?,
        "1"
    );
    let prepared = PreparedQuery::parse(
        "SELECT ?record ?id { ?record a :Relationship; :id ?id } ORDER BY ?id",
        limits(),
    )?;
    let first = prepared.evaluate(facts.clone(), limits(), QueryCancellation::default())?;
    assert_eq!(
        first,
        prepared.evaluate(facts, limits(), QueryCancellation::default())?
    );
    Ok(())
}

#[test]
fn replayed_record_identity_refuses_even_when_payload_changes() -> TestResult {
    for changed in [false, true] {
        let mut facts = facts()?;
        facts.relationship_records[1].id = facts.relationship_records[0].id.clone();
        if changed {
            facts.relationship_records[1].relationship.kind.0 = "different".to_owned();
        }
        assert!(
            PreparedQuery::parse("ASK {}", limits())?
                .evaluate(facts, limits(), QueryCancellation::default())
                .is_err()
        );
    }
    assert!(RelationshipRecordId::parse("12345678").is_err());
    Ok(())
}

#[test]
fn original_usage_evidence_keeps_equal_payload_ids_and_shared_physical_locations() -> TestResult {
    let mut facts = facts()?;
    let usage = facts.occurrences.pop().ok_or("physical usage")?;
    for record in &facts.relationship_records {
        facts.occurrence_records.push(RelationshipOccurrence {
            id: record.id.clone(),
            relationship: Some(record.id.clone()),
            occurrence: usage.clone(),
        });
    }
    assert!(facts.occurrences.is_empty());
    assert_eq!(
        count(
            "SELECT (COUNT(?usage) AS ?count) { ?target :usages ?usage }",
            facts.clone()
        )?,
        "1"
    );
    assert_eq!(
        count(
            "SELECT (COUNT(DISTINCT ?record) AS ?count) { ?usage :evidence ?record . ?record :id ?id }",
            facts.clone()
        )?,
        "2"
    );
    let mut another_facet = facts.occurrence_records[0].clone();
    another_facet.occurrence.facet = rift_protocol::read::RelationshipFacet::References;
    facts.occurrence_records.push(another_facet);
    assert_eq!(
        count(
            "SELECT (COUNT(?usage) AS ?count) { ?target :usages ?usage }",
            facts.clone()
        )?,
        "1"
    );
    let mut another_location = facts.occurrence_records[0].clone();
    another_location.occurrence.source.range.start = 1;
    facts.occurrence_records.push(another_location);
    assert_eq!(
        count(
            "SELECT (COUNT(?usage) AS ?count) { ?target :usages ?usage }",
            facts.clone()
        )?,
        "2"
    );
    assert_eq!(
        count(
            "SELECT (COUNT(DISTINCT ?record) AS ?count) { ?usage :evidence ?record . ?record :id ?id }",
            facts.clone()
        )?,
        "2"
    );
    let mut bounded = limits();
    bounded.rdf_bytes_max = 1;
    assert!(
        PreparedQuery::parse("ASK { ?target :usages ?usage }", bounded)?
            .evaluate(facts, bounded, QueryCancellation::default())
            .is_err()
    );
    Ok(())
}
