//! Resolves explicit JavaScript module paths and lexical bindings.

use std::collections::BTreeMap;

use rift_syntax::{StaticBinding, StaticBindingKind, StaticCall, StaticCallee};

use super::{Binding, File, Index, Target, Work};
use crate::NamespaceInput;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ModuleMode {
    Esm,
    CommonJs,
    NodeDefault,
    Unknown,
}

/// Module-mode declarations captured from the nearest package manifest.
pub(super) struct PackageModes<'source> {
    modes: BTreeMap<String, ModuleMode>,
    module_syntax: std::collections::BTreeSet<&'source str>,
    pub(super) owner: rift_protocol::identity::SymbolOwner,
    pub(super) scopes: BTreeMap<&'source str, Scopes<'source>>,
}

pub(super) struct Scopes<'source> {
    pub(super) values: BTreeMap<rift_syntax::ByteRange, Option<&'source rift_syntax::SyntaxSymbol>>,
    pub(super) classes: crate::EnclosingDefinitions<&'source rift_syntax::SyntaxSymbol>,
    pub(super) definitions: crate::EnclosingDefinitions<&'source rift_syntax::SyntaxSymbol>,
    pub(super) callables: crate::EnclosingDefinitions<&'source rift_syntax::SyntaxSymbol>,
    /// Every arrow function and function expression, named or not.
    pub(super) function_values: crate::EnclosingDefinitions<rift_syntax::ByteRange>,
    pub(super) functions: crate::EnclosingDefinitions<rift_syntax::ByteRange>,
    pub(super) blocks: crate::EnclosingDefinitions<rift_syntax::ByteRange>,
    pub(super) deferred: crate::EnclosingDefinitions<rift_syntax::ByteRange>,
}

impl<'source> PackageModes<'source> {
    pub(super) fn new(
        namespace: &NamespaceInput<'_>,
        files: &BTreeMap<&'source str, File<'source>>,
        work: &mut Work,
    ) -> Option<Self> {
        let mut modes = BTreeMap::new();
        for source in namespace.files().iter().chain(namespace.context_sources()) {
            if !work.charge() {
                return None;
            }
            let Some(directory) = manifest_directory(source.path().as_str()) else {
                continue;
            };
            let mode = if source.text().len() <= namespace.syntax().source_bytes_max() {
                package_mode(source.text())
            } else {
                ModuleMode::Unknown
            };
            modes.entry(directory.to_owned()).or_insert(mode);
        }
        let mut scopes = BTreeMap::new();
        let mut module_syntax = std::collections::BTreeSet::new();
        for file in files.values().filter(|file| {
            matches!(
                file.document.facts.language().name.as_str(),
                "javascript" | "typescript"
            )
        }) {
            if file.facts.has_javascript_module_syntax() {
                module_syntax.insert(file.document.path.as_str());
            }
            scopes.insert(file.document.path.as_str(), Scopes::new(file, work)?);
        }
        Some(Self {
            modes,
            module_syntax,
            owner: namespace.owner().clone(),
            scopes,
        })
    }

    pub(super) fn mode(&self, path: &str, work: &mut Work) -> ModuleMode {
        match path.rsplit_once('.').map(|(_, extension)| extension) {
            Some("mjs" | "ts" | "tsx" | "mts" | "cts") => return ModuleMode::Esm,
            Some("cjs") => return ModuleMode::CommonJs,
            Some("js") => {}
            _ => return ModuleMode::Unknown,
        }
        let mut directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
        loop {
            if !work.charge() {
                return ModuleMode::Unknown;
            }
            if let Some(&mode) = self.modes.get(directory) {
                if mode == ModuleMode::NodeDefault {
                    return if self.module_syntax.contains(path) {
                        ModuleMode::Esm
                    } else {
                        ModuleMode::CommonJs
                    };
                }
                return mode;
            }
            if directory.is_empty() {
                return ModuleMode::Unknown;
            }
            directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }
}

pub(super) fn typescript_commonjs(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cts"))
}

fn manifest_directory(path: &str) -> Option<&str> {
    if path == "package.json" {
        Some("")
    } else {
        path.strip_suffix("/package.json")
    }
}

fn package_mode(text: &str) -> ModuleMode {
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(text) {
        Ok(manifest) => match manifest.get("type") {
            Some(value) if value.as_str() == Some("module") => ModuleMode::Esm,
            Some(value) if value.as_str() == Some("commonjs") => ModuleMode::CommonJs,
            None => ModuleMode::NodeDefault,
            _ => ModuleMode::Unknown,
        },
        Err(_) => ModuleMode::Unknown,
    }
}

impl<'source> Index<'source> {
    pub(super) fn edge_javascript_target(
        &self,
        file: &File<'source>,
        range: rift_syntax::ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let name = file.text(range)?.trim();
        if name
            .split_once('.')
            .is_some_and(|(receiver, _)| matches!(receiver.trim(), "this" | "super"))
        {
            return None;
        }
        self.javascript_target(file, range, rift_core::ReferenceRole::Type, work)
    }

    pub(super) fn javascript_target(
        &self,
        file: &File<'source>,
        range: rift_syntax::ByteRange,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if let Some(member) = file.members.get(&range) {
            let member = (*member)?;
            let name = file.text(member.member?)?;
            if !file.members.contains_key(&member.receiver) {
                return self.javascript_named_member(
                    file,
                    file.text(member.receiver)?,
                    name,
                    range.start,
                    role,
                    work,
                );
            }
        }
        let name = file.text(range)?.trim();
        if let Some((receiver, member)) = name.split_once('.') {
            return self.javascript_named_member(
                file,
                receiver.trim(),
                member.trim(),
                range.start,
                role,
                work,
            );
        }
        let binding = js_binding(file, &self.javascript, name, range.start, role, work)?;
        self.javascript_binding_target(file, binding, role, work)
    }

    fn javascript_named_member(
        &self,
        file: &File<'source>,
        receiver: &str,
        member: &'source str,
        position: u64,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if matches!(receiver, "this" | "super") {
            return self.javascript_receiver_member(file, position, receiver, member, role, work);
        }
        match js_binding(file, &self.javascript, receiver, position, role, work) {
            Some(binding) => self.javascript_member(file, binding, member, role, work),
            None => self.javascript_commonjs_member(file, receiver, member, position, role, work),
        }
    }

    pub(super) fn javascript_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if call.callee == StaticCallee::Unsupported {
            return None;
        }
        if call.callee == StaticCallee::Constructor && file.text(call.callee_range)? == "super" {
            return self.javascript_super_constructor(file, call.range.start, work);
        }
        let target = match call.callee {
            StaticCallee::Member { receiver, member } if !file.text(receiver)?.contains('.') => {
                self.javascript_named_member(
                    file,
                    file.text(receiver)?,
                    file.text(member)?,
                    call.range.start,
                    rift_core::ReferenceRole::Call,
                    work,
                )?
            }
            _ => self.javascript_target(
                file,
                call.callee_range,
                rift_core::ReferenceRole::Call,
                work,
            )?,
        };
        if call.callee == StaticCallee::Constructor {
            return (target.symbol.kind == "class"
                || target
                    .symbol
                    .kinds
                    .contains(&rift_protocol::read::SymbolKind::Constructible)
                || self.javascript_declared_signature(target, true, work))
            .then_some(target);
        }
        target.callable().or_else(|| {
            self.javascript_declared_signature(target, false, work)
                .then_some(target)
        })
    }

    /// Whether the binding's declared type states that its value can be called or constructed.
    ///
    /// The annotation must name, through at most `DECLARED_TYPE_DEPTH_MAX` aliases, a type
    /// declaration whose own shape is the signature: a function or constructor type alias,
    /// or an interface with a call or construct signature. The call then targets the
    /// binding itself; no function body is chosen for it.
    fn javascript_declared_signature(
        &self,
        target: Target<'source>,
        constructs: bool,
        work: &mut Work,
    ) -> bool {
        let declared = |file: &File<'source>, name: rift_syntax::ByteRange| {
            file.declared_types.get(&name).copied().flatten()
        };
        let Some(mut file) = self.files.get(target.file.path.as_str()) else {
            return false;
        };
        let Some(mut annotation) = target
            .symbol
            .name_range
            .and_then(|name| declared(file, name))
        else {
            return false;
        };
        for _ in 0..super::javascript_members::DECLARED_TYPE_DEPTH_MAX {
            if !work.charge() {
                return false;
            }
            let Some(named) =
                self.javascript_target(file, annotation, rift_core::ReferenceRole::Type, work)
            else {
                return false;
            };
            let Some((owner, name)) = self
                .files
                .get(named.file.path.as_str())
                .zip(named.symbol.name_range)
            else {
                return false;
            };
            if owner.type_signatures.contains(&(name, constructs)) {
                return true;
            }
            if named.symbol.kind != "type_alias" {
                return false;
            }
            let Some(next) = declared(owner, name) else {
                return false;
            };
            file = owner;
            annotation = next;
        }
        false
    }

    pub(super) fn javascript_import(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if file.blocked(binding.name.start, work) {
            return None;
        }
        if let StaticBindingKind::JavaScriptRequire { export, .. } = binding.kind {
            export?;
            return self
                .javascript_require_value(file, binding, rift_core::ReferenceRole::Type, work)
                .or_else(|| {
                    self.javascript_require_value(
                        file,
                        binding,
                        rift_core::ReferenceRole::Read,
                        work,
                    )
                });
        }
        self.javascript_binding_target(file, binding, rift_core::ReferenceRole::Type, work)
            .or_else(|| {
                self.javascript_binding_target(file, binding, rift_core::ReferenceRole::Read, work)
            })
    }

    fn javascript_binding_target(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match binding.kind {
            StaticBindingKind::Local => file.target_at(binding.name).or_else(|| {
                let scopes = self.javascript.scopes.get(file.document.path.as_str())?;
                let symbol = scopes.values.get(&binding.scope).copied().flatten()?;
                Some(Target {
                    file: file.document,
                    symbol,
                })
            }),
            StaticBindingKind::JavaScriptInstance { .. } => file.target_at(binding.name),
            StaticBindingKind::JavaScriptImport { module, export } => {
                let name = match export {
                    Some(range) => file.text(range)?,
                    None => "default",
                };
                if self.javascript.mode(file.document.path.as_str(), work) == ModuleMode::CommonJs {
                    return None;
                }
                self.javascript_export(file, file.text(module)?, name, ModuleMode::Esm, role, work)
            }
            StaticBindingKind::JavaScriptRequire { .. }
                if matches!(
                    role,
                    rift_core::ReferenceRole::Read | rift_core::ReferenceRole::Write
                ) =>
            {
                file.target_at(binding.name)
            }
            StaticBindingKind::JavaScriptRequire { .. } => {
                self.javascript_require_value(file, binding, role, work)
            }
            _ => None,
        }
    }

    fn javascript_require_value(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let StaticBindingKind::JavaScriptRequire {
            module,
            export,
            require,
        } = binding.kind
        else {
            return None;
        };
        if !self.builtin_require(file, require.start, work) {
            return None;
        }
        if let Some(export) = export {
            return self.javascript_export(
                file,
                file.text(module)?,
                file.text(export)?,
                ModuleMode::CommonJs,
                role,
                work,
            );
        }
        let value = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
        }
        .resolve_module_value(file, file.text(module)?, role, work)?;
        self.javascript_export_value(value, role, work)
    }

    fn javascript_member(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        member: &'source str,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if role != rift_core::ReferenceRole::Type
            && let Some(&Some(annotation)) = file.declared_types.get(&binding.name)
        {
            let owner = self.javascript_declared_type(file, annotation, work)?;
            return self.javascript_class_member(owner, member, MemberMode::Instance, role, work);
        }
        if role != rift_core::ReferenceRole::Type
            && binding.kind == StaticBindingKind::Shadow
            && let Some(owner) = self
                .javascript_callback_parameter(file, binding, work)
                .or_else(|| self.javascript_destructured_name(file, binding, work))
        {
            return self.javascript_class_member(owner, member, MemberMode::Instance, role, work);
        }
        match binding.kind {
            StaticBindingKind::JavaScriptEdgeNamespace { module } => self.javascript_export(
                file,
                file.text(module)?,
                member,
                ModuleMode::Esm,
                role,
                work,
            ),
            StaticBindingKind::JavaScriptInstance { constructor } => {
                let class = self.javascript_target(
                    file,
                    constructor,
                    rift_core::ReferenceRole::Call,
                    work,
                )?;
                self.javascript_class_member(class, member, MemberMode::Instance, role, work)
            }
            StaticBindingKind::JavaScriptImport { .. } => {
                self.javascript_imported_member(file, binding, member, role, work)
            }
            StaticBindingKind::Local => self
                .javascript_binding_target(file, binding, role, work)
                .and_then(|target| self.javascript_object_member(target, member, role, work))
                .or_else(|| {
                    if role == rift_core::ReferenceRole::Type {
                        return None;
                    }
                    let owner = self.javascript_destructured_name(file, binding, work)?;
                    self.javascript_class_member(owner, member, MemberMode::Instance, role, work)
                }),
            StaticBindingKind::JavaScriptRequire {
                module,
                export: None,
                require,
            } if self.builtin_require(file, require.start, work) => self.javascript_export(
                file,
                file.text(module)?,
                member,
                ModuleMode::CommonJs,
                role,
                work,
            ),
            _ => None,
        }
    }

    fn javascript_imported_member(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        member: &'source str,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let StaticBindingKind::JavaScriptImport { module, export } = binding.kind else {
            return None;
        };
        let name = export
            .and_then(|range| file.text(range))
            .unwrap_or("default");
        let resolver = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
        };
        match resolver.resolve(file, file.text(module)?, name, ModuleMode::Esm, role, work)? {
            super::javascript_exports::ResolvedExport::Module(imported) => {
                let imported = self.files.get(imported.path.as_str())?;
                self.javascript_export_value(
                    resolver.resolve_file(imported, member, ModuleMode::Esm, role, work)?,
                    role,
                    work,
                )
            }
            value @ super::javascript_exports::ResolvedExport::Declaration { .. } => {
                let target = self.javascript_export_value(value, role, work)?;
                self.javascript_object_member(target, member, role, work)
            }
        }
    }

    pub(super) fn builtin_require(
        &self,
        file: &File<'source>,
        position: u64,
        work: &mut Work,
    ) -> bool {
        self.builtin_commonjs_name(file, "require", position, work)
    }

    fn builtin_commonjs_name(
        &self,
        file: &File<'source>,
        name: &str,
        position: u64,
        work: &mut Work,
    ) -> bool {
        (self.javascript.mode(file.document.path.as_str(), work) == ModuleMode::CommonJs
            || typescript_commonjs(file.document.path.as_str()))
            && !file.blocked(position, work)
            && matches!(
                file.binding(name, position, |_| true, work),
                Binding::Missing
            )
            && !file.javascript_reassigned(name, None)
    }

    fn javascript_commonjs_member(
        &self,
        file: &File<'source>,
        receiver: &str,
        member: &'source str,
        position: u64,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let name = match receiver {
            "exports" => member,
            "module" => member.strip_prefix("exports.")?,
            _ => return None,
        };
        if !self.builtin_commonjs_name(file, receiver, position, work) {
            return None;
        }
        let export = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
        }
        .resolve_file(file, name, ModuleMode::CommonJs, role, work)?;
        self.javascript_export_value(export, role, work)
    }

    pub(super) fn edge_javascript_module(&self, path: &str) -> Option<&File<'source>> {
        super::javascript_exports::module_file(&self.files, path)
    }

    fn javascript_export(
        &self,
        from: &File<'source>,
        module: &'source str,
        name: &'source str,
        mode: ModuleMode,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let resolved = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
        }
        .resolve(from, module, name, mode, role, work)?;
        self.javascript_export_value(resolved, role, work)
    }

    fn javascript_export_value(
        &self,
        resolved: super::javascript_exports::ResolvedExport<'source>,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match resolved {
            super::javascript_exports::ResolvedExport::Declaration {
                target,
                member: Some(member),
            } if target.symbol.kind == "class" => {
                self.javascript_class_member(target, member, MemberMode::Static, role, work)
            }
            super::javascript_exports::ResolvedExport::Declaration {
                target,
                member: Some(member),
            } => self.javascript_object_member(target, member, role, work),
            super::javascript_exports::ResolvedExport::Declaration {
                target,
                member: None,
            } => Some(target),
            super::javascript_exports::ResolvedExport::Module(_) => None,
        }
    }
}

impl File<'_> {
    pub(super) fn javascript_reassigned(&self, name: &str, member: Option<&str>) -> bool {
        self.assignments.names.contains(name)
            || member.is_some_and(|member| {
                self.assignments.unknown || self.assignments.members.contains(&(name, member))
            })
    }
}

impl File<'_> {
    /// Whether an assignment inside the scope of `binding` writes its name again. A
    /// write to an inner binding of the same name counts too: the check only refuses.
    pub(super) fn javascript_binding_reassigned(&self, binding: &StaticBinding) -> bool {
        self.text(binding.name)
            .and_then(|name| self.assignments.name_writes.get(name))
            .is_some_and(|writes| writes.iter().any(|&start| binding.scope.contains(start)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MemberMode {
    Instance,
    Static,
}

pub(super) fn js_binding<'source>(
    file: &File<'source>,
    modes: &PackageModes<'source>,
    name: &str,
    position: u64,
    role: rift_core::ReferenceRole,
    work: &mut Work,
) -> Option<&'source StaticBinding> {
    if file.blocked(position, work)
        || (role == rift_core::ReferenceRole::Call && file.javascript_reassigned(name, None))
    {
        return None;
    }
    let mut best: Option<&StaticBinding> = None;
    let mut ambiguous = false;
    for &binding in file.by_name.get(name)? {
        if !work.charge() {
            return None;
        }
        if !binding.scope.contains(position) {
            continue;
        }
        let symbol = file.target_at(binding.name).map(|target| target.symbol);
        if role != rift_core::ReferenceRole::Type
            && symbol.is_some_and(|symbol| matches!(symbol.kind, "interface" | "type_alias"))
        {
            continue;
        }
        if role == rift_core::ReferenceRole::Type
            && symbol.is_some_and(|symbol| matches!(symbol.kind, "variable" | "function"))
        {
            continue;
        }
        match best {
            Some(current) if current.scope == binding.scope => ambiguous = true,
            Some(current) if !current.scope.encloses(binding.scope) => {}
            _ => {
                best = Some(binding);
                ambiguous = false;
            }
        }
    }
    let binding = best?;
    let deferred = modes
        .scopes
        .get(file.document.path.as_str())
        .and_then(|scopes| scopes.deferred.resolve(position, position.checked_add(1)?))
        .is_some_and(|function| !function.encloses(binding.name));
    let available =
        binding.available_from <= position || role == rift_core::ReferenceRole::Type || deferred;
    (!ambiguous && available).then_some(binding)
}
