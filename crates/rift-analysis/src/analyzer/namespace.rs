//! Logical Python placement from accepted import roots, preserving original source paths.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::identity::{SymbolIdentity, SymbolOccurrence};
use rift_protocol::index::python_identifier_is_valid;
use rift_syntax::SyntaxFacts;

use crate::{ExactPackageInput, NamespaceInput, PackageImportRoot, PackageImportRootOrigin};

pub(crate) mod python;
mod rust;
mod typescript;

/// One selected file with its retained, path-independent syntax facts.
pub(crate) struct SelectedFile<'source> {
    pub(crate) path: &'source rift_core::ProjectPath,
    pub(crate) source: &'source str,
    pub(crate) syntax: &'source SyntaxFacts,
}

/// Original file paths and exact provider declaration names mapped to established IDs.
pub(super) type Anchors = BTreeMap<String, BTreeMap<String, SymbolId>>;

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
    pub(crate) mappings: BTreeMap<String, BTreeMap<String, rift_syntax::LogicalDeclaration>>,
    pub(crate) exports: Vec<ExportBinding>,
    pub(crate) unresolved_exports: bool,
}

pub(crate) fn placed_aliases(
    prepared: &PreparedNamespace,
    selected: &[SelectedFile<'_>],
    placements: &[&rift_syntax::DocumentPlacement],
    invalid: impl Fn() -> rift_error::RiftError,
) -> Result<BTreeMap<String, Vec<rift_syntax::PlacedAlias>>, rift_error::RiftError> {
    let positions = selected
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let mut aliases = BTreeMap::<String, Vec<rift_syntax::PlacedAlias>>::new();
    for export in &prepared.exports {
        let source = *positions.get(export.path.as_str()).ok_or_else(&invalid)?;
        let target = *positions
            .get(export.target_path.as_str())
            .ok_or_else(&invalid)?;
        let range = export.binding.range;
        let bytes = usize::try_from(range.start)
            .ok()
            .zip(usize::try_from(range.end).ok())
            .filter(|(start, end)| start < end)
            .and_then(|(start, end)| selected[source].source.get(start..end));
        if bytes.is_none() {
            return Err(invalid());
        }
        let placement = placements.get(target).ok_or_else(&invalid)?;
        aliases
            .entry(export.path.clone())
            .or_default()
            .push(rift_syntax::PlacedAlias {
                name: export.name.clone(),
                qualified_name: export.qualified_name.clone(),
                identity: export.identity.clone(),
                target_identity: rift_core::symbol_identity(
                    &selected[target].syntax.language().identity_segment(),
                    placement.identity_path(),
                    &export.target_qualified_name,
                ),
                range,
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

pub(crate) fn prepare_context(
    input: &NamespaceInput<'_>,
    selected: &[SelectedFile<'_>],
) -> PreparedNamespace {
    let mut prepared = typescript::prepare(input, selected);
    prepared.anchors.extend(rust::prepare(input, selected));
    let mut roots = input.import_roots().to_vec();
    for root in python::import_roots(input) {
        if !roots.iter().any(|accepted| {
            accepted.prefix() == root.prefix() && accepted.modules() == root.modules()
        }) {
            roots.push(root);
        }
    }
    let roots_accepted = crate::input::validate_roots(&roots).is_ok();
    for file in selected {
        if file.syntax.language().name == "python" {
            prepared.anchors.insert(
                file.path.as_str().to_owned(),
                if roots_accepted {
                    identity_anchors_with_roots(
                        input,
                        file.syntax,
                        file.path.as_str(),
                        file.source,
                        &roots,
                    )
                } else {
                    BTreeMap::new()
                },
            );
        }
    }
    prepared
}

fn identity_anchors_with_roots(
    input: &NamespaceInput<'_>,
    syntax: &SyntaxFacts,
    path: &str,
    source: &str,
    roots: &[PackageImportRoot],
) -> BTreeMap<String, SymbolId> {
    if syntax.language().name != "python"
        || syntax.language().dialect.is_some()
        || syntax.has_errors()
        || syntax.source_digest() != Some(&rift_core::FileDigest::of(source.as_bytes()))
    {
        return BTreeMap::new();
    }
    let Some(module) = python_module(path, roots) else {
        return BTreeMap::new();
    };
    let revision = crate::documentation::content_digest(source.as_bytes()).0;
    let mut occurrences = BTreeMap::<String, usize>::new();
    let mut declarations = BTreeMap::<String, Vec<&rift_syntax::SyntaxSymbol>>::new();
    for symbol in syntax.symbols() {
        if let Some(name) = python_declaration_name(symbol) {
            *occurrences.entry(name.clone()).or_default() += 1;
            declarations.entry(name).or_default().push(symbol);
        }
    }
    let overloads = declarations
        .iter()
        .filter_map(|(name, symbols)| {
            let mut ordinary = 0_usize;
            let mut overloaded = 0_usize;
            for symbol in symbols {
                if symbol.kind != "function" {
                    return None;
                }
                match symbol.python_overload.as_ref() {
                    Some(rift_syntax::PythonOverload::Overload { .. }) if ordinary == 0 => {
                        overloaded += 1;
                    }
                    Some(rift_syntax::PythonOverload::Ordinary) if ordinary == 0 => ordinary += 1,
                    _ => return None,
                }
            }
            (overloaded > 0).then(|| name.clone())
        })
        .collect::<BTreeSet<_>>();
    let mut anchors = BTreeMap::new();
    for symbol in syntax.symbols() {
        let Some(name) = python_declaration_name(symbol) else {
            continue;
        };
        let occurrence = if symbol.qualified_name == name || overloads.contains(&name) {
            None
        } else {
            if occurrences.get(&name).copied().unwrap_or_default() < 2 {
                continue;
            }
            let Some(number) = symbol
                .qualified_name
                .strip_prefix(&name)
                .and_then(|suffix| suffix.strip_prefix('~'))
                .and_then(|number| number.parse::<u32>().ok())
            else {
                continue;
            };
            Some(number)
        };
        let names = name.split('.').collect::<Vec<_>>();
        if !names.iter().all(|name| python_identifier_is_valid(name)) {
            continue;
        }
        let mut qualified_path = module.clone();
        qualified_path.extend(names.into_iter().map(str::to_owned));
        let Ok(mut identity) = SymbolIdentity::new(
            input.owner().clone(),
            syntax.language().clone(),
            qualified_path,
        ) else {
            continue;
        };
        if let Some(number) = occurrence {
            let Ok(occurrence) = SymbolOccurrence::new(number, revision.clone()) else {
                continue;
            };
            let Ok(qualified) = identity.with_occurrence(occurrence) else {
                continue;
            };
            identity = qualified;
        }
        if let Ok(identity) = SymbolId::new(identity.wire_identity()) {
            anchors.insert(symbol.qualified_name.clone(), identity);
        }
    }
    anchors
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

fn python_module(path: &str, roots: &[PackageImportRoot]) -> Option<Vec<String>> {
    let mut modules = BTreeSet::new();
    for root in roots {
        let relative = match root.prefix() {
            Some(prefix) => {
                let Some(relative) = path
                    .strip_prefix(prefix.as_str())
                    .and_then(|tail| tail.strip_prefix('/'))
                else {
                    continue;
                };
                relative
            }
            None => path,
        };
        let Some((base, extension)) = relative.rsplit_once('.') else {
            continue;
        };
        if !matches!(extension, "py" | "pyi") {
            continue;
        }
        let mut parts = base.split('/').collect::<Vec<_>>();
        if parts.last() == Some(&"__init__") {
            parts.pop();
        }
        if parts.is_empty() || !parts.iter().all(|part| python_identifier_is_valid(part)) {
            continue;
        }
        let spelling = parts.join(".");
        if !root.modules().iter().any(|module| {
            spelling == *module
                || (root.origin() != PackageImportRootOrigin::PyModules
                    && spelling
                        .strip_prefix(module.as_str())
                        .is_some_and(|tail| tail.starts_with('.')))
        }) {
            continue;
        }
        modules.insert(parts.into_iter().map(str::to_owned).collect::<Vec<_>>());
    }
    (modules.len() == 1)
        .then(|| modules.into_iter().next())
        .flatten()
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
