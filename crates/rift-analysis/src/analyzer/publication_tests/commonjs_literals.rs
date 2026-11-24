//! Exact package calls and property reads from direct `CommonJS` export literals.

use super::*;
use rift_protocol::index::StoredEdge;
use rift_protocol::read::{EdgeDerivation, EdgeKind, EdgeProvider};

const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","type":"commonjs","exports":{".":"./index.js","./literal":"./literal.js","./values":"./values.js","./forward":"./forward.js"}}"#;
const ROOT_ONLY_MANIFEST: &str =
    r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"index.js"}"#;
const OWNER: &str = "function checkExistence() { return 1 }
module.exports = { exist: checkExistence }
";
const USER: &str = "const library = require('./literal')
const { exist } = require('./literal')
const { kDirect } = require('./values')
function use() {
    library.exist()
    exist()
    return kDirect
}
module.exports = use
";
const VALUES: &str = "module.exports = { kDirect: Symbol('direct') }
";

fn analyze(user: &str, owner: &str) -> PackageAnalysis {
    analyze_with_manifest(user, owner, MANIFEST)
}

fn analyze_with_manifest(user: &str, owner: &str, manifest: &str) -> PackageAnalysis {
    fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.js", user),
            ("literal.js", owner),
            ("values.js", VALUES),
            ("forward.js", "module.exports = require('./literal')"),
        ],
        None,
        Some(("package.json", manifest)),
    )
    .expect("selected CommonJS package")
}

#[test]
fn direct_literal_without_a_published_module_identity_keeps_its_edges_unresolved() {
    let analyzed = analyze_with_manifest(USER, OWNER, ROOT_ONLY_MANIFEST);
    assert!(!analyzed.publication().edges.iter().any(|edge| {
        (edge.kinds == [EdgeKind::Calls]
            && (occurrence(edge, USER, "library.exist()") || occurrence(edge, USER, "exist()")))
            || (edge.kinds == [EdgeKind::Reads]
                && occurrence(edge, USER, "library.exist")
                && edge.to.as_str().ends_with("/default/exist"))
    }));
}

fn occurrence(edge: &StoredEdge, source: &str, written: &str) -> bool {
    edge.occurrence.as_ref().is_some_and(|occurrence| {
        occurrence.unit.as_str().ends_with("/index.js")
            && source.get(offset_in(occurrence.range.start)..offset_in(occurrence.range.end))
                == Some(written)
    })
}

#[test]
fn final_commonjs_literal_pair_keeps_its_property_and_calls_its_local_function() {
    let (publication, files, semantics, _) = analyze(USER, OWNER).into_parts();
    let owner = files
        .iter()
        .find(|file| file.file().path().as_str() == "literal.js")
        .expect("checked literal provider");
    let syntax = owner.file().syntax();
    let property = syntax
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name == "default.exist")
        .expect("physical property declaration");
    assert_eq!(property.syntax_element_kind, Some("pair"));
    let item = property.item_range;
    assert_eq!(
        OWNER.get(offset_in(item.start)..offset_in(item.end)),
        Some("exist: checkExistence")
    );
    let proof = syntax
        .static_facts()
        .and_then(rift_syntax::StaticFacts::commonjs_export)
        .expect("closed module literal");
    assert!(proof.assignment.encloses(item));
    assert!(proof.initialized.is_empty());
    assert!(proof.literal.is_some_and(|literal| literal.encloses(item)));
    let property = owner
        .placement()
        .logical_identity(&property.qualified_name)
        .expect("same retained member identity");
    let callable = owner
        .placement()
        .logical_identity("checkExistence")
        .expect("exact lexical function");
    assert_ne!(property, callable);

    for (kind, text, target) in [
        (EdgeKind::Calls, "library.exist()", callable.as_str()),
        (EdgeKind::Calls, "exist()", callable.as_str()),
        (EdgeKind::Reads, "library.exist", property.as_str()),
    ] {
        let edge = publication
            .edges
            .iter()
            .find(|edge| {
                edge.kinds == [kind] && edge.to.as_str() == target && occurrence(edge, USER, text)
            })
            .expect("exact static edge at the written occurrence");
        assert!(edge.from.as_str().ends_with("/use"));
        assert_eq!(edge.provider, EdgeProvider::Static);
        assert_eq!(edge.derivation, EdgeDerivation::Resolution);
        assert!(
            semantics
                .edges()
                .edges()
                .any(|stored| &stored.to_protocol() == edge)
        );
    }
    let value = publication
        .edges
        .iter()
        .find(|edge| edge.kinds == [EdgeKind::Imports] && occurrence(edge, USER, "kDirect"))
        .expect("destructuring imports the donor property");
    assert!(value.to.as_str().ends_with("/default/kDirect"));
    assert!(
        semantics
            .edges()
            .edges()
            .any(|stored| &stored.to_protocol() == value)
    );
    let local = publication
        .edges
        .iter()
        .find(|edge| edge.kinds == [EdgeKind::Reads] && occurrence(edge, USER, "kDirect"))
        .expect("the later occurrence reads the destructured local binding");
    assert_eq!(local.to, value.from);
}

#[test]
fn final_commonjs_literal_pair_refuses_uncertain_or_mutated_values() {
    let direct = "const library = require('./literal')
function use() { return library.exist() }
module.exports = use";
    let literal_negatives = [
        "function local() { return 1 }\nmodule.exports = { exist: local, [key]: 1 }".to_owned(),
        "function local() { return 1 }\nmodule.exports = { exist: local, 'exist': 1 }".to_owned(),
        "function local() { return 1 }\nmodule.exports = { exist: local, exist() {} }".to_owned(),
        "function local() { return 1 }\nmodule.exports = { exist: local, ...unknown }".to_owned(),
        format!("const module = {{ exports: {{}} }}\n{OWNER}"),
    ];
    for owner in literal_negatives.into_iter().chain(
        [
            "module.exports.exist = unknown",
            "module.exports[key] = unknown",
            "const alias = module.exports\nalias.exist = unknown",
            "module.exports = unknown",
        ]
        .map(|effect| format!("{OWNER}{effect}")),
    ) {
        assert_no_literal_member_edges(direct, &owner);
    }
    let rebound =
        "let local = function () { return 1 }\nlocal = unknown\nmodule.exports = { exist: local }";
    let analyzed = analyze(direct, rebound);
    assert!(!analyzed.publication().edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls] && occurrence(edge, direct, "library.exist()")
    }));
    for effect in [
        "library = unknown",
        "library.exist = unknown",
        "library[key] = unknown",
        "const alias = library\nalias.exist = unknown",
        "delete library.exist",
        "mutate(library)",
        "library[key]()",
        "const other = require('./literal.js')\nother.exist = unknown",
        "require('./literal').exist = unknown",
    ] {
        let user = format!(
            "let library = require('./literal')\n{effect}\nfunction use() {{ return library.exist() }}"
        );
        assert_no_literal_member_edges(&user, OWNER);
    }
}

#[test]
fn literal_pair_refuses_standalone_member_reads_without_a_checked_use() {
    let user = "const library = require('./literal')\nfunction use() { return library.exist }\nmodule.exports = use";
    let analyzed = analyze(user, OWNER);
    assert!(!analyzed.publication().edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads]
            && occurrence(edge, user, "library.exist")
            && edge.to.as_str().ends_with("/default/exist")
    }));
    let spread = "function checkExistence() { return 1 }\nmodule.exports = { exist: checkExistence, ...unknown }";
    let analyzed = analyze(user, spread);
    assert!(!analyzed.publication().edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads]
            && occurrence(edge, user, "library.exist")
            && edge.to.as_str().ends_with("/default/exist")
    }));
}

fn assert_no_literal_member_edges(user: &str, owner: &str) {
    let analyzed = analyze(user, owner);
    assert!(
        !analyzed.publication().edges.iter().any(|edge| {
            (edge.kinds == [EdgeKind::Calls] && occurrence(edge, user, "library.exist()"))
                || (edge.kinds == [EdgeKind::Reads]
                    && occurrence(edge, user, "library.exist")
                    && edge.to.as_str().ends_with("/default/exist"))
        }),
        "{user}\n{owner}"
    );
}

#[test]
fn literal_pair_keeps_unproved_forwarded_members_unresolved() {
    let user = "const library = require('./forward')
function use() { return [library.exist(), library.exist] }
module.exports = use";
    let analyzed = analyze(user, OWNER);
    assert!(!analyzed.publication().edges.iter().any(|edge| {
        (edge.kinds == [EdgeKind::Calls] && occurrence(edge, user, "library.exist()"))
            || (edge.kinds == [EdgeKind::Reads]
                && occurrence(edge, user, "library.exist")
                && edge.to.as_str().ends_with("/default/exist"))
    }));
}

#[test]
fn literal_pair_before_other_written_properties_keeps_its_call_and_read() {
    let user = "const library = require('./literal')
function use() { return library.exist() }
module.exports = use";
    for owner in [
        "function local() { return 1 }\nmodule.exports = { exist: local, other: 1 }",
        "function local() { return 1 }\nmodule.exports = { ...base, exist: local, 'other': 1, last() {} }",
    ] {
        let analyzed = analyze(user, owner);
        for (kind, text, target) in [
            (EdgeKind::Calls, "library.exist()", "/local"),
            (EdgeKind::Reads, "library.exist", "/default/exist"),
        ] {
            assert!(
                analyzed.publication().edges.iter().any(|edge| {
                    edge.kinds == [kind]
                        && occurrence(edge, user, text)
                        && edge.to.as_str().ends_with(target)
                }),
                "{kind:?} {owner}"
            );
        }
    }
}
