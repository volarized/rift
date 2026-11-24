//! Resolves captured ECMAScript exports without a normalized graph.

use super::javascript::{ModuleMode, PackageModes, js_binding};
use super::{File, Target, Work};
use rift_syntax::{StaticBindingKind, SyntaxExportKind};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ExportSlot<'source> {
    Default,
    Named(&'source str),
    Unknown,
}

#[derive(Clone, Copy)]
pub(super) enum ResolvedExport<'source> {
    Declaration {
        target: Target<'source>,
        member: Option<&'source str>,
    },
    Module(&'source super::StaticSource<'source>),
}

impl ResolvedExport<'_> {
    pub(super) fn key(&self) -> (&rift_core::ProjectPath, Option<&str>, Option<&str>) {
        match self {
            Self::Declaration { target, member } => (
                target.file.path,
                Some(&target.symbol.qualified_name),
                *member,
            ),
            Self::Module(file) => (file.path, None, None),
        }
    }
}

pub(super) struct Exports<'index, 'source> {
    pub(super) files: &'index BTreeMap<&'source str, File<'source>>,
    pub(super) modes: &'index PackageModes<'source>,
}

impl<'source> Exports<'_, 'source> {
    pub(super) fn imported(
        &self,
        from: &File<'source>,
        module: &str,
        work: &mut Work,
    ) -> Option<&File<'source>> {
        let path = relative_path(from.document.path.as_str(), module, work)?;
        module_file(self.files, &path, false)
    }

    fn imported_for_role(
        &self,
        from: &File<'source>,
        module: &str,
        mode: ModuleMode,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<&File<'source>> {
        let path = relative_path(from.document.path.as_str(), module, work)?;
        // A declaration file's type resolution can reach another declaration file.
        // A runtime lookup must still name a source it can execute.
        let declarations = mode == ModuleMode::Esm
            && role == rift_core::ReferenceRole::Type
            && from.document.path.as_str().ends_with(".d.ts");
        module_file(self.files, &path, declarations)
    }

    pub(super) fn resolve(
        &self,
        from: &File<'source>,
        module: &str,
        name: &'source str,
        mode: ModuleMode,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<ResolvedExport<'source>> {
        self.resolve_file(
            self.imported_for_role(from, module, mode, role, work)?,
            name,
            mode,
            role,
            work,
        )
    }

    pub(super) fn resolve_file(
        &self,
        file: &File<'source>,
        name: &'source str,
        mode: ModuleMode,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<ResolvedExport<'source>> {
        self.resolve_slot(file, ExportSlot::Named(name), mode, role, work)
    }

    pub(super) fn resolve_module_value(
        &self,
        from: &File<'source>,
        module: &str,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<ResolvedExport<'source>> {
        self.resolve_slot(
            self.imported(from, module, work)?,
            ExportSlot::Default,
            ModuleMode::CommonJs,
            role,
            work,
        )
    }

    fn resolve_slot(
        &self,
        file: &File<'source>,
        slot: ExportSlot<'source>,
        mode: ModuleMode,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<ResolvedExport<'source>> {
        let (name, member) = match slot {
            ExportSlot::Named(name) => {
                let (head, tail) = split_name(name);
                (ExportSlot::Named(head), tail)
            }
            ExportSlot::Default => (ExportSlot::Default, None),
            ExportSlot::Unknown => return None,
        };
        let mut pending = vec![(file, name, member)];
        let mut visited = std::collections::BTreeSet::new();
        let mut found: Option<ResolvedExport<'source>> = None;
        while let Some((file, name, member)) = pending.pop() {
            if !work.charge() {
                return None;
            }
            if !visited.insert((file.document.path.as_str(), name, member)) {
                continue;
            }
            if !self.accepts(file, mode, work) {
                continue;
            }
            let (direct, stars) = self.export_candidates(file, name, mode, role, work)?;
            let Some(export) = direct else {
                let default = matches!(name, ExportSlot::Default | ExportSlot::Named("default"));
                pending.extend(stars.into_iter().filter_map(|star| {
                    // Only `module.exports = require(..)` passes the module value on.
                    if default && !star.replaces_module {
                        return None;
                    }
                    Some((
                        self.imported_for_role(file, star.module, mode, role, work)?,
                        name,
                        member,
                    ))
                }));
                continue;
            };
            let export = self.local_export(file, export, role, work)?;
            let candidate = match export {
                Export::Declaration(range) => Some(ResolvedExport::Declaration {
                    target: file.target_at(range)?,
                    member,
                }),
                Export::Reexport { module, name } => {
                    if let Some(imported) = self.imported_for_role(file, module, mode, role, work) {
                        pending.push((imported, ExportSlot::Named(name), member));
                    }
                    None
                }
                Export::Module { module } => {
                    let imported = self.imported_for_role(file, module, mode, role, work)?;
                    if let Some(member) = member {
                        let (name, tail) = split_name(member);
                        pending.push((imported, ExportSlot::Named(name), tail));
                        None
                    } else {
                        Some(ResolvedExport::Module(imported.document))
                    }
                }
                Export::Local { .. } => None,
            };
            if let Some(candidate) = candidate {
                if found.is_some_and(|previous| previous.key() != candidate.key()) {
                    return None;
                }
                found = Some(candidate);
            }
        }
        found
    }

    fn accepts(&self, file: &File<'source>, mode: ModuleMode, work: &mut Work) -> bool {
        let actual = self.modes.mode(file.document.path.as_str(), work);
        match mode {
            ModuleMode::Esm => actual != ModuleMode::CommonJs,
            ModuleMode::CommonJs => {
                actual == ModuleMode::CommonJs
                    || super::javascript::typescript_commonjs(file.document.path.as_str())
            }
            ModuleMode::Unknown | ModuleMode::NodeDefault => false,
        }
    }

    fn local_export(
        &self,
        file: &File<'source>,
        export: Export<'source>,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Export<'source>> {
        let Export::Local { name, position } = export else {
            return Some(export);
        };
        // `get: function () { return module.name }` exports a member of a required module.
        let (name, member) = split_name(name);
        let binding = js_binding(file, self.modes, name, position, role, work)?;
        if let Some(member) = member {
            return self.required_member(file, binding, member, work);
        }
        match binding.kind {
            StaticBindingKind::Local => Some(Export::Declaration(binding.name)),
            StaticBindingKind::JavaScriptImport { module, export } => Some(Export::Reexport {
                module: file.text(module)?,
                name: export
                    .and_then(|range| file.text(range))
                    .unwrap_or("default"),
            }),
            StaticBindingKind::JavaScriptEdgeNamespace { module } => Some(Export::Module {
                module: file.text(module)?,
            }),
            // A required name exported again is a reexport of that module's export.
            StaticBindingKind::JavaScriptRequire {
                require,
                module,
                export,
            } if super::javascript::builtin_commonjs_name(
                file,
                self.modes,
                "require",
                require.start,
                work,
            ) =>
            {
                let module = file.text(module)?;
                Some(match export {
                    Some(export) => Export::Reexport {
                        module,
                        name: file.text(export)?,
                    },
                    None => Export::Module { module },
                })
            }
            _ => None,
        }
    }

    fn required_member(
        &self,
        file: &File<'source>,
        binding: &rift_syntax::StaticBinding,
        member: &'source str,
        work: &mut Work,
    ) -> Option<Export<'source>> {
        let StaticBindingKind::JavaScriptRequire {
            require,
            module,
            export: None,
        } = binding.kind
        else {
            return None;
        };
        (!file.javascript_binding_reassigned(binding)
            && super::javascript::builtin_commonjs_name(
                file,
                self.modes,
                "require",
                require.start,
                work,
            ))
        .then_some(())?;
        Some(Export::Reexport {
            module: file.text(module)?,
            name: member,
        })
    }

    /// Whether every computed key written on the file's exports is a symbol value: such a
    /// key replaces no named export. A key whose value is not established replaces any.
    fn export_keys_are_symbols(&self, file: &File<'source>, work: &mut Work) -> bool {
        file.export_keys
            .iter()
            .all(|&(name, position)| self.is_symbol_value(file, name, position, work))
    }

    /// Whether `name` is bound to a `const` whose initializer is a `Symbol(..)` value, in
    /// this file or, through `require`, in another file of the package.
    fn is_symbol_value(
        &self,
        file: &File<'source>,
        name: &str,
        position: u64,
        work: &mut Work,
    ) -> bool {
        if !work.charge() {
            return false;
        }
        let role = rift_core::ReferenceRole::Read;
        // The write runs after the key's declaration or fails: source order decides nothing.
        let super::Binding::Found(binding) = file.binding(name, position, |_| true, work) else {
            return false;
        };
        if file.javascript_binding_reassigned(binding) {
            return false;
        }
        match binding.kind {
            StaticBindingKind::Local => file
                .target_at(binding.name)
                .is_some_and(|target| file.is_symbol_declaration(target)),
            StaticBindingKind::JavaScriptRequire {
                require,
                module,
                export: Some(export),
            } => {
                super::javascript::builtin_commonjs_name(
                    file,
                    self.modes,
                    "require",
                    require.start,
                    work,
                ) && file
                    .text(module)
                    .zip(file.text(export))
                    .is_some_and(|(module, export)| {
                        self.exports_symbol(file, module, export, role, work)
                    })
            }
            _ => false,
        }
    }

    /// Whether the required module exports `name` as a symbol value: an exported `const`,
    /// or a property of the object literal the module exports.
    fn exports_symbol(
        &self,
        from: &File<'source>,
        module: &str,
        name: &'source str,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> bool {
        let Some(supplied) = self.imported(from, module, work) else {
            return false;
        };
        if !supplied.named_export_is_written_once(name) {
            return false;
        }
        if let Some(resolved) = self.resolve_file(supplied, name, ModuleMode::CommonJs, role, work)
        {
            let ResolvedExport::Declaration {
                target,
                member: None,
            } = resolved
            else {
                return false;
            };
            return self
                .files
                .get(target.file.path.as_str())
                .is_some_and(|file| file.is_symbol_declaration(target));
        }
        let value = self.resolve_slot(
            supplied,
            ExportSlot::Default,
            ModuleMode::CommonJs,
            role,
            work,
        );
        let owner = match value {
            Some(ResolvedExport::Declaration {
                target,
                member: None,
            }) => Some(target),
            Some(_) => return false,
            None => supplied
                .facts
                .commonjs_export()
                .and_then(|export| export.literal)
                .and_then(|_| supplied.qualified_target("default")),
        };
        let Some(owner) = owner else {
            return false;
        };
        let Some(file) = self.files.get(owner.file.path.as_str()) else {
            return false;
        };
        let property = file.qualified_target(&format!("{}.{}", owner.symbol.qualified_name, name));
        property.is_some_and(|property| file.is_symbol_property(owner, property))
    }

    fn export_candidates(
        &self,
        file: &File<'source>,
        name: ExportSlot<'source>,
        mode: ModuleMode,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<ExportCandidates<'source>> {
        let mut found: Option<Export<'source>> = None;
        let mut stars = Vec::new();
        let mut keys_are_symbols = None;
        for binding in file.exports.as_ref()?.candidates(name) {
            if !work.charge() {
                return None;
            }
            let written = file.text(binding.range)?;
            if !matches_export_form(written, mode)
                || binding.container.is_some()
                || !file.javascript_export_role(binding, role)
                || file.exported_pair(binding).is_some()
            {
                continue;
            }
            if binding.kind == SyntaxExportKind::All {
                stars.push(ExportStar {
                    module: literal_contents(file.text(binding.source?)?)?,
                    replaces_module: written.starts_with("module.exports"),
                });
                continue;
            }
            let exported = match binding.kind {
                SyntaxExportKind::Named | SyntaxExportKind::Namespace => binding
                    .exported
                    .and_then(|range| file.text(range))
                    .map(ExportSlot::Named),
                SyntaxExportKind::Default if mode == ModuleMode::CommonJs => {
                    Some(ExportSlot::Default)
                }
                SyntaxExportKind::Default => Some(ExportSlot::Named("default")),
                SyntaxExportKind::All => None,
            };
            if exported != Some(name) {
                continue;
            }
            if mode == ModuleMode::CommonJs && role == rift_core::ReferenceRole::Call {
                let symbols = match keys_are_symbols {
                    Some(symbols) => symbols,
                    None => *keys_are_symbols.insert(self.export_keys_are_symbols(file, work)),
                };
                if !symbols
                    || !file.javascript_written_export_is_stable(binding)
                    || !file.javascript_alias_write_is_stable(binding, work)
                {
                    return None;
                }
            }
            let target = file.javascript_export_candidate(binding, role, work)?;
            found = Some(match found {
                Some(previous) => previous.merge(target, file)?,
                None => target,
            });
        }
        Some((found, stars))
    }

    fn collect_names<'index>(
        &'index self,
        file: &File<'source>,
        names: &mut std::collections::BTreeSet<&'source str>,
        pending: &mut Vec<&'index File<'source>>,
        work: &mut Work,
    ) -> Option<()> {
        for binding in file.document.facts.export_bindings().unwrap_or_default() {
            if !work.charge() {
                return None;
            }
            if binding.container.is_some() {
                continue;
            }
            if binding.kind == SyntaxExportKind::All {
                if let Some(imported) =
                    self.imported(file, literal_contents(file.text(binding.source?)?)?, work)
                {
                    pending.push(imported);
                }
            } else if let Some(name) = binding
                .exported
                .and_then(|range| file.text(range))
                .filter(|name| *name != "default")
            {
                names.insert(name);
            }
        }
        Some(())
    }

    pub(super) fn names(
        &self,
        file: &File<'source>,
        work: &mut Work,
    ) -> Option<std::collections::BTreeSet<&'source str>> {
        let mut pending = vec![file];
        let mut visited = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        while let Some(file) = pending.pop() {
            if !work.charge() {
                return None;
            }
            if !visited.insert(file.document.path.as_str()) {
                continue;
            }
            self.collect_names(file, &mut names, &mut pending, work)?;
        }
        Some(names)
    }
}

fn split_name(name: &str) -> (&str, Option<&str>) {
    name.split_once('.')
        .map_or((name.trim(), None), |(head, tail)| {
            (head.trim(), Some(tail.trim()))
        })
}

pub(super) fn module_file<'index, 'source>(
    files: &'index BTreeMap<&'source str, File<'source>>,
    path: &str,
    declarations: bool,
) -> Option<&'index File<'source>> {
    if let Some(directory) = path.strip_suffix('/') {
        // A directory specifier names only the directory's index module.
        let mut found = None;
        for name in ["index.ts", "index.tsx", "index.js"] {
            let candidate = if directory.is_empty() {
                name.to_owned()
            } else {
                format!("{directory}/{name}")
            };
            if let Some(file) = files.get(candidate.as_str())
                && found.replace(file).is_some()
            {
                return None;
            }
        }
        return found.or_else(|| {
            declarations
                .then(|| files.get(format!("{path}index.d.ts").as_str()))
                .flatten()
        });
    }
    if let Some(file) = files.get(path).filter(|file| {
        matches!(
            file.document.facts.language().name.as_str(),
            "javascript" | "typescript"
        )
    }) {
        return Some(file);
    }
    let stem = path.strip_suffix(".js").unwrap_or(path);
    if let Some(stem) = path.strip_suffix(".mjs") {
        return files.get(format!("{stem}.mts").as_str());
    }
    if let Some(stem) = path.strip_suffix(".cjs") {
        return files.get(format!("{stem}.cts").as_str());
    }
    let mut found = None;
    for candidate in [
        format!("{stem}.ts"),
        format!("{stem}.tsx"),
        format!("{stem}.mts"),
        format!("{stem}.cts"),
        format!("{stem}.js"),
        format!("{stem}.mjs"),
        format!("{stem}.cjs"),
        format!("{stem}/index.ts"),
        format!("{stem}/index.tsx"),
        format!("{stem}/index.js"),
    ] {
        if let Some(file) = files.get(candidate.as_str())
            && found.replace(file).is_some()
        {
            return None;
        }
    }
    if found.is_some() || !declarations || std::path::Path::new(path).extension().is_some() {
        return found;
    }
    let direct = files.get(format!("{path}.d.ts").as_str());
    let index = files.get(format!("{path}/index.d.ts").as_str());
    match (direct, index) {
        (Some(file), None) | (None, Some(file)) => Some(file),
        _ => None,
    }
}

enum Export<'source> {
    Local {
        name: &'source str,
        position: u64,
    },
    Declaration(rift_syntax::ByteRange),
    Reexport {
        module: &'source str,
        name: &'source str,
    },
    Module {
        module: &'source str,
    },
}

/// A file's export bindings, read once: a lookup charges only the bindings that can answer.
pub(super) struct ExportBindings<'source> {
    named: BTreeMap<&'source str, Vec<&'source rift_syntax::SyntaxExportBinding>>,
    defaults: Vec<&'source rift_syntax::SyntaxExportBinding>,
    stars: Vec<&'source rift_syntax::SyntaxExportBinding>,
}

impl<'source> ExportBindings<'source> {
    fn read(
        file: &File<'source>,
        bindings: &'source [rift_syntax::SyntaxExportBinding],
        work: &mut Work,
    ) -> Option<Self> {
        let mut read = Self {
            named: BTreeMap::new(),
            defaults: Vec::new(),
            stars: Vec::new(),
        };
        for binding in bindings {
            if !work.charge() {
                return None;
            }
            match binding.kind {
                SyntaxExportKind::All => read.stars.push(binding),
                SyntaxExportKind::Default => read.defaults.push(binding),
                SyntaxExportKind::Named | SyntaxExportKind::Namespace => {
                    if let Some(name) = binding.exported.and_then(|range| file.text(range)) {
                        read.named.entry(name).or_default().push(binding);
                    }
                }
            }
        }
        Some(read)
    }

    fn candidates(
        &self,
        slot: ExportSlot<'source>,
    ) -> impl Iterator<Item = &'source rift_syntax::SyntaxExportBinding> + '_ {
        let (name, default) = match slot {
            ExportSlot::Named(name) => (Some(name), name == "default"),
            ExportSlot::Default => (None, true),
            ExportSlot::Unknown => (None, false),
        };
        name.and_then(|name| self.named.get(name))
            .into_iter()
            .flatten()
            .chain(self.defaults.iter().filter(move |_| default))
            .chain(&self.stars)
            .copied()
    }
}

/// A whole-module export: `export * from`, `module.exports = require(..)` or a copy of
/// another module's exports onto `exports`.
struct ExportStar<'source> {
    module: &'source str,
    replaces_module: bool,
}

type ExportCandidates<'source> = (Option<Export<'source>>, Vec<ExportStar<'source>>);

impl<'source> Export<'source> {
    fn merge(self, incoming: Self, file: &File<'source>) -> Option<Self> {
        if self.key(file)? == incoming.key(file)? {
            return Some(incoming);
        }
        let (Self::Declaration(previous), Self::Declaration(next)) = (&self, &incoming) else {
            return None;
        };
        let previous = file.target_at(*previous)?.symbol;
        let next = file.target_at(*next)?.symbol;
        if previous.qualified_name != next.qualified_name
            || [previous, next].iter().any(|symbol| {
                symbol.kind != "function"
                    || symbol.syntax_element_kind == Some("assignment_expression")
            })
            || (previous.body_range.is_some() && next.body_range.is_some())
        {
            return None;
        }
        // Retain the implementation so any later second body remains ambiguous.
        Some(if previous.body_range.is_some() {
            self
        } else {
            incoming
        })
    }

    fn key(
        &self,
        file: &File<'source>,
    ) -> Option<(
        Option<&'source str>,
        &'source str,
        Option<rift_syntax::ByteRange>,
    )> {
        match self {
            Self::Local { name, .. } => Some((None, name, None)),
            Self::Declaration(range) => Some((
                None,
                file.target_at(*range)?.symbol.qualified_name.as_str(),
                Some(*range),
            )),
            Self::Reexport { module, name } => Some((Some(module), name, None)),
            Self::Module { module } => Some((Some(module), "*", None)),
        }
    }
}

impl<'source> File<'source> {
    pub(super) fn index_javascript_members(&mut self, work: &mut Work) -> Option<()> {
        self.exports = match self.document.facts.export_bindings() {
            Some(bindings) => Some(ExportBindings::read(self, bindings, work)?),
            None => None,
        };
        if self.facts.members().is_empty() {
            return Some(());
        }
        let mut declared = None;
        for member in self.facts.members() {
            if !work.charge() {
                return None;
            }
            self.members
                .entry(member.range)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(member));
        }
        for &range in self.facts.assignments() {
            if !work.charge() {
                return None;
            }
            if self.text(range)?.contains('\\') {
                self.unknown_member_writes.insert(None);
            }
            let Some(member) = self.members.get(&range).copied().flatten() else {
                continue;
            };
            self.index_javascript_object_write(member, work);
            self.index_exported_object_write(member, &mut declared, work)?;
            let Some((receiver, slot)) = self.javascript_export_slot(member) else {
                continue;
            };
            if matches!(
                self.binding(self.text(receiver)?, receiver.start, |_| true, work),
                super::Binding::Missing
            ) {
                // A key bound to a symbol value names no export (D41); the exports that
                // supply the key decide it.
                if slot == ExportSlot::Unknown
                    && let Some(key) = self.javascript_export_key(member)
                {
                    self.export_keys.push((key, member.range.start));
                    continue;
                }
                self.export_writes
                    .entry(slot)
                    .and_modify(|start| {
                        if *start != Some(range.start) {
                            *start = None;
                        }
                    })
                    .or_insert(Some(range.start));
            }
        }
        Some(())
    }

    /// Records a write to a member of the value an export assignment declares:
    /// `module.exports.helpers.add = value`. `declared` holds the exports the file declares
    /// this way, read once.
    fn index_exported_object_write(
        &mut self,
        member: &rift_syntax::StaticMember,
        declared: &mut Option<BTreeMap<&'source str, Option<rift_syntax::ByteRange>>>,
        work: &mut Work,
    ) -> Option<()> {
        let Some(name) = self.text(member.receiver).and_then(|receiver| {
            receiver
                .strip_prefix("module.exports.")
                .or_else(|| receiver.strip_prefix("exports."))
                .filter(|name| !name.contains(['.', '[', '(']))
        }) else {
            return Some(());
        };
        let declared = match declared {
            Some(declared) => declared,
            None => declared.insert(self.declared_exports(work)?),
        };
        // An export declared twice is stable for no call.
        if let Some(Some(declaration)) = declared.get(name) {
            let written = member.member.and_then(|range| self.text(range));
            self.member_writes
                .entry((*declaration, written))
                .and_modify(|position| *position = None)
                .or_insert(Some(member.range.start));
        }
        Some(())
    }

    /// Each name an export assignment declares, with its one declaration.
    fn declared_exports(
        &self,
        work: &mut Work,
    ) -> Option<BTreeMap<&'source str, Option<rift_syntax::ByteRange>>> {
        let mut declared = BTreeMap::new();
        for binding in self.document.facts.export_bindings().unwrap_or_default() {
            if !work.charge() {
                return None;
            }
            let Some(exported) = binding
                .exported
                .filter(|&exported| binding.local == Some(exported))
            else {
                continue;
            };
            if let Some(name) = self.text(exported) {
                declared
                    .entry(name)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(exported));
            }
        }
        Some(declared)
    }

    fn javascript_export_slot(
        &self,
        member: &rift_syntax::StaticMember,
    ) -> Option<(rift_syntax::ByteRange, ExportSlot<'source>)> {
        let receiver = self.text(member.receiver)?;
        let name = member.member.and_then(|range| self.text(range));
        let slot = match receiver {
            "exports" => name.map_or(ExportSlot::Unknown, ExportSlot::Named),
            "module" => match name {
                Some("exports") => ExportSlot::Default,
                None => ExportSlot::Unknown,
                _ => return None,
            },
            _ => return self.javascript_nested_export_slot(member),
        };
        Some((member.receiver, slot))
    }

    /// The name written as the key of `exports[key]` or `module.exports[key]`.
    fn javascript_export_key(&self, member: &rift_syntax::StaticMember) -> Option<&'source str> {
        if member.member.is_some()
            || !matches!(
                self.text(member.receiver),
                Some("exports" | "module.exports")
            )
        {
            return None;
        }
        self.text(rift_syntax::ByteRange {
            start: member.receiver.end,
            end: member.range.end,
        })
        .map(str::trim)
        .and_then(|key| key.strip_prefix('['))
        .and_then(|key| key.strip_suffix(']'))
        .map(str::trim)
        .filter(|key| {
            !key.is_empty()
                && key
                    .chars()
                    .all(|character| character.is_alphanumeric() || matches!(character, '_' | '$'))
        })
    }

    /// Whether `target` is a `const` declaring one name with a `Symbol(..)` value.
    fn is_symbol_declaration(&self, target: Target<'source>) -> bool {
        target.symbol.kind == "variable"
            && self
                .text(rift_syntax::ByteRange {
                    start: 0,
                    end: target.symbol.item_range.start,
                })
                .is_some_and(|before| before.trim_end().ends_with("const"))
            && self.is_symbol_initializer(target)
    }

    fn is_symbol_initializer(&self, target: Target<'source>) -> bool {
        target
            .symbol
            .body_range
            .and_then(|range| self.text(range))
            .is_some_and(|value| {
                // The call is the whole value: a member of it, or an operator, gives another.
                ["Symbol(", "Symbol.for("]
                    .iter()
                    .find_map(|call| value.trim().strip_prefix(call)?.strip_suffix(')'))
                    .is_some_and(|argument| !argument.contains(['(', ')']))
            })
            && !self.by_name.contains_key("Symbol")
            && !self.javascript_reassigned("Symbol", None)
    }

    /// Whether `property` is a `name: Symbol(..)` pair of the object literal `owner` holds
    /// and no write of this file changes it.
    fn is_symbol_property(&self, owner: Target<'source>, property: Target<'source>) -> bool {
        let Some(literal) = owner.symbol.body_range else {
            return false;
        };
        let unwritten = owner.symbol.name_range.is_some_and(|name| {
            !self.member_writes.contains_key(&(name, None))
                && !self
                    .member_writes
                    .contains_key(&(name, Some(property.symbol.name.as_str())))
        });
        self.text(literal)
            .is_some_and(|value| value.starts_with('{'))
            && literal.encloses(property.symbol.item_range)
            && property.symbol.syntax_element_kind == Some("pair")
            && self.property_is_settled(literal, property)
            && !self.unknown_member_writes.contains(&None)
            && !self
                .unknown_member_writes
                .contains(&Some(owner.symbol.name.as_str()))
            && !self.javascript_reassigned(&owner.symbol.name, None)
            && unwritten
            && self.is_symbol_initializer(property)
    }

    /// Whether no spread and no computed key written after the property can replace it.
    fn property_is_settled(
        &self,
        literal: rift_syntax::ByteRange,
        property: Target<'source>,
    ) -> bool {
        self.text(rift_syntax::ByteRange {
            start: property.symbol.item_range.end,
            end: literal.end,
        })
        .is_some_and(|later| !later.contains("...") && !later.contains('['))
    }

    /// Whether no write through `exports` can give the export `name` a second value.
    fn named_export_is_written_once(&self, name: &str) -> bool {
        self.export_keys.is_empty()
            && !self.unknown_member_writes.contains(&None)
            && !self.export_writes.contains_key(&ExportSlot::Unknown)
            && [ExportSlot::Named(name), ExportSlot::Default]
                .iter()
                .all(|slot| self.export_writes.get(slot).is_none_or(Option::is_some))
    }

    /// The pair of an exported object literal a binding names. Its use is judged with the
    /// object that holds it, never as a free-standing export.
    pub(super) fn exported_pair(
        &self,
        binding: &rift_syntax::SyntaxExportBinding,
    ) -> Option<Target<'source>> {
        if binding.kind != SyntaxExportKind::Named || binding.container.is_some() {
            return None;
        }
        self.target_at(binding.exported?)
            .filter(|target| target.symbol.syntax_element_kind == Some("pair"))
    }

    /// Whether no later property of the exported literal can replace `pair`.
    pub(super) fn exported_pair_is_settled(&self, pair: Target<'source>, work: &mut Work) -> bool {
        self.document
            .facts
            .export_bindings()
            .unwrap_or_default()
            .iter()
            .any(|binding| {
                work.charge()
                    && binding.exported == pair.symbol.name_range
                    && self.exported_pair(binding).is_some()
            })
    }

    fn javascript_nested_export_slot(
        &self,
        member: &rift_syntax::StaticMember,
    ) -> Option<(rift_syntax::ByteRange, ExportSlot<'source>)> {
        let parent = self.members.get(&member.receiver).copied().flatten()?;
        if self.text(parent.receiver)? != "module" {
            return None;
        }
        let name = parent.member.and_then(|range| self.text(range));
        let slot = match name {
            Some("exports") => member
                .member
                .and_then(|range| self.text(range))
                .map_or(ExportSlot::Unknown, ExportSlot::Named),
            None => ExportSlot::Unknown,
            _ => return None,
        };
        Some((parent.receiver, slot))
    }

    /// Whether no other write of the file gives the export another value. A computed key
    /// of unknown value counts as such a write.
    pub(super) fn javascript_export_is_stable(
        &self,
        binding: &rift_syntax::SyntaxExportBinding,
    ) -> bool {
        self.export_keys.is_empty() && self.javascript_written_export_is_stable(binding)
    }

    /// The same, with every computed key established as a symbol value.
    fn javascript_written_export_is_stable(
        &self,
        binding: &rift_syntax::SyntaxExportBinding,
    ) -> bool {
        if self.unknown_member_writes.contains(&None) {
            return false;
        }
        let slot = match binding.kind {
            SyntaxExportKind::Default => ExportSlot::Default,
            SyntaxExportKind::Named => match binding.exported.and_then(|range| self.text(range)) {
                Some(name) => ExportSlot::Named(name),
                None => return false,
            },
            SyntaxExportKind::All | SyntaxExportKind::Namespace => return true,
        };
        let replaced = |slot: &ExportSlot<'_>| {
            self.export_writes
                .get(slot)
                .is_some_and(|start| start.is_none_or(|start| !binding.range.contains(start)))
        };
        if replaced(&ExportSlot::Unknown) || replaced(&slot) {
            return false;
        }
        // `module.exports = value` followed by `module.exports.name = ...` at the top
        // level adds `name` to that value: the earlier write does not replace the later.
        !replaced(&ExportSlot::Default)
            || (slot != ExportSlot::Default
                && binding.container.is_none()
                && self
                    .export_writes
                    .get(&ExportSlot::Default)
                    .is_some_and(|start| {
                        start.is_some_and(|start| {
                            start < binding.range.start && self.top_level_default_export(start)
                        })
                    }))
    }

    /// Whether a default export binding outside every declaration starts at `start`.
    fn top_level_default_export(&self, start: u64) -> bool {
        self.document
            .facts
            .export_bindings()
            .unwrap_or_default()
            .iter()
            .any(|binding| {
                binding.kind == SyntaxExportKind::Default
                    && binding.container.is_none()
                    && binding.range.contains(start)
            })
    }

    /// Whether an export written through an alias of the exported object
    /// (`app.name = value`) has no other write through that alias.
    fn javascript_alias_write_is_stable(
        &self,
        binding: &rift_syntax::SyntaxExportBinding,
        work: &mut Work,
    ) -> bool {
        let Some((written, name)) = self
            .text(binding.range)
            .zip(binding.exported.and_then(|range| self.text(range)))
        else {
            return true;
        };
        if ["exports", "module.exports", "Object."]
            .iter()
            .any(|prefix| written.starts_with(prefix))
        {
            return true;
        }
        let Some((alias, _)) = written.split_once('.') else {
            return true;
        };
        let super::Binding::Found(alias) =
            self.binding(alias.trim(), binding.range.start, |_| true, work)
        else {
            return false;
        };
        !self.member_writes.contains_key(&(alias.name, None))
            && self
                .member_writes
                .get(&(alias.name, Some(name)))
                .is_some_and(Option::is_some)
    }

    /// Whether the binding exports what its own assignment declares. Such a name is no
    /// reference: a lexical declaration of the same name is another one.
    pub(super) fn declares_its_export(&self, binding: &rift_syntax::SyntaxExportBinding) -> bool {
        binding.local.is_some()
            && binding.local == binding.exported
            && self
                .text(binding.range)
                .is_some_and(|written| matches_export_form(written, ModuleMode::CommonJs))
            && !binding.local.is_some_and(|local| {
                self.target_at(local).is_some_and(|target| {
                    target.symbol.syntax_element_kind == Some("shorthand_property_identifier")
                })
            })
    }

    fn javascript_export_role(
        &self,
        binding: &rift_syntax::SyntaxExportBinding,
        role: rift_core::ReferenceRole,
    ) -> bool {
        if binding.type_only && role != rift_core::ReferenceRole::Type {
            return false;
        }
        let declaration = binding.local.and_then(|local| self.target_at(local));
        match declaration.map(|target| target.symbol.kind) {
            Some("interface" | "type_alias") => role == rift_core::ReferenceRole::Type,
            // A value declaration is never a type; an interface of the same name is.
            Some("variable" | "function") => role != rift_core::ReferenceRole::Type,
            _ => true,
        }
    }

    fn javascript_export_candidate(
        &self,
        binding: &rift_syntax::SyntaxExportBinding,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Export<'source>> {
        if binding.kind == SyntaxExportKind::Namespace {
            return Some(Export::Module {
                module: literal_contents(self.text(binding.source?)?)?,
            });
        }
        let local = binding.local?;
        let declaration = self.target_at(local);
        if declaration.is_none() && self.declares_its_export(binding) {
            return None;
        }
        let local_name = declaration
            .map(|target| target.symbol.name.as_str())
            .or_else(|| self.text(local))?;
        let shorthand = declaration.is_some_and(|target| {
            target.symbol.syntax_element_kind == Some("shorthand_property_identifier")
        });
        match binding.source {
            Some(source) => Some(Export::Reexport {
                module: literal_contents(self.text(source)?)?,
                name: local_name,
            }),
            None if !(declaration.is_none()
                || shorthand
                || self.blocked(local.start, work)
                || role == rift_core::ReferenceRole::Call
                    && self.javascript_local_written(local, local_name, work)) =>
            {
                Some(Export::Declaration(local))
            }
            None => Some(Export::Local {
                name: local_name,
                position: local.start,
            }),
        }
    }
}

impl File<'_> {
    /// Whether the exported local declaration `local` is written with a value that may not
    /// be callable. A name without its own binding falls back to any write of the name.
    fn javascript_local_written(
        &self,
        local: rift_syntax::ByteRange,
        name: &str,
        work: &mut Work,
    ) -> bool {
        match self
            .by_name
            .get(name)
            .and_then(|bindings| bindings.iter().find(|binding| binding.name == local))
        {
            Some(binding) => self.javascript_binding_written(binding, work),
            None => self.javascript_reassigned(name, None),
        }
    }
}

fn matches_export_form(source: &str, mode: ModuleMode) -> bool {
    let source = source.trim_start();
    let esm = source.strip_prefix("export").is_some_and(|rest| {
        rest.starts_with(|character: char| {
            character.is_whitespace() || matches!(character, '{' | '*' | '/')
        })
    });
    match mode {
        ModuleMode::Esm => esm,
        ModuleMode::CommonJs => !esm,
        ModuleMode::Unknown | ModuleMode::NodeDefault => false,
    }
}

pub(super) fn literal_contents(source: &str) -> Option<&str> {
    let quote = source.chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    source.strip_prefix(quote)?.strip_suffix(quote)
}

/// The project path a relative specifier names from `from`. A specifier ending in `/`,
/// and `.` or `..` alone, names a directory: the path then ends in `/`.
pub(super) fn relative_path(from: &str, module: &str, work: &mut Work) -> Option<String> {
    let directory = module.ends_with('/') || matches!(module, "." | "..");
    let relative = module.starts_with("./") || module.starts_with("../") || directory;
    if !relative || module.contains(['\\', '?', '#', '%']) || module.starts_with('/') {
        return None;
    }
    let mut segments = from
        .rsplit_once('/')
        .map_or_else(Vec::new, |(directory, _)| directory.split('/').collect());
    for segment in module.strip_suffix('/').unwrap_or(module).split('/') {
        if !work.charge() {
            return None;
        }
        match segment {
            "." => {}
            ".." => {
                segments.pop()?;
            }
            "" => return None,
            name => segments.push(name),
        }
    }
    let mut path = segments.join("/");
    if directory {
        path.push('/');
    }
    Some(path)
}
