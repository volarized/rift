//! Identities and visibility of npm package files that no entry names.

use super::*;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::EdgeKind;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const MAIN: &str = r#"{"name":"beacon","version":"1.0.0","main":"index.js"}"#;
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
    let rows = rows(analysis);
    let declared = rows.iter().cloned().collect::<BTreeMap<_, _>>();
    assert_eq!(
        declared.len(),
        rows.len(),
        "each declaration has its own identity: {rows:?}"
    );
    declared
}

/// Every published declaration row, one for each declaration of an identity.
fn rows(analysis: &PackageAnalysis) -> Vec<((Vec<String>, String), bool)> {
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

pub(super) fn path(segments: &[&str]) -> Vec<String> {
    segments
        .iter()
        .map(|segment| (*segment).to_owned())
        .collect()
}

pub(super) fn public(analysis: &PackageAnalysis, segments: &[&str], file: &str) -> Option<bool> {
    declared(analysis)
        .get(&(path(segments), file.to_owned()))
        .copied()
}

#[test]
fn versioned_types_entry_scripts_keep_their_file_identities() {
    let manifest = r#"{"name":"beacon","version":"1.0.0","main":"","types":"index.d.ts","typesVersions":{"<=5.6":{"*":["ts5.6/*"]}}}"#;
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![(
            "index.d.ts",
            "interface MediaMetrics { ready: boolean }\ndeclare var Probe: MediaMetrics;\n",
        )],
        None,
        Some(("package.json", manifest)),
    )
    .expect("published declaration script");
    let rows = declared(&analysis);
    let module = "beacon/index.d.ts";
    let file = "index.d.ts".to_owned();
    let identities = [
        path(&[module, "MediaMetrics"]),
        path(&[module, "MediaMetrics", "ready"]),
        path(&[module, "Probe"]),
    ];
    for identity in &identities {
        assert_eq!(
            rows.get(&(identity.clone(), file.clone())),
            Some(&false),
            "{rows:?}"
        );
    }
    assert!(
        rows.keys()
            .all(|(path, _)| path.first().is_some_and(|name| name == module))
    );
    let found = edges(&analysis);
    assert!(
        found.contains(&(
            identities[0].clone(),
            EdgeKind::Contains,
            identities[1].clone()
        )),
        "{found:?}"
    );
    assert!(
        found.contains(&(
            identities[2].clone(),
            EdgeKind::HasType,
            identities[0].clone()
        )),
        "{found:?}"
    );
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
fn a_typescript_script_names_no_module_by_its_path() {
    let files = [
        ("index.js", "export function load() {}\n"),
        (
            "ambient.d.ts",
            "/** Documented. */\ndeclare module 'other' { import { A } from 'a'; export function g(a: A): void }\n",
        ),
        (
            "ns.d.ts",
            "declare namespace NS { export interface I { a: 1 } }\ndeclare function beside(): void;\n",
        ),
        (
            "ns2.d.ts",
            "declare namespace NS { export interface I { b: 1 } }\n",
        ),
    ];
    let analysis = analyze(MAIN, &files);
    for (path, file) in declared(&analysis).into_keys() {
        assert!(!path[0].ends_with(".d.ts"), "{path:?} {file}");
    }
    let modules = analyze(
        MAIN,
        &[
            files[0],
            ("empty.ts", "export {};\nfunction inEmpty() {}\n"),
            ("imports.ts", "import './s';\nfunction inImports() {}\n"),
            ("always.mts", "function inMts() {}\n"),
        ],
    );
    for (name, file) in [
        ("inEmpty", "empty.ts"),
        ("inImports", "imports.ts"),
        ("inMts", "always.mts"),
    ] {
        let module = format!("beacon/{file}");
        assert!(public(&modules, &[&module, name], file).is_some(), "{file}");
    }
}

#[test]
fn an_exports_subpath_keeps_the_name_of_another_file() {
    let source = "export function run() {}\n";
    for (exports, identified) in [
        (r#"{".":"./a.js","./lib/a.js":"./a.js"}"#, false),
        (r#"{".":"./a.js","./lib/a.js":"./dist/a.wasm"}"#, false),
        (r#"{".":"./a.js","./lib/*":"./dist/*"}"#, false),
        (r#"{".":"./a.js","./lib/a.js":["./a.js"]}"#, false),
        (
            r#"{".":"./a.js","./lib/*":{"import":"./lib/*","require":"./dist/*"}}"#,
            false,
        ),
        (r#"{".":"./a.js","./lib/a.js":{"browser":"./a.js"}}"#, false),
        (r#"{".":"./a.js","./*":"./*","./lib/*":null}"#, true),
        (
            r#"{".":"./a.js","./lib/*":{"custom":"./src/*","types":"./dist/*","import":"./lib/*"}}"#,
            true,
        ),
        (
            r#"{".":"./a.js","./*":"./dist/*","./lib/*":"./lib/*"}"#,
            true,
        ),
        (r#"{".":"./a.js","./lib/*":{"browser":"./lib/*"}}"#, true),
        (r#"{".":"./a.js","./other/*":"./dist/*"}"#, true),
    ] {
        let manifest = format!(r#"{{"name":"beacon","version":"1.0.0","exports":{exports}}}"#);
        let analysis = analyze(&manifest, &[("a.js", source), ("lib/a.js", source)]);
        assert_eq!(
            public(&analysis, &["beacon/lib/a.js", "run"], "lib/a.js").is_some(),
            identified,
            "{exports}"
        );
        if !identified {
            assert!(!analysis.publication().coverage[0].identity_complete);
        }
    }
}

/// How often each name of `module` is declared under an identity.
fn counted(analysis: &PackageAnalysis, module: &str) -> BTreeMap<String, usize> {
    let mut counted = BTreeMap::new();
    for ((path, _), _) in rows(analysis) {
        if path[0] == module && path.len() > 1 {
            *counted.entry(path[1..].join("/")).or_default() += 1;
        }
    }
    counted
}

const REPEATED: &str = "it(function () { function helper() {} });
it(function () { var helper = 1; });
it(function () { class dup { secret() {} } });
function dup() { { let block = 1; } { let block = 2; } var twice = 1; var twice = 2; }
exports.dup = dup;
class A { static x() {} x() {} y() {} }
const E = class { static s() {} s() {} };
class B { get v() { return 1 } set v(value) {} constructor() { this.w = 1 } w() {} }
function F() { this.z = 1; }
F.z = 2;
F.prototype.p = function () {};
var again = 1;
var again = 2;
var own = { key: 1, nested: { deep: 1 } };
own.key = 2;
run({ option: 1 });
run({ option: 2 });
run({ get pair() { return 1 }, set pair(value) {} });
function build() { if (cond) return { part: 1 }; return { part: 2 } }
module.exports = { A, B, F };
";

#[test]
fn a_name_declared_in_several_scopes_takes_no_identity() {
    let analysis = analyze(MAIN, &[("index.js", ""), ("lib/k.js", REPEATED)]);
    let counted = counted(&analysis, "beacon/lib/k.js");
    for name in [
        "helper",
        "dup",
        "dup/secret",
        "dup/block",
        "dup/twice",
        "A/x",
        "E/s",
        "option",
        "build/part",
    ] {
        assert!(!counted.contains_key(name), "{name} {counted:?}");
    }
    for (name, count) in [
        ("A", 1),
        ("A/y", 1),
        ("B/v", 2),
        ("pair", 2),
        ("B/w", 2),
        ("F/p", 1),
        ("F/z", 1),
        ("again", 2),
        ("own/key", 1),
        ("own/nested/deep", 1),
    ] {
        assert_eq!(counted.get(name), Some(&count), "{name} {counted:?}");
    }
    for name in ["helper", "dup", "secret", "block", "x"] {
        assert!(
            analysis
                .publication()
                .objects
                .iter()
                .any(|object| object.name == name && object.id.is_none()),
            "{name} keeps its object"
        );
    }
    let merged = "export function over(a: string): void;
export function over(a: number): void;
export function over(a: unknown): void {}
export interface Shape { a: 1 }
export interface Shape { b: 1 }
export interface Value { a: 1 }
export const Value = 1;
export function Merged(): void {}
export namespace Merged { export const inner = 1; const hidden = 1; }
export namespace Merged { const hidden = 2; export function other(): void {} }
export type Union = { kind: 1; only: 1 } | { kind: 2 };
export type Whole = { whole: 1; nested: { deep: 1 } };
export interface Calls { (options: { limit: 1 }): void; (options: { limit: 2 }): void; own: 1 }
export type Both = { shared: 1 };
export const Both = { shared: 1 };
export interface Made { go(): void }
export const Made = make(() => ({ go() {} }));
run(() => { interface Local { a: 1 } const scoped = 1; return scoped });
run(() => { const scoped = 2; return scoped });
";
    let analysis = analyze(MAIN, &[("index.js", ""), ("lib/m.ts", merged)]);
    let counted = self::counted(&analysis, "beacon/lib/m.ts");
    for (name, count) in [
        ("over", 3),
        ("Shape", 2),
        ("Shape/a", 1),
        ("Shape/b", 1),
        ("Value", 2),
        ("Merged", 3),
        ("Merged/inner", 1),
        ("Merged/other", 1),
        ("Made", 2),
        ("Union", 1),
        ("Union/only", 1),
        ("Whole/whole", 1),
        ("Whole/nested/deep", 1),
        ("Calls/own", 1),
        ("Both/shared", 2),
    ] {
        assert_eq!(counted.get(name), Some(&count), "{name} {counted:?}");
    }
    for name in [
        "Merged/hidden",
        "scoped",
        "Made/go",
        "Union/kind",
        "Calls/limit",
    ] {
        assert!(!counted.contains_key(name), "{name} {counted:?}");
    }
}

/// Members of classes, object literals and functions that no declaration holds alone.
const BODIES: &str = "run(class { hook() {} constructor() { this.made = 1 } });
run(class { hook() {} constructor() { this.made = 2 } });
new (class { own1 = 1; static own2 = 1; static both() {} both() {} })();
new (class { own1 = 2; static own2 = 2 })();
var list = [class { item() {} }, class { item() {} }];
var fns = [function () { this.slot = 1 }, function () { this.slot = 2 }];
let Late;
Late = class { late() {} };
Late = class { late() {} };
run(class { single() {} get access() { return 1 } set access(value) {} });
const Held = class { kept() {} };
var double = { part1: 1 };
var double = { part1: 2 };
";

#[test]
fn members_of_several_bodies_without_a_name_take_no_identity() {
    let classes = "export declare function make(value: unknown): void;
make(class { constructor(readonly param: 1) {} several(a: 1): void; several(a: 2): void; several() {} });
make(class { constructor(readonly param: 2) {} field = 1 });
export const classes = [class { constructor(readonly inner: 1) {} }, class { inner = 1 }];
export class Overloads { go(a: 1): void; go(a: 2): void; go() {} constructor(readonly value: 1) {} }
";
    let analysis = analyze(
        MAIN,
        &[
            ("index.js", ""),
            ("lib/b.js", BODIES),
            ("lib/c.ts", classes),
        ],
    );
    let javascript = counted(&analysis, "beacon/lib/b.js");
    let typescript = counted(&analysis, "beacon/lib/c.ts");
    for name in [
        "hook",
        "constructor",
        "made",
        "own1",
        "own2",
        "both",
        "list/item",
        "slot",
        "fns/slot",
        "late",
        "double/part1",
        "param",
        "classes/inner",
    ] {
        assert!(
            !javascript.contains_key(name) && !typescript.contains_key(name),
            "{name} {javascript:?} {typescript:?}"
        );
    }
    for (name, count) in [
        ("single", 1),
        ("access", 2),
        ("Held", 1),
        ("Held/kept", 1),
        ("double", 2),
    ] {
        assert_eq!(javascript.get(name), Some(&count), "{name} {javascript:?}");
    }
    for (name, count) in [
        ("several", 3),
        ("field", 1),
        ("classes", 1),
        ("classes/constructor", 1),
        ("Overloads/go", 3),
        ("Overloads/value", 1),
    ] {
        assert_eq!(typescript.get(name), Some(&count), "{name} {typescript:?}");
    }
}

#[test]
fn an_entry_keeps_the_identities_of_names_in_several_scopes() {
    let analysis = analyze(MAIN, &[("index.js", REPEATED)]);
    let counts = counted(&analysis, "beacon");
    for (name, count) in [("helper", 1), ("helper#1", 1), ("dup", 2), ("A/x", 2)] {
        assert_eq!(counts.get(name), Some(&count), "{name} {counts:?}");
    }
    let publication = analysis.publication();
    let unit = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.js")
        .expect("authored entry source");
    for (name, declaration) in [
        ("helper", "function helper() {}"),
        ("helper#1", "helper = 1"),
    ] {
        let row = publication
            .declarations
            .iter()
            .find(|row| row.unit == unit.unit && spelled(row.symbol.as_str()) == ["beacon", name])
            .expect("physical helper declaration");
        let start = usize::try_from(row.range.start).expect("declaration start");
        let end = usize::try_from(row.range.end).expect("declaration end");
        assert_eq!(REPEATED.get(start..end), Some(declaration));
        assert!(!row.public);
    }
}

/// Forwards of `child.js`, and whether one gives the module its value.
const FORWARDS: &[(&str, bool)] = &[
    ("module.exports = require('./child');", true),
    ("exports = module.exports = require('./child');", true),
    ("var child = module.exports = require('./child');", true),
    (
        "module.exports = require('./child'); exports.other = 1;",
        true,
    ),
    ("{ module.exports = require('./child') }", true),
    ("(module.exports = require('./child'));", true),
    (
        "(function () { module.exports = require('./child') })();",
        true,
    ),
    (
        "module.exports = require('./child'); module.exports = require('./child');",
        true,
    ),
    (
        "module.exports = require('./child'); if (cond) module.exports = require('./child');",
        true,
    ),
    (
        "module.exports = other; module.exports = require('./child');",
        true,
    ),
    (
        "module.exports = require('./other'); module.exports = require('./child');",
        true,
    ),
    (
        "module.exports = require('./child'); module.exports = require('./other');",
        false,
    ),
    (
        "module.exports = (function () { module.exports = require('./child'); return other })();",
        false,
    ),
    (
        "(function () { if (cond) return; module.exports = require('./child') })();",
        false,
    ),
    (
        "if (cond) return; module.exports = require('./child');",
        false,
    ),
    (
        "if (cond) module.exports = other; module.exports = require('./child');",
        true,
    ),
    (
        "if (cond) { module.exports = other; return } module.exports = require('./child');",
        false,
    ),
    (
        "class A { static { module.exports = other } } module.exports = require('./child');",
        false,
    ),
    (
        "module.exports = require('./child'); (() => { module.exports = other })();",
        false,
    ),
    (
        "module.exports = require('./child'); module['exports'] = other;",
        false,
    ),
    (
        "module['exports'] = other; module.exports = require('./child');",
        true,
    ),
    (
        "module[key] = other; module.exports = require('./child');",
        true,
    ),
    (
        "module.exports = require('./child'); module[key] = other;",
        false,
    ),
    (
        "module.exports = require('./child'); module['id'] = other;",
        true,
    ),
    (
        "if (cond) { module.exports = require('./child') } else { module.exports = require('./child') }",
        true,
    ),
    (
        "if (cond) { module.exports = require('./child') } else { module.exports = require('./other') }",
        false,
    ),
    ("if (cond) module.exports = require('./child');", false),
    ("cond && (module.exports = require('./child'));", false),
    (
        "module.exports = require('./child'); module.exports = other;",
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
];

#[test]
fn a_forward_another_value_can_replace_reaches_no_target() {
    let user = "var i = require('./index');
var { open } = require('./index');
function use() { i.open(); open() }
exports.use = use;
";
    for &(forward, settled) in FORWARDS {
        let analysis = analyze(
            MAIN,
            &[
                ("index.js", forward),
                ("child.js", "exports.open = function open() {};\n"),
                ("other.js", "exports.open = function open() {};\n"),
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
