//! The stub and module join through the whole analysis: each paired stub and module
//! declaration answers one symbol, at the module.

use std::collections::BTreeSet;

use rift_protocol::index::{PackagePublication, PackageSymbol};
use rift_protocol::read::TextRange;
use rift_syntax::ShippedLanguage;

use super::StubForm;
use super::fixture::{analyzed, package_analysis};

/// The symbol one publication files under `path` as `qualified_name`.
fn symbol_at<'publication>(
    publication: &'publication PackagePublication,
    path: &str,
    qualified_name: &str,
) -> Option<&'publication PackageSymbol> {
    let suffix = format!("/{path}");
    publication
        .symbols
        .iter()
        .find(|symbol| symbol.unit.0.ends_with(&suffix) && symbol.qualified_name == qualified_name)
}

fn signature_displays(symbol: &PackageSymbol) -> Vec<&str> {
    symbol
        .presentation
        .signatures
        .iter()
        .map(|signature| signature.display.as_str())
        .collect()
}

fn assert_unique_identities(publication: &PackagePublication) {
    let identities: BTreeSet<&str> = publication
        .symbols
        .iter()
        .map(|symbol| symbol.symbol.0.as_str())
        .collect();
    assert_eq!(
        identities.len(),
        publication.symbols.len(),
        "no two records share one identity"
    );
}

/// A stub's `typing.overload` forms join the implementation into one symbol: the
/// implementation address, source, and documentation, with the stub's forms as one
/// signature list. Both files still reach the semantic build under their own identities,
/// so no two records share an identity.
#[test]
fn test_a_python_stub_joins_its_module_into_one_symbol_per_name() {
    let publication = analyzed(
        ShippedLanguage::Python,
        vec![
            (
                "mod.pyi",
                "from typing import overload\n\n@overload\ndef f(x: int) -> int: ...\n@overload\ndef f(x: str) -> str: ...\ndef stubbed(flag: bool) -> None: ...\n",
            ),
            (
                "mod.py",
                "def f(x):\n    \"\"\"Return x unchanged.\"\"\"\n    return x\n\ndef helper():\n    return 1\n",
            ),
        ],
    );

    let joined = symbol_at(&publication, "mod.py", "f").expect("f answers at the module");
    assert_eq!(
        signature_displays(joined),
        ["def f(x: int) -> int:", "def f(x: str) -> str:"]
    );
    assert_eq!(
        joined
            .signature
            .as_ref()
            .map(|signature| signature.display.as_str()),
        Some("def f(x: int) -> int:")
    );
    assert!(
        joined
            .documentation
            .as_ref()
            .is_some_and(|documentation| documentation.text.contains("Return x unchanged."))
    );
    assert!(joined.source.starts_with("def f(x):"));
    assert!(joined.public);
    let document = publication
        .documents
        .iter()
        .find(|document| document.identity == joined.symbol.0)
        .expect("the joined symbol ranks under one document");
    assert_eq!(document.signature.as_deref(), Some("def f(x: int) -> int:"));
    assert!(symbol_at(&publication, "mod.pyi", "f~1").is_none());
    assert!(symbol_at(&publication, "mod.pyi", "f~2").is_none());
    assert!(
        symbol_at(&publication, "mod.pyi", "stubbed").is_some_and(|stubbed| stubbed.public),
        "a stub name the module lacks answers from the stub"
    );
    assert!(
        symbol_at(&publication, "mod.py", "helper").is_some_and(|helper| !helper.public),
        "the stub defines the public set"
    );
    assert_unique_identities(&publication);
}

/// When the module repeats the overload forms before its implementation, the last form is
/// the one the module binds, so it answers for the stub's forms.
#[test]
fn test_a_module_repeating_its_overloads_answers_at_the_implementation_form() {
    let overloads = "from typing import overload\n\n@overload\ndef f(x: int) -> int: ...\n@overload\ndef f(x: str) -> str: ...\n";
    let module = format!("{overloads}def f(x):\n    return x\n");
    let publication = analyzed(
        ShippedLanguage::Python,
        vec![("mod.pyi", overloads), ("mod.py", module.as_str())],
    );

    let joined = symbol_at(&publication, "mod.py", "f~3").expect("the implementation form");
    assert!(joined.public);
    assert!(joined.source.starts_with("def f(x):"));
    assert_eq!(
        signature_displays(joined),
        ["def f(x: int) -> int:", "def f(x: str) -> str:"]
    );
    for form in ["f~1", "f~2"] {
        assert!(symbol_at(&publication, "mod.pyi", form).is_none(), "{form}");
        assert!(
            symbol_at(&publication, "mod.py", form).is_some_and(|symbol| !symbol.public),
            "{form}"
        );
    }
}

/// A stub member whose class joined the module names the class the module declares as its
/// container.
#[test]
fn test_a_stub_member_of_a_joined_class_names_the_module_class_as_container() {
    let publication = analyzed(
        ShippedLanguage::Python,
        vec![
            (
                "mod.pyi",
                "class Client:\n    def open(self) -> int: ...\n    def close(self) -> None: ...\n",
            ),
            (
                "mod.py",
                "class Client:\n    def open(self):\n        return 1\n",
            ),
        ],
    );

    let class = symbol_at(&publication, "mod.py", "Client").expect("the class joins");
    let close = symbol_at(&publication, "mod.pyi", "Client.close").expect("stub-only member");
    assert_eq!(close.presentation.container.as_ref(), Some(&class.symbol));
    assert!(symbol_at(&publication, "mod.py", "Client.open").is_some_and(|open| open.public));
    assert!(symbol_at(&publication, "mod.pyi", "Client.open").is_none());
}

/// A `.d.ts` declaration file joins the `.js` module beside it: the stub's signature over
/// the module's address, and an interface the module cannot declare answers from the stub.
#[test]
fn test_a_typescript_declaration_file_joins_its_javascript_module() {
    let publication = analyzed(
        ShippedLanguage::TypeScript,
        vec![
            (
                "index.d.ts",
                "export declare function open(path: string): Handle;\nexport declare const version: string;\nexport interface Handle {\n  close(): void;\n}\n",
            ),
            (
                "index.js",
                "export function open(path) {\n  return { close() {} };\n}\nexport const version = \"1.0.0\";\nfunction internal() {}\n",
            ),
        ],
    );

    let open = symbol_at(&publication, "index.js", "open").expect("open joins");
    assert_eq!(
        signature_displays(open),
        ["function open(path: string): Handle"]
    );
    assert!(open.public);
    assert!(symbol_at(&publication, "index.js", "version").is_some_and(|value| value.public));
    assert!(symbol_at(&publication, "index.d.ts", "open").is_none());
    assert!(symbol_at(&publication, "index.d.ts", "Handle").is_some());
    let close = symbol_at(&publication, "index.d.ts", "Handle.close").expect("a member signature");
    assert_eq!(signature_displays(close), ["close(): void"]);
    assert!(symbol_at(&publication, "index.js", "internal").is_some_and(|value| !value.public));
    assert_unique_identities(&publication);
}

/// A stub form that renders no signature leaves the module's own: the `.d.ts` declares
/// `parse` as a `const`, and the module's `function parse(text)` keeps its signature.
#[test]
fn test_a_joined_symbol_keeps_the_module_signature_when_no_stub_form_renders_one() {
    let publication = analyzed(
        ShippedLanguage::TypeScript,
        vec![
            (
                "index.d.ts",
                "export declare const parse: (text: string) => Node;\n",
            ),
            (
                "index.js",
                "export function parse(text) {\n  return text;\n}\n",
            ),
        ],
    );

    let parse = symbol_at(&publication, "index.js", "parse").expect("parse joins");
    assert_eq!(signature_displays(parse), ["function parse(text)"]);
    assert_eq!(
        parse
            .signature
            .as_ref()
            .map(|signature| signature.display.as_str()),
        Some("function parse(text)")
    );
    assert!(symbol_at(&publication, "index.d.ts", "parse").is_none());
}

/// Each declaration file joins the build its extension names: `.d.mts` its `.mjs`, and
/// `.d.cts` its `.cjs`, once the JavaScript definition claims both extensions.
#[test]
fn test_a_declaration_file_joins_its_esm_and_commonjs_builds() {
    let publication = analyzed(
        ShippedLanguage::TypeScript,
        vec![
            (
                "dist/index.d.mts",
                "export declare function load(path: string): Buffer;\n",
            ),
            (
                "dist/index.mjs",
                "export function load(path) {\n  return path;\n}\n",
            ),
            (
                "dist/index.d.cts",
                "export declare function load(path: string, flag: boolean): Buffer;\n",
            ),
            (
                "dist/index.cjs",
                "function load(path, flag) {\n  return path;\n}\nmodule.exports = { load };\n",
            ),
        ],
    );

    let esm = symbol_at(&publication, "dist/index.mjs", "load").expect("the ESM build joins");
    assert_eq!(
        signature_displays(esm),
        ["function load(path: string): Buffer"]
    );
    assert!(esm.public);
    let commonjs = symbol_at(&publication, "dist/index.cjs", "load").expect("the CJS build joins");
    assert_eq!(
        signature_displays(commonjs),
        ["function load(path: string, flag: boolean): Buffer"]
    );
    assert!(symbol_at(&publication, "dist/index.d.mts", "load").is_none());
    assert!(symbol_at(&publication, "dist/index.d.cts", "load").is_none());
    assert_unique_identities(&publication);
}

/// The collector places `@types/node`'s `fs.d.ts` beside `lib/fs.js`; its `declare module`
/// members come out under bare names and join the module's values.
#[test]
fn test_node_declare_module_members_join_the_runtime_module() {
    let publication = analyzed(
        ShippedLanguage::TypeScript,
        vec![
            (
                "lib/fs.d.ts",
                "declare module \"fs\" {\n    export function readFile(path: string, callback: (data: string) => void): void;\n    export function readFile(path: string, encoding: string, callback: (data: string) => void): void;\n    export function watch(path: string): void;\n}\n",
            ),
            (
                "lib/fs.js",
                "'use strict';\n\nfunction readFile(path, options, callback) {\n  callback = callback || options;\n}\n\nmodule.exports = {\n  readFile,\n};\n",
            ),
        ],
    );

    let read_file = symbol_at(&publication, "lib/fs.js", "readFile").expect("readFile joins");
    assert_eq!(signature_displays(read_file).len(), 2, "{read_file:?}");
    assert!(read_file.public);
    assert!(symbol_at(&publication, "lib/fs.d.ts", "watch").is_some_and(|watch| watch.public));
}

/// A module the package ships no stub for answers under its own rules, beside the modules a
/// stub joined.
#[test]
fn test_an_unstubbed_module_of_a_stubbed_package_answers() {
    let publication = analyzed(
        ShippedLanguage::Python,
        vec![
            ("pkg/__init__.pyi", "def start(port: int) -> None: ...\n"),
            ("pkg/__init__.py", "def start(port):\n    pass\n"),
            (
                "pkg/helpers.py",
                "def format_port(port):\n    return str(port)\n\ndef _private():\n    pass\n",
            ),
        ],
    );

    assert!(symbol_at(&publication, "pkg/__init__.py", "start").is_some_and(|start| start.public));
    assert!(
        symbol_at(&publication, "pkg/helpers.py", "format_port").is_some_and(|value| value.public)
    );
    assert!(
        symbol_at(&publication, "pkg/helpers.py", "_private").is_some_and(|value| !value.public)
    );
}

/// The publication addresses a joined declaration at its module; the analysis keeps each
/// stub form's identity and range beside it, so a position inside the stub names the joined
/// declaration.
#[test]
fn test_a_joined_declaration_records_its_stub_forms_and_ranges() {
    let stub = "from typing import overload\n\n@overload\ndef f(x: int) -> int: ...\n@overload\ndef f(x: str) -> str: ...\n";
    let analysis = package_analysis(
        ShippedLanguage::Python,
        vec![("mod.pyi", stub), ("mod.py", "def f(x):\n    return x\n")],
    );
    let module = analysis
        .files()
        .iter()
        .find(|held| held.file().path().as_str() == "mod.py")
        .expect("the module is analyzed");
    let forms = module.stub_forms("f");

    let identities: Vec<&str> = forms
        .iter()
        .map(|form| form.identity().0.as_str())
        .collect();
    assert_eq!(
        identities,
        [
            "rift://symbol/python/cargo/beacon@1.0.0/mod.pyi/f~1",
            "rift://symbol/python/cargo/beacon@1.0.0/mod.pyi/f~2",
        ]
    );
    assert!(forms.iter().all(|form| form.path().0 == "mod.pyi"));
    let first = stub.find("@overload").expect("the first form");
    let second = stub.rfind("@overload").expect("the second form");
    let ranges: Vec<&TextRange> = forms.iter().map(StubForm::range).collect();
    assert_eq!(ranges[0].start, u64::try_from(first).expect("offset"));
    assert_eq!(ranges[1].start, u64::try_from(second).expect("offset"));
    assert_eq!(
        ranges[1].end,
        u64::try_from(stub.trim_end().len()).expect("offset")
    );
    assert!(module.stub_forms("missing").is_empty());
    let stub_file = analysis
        .files()
        .iter()
        .find(|held| held.file().path().as_str() == "mod.pyi")
        .expect("the stub is analyzed");
    assert!(
        stub_file.stub_forms("f~1").is_empty(),
        "a stub records no forms"
    );
}

/// Variants of a public enum are public, as items of a public trait are.
#[test]
fn test_variants_of_a_public_enum_are_public() {
    let publication = analyzed(
        ShippedLanguage::Rust,
        vec![(
            "src/lib.rs",
            "pub enum Shape {\n    Circle,\n    Square,\n}\nenum Hidden {\n    Inner,\n}\n",
        )],
    );

    let public: Vec<&str> = publication
        .symbols
        .iter()
        .filter(|symbol| symbol.public)
        .map(|symbol| symbol.qualified_name.as_str())
        .collect();
    assert_eq!(public, ["Shape", "Shape::Circle", "Shape::Square"]);
    let variant = symbol_at(&publication, "src/lib.rs", "Shape::Circle").expect("a variant");
    assert_eq!(variant.kind.0, "variant");
}

/// Global ingestion reads a registry archive, selects its files, and analyzes them; the same
/// path runs here over a small package: tests leave through the `[source] exclude` patterns
/// the caller passes, build output never reaches the analysis, the package's
/// `context7.json` narrows its documentation, and the stub joins its module.
#[cfg(feature = "archive")]
#[test]
fn test_a_package_archive_is_selected_and_analyzed_as_global_ingestion_does() {
    use std::io::Write as _;

    use rift_core::{ContributionOrigin, SourceKind, SourceLocation};
    use rift_protocol::documentation::DocumentationConfiguration;
    use rift_protocol::read::{PackageIdentity, PathPattern};
    use sha2::Digest as _;

    use crate::archive::{ArchiveDigest, ArchiveFormat, ArchiveLimits, read_archive};
    use crate::{
        CONTEXT7_FILE, Context7, DocumentationSelection, ExactPackageInput, ExactPackageLimits,
        PackageAnalyzer, PackageFileSelection, PackageLanguage, PackageSource,
    };

    let entries: [(&str, &str); 10] = [
        (
            "beacon-1.0.0/beacon/__init__.pyi",
            "def start(port: int) -> None: ...\n",
        ),
        (
            "beacon-1.0.0/beacon/__init__.py",
            "def start(port):\n    \"\"\"Start serving on `port`.\"\"\"\n",
        ),
        (
            "beacon-1.0.0/beacon/__pycache__/cached.py",
            "def stale():\n    pass\n",
        ),
        (
            "beacon-1.0.0/tests/test_start.py",
            "def test_start():\n    pass\n",
        ),
        ("beacon-1.0.0/README.md", "# Beacon\n\nServes signals.\n"),
        ("beacon-1.0.0/CHANGELOG.md", "# Changes\n\n- 1.0.0\n"),
        ("beacon-1.0.0/docs/guide.md", "# Guide\n\nCall `start`.\n"),
        (
            "beacon-1.0.0/docs/internal/notes.md",
            "# Notes\n\nInternal.\n",
        ),
        (
            "beacon-1.0.0/context7.json",
            r#"{"projectTitle": "Beacon", "excludeFolders": ["docs/internal"]}"#,
        ),
        (
            "beacon-1.0.0/pyproject.toml",
            "[project]\nname = \"beacon\"\n",
        ),
    ];
    let mut builder = tar::Builder::new(Vec::new());
    for (path, text) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(text.len()).expect("fixture size"));
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, text.as_bytes())
            .expect("a tar entry");
    }
    let tar = builder.into_inner().expect("a tar stream");
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&tar).expect("a gzip stream");
    let archive = gzip.finish().expect("a gzip archive");
    let digest = ArchiveDigest::Sha256(sha2::Sha256::digest(&archive).into());
    let files = read_archive(
        &archive,
        ArchiveFormat::TarGzip,
        &digest,
        Some("beacon-1.0.0"),
        ArchiveLimits::default(),
    )
    .expect("a verified archive");

    let context7 = files
        .files()
        .iter()
        .find(|(path, _)| path.as_str() == CONTEXT7_FILE)
        .map(|(_, bytes)| Context7::parse(bytes).expect("a valid context7.json"))
        .expect("the package ships a context7.json");
    let documentation = DocumentationSelection::new(&DocumentationConfiguration::default())
        .and_then(|selection| selection.narrowed_by(&context7))
        .expect("the documentation selection compiles");
    let selection = PackageFileSelection::new(
        PackageLanguage::Python,
        &[PathPattern("**/tests/**".to_owned())],
        documentation,
    )
    .expect("the exclude patterns compile");
    let selected = selection.select(files.files().keys());
    let names: Vec<&str> = selected.files().iter().map(|path| path.as_str()).collect();
    assert_eq!(
        names,
        [
            "README.md",
            "beacon/__init__.py",
            "beacon/__init__.pyi",
            "docs/guide.md",
        ]
    );

    let texts: Vec<(&rift_core::ProjectPath, &str)> = selected
        .files()
        .into_iter()
        .map(|path| {
            let bytes = &files.files()[path];
            (path, std::str::from_utf8(bytes).expect("UTF-8 fixture"))
        })
        .collect();
    let sources: Vec<PackageSource<'_>> = texts
        .iter()
        .map(|(path, text)| PackageSource::new(path, text))
        .collect();
    let package = PackageIdentity {
        manager: "pypi".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )
    .expect("origin");
    let language = ShippedLanguage::Python.language();
    let input = ExactPackageInput::new(
        &package,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(16, 1 << 20),
    )
    .expect("bounded package input");
    let analysis = PackageAnalyzer::analyze(input, 1).expect("the package analyzes");
    let publication = analysis.publication();

    let start = symbol_at(publication, "beacon/__init__.py", "start").expect("start joins");
    assert_eq!(signature_displays(start), ["def start(port: int) -> None:"]);
    assert!(start.public);
    assert!(symbol_at(publication, "beacon/__init__.pyi", "start").is_none());
    let documented: Vec<&str> = publication
        .documentation
        .sources
        .iter()
        .filter_map(|source| match &source.identity.source {
            rift_protocol::documentation::DocumentationSourceIdentity::Package { unit } => {
                Some(unit.0.as_str())
            }
            rift_protocol::documentation::DocumentationSourceIdentity::Project { .. } => None,
        })
        .collect();
    assert!(documented.iter().any(|unit| unit.ends_with("/README.md")));
    assert!(
        documented
            .iter()
            .any(|unit| unit.ends_with("/docs/guide.md"))
    );
    assert!(!documented.iter().any(|unit| unit.contains("CHANGELOG")));
    assert!(!documented.iter().any(|unit| unit.contains("internal")));
}
