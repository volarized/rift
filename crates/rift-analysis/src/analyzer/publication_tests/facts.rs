//! A publication states which fact kinds it covers, and its facts agree with that coverage.

use rift_protocol::index::PackageOccurrence;
use rift_protocol::index::StoredEdge;
use rift_protocol::producer::{FactCoverage, FactKind, FactProducer};
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

fn edge(from: &str, producer: Option<FactProducer>) -> StoredEdge {
    let mut edge: StoredEdge = serde_json::from_value(json!({
        "from": from,
        "kind": "calls",
        "kinds": ["calls"],
        "to": ECHO,
        "provider": "static", "derivation": "resolution",
    }))
    .expect("edge");
    edge.producer = producer;
    edge
}

fn cover(publication: &mut PackagePublication, kind: FactKind) {
    publication.coverage[0].facts.push(FactCoverage {
        kind,
        producer: Some(producer()),
    });
}

/// Inserts a producer occurrence among the already recorded static evidence.
fn add_producer_occurrence(publication: &mut PackagePublication) {
    let added = occurrence(publication, 64, 68);
    let position = publication.occurrences.partition_point(|old| {
        (
            old.unit.as_str(),
            old.range.start,
            old.range.end,
            old.symbol.as_str(),
            old.producer.as_ref(),
        ) < (
            added.unit.as_str(),
            added.range.start,
            added.range.end,
            added.symbol.as_str(),
            added.producer.as_ref(),
        )
    });
    publication.occurrences.insert(position, added);
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
fn built_in_publication_covers_its_edges_and_no_occurrences() {
    let publication = publication(
        &[("click/core.py", "def echo(value):\n    return 1\n")],
        None,
        &["click"],
    );
    publication.validate_facts().expect("built-in facts agree");
    for coverage in &publication.coverage {
        assert!(coverage.covers(FactKind::Edges, None));
        assert!(!coverage.covers(FactKind::Occurrences, None));
        assert!(!coverage.covers(FactKind::Edges, Some(&producer())));
    }
    assert!(publication.occurrences.is_empty());
    let wire = serde_json::to_value(&publication).expect("serialize");
    assert!(wire.get("occurrences").is_none(), "no occurrences, no key");
    assert_eq!(wire["coverage"][0]["facts"], json!([{"kind": "edges"}]));
}

#[test]
fn built_in_publication_covers_the_exact_parameter_read() {
    let source = "def echo(value):\n    return value\n";
    let publication = publication(&[("click/core.py", source)], None, &["click"]);
    publication.validate_facts().expect("built-in facts agree");
    let [read] = publication.occurrences.as_slice() else {
        panic!(
            "a single physical parameter read: {:?}",
            publication.occurrences
        );
    };
    assert!(read.producer.is_none());
    assert_eq!(read.symbol.as_str(), format!("{ECHO}/value"));
    let start = u64::try_from(source.rfind("value").expect("authored read")).expect("offset");
    assert_eq!(read.range.start, start);
    assert_eq!(read.range.end, start + 5);
    assert!(publication.coverage[0].covers(FactKind::Occurrences, None));
    assert!(!publication.coverage[0].covers(FactKind::Occurrences, Some(&producer())));
}

#[test]
fn producer_facts_survive_a_round_trip_once_their_coverage_names_them() {
    let mut publication = click();
    add_producer_occurrence(&mut publication);
    publication.edges.push(edge(ECHO, Some(producer())));
    cover(&mut publication, FactKind::Occurrences);
    cover(&mut publication, FactKind::Edges);
    publication.validate_facts().expect("covered facts agree");
    let wire = serde_json::to_string(&publication).expect("serialize");
    let read: PackagePublication = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(read, publication);
    assert_eq!(
        read.edges.last().and_then(|edge| edge.producer.clone()),
        Some(producer())
    );
}

#[test]
fn uncovered_producer_facts_are_refused() {
    let mut occurrences = click();
    add_producer_occurrence(&mut occurrences);
    assert!(
        refusal(&occurrences).contains("field occurrences, violation uncovered"),
        "{}",
        refusal(&occurrences)
    );
    let mut edges = click();
    edges.edges.push(edge(ECHO, Some(producer())));
    assert!(refusal(&edges).contains("field edges, violation uncovered"));
    let mut malformed = click();
    let mut unparsed = edge(ECHO, None);
    unparsed.from = rift_protocol::read::SymbolId("rift://symbol/".to_owned());
    malformed.edges.push(unparsed);
    assert!(refusal(&malformed).contains("field edges.from, violation symbol"));
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
        changed.occurrences = vec![fact];
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
