use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageImportRoot,
    PackageImportRootOrigin, PackageSource,
};
use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::index::PackagePublication;
use rift_protocol::producer::{FactKind, ProducerFacts};
use rift_protocol::read::{Language, PackageIdentity, SourceUnitId};
use rift_syntax::{ByteRange, NodeMatch, SyntaxLimits};
use serde_json::{Value, json};

use super::{PublicationNodes, accept_producer_facts};
use crate::located_node::NodeAddress;

const SOURCE: &str =
    "def echo(value):\n    return value\n\ndef shout(value):\n    return echo(value)\n";
const ECHO: &str = "rift://symbol/pypi/pypi.org/click/python/click/core/echo";
const SHOUT: &str = "rift://symbol/pypi/pypi.org/click/python/click/core/shout";
/// A plain-text unit, which no syntax provider parses.
const NOTES: &str = "echo returns its value\n";
/// The `echo` call inside `shout`.
const CALL: (u64, u64) = (64, 68);

fn python() -> Language {
    Language {
        name: "python".to_owned(),
        dialect: None,
    }
}

fn click() -> PackagePublication {
    let package = PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "click".to_owned(),
        version: "8.3.3".to_owned(),
    };
    let owner = package.owner().expect("canonical package");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("authored package");
    let path = ProjectPath::new("click/core.py".to_owned()).expect("package path");
    let notes = ProjectPath::new("click/notes.txt".to_owned()).expect("notes path");
    let sources = [
        PackageSource::new(&path, SOURCE),
        PackageSource::new(&notes, NOTES),
    ];
    let roots = [PackageImportRoot::new(
        None,
        vec!["click".to_owned()],
        PackageImportRootOrigin::Wheel,
    )
    .expect("import root")];
    let language = python();
    let limits = ExactPackageLimits::new(
        2,
        u64::try_from(SOURCE.len() + NOTES.len()).expect("length"),
    );
    let input = ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
        .expect("bounded input")
        .with_import_roots(&roots)
        .expect("accepted placement");
    PackageAnalyzer::analyze(input, 1)
        .expect("analyzed package")
        .publication()
        .clone()
}

fn unit(publication: &PackagePublication) -> SourceUnitId {
    publication.units[0].unit.clone()
}

fn facts(publication: &PackagePublication, body: Value) -> ProducerFacts {
    let mut facts = json!({
        "producer": {"name": "demo-indexer", "revision": "0.6.1"},
        "language": "python",
        "kinds": ["relationships", "occurrences"],
    });
    let Value::Object(lists) = body else {
        panic!("fact lists must be an object: {body}");
    };
    for (key, value) in lists {
        facts[key] = value;
    }
    let text = facts
        .to_string()
        .replace("UNIT", publication.units[0].unit.0.as_str());
    serde_json::from_str(&text).expect("producer facts")
}

fn call_facts(publication: &PackagePublication) -> ProducerFacts {
    facts(
        publication,
        json!({
            "relationships": [{
                "kind": "call",
                "facets": ["calls"],
                "to": ECHO,
                "occurrence": {"unit": "UNIT", "range": {"start": CALL.0, "end": CALL.1}},
                "derivation": "resolution",
            }, {
                "from": SHOUT,
                "kind": "reference",
                "facets": ["references"],
                "to": ECHO,
                "derivation": "heuristic",
                "confidence": 0.5,
            }],
            "occurrences": [{
                "symbol": ECHO,
                "unit": "UNIT",
                "range": {"start": CALL.0, "end": CALL.1},
            }, {
                "symbol": ECHO,
                "unit": "UNIT",
                "range": {"start": 4, "end": 8},
            }],
        }),
    )
}

fn refusal(publication: PackagePublication, facts: ProducerFacts) -> String {
    accept_producer_facts(publication, facts, SyntaxLimits::default())
        .expect_err("refused facts")
        .to_string()
}

#[test]
fn a_range_resolves_to_its_place_inside_the_enclosing_declaration() {
    let publication = click();
    let mut nodes = PublicationNodes::new(&publication, SyntaxLimits::default());
    let call = ByteRange {
        start: CALL.0,
        end: CALL.1,
    };
    let located = nodes
        .node_at(&unit(&publication), call, NodeMatch::Exact)
        .expect("resolved call");
    assert_eq!(located.range, call);
    assert!(
        matches!(
            &located.address,
            NodeAddress::Declaration { symbol, declaration: 0, index } if symbol.as_str() == SHOUT && *index > 0
        ),
        "{located:?}"
    );
    let echo = nodes
        .node_at(
            &unit(&publication),
            ByteRange { start: 0, end: 16 },
            NodeMatch::Enclosing,
        )
        .expect("resolved declaration");
    assert!(
        matches!(&echo.address, NodeAddress::Declaration { symbol, index: 0, .. } if symbol.as_str() == ECHO),
        "{echo:?}"
    );
    let between = nodes
        .node_at(
            &unit(&publication),
            ByteRange { start: 34, end: 35 },
            NodeMatch::Enclosing,
        )
        .expect("resolved module");
    assert!(
        matches!(between.address, NodeAddress::Unit { .. }),
        "{between:?}"
    );
    let text_unit = publication
        .units
        .iter()
        .find(|record| record.unit.0.ends_with("notes.txt"))
        .map(|record| record.unit.clone())
        .expect("notes unit");
    let file = nodes
        .node_at(
            &text_unit,
            ByteRange { start: 0, end: 4 },
            NodeMatch::Enclosing,
        )
        .expect("a text unit answers its file node");
    assert_eq!(file.address, NodeAddress::Unit { index: 0 });
    assert!(
        format!("{nodes:?}").contains("units_parsed: 2"),
        "{nodes:?}"
    );
}

#[test]
fn a_range_that_resolves_to_nothing_is_refused() {
    let publication = click();
    let mut nodes = PublicationNodes::new(&publication, SyntaxLimits::default());
    let past = u64::try_from(SOURCE.len()).expect("length") + 1;
    let outside = nodes
        .node_at(
            &unit(&publication),
            ByteRange {
                start: 0,
                end: past,
            },
            NodeMatch::Enclosing,
        )
        .expect_err("range past the unit");
    assert!(
        outside
            .to_string()
            .contains("resolves to no node of its source unit"),
        "{outside}"
    );
    let missing = SourceUnitId(format!("{}.missing", unit(&publication).0));
    let absent = nodes
        .node_at(
            &missing,
            ByteRange { start: 0, end: 1 },
            NodeMatch::Enclosing,
        )
        .expect_err("unit the publication does not carry");
    assert!(absent.to_string().contains(".missing"), "{absent}");
    let mut cut = publication.clone();
    cut.units[0].source_complete = false;
    let mut nodes = PublicationNodes::new(&cut, SyntaxLimits::default());
    assert!(
        nodes
            .node_at(
                &unit(&cut),
                ByteRange { start: 0, end: 1 },
                NodeMatch::Enclosing
            )
            .is_err(),
        "a cut source resolves nothing"
    );
}

#[test]
fn producer_facts_enter_the_publication_under_their_producer() {
    let publication = click();
    let before = publication.relationships.len();
    let facts = call_facts(&publication);
    let producer = facts.producer.clone();
    let accepted =
        accept_producer_facts(publication, facts, SyntaxLimits::default()).expect("accepted");
    let added = &accepted.relationships[before..];
    assert_eq!(added.len(), 2);
    assert_eq!(
        added[0].from.as_str(),
        SHOUT,
        "range resolved to its declaration"
    );
    assert_eq!(added[1].from.as_str(), SHOUT, "a named symbol is kept");
    assert!(
        added
            .iter()
            .all(|edge| edge.producer.as_ref() == Some(&producer))
    );
    let ranges = accepted
        .occurrences
        .iter()
        .map(|occurrence| (occurrence.range.start, occurrence.range.end))
        .collect::<Vec<_>>();
    assert_eq!(
        ranges,
        [(4, 8), CALL],
        "occurrences in unit and range order"
    );
    assert!(
        accepted
            .occurrences
            .iter()
            .all(|occurrence| occurrence.producer.as_ref() == Some(&producer))
    );
    let coverage = &accepted.coverage[0];
    assert!(coverage.covers(FactKind::Relationships, None));
    assert!(coverage.covers(FactKind::Relationships, Some(&producer)));
    assert!(coverage.covers(FactKind::Occurrences, Some(&producer)));
    accepted.validate_facts().expect("facts agree");
    let wire = serde_json::to_string(&accepted).expect("serialize");
    let read: PackagePublication = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(read, accepted);
}

#[test]
fn producer_facts_naming_a_symbol_keep_it_and_still_resolve_their_range() {
    let publication = click();
    let facts = facts(
        &publication,
        json!({
            "kinds": ["relationships"],
            "relationships": [{
                "from": SHOUT,
                "kind": "call",
                "facets": ["calls"],
                "to": ECHO,
                "occurrence": {"unit": "UNIT", "range": {"start": 0, "end": 200}},
                "derivation": "resolution",
            }],
        }),
    );
    assert!(refusal(publication, facts).contains("resolves to no node"));
    let publication = click();
    let covered_only = facts_with_kinds(&publication, &["occurrences"]);
    let accepted = accept_producer_facts(publication, covered_only, SyntaxLimits::default())
        .expect("a covered kind without facts");
    assert!(accepted.occurrences.is_empty());
    assert!(accepted.coverage[0].covers(FactKind::Occurrences, Some(&accepted_producer())));
}

fn accepted_producer() -> rift_protocol::producer::FactProducer {
    serde_json::from_value(json!({"name": "demo-indexer", "revision": "0.6.1"})).expect("producer")
}

fn facts_with_kinds(publication: &PackagePublication, kinds: &[&str]) -> ProducerFacts {
    facts(publication, json!({"kinds": kinds}))
}

#[test]
fn producer_facts_outside_their_coverage_or_address_are_refused() {
    let cases = [
        (
            json!({"language": "javascript"}),
            "field language, violation uncovered",
        ),
        (
            json!({"kinds": ["occurrences"], "relationships": [{
                "from": SHOUT, "kind": "call", "facets": ["calls"], "to": ECHO,
                "derivation": "resolution",
            }]}),
            "field kinds, violation uncovered",
        ),
        (
            json!({"kinds": ["occurrences", "occurrences"]}),
            "field kinds, violation duplicate",
        ),
        (
            json!({"relationships": [{
                "kind": "call", "facets": ["calls"], "to": ECHO, "derivation": "resolution",
            }]}),
            "field relationships.from, violation missing",
        ),
        (
            json!({"relationships": [{
                "kind": "call", "facets": ["calls"], "to": ECHO, "derivation": "resolution",
                "occurrence": {"unit": "UNIT", "range": {"start": 34, "end": 35}},
            }]}),
            "field relationships.occurrence, violation outside_declaration",
        ),
    ];
    for (body, expected) in cases {
        let publication = click();
        let facts = facts(&publication, body);
        let message = refusal(publication, facts);
        assert!(message.contains(expected), "{expected}: {message}");
    }
    let publication = click();
    let once = accept_producer_facts(
        publication,
        facts_with_kinds(&click(), &["occurrences"]),
        SyntaxLimits::default(),
    )
    .expect("first acceptance");
    let again = facts_with_kinds(&once, &["occurrences"]);
    assert!(refusal(once, again).contains("field kinds, violation duplicate"));
}
