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
            let (direct, stars) = export_candidates(file, name, mode, role, work)?;
            let Some(export) = direct else {
                if mode == ModuleMode::CommonJs || name != ExportSlot::Named("default") {
                    pending.extend(stars.into_iter().filter_map(|module| {
                        Some((
                            self.imported_for_role(file, module, mode, role, work)?,
                            name,
                            member,
                        ))
                    }));
                }
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
        let binding = js_binding(file, self.modes, name, position, role, work)?;
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

type ExportCandidates<'source> = (Option<Export<'source>>, Vec<&'source str>);

fn export_candidates<'source>(
    file: &File<'source>,
    name: ExportSlot<'source>,
    mode: ModuleMode,
    role: rift_core::ReferenceRole,
    work: &mut Work,
) -> Option<ExportCandidates<'source>> {
    let mut found: Option<Export<'source>> = None;
    let mut stars = Vec::new();
    for binding in file.document.facts.export_bindings()? {
        if !work.charge() {
            return None;
        }
        if !matches_export_form(file.text(binding.range)?, mode)
            || binding.container.is_some()
            || !file.javascript_export_role(binding, role)
        {
            continue;
        }
        if binding.kind == SyntaxExportKind::All {
            stars.push(literal_contents(file.text(binding.source?)?)?);
            continue;
        }
        let exported = match binding.kind {
            SyntaxExportKind::Named | SyntaxExportKind::Namespace => binding
                .exported
                .and_then(|range| file.text(range))
                .map(ExportSlot::Named),
            SyntaxExportKind::Default if mode == ModuleMode::CommonJs => Some(ExportSlot::Default),
            SyntaxExportKind::Default => Some(ExportSlot::Named("default")),
            SyntaxExportKind::All => None,
        };
        if exported != Some(name) {
            continue;
        }
        if mode == ModuleMode::CommonJs
            && role == rift_core::ReferenceRole::Call
            && !file.javascript_export_is_stable(binding)
        {
            return None;
        }
        let target = file.javascript_export_candidate(binding, role, work)?;
        found = Some(match found {
            Some(previous) => previous.merge(target, file)?,
            None => target,
        });
    }
    Some((found, stars))
}

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
        if self.facts.members().is_empty() {
            return Some(());
        }
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
            let Some((receiver, slot)) = self.javascript_export_slot(member) else {
                continue;
            };
            // A key bound to a `Symbol(..)` value of this file names no export (D41).
            if slot == ExportSlot::Unknown && self.javascript_symbol_key(member, work) {
                continue;
            }
            if matches!(
                self.binding(self.text(receiver)?, receiver.start, |_| true, work),
                super::Binding::Missing
            ) {
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

    /// Whether `member` is `exports[key]` or `module.exports[key]` with `key` a `const`
    /// of this file bound to a `Symbol(..)` value: such a key is no export name. Only a
    /// `const` declaring that one name counts.
    fn javascript_symbol_key(&self, member: &rift_syntax::StaticMember, work: &mut Work) -> bool {
        if member.member.is_some()
            || !matches!(
                self.text(member.receiver),
                Some("exports" | "module.exports")
            )
        {
            return false;
        }
        let key = self
            .text(rift_syntax::ByteRange {
                start: member.receiver.end,
                end: member.range.end,
            })
            .map(str::trim)
            .and_then(|key| key.strip_prefix('['))
            .and_then(|key| key.strip_suffix(']'))
            .map(str::trim);
        let Some(key) = key.filter(|key| {
            !key.is_empty()
                && key
                    .chars()
                    .all(|character| character.is_alphanumeric() || matches!(character, '_' | '$'))
        }) else {
            return false;
        };
        let super::Binding::Found(binding) = self.binding(key, member.range.start, |_| true, work)
        else {
            return false;
        };
        self.target_at(binding.name).is_some_and(|target| {
            target.symbol.kind == "variable"
                && self
                    .text(rift_syntax::ByteRange {
                        start: 0,
                        end: target.symbol.item_range.start,
                    })
                    .is_some_and(|before| before.trim_end().ends_with("const"))
                && target
                    .symbol
                    .body_range
                    .and_then(|range| self.text(range))
                    .is_some_and(|value| value.trim_start().starts_with("Symbol("))
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

    pub(super) fn javascript_export_is_stable(
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
                .is_some_and(|start| *start != Some(binding.range.start))
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
                    && binding.range.start == start
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
        ModuleMode::CommonJs => {
            source.starts_with("module.exports") || source.starts_with("exports.")
        }
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
