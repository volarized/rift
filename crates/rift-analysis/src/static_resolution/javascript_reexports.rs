//! Admits ECMAScript re-export declarations before graph normalization.

use std::collections::BTreeMap;

use rift_core::{ReferenceRole, SymbolId};
use rift_protocol::identity::SymbolIdentity;
use rift_syntax::{DocumentPlacement, SyntaxExportBinding, SyntaxExportKind};

use super::javascript::{ModuleMode, PackageModes};
use super::javascript_exports::{Exports, ResolvedExport, literal_contents};
use super::{File, StaticSource, Work, admitted_facts};
use crate::NamespaceInput;
use crate::analyzer::namespace::{ExportBinding, PreparedNamespace, SelectedFile};

const WORK_PER_DECLARATION: usize = 16;

pub(super) fn prepare(
    input: &NamespaceInput<'_>,
    prepared: &mut PreparedNamespace,
    selected: &[SelectedFile<'_>],
    placements: &[&DocumentPlacement],
    aliases_max: usize,
) {
    if !selected.iter().any(|file| {
        matches!(
            file.syntax.language().name.as_str(),
            "javascript" | "typescript"
        )
    }) {
        return;
    }
    let mut work = Work::new(aliases_max.saturating_mul(WORK_PER_DECLARATION));
    let documents = selected
        .iter()
        .enumerate()
        .map(|(index, file)| {
            Some(StaticSource {
                path: file.path,
                source: file.source,
                facts: file.syntax,
                placement: *placements.get(index)?,
            })
        })
        .collect::<Option<Vec<_>>>();
    let Some(documents) = documents else {
        prepared.unresolved_exports = true;
        return;
    };
    let Some(files) = files(&documents, &mut work) else {
        prepared.unresolved_exports = true;
        return;
    };
    let Some(modes) = PackageModes::new(input, &BTreeMap::new(), &mut work) else {
        prepared.unresolved_exports = true;
        return;
    };
    let resolver = Exports {
        files: &files,
        modes: &modes,
    };
    let mut additions = Vec::new();
    for file in files.values() {
        if !work.charge() {
            break;
        }
        collect(
            &resolver,
            file,
            prepared,
            &mut additions,
            aliases_max,
            &mut work,
        );
    }
    prepared.unresolved_exports |= work.exhausted;
    prepared.exports.extend(additions);
    let refused =
        crate::analyzer::namespace::typescript::conflicting_export_identities(&prepared.exports);
    prepared.unresolved_exports |= !refused.is_empty();
    prepared
        .exports
        .retain(|export| !refused.contains(export.identity.as_str()));
}

fn files<'source>(
    documents: &'source [StaticSource<'source>],
    work: &mut Work,
) -> Option<BTreeMap<&'source str, File<'source>>> {
    let mut files = BTreeMap::new();
    for document in documents.iter().filter(|document| {
        matches!(
            document.facts.language().name.as_str(),
            "javascript" | "typescript"
        )
    }) {
        if !work.charge() {
            return None;
        }
        let Some(facts) = admitted_facts(document) else {
            continue;
        };
        files.insert(document.path.as_str(), File::new(document, facts, work)?);
    }
    Some(files)
}

fn collect<'source>(
    resolver: &Exports<'_, 'source>,
    file: &File<'source>,
    prepared: &mut PreparedNamespace,
    additions: &mut Vec<ExportBinding>,
    aliases_max: usize,
    work: &mut Work,
) {
    for binding in file.document.facts.export_bindings().unwrap_or_default() {
        if !work.charge() || additions.len().saturating_add(prepared.exports.len()) >= aliases_max {
            prepared.unresolved_exports = true;
            return;
        }
        collect_binding(
            resolver,
            file,
            binding,
            prepared,
            additions,
            aliases_max,
            work,
        );
    }
}

fn collect_binding<'source>(
    resolver: &Exports<'_, 'source>,
    file: &File<'source>,
    binding: &SyntaxExportBinding,
    prepared: &mut PreparedNamespace,
    additions: &mut Vec<ExportBinding>,
    aliases_max: usize,
    work: &mut Work,
) {
    let Some(module) = binding
        .source
        .and_then(|range| file.text(range))
        .and_then(literal_contents)
    else {
        return;
    };
    let Some(imported) = resolver.imported(file, module, work) else {
        return;
    };
    let names = match binding.kind {
        SyntaxExportKind::All => resolver
            .names(imported, work)
            .map(|names| names.into_iter().collect::<Vec<_>>()),
        _ => binding
            .exported
            .and_then(|range| file.text(range))
            .map(|name| vec![name]),
    };
    for name in names.unwrap_or_default() {
        if !work.charge() || additions.len().saturating_add(prepared.exports.len()) >= aliases_max {
            prepared.unresolved_exports = true;
            return;
        }
        let target = target(resolver, file, imported, binding, name, work);
        if let Some(target) =
            target.and_then(|target| placed(file, binding, name, target, prepared))
        {
            additions.push(target);
        }
    }
}

fn target<'source>(
    resolver: &Exports<'_, 'source>,
    file: &File<'source>,
    imported: &File<'source>,
    binding: &SyntaxExportBinding,
    name: &'source str,
    work: &mut Work,
) -> Option<ResolvedExport<'source>> {
    if binding.kind == SyntaxExportKind::Namespace {
        return Some(ResolvedExport::Module(imported.document));
    }
    let local = if binding.kind == SyntaxExportKind::All {
        name
    } else {
        file.text(binding.local?)?
    };
    let value = resolver
        .resolve_file(imported, local, ModuleMode::Esm, ReferenceRole::Type, work)
        .or_else(|| {
            resolver.resolve_file(imported, local, ModuleMode::Esm, ReferenceRole::Read, work)
        })?;
    if binding.kind == SyntaxExportKind::All {
        let winner = resolver
            .resolve_file(file, name, ModuleMode::Esm, ReferenceRole::Type, work)
            .or_else(|| {
                resolver.resolve_file(file, name, ModuleMode::Esm, ReferenceRole::Read, work)
            })?;
        if winner.key() != value.key() {
            return None;
        }
    }
    Some(value)
}

fn module_id(file: &StaticSource<'_>, prepared: &PreparedNamespace) -> Option<SymbolId> {
    prepared.anchors.get(file.path.as_str())?.get(".").cloned()
}

fn placed(
    file: &File<'_>,
    binding: &SyntaxExportBinding,
    name: &str,
    resolved: ResolvedExport<'_>,
    prepared: &PreparedNamespace,
) -> Option<ExportBinding> {
    if !rift_core::is_portable_name(name) {
        return None;
    }
    let source = module_id(file.document, prepared)?;
    let source = SymbolIdentity::parse(source.as_str()).ok()?;
    let mut path = source.qualified_path().to_vec();
    path.push(name.to_owned());
    let identity =
        SymbolIdentity::new(source.owner().clone(), source.language().clone(), path).ok()?;
    let identity = SymbolId::new(identity.wire_identity()).ok()?;
    if prepared
        .anchors
        .get(file.document.path.as_str())?
        .values()
        .any(|own| *own == identity)
    {
        return None;
    }
    let (target_file, target, target_qualified_name) = match resolved {
        ResolvedExport::Declaration {
            target,
            member: None,
        } => (
            target.file,
            prepared
                .anchors
                .get(target.file.path.as_str())?
                .get(&target.symbol.qualified_name)?
                .clone(),
            target.symbol.qualified_name.clone(),
        ),
        ResolvedExport::Module(target) => (target, module_id(target, prepared)?, ".".to_owned()),
        ResolvedExport::Declaration { .. } => return None,
    };
    Some(ExportBinding {
        path: file.document.path.as_str().to_owned(),
        binding: binding.clone(),
        identity,
        target,
        target_path: target_file.path.as_str().to_owned(),
        target_qualified_name,
        name: name.to_owned(),
        qualified_name: name.to_owned(),
    })
}
