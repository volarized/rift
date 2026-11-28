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
fn test_python_dotted_imports_retain_same_name_alias_targets() {
    let source = "import http.client as http\nimport http.server as server\n\
                  def run(): return http.HTTPConnection('host'), server.HTTPServer()\n";
    let result = resolution("python", &[("app/main.py", source)], &[], WORK_MAX);
    assert_eq!(
        targets(&result, ReferenceRole::Import),
        BTreeSet::from([
            "rift://symbol/stdlib/cpython/python/http/client",
            "rift://symbol/stdlib/cpython/python/http/server",
        ]),
    );
    assert_eq!(
        targets(&result, ReferenceRole::Call),
        BTreeSet::from([
            "rift://symbol/stdlib/cpython/python/http/client/HTTPConnection",
            "rift://symbol/stdlib/cpython/python/http/server/HTTPServer",
        ]),
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
            "rift://symbol/stdlib/node/typescript/fs/readFileSync",
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
                "rift://symbol/stdlib/node/typescript/util/TextEncoder",
            ),
        ])
    );
    assert!(result.occurrences.iter().any(|occurrence| {
        occurrence.role == ReferenceRole::Type
            && matches!(&occurrence.target, rift_provider::NormalizedTarget::Symbol(id)
                if id.as_str() == "rift://symbol/local/typescript/.%2Fnear.ts/Local")
    }));
    assert_eq!(result.omitted.get(&EdgeKind::HasType), Some(&2));
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
