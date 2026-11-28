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
        "E/s",
        "option",
        "build/part",
    ] {
        assert!(!counted.contains_key(name), "{name} {counted:?}");
    }
    for (name, count) in [
        ("A", 1),
        ("A/x", 1),
        ("A/x#1", 1),
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
    for name in ["helper", "dup", "secret", "block", "s"] {
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
    for (name, count) in [
        ("helper", 1),
        ("helper#1", 1),
        ("dup", 2),
        ("A/x", 1),
        ("A/x#1", 1),
        ("E/s", 2),
    ] {
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

#[test]
fn non_entry_lexical_declarations_keep_separate_object_slot_names() {
    const HEADER: &str = concat!(
        "Object.defineProperty(exports, '__esModule', { value: true });\n",
        "function observe(entries) { return entries; }\n",
    );
    const DECLARATIONS: [&str; 5] = [
        "class Entry { count = 1; read() { return this.count; } }",
        "count = 1",
        "read() { return this.count; }",
        "function create() { return new Entry(); }",
        "LIMIT = 2",
    ];
    const LEXICAL: &str = concat!(
        "class Entry { count = 1; read() { return this.count; } }\n",
        "function create() { return new Entry(); }\n",
        "const LIMIT = 2;\n",
    );
    const GETTERS: &str = "observe({ get Entry() { return Entry; }, get create() { return create; }, get LIMIT() { return LIMIT; } });\n";
    const GUARDED: &str = "0 && (module.exports = { Entry: null, create: null, LIMIT: null });\n";
    let combined = format!("{GETTERS}{GUARDED}");
    let false_guard = GUARDED.replacen('0', "false", 1);
    let real_guard = GUARDED.replacen('0', "1", 1);
    let unknown_guard = GUARDED.replacen('0', "unknown", 1);
    let alternate = format!("{GETTERS}if (choice) {{ class Entry {{}} }}\n");
    for (before, after, retained) in [
        (GETTERS, "", true),
        ("", GETTERS, true),
        (GUARDED, "", true),
        ("", false_guard.as_str(), true),
        (combined.as_str(), "", true),
        ("", combined.as_str(), true),
        (real_guard.as_str(), GETTERS, false),
        (unknown_guard.as_str(), GETTERS, false),
        (GETTERS, GETTERS, false),
    ] {
        let source = format!("{HEADER}{before}{LEXICAL}{after}");
        let analysis = analyze(
            MAIN,
            &[("index.js", "exports.main = 1;"), ("impl.js", &source)],
        );
        let result = analysis.publication();
        result.validate_facts().expect("checked package facts");
        let module = result
            .units
            .iter()
            .find(|unit| unit.path.0 == "impl.js")
            .expect("source unit");
        assert!(module.source_complete);
        let placed = DECLARATIONS.map(|written| {
            result
                .declarations
                .iter()
                .find(|row| row.unit == module.unit && row.source == written)
        });
        assert_eq!(
            placed.iter().flatten().count(),
            if retained { DECLARATIONS.len() } else { 0 },
            "{before} {after}"
        );
        if let [
            Some(class),
            Some(field),
            Some(method),
            Some(function),
            Some(variable),
        ] = placed
        {
            for row in [class, field, method, function, variable] {
                let start = usize::try_from(row.range.start).expect("start");
                let end = usize::try_from(row.range.end).expect("end");
                assert_eq!(source.get(start..end), Some(row.source.as_str()));
                assert!(row.source_complete && !row.public, "{row:?}");
            }
            for member in [field, method] {
                let object = result
                    .objects
                    .iter()
                    .find(|object| object.id.as_ref() == Some(&member.symbol))
                    .expect("class member");
                assert_eq!(object.container.as_ref(), Some(&class.symbol));
            }
        }
    }
    let source = format!("{HEADER}{alternate}{LEXICAL}");
    let analysis = analyze(
        MAIN,
        &[("index.js", "exports.main = 1;"), ("impl.js", &source)],
    );
    assert!(
        !analysis
            .publication()
            .declarations
            .iter()
            .any(|row| row.source == DECLARATIONS[0])
    );
}

#[test]
fn dual_commonjs_getter_keys_keep_checked_bindings_and_children() {
    const RUNTIME: &str = concat!(
        "var table = {};\n",
        "fill(table, { choose: () => choose, options: () => options, Entry: () => Entry });\n",
        "module.exports = table;\n",
        "var options = { safe: 1 };\n",
        "function choose(value) {\n",
        "    const flags = { found: void 0 };\n",
        "    return value + options.safe + Number(flags.found);\n",
        "}\n",
        "var Constructor = class {};\n",
        "var Entry = Constructor;\n",
    );
    const GUARD: &str = "0 && (module.exports = { choose, options, Entry });\n";
    const ESM: &str = "export function choose(value) { return value; }\nexport const options = { safe: 1 };\nexport class Entry {}\n";
    const TYPES: &str = "export declare function choose(value: number): number;\nexport declare const options: { safe: number };\nexport declare class Entry {}\n";
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./dist/index.cjs","exports":{"import":{"types":"./dist/index.d.ts","import":"./dist/index.js"},"require":{"types":"./dist/index.d.cts","require":"./dist/index.cjs"}}}"#;
    let analyze = |runtime: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![
                ("dist/index.cjs", runtime),
                ("dist/index.js", ESM),
                ("dist/index.d.ts", TYPES),
                ("dist/index.d.cts", TYPES),
            ],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("separate runtime variants")
    };
    let source = format!("{RUNTIME}{GUARD}");
    let captured = analyze(&source);
    let publication = captured.publication();
    publication
        .validate_facts()
        .expect("physical CommonJS owners");
    let written = |text: &str, kind: &str| {
        publication
            .declarations
            .iter()
            .find(|row| {
                row.unit.as_str().ends_with("dist/index.cjs")
                    && row.syntax_element_kind.as_deref() == Some(kind)
                    && row.source == text
            })
            .expect("authored CommonJS declaration")
    };
    let options = written("options = { safe: 1 }", "variable_declarator");
    let flags = written("flags = { found: void 0 }", "variable_declarator");
    let entry = written("Entry = Constructor", "variable_declarator");
    assert!(!entry.public);
    for (member, parent) in [
        (written("safe: 1", "pair"), &options.symbol),
        (written("found: void 0", "pair"), &flags.symbol),
    ] {
        assert!(publication.objects.iter().any(|symbol| {
            symbol.id.as_ref() == Some(&member.symbol) && symbol.container.as_ref() == Some(parent)
        }));
    }
    assert!(publication.declarations.iter().any(|row| {
        row.unit.as_str().ends_with("dist/index.cjs")
            && row.syntax_element_kind.as_deref() == Some("function_declaration")
            && row.range.start
                == u64::try_from(source.find("function choose").expect("function"))
                    .expect("function offset")
    }));
    let suppressed = analyze(&source.replace("0 &&", "condition &&"));
    let earlier_flags = suppressed
        .publication()
        .declarations
        .iter()
        .find(|row| {
            row.unit.as_str().ends_with("dist/index.cjs")
                && row.source == "flags = { found: void 0 }"
        })
        .expect("earlier local binding");
    assert_eq!(flags.symbol, earlier_flags.symbol);
    let unsupported = source.replace("0 &&", "condition &&");
    assert!(
        !analyze(&unsupported)
            .publication()
            .declarations
            .iter()
            .any(|row| {
                row.unit.as_str().ends_with("dist/index.cjs")
                    && row.syntax_element_kind.as_deref() == Some("function_declaration")
                    && row.source.starts_with("function choose")
            })
    );
}

#[test]
fn restored_esm_callable_keeps_its_exact_argument_members_and_local_binding() {
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.js"}"#;
    const ENTRY: &str =
        "import { registry } from './helper.js';\nexport const result = registry;\n";
    const SOURCE: &str = concat!(
        "const registry = {};\n",
        "publish(registry, { visit: () => visit });\n",
        "function visit(parser) {\n",
        "    const index = 1;\n",
        "    parser.collect({ type: 'head', index, token: { option: index } });\n",
        "    parser.collect({ type: 'tail', index, token: { option: index } });\n",
        "    parser.collect({ type: 'duplicate', type: 'competing' });\n",
        "    defer(() => { parser.collect({ type: 'deferred' }); });\n",
        "}\n",
        "export { registry };\n",
    );
    let result = analyze(MANIFEST, &[("index.js", ENTRY), ("helper.js", SOURCE)]);
    let graph = result.publication();
    graph
        .validate_facts()
        .expect("separate literal and binding facts");
    let owner = graph
        .declarations
        .iter()
        .find(|row| row.source.starts_with("function visit(") && row.source_complete)
        .expect("checked visit owner");
    let container = |symbol: &rift_protocol::read::SymbolId| {
        graph
            .objects
            .iter()
            .find(|row| row.id.as_ref() == Some(symbol))
            .and_then(|row| row.container.as_ref())
    };
    let direct = graph
        .declarations
        .iter()
        .filter(|row| row.unit == owner.unit && container(&row.symbol) == Some(&owner.symbol))
        .collect::<Vec<_>>();
    for written in ["type: 'head'", "type: 'tail'"] {
        assert!(direct.iter().any(|row| row.source == written));
    }
    assert_eq!(
        direct
            .iter()
            .filter(|row| {
                row.source == "index"
                    && row.syntax_element_kind.as_deref() == Some("shorthand_property_identifier")
            })
            .count(),
        2
    );
    for token in direct
        .iter()
        .filter(|row| row.source == "token: { option: index }")
    {
        assert!(graph.declarations.iter().any(|row| {
            row.source == "option: index"
                && container(&row.symbol) == Some(&token.symbol)
                && token.range.start < row.range.start
                && row.range.end < token.range.end
        }));
    }
    assert_eq!(
        direct
            .iter()
            .filter(|row| row.source == "token: { option: index }")
            .count(),
        2
    );
    assert!(direct.iter().all(|row| !matches!(
        row.source.as_str(),
        "type: 'duplicate'" | "type: 'competing'" | "type: 'deferred'"
    )));
    assert!(
        !graph
            .edges
            .iter()
            .any(|edge| edge.from == owner.symbol && edge.kinds == [EdgeKind::Calls])
    );
    let local = graph
        .declarations
        .iter()
        .find(|row| row.source == "index = 1")
        .expect("authored local binding");
    let refused = SOURCE.replace("() => visit", "() => 0");
    let open = analyze(MANIFEST, &[("index.js", ENTRY), ("helper.js", &refused)]);
    let before = open
        .publication()
        .declarations
        .iter()
        .find(|row| row.source == local.source)
        .expect("existing local binding");
    assert_eq!(before.symbol, local.symbol);
    assert!(
        !open
            .publication()
            .declarations
            .iter()
            .any(|row| row.symbol == owner.symbol || row.source == "type: 'head'")
    );
}

#[test]
fn checked_object_method_returns_keep_both_written_binding_and_shorthand() {
    const META: &str = r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.js"}"#;
    const ENTRY: &str = "export { renderer } from './impl.js';\n";
    const SOURCE: &str = concat!(
        "export const renderer = {\n",
        "    build(value) {\n",
        "        const width = value;\n",
        "        const height = value + 1;\n",
        "        return { width, height };\n",
        "    }\n",
        "};\n",
    );
    let result = analyze(META, &[("index.js", ENTRY), ("impl.js", SOURCE)]);
    let graph = result.publication();
    graph
        .validate_facts()
        .expect("written method and returned keys");
    let method = graph
        .declarations
        .iter()
        .find(|row| {
            row.source.starts_with("build(value)")
                && row.syntax_element_kind.as_deref() == Some("method_definition")
        })
        .expect("written method");
    let mut property_ids = BTreeSet::new();
    for (name, written_local) in [("width", "width = value"), ("height", "height = value + 1")] {
        let binding = graph
            .declarations
            .iter()
            .find(|row| row.source == written_local)
            .expect("written local");
        let property = graph
            .declarations
            .iter()
            .find(|row| {
                row.source == name
                    && row.syntax_element_kind.as_deref() == Some("shorthand_property_identifier")
            })
            .expect("written shorthand");
        assert_ne!(property.symbol, binding.symbol);
        property_ids.insert(property.symbol.clone());
        let first =
            u64::try_from(SOURCE.rfind(name).expect("written shorthand")).expect("written offset");
        assert_eq!(property.range.start, first);
        assert_eq!(
            property.range.end,
            first + u64::try_from(name.len()).expect("name length")
        );
        assert!(graph.objects.iter().any(|object| {
            object.id.as_ref() == Some(&property.symbol)
                && object.container.as_ref() == Some(&method.symbol)
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge.to == binding.symbol
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.unit == property.unit && site.range == property.range)
        }));
    }
    let unrelated = SOURCE.replace("return { width, height };", "inspect({ width, height });");
    let competing = SOURCE.replace(
        "return { width, height };",
        "return { width, width, height, height };",
    );
    for source in [unrelated, competing] {
        assert!(
            !analyze(META, &[("index.js", ENTRY), ("impl.js", &source)])
                .publication()
                .declarations
                .iter()
                .any(|row| property_ids.contains(&row.symbol))
        );
    }
}

#[test]
fn checked_method_return_pairs_survive_same_named_unproved_call_arguments() {
    const META: &str = r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.js"}"#;
    const ENTRY: &str = "export { renderer } from './impl.js';\n";
    const SOURCE: &str = concat!(
        "export const renderer = {\n",
        "    build(input) {\n",
        "        if (input) inspect({ width: 1, height: 2 });\n",
        "        return { width: input.width, height: input.height };\n",
        "    }\n",
        "};\n",
    );
    let result = analyze(META, &[("index.js", ENTRY), ("impl.js", SOURCE)]);
    let graph = result.publication();
    graph.validate_facts().expect("checked method return");
    let method = graph
        .declarations
        .iter()
        .find(|row| row.syntax_element_kind.as_deref() == Some("method_definition"))
        .expect("written method");
    for property in ["width: input.width", "height: input.height"] {
        let declaration = graph
            .declarations
            .iter()
            .find(|row| row.source == property)
            .expect("written returned property");
        let start = u64::try_from(SOURCE.find(property).expect("written property")).expect("start");
        assert_eq!(declaration.range.start, start);
        assert!(graph.objects.iter().any(|object| {
            object.id.as_ref() == Some(&declaration.symbol)
                && object.container.as_ref() == Some(&method.symbol)
        }));
    }
    assert!(
        !graph
            .declarations
            .iter()
            .any(|row| ["width: 1", "height: 2"].contains(&row.source.as_str()))
    );
    let ambiguous = SOURCE.replace(
        "width: input.width, height: input.height",
        "width: input.width, width: input.height",
    );
    let ambiguous_result = analyze(META, &[("index.js", ENTRY), ("impl.js", &ambiguous)]);
    let ambiguous = ambiguous_result.publication();
    assert!(
        !ambiguous
            .declarations
            .iter()
            .any(|row| ["width: input.width", "width: input.height"].contains(&row.source.as_str()))
    );
}

#[test]
fn checked_method_return_pair_keeps_the_separate_read_local() {
    const META: &str = r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.js"}"#;
    const ENTRY: &str = "export { renderer } from './impl.js';\n";
    const SOURCE: &str = concat!(
        "export const renderer = {\n",
        "    build(input) {\n",
        "        const width = input.width;\n",
        "        return { width: width + 1 };\n",
        "    }\n",
        "};\n",
    );
    let result = analyze(META, &[("index.js", ENTRY), ("impl.js", SOURCE)]);
    let graph = result.publication();
    graph.validate_facts().expect("checked return and local");
    let binding = graph
        .declarations
        .iter()
        .find(|row| row.source == "width = input.width")
        .expect("written local");
    let property = graph
        .declarations
        .iter()
        .find(|row| row.source == "width: width + 1")
        .expect("written return pair");
    assert_ne!(binding.symbol, property.symbol);
    assert!(graph.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads]
            && edge.from == property.symbol
            && edge.to == binding.symbol
            && edge.occurrence.as_ref().is_some_and(|site| {
                site.unit == property.unit
                    && property.range.start <= site.range.start
                    && site.range.end <= property.range.end
            })
    }));
}
const CALLBACK_MANIFEST: &str =
    r#"{"name":"beacon","version":"1.0.0","main":"./index.js","type":"module"}"#;
const CALLBACK_BINDINGS: &str = r#"
import { prepare } from "../index.js";
suite(() => {
    check(() => { const marker = prepare(); return marker; });
    check(() => { const marker = prepare() + 1; return marker; });
});
"#;

#[test]
fn repeated_typescript_bindings_keep_their_admitted_callback_scopes() {
    let found = analyze(
        CALLBACK_MANIFEST,
        &[
            ("index.js", "export function prepare() { return 3; }"),
            ("lib/check.ts", CALLBACK_BINDINGS),
        ],
    );
    let publication = found.publication();
    publication
        .validate_facts()
        .expect("valid callback bindings");
    let markers = publication
        .objects
        .iter()
        .filter(|object| object.name == "marker" && object.id.is_some())
        .collect::<Vec<_>>();
    assert_eq!(markers.len(), 2);
    let mut owners = BTreeSet::new();
    for (ordinal, spelling) in ["const marker = prepare()", "const marker = prepare() + 1"]
        .into_iter()
        .enumerate()
    {
        let start = CALLBACK_BINDINGS.find(spelling).expect("written binding");
        let expected = format!("marker#{ordinal}");
        let object = markers
            .iter()
            .find(|object| {
                spelled(object.id.as_ref().expect("placed local").as_str()).last()
                    == Some(&expected)
            })
            .expect("separate lexical identity");
        let binding = publication
            .declarations
            .iter()
            .find(|row| Some(&row.symbol) == object.id.as_ref())
            .expect("written binding facet");
        assert_eq!(
            binding.range.start,
            u64::try_from(start).expect("source offset")
        );
        assert_eq!(binding.source, spelling);
        assert!(binding.source_complete && !binding.public && object.document_local);
        let owner = object.container.as_ref().expect("physical callable owner");
        assert!(owners.insert(owner));
        assert!(publication.declarations.iter().any(|row| {
            row.symbol == *owner
                && row.unit == binding.unit
                && row.source_complete
                && row.range.start < binding.range.start
                && binding.range.end < row.range.end
                && row.syntax_element_kind.as_deref() == Some("arrow_function")
        }));
    }
}

#[test]
fn typescript_callback_locals_coexist_with_same_spelled_formals() {
    let source = r#"
import { prepare } from "../index.js";
suite(() => {
    check(() => { const marker = prepare(); return marker; });
    check(() => { const marker = prepare() + 1; return marker; });
    check((marker: number) => {
        consume(marker);
        {
            const marker = prepare() + 2;
            return marker;
        }
    });
});
"#;
    let found = analyze(
        CALLBACK_MANIFEST,
        &[
            ("index.js", "export function prepare() { return 3; }"),
            ("lib/check.ts", source),
        ],
    );
    let publication = found.publication();
    publication
        .validate_facts()
        .expect("valid callback bindings");
    let bindings = publication
        .objects
        .iter()
        .filter_map(|object| {
            (object.name == "marker")
                .then_some(object.id.as_ref())
                .flatten()
                .map(|id| (object, id))
        })
        .collect::<Vec<_>>();
    assert_eq!(bindings.len(), 4);
    let mut owners = BTreeSet::new();
    let mut shadow_owner = None;
    for (ordinal, expression) in ["prepare()", "prepare() + 1", "prepare() + 2"]
        .into_iter()
        .enumerate()
    {
        let written = format!("const marker = {expression}");
        let expected = format!("marker#{ordinal}");
        let (object, id) = bindings
            .iter()
            .find(|(_, id)| spelled(id.as_str()).last() == Some(&expected))
            .expect("separate private binding");
        let facets = publication
            .declarations
            .iter()
            .filter(|row| &row.symbol == *id)
            .collect::<Vec<_>>();
        assert_eq!(facets.len(), 1);
        let binding = facets[0];
        let start = source.find(&written).expect("written binding");
        assert_eq!(
            binding.range.start,
            u64::try_from(start).expect("source offset")
        );
        assert_eq!(binding.source, written);
        assert!(binding.source_complete && !binding.public && object.document_local);
        let owner = object.container.as_ref().expect("physical callback");
        assert!(owners.insert(owner));
        assert!(publication.declarations.iter().any(|row| {
            row.symbol == *owner
                && row.unit == binding.unit
                && row.range.start < binding.range.start
                && binding.range.end < row.range.end
                && row.syntax_element_kind.as_deref() == Some("arrow_function")
        }));
        shadow_owner = Some(owner);
    }
    let formal = bindings
        .iter()
        .find(|(object, _)| object.kinds.contains(&SymbolKind::Parameter))
        .expect("written arrow formal");
    let mut expected_formal_path = spelled(shadow_owner.expect("shadowing callback").as_str());
    expected_formal_path.push("marker".to_owned());
    assert_eq!(spelled(formal.1.as_str()), expected_formal_path);
    let facets = publication
        .declarations
        .iter()
        .filter(|row| &row.symbol == formal.1)
        .collect::<Vec<_>>();
    assert_eq!(facets.len(), 1);
    assert_eq!(facets[0].source, "marker");
    assert_eq!(
        facets[0].range.start,
        u64::try_from(source.find("marker:").expect("written parameter")).expect("source offset")
    );
}

#[test]
fn repeated_typescript_bindings_refuse_incomplete_callback_groups() {
    let missing_owner = CALLBACK_BINDINGS.replace("prepare() + 1", "4");
    let unsupported = CALLBACK_BINDINGS.replace(
        "const marker = prepare() + 1; return marker;",
        "function marker() { return prepare() + 1; } return marker();",
    );
    for source in [&missing_owner, &unsupported] {
        let found = analyze(
            CALLBACK_MANIFEST,
            &[
                ("index.js", "export function prepare() { return 3; }"),
                ("lib/check.ts", source),
            ],
        );
        assert!(
            found
                .publication()
                .objects
                .iter()
                .all(|object| object.name != "marker" || object.id.is_none())
        );
    }
}

#[test]
fn checked_callable_initializers_keep_written_argument_shorthands() {
    const META: &str = r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.js"}"#;
    const ENTRY: &str = "export { renderer } from './impl.js';\n";
    for initializer in ["function (input)", "(input) =>"] {
        let source = format!(
            "const inspect = input => input.width;\n\
             export const renderer = {initializer} {{\n\
                 const width = input.width;\n\
                 const height = input.height;\n\
                 return inspect({{ width, height }});\n\
             }};\n"
        );
        let indexed = analyze(META, &[("index.js", ENTRY), ("impl.js", &source)]);
        let graph = indexed.publication();
        graph
            .validate_facts()
            .expect("checked callable initializer");
        let owner = graph
            .declarations
            .iter()
            .find(|row| row.source.starts_with(&format!("renderer = {initializer}")))
            .expect("written callable initializer");
        for (name, written) in [
            ("width", "width = input.width"),
            ("height", "height = input.height"),
        ] {
            let binding = graph
                .declarations
                .iter()
                .find(|row| row.source == written)
                .expect("written binding");
            let property = graph
                .declarations
                .iter()
                .find(|row| {
                    row.source == name
                        && row.syntax_element_kind.as_deref()
                            == Some("shorthand_property_identifier")
                })
                .expect("written direct argument property");
            assert_ne!(binding.symbol, property.symbol);
            assert_eq!(
                property.range.start,
                u64::try_from(source.rfind(name).expect("written key")).expect("source offset")
            );
            assert!(graph.objects.iter().any(|object| {
                object.id.as_ref() == Some(&property.symbol)
                    && object.container.as_ref() == Some(&owner.symbol)
            }));
            assert!(graph.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Reads]
                    && edge.to == binding.symbol
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.unit == property.unit && site.range == property.range
                    })
            }));
        }

        for altered in [
            source.replace("{ width, height }", "{ width, width, height, height }"),
            source.replace(
                "inspect({ width, height })",
                "inspect(function () { return inspect({ width, height }); })",
            ),
        ] {
            let indexed = analyze(META, &[("index.js", ENTRY), ("impl.js", &altered)]);
            let publication = indexed.publication();
            assert!(!publication.declarations.iter().any(|row| {
                ["width", "height"].contains(&row.source.as_str())
                    && row.syntax_element_kind.as_deref() == Some("shorthand_property_identifier")
                    && publication.objects.iter().any(|object| {
                        object.id.as_ref() == Some(&row.symbol)
                            && object.container.as_ref() == Some(&owner.symbol)
                    })
            }));
        }
    }
}

#[test]
fn checked_commonjs_argument_pair_keeps_the_written_key_beside_its_binding() {
    const META: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"./index.js"}"#;
    const ENTRY: &str = "module.exports = require('./holder.js');\n";
    const SOURCE: &str = concat!(
        "(function () {\n",
        "    function keep(value, marker) { return value; }\n",
        "    function collect(limit) {\n",
        "        var values, marker = 0;\n",
        "        values = [];\n",
        "        if (limit > 0) values.push(limit);\n",
        "        return keep({ kind: 1, values: values }, marker);\n",
        "    }\n",
        "    exports.collect = collect;\n",
        "}());\n",
    );
    let collect = |source: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::TypeScript,
            vec![("index.js", ENTRY), ("holder.js", source)],
            None,
            Some(("package.json", META)),
        )
        .expect("selected CommonJS package")
    };
    let indexed = collect(SOURCE);
    let graph = indexed.publication();
    graph
        .validate_facts()
        .expect("exact CommonJS argument identity");
    let owner = graph
        .declarations
        .iter()
        .find(|row| row.source.starts_with("function collect(limit)"))
        .expect("written function declaration");
    let binding = graph
        .declarations
        .iter()
        .find(|row| {
            row.source == "values"
                && row.syntax_element_kind.as_deref() == Some("variable_declarator")
        })
        .expect("written local binding");
    let property = graph
        .declarations
        .iter()
        .find(|row| row.source == "values: values")
        .expect("written argument pair");
    assert_ne!(binding.symbol, property.symbol);
    assert!(graph.objects.iter().any(|object| {
        object.id.as_ref() == Some(&property.symbol)
            && object.container.as_ref() == Some(&owner.symbol)
    }));
    let read =
        u64::try_from(SOURCE.rfind("values").expect("written pair value")).expect("source offset");
    assert!(graph.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads]
            && edge.to == binding.symbol
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|site| site.unit == property.unit && site.range.start == read)
    }));

    for altered in [
        SOURCE.replace("values: values", "values: values, values: values"),
        SOURCE.replace(
            "keep({ kind: 1, values: values }, marker)",
            "keep(function () { return keep({ kind: 1, values: values }, marker); }, marker)",
        ),
    ] {
        let indexed = collect(&altered);
        let publication = indexed.publication();
        assert!(!publication.declarations.iter().any(|row| {
            row.source == "values: values"
                && publication.objects.iter().any(|object| {
                    object.id.as_ref() == Some(&row.symbol)
                        && object.container.as_ref() == Some(&owner.symbol)
                })
        }));
    }
}

#[test]
fn checked_instance_method_keeps_distinct_return_property_and_local_identities() {
    const META: &str = r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.js"}"#;
    const ENTRY: &str = "export { Renderer } from './worker.js';\n";
    const BODY: &str = concat!(
        "const width = value.width;\n",
        "if (value.first) return { width };\n",
        "return { width: width };\n",
    );
    let collect = |method: &str, body: &str| {
        let source = format!("export class Renderer {{ {method}(value) {{ {body} }} }}\n");
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::TypeScript,
            vec![("index.js", ENTRY), ("worker.js", &source)],
            None,
            Some(("package.json", META)),
        )
        .expect("checked class fixture");
        (analysis, source)
    };
    for method in ["select", "static select"] {
        let (analysis, source) = collect(method, BODY);
        let graph = analysis.publication();
        graph.validate_facts().expect("checked method publication");
        let owner = graph
            .declarations
            .iter()
            .find(|row| row.syntax_element_kind.as_deref() == Some("method_definition"))
            .expect("written method");
        let binding = graph
            .declarations
            .iter()
            .find(|row| row.source == "width = value.width")
            .expect("written local binding");
        let properties = ["width", "width: width"].map(|text| {
            graph
                .declarations
                .iter()
                .find(|row| row.source == text)
                .expect("written return property")
        });
        assert_ne!(properties[0].symbol, properties[1].symbol);
        for (property, occurrence) in properties.into_iter().zip(["width };", "width: width };"]) {
            assert_ne!(binding.symbol, property.symbol);
            assert!(graph.objects.iter().any(|object| {
                object.id.as_ref() == Some(&property.symbol)
                    && object.container.as_ref() == Some(&owner.symbol)
            }));
            let start = source.find(occurrence).expect("written value occurrence")
                + occurrence.find(':').map_or(0, |separator| separator + 2);
            assert!(graph.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Reads]
                    && edge.to == binding.symbol
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.unit == property.unit && usize::try_from(site.range.start) == Ok(start)
                    })
            }));
        }
    }

    let (analysis, _) = collect(
        "select",
        "const width = value.width; const inner = () => {return { width };}; return inner();",
    );
    let graph = analysis.publication();
    let method = graph
        .declarations
        .iter()
        .find(|row| row.syntax_element_kind.as_deref() == Some("method_definition"))
        .expect("written enclosing method");
    assert!(!graph.declarations.iter().any(|row| {
        row.syntax_element_kind.as_deref() == Some("shorthand_property_identifier")
            && row.source == "width"
            && graph.objects.iter().any(|object| {
                object.id.as_ref() == Some(&row.symbol)
                    && object.container.as_ref() == Some(&method.symbol)
            })
    }));
}

#[test]
fn checked_compound_argument_preserves_the_property_and_bare_value_read() {
    const META: &str = r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.js"}"#;
    const ENTRY: &str = "export { renderer } from './worker.js';\n";
    let collect = |source: &str| analyze(META, &[("index.js", ENTRY), ("worker.js", source)]);
    for (owner_source, value, sibling) in [
        ("function renderer(input)", "[...plugins, input]", ""),
        ("function renderer(input)", "input ? plugins : []", ""),
        (
            "const renderer = (input) =>",
            "[...plugins, input]",
            "inspect({ plugins });",
        ),
    ] {
        let source = format!(
            "const inspect = value => value;\n\
             export {owner_source} {{ let plugins = [input]; {sibling}\
             return inspect({{ plugins: {value}, ready: true }}); }}\n"
        );
        let analysis = collect(&source);
        let graph = analysis.publication();
        graph
            .validate_facts()
            .expect("checked compound pair publication");
        let owner = graph
            .declarations
            .iter()
            .find(|row| {
                row.source
                    .starts_with(owner_source.trim_start_matches("const "))
            })
            .expect("checked callable");
        let binding = graph
            .declarations
            .iter()
            .find(|row| row.source == "plugins = [input]")
            .expect("written local");
        let text = format!("plugins: {value}");
        let property = graph
            .declarations
            .iter()
            .find(|row| row.source == text)
            .expect("written compound pair");
        assert_ne!(property.symbol, binding.symbol);
        assert!(graph.objects.iter().any(|object| {
            object.id.as_ref() == Some(&property.symbol)
                && object.container.as_ref() == Some(&owner.symbol)
        }));
        let start = source.find(&text).expect("pair source")
            + "plugins: ".len()
            + value.find("plugins").expect("bare read source");
        assert!(graph.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge.to == binding.symbol
                && edge.occurrence.as_ref().is_some_and(|site| {
                    site.unit == property.unit && usize::try_from(site.range.start) == Ok(start)
                })
        }));
        if !sibling.is_empty() {
            let shorthand = graph
                .declarations
                .iter()
                .find(|row| {
                    row.source == "plugins"
                        && row.syntax_element_kind.as_deref()
                            == Some("shorthand_property_identifier")
                })
                .expect("separate written shorthand");
            assert_ne!(shorthand.symbol, property.symbol);
            assert_ne!(shorthand.symbol, binding.symbol);
        }
    }

    for value in [
        "(plugins = [])",
        "input.plugins",
        "(function () { return plugins; })()",
        "[...plugins], plugins: [...plugins]",
    ] {
        let source = format!(
            "const inspect = value => value;\n\
             export function renderer(input) {{ let plugins = [input];\
             return inspect({{ plugins: {value}, ready: true }}); }}\n"
        );
        let analysis = collect(&source);
        assert!(analysis.publication().declarations.iter().all(|row| {
            row.syntax_element_kind.as_deref() != Some("pair")
                || !row.source.starts_with("plugins:")
        }));
    }
}
