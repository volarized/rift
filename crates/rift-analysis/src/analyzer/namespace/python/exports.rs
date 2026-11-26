//! Explicit Python re-exports resolved against captured module declarations.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::index::python_identifier_is_valid;
use rift_syntax::{ShippedLanguage, SyntaxExportBinding, SyntaxExportKind};

use super::super::{ExportBinding, PreparedNamespace, SelectedFile, typescript};
use crate::{NamespaceInput, PackageImportRoot, PackageSource};

type Target<'source> = (&'source [String], &'source str);
type Definition = (SymbolId, String, String);

struct PythonReexport<'source> {
    path: &'source str,
    binding: &'source SyntaxExportBinding,
    module: Vec<String>,
    name: String,
    target_module: Vec<String>,
    target_name: String,
}

fn established_module(
    file: &SelectedFile<'_>,
    captured: &BTreeMap<&str, PackageSource<'_>>,
    prepared: &PreparedNamespace,
) -> Option<Vec<String>> {
    let syntax = file.syntax;
    if syntax.language() != &ShippedLanguage::Python.language() {
        return None;
    }
    // Selection pairs each file's syntax facts with the bytes they were parsed from, and
    // the provider recorded the digest of those bytes. Membership by path, the byte
    // length, and that recorded digest identify the captured source; comparing or
    // hashing the content again ran over every selected file on every index pass.
    if syntax.source_digest().is_none()
        || captured
            .get(file.path.as_str())
            .is_none_or(|source| source.text().len() != file.source.len())
    {
        return None;
    }
    let identity = prepared.anchors.get(file.path.as_str())?.get(".")?;
    let identity = SymbolIdentity::parse(identity.as_str()).ok()?;
    let module = identity.qualified_path();
    module
        .iter()
        .all(|part| python_identifier_is_valid(part))
        .then(|| module.to_vec())
}

/// Leading dots climb from the importing package without passing its top level.
pub(crate) fn python_import_target(
    module: &[String],
    is_package: bool,
    written: &str,
) -> Option<Vec<String>> {
    let rest = written.trim_start_matches('.');
    let dots = written.len() - rest.len();
    let mut target = if dots == 0 {
        Vec::new()
    } else {
        let package = if is_package {
            module
        } else {
            module.split_last()?.1
        };
        if dots > package.len() {
            return None;
        }
        package[..package.len().checked_sub(dots - 1)?].to_vec()
    };
    if !rest.is_empty() {
        for part in rest.split('.') {
            if !python_identifier_is_valid(part) {
                return None;
            }
            target.push(part.to_owned());
        }
    }
    (!target.is_empty()).then_some(target)
}

fn requests<'source>(
    file: &SelectedFile<'source>,
    module: &[String],
    unresolved: &mut bool,
) -> Vec<PythonReexport<'source>> {
    let is_package = file
        .path
        .as_str()
        .rsplit('/')
        .next()
        .is_some_and(|name| matches!(name, "__init__.py" | "__init__.pyi"));
    let spelled = |range: Option<rift_syntax::ByteRange>| {
        let range = range?;
        file.source
            .get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
    };
    let mut requests = Vec::new();
    // A statement that lists one name twice re-exports it once.
    let mut listed = BTreeSet::new();
    for binding in file.syntax.export_bindings().unwrap_or_default() {
        if binding.kind != SyntaxExportKind::Named {
            continue;
        }
        let (Some(target_name), Some(name), Some(written)) = (
            spelled(binding.local),
            spelled(binding.exported),
            spelled(binding.source),
        ) else {
            continue;
        };
        if !listed.insert((binding.range, name, target_name)) {
            continue;
        }
        let Some(target_module) = python_import_target(module, is_package, written) else {
            *unresolved = true;
            continue;
        };
        requests.push(PythonReexport {
            path: file.path.as_str(),
            binding,
            module: module.to_vec(),
            name: name.to_owned(),
            target_module,
            target_name: target_name.to_owned(),
        });
    }
    requests
}

fn declaration(
    target: Target<'_>,
    modules: &BTreeMap<Vec<String>, &str>,
    prepared: &PreparedNamespace,
) -> Option<Definition> {
    let path = *modules.get(target.0)?;
    let identity = prepared.anchors.get(path)?.get(target.1)?;
    Some((identity.clone(), path.to_owned(), target.1.to_owned()))
}

/// Each target is followed once and cached, including cycles and absent targets.
/// A chain takes at most the recorded re-export count plus its defining target.
fn resolve<'source>(
    mut target: Target<'source>,
    modules: &BTreeMap<Vec<String>, &str>,
    reexported: &BTreeMap<Target<'source>, Target<'source>>,
    resolved: &mut BTreeMap<Target<'source>, Option<Definition>>,
    prepared: &PreparedNamespace,
) -> Option<Definition> {
    let mut followed = Vec::new();
    let mut defined = None;
    for _ in 0..=reexported.len() {
        if let Some(known) = resolved.get(&target) {
            defined.clone_from(known);
            break;
        }
        resolved.insert(target, None);
        followed.push(target);
        if let Some(declared) = declaration(target, modules, prepared) {
            defined = Some(declared);
            break;
        }
        let Some(next) = reexported.get(&target) else {
            break;
        };
        target = *next;
    }
    for target in followed {
        resolved.insert(target, defined.clone());
    }
    defined
}

fn export(
    input: &NamespaceInput<'_>,
    request: &PythonReexport<'_>,
    defined: Definition,
    anchors: &BTreeSet<&SymbolId>,
) -> Option<ExportBinding> {
    let (target, target_path, target_qualified_name) = defined;
    let mut alias_path = request.module.clone();
    alias_path.push(request.name.clone());
    let identity = SymbolIdentity::new(
        input.owner().clone(),
        ShippedLanguage::Python.language(),
        alias_path,
    )
    .ok()?;
    let identity = SymbolId::new(identity.wire_identity()).ok()?;
    if identity == target || anchors.contains(&identity) {
        return None;
    }
    Some(ExportBinding {
        path: request.path.to_owned(),
        binding: request.binding.clone(),
        identity,
        target,
        target_path,
        target_qualified_name,
        name: request.name.clone(),
        qualified_name: request.name.clone(),
    })
}

/// Places aliases at their importing module and retains their defining declaration.
pub(in crate::analyzer::namespace) fn prepare(
    input: &NamespaceInput<'_>,
    selected: &[SelectedFile<'_>],
    captured: &BTreeMap<&str, PackageSource<'_>>,
    _roots: &[PackageImportRoot],
    prepared: &mut PreparedNamespace,
) {
    let mut modules = BTreeMap::new();
    let mut imports = Vec::new();
    for file in selected {
        let Some(module) = established_module(file, captured, prepared) else {
            continue;
        };
        imports.extend(requests(file, &module, &mut prepared.unresolved_exports));
        modules.insert(module, file.path.as_str());
    }
    let reexported = imports
        .iter()
        .map(|request| {
            (
                (request.module.as_slice(), request.name.as_str()),
                (
                    request.target_module.as_slice(),
                    request.target_name.as_str(),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let anchors = prepared
        .anchors
        .values()
        .flat_map(BTreeMap::values)
        .collect::<BTreeSet<_>>();
    let mut resolved = BTreeMap::new();
    let mut exports = Vec::new();
    for request in &imports {
        let target = (
            request.target_module.as_slice(),
            request.target_name.as_str(),
        );
        let Some(defined) = resolve(target, &modules, &reexported, &mut resolved, prepared) else {
            prepared.unresolved_exports = true;
            continue;
        };
        exports.extend(export(input, request, defined, &anchors));
    }
    let conflicts = typescript::conflicting_export_identities(&exports);
    prepared.unresolved_exports |= !conflicts.is_empty();
    exports.retain(|export| !conflicts.contains(export.identity.as_str()));
    prepared.exports.extend(exports);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexport_cycles_stay_unresolved_and_reuse_the_recorded_targets() {
        let first = vec!["first".to_owned()];
        let second = vec!["second".to_owned()];
        let a = (first.as_slice(), "Reader");
        let b = (second.as_slice(), "Reader");
        let reexported = BTreeMap::from([(a, b), (b, a)]);
        let mut resolved = BTreeMap::new();
        let prepared = PreparedNamespace::default();
        let modules = BTreeMap::new();
        assert!(resolve(a, &modules, &reexported, &mut resolved, &prepared).is_none());
        assert_eq!(resolved.len(), 2);
        assert!(resolve(b, &modules, &reexported, &mut resolved, &prepared).is_none());
        assert_eq!(resolved.len(), 2);
    }
}
