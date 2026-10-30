use serde_json::json;

use super::{
    FactCoverage, FactKind, FactProducer, PRODUCER_NAME_BYTES_MAX, PRODUCER_NAME_PATTERN,
    PRODUCER_REVISION_BYTES_MAX, PRODUCER_REVISION_PATTERN, ProducerName, ProducerRelationship,
    ProducerRevision,
};

fn violation(error: &rift_error::RiftError) -> String {
    error.to_string()
}

#[test]
fn test_producer_name_accepts_lowercase_names_up_to_the_bound() {
    for name in [
        "demo-indexer",
        "a",
        "rust.analyzer_2",
        &"a".repeat(PRODUCER_NAME_BYTES_MAX),
    ] {
        let parsed = ProducerName::parse(name).expect("accepted name");
        assert_eq!(parsed.as_str(), name);
        assert_eq!(parsed.to_string(), name);
    }
}

#[test]
fn test_producer_name_refuses_empty_long_and_foreign_spellings() {
    let long = "a".repeat(PRODUCER_NAME_BYTES_MAX + 1);
    for (name, expected) in [
        ("", "violation length"),
        (long.as_str(), "violation length"),
        ("Demo", "violation character"),
        ("1demo", "violation character"),
        ("demo indexer", "violation character"),
        ("demo/indexer", "violation character"),
        ("démo", "violation character"),
    ] {
        let error = ProducerName::parse(name).expect_err("refused name");
        let message = violation(&error);
        assert!(
            message.contains("field producer.name"),
            "{name:?}: {message}"
        );
        assert!(message.contains(expected), "{name:?}: {message}");
    }
}

#[test]
fn test_producer_revision_accepts_versions_and_build_suffixes() {
    let longest = "0".repeat(PRODUCER_REVISION_BYTES_MAX);
    for revision in ["0.6.1", "0.6.1+3f2a9c1", "V2-rc_1", longest.as_str()] {
        assert_eq!(
            ProducerRevision::parse(revision)
                .expect("accepted revision")
                .as_str(),
            revision
        );
    }
}

#[test]
fn test_producer_revision_refuses_empty_long_and_foreign_spellings() {
    let long = "0".repeat(PRODUCER_REVISION_BYTES_MAX + 1);
    for (revision, expected) in [
        ("", "violation length"),
        (long.as_str(), "violation length"),
        ("+1", "violation character"),
        ("1.0 beta", "violation character"),
        ("1.0@x", "violation character"),
    ] {
        let message = violation(&ProducerRevision::parse(revision).expect_err("refused"));
        assert!(message.contains("field producer.revision"), "{message}");
        assert!(message.contains(expected), "{revision:?}: {message}");
    }
}

#[test]
fn test_fact_producer_round_trips_and_deserialization_enforces_the_spelling() {
    let wire = json!({"name": "demo-indexer", "revision": "0.6.1"});
    let producer: FactProducer = serde_json::from_value(wire.clone()).expect("deserialize");
    assert_eq!(producer.name.as_str(), "demo-indexer");
    assert_eq!(serde_json::to_value(&producer).expect("serialize"), wire);
    let refused = serde_json::from_value::<FactProducer>(json!({
        "name": "Demo",
        "revision": "0.6.1",
    }))
    .expect_err("an uppercase name is refused while deserializing");
    assert!(refused.to_string().contains("producer.name"), "{refused}");
}

#[test]
fn test_fact_coverage_omits_the_built_in_producer() {
    let built_in = FactCoverage {
        kind: FactKind::Relationships,
        producer: None,
    };
    assert_eq!(
        serde_json::to_value(&built_in).expect("serialize"),
        json!({"kind": "relationships"})
    );
    let external: FactCoverage = serde_json::from_value(json!({
        "kind": "occurrences",
        "producer": {"name": "demo-indexer", "revision": "0.6.1"},
    }))
    .expect("deserialize");
    assert_eq!(external.kind, FactKind::Occurrences);
    assert!(external.producer.is_some());
}

/// The schema states each spelling's bounds and pattern through the constants the
/// classifier enforces.
#[test]
fn test_producer_schema_advertises_the_enforced_bounds_and_patterns() {
    let schema = serde_json::to_value(schemars::schema_for!(FactProducer)).expect("schema");
    for (definition, bytes_max, pattern) in [
        (
            "ProducerName",
            PRODUCER_NAME_BYTES_MAX,
            PRODUCER_NAME_PATTERN,
        ),
        (
            "ProducerRevision",
            PRODUCER_REVISION_BYTES_MAX,
            PRODUCER_REVISION_PATTERN,
        ),
    ] {
        let advertised = &schema["$defs"][definition];
        assert_eq!(advertised["maxLength"], json!(bytes_max), "{definition}");
        assert_eq!(advertised["minLength"], json!(1), "{definition}");
        assert_eq!(advertised["pattern"], json!(pattern), "{definition}");
    }
}

/// Every spelling the served schema accepts is one the classifier accepts, and every
/// spelling the classifier refuses fails the schema too.
#[test]
fn test_producer_schema_agrees_with_the_classifier() {
    let schema = serde_json::to_value(schemars::schema_for!(FactProducer)).expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("producer validator");
    let long_name = "a".repeat(PRODUCER_NAME_BYTES_MAX + 1);
    let long_revision = "0".repeat(PRODUCER_REVISION_BYTES_MAX + 1);
    let values = [
        "demo-indexer",
        "Demo",
        "1a",
        "a.b_c-d",
        "a b",
        "",
        "0.6.1+x",
        "+1",
        long_name.as_str(),
        long_revision.as_str(),
    ];
    for value in values {
        let as_name = json!({"name": value, "revision": "1"});
        assert_eq!(
            validator.is_valid(&as_name),
            ProducerName::parse(value).is_ok(),
            "name {value:?}"
        );
        let as_revision = json!({"name": "a", "revision": value});
        assert_eq!(
            validator.is_valid(&as_revision),
            ProducerRevision::parse(value).is_ok(),
            "revision {value:?}"
        );
    }
}

#[test]
fn producer_relationship_without_extensions_serializes_without_the_field() {
    let wire = json!({
        "from": "rift://symbol/pypi/pypi.org/click/python/click/core/shout",
        "kind": "call",
        "facets": ["calls"],
        "to": "rift://symbol/pypi/pypi.org/click/python/click/core/echo",
        "derivation": "resolution",
    });
    let relationship: ProducerRelationship =
        serde_json::from_value(wire.clone()).expect("deserialize");
    assert!(relationship.extensions.is_empty());
    assert_eq!(
        serde_json::to_value(&relationship).expect("serialize"),
        wire
    );
}

/// The published schema describes fact coverage as the producer of a kind's facts and makes
/// no claim that a producer found every fact of it.
#[test]
fn test_fact_coverage_schema_makes_no_completeness_claim() {
    let schema: serde_json::Value =
        serde_json::from_str(&crate::schema::package_index_schema_document()).expect("schema");
    let prose = |value: &serde_json::Value| {
        let text = value.as_str().expect("a description");
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let coverage = prose(&schema["$defs"]["FactCoverage"]["description"]);
    assert!(coverage.contains("makes no claim"), "{coverage}");
    assert!(!coverage.contains("found none"), "{coverage}");
    let facts = prose(&schema["$defs"]["PackageCoverage"]["properties"]["facts"]["description"]);
    assert!(facts.contains("no completeness claim"), "{facts}");
}
