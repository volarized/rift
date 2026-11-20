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
        module_file(self.files, &path)
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
        self.resolve_file(self.imported(from, module, work)?, name, mode, role, work)
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
                        Some((self.imported(file, module, work)?, name, member))
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
                    if let Some(imported) = self.imported(file, module, work) {
                        pending.push((imported, ExportSlot::Named(name), member));
                    }
                    None
                }
                Export::Module { module } => {
                    let imported = self.imported(file, module, work)?;
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
            ModuleMode::Unknown => false,
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
) -> Option<&'index File<'source>> {
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
    found
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
        if found
            .as_ref()
            .is_some_and(|previous| previous.key(file) != target.key(file))
        {
            return None;
        }
        found = Some(target);
    }
    Some((found, stars))
}

impl<'source> Export<'source> {
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
            let Some(member) = self.members.get(&range).copied().flatten() else {
                continue;
            };
            let Some((receiver, slot)) = self.javascript_export_slot(member) else {
                continue;
            };
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

    fn javascript_export_is_stable(&self, binding: &rift_syntax::SyntaxExportBinding) -> bool {
        let slot = match binding.kind {
            SyntaxExportKind::Default => ExportSlot::Default,
            SyntaxExportKind::Named => match binding.exported.and_then(|range| self.text(range)) {
                Some(name) => ExportSlot::Named(name),
                None => return false,
            },
            SyntaxExportKind::All | SyntaxExportKind::Namespace => return true,
        };
        [ExportSlot::Unknown, ExportSlot::Default, slot]
            .iter()
            .filter_map(|slot| self.export_writes.get(slot))
            .all(|start| *start == Some(binding.range.start))
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
            Some("variable") => role != rift_core::ReferenceRole::Type,
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
                    && self.javascript_reassigned(local_name, None)) =>
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
        ModuleMode::Unknown => false,
    }
}

pub(super) fn literal_contents(source: &str) -> Option<&str> {
    let quote = source.chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    source.strip_prefix(quote)?.strip_suffix(quote)
}

pub(super) fn relative_path(from: &str, module: &str, work: &mut Work) -> Option<String> {
    let relative = module.starts_with("./") || module.starts_with("../");
    if !relative || module.contains(['\\', '?', '#', '%']) {
        return None;
    }
    let mut segments = from
        .rsplit_once('/')
        .map_or_else(Vec::new, |(directory, _)| directory.split('/').collect());
    for segment in module.split('/') {
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
    Some(segments.join("/"))
}
