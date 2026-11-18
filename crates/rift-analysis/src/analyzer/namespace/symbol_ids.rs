use std::collections::BTreeMap;

use rift_core::{ProjectPath, SymbolId};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_syntax::{SyntaxLimits, SyntaxSource, registry};

use super::{PreparedNamespace, SelectedFile, prepare_context};
use crate::{NamespaceInput, PackageSource};

fn prepared(files: &[(&str, &str)], metadata: &[(&str, &str)]) -> PreparedNamespace {
    prepared_with_paths(files, metadata, &BTreeMap::new())
}

fn prepared_with_paths(
    files: &[(&str, &str)],
    metadata: &[(&str, &str)],
    build_paths: &BTreeMap<rift_protocol::read::ProjectPath, Option<crate::ArchiveMemberKind>>,
) -> PreparedNamespace {
    let paths = files
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("source path"))
        .collect::<Vec<_>>();
    let sources = paths
        .iter()
        .zip(files)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect::<Vec<_>>();
    let metadata_paths = metadata
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("metadata path"))
        .collect::<Vec<_>>();
    let context = metadata_paths
        .iter()
        .zip(metadata)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect::<Vec<_>>();
    let facts = sources
        .iter()
        .map(|source| {
            let extension = source
                .path()
                .as_str()
                .rsplit_once('.')
                .expect("extension")
                .1;
            registry::provider_for_extension(extension)
                .expect("shipped language")
                .analyze(
                    SyntaxSource {
                        path: source.path(),
                        text: source.text(),
                    },
                    SyntaxLimits::DEFAULT,
                )
                .expect("syntax facts")
                .into_facts()
        })
        .collect::<Vec<_>>();
    let selected = sources
        .iter()
        .zip(&facts)
        .map(|(source, syntax)| SelectedFile {
            path: source.path(),
            source: source.text(),
            syntax,
        })
        .collect::<Vec<_>>();
    let owner = SymbolOwner::Local;
    let language = facts[0].language();
    let input = NamespaceInput::workspace(
        &owner,
        language,
        &sources,
        &context,
        &[],
        (64, 1024 * 1024, SyntaxLimits::DEFAULT),
    )
    .expect("local namespace")
    .with_build_paths(build_paths)
    .expect("captured paths");
    prepare_context(&input, &selected)
}

fn identity(found: &PreparedNamespace, path: &str, name: &str) -> SymbolIdentity {
    let anchors = found.anchors.get(path).expect("file anchors");
    let id = anchors
        .get(name)
        .unwrap_or_else(|| panic!("declaration {name} at {path}: {anchors:?}"));
    SymbolIdentity::parse(id.as_str()).expect("canonical identity")
}

#[test]
fn typescript_local_files_and_tsx_use_relative_modules() {
    for path in ["src/main.ts", "src/main.tsx"] {
        let found = prepared(
            &[(path, "export function beacon(): number { return 1; }")],
            &[],
        );
        let id = identity(&found, path, "beacon");
        assert_eq!(id.owner(), &SymbolOwner::Local);
        assert_eq!(
            id.qualified_path(),
            [format!("./{path}"), "beacon".to_owned()]
        );
    }
}

#[test]
fn python_local_layouts_do_not_depend_on_build_backend() {
    for (backend, requirement) in [
        ("hatchling.build", "hatchling"),
        ("poetry.core.masonry.api", "poetry-core>=2.0"),
        ("setuptools.build_meta", "setuptools>=61"),
        ("setuptools.build_meta", "setuptools==80.9.0"),
        ("setuptools.build_meta", "setuptools"),
        ("flit_core.buildapi", "flit_core>=3.2,<4"),
    ] {
        let metadata =
            format!("[build-system]\nrequires=['{requirement}']\nbuild-backend='{backend}'\n");
        for context in [vec![], vec![("pyproject.toml", metadata.as_str())]] {
            assert_python_layouts(&context);
        }
    }
}

fn assert_python_layouts(context: &[(&str, &str)]) {
    for path in ["pkg/models.py", "src/pkg/models.py", "pkg/space/models.py"] {
        let found = prepared(&[(path, "class Model:\n    pass\n")], context);
        let expected = path
            .strip_prefix("src/")
            .unwrap_or(path)
            .strip_suffix(".py")
            .expect("module");
        let mut parts = expected.split('/').map(str::to_owned).collect::<Vec<_>>();
        parts.push("Model".to_owned());
        assert_eq!(identity(&found, path, "Model").qualified_path(), parts);
    }
}

#[test]
fn python_nested_projects_and_package_initializers_keep_module_names() {
    let found = prepared(
        &[("nested/src/pkg/__init__.py", "def beacon():\n    pass\n")],
        &[("nested/pyproject.toml", "[project]\nname='pkg'\n")],
    );
    assert_eq!(
        identity(&found, "nested/src/pkg/__init__.py", "beacon").qualified_path(),
        ["pkg", "beacon"]
    );
}

#[test]
fn valid_declarations_keep_ids_beside_parse_errors() {
    for (path, text, metadata) in [
        (
            "pkg/bad.py",
            "def ok():\n    pass\ndef broken(: pass\nclass Fine:\n    pass\n",
            vec![],
        ),
        (
            "bad.ts",
            "function ok() {}\nfunction broken(,) {}\nclass Fine {}",
            vec![],
        ),
        (
            "bad.js",
            "function ok() {}\nfunction broken(,) {}\nclass Fine {}",
            vec![],
        ),
        (
            "src/lib.rs",
            "pub fn ok() {}\nconst broken: = ;\npub struct Fine;",
            vec![("Cargo.toml", "[package]\nname='beacon'\nversion='1.0.0'\n")],
        ),
    ] {
        let found = prepared(&[(path, text)], &metadata);
        identity(&found, path, "ok");
        identity(&found, path, "Fine");
        assert!(!found.anchors[path].contains_key("broken"), "{path}");
    }
}

#[test]
fn python_module_keeps_id_with_broken_function() {
    let path = "pkg/bad.py";
    for text in ["def broken(: pass\n", "def broken(: pass"] {
        let found = prepared(&[(path, text)], &[]);
        assert_eq!(identity(&found, path, ".").qualified_path(), ["pkg", "bad"]);
        assert!(!found.anchors[path].contains_key("broken"));
    }
}

#[test]
fn python_class_keeps_id_and_valid_methods_with_broken_method() {
    let path = "pkg/bad.py";
    let text = "class Container:\n    def ok(self):\n        pass\n    def broken(self,: pass\n    def fine(self):\n        pass\n";
    let found = prepared(&[(path, text)], &[]);
    for name in [".", "Container", "Container.ok", "Container.fine"] {
        identity(&found, path, name);
    }
    assert!(!found.anchors[path].contains_key("Container.broken"));
}

#[test]
fn python_declaration_inside_error_subtree_loses_id() {
    let path = "pkg/bad.py";
    let text = "else:\n    def broken():\n        pass\n";
    let found = prepared(&[(path, text)], &[]);
    identity(&found, path, ".");
    assert!(!found.anchors[path].contains_key("broken"));
}

#[test]
fn missing_container_token_keeps_valid_member_identity() {
    let found = prepared(&[("main.ts", "class Container { method() {}\n")], &[]);
    assert_eq!(
        identity(&found, "main.ts", "method").qualified_path(),
        ["./main.ts", "method"]
    );
    assert!(!found.anchors["main.ts"].contains_key("Container"));
}

#[test]
fn document_declarations_use_original_paths_and_qualified_names() {
    for (path, source, name) in [
        ("README.md", "# Install\n\nText.\n", "Install"),
        ("settings.json", "{\"port\": 8080}", "port"),
        ("settings.yaml", "port: 8080\n", "port"),
        ("settings.toml", "port = 8080\n", "port"),
    ] {
        let other = format!("nested/{path}");
        let found = prepared(&[(path, source), (&other, source)], &[]);
        assert_eq!(identity(&found, path, name).qualified_path(), [path, name]);
        assert_ne!(found.anchors[path][name], found.anchors[&other][name]);
    }
}

#[test]
fn python_invalid_module_paths_remain_unresolved() {
    let found = prepared(&[("invalid-dir/main.py", "def beacon():\n    pass\n")], &[]);
    assert_eq!(
        found.anchors["invalid-dir/main.py"],
        BTreeMap::<String, SymbolId>::new()
    );
}

#[test]
fn python_local_package_fallback_keeps_captured_roots_for_other_packages() {
    use crate::ArchiveMemberKind::{Directory, File};
    let paths = [
        ("pdm", Some(Directory)),
        ("pdm/pyproject.toml", Some(File)),
        ("pdm/pdm_build.py", None),
        ("pdm/src", None),
        ("pdm/pkg", Some(Directory)),
        ("pdm/pkg/__init__.py", Some(File)),
    ]
    .into_iter()
    .map(|(path, kind)| (rift_protocol::read::ProjectPath(path.to_owned()), kind))
    .collect();
    let found = prepared_with_paths(
        &[
            ("pdm/pkg/__init__.py", "def pdm_beacon():\n    pass\n"),
            ("other/src/pkg/main.py", "def beacon():\n    pass\n"),
        ],
        &[
            (
                "pdm/pyproject.toml",
                "[build-system]\nrequires=['pdm-backend']\nbuild-backend='pdm.backend'\n",
            ),
            (
                "other/pyproject.toml",
                "[build-system]\nrequires=['hatchling']\nbuild-backend='hatchling.build'\n",
            ),
        ],
        &paths,
    );
    assert_eq!(
        identity(&found, "pdm/pkg/__init__.py", "pdm_beacon").qualified_path(),
        ["pkg", "pdm_beacon"]
    );
    assert_eq!(
        identity(&found, "other/src/pkg/main.py", "beacon").qualified_path(),
        ["pkg", "main", "beacon"]
    );
}
