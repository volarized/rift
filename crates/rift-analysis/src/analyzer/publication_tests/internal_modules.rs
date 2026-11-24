//! Identities and visibility of npm package files that no entry names.

use super::*;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::EdgeKind;
use std::collections::{BTreeMap, BTreeSet};

const MAIN: &str = r#"{"name":"beacon","version":"1.0.0","main":"index.js"}"#;
const EXPORTS: &str = r#"{"name":"beacon","version":"1.0.0","exports":"./index.js"}"#;
const ENTRY: &str = "var inner = require('./lib/inner');
module.exports = function load() { return inner() };
";
const INNER: &str = "module.exports = function inner() {};\n";

pub(super) fn analyze(manifest: &str, files: &[(&str, &str)]) -> PackageAnalysis {
    fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        files.to_vec(),
        None,
        Some(("package.json", manifest)),
    )
    .expect("selected npm package")
}

fn spelled(identity: &str) -> Vec<String> {
    SymbolIdentity::parse(identity)
        .expect("published identity")
        .qualified_path()
        .to_vec()
}

/// Every published declaration by its module, name path, and file, with its public flag.
pub(super) fn declared(analysis: &PackageAnalysis) -> BTreeMap<(Vec<String>, String), bool> {
    let publication = analysis.publication();
    publication
        .declarations
        .iter()
        .map(|row| {
            let unit = publication
                .units
                .iter()
                .find(|unit| unit.unit == row.unit)
                .expect("published unit");
            (
                (spelled(row.symbol.as_str()), unit.path.0.clone()),
                row.public,
            )
        })
        .collect()
}

pub(super) fn edges(analysis: &PackageAnalysis) -> BTreeSet<(Vec<String>, EdgeKind, Vec<String>)> {
    analysis
        .publication()
        .edges
        .iter()
        .flat_map(|edge| {
            edge.kinds.iter().map(|kind| {
                (
                    spelled(edge.from.as_str()),
                    *kind,
                    spelled(edge.to.as_str()),
                )
            })
        })
        .collect()
}

fn path(segments: &[&str]) -> Vec<String> {
    segments
        .iter()
        .map(|segment| (*segment).to_owned())
        .collect()
}

fn public(analysis: &PackageAnalysis, segments: &[&str], file: &str) -> Option<bool> {
    declared(analysis)
        .get(&(path(segments), file.to_owned()))
        .copied()
}

#[test]
fn a_required_file_is_a_module_named_by_its_path() {
    let files = [("index.js", ENTRY), ("lib/inner.js", INNER)];
    for (manifest, importable) in [(MAIN, true), (EXPORTS, false)] {
        let analysis = analyze(manifest, &files);
        assert_eq!(
            public(&analysis, &["beacon/lib/inner.js", "inner"], "lib/inner.js"),
            Some(importable),
            "{manifest} {:?}",
            declared(&analysis)
        );
        assert!(
            public(&analysis, &["beacon", "inner"], "index.js").is_some(),
            "the entry's own variable keeps its identity: {:?}",
            declared(&analysis)
        );
        assert!(
            edges(&analysis)
                .iter()
                .any(|(_, kind, to)| *kind == EdgeKind::Calls
                    && *to == path(&["beacon/lib/inner.js", "inner"])),
            "{manifest} {:?}",
            edges(&analysis)
        );
    }
}

#[test]
fn files_no_entry_reaches_are_identified_and_public_only_without_an_exports_map() {
    let files = [
        ("index.js", "export function load() {}\n"),
        (
            "lib/x.js",
            "import { y } from './y.js';\nexport function x() { return y() }\n",
        ),
        ("lib/y.js", "export function y() {}\nfunction hidden() {}\n"),
    ];
    for (manifest, importable) in [(MAIN, true), (EXPORTS, false)] {
        let analysis = analyze(manifest, &files);
        assert_eq!(
            public(&analysis, &["beacon", "load"], "index.js"),
            Some(true)
        );
        for (name, file) in [("x", "lib/x.js"), ("y", "lib/y.js")] {
            assert_eq!(
                public(&analysis, &[&format!("beacon/{file}"), name], file),
                Some(importable),
                "{manifest} {file}"
            );
        }
        assert_eq!(
            public(&analysis, &["beacon/lib/y.js", "hidden"], "lib/y.js"),
            Some(false)
        );
        assert!(
            edges(&analysis).contains(&(
                path(&["beacon/lib/x.js", "x"]),
                EdgeKind::Calls,
                path(&["beacon/lib/y.js", "y"])
            )),
            "{manifest} {:?}",
            edges(&analysis)
        );
    }
}

#[test]
fn the_other_file_of_a_dual_entry_keeps_its_own_module() {
    let analysis = analyze(
        r#"{"name":"beacon","version":"1.0.0","exports":{".":{"import":"./index.mjs","require":"./index.cjs"}}}"#,
        &[
            ("index.mjs", "export function load() {}\n"),
            ("index.cjs", "exports.load = function load() {};\n"),
        ],
    );
    assert!(
        public(&analysis, &["beacon/index.cjs", "load"], "index.cjs").is_some(),
        "{:?}",
        declared(&analysis)
    );
    let rows = declared(&analysis);
    let modules = rows
        .keys()
        .map(|(path, file)| (path[0].clone(), file.clone()))
        .collect::<BTreeSet<_>>();
    let names = modules
        .iter()
        .map(|(module, _)| module)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names.len(),
        modules.len(),
        "no two files share a module: {modules:?}"
    );
}

#[test]
fn an_entry_keeps_the_name_another_file_would_take() {
    let analysis = analyze(
        r#"{"name":"beacon","version":"1.0.0","exports":{"./lib/a.js":"./dist/a.js"}}"#,
        &[
            ("dist/a.js", "export function a() {}\n"),
            ("lib/a.js", "export function shadowed() {}\n"),
        ],
    );
    assert_eq!(
        public(&analysis, &["beacon/lib/a.js", "a"], "dist/a.js"),
        Some(true)
    );
    assert!(
        declared(&analysis)
            .keys()
            .all(|(_, file)| file != "lib/a.js"),
        "{:?}",
        declared(&analysis)
    );
    assert!(!analysis.publication().coverage[0].identity_complete);
}

#[test]
fn files_of_nested_packages_take_no_identity_of_this_package() {
    for (nested, source) in [
        (vec![], "vendor/node_modules/dep/index.js"),
        (vec![], "node_modules/dep/index.js"),
        (
            vec![("sub/package.json", r#"{"name":"other","version":"1.0.0"}"#)],
            "sub/index.js",
        ),
    ] {
        let mut files = vec![
            ("index.js", "export function load() {}\n"),
            (source, "export function foreign() {}\n"),
        ];
        files.extend(nested);
        let analysis = analyze(MAIN, &files);
        assert_eq!(
            public(&analysis, &["beacon", "load"], "index.js"),
            Some(true)
        );
        assert!(
            declared(&analysis).keys().all(|(_, file)| file != source),
            "{source} {:?}",
            declared(&analysis)
        );
    }
    let same = analyze(
        MAIN,
        &[
            ("index.js", "export function load() {}\n"),
            ("sub/package.json", r#"{"type":"module"}"#),
            ("sub/index.js", "export function own() {}\n"),
        ],
    );
    assert!(public(&same, &["beacon/sub/index.js", "own"], "sub/index.js").is_some());
}

#[test]
fn a_file_that_only_reexports_has_the_module_of_its_path() {
    let analysis = analyze(
        MAIN,
        &[
            ("index.js", "export function load() {}\n"),
            ("lib/barrel.js", "export * from './a.js';\n"),
            ("lib/a.js", "export function a() {}\n"),
        ],
    );
    assert!(public(&analysis, &["beacon/lib/a.js", "a"], "lib/a.js").is_some());
    for (from, kind, to) in edges(&analysis) {
        assert!(
            [&from, &to].iter().all(|path| !path[0].starts_with("./")),
            "{from:?} {kind:?} {to:?}"
        );
    }
}

#[test]
fn the_ambient_modules_of_an_internal_file_keep_their_identity_and_visibility() {
    let files = [
        ("index.js", "export function load() {}\n"),
        (
            "globals.d.ts",
            "declare module 'other' { export function g(): void; export interface I { m: string } }\n",
        ),
    ];
    for manifest in [MAIN, EXPORTS] {
        let analysis = analyze(manifest, &files);
        for name in [&["other"][..], &["other", "g"], &["other", "I"]] {
            assert_eq!(
                public(&analysis, name, "globals.d.ts"),
                Some(true),
                "{manifest} {name:?} {:?}",
                declared(&analysis)
            );
        }
        if manifest == MAIN {
            // The exports map names no declaration file, which leaves TypeScript's entry open.
            for coverage in &analysis.publication().coverage {
                assert!(coverage.identity_complete, "{coverage:?}");
            }
        }
    }
}

#[test]
fn an_alias_of_an_entry_keeps_the_aliased_declaration_and_its_members_public() {
    let analysis = analyze(
        r#"{"name":"beacon","version":"1.0.0","type":"module","exports":"./index.js"}"#,
        &[
            ("index.js", "export { Shown as Seen } from './lib/a.js';\n"),
            (
                "lib/a.js",
                "export class Shown { m() {} }\nexport class ShownToo { m() {} }\n",
            ),
        ],
    );
    let file = "lib/a.js";
    for (name, importable) in [("Shown", true), ("ShownToo", false)] {
        assert_eq!(
            [
                public(&analysis, &["beacon/lib/a.js", name], file),
                public(&analysis, &["beacon/lib/a.js", name, "m"], file),
            ],
            [Some(importable); 2],
            "{name} {:?}",
            edges(&analysis)
        );
    }
}

#[test]
fn a_forward_another_value_can_replace_reaches_no_target() {
    let user = "var i = require('./index');
var { open } = require('./index');
function use() { i.open(); open() }
exports.use = use;
";
    for (forward, settled) in [
        ("module.exports = require('./child');", true),
        ("exports = module.exports = require('./child');", true),
        ("var child = module.exports = require('./child');", true),
        (
            "module.exports = require('./child'); exports.other = 1;",
            true,
        ),
        ("if (cond) module.exports = require('./child');", false),
        ("cond && (module.exports = require('./child'));", false),
        (
            "module.exports = require('./child'); module.exports = other;",
            false,
        ),
        (
            "module.exports = other; module.exports = require('./child');",
            false,
        ),
        (
            "module.exports = require('./child'); if (cond) module.exports = other;",
            false,
        ),
        (
            "module.exports = require('./child'); function swap() { module.exports = other }",
            false,
        ),
        ("module.exports = require(target);", false),
        (
            "const require = other; module.exports = require('./child');",
            false,
        ),
        ("module.exports = require('../child');", false),
        ("module.exports = require('./missing');", false),
    ] {
        let analysis = analyze(
            MAIN,
            &[
                ("index.js", forward),
                ("child.js", "exports.open = function open() {};\n"),
                ("user.js", user),
            ],
        );
        let reached = edges(&analysis)
            .into_iter()
            .filter(|(from, kind, to)| {
                from[0] == "beacon/user.js"
                    && to[0] == "beacon/child.js"
                    && [EdgeKind::Calls, EdgeKind::Reads].contains(kind)
            })
            .collect::<Vec<_>>();
        assert_eq!(!reached.is_empty(), settled, "{forward} {reached:?}");
    }
}
