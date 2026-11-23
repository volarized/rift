//! Targets owned by dependencies and standard libraries.

use std::collections::BTreeSet;

use super::{WORK_MAX, resolution};
use crate::static_resolution::StaticResolution;
use rift_core::ReferenceRole;
use rift_protocol::read::EdgeKind;

fn targets(result: &StaticResolution, role: ReferenceRole) -> BTreeSet<&str> {
    result
        .external
        .iter()
        .filter(|occurrence| occurrence.role == role)
        .map(|occurrence| occurrence.target.as_str())
        .collect()
}

const CARGO: &str = "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n\
                     [dependencies]\nserde_json = \"1\"\nrenamed = { package = \"real-name\", version = \"1\" }\n\
                     sibling = { path = \"../sibling\" }\n";

#[test]
fn rust_sites_name_registry_crates_and_the_standard_library() {
    let source = "use std::fmt;\nuse serde_json::to_string;\n\
                  pub struct Task;\n\
                  impl fmt::Display for Task {\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { Ok(()) }\n}\n\
                  pub fn encode() -> String { to_string(&1).unwrap() }\n\
                  pub fn build() { let _ = Vec::<u8>::new(); let _ = renamed::open(); let _ = sibling::open(); }\n\
                  pub fn value() { serde_json::from_str::<u8>(\"1\").ok(); }\n";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", CARGO)],
        WORK_MAX,
    );
    let calls = targets(&result, ReferenceRole::Call);
    assert!(
        calls.contains("rift://symbol/cargo/crates.io/serde_json/rust/serde_json/to_string"),
        "{calls:?}"
    );
    assert!(calls.contains("rift://symbol/cargo/crates.io/real-name/rust/real_name/open"));
    assert!(calls.contains("rift://symbol/stdlib/rust/rust/alloc/vec/Vec/new"));
    assert!(calls.contains("rift://symbol/stdlib/rust/rust/core/result/Result/Ok"));
    assert!(!calls.iter().any(|target| target.contains("sibling")));
    let imports = targets(&result, ReferenceRole::Import);
    assert!(imports.contains("rift://symbol/stdlib/rust/rust/std/fmt"));
    assert!(imports.contains("rift://symbol/cargo/crates.io/serde_json/rust/serde_json/to_string"));
    let implemented = result
        .external
        .iter()
        .filter(|occurrence| occurrence.edge_kind == Some(EdgeKind::Implements))
        .map(|occurrence| {
            (
                occurrence
                    .edge_from
                    .as_ref()
                    .map(rift_core::SymbolId::as_str),
                occurrence.target.as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        implemented,
        [(
            Some("rift://symbol/local/rust/fixture/Task"),
            "rift://symbol/stdlib/rust/rust/std/fmt/Display"
        )]
    );
    assert!(
        result
            .external
            .iter()
            .all(|occurrence| !occurrence.declared)
    );
}

#[test]
fn rust_root_declared_by_a_selected_source_names_no_dependency() {
    let result = resolution(
        "rust",
        &[
            (
                "src/lib.rs",
                "mod serde_json;\nmod core { pub fn start() {} }\n\
                 pub fn run() { serde_json::missing(); core::absent(); }\n",
            ),
            ("src/serde_json.rs", "pub fn to_string() {}\n"),
        ],
        &[("Cargo.toml", CARGO)],
        WORK_MAX,
    );
    assert!(result.external.is_empty(), "{:?}", result.external);
    assert_eq!(result.omitted.get(&EdgeKind::Calls), Some(&2));
}

#[test]
fn python_sites_name_distributions_builtins_and_standard_modules() {
    let source = "import os\nimport json as codec\nfrom greeting.core import greet, Base\n\
                  import unknown_module\nfrom .sibling import near\n\n\
                  class Child(Base):\n    pass\n\n\
                  def hello(items):\n    greet()\n    codec.dumps(items)\n    os.path.join('a', 'b')\n\
                  \x20   unknown_module.run()\n    return len(items)\n";
    let result = resolution(
        "python",
        &[("app/main.py", source), ("app/sibling.py", "near = 1\n")],
        &[(
            "pyproject.toml",
            "[project]\nname = \"app\"\nversion = \"1\"\ndependencies = [\"Greeting>=1.0\", \"local @ file:///x\"]\n",
        )],
        WORK_MAX,
    );
    let calls = targets(&result, ReferenceRole::Call);
    assert_eq!(
        calls,
        BTreeSet::from([
            "rift://symbol/pypi/pypi.org/greeting/python/greeting/core/greet",
            "rift://symbol/stdlib/cpython/python/builtins/len",
            "rift://symbol/stdlib/cpython/python/json/dumps",
            "rift://symbol/stdlib/cpython/python/os/path/join",
        ])
    );
    let imports = targets(&result, ReferenceRole::Import);
    assert!(imports.contains("rift://symbol/pypi/pypi.org/greeting/python/greeting/core/Base"));
    let extended = result
        .external
        .iter()
        .filter(|occurrence| occurrence.edge_kind == Some(EdgeKind::Extends))
        .map(|occurrence| occurrence.target.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        extended,
        ["rift://symbol/pypi/pypi.org/greeting/python/greeting/core/Base"]
    );
}

#[test]
fn typescript_sites_name_packages_and_node_modules() {
    let source = "import { z, ZodType } from \"zod\";\nimport * as fs from \"node:fs\";\n\
                  import parse from \"@scope/parser/sub\";\nimport { near } from \"./near\";\n\
                  import { hidden } from \"undeclared\";\nimport { other } from \"aliased\";\n\
                  export class Shape implements ZodType {}\n\
                  export function build() { z.string(); fs.readFileSync(\"a\"); parse(); hidden(); other(); near(); }\n";
    let result = resolution(
        "typescript",
        &[
            ("src/app.ts", source),
            ("src/near.ts", "export function near() {}\n"),
        ],
        &[(
            "package.json",
            "{\"name\":\"app\",\"dependencies\":{\"zod\":\"^4\",\"@scope/parser\":\"1.0.0\",\
             \"aliased\":\"npm:real@^2\",\"linked\":\"workspace:*\"}}",
        )],
        WORK_MAX,
    );
    let calls = targets(&result, ReferenceRole::Call);
    assert_eq!(
        calls,
        BTreeSet::from([
            "rift://symbol/npm/npmjs.org/@scope/parser/typescript/@scope%2Fparser%2Fsub/default",
            "rift://symbol/npm/npmjs.org/real/typescript/real/other",
            "rift://symbol/npm/npmjs.org/zod/typescript/zod/z/string",
            "rift://symbol/stdlib/node/javascript/fs/readFileSync",
        ])
    );
    let imports = targets(&result, ReferenceRole::Import);
    assert!(imports.contains("rift://symbol/npm/npmjs.org/zod/typescript/zod/ZodType"));
    assert!(!imports.iter().any(|target| target.contains("undeclared")));
    let implemented = result
        .external
        .iter()
        .filter(|occurrence| occurrence.edge_kind == Some(EdgeKind::Implements))
        .map(|occurrence| occurrence.target.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        implemented,
        ["rift://symbol/npm/npmjs.org/zod/typescript/zod/ZodType"]
    );
    assert!(
        result
            .omitted
            .get(&EdgeKind::Calls)
            .is_some_and(|count| *count >= 1)
    );
}

#[test]
fn typescript_type_uses_name_external_imports_after_local_resolution() {
    let source = r#"import type { ZodType as Imported } from "shape";
import type * as node from "node:util";
import type * as sibling from "./near";
import type { Missing } from "undeclared";
import type { Linked } from "linked";
import type { ZodType as Contested } from "shape";
import type { Missing as Contested } from "undeclared";
export type Schema = Imported;
export type Encoder = node.TextEncoder;
export type Neighbor = sibling.Local;
export type Unknown = Missing | Linked;
export type Ambiguous = Contested;
export type ValueQuery = typeof Imported;
export function scoped<Imported>(value: Imported): Imported { return value; }
"#;
    let sources = [
        ("index.ts", source),
        ("near.ts", "export interface Local {}\n"),
    ];
    let metadata = [(
        "package.json",
        r#"{"name":"app","dependencies":{"shape":"npm:zod@^4","linked":"workspace:*"}}"#,
    )];
    let result = resolution("typescript", &sources, &metadata, WORK_MAX);
    let external = result
        .external
        .iter()
        .filter(|occurrence| occurrence.role == ReferenceRole::Type)
        .map(|occurrence| {
            let range = occurrence.binding.range();
            let start = usize::try_from(range.start()).expect("type use start");
            let end = usize::try_from(range.end()).expect("type use end");
            (start, &source[start..end], occurrence.target.as_str())
        })
        .collect::<BTreeSet<_>>();
    let schema = source.find("= Imported;").expect("schema type") + 2;
    let encoder = source.find("node.TextEncoder").expect("namespace type");
    assert_eq!(
        external,
        BTreeSet::from([
            (
                schema,
                "Imported",
                "rift://symbol/npm/npmjs.org/zod/typescript/zod/ZodType",
            ),
            (
                encoder,
                "node.TextEncoder",
                "rift://symbol/stdlib/node/javascript/util/TextEncoder",
            ),
        ])
    );
    assert!(result.occurrences.iter().any(|occurrence| {
        occurrence.role == ReferenceRole::Type
            && matches!(&occurrence.target, rift_provider::NormalizedTarget::Symbol(id)
                if id.as_str() == "rift://symbol/local/typescript/.%2Fnear.ts/Local")
    }));
    // `Missing`, `Linked`, and the ambiguous `Contested` name no owner.
    assert_eq!(result.omitted.get(&EdgeKind::HasType), Some(&3));
    let exhausted = resolution("typescript", &sources, &metadata, 0);
    assert!(exhausted.exhausted);
    assert!(exhausted.external.is_empty());
}

#[test]
fn python_annotation_sites_name_owned_types_without_crossing_generic_bindings() {
    let source = "from greeting import Base, T\nimport greeting as pkg\nclass Holder:\n    item: Base\n    other: pkg.Base\n\ndef generic[T](value: T) -> T:\n    pass\n";
    let result = resolution(
        "python",
        &[("app.py", source)],
        &[(
            "pyproject.toml",
            "[project]\nname='app'\nversion='1.0.0'\ndependencies=['greeting']\n",
        )],
        WORK_MAX,
    );
    let types = targets(&result, ReferenceRole::Type);
    assert!(
        types.contains("rift://symbol/pypi/pypi.org/greeting/python/greeting/Base"),
        "{types:?}"
    );
    assert!(
        !types.contains("rift://symbol/pypi/pypi.org/greeting/python/greeting/T"),
        "{types:?}"
    );
}

/// Targets of one role with the source text of each site.
fn sites<'a>(
    result: &'a StaticResolution,
    role: ReferenceRole,
    source: &'a str,
) -> BTreeSet<(&'a str, &'a str)> {
    result
        .external
        .iter()
        .filter(|occurrence| occurrence.role == role)
        .filter_map(|occurrence| {
            let range = occurrence.binding.range();
            let start = usize::try_from(range.start()).ok()?;
            let end = usize::try_from(range.end()).ok()?;
            Some((source.get(start..end)?, occurrence.target.as_str()))
        })
        .collect()
}

const ZOD: &str = "{\"name\":\"app\",\"dependencies\":{\"zod\":\"^4\"}}";

#[test]
fn ecmascript_module_imports_and_reexports_name_the_package_module() {
    let source = "import * as z from \"zod\";\nimport * as lost from \"undeclared\";\n\
                  export * from \"zod\";\nexport { ZodError as Failure } from \"zod\";\n\
                  export * as all from \"zod\";\nexport { gone } from \"undeclared\";\n";
    let result = resolution(
        "typescript",
        &[("src/app.ts", source)],
        &[("package.json", ZOD)],
        WORK_MAX,
    );
    let module = "rift://symbol/npm/npmjs.org/zod/typescript/zod";
    assert_eq!(
        sites(&result, ReferenceRole::Import, source),
        BTreeSet::from([
            ("z", module),
            ("export * from \"zod\";", module),
            ("export { ZodError as Failure } from \"zod\";", module),
            (
                "ZodError",
                "rift://symbol/npm/npmjs.org/zod/typescript/zod/ZodError"
            ),
            ("export * as all from \"zod\";", module),
        ])
    );
    assert_eq!(result.omitted.get(&EdgeKind::Imports), Some(&2));
    assert!(result.external.iter().all(|occurrence| {
        occurrence
            .edge_from
            .as_ref()
            .is_some_and(|from| from.as_str() == "rift://symbol/local/typescript/.%2Fsrc%2Fapp.ts")
    }));
}

#[test]
fn commonjs_requires_name_the_package_module_and_its_members() {
    let source = "const zod = require(\"zod\");\nconst { parse } = require(\"zod\");\n\
                  function run() { zod.object(parse(1)); }\n";
    let result = resolution(
        "javascript",
        &[("src/app.cjs", source)],
        &[("package.json", ZOD)],
        WORK_MAX,
    );
    let prefix = "rift://symbol/npm/npmjs.org/zod/javascript/zod";
    assert_eq!(
        sites(&result, ReferenceRole::Import, source),
        BTreeSet::from([
            ("zod", prefix),
            ("parse", format!("{prefix}/parse").as_str()),
        ])
    );
    assert_eq!(
        targets(&result, ReferenceRole::Call),
        BTreeSet::from([
            format!("{prefix}/object").as_str(),
            format!("{prefix}/parse").as_str(),
        ])
    );
    assert_eq!(result.omitted.get(&EdgeKind::Imports), None);
}

#[test]
fn value_accesses_of_imports_name_dependency_declarations() {
    let source = "import { limit, schema } from \"zod\";\nimport { lost } from \"undeclared\";\n\
                  export function run(local) { const held = limit; return [schema.shape, lost, local, undefined]; }\n";
    let result = resolution(
        "javascript",
        &[("src/app.mjs", source)],
        &[("package.json", ZOD)],
        WORK_MAX,
    );
    let prefix = "rift://symbol/npm/npmjs.org/zod/javascript/zod";
    let reads = sites(&result, ReferenceRole::Read, source);
    assert!(reads.contains(&("limit", format!("{prefix}/limit").as_str())));
    assert!(reads.contains(&("schema.shape", format!("{prefix}/schema/shape").as_str())));
    assert_eq!(
        result.omitted.get(&EdgeKind::Reads),
        Some(&1),
        "only the import without an owner is left out: {reads:?}"
    );

    let source = "import settings\nimport greeting.core\nfrom greeting import core as alias\n\n\
                  def run(value):\n    greeting.core.state = alias.LIMIT\n    return settings.DEBUG\n";
    let result = resolution(
        "python",
        &[("app.py", source)],
        &[(
            "pyproject.toml",
            "[project]\nname='app'\nversion='1.0.0'\ndependencies=['greeting']\n",
        )],
        WORK_MAX,
    );
    let prefix = "rift://symbol/pypi/pypi.org/greeting/python/greeting/core";
    let reads = sites(&result, ReferenceRole::Read, source);
    assert!(reads.contains(&("alias.LIMIT", format!("{prefix}/LIMIT").as_str())));
    assert!(
        sites(&result, ReferenceRole::Write, source)
            .contains(&("greeting.core.state", format!("{prefix}/state").as_str()))
    );
    assert_eq!(result.omitted.get(&EdgeKind::Reads), Some(&1));
    assert!(
        result
            .external
            .iter()
            .filter(|occurrence| occurrence.role != ReferenceRole::Import)
            .all(|occurrence| occurrence
                .edge_from
                .as_ref()
                .is_some_and(|from| from.as_str() == "rift://symbol/local/python/app/run"))
    );
}

#[test]
fn python_dotted_and_repeated_imports_spell_one_root() {
    let source = "import os\nimport os.path\nimport xml.dom\nimport json\nimport greeting as json\n\n\
                  def run():\n    os.path.join('a', 'b')\n    xml.dom.minidom()\n    json.dumps()\n";
    let result = resolution(
        "python",
        &[("app.py", source)],
        &[(
            "pyproject.toml",
            "[project]\nname='app'\nversion='1.0.0'\ndependencies=['greeting']\n",
        )],
        WORK_MAX,
    );
    assert_eq!(
        targets(&result, ReferenceRole::Call),
        BTreeSet::from([
            "rift://symbol/stdlib/cpython/python/os/path/join",
            "rift://symbol/stdlib/cpython/python/xml/dom/minidom",
        ])
    );
    assert!(
        targets(&result, ReferenceRole::Import)
            .contains("rift://symbol/stdlib/cpython/python/xml/dom")
    );
    assert_eq!(result.omitted.get(&EdgeKind::Calls), Some(&1));
}

#[test]
fn python_root_declared_by_a_selected_source_names_no_runtime() {
    let result = resolution(
        "python",
        &[
            (
                "app.py",
                "import types\nimport email\n\ndef run():\n    types.absent()\n    email.absent()\n",
            ),
            ("types.py", "present = 1\n"),
            ("email/__init__.py", ""),
            ("email/inner.py", "value = 1\n"),
        ],
        &[],
        WORK_MAX,
    );
    assert!(result.external.is_empty(), "{:?}", result.external);
    assert_eq!(result.omitted.get(&EdgeKind::Calls), Some(&2));
}

#[test]
fn wildcard_imports_name_their_module_and_leave_names_under_them_unnamed() {
    let source = "from greeting import *\n\ndef run(items):\n    return len(items)\n";
    let result = resolution(
        "python",
        &[("app.py", source)],
        &[(
            "pyproject.toml",
            "[project]\nname='app'\nversion='1.0.0'\ndependencies=['greeting']\n",
        )],
        WORK_MAX,
    );
    assert_eq!(
        targets(&result, ReferenceRole::Import),
        BTreeSet::from(["rift://symbol/pypi/pypi.org/greeting/python/greeting"])
    );
    assert!(targets(&result, ReferenceRole::Call).is_empty());
    assert_eq!(result.omitted.get(&EdgeKind::Calls), Some(&1));

    let source = "use serde_json::*;\nuse absent::*;\npub fn run() { to_string(&1); }\n";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source)],
        &[("Cargo.toml", CARGO)],
        WORK_MAX,
    );
    assert_eq!(
        targets(&result, ReferenceRole::Import),
        BTreeSet::from(["rift://symbol/cargo/crates.io/serde_json/rust/serde_json"])
    );
    assert!(targets(&result, ReferenceRole::Call).is_empty());
    assert_eq!(result.omitted.get(&EdgeKind::Imports), Some(&1));
    assert_eq!(result.omitted.get(&EdgeKind::Calls), Some(&1));
}

#[test]
fn one_target_has_one_language_in_every_file_that_spells_it() {
    let source = "import { parse } from \"zod\";\nimport { plain } from \"plain\";\n\
                  import { readFileSync } from \"node:fs\";\nimport fs, * as all from \"fs\";\n\
                  export function run() { parse(); plain(); readFileSync(\"a\"); fs.readFileSync(\"a\"); all.readFileSync(\"a\"); }\n";
    let result = resolution(
        "typescript",
        &[("src/app.ts", source), ("src/other.mjs", source)],
        &[
            (
                "package.json",
                "{\"name\":\"app\",\"dependencies\":{\"zod\":\"^4\",\"plain\":\"^1\"}}",
            ),
            (
                "node_modules/zod/package.json",
                "{\"name\":\"zod\",\"exports\":{\".\":{\"import\":[\"./index.d.mts\"]}}}",
            ),
            (
                "node_modules/plain/package.json",
                "{\"name\":\"plain\",\"main\":\"index.js\"}",
            ),
        ],
        WORK_MAX,
    );
    assert_eq!(
        targets(&result, ReferenceRole::Call),
        BTreeSet::from([
            "rift://symbol/npm/npmjs.org/plain/javascript/plain/plain",
            "rift://symbol/npm/npmjs.org/zod/typescript/zod/parse",
            "rift://symbol/stdlib/node/javascript/fs/readFileSync",
        ])
    );
}

#[test]
fn packages_of_the_selection_and_other_sources_name_no_registry() {
    let source = "import { known } from \"member\";\nimport buffer from \"buffer/\";\n\
                  export function run() { known(); buffer(); }\n";
    let result = resolution(
        "typescript",
        &[("src/app.ts", source)],
        &[
            (
                "package.json",
                "{\"name\":\"app\",\"dependencies\":{\"member\":\"1.0.0\",\"buffer\":\"6.0.0\"}}",
            ),
            (
                "packages/member/package.json",
                "{\"name\":\"member\",\"version\":\"1.0.0\"}",
            ),
        ],
        WORK_MAX,
    );
    assert!(result.external.is_empty(), "{:?}", result.external);
    assert_eq!(result.omitted.get(&EdgeKind::Imports), Some(&2));

    let source = "import sourced\nimport private\nimport listed\nimport plain\n";
    let result = resolution(
        "python",
        &[("app.py", source)],
        &[(
            "pyproject.toml",
            "[project]\nname='app'\nversion='1.0.0'\ndependencies=['sourced', 'plain']\n\
             [tool.uv.sources]\nsourced = { git = 'https://example.com/sourced' }\n\
             [tool.poetry.dependencies]\nprivate = { version = '1', source = 'internal' }\n\
             listed = [{ version = '1' }, { path = '../listed' }]\n",
        )],
        WORK_MAX,
    );
    assert_eq!(
        targets(&result, ReferenceRole::Import),
        BTreeSet::from(["rift://symbol/pypi/pypi.org/plain/python/plain"])
    );
    assert_eq!(result.omitted.get(&EdgeKind::Imports), Some(&3));
}

#[test]
fn an_uppercase_crate_name_names_no_owner() {
    let result = resolution(
        "rust",
        &[(
            "src/lib.rs",
            "pub fn run() { Inflector::to_plural(\"a\"); }\n",
        )],
        &[(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[dependencies]\nInflector = \"0.11\"\n",
        )],
        WORK_MAX,
    );
    assert!(result.external.is_empty(), "{:?}", result.external);
    assert_eq!(result.omitted.get(&EdgeKind::Calls), Some(&1));
}

#[test]
fn omissions_are_counted_for_each_unit_and_kind() {
    let result = resolution(
        "rust",
        &[
            (
                "src/lib.rs",
                "pub mod other;\npub struct Held;\nimpl Held { pub fn own(&self) -> Self { Held } }\n\
                 pub fn generic<T>(value: T) -> T { value }\npub fn known(held: &Held) { absent::run(); }\n",
            ),
            (
                "src/other.rs",
                "pub struct Wrapped(pub absent::Kind);\npub fn read(wrapped: &Wrapped) -> u8 { absent::make().field }\n",
            ),
        ],
        &[("Cargo.toml", CARGO)],
        WORK_MAX,
    );
    let by_unit = result
        .edge_calls
        .iter()
        .map(|(unit, coverage)| (unit.key().as_str(), coverage.omitted.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        by_unit["src/lib.rs"],
        [(EdgeKind::Calls, 1)].into_iter().collect()
    );
    assert_eq!(
        by_unit["src/other.rs"],
        [
            (EdgeKind::Calls, 1),
            (EdgeKind::HasType, 1),
            (EdgeKind::Reads, 1)
        ]
        .into_iter()
        .collect()
    );
}
