//! Admits ECMAScript re-export declarations before graph normalization.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{ProjectPath, ReferenceRole, SymbolId};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_syntax::{DocumentPlacement, StaticBindingKind, SyntaxExportBinding, SyntaxExportKind};

use super::javascript::{ModuleMode, PackageModes, builtin_commonjs_name, js_binding};
use super::javascript_exports::{Exports, NameIndex, ResolvedExport, literal_contents};
use super::{File, StaticSource, Target, Work, admitted_facts};
use crate::NamespaceInput;
use crate::analyzer::namespace::typescript::commonjs;
use crate::analyzer::namespace::{ExportBinding, PreparedNamespace, SelectedFile};

const WORK_PER_DECLARATION: usize = 16;
/// The name an ECMAScript module gives its default export and a `CommonJS` module value.
const DEFAULT_NAME: &str = "default";

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
    let names = NameIndex::default();
    let resolver = Exports {
        files: &files,
        modes: &modes,
        names: Some(&names),
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
    // A declaration of any file, as in an ambient module, keeps its identity.
    let held = prepared
        .anchors
        .values()
        .flat_map(BTreeMap::values)
        .collect::<BTreeSet<_>>();
    let placed = additions.len();
    additions.retain(|alias: &ExportBinding| !held.contains(&alias.identity));
    prepared.unresolved_exports |= additions.len() < placed;
    prepared.exports.extend(additions);
    collect_commonjs_modules(input, &resolver, prepared, aliases_max, &mut work);
    prepared.unresolved_exports |= work.exhausted;
    let refused =
        crate::analyzer::namespace::typescript::conflicting_export_identities(&prepared.exports);
    prepared.unresolved_exports |= !refused.is_empty();
    prepared
        .exports
        .retain(|export| !refused.contains(export.identity.as_str()));
}

struct CommonJsModules<'input> {
    package_name: &'input str,
    physical_paths: BTreeSet<&'input ProjectPath>,
    module_owners: BTreeMap<SymbolId, Option<String>>,
    alias_ids: BTreeSet<SymbolId>,
}

impl CommonJsModules<'_> {
    fn collect_forward<'source>(
        &mut self,
        input: &NamespaceInput<'_>,
        resolver: &Exports<'_, 'source>,
        file: &File<'source>,
        binding: &SyntaxExportBinding,
        prepared: &mut PreparedNamespace,
        work: &mut Work,
    ) {
        let Some((exported, module)) = commonjs_forward(resolver, file, binding, work) else {
            return;
        };
        let Some(identity) = export_identity(file, exported, prepared)
            .filter(|identity| !self.alias_ids.contains(identity))
        else {
            prepared.unresolved_exports = true;
            return;
        };
        let Some(path) = commonjs::relative_target(file.document.path, module, |path| {
            self.physical_paths.contains(path)
        }) else {
            return;
        };
        let Some(imported) = resolver
            .imported(file, module, work)
            .filter(|imported| imported.document.path == &path)
            .filter(|imported| commonjs_file(resolver, imported, work))
        else {
            return;
        };
        let Some(target) = commonjs_value(resolver, imported, false, work) else {
            return;
        };
        let label = format!("{}/{path}", self.package_name);
        let Some(module_identity) = identity_for(
            input.owner(),
            imported.document.facts.language(),
            vec![label.clone()],
        ) else {
            return;
        };
        if self
            .module_owners
            .get(&module_identity)
            .is_some_and(|owner| owner.as_deref() != Some(path.as_str()))
        {
            prepared.unresolved_exports = true;
            return;
        }
        let selected = SelectedFile {
            path: imported.document.path,
            source: imported.document.source,
            syntax: imported.document.facts,
        };
        let already_placed = prepared.anchors.get(path.as_str()).is_some_and(|anchors| {
            anchors.get(".") == Some(&module_identity)
                && anchors.contains_key(&target.symbol.qualified_name)
        });
        if !already_placed
            && !commonjs::admit_file(
                input,
                &selected,
                &label,
                Some(&target.symbol.qualified_name),
                prepared,
            )
        {
            prepared.unresolved_exports = true;
            return;
        }
        let declaration = ResolvedExport::Declaration {
            target,
            member: None,
        };
        if let Some(export) =
            placed_target(file, binding, exported, identity, declaration, prepared)
        {
            self.module_owners
                .insert(module_identity, Some(path.as_str().to_owned()));
            self.alias_ids.insert(export.identity.clone());
            prepared.exports.push(export);
        }
    }
}

fn collect_commonjs_modules(
    input: &NamespaceInput<'_>,
    resolver: &Exports<'_, '_>,
    prepared: &mut PreparedNamespace,
    aliases_max: usize,
    work: &mut Work,
) {
    let SymbolOwner::Package { name, manager, .. } = input.owner() else {
        return;
    };
    if manager != "npm" || prepared.commonjs_modules.is_empty() {
        return;
    }
    let mut physical_paths = BTreeSet::new();
    for source in input.files().iter().chain(input.context_sources()) {
        if !work.charge() {
            return;
        }
        physical_paths.insert(source.path());
    }
    let root_files = std::mem::take(&mut prepared.commonjs_modules);
    let mut module_owners = BTreeMap::<SymbolId, Option<String>>::new();
    for (path, anchors) in &prepared.anchors {
        if !work.charge() {
            return;
        }
        if let Some(identity) = anchors.get(".") {
            module_owners
                .entry(identity.clone())
                .and_modify(|owner| *owner = None)
                .or_insert_with(|| Some(path.clone()));
        }
    }
    let mut alias_ids = BTreeSet::new();
    for export in &prepared.exports {
        if !work.charge() {
            return;
        }
        alias_ids.insert(export.identity.clone());
    }
    let mut modules = CommonJsModules {
        package_name: name,
        physical_paths,
        module_owners,
        alias_ids,
    };
    for source_path in root_files {
        if !work.charge() {
            return;
        }
        let Some(file) = resolver.files.get(source_path.as_str()) else {
            continue;
        };
        if !commonjs_file(resolver, file, work)
            || commonjs_value(resolver, file, true, work).is_none()
        {
            continue;
        }
        let Some(source) = module_id(file.document, prepared) else {
            continue;
        };
        if modules
            .module_owners
            .get(&source)
            .and_then(Option::as_deref)
            != Some(source_path.as_str())
        {
            continue;
        }
        for binding in file.document.facts.export_bindings().unwrap_or_default() {
            if !work.charge() || prepared.exports.len() >= aliases_max {
                prepared.unresolved_exports = true;
                return;
            }
            modules.collect_forward(input, resolver, file, binding, prepared, work);
        }
    }
}

fn commonjs_file<'source>(
    resolver: &Exports<'_, 'source>,
    file: &File<'source>,
    work: &mut Work,
) -> bool {
    file.document.facts.language().name == "javascript"
        && !file.facts.has_javascript_module_syntax()
        && resolver.modes.mode(file.document.path.as_str(), work) == ModuleMode::CommonJs
}

fn commonjs_value<'source>(
    resolver: &Exports<'_, 'source>,
    file: &File<'source>,
    synchronized: bool,
    work: &mut Work,
) -> Option<Target<'source>> {
    let witness = file.facts.commonjs_export()?;
    if witness.synchronized != synchronized {
        return None;
    }
    // Bindings are sorted by their assignment range. The witness points to the complete write.
    let bindings = file.document.facts.export_bindings()?;
    let index = bindings.partition_point(|binding| binding.range < witness.assignment);
    let export = bindings.get(index)?;
    if !work.charge()
        || export.range != witness.assignment
        || export.kind != SyntaxExportKind::Default
        || !file.javascript_export_is_stable(export)
    {
        return None;
    }
    let local = export.local?;
    let name = file.text(local)?;
    let binding = js_binding(
        file,
        resolver.modes,
        name,
        local.start,
        ReferenceRole::Read,
        work,
    )?;
    if binding.kind != StaticBindingKind::Local
        || witness.initialized.binary_search(&binding.name).is_err()
        || file.javascript_binding_reassigned(binding)
    {
        return None;
    }
    let target = file.target_at(binding.name)?;
    let kind = if synchronized { "function" } else { "variable" };
    if target.symbol.kind != kind
        || target.symbol.container.is_some()
        || (synchronized && target.symbol.body_range.is_none())
    {
        return None;
    }
    if synchronized {
        // A reference outside the wrapper could freeze or replace the function's export names.
        for reference in file.facts.references() {
            if !work.charge() || (reference.target != local && file.text(reference.target)? == name)
            {
                return None;
            }
        }
    }
    Some(target)
}

fn commonjs_forward<'source>(
    resolver: &Exports<'_, 'source>,
    file: &File<'source>,
    export: &SyntaxExportBinding,
    work: &mut Work,
) -> Option<(&'source str, &'source str)> {
    if export.kind != SyntaxExportKind::Named
        || export.source.is_some()
        || export.container.is_some()
        || file.declares_its_export(export)
        || !file.javascript_export_is_stable(export)
    {
        return None;
    }
    let local = export.local?;
    let exported = file.text(export.exported?)?;
    if matches!(
        exported,
        "__proto__" | "prototype" | "name" | "length" | "arguments" | "caller"
    ) {
        return None;
    }
    let binding = js_binding(
        file,
        resolver.modes,
        file.text(local)?,
        local.start,
        ReferenceRole::Read,
        work,
    )?;
    let StaticBindingKind::JavaScriptRequire {
        require,
        module,
        export: None,
    } = binding.kind
    else {
        return None;
    };
    (file
        .facts
        .commonjs_export()?
        .initialized
        .binary_search(&binding.name)
        .is_ok()
        && !file.javascript_binding_reassigned(binding)
        && builtin_commonjs_name(file, resolver.modes, "require", require.start, work))
    .then_some((exported, file.text(module)?))
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
        files.insert(
            document.path.as_str(),
            File::new(document, facts, None, work)?,
        );
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
    let forwarding = Forwarding {
        resolver,
        file,
        commonjs: commonjs_file(resolver, file, work),
    };
    let mut previous = None;
    for binding in file.document.facts.export_bindings().unwrap_or_default() {
        let next = additions.len();
        // Two modules of one condition can be the same file.
        let first = match previous {
            Some((range, first)) if forwarding.commonjs && range == binding.range => first,
            _ => next,
        };
        previous = Some((binding.range, first));
        for name in forwarding.names(binding, work).unwrap_or_default() {
            if !work.charge()
                || additions.len().saturating_add(prepared.exports.len()) >= aliases_max
            {
                prepared.unresolved_exports = true;
                return;
            }
            let Some(target) = forwarding
                .target(binding, name, work)
                .and_then(|target| placed(file, binding, name, target, prepared))
            else {
                continue;
            };
            if additions[first..next]
                .iter()
                .all(|alias| alias.identity != target.identity)
            {
                additions.push(target);
            }
        }
    }
}

/// What the export bindings of one file give to names of other files.
struct Forwarding<'resolver, 'index, 'source> {
    resolver: &'resolver Exports<'index, 'source>,
    file: &'resolver File<'source>,
    commonjs: bool,
}

impl<'source> Forwarding<'_, '_, 'source> {
    /// The module a binding writes, by the contents of its specifier.
    fn module(&self, binding: &SyntaxExportBinding) -> Option<&'source str> {
        literal_contents(self.file.text(binding.source?)?)
    }

    /// The names a binding can give to declarations of other files.
    fn names(&self, binding: &SyntaxExportBinding, work: &mut Work) -> Option<Vec<&'source str>> {
        if !work.charge() {
            return None;
        }
        let Self {
            resolver,
            file,
            commonjs,
        } = *self;
        if let Some(module) = self.module(binding) {
            let imported = resolver.imported(file, module, work)?;
            if binding.kind == SyntaxExportKind::All {
                let names = resolver.names(imported, work)?;
                let default = commonjs.then_some(DEFAULT_NAME);
                return Some(names.iter().copied().chain(default).collect());
            }
        } else if binding.source.is_some()
            || binding.container.is_some()
            || commonjs
            || binding
                .local
                .is_none_or(|local| file.target_at(local).is_some())
        {
            return None;
        }
        let name = match binding.kind {
            SyntaxExportKind::Default => DEFAULT_NAME,
            _ => file.text(binding.exported?)?,
        };
        Some(vec![name])
    }

    /// What one of those names reaches.
    fn target(
        &self,
        binding: &SyntaxExportBinding,
        name: &'source str,
        work: &mut Work,
    ) -> Option<ResolvedExport<'source>> {
        let Self {
            resolver,
            file,
            commonjs,
        } = *self;
        if commonjs {
            return self
                .required(binding, name, work)
                .filter(|target| target.key().0 != file.document.path);
        }
        let Some(module) = self.module(binding) else {
            // `import { name } from` with `export { name }` gives what `export { name } from`
            // gives.
            return own_export(resolver, file, name, work)
                .filter(|target| target.key().0 != file.document.path);
        };
        let imported = resolver.imported(file, module, work)?;
        if binding.kind == SyntaxExportKind::Namespace {
            return Some(ResolvedExport::Module(imported.document));
        }
        let local = if binding.kind == SyntaxExportKind::All {
            name
        } else {
            file.text(binding.local?)?
        };
        let value = own_export(resolver, imported, local, work)?;
        if binding.kind == SyntaxExportKind::All
            && own_export(resolver, file, name, work)?.key() != value.key()
        {
            return None;
        }
        Some(value)
    }

    /// What a `CommonJS` file gives a name through a module it requires.
    fn required(
        &self,
        binding: &SyntaxExportBinding,
        name: &'source str,
        work: &mut Work,
    ) -> Option<ResolvedExport<'source>> {
        let Self { resolver, file, .. } = *self;
        let target = if name == DEFAULT_NAME {
            resolver.resolve_value(file, ReferenceRole::Call, work)
        } else {
            resolver.resolve_file(file, name, ModuleMode::CommonJs, ReferenceRole::Call, work)
        };
        let member = || {
            // A member of an object literal is no export of its own.
            resolver
                .literal_member(file, (name != DEFAULT_NAME).then_some(name), work)
                .map(|target| ResolvedExport::Declaration {
                    target,
                    member: None,
                })
        };
        target
            .or_else(member)
            .filter(|target| matches!(target, ResolvedExport::Declaration { .. }))
            .filter(|_| binding.kind != SyntaxExportKind::Named)
    }
}

/// What the file gives a name, as a type before a value.
fn own_export<'source>(
    resolver: &Exports<'_, 'source>,
    file: &File<'source>,
    name: &'source str,
    work: &mut Work,
) -> Option<ResolvedExport<'source>> {
    resolver
        .resolve_file(file, name, ModuleMode::Esm, ReferenceRole::Type, work)
        .or_else(|| resolver.resolve_file(file, name, ModuleMode::Esm, ReferenceRole::Read, work))
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
    let identity = export_identity(file, name, prepared)?;
    placed_target(file, binding, name, identity, resolved, prepared)
}

fn export_identity(file: &File<'_>, name: &str, prepared: &PreparedNamespace) -> Option<SymbolId> {
    if !rift_core::is_portable_name(name) {
        return None;
    }
    let source = module_id(file.document, prepared)?;
    let source = SymbolIdentity::parse(source.as_str()).ok()?;
    let mut path = source.qualified_path().to_vec();
    path.push(name.to_owned());
    let identity = identity_for(source.owner(), source.language(), path)?;
    if prepared
        .anchors
        .get(file.document.path.as_str())?
        .values()
        .any(|own| *own == identity)
    {
        return None;
    }
    Some(identity)
}

fn identity_for(
    owner: &SymbolOwner,
    language: &rift_protocol::read::Language,
    path: Vec<String>,
) -> Option<SymbolId> {
    let identity = SymbolIdentity::new(owner.clone(), language.clone(), path).ok()?;
    SymbolId::new(identity.wire_identity()).ok()
}

fn placed_target(
    file: &File<'_>,
    binding: &SyntaxExportBinding,
    name: &str,
    identity: SymbolId,
    resolved: ResolvedExport<'_>,
    prepared: &PreparedNamespace,
) -> Option<ExportBinding> {
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
