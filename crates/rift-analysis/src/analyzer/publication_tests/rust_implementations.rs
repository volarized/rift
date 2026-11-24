//! Publication of Rust implementations written outside their type's module (#775).

use super::*;

const ROOT: &str = "rift://symbol/cargo/crates.io/beacon/rust/beacon";
const LIB: &str = "use std::fmt;\npub struct Version { pub major: u64 }\npub trait Parse { fn parse(text: &str) -> Self; }\npub fn leaf() {}\nmod display;\nmod parse;\nmod jobs;\n";
const DISPLAY: &str = "use crate::Version;\nuse core::fmt;\nimpl fmt::Display for Version { fn fmt(&self, _f: &mut fmt::Formatter) -> fmt::Result { self.helper(); Ok(()) } }\n";
const PARSE: &str = "use crate::{Parse, Version as Parsed};\nimpl Parse for Parsed { fn parse(_text: &str) -> Self { Parsed { major: 1 } } }\n";
const JOBS: &str = "use super::{leaf, Version};\nimpl Version { pub fn helper(&self) -> u64 { leaf(); self.major } }\n";

#[test]
fn implementations_are_facets_of_the_imported_type() {
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::Rust,
        vec![
            ("src/lib.rs", LIB),
            ("src/display.rs", DISPLAY),
            ("src/parse.rs", PARSE),
            ("src/jobs.rs", JOBS),
        ],
        None,
        Some(("Cargo.toml", "[package]\nname='beacon'\nversion='1.0.0'\n")),
    )
    .expect("captured Rust package");
    let publication = analysis.publication();
    for identity in publication
        .objects
        .iter()
        .filter_map(|object| object.id.as_ref())
        .map(rift_protocol::read::SymbolId::as_str)
        .chain(
            publication
                .edges
                .iter()
                .flat_map(|edge| [edge.from.as_str(), edge.to.as_str()]),
        )
    {
        for moved in ["/display/Version", "/parse/Parsed", "/jobs/Version"] {
            assert!(
                !identity.contains(moved),
                "module-local identity {identity}"
            );
        }
    }
    let version = format!("{ROOT}/Version");
    let units = publication
        .declarations
        .iter()
        .filter(|declaration| declaration.symbol.as_str() == version)
        .map(|declaration| {
            declaration
                .unit
                .0
                .rsplit('/')
                .next()
                .expect("file")
                .to_owned()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        units,
        ["display.rs", "jobs.rs", "lib.rs", "parse.rs"]
            .map(str::to_owned)
            .into(),
        "the type and its implementations are facets of one symbol"
    );
    let edge = |kind: EdgeKind, from: &str, to: &str| {
        publication.edges.iter().any(|edge| {
            edge.kinds.contains(&kind)
                && edge.from.as_str() == format!("{ROOT}/{from}")
                && edge.to.as_str() == format!("{ROOT}/{to}")
        })
    };
    assert!(
        publication.edges.iter().any(|edge| {
            edge.kinds.contains(&EdgeKind::Imports)
                && edge.from.as_str() == format!("{ROOT}/display")
                && edge.to.as_str().ends_with("/core/fmt")
        }),
        "an import belongs to the module of its file"
    );
    for (kind, from, to) in [
        (EdgeKind::Contains, "Version", "Version/major"),
        (EdgeKind::Contains, "Version", "Version/helper"),
        (EdgeKind::Contains, "Version", "Version/fmt"),
        (EdgeKind::Contains, "Version", "Version/parse"),
        (EdgeKind::Implements, "Version", "Parse"),
        (EdgeKind::Overrides, "Version/parse", "Parse/parse"),
        (EdgeKind::Calls, "Version/helper", "leaf"),
        (EdgeKind::Calls, "Version/fmt", "Version/helper"),
        (EdgeKind::Reads, "Version/helper", "Version/major"),
    ] {
        assert!(edge(kind, from, to), "{kind:?} {from} -> {to}");
    }
}
