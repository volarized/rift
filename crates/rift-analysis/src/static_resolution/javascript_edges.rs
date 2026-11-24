//! ECMAScript containment, access, type usage, and override facts.

use rift_core::{ReferenceRole, SourceRange, SymbolId};
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_provider::NormalizedTarget;
use rift_syntax::{
    ByteRange, StaticBinding, StaticBindingKind, StaticReference, SyntaxExportBinding,
    SyntaxExportKind, SyntaxSymbol,
};

use super::javascript::MemberMode;
use super::{File, Index, StaticOccurrence, StaticResolution, Target, Work};

impl<'source> Index<'source> {
    pub(super) fn javascript_call_from(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<SymbolId> {
        let scopes = self.javascript.scopes.get(file.document.path.as_str())?;
        let callable = scopes.callables.resolve(range.start, range.end).copied();
        let value = scopes.function_values.resolve(range.start, range.end);
        let holder = match (value, callable) {
            // A callable declared inside the function value is the nearer caller.
            (Some(value), Some(symbol))
                if value.encloses(symbol.range) && *value != symbol.range =>
            {
                None
            }
            // The call runs when the function value does. The innermost declaration
            // holding that value is the caller: the callable it defines, or the
            // variable, field or object member whose initializer builds it.
            (Some(value), _) => scopes.definitions.resolve(value.start, value.end).copied(),
            // Outside every function, an instance field initializer runs with its field.
            (None, None) => scopes
                .definitions
                .resolve(range.start, range.end)
                .copied()
                .filter(|symbol| {
                    matches!(
                        symbol.syntax_element_kind,
                        Some("field_definition" | "public_field_definition")
                    )
                }),
            (None, Some(_)) => None,
        };
        for symbol in [holder, callable].into_iter().flatten() {
            let identity = self.javascript_identity(Target {
                file: file.document,
                symbol,
            });
            if identity.is_some() {
                return identity;
            }
        }
        self.javascript_module_identity(file, work)
    }

    pub(super) fn javascript_edges(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        for reference in file.facts.references() {
            if !work.charge() {
                return;
            }
            let local = self.javascript_target(file, reference.target, reference.role, work);
            let target = local
                .filter(|target| {
                    reference.role != ReferenceRole::Type
                        || target.symbol.kinds.contains(&SymbolKind::Type)
                        || target.symbol.kinds.contains(&SymbolKind::Variant)
                })
                .or_else(|| self.javascript_merged_alias_type(file, reference, work));
            let mut occurrence = target
                .and_then(|target| {
                    self.occurrence(
                        file,
                        reference.target,
                        reference.target,
                        reference.role,
                        target,
                    )
                })
                .or_else(|| {
                    self.javascript_module_reference(file, reference.target, reference.role, work)
                });
            if let Some(occurrence) = &mut occurrence {
                occurrence.edge_from = self
                    .javascript_assignment_reference_from(
                        file,
                        reference.target,
                        reference.role,
                        work,
                    )
                    .or_else(|| self.javascript_module_from(file, reference.target, work));
            } else if local.is_none()
                && reference.role == ReferenceRole::Type
                && file.document.facts.language().name == "typescript"
                && self
                    .record_external_typescript_type_use(file, reference.target, work, result)
                    .is_some()
            {
                continue;
            }
            result.record(occurrence);
        }
        for binding in file.facts.bindings() {
            if !work.charge() {
                return;
            }
            if let StaticBindingKind::JavaScriptEdgeNamespace { module } = binding.kind {
                result.record(self.javascript_namespace_import(file, binding.name, module, work));
            }
            if let StaticBindingKind::JavaScriptImport { module, .. } = binding.kind
                && let Some(written) = self.javascript_written_import(file, binding, module, work)
            {
                // An import without a separate written module is no unresolved site.
                result.record(Some(written));
            }
            if let StaticBindingKind::JavaScriptRequire {
                module,
                export: None,
                require,
            } = binding.kind
                && self.builtin_require(file, require.start, work)
            {
                result.record(self.javascript_namespace_import(file, binding.name, module, work));
            }
        }
        for export in file.document.facts.export_bindings().unwrap_or_default() {
            if !work.charge() {
                return;
            }
            if let Some(module) = export.source {
                let text = file.text(module).unwrap_or_default();
                if text.len() < 2 {
                    continue;
                }
                let module = ByteRange {
                    start: module.start + 1,
                    end: module.end - 1,
                };
                result.record(self.javascript_reexport_module(file, export, module, work));
            }
        }
        for alias in file.facts.member_aliases() {
            if !work.charge() {
                return;
            }
            let occurrence = || {
                let source = file.target_at(alias.name)?.callable()?;
                let target = file.target_at(alias.target)?.callable()?;
                let from = self.javascript_identity(source)?;
                let mut occurrence =
                    self.occurrence(file, alias.name, alias.name, ReferenceRole::Unknown, target)?;
                occurrence.edge_from = Some(from);
                occurrence.edge_kind = Some(EdgeKind::Aliases);
                Some(occurrence)
            };
            result.record(occurrence());
        }
        self.javascript_contains(file, work, result);
    }

    /// Selects the alias facet for a direct TypeScript reference to an established
    /// alias and namespace pair in the same lexical scope.
    fn javascript_merged_alias_type(
        &self,
        file: &File<'source>,
        reference: &StaticReference,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if reference.role != ReferenceRole::Type
            || !matches!(
                file.document.path.as_str().rsplit_once('.'),
                Some((_, "ts"))
            )
            || file.members.contains_key(&reference.target)
        {
            return None;
        }
        let name = file.text(reference.target)?;
        let position = reference.target.start;
        if file.by_name.get(name)?.len() < 2 || file.blocked(position, work) {
            return None;
        }

        let mut pair: [Option<&StaticBinding>; 2] = [None, None];
        let mut additional = false;
        file.innermost_bindings(
            name,
            position,
            work,
            // Keep the fallback limited to an unshadowed pair of written declarations.
            |_| true,
            |binding, first| {
                if first {
                    pair = [Some(binding), None];
                    additional = false;
                } else if pair[1].is_none() {
                    pair[1] = Some(binding);
                } else {
                    additional = true;
                }
            },
        )?;

        let [Some(first), Some(second)] = pair else {
            return None;
        };
        if additional
            || first.kind != StaticBindingKind::Local
            || second.kind != StaticBindingKind::Local
        {
            return None;
        }
        let left = file.target_at(first.name)?;
        let right = file.target_at(second.name)?;
        let (alias, namespace) = match (
            left.symbol.syntax_element_kind,
            right.symbol.syntax_element_kind,
        ) {
            (Some("type_alias_declaration"), Some("internal_module")) => (left, right),
            (Some("internal_module"), Some("type_alias_declaration")) => (right, left),
            _ => return None,
        };
        if alias.symbol.kinds.contains(&SymbolKind::Public)
            != namespace.symbol.kinds.contains(&SymbolKind::Public)
        {
            return None;
        }
        (self.javascript_identity(alias)? == self.javascript_identity(namespace)?).then_some(alias)
    }

    fn javascript_assignment_reference_from(
        &self,
        file: &File<'source>,
        range: ByteRange,
        role: ReferenceRole,
        work: &mut Work,
    ) -> Option<SymbolId> {
        if !matches!(role, ReferenceRole::Read | ReferenceRole::Write) {
            return None;
        }
        let scopes = self.javascript.scopes.get(file.document.path.as_str())?;
        let symbol = scopes.definitions.resolve(range.start, range.end)?;
        let assignment = symbol.syntax_element_kind == Some("assignment_expression");
        // The write of `this.name = value` belongs to the code that runs it, also when the
        // assignment declares the member (D46); so do the reads of a value that is no
        // function and no object literal.
        let written = role == ReferenceRole::Write
            && symbol.name_range.is_some_and(|name| range.encloses(name));
        let read =
            role == ReferenceRole::Read && symbol.kind == "property" && symbol.body_range.is_none();
        if assignment
            && (written || read)
            && matches!(symbol.kind, "method" | "property")
            && symbol.container.is_some()
            && file
                .text(symbol.range)
                .is_some_and(|text| text.starts_with("this"))
        {
            let outside = ByteRange {
                start: symbol.range.start.checked_sub(1)?,
                end: symbol.range.end,
            };
            return self
                .javascript_call_from(file, outside, work)
                .or_else(|| self.javascript_module_from(file, outside, work));
        }
        // So does the write of `exports.name = value`, when the assignment declares the name.
        if assignment && written && matches!(symbol.kind, "variable" | "class") {
            let outside = ByteRange {
                start: symbol.range.start,
                end: symbol.range.end.checked_add(1)?,
            };
            return self
                .javascript_call_from(file, outside, work)
                .or_else(|| self.javascript_module_from(file, outside, work));
        }
        let callable = matches!(symbol.kind, "function" | "method");
        if assignment && callable {
            self.javascript_call_from(file, range, work)
        } else {
            None
        }
    }

    /// The source of an occurrence written outside every declaration of its file: the
    /// file's own module. A file can declare into another module (an ambient module
    /// declaration), so its declarations alone do not always name one module.
    pub(super) fn javascript_module_from(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<SymbolId> {
        let scopes = self.javascript.scopes.get(file.document.path.as_str())?;
        if scopes.definitions.resolve(range.start, range.end).is_some() {
            return None;
        }
        self.javascript_module_identity(file, work)
    }

    fn javascript_contains(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let module = self.javascript_module_identity(file, work);
        self.javascript_alias_edges(file, module.as_ref(), work, result);
        for symbol in file.document.facts.symbols() {
            if !work.charge() {
                return;
            }
            let target = Target {
                file: file.document,
                symbol,
            };
            let from = symbol
                .container
                .as_deref()
                .and_then(|container| {
                    file.javascript_container(container, work)
                        .or_else(|| file.qualified_target(container))
                })
                .and_then(|target| self.javascript_identity(target));
            let from = from
                .or_else(|| self.javascript_merged_alias_property_parent(file, target, work))
                .or_else(|| module.clone());
            if let Some(from) = from {
                result.record(self.javascript_declaration_edge(
                    file,
                    target,
                    from,
                    EdgeKind::Contains,
                ));
            }
            if matches!(symbol.kind, "method" | "property" | "field") {
                self.javascript_overrides(file, symbol, work, result);
            }
        }
    }

    /// A property written inside the alias facet of one local namespace and type alias.
    fn javascript_merged_alias_property_parent(
        &self,
        file: &File<'source>,
        child: Target<'source>,
        work: &mut Work,
    ) -> Option<SymbolId> {
        if child.symbol.syntax_element_kind != Some("property_signature") {
            return None;
        }
        let [left, right] = file
            .by_qualified_name
            .get(child.symbol.container.as_deref()?)?
            .as_slice()
        else {
            return None;
        };
        if !work.charge() || !work.charge() {
            return None;
        }
        let (alias, namespace) = match (left.syntax_element_kind, right.syntax_element_kind) {
            (Some("type_alias_declaration"), Some("internal_module")) => (*left, *right),
            (Some("internal_module"), Some("type_alias_declaration")) => (*right, *left),
            _ => return None,
        };
        if alias.container != namespace.container
            || !alias.item_range.encloses(child.symbol.item_range)
            || (alias.item_range.start < namespace.item_range.end
                && namespace.item_range.start < alias.item_range.end)
        {
            return None;
        }
        let target = |symbol| Target {
            file: file.document,
            symbol,
        };
        let identity = self.javascript_identity(target(alias))?;
        (self.javascript_identity(target(namespace))? == identity
            && self.javascript_identity(child)? != identity)
            .then_some(identity)
    }

    fn javascript_module_reference(
        &self,
        file: &File<'source>,
        range: ByteRange,
        role: ReferenceRole,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        if role != ReferenceRole::Read {
            return None;
        }
        let (name, member) = file.text(range)?.split_once('.')?;
        let binding = super::javascript::js_binding(
            file,
            &self.javascript,
            name.trim(),
            range.start,
            role,
            work,
        )?;
        let resolver = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
        };
        let value = match binding.kind {
            StaticBindingKind::JavaScriptEdgeNamespace { module } => resolver.resolve(
                file,
                file.text(module)?,
                member.trim(),
                super::javascript::ModuleMode::Esm,
                role,
                work,
            )?,
            StaticBindingKind::JavaScriptImport { module, export } => {
                let name = export
                    .and_then(|range| file.text(range))
                    .unwrap_or("default");
                let super::javascript_exports::ResolvedExport::Module(imported) = resolver
                    .resolve(
                        file,
                        file.text(module)?,
                        name,
                        super::javascript::ModuleMode::Esm,
                        role,
                        work,
                    )?
                else {
                    return None;
                };
                resolver.resolve_file(
                    self.files.get(imported.path.as_str())?,
                    member.trim(),
                    super::javascript::ModuleMode::Esm,
                    role,
                    work,
                )?
            }
            _ => return None,
        };
        let super::javascript_exports::ResolvedExport::Module(imported) = value else {
            return None;
        };
        self.javascript_module_occurrence(
            file,
            range,
            self.files.get(imported.path.as_str())?,
            role,
            work,
        )
    }

    fn javascript_alias_edges(
        &self,
        file: &File<'source>,
        module: Option<&SymbolId>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let Some(source_digest) = file.document.facts.source_digest().copied() else {
            return;
        };
        for alias in file.document.placement.aliases() {
            if !work.charge() {
                return;
            }
            let Ok(range) = SourceRange::new(alias.range.start, alias.range.end) else {
                continue;
            };
            if let Some(module) = module {
                result.record(Some(StaticOccurrence {
                    binding: rift_core::DeclarationBinding::new(
                        file.document.placement.unit().clone(),
                        range,
                    ),
                    target: NormalizedTarget::Symbol(alias.identity.clone()),
                    target_binding: Some(rift_core::DeclarationBinding::new(
                        file.document.placement.unit().clone(),
                        range,
                    )),
                    source_digest,
                    callee_range: alias.range,
                    role: ReferenceRole::Unknown,
                    edge_from: Some(module.clone()),
                    edge_kind: Some(EdgeKind::Contains),
                }));
            }
            let Ok(target) = rift_core::parse_symbol_identity(&alias.target_identity) else {
                continue;
            };
            if target.qualified_name() != "." {
                continue;
            }
            let imported = self.files.values().find(|candidate| {
                work.charge() && candidate.document.placement.identity_path() == target.path()
            });
            let Some(imported) = imported else {
                continue;
            };
            let mut occurrence = self.javascript_module_occurrence(
                file,
                alias.range,
                imported,
                ReferenceRole::Unknown,
                work,
            );
            if let Some(occurrence) = &mut occurrence {
                occurrence.edge_kind = Some(EdgeKind::Aliases);
                occurrence.edge_from = Some(alias.identity.clone());
            }
            result.record(occurrence);
        }
    }

    fn javascript_declaration_edge(
        &self,
        file: &File<'source>,
        target: Target<'source>,
        from: SymbolId,
        kind: EdgeKind,
    ) -> Option<StaticOccurrence> {
        let range = target.symbol.name_range.unwrap_or(target.symbol.item_range);
        let mut occurrence = self.occurrence(file, range, range, ReferenceRole::Unknown, target)?;
        occurrence.edge_kind = Some(kind);
        occurrence.edge_from = Some(from);
        Some(occurrence)
    }

    fn javascript_namespace_import(
        &self,
        file: &File<'source>,
        range: ByteRange,
        module: ByteRange,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let path = super::javascript_exports::relative_path(
            file.document.path.as_str(),
            file.text(module)?,
            work,
        )?;
        let imported = self.edge_javascript_module(&path)?;
        self.javascript_import_module_occurrence(file, range, imported, work)
    }

    fn javascript_reexport_module(
        &self,
        file: &File<'source>,
        export: &SyntaxExportBinding,
        module: ByteRange,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        if export.kind != SyntaxExportKind::All
            || !export.type_only
            || export.container.is_some()
            || !file.document.path.as_str().ends_with(".d.ts")
        {
            return self.javascript_namespace_import(file, export.range, module, work);
        }
        let path = super::javascript_exports::relative_path(
            file.document.path.as_str(),
            file.text(module)?,
            work,
        )?;
        let imported = super::javascript_exports::module_file(&self.files, &path, true)?;
        self.javascript_import_module_occurrence(file, export.range, imported, work)
    }

    fn javascript_import_module_occurrence(
        &self,
        file: &File<'source>,
        range: ByteRange,
        imported: &File<'source>,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let mut occurrence =
            self.javascript_module_occurrence(file, range, imported, ReferenceRole::Import, work)?;
        occurrence.edge_from = self.javascript_module_identity(file, work);
        Some(occurrence)
    }

    /// The written module of a reexport or a relative declaration-only import.
    ///
    /// The terminal declaration is stored by the import itself. A name reached through a
    /// reexport also keeps the module the import is written against. Type resolution
    /// through a declaration-only module keeps the newly admitted module as well.
    fn javascript_written_import(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        module: ByteRange,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        if file.inactive(binding.name, work) {
            return None;
        }
        let terminal = self.javascript_import(file, binding, work)?;
        let path = super::javascript_exports::relative_path(
            file.document.path.as_str(),
            file.text(module)?,
            work,
        )?;
        let written = match self.edge_javascript_module(&path) {
            Some(written) if written.document.path == terminal.file.path => return None,
            Some(written) => written,
            None if file.document.path.as_str().ends_with(".d.ts") => {
                super::javascript_exports::module_file(&self.files, &path, true)?
            }
            None => return None,
        };
        self.javascript_import_module_occurrence(file, binding.name, written, work)
    }

    fn javascript_module_occurrence(
        &self,
        file: &File<'source>,
        range: ByteRange,
        imported: &File<'source>,
        role: ReferenceRole,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let identity = self.javascript_module_identity(imported, work)?;
        let source_range = SourceRange::new(range.start, range.end).ok()?;
        Some(StaticOccurrence {
            binding: rift_core::DeclarationBinding::new(
                file.document.placement.unit().clone(),
                source_range,
            ),
            target: NormalizedTarget::Symbol(identity),
            target_binding: None,
            source_digest: *file.document.facts.source_digest()?,
            callee_range: range,
            role,
            edge_kind: None,
            edge_from: None,
        })
    }

    fn javascript_identity(&self, target: Target<'source>) -> Option<SymbolId> {
        let file = self.files.get(target.file.path.as_str())?;
        let range = target.symbol.name_range.unwrap_or(target.symbol.item_range);
        match self
            .occurrence(file, range, range, ReferenceRole::Unknown, target)?
            .target
        {
            NormalizedTarget::Symbol(identity) => Some(identity),
            NormalizedTarget::Contribution(_) => None,
        }
    }

    fn javascript_module_identity(
        &self,
        file: &File<'source>,
        work: &mut Work,
    ) -> Option<SymbolId> {
        if let Some(identity) = file.document.placement.logical_identity(".") {
            return Some(identity.clone());
        }
        for symbol in file.document.facts.symbols() {
            if !work.charge() {
                return None;
            }
            if symbol.container.is_some() {
                continue;
            }
            let target = Target {
                file: file.document,
                symbol,
            };
            let Some(identity) = self.javascript_identity(target) else {
                continue;
            };
            let parsed = SymbolIdentity::parse(identity.as_str()).ok()?;
            let mut path = parsed.qualified_path().to_vec();
            path.pop()?;
            if path.is_empty() {
                // An ambient module declaration is named outside the file's module.
                continue;
            }
            let module =
                SymbolIdentity::new(parsed.owner().clone(), parsed.language().clone(), path)
                    .ok()?;
            return SymbolId::new(module.wire_identity()).ok();
        }
        if !self.javascript.owner.is_local() {
            // A package names a module by its entry or package path, never by `./`.
            return None;
        }
        let module = SymbolIdentity::new(
            self.javascript.owner.clone(),
            file.document.facts.language().clone(),
            vec![format!("./{}", file.document.path.as_str())],
        )
        .ok()?;
        SymbolId::new(module.wire_identity()).ok()
    }

    fn javascript_overrides(
        &self,
        file: &File<'source>,
        symbol: &'source SyntaxSymbol,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let Some(class) = symbol.container.as_deref().and_then(|container| {
            file.javascript_container(container, work)
                .or_else(|| file.qualified_target(container))
        }) else {
            return;
        };
        if !matches!(class.symbol.kind, "class" | "interface")
            || !class.symbol.range.encloses(symbol.range)
        {
            return;
        }
        // A constructor is invoked through `super`, never inherited and replaced. A member
        // assigned through `this` writes the inherited member; it declares no override.
        if symbol.name == "constructor"
            || symbol.syntax_element_kind == Some("assignment_expression")
        {
            return;
        }
        let Some(from) = self.javascript_identity(Target {
            file: file.document,
            symbol,
        }) else {
            return;
        };
        let mode = if symbol.kinds.contains(&SymbolKind::Static) {
            MemberMode::Static
        } else {
            MemberMode::Instance
        };
        for edge in file.facts.heritage_edges() {
            if !work.charge() {
                return;
            }
            // An implemented interface is fulfilled, not inherited: only `extends` passes
            // a member down for the declaration to replace.
            if Some(edge.from) != class.symbol.name_range || edge.kind != EdgeKind::Extends {
                continue;
            }
            let base = self.edge_javascript_target(file, edge.target, work);
            let target = base.and_then(|base| {
                self.javascript_class_member(base, &symbol.name, mode, ReferenceRole::Type, work)
            });
            if let Some(target) = target {
                let range = symbol.name_range.unwrap_or(symbol.item_range);
                let mut occurrence =
                    self.occurrence(file, range, range, ReferenceRole::Unknown, target);
                if let Some(occurrence) = &mut occurrence {
                    occurrence.edge_kind = Some(EdgeKind::Overrides);
                    occurrence.edge_from = Some(from.clone());
                }
                result.record(occurrence);
            }
        }
    }
}
