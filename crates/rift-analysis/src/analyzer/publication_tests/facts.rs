//! A publication states which fact kinds it covers, and its facts agree with that coverage.

use rift_protocol::index::PackageOccurrence;
use rift_protocol::producer::{FactCoverage, FactKind, FactProducer};
use rift_protocol::read::Relationship;
use serde_json::json;

use super::publication;
use crate::analyzer::PackagePublication;

const ECHO: &str = "rift://symbol/pypi/pypi.org/click/python/click/core/echo";

fn click() -> PackagePublication {
    publication(
        &[(
            "click/core.py",
            "def echo(value):\n    return value\n\ndef shout(value):\n    return echo(value)\n",
        )],
        None,
        &["click"],
    )
}

fn producer() -> FactProducer {
    serde_json::from_value(json!({"name": "demo-indexer", "revision": "0.6.1"})).expect("producer")
}

fn occurrence(publication: &PackagePublication, start: u64, end: u64) -> PackageOccurrence {
    PackageOccurrence {
        symbol: serde_json::from_value(json!(ECHO)).expect("symbol"),
        unit: publication.units[0].unit.clone(),
        range: rift_protocol::read::TextRange { start, end },
        producer: Some(producer()),
    }
}

fn relationship(from: &str, producer: Option<FactProducer>) -> Relationship {
    let mut relationship: Relationship = serde_json::from_value(json!({
        "from": from,
        "kind": "calls",
        "facets": ["calls"],
        "to": ECHO,
        "derivation": "resolution",
    }))
    .expect("relationship");
    relationship.producer = producer;
    relationship
}

fn cover(publication: &mut PackagePublication, kind: FactKind) {
    publication.coverage[0].facts.push(FactCoverage {
        kind,
        producer: Some(producer()),
    });
}

/// One change to an occurrence, given its unit's length.
type OccurrenceChange = fn(&mut PackageOccurrence, u64);

fn refusal(publication: &PackagePublication) -> String {
    publication
        .validate_facts()
        .expect_err("refused facts")
        .to_string()
}

#[test]
fn built_in_publication_covers_its_relationships_and_no_occurrences() {
    let publication = click();
    publication.validate_facts().expect("built-in facts agree");
    for coverage in &publication.coverage {
        assert!(coverage.covers(FactKind::Relationships, None));
        assert!(!coverage.covers(FactKind::Occurrences, None));
        assert!(!coverage.covers(FactKind::Relationships, Some(&producer())));
    }
    assert!(publication.occurrences.is_empty());
    let wire = serde_json::to_value(&publication).expect("serialize");
    assert!(wire.get("occurrences").is_none(), "no occurrences, no key");
    assert_eq!(
        wire["coverage"][0]["facts"],
        json!([{"kind": "relationships"}])
    );
}

#[test]
fn producer_facts_survive_a_round_trip_once_their_coverage_names_them() {
    let mut publication = click();
    publication
        .occurrences
        .push(occurrence(&publication, 64, 68));
    publication
        .relationships
        .push(relationship(ECHO, Some(producer())));
    cover(&mut publication, FactKind::Occurrences);
    cover(&mut publication, FactKind::Relationships);
    publication.validate_facts().expect("covered facts agree");
    let wire = serde_json::to_string(&publication).expect("serialize");
    let read: PackagePublication = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(read, publication);
    assert_eq!(
        read.relationships
            .last()
            .and_then(|edge| edge.producer.clone()),
        Some(producer())
    );
}

#[test]
fn uncovered_producer_facts_are_refused() {
    let mut occurrences = click();
    occurrences
        .occurrences
        .push(occurrence(&occurrences, 64, 68));
    assert!(
        refusal(&occurrences).contains("field occurrences, violation uncovered"),
        "{}",
        refusal(&occurrences)
    );
    let mut relationships = click();
    relationships
        .relationships
        .push(relationship(ECHO, Some(producer())));
    assert!(refusal(&relationships).contains("field relationships, violation uncovered"));
    let mut malformed = click();
    let mut unparsed = relationship(ECHO, None);
    unparsed.from = rift_protocol::read::SymbolId("rift://symbol/".to_owned());
    malformed.relationships.push(unparsed);
    assert!(refusal(&malformed).contains("field relationships.from, violation symbol"));
}

#[test]
fn occurrences_outside_their_units_or_order_are_refused() {
    let mut publication = click();
    cover(&mut publication, FactKind::Occurrences);
    let length = u64::try_from(publication.units[0].source.len()).expect("length");
    let cases: [(OccurrenceChange, &str); 2] = [
        (
            |occurrence, _| occurrence.unit.0.push_str(".missing"),
            "field occurrences.unit, violation unit",
        ),
        (
            |occurrence, length| occurrence.range.end = length + 1,
            "field occurrences.range, violation range",
        ),
    ];
    for (change, expected) in cases {
        let mut changed = publication.clone();
        let mut fact = occurrence(&changed, 0, 3);
        change(&mut fact, length);
        changed.occurrences.push(fact);
        assert!(
            refusal(&changed).contains(expected),
            "{}",
            refusal(&changed)
        );
    }
    let mut reversed = publication.clone();
    reversed.occurrences = vec![occurrence(&reversed, 4, 8), occurrence(&reversed, 0, 3)];
    assert!(refusal(&reversed).contains("field occurrences, violation order"));
    let mut repeated = publication;
    repeated.occurrences = vec![occurrence(&repeated, 0, 3), occurrence(&repeated, 0, 3)];
    assert!(refusal(&repeated).contains("violation order"));
}

#[test]
fn a_repeated_coverage_entry_is_refused() {
    let mut publication = click();
    cover(&mut publication, FactKind::Occurrences);
    cover(&mut publication, FactKind::Occurrences);
    assert!(refusal(&publication).contains("field coverage.facts, violation duplicate"));
}
