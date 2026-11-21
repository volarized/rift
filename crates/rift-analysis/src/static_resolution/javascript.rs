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

/// Whether the file is JavaScript source, whose types documentation comments write.
pub(super) fn javascript_source(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| {
            ["js", "cjs", "mjs", "jsx"]
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
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

/// Whether `new` can construct the target: in JavaScript every function written with the
/// `function` keyword is a constructor; an arrow function, a method and a generator are not.
fn javascript_constructor_function(target: Target<'_>) -> bool {
    javascript_source(target.file.path.as_str())
        && target.symbol.kind == "function"
        && matches!(
            target.symbol.syntax_element_kind,
            Some("function_declaration" | "function_expression" | "assignment_expression")
        )
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
            if role != rift_core::ReferenceRole::Type && file.callbacks.is_call(member.receiver) {
                let owner = self.javascript_call_result(file, member.receiver, work)?;
                return self.javascript_class_member(owner, name, MemberMode::Instance, role, work);
            }
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
        // A receiver with a declared type keeps it when the binding is assigned again: the
        // call is refused only for a callee named by the binding itself.
        let binding =
            js_binding(file, &self.javascript, receiver, position, role, work).or_else(|| {
                (role == rift_core::ReferenceRole::Call)
                    .then(|| {
                        js_binding(
                            file,
                            &self.javascript,
                            receiver,
                            position,
                            rift_core::ReferenceRole::Read,
                            work,
                        )
                    })
                    .flatten()
                    .filter(|binding| {
                        file.declared_types
                            .get(&binding.name)
                            .copied()
                            .flatten()
                            .is_some()
                    })
            });
        match binding {
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
            StaticCallee::Member { receiver, member } if file.callbacks.is_call(receiver) => {
                let owner = self.javascript_call_result(file, receiver, work)?;
                self.javascript_class_member(
                    owner,
                    file.text(member)?,
                    MemberMode::Instance,
                    rift_core::ReferenceRole::Call,
                    work,
                )?
            }
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
                || javascript_constructor_function(target)
                || self.javascript_declared_signature(target, true, work))
            .then_some(target);
        }
        target
            .callable()
            .or_else(|| {
                self.javascript_declared_signature(target, false, work)
                    .then_some(target)
            })
            .or_else(|| {
                self.javascript_callable_result(target, work)
                    .then_some(target)
            })
            .or_else(|| {
                self.javascript_shorthand_callable(target, work)
                    .then_some(target)
            })
    }

    /// Whether a shorthand object member (`{ verify }`) holds a callable value: the name it
    /// is written with resolves, in the member's own file, to a callable declaration. The
    /// call targets the member itself.
    fn javascript_shorthand_callable(&self, target: Target<'source>, work: &mut Work) -> bool {
        if target.symbol.syntax_element_kind != Some("shorthand_property_identifier") {
            return false;
        }
        let Some((file, name)) = self
            .files
            .get(target.file.path.as_str())
            .zip(target.symbol.name_range)
        else {
            return false;
        };
        work.javascript_lookup(|work| {
            let binding = js_binding(
                file,
                &self.javascript,
                file.text(name)?,
                name.start,
                rift_core::ReferenceRole::Call,
                work,
            )?;
            let value = self.javascript_binding_target(
                file,
                binding,
                rift_core::ReferenceRole::Call,
                work,
            )?;
            value.callable().map(|_| ())
        })
        .is_some()
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
        let Some(file) = self.files.get(target.file.path.as_str()) else {
            return false;
        };
        // A declaration written with a function type, `run: () => number`, is callable.
        if !constructs
            && target
                .symbol
                .name_range
                .is_some_and(|name| file.callbacks.has_function_type(name))
        {
            return true;
        }
        let Some(annotation) = target
            .symbol
            .name_range
            .and_then(|name| file.declared_types.get(&name).copied().flatten())
        else {
            return false;
        };
        self.javascript_written_signature(file, annotation, constructs, work)
    }

    /// Whether the written type `annotation` names, through at most
    /// `DECLARED_TYPE_DEPTH_MAX` aliases, a type whose own shape is a call or construct
    /// signature.
    pub(super) fn javascript_written_signature<'index>(
        &'index self,
        mut file: &'index File<'source>,
        mut annotation: rift_syntax::ByteRange,
        constructs: bool,
        work: &mut Work,
    ) -> bool {
        let declared = |file: &File<'source>, name: rift_syntax::ByteRange| {
            file.declared_types.get(&name).copied().flatten()
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
            StaticBindingKind::JavaScriptTypeImport { module, export }
                if role == rift_core::ReferenceRole::Type =>
            {
                let mode = match self.javascript.mode(file.document.path.as_str(), work) {
                    ModuleMode::CommonJs => ModuleMode::CommonJs,
                    _ => ModuleMode::Esm,
                };
                self.javascript_export(
                    file,
                    file.text(module)?,
                    file.text(export)?,
                    mode,
                    role,
                    work,
                )
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
            let module = file.text(module)?;
            let export = file.text(export)?;
            // `const { name } = require(module)` reads what `require(module).name` reads.
            return self
                .javascript_export(file, module, export, ModuleMode::CommonJs, role, work)
                .or_else(|| self.javascript_module_value_member(file, module, export, role, work));
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
                    self.javascript_destructured_name(file, binding, work)
                        .and_then(|owner| {
                            self.javascript_class_member(
                                owner,
                                member,
                                MemberMode::Instance,
                                role,
                                work,
                            )
                        })
                        .or_else(|| self.javascript_returned_member(file, binding, member, work))
                }),
            StaticBindingKind::JavaScriptRequire {
                module,
                export: None,
                require,
            } if self.builtin_require(file, require.start, work) => {
                let module = file.text(module)?;
                self.javascript_export(file, module, member, ModuleMode::CommonJs, role, work)
                    .or_else(|| {
                        self.javascript_module_value_member(file, module, member, role, work)
                    })
            }
            _ => None,
        }
    }

    fn javascript_imported_member(
        &self,
    /// A member of a required module's value: `module.exports = name` with `name` an object
    /// declared in that module, or `module.exports = { ... }`.
    fn javascript_module_value_member(
        &self,
        file: &File<'source>,
        module: &'source str,
        member: &'source str,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let value = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
        }
        .resolve_module_value(file, module, role, work)?;
        let target = self.javascript_export_value(value, role, work)?;
        self.javascript_object_member(target, member, role, work)
    }

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
        builtin_commonjs_name(file, &self.javascript, name, position, work)
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
    /// Whether an assignment writes `binding` with a value that may not be callable: a write
    /// of its name inside its scope that no inner binding of the same name takes, whose
    /// value is not a function written in place. A write whose binding is ambiguous counts.
    pub(super) fn javascript_binding_written(
        &self,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> bool {
        let Some(name) = self.text(binding.name) else {
            return true;
        };
        let Some(writes) = self.assignments.name_writes.get(name) else {
            return false;
        };
        let bindings = self.by_name.get(name).map_or(&[][..], Vec::as_slice);
        writes.iter().any(|&start| {
            // A function written in place keeps the binding callable (D35).
            if !binding.scope.contains(start) || self.function_writes.contains(&start) {
                return false;
            }
            // The innermost scope holding the write decides which binding it writes.
            let mut innermost: Option<rift_syntax::ByteRange> = None;
            let mut takes = false;
            for candidate in bindings {
                if !work.charge() {
                    return true;
                }
                if !candidate.scope.contains(start) {
                    continue;
                }
                match innermost {
                    Some(scope) if scope == candidate.scope => {
                        takes |= candidate.name == binding.name;
                    }
                    Some(scope) if !scope.encloses(candidate.scope) => {}
                    _ => {
                        innermost = Some(candidate.scope);
                        takes = candidate.name == binding.name;
                    }
                }
            }
            takes
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

/// Whether `name` at `position` is the `CommonJS` builtin: the file runs as `CommonJS` and
/// no binding in scope takes the name.
pub(super) fn builtin_commonjs_name<'source>(
    file: &File<'source>,
    modes: &PackageModes<'source>,
    name: &str,
    position: u64,
    work: &mut Work,
) -> bool {
    (modes.mode(file.document.path.as_str(), work) == ModuleMode::CommonJs
        || typescript_commonjs(file.document.path.as_str()))
        && !file.blocked(position, work)
        && matches!(
            file.binding(name, position, |_| true, work),
            Binding::Missing
        )
        && !file.javascript_reassigned(name, None)
}

pub(super) fn js_binding<'source>(
    file: &File<'source>,
    modes: &PackageModes<'source>,
    name: &str,
    position: u64,
    role: rift_core::ReferenceRole,
    work: &mut Work,
) -> Option<&'source StaticBinding> {
    if file.blocked(position, work) {
        return None;
    }
    let mut best: Option<&StaticBinding> = None;
    let mut ambiguous = false;
    let bound = file.innermost_bindings(
        name,
        position,
        work,
        |binding| {
            // A type a documentation comment imports names no value.
            if role != rift_core::ReferenceRole::Type
                && matches!(binding.kind, StaticBindingKind::JavaScriptTypeImport { .. })
            {
                return false;
            }
            let symbol = file.target_at(binding.name).map(|target| target.symbol);
            if role == rift_core::ReferenceRole::Type {
                // A JavaScript type names a class value; a name bound by `require` can hold one.
                let required_value =
                let javascript = javascript_source(file.document.path.as_str());
                    matches!(binding.kind, StaticBindingKind::JavaScriptRequire { .. })
                        && javascript;
                // A TypeScript function names no type: an interface of the same name does.
                required_value
                    || symbol.is_none_or(|symbol| {
                        symbol.kind != "variable" && (javascript || symbol.kind != "function")
                    })
            } else {
                symbol.is_none_or(|symbol| !matches!(symbol.kind, "interface" | "type_alias"))
            }
        },
        |binding, first| {
            ambiguous = !first;
            if first {
                best = Some(binding);
            }
        },
    )?;
    if !bound {
        return None;
    }
    let binding = best?;
    let deferred = modes
        .scopes
        .get(file.document.path.as_str())
        .and_then(|scopes| scopes.deferred.resolve(position, position.checked_add(1)?))
        .is_some_and(|function| !function.encloses(binding.name));
    let available =
        binding.available_from <= position || role == rift_core::ReferenceRole::Type || deferred;
    let written =
        role == rift_core::ReferenceRole::Call && file.javascript_binding_written(binding, work);
    (!ambiguous && available && !written).then_some(binding)
}
