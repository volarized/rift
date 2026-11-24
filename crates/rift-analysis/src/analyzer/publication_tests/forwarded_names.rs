//! Names an npm package file gives to declarations of its other files.

use super::internal_modules::{MAIN, analyze, edges, path, public};
use super::*;
use rift_protocol::read::EdgeKind;
use std::collections::BTreeSet;

const MODULE: &str = r#"{"name":"beacon","version":"1.0.0","type":"module","main":"index.js"}"#;
const TYPES: &str = r#"{"name":"beacon","version":"1.0.0","types":"index.d.ts"}"#;
const INNER: &str = "export function u() {}
export default function given() {}
export class K { m() {} }
";

/// Every alias of `module`, with the declaration it names.
fn aliases(analysis: &PackageAnalysis, module: &str) -> BTreeSet<(String, String)> {
    edges(analysis)
        .into_iter()
        .filter(|(from, kind, _)| *kind == EdgeKind::Aliases && from[0] == module)
        .map(|(from, _, to)| (from[1..].join("/"), to.join("/")))
        .collect()
}

fn expected(rows: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    rows.iter()
        .map(|(name, target)| ((*name).to_owned(), (*target).to_owned()))
        .collect()
}

#[test]
fn an_import_exported_again_is_an_alias_of_its_declaration() {
    for (entry, names) in [
        (
            "import { u } from './lib/x.js';\nexport { u };\n",
            &[("u", "beacon/lib/x.js/u")][..],
        ),
        (
            "import { u as local } from './lib/x.js';\nexport { local as z };\n",
            &[("z", "beacon/lib/x.js/u")],
        ),
        (
            "import d from './lib/x.js';\nexport { d };\nexport default d;\n",
            &[
                ("d", "beacon/lib/x.js/given"),
                ("default", "beacon/lib/x.js/given"),
            ],
        ),
        (
            "import * as ns from './lib/x.js';\nexport { ns };\n",
            &[("ns", "beacon/lib/x.js")],
        ),
        (
            "export { u } from './lib/x.js';\nexport { default as max } from './lib/x.js';\n",
            &[("u", "beacon/lib/x.js/u"), ("max", "beacon/lib/x.js/given")],
        ),
        (
            "import { gone } from './lib/x.js';\nexport { gone };\n",
            &[],
        ),
        ("import { u } from 'other';\nexport { u };\n", &[]),
        (
            "import { u as local } from './lib/x.js';\nexport { local as K };\nclass K {}\n",
            &[],
        ),
        ("import { u } from './lib/x.js';\nu();\n", &[]),
    ] {
        let analysis = analyze(MODULE, &[("index.js", entry), ("lib/x.js", INNER)]);
        assert_eq!(aliases(&analysis, "beacon"), expected(names), "{entry}");
        for (name, _) in names.iter().filter(|(name, _)| *name != "ns") {
            assert_eq!(
                public(&analysis, &["beacon", name], "index.js"),
                Some(true),
                "{entry}"
            );
        }
        assert!(public(&analysis, &["beacon/lib/x.js", "u"], "lib/x.js").is_some());
    }
}

#[test]
fn a_declaration_file_forwards_the_names_of_declaration_files() {
    let files = [
        (
            "lib/x.d.ts",
            "export declare function u(): void;\nexport default function given(): void;\n",
        ),
        ("lib/x.js", "exports.u = function u() {};\n"),
        (
            "lib/more.d.ts",
            "export interface I { a: 1 }\nexport { u as again } from './x';\n",
        ),
        ("lib/two.d.ts", "export declare const a: 1;\n"),
        ("lib/two/index.d.ts", "export declare const b: 1;\n"),
        ("lib/dir/index.d.ts", "export type T = string;\n"),
        ("lib/esm.d.mts", "export declare const m: 1;\n"),
        ("lib/only.js", "exports.only = 1;\n"),
        (
            "lib/renamed.d.ts",
            "declare function _enum(a: string): void;\ndeclare function _enum(a: number): void;\ndeclare function _null(): void;\nexport { _enum as enum, _null as null };\n",
        ),
    ];
    for (entry, names) in [
        (
            "export { u } from './lib/x';",
            &[("u", "beacon/lib/x.d.ts/u")][..],
        ),
        (
            "export { u } from './lib/x.js';",
            &[("u", "beacon/lib/x.d.ts/u")],
        ),
        (
            "export { default as max } from './lib/x';",
            &[("max", "beacon/lib/x.d.ts/given")],
        ),
        (
            "export * from './lib/more';",
            &[
                ("I", "beacon/lib/more.d.ts/I"),
                ("again", "beacon/lib/x.d.ts/u"),
            ],
        ),
        (
            "import D, { u } from './lib/x';\nexport { u, D };",
            &[
                ("u", "beacon/lib/x.d.ts/u"),
                ("D", "beacon/lib/x.d.ts/given"),
            ],
        ),
        (
            "export * from './lib/dir';",
            &[("T", "beacon/lib/dir/index.d.ts/T")],
        ),
        (
            "export * from './lib/esm.mjs';",
            &[("m", "beacon/lib/esm.d.mts/m")],
        ),
        (
            "export * from './lib/renamed';",
            &[
                ("enum", "beacon/lib/renamed.d.ts/_enum"),
                ("null", "beacon/lib/renamed.d.ts/_null"),
            ],
        ),
        ("export * from './lib/two';", &[]),
        ("export * from './lib/only';", &[]),
        ("export * from './lib/esm.js';", &[]),
    ] {
        let mut selected = vec![("index.d.ts", entry)];
        selected.extend(files);
        let analysis = analyze(TYPES, &selected);
        assert_eq!(aliases(&analysis, "beacon"), expected(names), "{entry}");
    }
}

#[test]
fn a_commonjs_forward_gives_the_names_and_the_value_of_its_module() {
    let files = [
        ("lib/x.js", "exports.open = function open() {};\n"),
        ("lib/index.js", "exports.main = function main() {};\n"),
        ("lib/fn.js", "function fn() {}\nmodule.exports = fn;\n"),
        (
            "lib/K.js",
            "exports.K = class K {};\nexports.default = function given() {};\n",
        ),
        (
            "lib/literal.js",
            "function a() {}\nfunction b() {}\nmodule.exports = { a: a, b };\n",
        ),
        ("lib/chain.js", "module.exports = require('./literal');\n"),
        (
            "lib/twice.js",
            "exports.K = function () {};\nexports.K = function () {};\n",
        ),
        ("lib-two/x.js", "exports.open = function open() {};\n"),
    ];
    for (entry, names) in [
        (
            "module.exports = require('./lib/x');",
            &[("open", "beacon/lib/x.js/open")][..],
        ),
        (
            "module.exports = require('./lib/');",
            &[("main", "beacon/lib/index.js/main")],
        ),
        (
            "module.exports = require('./lib');",
            &[("main", "beacon/lib/index.js/main")],
        ),
        (
            "module.exports = require('./lib/fn');",
            &[("default", "beacon/lib/fn.js/fn")],
        ),
        (
            "module.exports = require('./lib/K').K;",
            &[("default", "beacon/lib/K.js/K")],
        ),
        (
            "module.exports = require('./lib/K.js').default;",
            &[("default", "beacon/lib/K.js/default")],
        ),
        (
            "module.exports = require('./lib/literal').a;",
            &[("default", "beacon/lib/literal.js/default/a")],
        ),
        (
            "module.exports = require('./lib/chain');",
            &[
                ("a", "beacon/lib/literal.js/default/a"),
                ("b", "beacon/lib/literal.js/b"),
            ],
        ),
        (
            "module.exports = process.env.COV ? require('./lib-cov/x') : require('./lib/x');",
            &[("open", "beacon/lib/x.js/open")],
        ),
        (
            "module.exports = process.env.COV ? require('./lib-cov/fn') : require('./lib/fn');",
            &[("default", "beacon/lib/fn.js/fn")],
        ),
        (
            "module.exports = process.env.COV ? require('./lib-two/x') : require('./lib/x');",
            &[],
        ),
        (
            "module.exports = process.env.COV ? require('./lib/x') : require('./lib/x.js');",
            &[("open", "beacon/lib/x.js/open")],
        ),
        (
            "module.exports = process.env.COV ? require('./data.json') : require('./lib/x');",
            &[],
        ),
        (
            "module.exports = process.env.COV ? require('other') : require('./lib/x');",
            &[],
        ),
        (
            "module.exports = process.env.COV ? require('./lib/x') : other;",
            &[],
        ),
        ("module.exports = require('./lib/K').gone;", &[]),
        ("module.exports = require('./lib/K')[name];", &[]),
        ("module.exports = require('./lib/twice').K;", &[]),
        ("if (cond) module.exports = require('./lib/K').K;", &[]),
        (
            "module.exports = require('./lib/K').K;\nmodule.exports = other;",
            &[],
        ),
        ("module.exports = require('./gone').K;", &[]),
        ("module.exports = require('../lib/x');", &[]),
    ] {
        let mut selected = vec![("index.js", entry)];
        selected.extend(files);
        let analysis = analyze(MAIN, &selected);
        assert_eq!(aliases(&analysis, "beacon"), expected(names), "{entry}");
    }
}

#[test]
fn calls_reach_the_declaration_behind_a_forwarded_property_or_condition() {
    for (entry, call, target) in [
        (
            "module.exports = require('./lib/x').open;",
            "entry()",
            Some("open"),
        ),
        (
            "module.exports = c ? require('./cov/x') : require('./lib/x');",
            "entry.open()",
            Some("open"),
        ),
        (
            "module.exports = c ? require('./two/x') : require('./lib/x');",
            "entry.open()",
            None,
        ),
        (
            "if (c) module.exports = require('./lib/x').open;",
            "entry()",
            None,
        ),
    ] {
        let user = format!(
            "var entry = require('./index');\nfunction use() {{ {call} }}\nexports.use = use;\n"
        );
        let analysis = analyze(
            MAIN,
            &[
                ("index.js", entry),
                ("lib/x.js", "exports.open = function open() {};\n"),
                ("two/x.js", "exports.open = function open() {};\n"),
                ("user.js", &user),
            ],
        );
        let called = edges(&analysis)
            .into_iter()
            .filter(|(from, kind, _)| {
                *kind == EdgeKind::Calls && *from == path(&["beacon/user.js", "use"])
            })
            .map(|(_, _, to)| to)
            .collect::<Vec<_>>();
        assert_eq!(
            called,
            target
                .map(|name| path(&["beacon/lib/x.js", name]))
                .into_iter()
                .collect::<Vec<_>>(),
            "{entry}"
        );
    }
}
