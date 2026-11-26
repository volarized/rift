//! Resolves captured ECMAScript exports without a normalized graph.

use super::javascript::{ModuleMode, PackageModes, js_binding};
use super::{File, Target, Work};
use crate::analyzer::namespace::typescript::folder_path;
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
    /// The names files export, for a caller that resolves every one of them.
    pub(super) names: Option<&'index NameIndex<'source>>,
}

/// The names each file exports, read once. `None` for a file whose module value is open.
pub(super) type NameIndex<'source> =
    std::cell::RefCell<BTreeMap<&'source str, Option<std::rc::Rc<Names<'source>>>>>;
pub(super) type Names<'source> = std::collections::BTreeSet<&'source str>;

impl<'source> Exports<'_, 'source> {
    pub(super) fn imported(
        &self,
        from: &File<'source>,
        module: &str,
        work: &mut Work,
    ) -> Option<&File<'source>> {
        let from_path = from.document.path.as_str();
        let path = relative_path(from_path, module, work)?;
        if is_declaration_file(from_path) {
            return declared_file(self.files, self.modes, &path);
        }
        let commonjs = self
            .modes
            .folder(path.strip_suffix('/').unwrap_or(&path))
            .is_some_and(|folder| {
                (folder.observes_explicit_main
                    || (folder.observed_entry.is_some()
                        && (path.ends_with('/') || folder.earlier_file == Some(false))))
                    && !from.facts.has_javascript_module_syntax()
                    && self.modes.mode(from_path, work) == ModuleMode::CommonJs
            });
        if work.exhausted {
            return None;
        }
        module_file_with_runtime_observations(self.files, self.modes, &path, false, commonjs)
    }

    /// The file a specifier names for one export: in a declaration file the declaration file
    /// it reaches, and the written runtime file for a name that one does not declare.
    pub(super) fn imported_for(
        &self,
        from: &File<'source>,
        module: &str,
        name: &str,
        work: &mut Work,
    ) -> Option<&File<'source>> {
        let imported = self.imported(from, module, work);
        let from = from.document.path.as_str();
        let (name, _) = split_name(name);
        if !is_declaration_file(from)
            || imported.is_some_and(|imported| self.declares(imported, name, work))
        {
            return imported;
        }
        let path = relative_path(from, module, work)?;
        module_file(self.files, self.modes, &path, false).or(imported)
    }

    /// Whether the file exports `name`, itself or through a star.
    fn declares(&self, file: &File<'source>, name: &str, work: &mut Work) -> bool {
        if name != "default" {
            return self
                .names(file, work)
                .is_none_or(|names| names.contains(name));
        }
        file.document
            .facts
            .export_bindings()
            .unwrap_or_default()
            .iter()
            .any(|binding| {
                binding.container.is_none()
                    && (binding.kind == SyntaxExportKind::Default
                        || binding.exported.and_then(|range| file.text(range)) == Some(name))
            })
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
            self.imported_for(from, module, name, work)?,
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
        self.resolve_value(self.imported(from, module, work)?, role, work)
    }

    /// The one file a forward of the whole module reaches. A condition that selects between
    /// modules leaves one only when the others are relative paths that name no file.
    fn forwarded_file(&self, file: &File<'source>, work: &mut Work) -> Option<&File<'source>> {
        let mut found: Option<&File<'source>> = None;
        for (module, member) in &file.module_value.modules {
            if member.is_some() {
                return None;
            }
            match self.imported(file, module, work) {
                Some(imported)
                    if found.is_some_and(|found| found.document.path != imported.document.path) =>
                {
                    return None;
                }
                Some(imported) => found = Some(imported),
                None if names_source(module) => {}
                None => return None,
            }
        }
        found
    }

    /// The settled pair of the object literal a `CommonJS` file forwards: `name`, or for the
    /// module value the member it takes from the module.
    pub(super) fn literal_member(
        &self,
        file: &File<'source>,
        name: Option<&'source str>,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut path = file.document.path.as_str();
        let mut name = name;
        let mut visited = std::collections::BTreeSet::new();
        while visited.insert(path) {
            let file = self.files.get(path)?;
            if !work.charge() {
                return None;
            }
            if file.module_value.value == ModuleValue::Forward {
                path = match (name, file.module_value.modules.as_slice()) {
                    (None, &[(module, Some(member))]) => {
                        name = Some(member);
                        self.imported(file, module, work)?
                    }
                    (_, _) => self.forwarded_file(file, work)?,
                }
                .document
                .path
                .as_str();
                continue;
            }
            let literal = file.facts.commonjs_export()?.literal?;
            let owner = file.qualified_target("default")?;
            return (owner.symbol.kind == "object" && owner.symbol.body_range == Some(literal))
                .then(|| file.qualified_target(&format!("default.{}", name?)))?
                .filter(|member| {
                    member.symbol.syntax_element_kind == Some("pair")
                        && file.exported_pair_is_settled(*member, work)
                });
        }
        None
    }

    /// What the module value of a `CommonJS` file is.
    pub(super) fn resolve_value(
        &self,
        file: &File<'source>,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<ResolvedExport<'source>> {
        self.resolve_slot(file, ExportSlot::Default, ModuleMode::CommonJs, role, work)
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
                // A forward another value can replace establishes no target.
                if stars.iter().any(|star| !star.settled) {
                    return None;
                }
                for star in stars {
                    // Only `module.exports = require(..)` passes the module value on.
                    if default && !star.replaces_module {
                        continue;
                    }
                    let imported = if star.conditional {
                        Some(self.forwarded_file(file, work)?)
                    } else {
                        self.imported(file, star.module, work)
                    };
                    let imported = imported.filter(|imported| match name {
                        ExportSlot::Named(name) => default || self.supplies(imported, name, work),
                        ExportSlot::Default | ExportSlot::Unknown => true,
                    });
                    pending.extend(imported.map(|imported| (imported, name, member)));
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
                    if let Some(imported) = self.imported_for(file, module, name, work) {
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
        if member.is_none()
            && let Some(signature) = file.declared_signature(name)
        {
            return Some(Export::Declaration(signature));
        }
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
                let module = literal_contents(file.text(binding.source?)?)?;
                let values = &file.module_value;
                let value = values.forwarded(binding.range);
                let settled = match value {
                    ModuleValue::Forward if !values.modules.contains(&(module, None)) => continue,
                    ModuleValue::Replaced => continue,
                    ModuleValue::Unsettled => false,
                    ModuleValue::Forward | ModuleValue::Unwritten => true,
                };
                stars.push(ExportStar {
                    module,
                    replaces_module: written.starts_with("module.exports"),
                    settled,
                    conditional: value == ModuleValue::Forward && values.modules.len() > 1,
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
            // `module.exports = require(..).name` gives the value as a whole forward does.
            if binding.kind == SyntaxExportKind::Default
                && let Some(module) = file.required_module(binding)
            {
                match file.module_value.forwarded(binding.range) {
                    ModuleValue::Forward if !file.module_value.modules.contains(&module) => {
                        continue;
                    }
                    ModuleValue::Replaced => continue,
                    ModuleValue::Unsettled => return None,
                    ModuleValue::Forward | ModuleValue::Unwritten => {}
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

    /// The names a file exports, itself and through its stars. `None` when the work bound is
    /// spent or a forward leaves open what the file exports.
    pub(super) fn names(
        &self,
        file: &File<'source>,
        work: &mut Work,
    ) -> Option<std::rc::Rc<Names<'source>>> {
        let path = file.document.path.as_str();
        let known = self
            .names
            .and_then(|index| index.borrow().get(path).cloned());
        if let Some(names) = known {
            return names;
        }
        let names = self.read_names(file, work).map(std::rc::Rc::new);
        if let Some(index) = self.names.filter(|_| !work.exhausted) {
            index.borrow_mut().insert(path, names.clone());
        }
        names
    }

    /// Whether the file can export `name`.
    fn supplies(&self, file: &File<'source>, name: &str, work: &mut Work) -> bool {
        self.names.is_none()
            || self
                .names(file, work)
                .is_none_or(|names| names.contains(name))
    }

    fn read_names(&self, file: &File<'source>, work: &mut Work) -> Option<Names<'source>> {
        let mut pending = vec![file];
        let mut visited = std::collections::BTreeSet::new();
        let mut names = Names::new();
        while let Some(file) = pending.pop() {
            if !work.charge() {
                return None;
            }
            if !visited.insert(file.document.path.as_str()) {
                continue;
            }
            if file.module_value.value == ModuleValue::Unsettled {
                return None;
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

/// Whether a specifier is a relative path that can only name a JavaScript file.
fn names_source(module: &str) -> bool {
    (module.starts_with("./") || module.starts_with("../"))
        && std::path::Path::new(module)
            .extension()
            .is_none_or(|extension| matches!(extension.to_str(), Some("js" | "cjs" | "mjs")))
}

/// Whether the file declares types alone.
pub(crate) fn is_declaration_file(path: &str) -> bool {
    [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|extension| path.ends_with(extension))
}

/// The file a specifier of a declaration file names: the TypeScript source in place of the
/// written runtime file, else the declaration file. Two of one rank name none. A folder names
/// what its manifest writes as types or as `main`, else its index.
fn declared_file<'index, 'source>(
    files: &'index BTreeMap<&'source str, File<'source>>,
    modes: &PackageModes<'source>,
    path: &str,
) -> Option<&'index File<'source>> {
    let directory = path.strip_suffix('/');
    let mut stems = Vec::new();
    if directory.is_none() {
        if let Some(file) = typed_file(files, path) {
            return Some(file);
        }
        if let Some((stem, sources, declarations)) = declared_extensions(path) {
            return first_declared(files, &[stem.to_owned()], sources, declarations);
        }
        stems.push(path.to_owned());
    }
    let folder = directory.unwrap_or(path);
    let manifest = modes.folder(folder);
    if manifest.is_some_and(|manifest| !manifest.known) {
        return None;
    }
    let written = manifest.and_then(|manifest| manifest.types.as_ref().or(manifest.main.as_ref()));
    if let Some(written) = written
        && stems.iter().all(|stem| {
            first_declared(files, std::slice::from_ref(stem), SOURCES, DECLARATIONS).is_none()
        })
        && let Some(file) = folder_path(folder, written).and_then(|written| {
            typed_file(files, &written).or_else(|| {
                let (stem, sources, declarations) = declared_extensions(&written).unwrap_or((
                    written.as_str(),
                    SOURCES,
                    DECLARATIONS,
                ));
                first_declared(files, &[stem.to_owned()], sources, declarations)
            })
        })
    {
        return Some(file);
    }
    stems.push(folder_path(folder, "index")?);
    first_declared(files, &stems, SOURCES, DECLARATIONS)
}

const SOURCES: &[&str] = &[".ts", ".tsx"];
const DECLARATIONS: &[&str] = &[".d.ts"];

/// The stem of a written runtime path, with the extensions of its sources and declarations.
fn declared_extensions(
    path: &str,
) -> Option<(&str, &'static [&'static str], &'static [&'static str])> {
    [
        (".js", SOURCES, DECLARATIONS),
        (".jsx", SOURCES, DECLARATIONS),
        (".mjs", &[".mts"], &[".d.mts"]),
        (".cjs", &[".cts"], &[".d.cts"]),
    ]
    .into_iter()
    .find_map(|(written, sources, declarations)| {
        Some((path.strip_suffix(written)?, sources, declarations))
    })
}

fn typed_file<'index, 'source>(
    files: &'index BTreeMap<&'source str, File<'source>>,
    path: &str,
) -> Option<&'index File<'source>> {
    files
        .get(path)
        .filter(|file| file.document.facts.language().name == "typescript")
}

/// The one source of the stems, else their one declaration file.
fn first_declared<'index, 'source>(
    files: &'index BTreeMap<&'source str, File<'source>>,
    stems: &[String],
    sources: &[&str],
    declarations: &[&str],
) -> Option<&'index File<'source>> {
    for extensions in [sources, declarations] {
        let mut found = None;
        for stem in stems {
            for extension in extensions {
                if let Some(file) = typed_file(files, &format!("{stem}{extension}"))
                    && found.replace(file).is_some()
                {
                    return None;
                }
            }
        }
        if found.is_some() {
            return found;
        }
    }
    None
}

pub(super) fn module_file<'index, 'source>(
    files: &'index BTreeMap<&'source str, File<'source>>,
    modes: &PackageModes<'source>,
    path: &str,
    declarations: bool,
) -> Option<&'index File<'source>> {
    module_file_with_runtime_observations(files, modes, path, declarations, false)
}

fn module_file_with_runtime_observations<'index, 'source>(
    files: &'index BTreeMap<&'source str, File<'source>>,
    modes: &PackageModes<'source>,
    path: &str,
    declarations: bool,
    commonjs: bool,
) -> Option<&'index File<'source>> {
    if path.ends_with('/') {
        let found = match folder_file(files, modes, path, commonjs) {
            Candidate::File(file) => Some(file),
            Candidate::Open => return None,
            Candidate::Missing => None,
        };
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
    let direct = one_file(files, stem, MODULES);
    let candidate = match direct {
        Candidate::File(_) if matches!(folder_index(files, stem), Candidate::Missing) => direct,
        Candidate::File(_) | Candidate::Open => return None,
        // `./x.js` never names a folder by its manifest.
        Candidate::Missing if stem != path => folder_index(files, stem),
        Candidate::Missing => folder_file(files, modes, stem, commonjs),
    };
    let found = match candidate {
        Candidate::File(file) => Some(file),
        Candidate::Open => return None,
        Candidate::Missing => None,
    };
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

const MODULES: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".js", ".mjs", ".cjs"];
const INDEXES: &[&str] = &[".ts", ".tsx", ".js"];

/// What the files of the analysis hold for a path.
enum Candidate<'index, 'source> {
    File(&'index File<'source>),
    /// Several files, or one that is not read here.
    Open,
    Missing,
}

/// The one file of a stem.
fn one_file<'index, 'source>(
    files: &'index BTreeMap<&'source str, File<'source>>,
    stem: &str,
    extensions: &[&str],
) -> Candidate<'index, 'source> {
    let mut found = Candidate::Missing;
    for extension in extensions {
        if let Some(file) = files.get(format!("{stem}{extension}").as_str()) {
            if matches!(found, Candidate::File(_)) {
                return Candidate::Open;
            }
            found = Candidate::File(file);
        }
    }
    found
}

fn folder_index<'index, 'source>(
    files: &'index BTreeMap<&'source str, File<'source>>,
    directory: &str,
) -> Candidate<'index, 'source> {
    folder_path(directory, "index")
        .map_or(Candidate::Missing, |stem| one_file(files, &stem, INDEXES))
}

/// The file Node loads for a folder: what `main` of its manifest names, as a file or as a
/// folder with an index, else the folder's index.
fn folder_file<'index, 'source>(
    files: &'index BTreeMap<&'source str, File<'source>>,
    modes: &PackageModes<'source>,
    specifier: &str,
    commonjs: bool,
) -> Candidate<'index, 'source> {
    let directory = specifier.strip_suffix('/').unwrap_or(specifier);
    let index = |directory: &str| folder_index(files, directory);
    let Some(manifest) = modes.folder(directory) else {
        return index(directory);
    };
    if !manifest.known {
        return Candidate::Open;
    }
    let Some(main) = &manifest.main else {
        return index(directory);
    };
    let Some(path) = folder_path(directory, main) else {
        return Candidate::Open;
    };
    let observed = || {
        if !commonjs || !(specifier.ends_with('/') || manifest.earlier_file == Some(false)) {
            return None;
        }
        files.get(manifest.observed_entry.as_ref()?.as_str())
    };
    if commonjs && manifest.observes_explicit_main {
        let entry = if !specifier.ends_with('/') && manifest.earlier_file.is_none() {
            // A bounded observation can leave earlier paths unknown. Keep the already
            // selected written source, while requiring complete observations for any fallback.
            files.get(path.as_str())
        } else {
            observed()
        };
        return entry
            .filter(|file| file.document.facts.language().name.as_str() == "javascript")
            .map_or(Candidate::Open, Candidate::File);
    }
    if let Some(file) = files.get(path.as_str()) {
        return Candidate::File(file);
    }
    let extension = std::path::Path::new(&path)
        .extension()
        .and_then(std::ffi::OsStr::to_str);
    match extension {
        // A data or native file is no source of the analysis, and Node loads it when it exists.
        Some("json" | "node" | "wasm") => return Candidate::Open,
        // Node reads no TypeScript source for a file the package does not ship.
        Some("js" | "cjs" | "mjs") => {}
        _ => {
            if let found @ Candidate::File(_) = one_file(files, &path, &[".js"]) {
                return found;
            }
        }
    }
    match index(&path) {
        // Node tries a data or native file of this name before the folder's index, and the
        // selected files of a package name none.
        Candidate::Missing if !matches!(extension, Some("js" | "cjs" | "mjs")) => {
            observed().map_or(Candidate::Open, Candidate::File)
        }
        Candidate::Missing => index(directory),
        found => found,
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
    /// Whether no other write of the file can give the module another value.
    settled: bool,
    /// Whether a condition selects between this module and another.
    conditional: bool,
}

/// What the writes of `module.exports` leave the module with.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ModuleValue {
    /// The value the module starts with.
    #[default]
    Unwritten,
    /// What a forward requires.
    Forward,
    /// A value the file writes itself, after every forward.
    Replaced,
    /// The value depends on a condition, a call or a computed key.
    Unsettled,
}

/// A required module by the contents of its specifier, and the one export taken from it.
type Required<'source> = (&'source str, Option<&'source str>);

/// The writes of `module.exports` in one file and what they leave the module with.
#[derive(Default)]
pub(super) struct ModuleValues<'source> {
    /// Start offsets of the written targets, in source order.
    writes: Vec<u64>,
    value: ModuleValue,
    /// What the deciding forward selects between.
    modules: Vec<Required<'source>>,
}

impl ModuleValues<'_> {
    /// The value of the module for an export binding that writes it, and the value before
    /// every write for any other binding.
    fn forwarded(&self, binding: rift_syntax::ByteRange) -> ModuleValue {
        let first = self.writes.partition_point(|start| *start < binding.start);
        if self
            .writes
            .get(first)
            .is_some_and(|start| binding.contains(*start))
        {
            self.value
        } else {
            ModuleValue::Unwritten
        }
    }
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
                if receiver == member.receiver
                    && self.text(receiver) == Some("module")
                    && self
                        .written_key(member)
                        .and_then(literal_contents)
                        .is_none_or(|key| key == "exports")
                {
                    self.module_value.writes.push(range.start);
                }
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
        self.module_value.writes.sort_unstable();
        let (value, modules) = self.module_value(work)?;
        self.module_value.value = value;
        self.module_value.modules = modules;
        Some(())
    }

    /// The name of a module-scope function a declaration file writes by signatures alone.
    fn declared_signature(&self, name: &str) -> Option<rift_syntax::ByteRange> {
        let declarations = self.by_qualified_name.get(name)?;
        (is_declaration_file(self.document.path.as_str())
            && declarations
                .iter()
                .all(|symbol| symbol.syntax_element_kind == Some("function_signature")))
        .then(|| declarations.first()?.name_range)?
    }

    /// The module a binding of the module value requires.
    fn required_module(
        &self,
        binding: &rift_syntax::SyntaxExportBinding,
    ) -> Option<Required<'source>> {
        let module = literal_contents(self.text(binding.source?)?)?;
        match binding.kind {
            SyntaxExportKind::All => Some((module, None)),
            SyntaxExportKind::Default => Some((module, Some(self.text(binding.local?)?))),
            SyntaxExportKind::Named | SyntaxExportKind::Namespace => None,
        }
    }

    /// What the writes of `module.exports` leave the module with, or `None` once the work
    /// bound is spent. The last write that always runs decides, unless a later one that can run
    /// or not gives another value. A forward of the same module gives the same value.
    fn module_value(&self, work: &mut Work) -> Option<(ModuleValue, Vec<Required<'source>>)> {
        let writes = &self.module_value.writes;
        if writes.is_empty() {
            return Some((ModuleValue::Unwritten, Vec::new()));
        }
        let mut forwards = BTreeMap::<rift_syntax::ByteRange, Vec<Required<'source>>>::new();
        for binding in self.document.facts.export_bindings().unwrap_or_default() {
            if !work.charge() {
                return None;
            }
            if binding.container.is_none()
                && let Some(module) = self.required_module(binding)
            {
                forwards.entry(binding.range).or_default().push(module);
            }
        }
        let forward = |start: u64| {
            let after = rift_syntax::ByteRange {
                start: start.saturating_add(1),
                end: 0,
            };
            forwards
                .range(..after)
                .next_back()
                .filter(|(range, _)| range.contains(start))
                .map(|(_, modules)| modules)
        };
        // The end of the source a `return` can skip, for each place where more of it starts.
        let mut skipped = self.facts.skippable().to_vec();
        skipped.sort_unstable();
        let mut end = 0;
        for range in &mut skipped {
            end = range.end.max(end);
            range.end = end;
        }
        let skippable = |write: &rift_syntax::ByteRange| {
            let after = skipped.partition_point(|range| range.start <= write.start);
            skipped[..after]
                .last()
                .is_some_and(|range| write.start < range.end)
        };
        let runs = self
            .facts
            .commonjs_writes()
            .iter()
            .map(|(write, always)| (write.start, (write.end, *always && !skippable(write))))
            .collect::<BTreeMap<_, _>>();
        // A write runs once its value is read, so the one that ends last runs last.
        let last = writes
            .iter()
            .filter_map(|start| {
                let (end, always) = *runs.get(start)?;
                always.then_some((end, *start))
            })
            .max();
        let Some((last_end, last)) = last else {
            return Some((ModuleValue::Unsettled, Vec::new()));
        };
        let last = forward(last);
        // A write in a function called later, or under a condition after the last one.
        let replaced = writes
            .iter()
            .filter(|start| runs.get(start).is_none_or(|(end, _)| *end > last_end))
            .map(|start| forward(*start))
            .any(|value| value.is_none() || value != last);
        Some(match last {
            _ if replaced => (ModuleValue::Unsettled, Vec::new()),
            Some(modules) => (ModuleValue::Forward, modules.clone()),
            None => (ModuleValue::Replaced, Vec::new()),
        })
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

    /// The source between the brackets of `receiver[key]`.
    fn written_key(&self, member: &rift_syntax::StaticMember) -> Option<&'source str> {
        if member.member.is_some() {
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
    }

    /// The name written as the key of `exports[key]` or `module.exports[key]`.
    fn javascript_export_key(&self, member: &rift_syntax::StaticMember) -> Option<&'source str> {
        if !matches!(
            self.text(member.receiver),
            Some("exports" | "module.exports")
        ) {
            return None;
        }
        self.written_key(member).filter(|key| {
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
        pair.symbol
            .name_range
            .and_then(|range| self.text(range))
            .zip(self.exports.as_ref())
            .and_then(|(name, exports)| exports.named.get(name))
            .into_iter()
            .flatten()
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
        // Only a whole leading name is the module's own object, not `exportsApi`.
        if written.starts_with("Object.defineProperty(")
            || ["exports", "module.exports"].iter().any(|name| {
                written.strip_prefix(name).is_some_and(|rest| {
                    !rest.starts_with(|next: char| {
                        next.is_alphanumeric() || matches!(next, '_' | '$')
                    })
                })
            })
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
