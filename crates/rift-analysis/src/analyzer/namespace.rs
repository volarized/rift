//! Logical Python placement from accepted import roots, preserving original source paths.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound::{Included, Unbounded},
};

use rift_core::SymbolId;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::index::python_identifier_is_valid;
use rift_syntax::{ScopedIdentityAnchors, SyntaxFacts};

use crate::{ExactPackageInput, NamespaceInput, PackageImportRoot, PackageImportRootOrigin};

mod errors;
pub(crate) mod python;
mod rust;
pub(crate) mod typescript;

use self::errors::retain_valid_declarations;

/// One selected file with its retained, path-independent syntax facts.
pub(crate) struct SelectedFile<'source> {
    pub(crate) path: &'source rift_core::ProjectPath,
    pub(crate) source: &'source str,
    pub(crate) syntax: &'source SyntaxFacts,
}

/// Original file paths and exact provider declaration names mapped to established IDs.
pub(super) type Anchors = BTreeMap<String, BTreeMap<String, SymbolId>>;

#[cfg(test)]
mod digest_tests;
#[cfg(test)]
mod symbol_ids;

/// One export binding whose defining declaration and logical owner are established.
pub(crate) struct ExportBinding {
    pub(crate) path: String,
    pub(crate) binding: rift_syntax::SyntaxExportBinding,
    pub(crate) identity: SymbolId,
    pub(crate) target: SymbolId,
    pub(crate) target_path: String,
    pub(crate) target_qualified_name: String,
    pub(crate) name: String,
    pub(crate) qualified_name: String,
}

/// Established declaration anchors and explicit export bindings from one selected inventory.
#[derive(Default)]
pub(crate) struct PreparedNamespace {
    pub(crate) anchors: Anchors,
    pub(crate) scoped_anchors: BTreeMap<String, ScopedIdentityAnchors>,
    /// The source path and accepted crate namespace of a Cargo library root.
    pub(crate) rust_package_root: Option<(String, SymbolId)>,
    /// The module of each Rust file a captured library's module tree reaches.
    pub(crate) rust_modules: BTreeMap<String, SymbolId>,
    /// Rust units with an authored module declaration and a unique checked source.
    pub(crate) rust_module_sources: BTreeSet<String>,
    /// Written method names selected by a local Rust macro.
    pub(crate) rust_macro_methods: BTreeMap<String, Vec<rift_syntax::SyntaxSymbol>>,
    /// Separate logical Rust owners of the same checked physical source.
    pub(crate) shared_rust_modules: BTreeMap<String, Vec<rust::SharedRustModule>>,
    pub(crate) public_names: BTreeMap<String, BTreeSet<String>>,
    /// Observed `CommonJS` files with a checked synchronized export object.
    pub(crate) commonjs_modules: BTreeSet<String>,
    pub(crate) mappings: BTreeMap<String, BTreeMap<String, rift_syntax::LogicalDeclaration>>,
    pub(crate) exports: Vec<ExportBinding>,
    pub(crate) alias_visibilities: BTreeMap<SymbolId, String>,
    pub(crate) unresolved_exports: bool,
    /// Files whose declarations require an export alias or an ambient module name to be
    /// public, with the names of their ambient modules.
    pub(crate) unreached_modules: BTreeMap<String, BTreeSet<String>>,
    /// Original file paths no namespace reaches by construction: their declarations carry
    /// no identity because the language assigns none there, not because analysis failed.
    pub(crate) outside_namespaces: BTreeSet<String>,
}

impl PreparedNamespace {
    /// The names of each unreached file that stay public: its ambient modules, and every
    /// name an alias of a reached file exposes, with `.` for a file exposed whole.
    ///
    /// A whole-file exposure exposes that file's own aliases in turn. Each file becomes
    /// reached at most once, so the walk reads every export at most once.
    pub(crate) fn exposed_names(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut exposed = self.unreached_modules.clone();
        if exposed.is_empty() {
            return exposed;
        }
        let mut exports = BTreeMap::<&str, Vec<&ExportBinding>>::new();
        for export in &self.exports {
            exports
                .entry(export.path.as_str())
                .or_default()
                .push(export);
        }
        let mut reached = exports
            .keys()
            .copied()
            .filter(|path| !self.unreached_modules.contains_key(*path))
            .collect::<Vec<_>>();
        while let Some(path) = reached.pop() {
            for export in exports.remove(path).into_iter().flatten() {
                let Some(names) = exposed.get_mut(&export.target_path) else {
                    continue;
                };
                names.insert(export.target_qualified_name.clone());
                if export.target_qualified_name == "." {
                    reached.push(export.target_path.as_str());
                }
            }
        }
        exposed
    }
}

pub(crate) fn placed_aliases(
    input: &NamespaceInput<'_>,
    prepared: &mut PreparedNamespace,
    selected: &[SelectedFile<'_>],
    placements: &[&rift_syntax::DocumentPlacement],
    aliases_max: usize,
    invalid: impl Fn() -> rift_error::RiftError,
) -> Result<BTreeMap<String, Vec<rift_syntax::PlacedAlias>>, rift_error::RiftError> {
    crate::static_resolution::prepare_javascript_reexports(
        input,
        prepared,
        selected,
        placements,
        aliases_max,
    );
    let positions = selected
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let mut aliases = BTreeMap::<String, Vec<rift_syntax::PlacedAlias>>::new();
    for export in &prepared.exports {
        let source = *positions.get(export.path.as_str()).ok_or_else(&invalid)?;
        let range = export.binding.range;
        let bytes = usize::try_from(range.start)
            .ok()
            .zip(usize::try_from(range.end).ok())
            .filter(|(start, end)| start < end)
            .and_then(|(start, end)| selected[source].source.get(start..end));
        if bytes.is_none() {
            return Err(invalid());
        }
        let owner = rift_protocol::identity::SymbolIdentity::parse(export.identity.as_str())
            .map_err(|_| invalid())?;
        let target_owner = rift_protocol::identity::SymbolIdentity::parse(export.target.as_str())
            .map_err(|_| invalid())?;
        let target_identity = if owner.owner() == target_owner.owner() {
            let target = *positions
                .get(export.target_path.as_str())
                .ok_or_else(&invalid)?;
            let placement = placements.get(target).ok_or_else(&invalid)?;
            rift_core::symbol_identity(
                &selected[target].syntax.language().identity_segment(),
                placement.identity_path(),
                &export.target_qualified_name,
            )
        } else {
            export.target.as_str().to_owned()
        };
        aliases
            .entry(export.path.clone())
            .or_default()
            .push(rift_syntax::PlacedAlias {
                name: export.name.clone(),
                qualified_name: export.qualified_name.clone(),
                identity: export.identity.clone(),
                target_identity,
                range,
                visibility: prepared.alias_visibilities.get(&export.identity).cloned(),
            });
    }
    Ok(aliases)
}

/// Prepares namespace assignments once over the admitted selected-file inventory.
pub(super) fn prepare(
    input: &ExactPackageInput<'_>,
    selected: &[SelectedFile<'_>],
) -> PreparedNamespace {
    prepare_context(&input.namespace_input(), selected)
}

/// Requests the path states that decide a build manifest's import roots or entry, without
/// using selected-file absence as evidence.
#[must_use]
pub fn build_paths(
    metadata: crate::PackageSource<'_>,
    files: &[crate::PackageSource<'_>],
    syntax: rift_syntax::SyntaxLimits,
) -> Option<Vec<rift_protocol::read::ProjectPath>> {
    typescript::build_paths(metadata, syntax.source_bytes_max())
        .or_else(|| python::build_paths(metadata, files, syntax))
}

pub(crate) fn rust_configuration_is_valid(flags: &[&str]) -> bool {
    rust::configuration::valid_flags(flags)
}

pub(crate) fn library(input: &NamespaceInput<'_>) -> Option<(String, String)> {
    rust::library(input)
}

pub(crate) fn prepare_context(
    input: &NamespaceInput<'_>,
    selected: &[SelectedFile<'_>],
) -> PreparedNamespace {
    let mut prepared = typescript::prepare(input, selected);
    let rust = rust::prepare(input, selected);
    prepared.anchors.extend(rust.anchors);
    prepared.rust_package_root = rust.rust_package_root;
    prepared.rust_modules = rust.rust_modules;
    prepared.rust_module_sources = rust.rust_module_sources;
    prepared.rust_macro_methods = rust.rust_macro_methods;
    prepared.public_names.extend(rust.public_names);
    prepared.shared_rust_modules = rust.shared_rust_modules;
    prepared.exports.extend(rust.exports);
    prepared.alias_visibilities.extend(rust.alias_visibilities);
    prepared.unresolved_exports |= rust.unresolved_exports;
    prepared.outside_namespaces.extend(rust.outside_namespaces);
    let mut roots = input.import_roots().to_vec();
    for root in python::import_roots(input) {
        if !roots.iter().any(|accepted| {
            accepted.prefix() == root.prefix() && accepted.modules() == root.modules()
        }) {
            roots.push(root);
        }
    }
    if roots.is_empty()
        && let Some(root) = python::stub_wheel_root(input)
    {
        roots.push(root);
    }
    let roots_accepted = crate::input::validate_roots(&roots)
        .and_then(|()| {
            crate::input::validate_runtime_roots(
                input.owner(),
                input.language(),
                input.files(),
                &roots,
            )
        })
        .is_ok();
    let captured = input
        .files()
        .iter()
        .map(|source| (source.path().as_str(), *source))
        .collect::<BTreeMap<_, _>>();
    let mut ordinary_python_paths = BTreeSet::new();
    if roots_accepted
        && roots
            .iter()
            .any(|root| root.origin() == PackageImportRootOrigin::Wheel)
    {
        ordinary_python_paths.extend(captured.iter().filter_map(|(path, source)| {
            if python_script_origin(input, *source).is_some() {
                return None;
            }
            let (base, extension) = path.rsplit_once('.')?;
            matches!(extension, "py" | "pyi").then_some(base)
        }));
    }
    for file in selected {
        if file.syntax.language().name == "python" {
            prepared.anchors.insert(
                file.path.as_str().to_owned(),
                captured
                    .get(file.path.as_str())
                    .filter(|source| roots_accepted && source.text() == file.source)
                    .map_or_else(BTreeMap::new, |source| {
                        identity_anchors_with_roots(
                            input,
                            file.syntax,
                            file.path.as_str(),
                            *source,
                            &roots,
                            &ordinary_python_paths,
                        )
                    }),
            );
        }
    }
    if roots_accepted {
        python::prepare_exports(input, selected, &captured, &roots, &mut prepared);
    }
    prepare_documents(input, selected, &captured, &mut prepared);
    prepared
}

fn prepare_documents(
    input: &NamespaceInput<'_>,
    selected: &[SelectedFile<'_>],
    captured: &BTreeMap<&str, crate::PackageSource<'_>>,
    prepared: &mut PreparedNamespace,
) {
    for file in selected {
        let language = file.syntax.language();
        let document = matches!(
            language.name.as_str(),
            "markdown" | "json" | "yaml" | "toml"
        );
        if !document || language.dialect.is_some() {
            continue;
        }
        // Selection pairs each file's syntax facts with the bytes they were parsed from, and
        // the provider recorded the digest of those bytes. Membership by path, the byte
        // length, and that recorded digest identify the captured source; comparing or
        // hashing the content again ran over every document on every index pass.
        if file.syntax.source_digest().is_none()
            || captured
                .get(file.path.as_str())
                .is_none_or(|source| source.text().len() != file.source.len())
        {
            continue;
        }
        let anchors = file
            .syntax
            .symbols()
            .iter()
            .filter_map(|symbol| {
                let path = vec![file.path.as_str().to_owned(), symbol.qualified_name.clone()];
                let identity =
                    SymbolIdentity::new(input.owner().clone(), language.clone(), path).ok()?;
                let identity = SymbolId::new(identity.wire_identity()).ok()?;
                Some((symbol.qualified_name.clone(), identity))
            })
            .collect();
        prepared.anchors.insert(
            file.path.as_str().to_owned(),
            retain_valid_declarations(input, file, anchors),
        );
    }
}

fn identity_anchors_with_roots(
    input: &NamespaceInput<'_>,
    syntax: &SyntaxFacts,
    path: &str,
    source: crate::PackageSource<'_>,
    roots: &[PackageImportRoot],
    ordinary_python_paths: &BTreeSet<&str>,
) -> BTreeMap<String, SymbolId> {
    if syntax.language().name != "python" || syntax.language().dialect.is_some() {
        return BTreeMap::new();
    }
    let digest = source.source_digest();
    if syntax.source_digest() != Some(&digest) {
        return BTreeMap::new();
    }
    let local_fallback = input.owner().is_local() && input.import_roots().is_empty();
    let Some(module) = python_script_origin(input, source)
        .map(|original| vec![format!("./{original}")])
        .or_else(|| python_module_with_inventory(path, roots, ordinary_python_paths))
        .or_else(|| selected_python_script(input, source, roots).then(|| vec![format!("./{path}")]))
        .or_else(|| {
            local_fallback
                .then(|| python::local_module(input, path))
                .flatten()
        })
    else {
        return BTreeMap::new();
    };
    let mut anchors = BTreeMap::new();
    for symbol in syntax.symbols() {
        if (symbol.is_python_parameter()
            || symbol.is_python_loop_binding()
            || symbol.is_python_assignment_expression_binding()
            || symbol.is_python_comprehension_binding())
            && (input.release_version().is_none()
                || !matches!(
                    input.owner(),
                    rift_protocol::identity::SymbolOwner::Package { .. }
                ))
        {
            continue;
        }
        let qualified_path = if symbol.kind == "module" {
            module.clone()
        } else {
            let Some(name) = python_declaration_name(symbol) else {
                continue;
            };
            let expression = if symbol.is_python_comprehension_binding() {
                Some("$comprehension#")
            } else if symbol.is_python_lambda_parameter() {
                Some("$lambda#")
            } else {
                None
            };
            let names = if let Some(expression) = expression {
                let Some(names) = python_expression_binding_path(symbol, &name, expression) else {
                    continue;
                };
                names
            } else {
                let names = name.split('.').collect::<Vec<_>>();
                if symbol.qualified_name != name
                    || !names.iter().all(|name| python_identifier_is_valid(name))
                {
                    continue;
                }
                names
            };
            let mut qualified_path = module.clone();
            qualified_path.extend(names.into_iter().map(str::to_owned));
            qualified_path
        };
        let Ok(identity) = SymbolIdentity::new(
            input.owner().clone(),
            syntax.language().clone(),
            qualified_path,
        ) else {
            continue;
        };
        if let Ok(identity) = SymbolId::new(identity.wire_identity()) {
            anchors.insert(symbol.qualified_name.clone(), identity);
        }
    }
    let file = SelectedFile {
        path: source.path(),
        source: source.text(),
        syntax,
    };
    retain_valid_declarations(input, &file, anchors)
}

/// One archive script explicitly selected through a synthetic Python analysis suffix.
fn python_script_origin<'source>(
    input: &NamespaceInput<'_>,
    source: crate::PackageSource<'source>,
) -> Option<&'source str> {
    let SymbolOwner::Package { manager, .. } = input.owner() else {
        return None;
    };
    let physical = source.source_unit()?;
    let original = physical.key().as_str();
    let extension = original.rsplit_once('.');
    (manager == "pypi"
        && physical.source_owner()? == input.owner()
        && source.origin()?.source_kind() == rift_protocol::read::SourceKind::Authored
        && !matches!(extension, Some((_, "py" | "pyi")))
        && source.path().as_str().strip_suffix(".py")? == original)
        .then_some(original)
}

/// A selected Python script with a hyphenated filename under an established package.
fn selected_python_script(
    input: &NamespaceInput<'_>,
    source: crate::PackageSource<'_>,
    roots: &[PackageImportRoot],
) -> bool {
    let SymbolOwner::Package { manager, .. } = input.owner() else {
        return false;
    };
    let path = source.path().as_str();
    let Some((directory, stem)) = path
        .strip_suffix(".py")
        .and_then(|path| path.rsplit_once('/'))
    else {
        return false;
    };
    manager == "pypi"
        && stem.contains('-')
        && stem.split(['-', '.']).all(python_identifier_is_valid)
        && source.source_unit().is_none_or(|unit| {
            unit.source_owner() == Some(input.owner())
                && unit.key().as_str() == path
                && source.origin().is_some_and(|origin| {
                    origin.source_kind() == rift_protocol::read::SourceKind::Authored
                })
        })
        && roots.iter().any(|root| {
            !matches!(
                root.origin(),
                PackageImportRootOrigin::PyModules | PackageImportRootOrigin::Runtime
            ) && python_path_under_root(root, directory).is_some_and(|relative| {
                relative.split('/').all(python_identifier_is_valid)
                    && python_root_contains_module(root, &relative.replace('/', "."))
            })
        })
}

fn python_declaration_name(symbol: &rift_syntax::SyntaxSymbol) -> Option<String> {
    if !python_identifier_is_valid(&symbol.name) {
        return None;
    }
    match symbol.container.as_deref() {
        Some(container) if container.split('.').all(python_identifier_is_valid) => {
            Some(format!("{container}.{}", symbol.name))
        }
        Some(_) => None,
        None => Some(symbol.name.clone()),
    }
}

/// The checked expression scope before its ordinary Python binding name.
fn python_expression_binding_path<'a>(
    symbol: &'a rift_syntax::SyntaxSymbol,
    ordinary: &str,
    expression: &str,
) -> Option<Vec<&'a str>> {
    let parent = ordinary.strip_suffix(symbol.name.as_str())?;
    let prefix = symbol.qualified_name.strip_prefix(parent)?;
    let (scope, name) = prefix.split_once('.')?;
    let position = scope.strip_prefix(expression)?;
    if name != symbol.name.as_str()
        || position.is_empty()
        || !position.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some(symbol.qualified_name.split('.').collect())
}

#[cfg(test)]
fn python_module(path: &str, roots: &[PackageImportRoot]) -> Option<Vec<String>> {
    python_module_with_inventory(path, roots, &BTreeSet::new())
}

fn python_module_with_inventory(
    path: &str,
    roots: &[PackageImportRoot],
    ordinary_python_paths: &BTreeSet<&str>,
) -> Option<Vec<String>> {
    let mut modules = BTreeSet::new();
    for root in roots {
        let Some(relative) = python_path_under_root(root, path) else {
            continue;
        };
        let Some((base, extension)) = relative.rsplit_once('.') else {
            continue;
        };
        if !matches!(extension, "py" | "pyi") {
            continue;
        }
        let mut parts = base.split('/').collect::<Vec<_>>();
        // A stub-only wheel uses a physical -stubs directory for the admitted module.
        // Its declarations keep their original stub source paths.
        if root.origin() == PackageImportRootOrigin::Wheel
            && extension == "pyi"
            && let Some((first, nested)) = parts.split_first_mut()
            && !nested.is_empty()
            && let Some(module) = (*first).strip_suffix("-stubs")
        {
            if has_ordinary_python_root(root, module, ordinary_python_paths) {
                continue;
            }
            *first = module;
        }
        if parts.last() == Some(&"__init__") {
            parts.pop();
        }
        if parts.is_empty() || !parts.iter().all(|part| python_identifier_is_valid(part)) {
            continue;
        }
        let spelling = parts.join(".");
        if !python_root_contains_module(root, &spelling) {
            continue;
        }
        modules.insert(parts.into_iter().map(str::to_owned).collect::<Vec<_>>());
    }
    (modules.len() == 1)
        .then(|| modules.into_iter().next())
        .flatten()
}

fn python_path_under_root<'path>(root: &PackageImportRoot, path: &'path str) -> Option<&'path str> {
    match root.prefix() {
        Some(prefix) => path.strip_prefix(prefix.as_str())?.strip_prefix('/'),
        None => Some(path),
    }
}

fn python_root_contains_module(root: &PackageImportRoot, spelling: &str) -> bool {
    root.origin() == PackageImportRootOrigin::Runtime
        || root.modules().iter().any(|module| {
            spelling == module
                || (root.origin() != PackageImportRootOrigin::PyModules
                    && spelling
                        .strip_prefix(module.as_str())
                        .is_some_and(|tail| tail.starts_with('.')))
        })
}

/// Selected ordinary source under this import root makes the layout non-stub-only.
fn has_ordinary_python_root(
    root: &PackageImportRoot,
    module: &str,
    paths: &BTreeSet<&str>,
) -> bool {
    let base = root
        .prefix()
        .map_or_else(|| module.to_owned(), |prefix| format!("{prefix}/{module}"));
    if paths.contains(base.as_str()) {
        return true;
    }
    let directory = base + "/";
    paths
        .range::<str, _>((Included(directory.as_str()), Unbounded))
        .next()
        .is_some_and(|path| path.starts_with(&directory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PackageImportRootOrigin;
    use rift_core::ProjectPath;

    fn root(
        prefix: Option<&str>,
        modules: &[&str],
        origin: PackageImportRootOrigin,
    ) -> PackageImportRoot {
        PackageImportRoot::new(
            prefix.map(|path| ProjectPath::new(path.to_owned()).expect("source prefix")),
            modules.iter().map(|module| (*module).to_owned()).collect(),
            origin,
        )
        .expect("verified import root")
    }

    #[test]
    fn wheel_and_source_roots_name_same_module_without_rewriting_paths() {
        let wheel = [root(
            None,
            &["six", "click"],
            PackageImportRootOrigin::Wheel,
        )];
        let six_source = [root(None, &["six"], PackageImportRootOrigin::PyModules)];
        let click_source = [root(Some("src"), &["click"], PackageImportRootOrigin::Flit)];
        assert_eq!(
            python_module("six.py", &wheel),
            Some(vec!["six".to_owned()])
        );
        assert_eq!(
            python_module("six.pyi", &six_source),
            Some(vec!["six".to_owned()])
        );
        assert_eq!(
            python_module("click/core.py", &wheel),
            python_module("src/click/core.pyi", &click_source)
        );
        assert_eq!(
            python_module("src/click/__init__.py", &click_source),
            Some(vec!["click".to_owned()])
        );
        assert_eq!(
            python_module("src/click/core.py", &wheel),
            None,
            "src is not stripped without an accepted root"
        );
        assert_eq!(
            python_module("src2/click/core.py", &click_source),
            None,
            "prefix ends at a path boundary"
        );
    }

    #[test]
    fn moved_sources_and_equal_basenames_keep_module_boundaries() {
        let first = [root(
            Some("src"),
            &["alpha", "beta"],
            PackageImportRootOrigin::Flit,
        )];
        let moved = [root(
            Some("lib"),
            &["alpha", "beta"],
            PackageImportRootOrigin::Flit,
        )];
        assert_eq!(
            python_module("src/alpha/core.py", &first),
            python_module("lib/alpha/core.py", &moved)
        );
        assert_ne!(
            python_module("src/alpha/core.py", &first),
            python_module("src/beta/core.py", &first)
        );
        assert_eq!(python_module("src/alphabeta/core.py", &first), None);
        assert_eq!(python_module("src/alpha/core.txt", &first), None);
        assert_eq!(python_module("src/alpha/../core.py", &first), None);
    }

    #[test]
    fn unknown_and_competing_roots_cannot_establish_module() {
        assert_eq!(python_module("click/core.py", &[]), None);
        let roots = [
            root(None, &["alpha"], PackageImportRootOrigin::Wheel),
            root(Some("alpha"), &["core"], PackageImportRootOrigin::Flit),
        ];
        assert_eq!(python_module("alpha/core.py", &roots), None);
    }

    #[test]
    fn explicit_modules_do_not_establish_package_children() {
        let modules = [root(None, &["main"], PackageImportRootOrigin::PyModules)];
        assert_eq!(
            python_module("main.py", &modules),
            Some(vec!["main".to_owned()])
        );
        assert_eq!(
            python_module("main.pyi", &modules),
            Some(vec!["main".to_owned()])
        );
        assert_eq!(python_module("main/child.py", &modules), None);
        for origin in [
            PackageImportRootOrigin::Wheel,
            PackageImportRootOrigin::Flit,
            PackageImportRootOrigin::Pdm,
        ] {
            let packages = [root(None, &["main"], origin)];
            assert_eq!(
                python_module("main/child.py", &packages),
                Some(vec!["main".to_owned(), "child".to_owned()])
            );
        }
    }

    #[test]
    fn wheel_data_roots_admit_only_selected_import_trees() {
        let roots = [root(
            Some("six-1.17.0.data/purelib"),
            &["six"],
            PackageImportRootOrigin::Wheel,
        )];
        assert_eq!(
            python_module("six-1.17.0.data/purelib/six.py", &roots),
            Some(vec!["six".to_owned()])
        );
        assert_eq!(
            python_module("six-1.17.0.data/scripts/six.py", &roots),
            None
        );
        assert_eq!(python_module("six-1.17.0.data/data/six.py", &roots), None);
    }
}

#[cfg(test)]
mod runtime_tests;

pub(crate) fn namespace_paths(
    input: &NamespaceInput<'_>,
    documents: &[&crate::IndexedFile],
    contains: &dyn Fn(&str) -> bool,
    selected: &BTreeSet<rift_core::ProjectPath>,
    files_max: usize,
) -> Result<BTreeSet<rift_core::ProjectPath>, rift_error::RiftError> {
    rust::namespace_paths(input, documents, contains, selected, files_max)
}

pub(crate) use rust::chain_modules as rust_chain_modules;
pub(crate) use rust::configuration::{
    Configuration as RustConfiguration, Configurations as RustConfigurations,
};
pub(crate) use rust::file_scope as rust_file_scope;
