//! The stub and module join through the whole analysis: each paired stub and module
//! declaration answers one symbol, at the module.

use std::collections::BTreeSet;

use rift_protocol::index::PackageSymbol;
use rift_protocol::read::TextRange;
use rift_syntax::ShippedLanguage;

use super::StubForm;
use super::fixture::{package_analysis, package_analysis as analyzed};

fn conditional_analysis(files: Vec<(&str, &str)>) -> super::PackageAnalysis {
    let package = rift_protocol::read::PackageIdentity {
        manager: "npm".to_owned(),
        registry: "registry.npmjs.org".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner().expect("fixture package owner");
    let origin = super::fixture::origin(&package);
    let language = ShippedLanguage::TypeScript.language();
    let files = files
        .into_iter()
        .map(|(path, text)| (rift_core::ProjectPath::new(path).expect("path"), text))
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 4);
    let sources = files
        .iter()
        .map(|(path, text)| crate::PackageSource::new(path, text))
        .collect::<Vec<_>>();
    assert_eq!(sources[0].path().as_str(), "dist/index.d.mts");
    assert_eq!(sources[1].path().as_str(), "dist/index.mjs");
    assert_eq!(sources[2].path().as_str(), "dist/index.d.cts");
    assert_eq!(sources[3].path().as_str(), "dist/index.cjs");
    let metadata_path = rift_core::ProjectPath::new("package.json").expect("metadata path");
    let metadata_text = r#"{"name":"beacon","version":"1.0.0","exports":{".":{"import":{"types":"./dist/index.d.mts","default":"./dist/index.mjs"},"require":{"types":"./dist/index.d.cts","default":"./dist/index.cjs"}}}}"#;
    let metadata = [crate::PackageSource::new(&metadata_path, metadata_text)];
    let esm = [sources[0]];
    let cjs = [sources[2]];
    let modules = [
        crate::NamespaceModule::new("beacon", sources[1], &esm, &["import"]),
        crate::NamespaceModule::new("beacon", sources[3], &cjs, &["require"]),
    ];
    let bytes = sources
        .iter()
        .map(|source| u64::try_from(source.text().len()).expect("source bytes"))
        .sum::<u64>()
        + u64::try_from(
            metadata_text.len() + 2 * "beacon".len() + "import".len() + "require".len(),
        )
        .expect("captured observation bytes");
    let input = crate::ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        crate::ExactPackageLimits::new(5, bytes),
    )
    .expect("bounded fixture input")
    .with_framework_context(&metadata, &[])
    .expect("captured conditional exports")
    .with_modules(&modules)
    .expect("separate import and require observations");
    super::PackageAnalyzer::analyze(input, 1).expect("conditional package analysis")
}

fn node_analysis(files: Vec<(&str, &str)>) -> super::PackageAnalysis {
    let runtime = rift_protocol::read::RuntimeIdentity {
        runtime: "node".to_owned(),
        version: "26.11.1".to_owned(),
    };
    let owner = runtime.owner().expect("fixture runtime owner");
    let origin = rift_core::ContributionOrigin::new(
        Some(rift_core::SourceLocation::Stdlib {
            runtime: Some(runtime),
        }),
        rift_core::SourceKind::Authored,
    )
    .expect("authored runtime source");
    let companion = rift_protocol::read::PackageIdentity {
        manager: "npm".to_owned(),
        registry: "registry.npmjs.org".to_owned(),
        name: "@types/node".to_owned(),
        version: "26.6.4".to_owned(),
    };
    let physical_owner = companion.owner().expect("companion package owner");
    let physical_origin = super::fixture::origin(&companion);
    let physical_unit = rift_core::SourceUnitId::for_owner(physical_owner, "fs.d.ts")
        .expect("archive-root-relative companion path");
    let language = ShippedLanguage::TypeScript.language();
    let files = files
        .into_iter()
        .map(|(path, text)| (rift_core::ProjectPath::new(path).expect("path"), text))
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].0.as_str(), "lib/fs.d.ts");
    assert_eq!(files[1].0.as_str(), "lib/fs.js");
    let sources = [
        crate::PackageSource::new(&files[0].0, files[0].1)
            .with_source_unit(&physical_unit, &physical_origin)
            .expect("physical companion source"),
        crate::PackageSource::new(&files[1].0, files[1].1),
    ];
    let declarations = [sources[0]];
    let modules = [crate::NamespaceModule::new(
        "fs",
        sources[1],
        &declarations,
        &[],
    )];
    let bytes = sources
        .iter()
        .map(|source| u64::try_from(source.text().len()).expect("source bytes"))
        .sum::<u64>()
        + u64::try_from("fs".len()).expect("module bytes");
    let input = crate::ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        crate::ExactPackageLimits::new(2, bytes),
    )
    .expect("bounded runtime fixture input")
    .with_modules(&modules)
    .expect("captured builtin module observation");
    super::PackageAnalyzer::analyze(input, 1).expect("runtime module analysis")
}

/// The physical declaration captured under `path` as `qualified_name`.
fn symbol_at<'publication>(
    analysis: &'publication super::PackageAnalysis,
    path: &str,
    qualified_name: &str,
) -> Option<&'publication PackageSymbol> {
    let held = analysis
        .files()
        .iter()
        .find(|held| held.file().path().as_str() == path)?;
    let syntax = held
        .file()
        .syntax()
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name == qualified_name)?;
    let unit = analysis
        .publication()
        .units
        .iter()
        .find(|unit| unit.unit.0 == held.placement.unit().to_string())?;
    analysis.publication().declarations.iter().find(|symbol| {
        symbol.unit == unit.unit
            && symbol.range.start == syntax.item_range.start
            && symbol.range.end == syntax.item_range.end
    })
}

fn object_for<'publication>(
    analysis: &'publication super::PackageAnalysis,
    declaration: &PackageSymbol,
) -> &'publication rift_protocol::read::Symbol {
    let mut objects = analysis
        .publication()
        .objects
        .iter()
        .filter(|object| object.id.as_ref() == Some(&declaration.symbol));
    let object = objects
        .next()
        .expect("a declaration refers to one logical object");
    assert!(objects.next().is_none(), "one ID names one logical object");
    for index in &declaration.signature_indices {
        assert!(usize::try_from(*index).is_ok_and(|index| index < object.signatures.len()));
    }
    for index in &declaration.type_indices {
        assert!(usize::try_from(*index).is_ok_and(|index| index < object.types.len()));
    }
    for index in &declaration.documentation_indices {
        assert!(usize::try_from(*index).is_ok_and(|index| index < object.documentation.len()));
    }
    object
}

fn signature_displays<'publication>(
    analysis: &'publication super::PackageAnalysis,
    symbol: &PackageSymbol,
) -> Vec<&'publication str> {
    object_for(analysis, symbol)
        .signatures
        .iter()
        .map(|signature| signature.display.as_str())
        .collect()
}

fn binding_signature_displays<'publication>(
    analysis: &'publication super::PackageAnalysis,
    declaration: &PackageSymbol,
) -> Vec<&'publication str> {
    let object = object_for(analysis, declaration);
    declaration
        .signature_indices
        .iter()
        .map(|index| {
            object.signatures[usize::try_from(*index).expect("validated signature index")]
                .display
                .as_str()
        })
        .collect()
}

fn assert_unique_identities(analysis: &super::PackageAnalysis) {
    let publication = analysis.publication();
    let identities: BTreeSet<&str> = publication
        .objects
        .iter()
        .filter_map(|symbol| symbol.id.as_ref().map(|id| id.0.as_str()))
        .collect();
    assert_eq!(
        identities.len(),
        publication
            .objects
            .iter()
            .filter(|object| object.id.is_some())
            .count(),
        "no two records share one identity"
    );
    for object in publication
        .objects
        .iter()
        .filter(|object| object.id.is_none())
    {
        let coverage = publication
            .coverage
            .iter()
            .find(|coverage| coverage.language == object.language)
            .expect("unresolved object language has coverage");
        assert!(!coverage.identity_complete);
    }
    assert!(
        publication
            .declarations
            .iter()
            .all(|declaration| identities.contains(declaration.symbol.as_str()))
    );
}

/// A stub's `typing.overload` forms join the implementation into one symbol: the
/// implementation source and documentation, with each stub signature bound to its
/// original declaration. Both files reach one logical object through their own bindings.
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
    let stub_signatures: Vec<&str> = ["f~1", "f~2"]
        .into_iter()
        .flat_map(|form| {
            binding_signature_displays(
                &publication,
                symbol_at(&publication, "mod.pyi", form).expect("retained stub form"),
            )
        })
        .collect();
    assert_eq!(
        stub_signatures,
        ["def f(x: int) -> int:", "def f(x: str) -> str:"]
    );
    assert_eq!(
        stub_signatures.first().copied(),
        Some("def f(x: int) -> int:")
    );
    assert_eq!(
        binding_signature_displays(&publication, joined),
        ["def f(x):"]
    );
    assert!(
        object_for(&publication, joined)
            .documentation
            .iter()
            .any(|documentation| documentation.text.contains("Return x unchanged."))
    );
    assert!(joined.source.starts_with("def f(x):"));
    assert!(joined.public);
    let document = publication
        .publication()
        .documents
        .iter()
        .find(|document| document.identity == joined.symbol.0)
        .expect("the joined symbol ranks under one document");
    assert_eq!(document.signature.as_deref(), Some("def f(x: int) -> int:"));
    for form in ["f~1", "f~2"] {
        let binding = symbol_at(&publication, "mod.pyi", form).expect("each stub form is retained");
        assert_eq!(binding.symbol, joined.symbol);
    }
    assert_eq!(
        publication
            .publication()
            .declarations
            .iter()
            .filter(|binding| binding.symbol == joined.symbol)
            .count(),
        3,
        "two overload forms and one implementation retain their bindings"
    );
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
        binding_signature_displays(&publication, joined),
        ["def f(x):"]
    );
    let stub_signatures: Vec<&str> = ["f~1", "f~2"]
        .into_iter()
        .flat_map(|form| {
            binding_signature_displays(
                &publication,
                symbol_at(&publication, "mod.pyi", form).expect("retained stub form"),
            )
        })
        .collect();
    assert_eq!(
        stub_signatures,
        ["def f(x: int) -> int:", "def f(x: str) -> str:"]
    );
    for form in ["f~1", "f~2"] {
        assert_eq!(
            symbol_at(&publication, "mod.pyi", form)
                .expect("a retained stub form")
                .symbol,
            joined.symbol,
            "{form}"
        );
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
    assert_eq!(
        object_for(&publication, close).container.as_ref(),
        Some(&class.symbol)
    );
    assert!(symbol_at(&publication, "mod.py", "Client.open").is_some_and(|open| open.public));
    assert_eq!(
        symbol_at(&publication, "mod.pyi", "Client.open")
            .expect("the stub member binding")
            .symbol,
        symbol_at(&publication, "mod.py", "Client.open")
            .expect("the module member binding")
            .symbol
    );
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
    let stub_open = symbol_at(&publication, "index.d.ts", "open").expect("the stub binding");
    assert_eq!(
        binding_signature_displays(&publication, stub_open),
        ["function open(path: string): Handle"]
    );
    assert_eq!(
        binding_signature_displays(&publication, open),
        ["function open(path)"]
    );
    assert!(open.public);
    assert!(symbol_at(&publication, "index.js", "version").is_some_and(|value| value.public));
    assert_eq!(
        symbol_at(&publication, "index.d.ts", "open")
            .expect("the stub binding")
            .symbol,
        open.symbol
    );
    assert!(symbol_at(&publication, "index.d.ts", "Handle").is_some());
    let close = symbol_at(&publication, "index.d.ts", "Handle.close").expect("a member signature");
    assert_eq!(signature_displays(&publication, close), ["close(): void"]);
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
    assert_eq!(
        signature_displays(&publication, parse),
        ["function parse(text)"]
    );
    assert_eq!(
        object_for(&publication, parse)
            .signatures
            .first()
            .map(|signature| signature.display.as_str()),
        Some("function parse(text)")
    );
    assert_eq!(
        symbol_at(&publication, "index.d.ts", "parse")
            .expect("the stub binding")
            .symbol,
        parse.symbol
    );
}

/// Each declaration file joins its explicitly selected package export observation.
#[test]
fn test_a_declaration_file_joins_its_esm_and_commonjs_builds() {
    let publication = conditional_analysis(vec![
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
    ]);

    let esm = symbol_at(&publication, "dist/index.mjs", "load").expect("the ESM build joins");
    let esm_stub =
        symbol_at(&publication, "dist/index.d.mts", "load").expect("the ESM stub binding");
    assert_eq!(
        binding_signature_displays(&publication, esm_stub),
        ["function load(path: string): Buffer"]
    );
    assert_eq!(
        binding_signature_displays(&publication, esm),
        ["function load(path)"]
    );
    assert!(esm.public);
    let commonjs = symbol_at(&publication, "dist/index.cjs", "load").expect("the CJS build joins");
    let commonjs_stub =
        symbol_at(&publication, "dist/index.d.cts", "load").expect("the CJS stub binding");
    assert_eq!(
        binding_signature_displays(&publication, commonjs_stub),
        ["function load(path: string, flag: boolean): Buffer"]
    );
    assert_eq!(
        binding_signature_displays(&publication, commonjs),
        ["function load(path, flag)"]
    );
    assert_eq!(esm_stub.symbol, esm.symbol);
    assert_eq!(commonjs_stub.symbol, commonjs.symbol);
    assert_unique_identities(&publication);
}

/// Captured Node module observations join declarations from their physical package owner.
#[test]
fn test_node_declare_module_members_join_the_runtime_module() {
    let declarations_source = "declare module \"fs\" {\n    export function readFile(path: string, callback: (data: string) => void): void;\n    export function readFile(path: string, encoding: string, callback: (data: string) => void): void;\n    export function watch(path: string): void;\n}\n";
    let publication = node_analysis(vec![
        ("lib/fs.d.ts", declarations_source),
        (
            "lib/fs.js",
            "'use strict';\n\nfunction readFile(path, options, callback) {\n  callback = callback || options;\n}\n\nmodule.exports = {\n  readFile,\n};\n",
        ),
    ]);

    let read_file = symbol_at(&publication, "lib/fs.js", "readFile").expect("readFile joins");
    assert_eq!(
        binding_signature_displays(&publication, read_file),
        ["function readFile(path, options, callback)"]
    );
    assert!(read_file.public);
    assert!(
        symbol_at(&publication, "lib/fs.d.ts", "\"fs\".watch").is_some_and(|watch| watch.public)
    );
    let companion = publication
        .publication()
        .units
        .iter()
        .find(|unit| unit.path.0 == "fs.d.ts")
        .expect("original companion member");
    let mut overloads = publication
        .publication()
        .declarations
        .iter()
        .filter(|binding| binding.unit == companion.unit && binding.symbol == read_file.symbol)
        .collect::<Vec<_>>();
    overloads.sort_by_key(|binding| binding.range.start);
    assert_eq!(overloads.len(), 2, "{overloads:?}");
    for (binding, expected) in overloads.iter().zip([
        "function readFile(path: string, callback: (data: string) => void): void",
        "function readFile(path: string, encoding: string, callback: (data: string) => void): void",
    ]) {
        assert_eq!(
            binding_signature_displays(&publication, binding),
            [expected]
        );
        let start = usize::try_from(binding.range.start).expect("portable source start");
        let end = usize::try_from(binding.range.end).expect("portable source end");
        assert_eq!(binding.source, declarations_source[start..end]);
    }
    assert_eq!(
        publication
            .publication()
            .declarations
            .iter()
            .filter(|binding| binding.symbol == read_file.symbol)
            .count(),
        3
    );
    let unit = rift_core::SourceUnitId::parse(&companion.unit.0).expect("physical companion unit");
    assert_eq!(unit.key().as_str(), "fs.d.ts");
    assert!(unit.source_owner().is_some_and(|owner| matches!(owner,
        rift_protocol::identity::SymbolOwner::Package { manager, registry, name, version }
            if manager == "npm" && registry == "registry.npmjs.org"
                && name == "@types/node" && version == "26.6.4")));
    assert!(
        publication
            .files()
            .iter()
            .any(|held| held.file.path().as_str() == "lib/fs.d.ts")
    );
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
    let stub_file = analysis
        .files()
        .iter()
        .find(|held| held.file().path().as_str() == "mod.pyi")
        .expect("the stub is analyzed");
    assert_eq!(
        stub_file.placement.identity_path(),
        "pypi/pypi.org/beacon@1.0.0/mod.pyi"
    );

    let identities: Vec<String> = forms.iter().map(|form| form.identity().0.clone()).collect();
    assert_eq!(
        identities,
        [
            rift_core::symbol_identity("python", stub_file.placement.identity_path(), "f~1"),
            rift_core::symbol_identity("python", stub_file.placement.identity_path(), "f~2"),
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
        .files()
        .iter()
        .flat_map(|held| held.file().syntax().symbols())
        .filter(|syntax| {
            symbol_at(&publication, "src/lib.rs", &syntax.qualified_name)
                .is_some_and(|binding| binding.public)
        })
        .map(|symbol| symbol.qualified_name.as_str())
        .collect();
    assert_eq!(public, ["Shape", "Shape::Circle", "Shape::Square"]);
    let variant = symbol_at(&publication, "src/lib.rs", "Shape::Circle").expect("a variant");
    assert_eq!(object_for(&publication, variant).kind.0, "variant");
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
        PackageAnalyzer, PackageFileSelection, PackageImportRoot, PackageImportRootOrigin,
        PackageLanguage, PackageSource,
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
            "[build-system]\nbuild-backend = 'flit_core.buildapi'\nrequires = ['flit_core']\n[project]\nname = 'beacon'\n[tool.flit.module]\nname = 'beacon'\n",
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
        registry: "pypi.org".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner().expect("fixture owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )
    .expect("origin");
    let language = ShippedLanguage::Python.language();
    let metadata_path = rift_core::ProjectPath::new("pyproject.toml").expect("metadata path");
    let metadata_text =
        std::str::from_utf8(&files.files()[&metadata_path]).expect("UTF-8 metadata");
    let metadata = [PackageSource::new(&metadata_path, metadata_text)];
    let roots = [PackageImportRoot::new(
        None,
        vec!["beacon".to_owned()],
        PackageImportRootOrigin::Flit,
    )
    .expect("the captured Flit module maps to the archive root")];
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(16, 1 << 20),
    )
    .expect("bounded package input")
    .with_framework_context(&metadata, &[])
    .expect("captured package metadata")
    .with_import_roots(&roots)
    .expect("verified static import root");
    let analysis = PackageAnalyzer::analyze(input, 1).expect("the package analyzes");
    let publication = analysis.publication();

    let start = symbol_at(&analysis, "beacon/__init__.py", "start").expect("start joins");
    assert_eq!(
        start.symbol.0,
        "rift://symbol/pypi/pypi.org/beacon@1.0.0/python/beacon/start"
    );
    assert_eq!(
        binding_signature_displays(
            &analysis,
            symbol_at(&analysis, "beacon/__init__.pyi", "start").expect("stub binding"),
        ),
        ["def start(port: int) -> None:"]
    );
    assert_eq!(
        binding_signature_displays(&analysis, start),
        ["def start(port):"]
    );
    assert!(start.public);
    assert_eq!(
        symbol_at(&analysis, "beacon/__init__.pyi", "start")
            .expect("the stub source binding")
            .symbol,
        start.symbol
    );
    assert_unique_identities(&analysis);
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.applicability_complete)
    );
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
