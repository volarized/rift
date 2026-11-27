//! ECMAScript containment, access, type usage, and override facts.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{ReferenceRole, SourceLocation, SourceRange, SymbolId};
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_provider::NormalizedTarget;
use rift_syntax::{
    ByteRange, StaticBinding, StaticBindingKind, StaticCall, StaticCallee, StaticReference,
    SyntaxExportBinding, SyntaxExportKind, SyntaxSource, SyntaxSymbol, TypeScriptDialect,
    TypeScriptSyntaxProvider,
};

use super::javascript::{AssignedMemberSite, MemberMode, ModuleMode, js_binding};
use super::javascript_exports::{self, is_declaration_file, matches_export_form};
use super::javascript_parameters::ParameterReads;
use super::{File, Index, StaticOccurrence, StaticResolution, Target, Work};

impl<'source> Index<'source> {
    /// References named selectors after admitting the existing static facts.
    pub(super) fn resolve_typescript_local_exports(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        'files: for file in self.files.values() {
            let facts = file.document.facts;
            if facts.language().name != "typescript" || !facts.declaration_syntax_complete() {
                continue;
            }
            for export in facts.export_bindings().unwrap_or_default() {
                if !work.charge() {
                    break 'files;
                }
                if let Some((reference, _)) =
                    self.typescript_local_export_reference(file, export, work)
                {
                    result.record(Some(reference));
                }
            }
        }
        result.typescript_local_export_start = (result.occurrences.len() != start).then_some(start);
    }

    /// Checks local selectors independently in declaration files with parser recovery.
    pub(super) fn resolve_incomplete_typescript_local_exports(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        let provider = TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript);
        for file in self.files.values() {
            let facts = file.document.facts;
            if facts.language().name != "typescript"
                || facts.declaration_syntax_complete()
                || !is_declaration_file(file.document.path.as_str())
            {
                continue;
            }
            let Some(limits) = facts.syntax_limits() else {
                continue;
            };
            let mut candidates = Vec::new();
            let mut references = Vec::new();
            for export in facts.export_bindings().unwrap_or_default() {
                if !work.charge() {
                    break;
                }
                if let Some((reference, target)) =
                    self.typescript_local_export_reference(file, export, work)
                {
                    candidates.push((export, target));
                    references.push(reference);
                }
            }
            if !candidates.is_empty() {
                let source = SyntaxSource {
                    path: file.document.path,
                    text: file.document.source,
                };
                if let Ok(checked) =
                    provider.checked_local_export_selectors(source, limits, &candidates)
                {
                    result.occurrences.extend(
                        references
                            .into_iter()
                            .filter(|reference| checked.contains(&reference.callee_range)),
                    );
                }
            }
            if work.exhausted {
                break;
            }
        }
        result.typescript_incomplete_local_export_start =
            (result.occurrences.len() != start).then_some(start);
    }

    fn typescript_local_export_reference(
        &self,
        file: &File<'source>,
        export: &SyntaxExportBinding,
        work: &mut Work,
    ) -> Option<(StaticOccurrence, &'source SyntaxSymbol)> {
        let range = export.local?;
        if export.kind != SyntaxExportKind::Named
            || export.source.is_some()
            || export.container.is_some()
            || file.by_range.contains_key(&range)
            || !matches_export_form(file.text(export.range)?, ModuleMode::Esm)
        {
            return None;
        }
        let name = file.text(range)?;
        let target = file.qualified_target(name)?;
        let written = target.symbol.name_range?;
        let is_type = target.symbol.kinds.contains(&SymbolKind::Type);
        if target.symbol.container.is_some() || written == range || (export.type_only && !is_type) {
            return None;
        }
        let role =
            if is_type && (export.type_only || !target.symbol.kinds.contains(&SymbolKind::Value)) {
                ReferenceRole::Type
            } else {
                ReferenceRole::Read
            };
        let local = js_binding(file, &self.javascript, name, range.start, role, work)
            .is_some_and(|binding| binding.name == written);
        let signature = role == ReferenceRole::Read
            && file.declared_signature(name) == Some(written)
            && !file.blocked(range.start, work);
        if !(local || signature) {
            return None;
        }
        let owner = self.javascript_module_from(file, range, work)?;
        let mut occurrence = self.occurrence(file, range, range, ReferenceRole::Unknown, target)?;
        occurrence.edge_from = Some(owner);
        Some((occurrence, target.symbol))
    }

    /// Retains selectors of already resolved relative imports after established edges.
    pub(super) fn resolve_typescript_imported_export_selectors(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        'files: for file in self.files.values().filter(|file| {
            file.document.facts.language().name == "typescript"
                && file.document.facts.declaration_syntax_complete()
                && self.javascript_has_file_module(file)
        }) {
            for export in file.document.facts.export_bindings().unwrap_or_default() {
                if !work.charge() {
                    break 'files;
                }
                if let Some(reference) =
                    self.typescript_imported_export_selector(file, export, work)
                {
                    result.record(Some(reference));
                }
            }
        }
        result.typescript_imported_export_start =
            (result.occurrences.len() != start).then_some(start);
    }

    fn typescript_imported_export_selector(
        &self,
        file: &File<'source>,
        export: &SyntaxExportBinding,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let range = export.local?;
        if export.kind != SyntaxExportKind::Named
            || export.source.is_some()
            || export.container.is_some()
            || !matches_export_form(file.text(export.range)?, ModuleMode::Esm)
        {
            return None;
        }
        let name = file.text(range)?;
        let typed = js_binding(
            file,
            &self.javascript,
            name,
            range.start,
            ReferenceRole::Type,
            work,
        );
        let read = (!export.type_only)
            .then(|| {
                js_binding(
                    file,
                    &self.javascript,
                    name,
                    range.start,
                    ReferenceRole::Read,
                    work,
                )
            })
            .flatten();
        if typed.zip(read).is_some_and(|(typed, read)| typed != read) {
            return None;
        }
        let binding = typed.or(read)?;
        let StaticBindingKind::JavaScriptImport { module, .. } = binding.kind else {
            return None;
        };
        let specifier = file.text(module)?;
        if !(specifier.starts_with("./") || specifier.starts_with("../")) {
            return None;
        }
        let destination = self.javascript_specifier_file(file, module, false, work)?;
        let type_target =
            typed.and_then(|_| self.javascript_target(file, range, ReferenceRole::Type, work));
        let value_target =
            read.and_then(|_| self.javascript_target(file, range, ReferenceRole::Read, work));
        if let Some((left, right)) = type_target.zip(value_target)
            && self.javascript_identity(left)? != self.javascript_identity(right)?
        {
            return None;
        }
        let target = type_target.or(value_target)?;
        if target.file.path != destination.document.path
            || (export.type_only && !target.symbol.kinds.contains(&SymbolKind::Type))
        {
            return None;
        }
        let owner = self.javascript_module_from(file, range, work)?;
        let mut occurrence = self.occurrence(file, range, range, ReferenceRole::Unknown, target)?;
        occurrence.edge_from = Some(owner);
        Some(occurrence)
    }

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
            // Outside every function, fields own their initializer calls. In package
            // facts, variable and literal property initializers also name their holder.
            (None, None) => scopes
                .definitions
                .resolve(range.start, range.end)
                .copied()
                .filter(|symbol| match symbol.syntax_element_kind {
                    Some("field_definition" | "public_field_definition") => true,
                    Some("variable_declarator" | "pair") => {
                        matches!(
                            file.document.placement.origin().location(),
                            Some(SourceLocation::Dependency { .. })
                        ) && symbol
                            .name_range
                            .is_some_and(|name| name.end <= range.start)
                    }
                    _ => false,
                }),
            (None, Some(_)) => None,
        };
        for symbol in [holder, callable].into_iter().flatten() {
            let target = Target {
                file: file.document,
                symbol,
            };
            if target.is_scoped_javascript_lexical_binding() {
                continue;
            }
            let identity = self.javascript_identity(target);
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
        parameter_reads: &mut ParameterReads,
    ) {
        self.javascript_references(file, work, result, parameter_reads);
        if work.exhausted {
            return;
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

    fn javascript_references(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
        parameter_reads: &mut ParameterReads,
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
            if target.is_some_and(Target::is_scoped_javascript_lexical_binding)
                && let Some(occurrence) = occurrence
            {
                result.deferred_scoped_javascript_lexical.push(occurrence);
                continue;
            }
            if occurrence.is_none() && reference.role == ReferenceRole::Read {
                file.record_parameter_read(reference.target, parameter_reads);
                if !file.facts.assigned_calls().is_empty()
                    && file.members.contains_key(&reference.target)
                {
                    result
                        .javascript_assigned_members
                        .entry(file.document.placement.unit().clone())
                        .or_default()
                        .push(AssignedMemberSite::Read(reference.target));
                }
            }
            if occurrence.is_none()
                && reference.role == ReferenceRole::Type
                && file.document.facts.language().name == "typescript"
                && file.document.facts.declaration_syntax_complete()
                && !file.members.contains_key(&reference.target)
                && file
                    .text(reference.target)
                    .is_some_and(|name| file.by_name.contains_key(name))
            {
                result
                    .typescript_namespace_type_sites
                    .entry(file.document.placement.unit().clone())
                    .or_default()
                    .push(reference.target);
            }
            result.record(occurrence);
        }
    }

    /// Resolves a plain type name when a checked type and namespace share its identity.
    pub(super) fn resolve_typescript_namespace_type_uses(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        let pending = std::mem::take(&mut result.typescript_namespace_type_sites);
        'files: for file in self.files.values() {
            let Some(sites) = pending.get(file.document.placement.unit()) else {
                continue;
            };
            for &range in sites {
                if !work.charge() {
                    break 'files;
                }
                if let Some(occurrence) = self.typescript_namespace_type_use(file, range, work) {
                    result.unresolved_sites = result.unresolved_sites.saturating_sub(1);
                    result.record(Some(occurrence));
                }
            }
        }
        result.typescript_namespace_type_start =
            (result.occurrences.len() > start).then_some(start);
    }

    fn typescript_namespace_type_use(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        if file.blocked(range.start, work) {
            return None;
        }
        let name = file.text(range)?;
        let pair = if let Some(binding) = js_binding(
            file,
            &self.javascript,
            name,
            range.start,
            ReferenceRole::Type,
            work,
        ) {
            let StaticBindingKind::JavaScriptImport {
                module,
                export: Some(export),
            } = binding.kind
            else {
                return None;
            };
            if self.javascript.mode(file.document.path.as_str(), work) == ModuleMode::CommonJs {
                return None;
            }
            let specifier = file.text(module)?;
            if !(specifier.starts_with("./") || specifier.starts_with("../")) {
                return None;
            }
            self.javascript_specifier_file(file, module, false, work)?
                .typescript_direct_namespace_export_pair(file.text(export)?, work)?
        } else {
            file.typescript_local_namespace_pair(name, range.start, work, |kind| {
                matches!(
                    kind,
                    StaticBindingKind::Local | StaticBindingKind::JavaScriptClassValue { .. }
                )
            })?
        };
        let [left, right] = pair;
        let (typed, namespace) = match (
            left.symbol.syntax_element_kind,
            right.symbol.syntax_element_kind,
        ) {
            (_, Some("internal_module")) => (left, right),
            (Some("internal_module"), _) => (right, left),
            _ => return None,
        };
        if !matches!(
            typed.symbol.syntax_element_kind,
            Some(
                "interface_declaration"
                    | "type_alias_declaration"
                    | "class_declaration"
                    | "enum_declaration"
            )
        ) || !typed.symbol.kinds.contains(&SymbolKind::Type)
            || typed.symbol.kinds.contains(&SymbolKind::Public)
                != namespace.symbol.kinds.contains(&SymbolKind::Public)
            || self.javascript_identity(typed)? != self.javascript_identity(namespace)?
        {
            return None;
        }
        let mut occurrence = self.occurrence(file, range, range, ReferenceRole::Type, typed)?;
        occurrence.edge_from = self
            .javascript_assignment_reference_from(file, range, ReferenceRole::Type, work)
            .or_else(|| self.javascript_module_from(file, range, work));
        Some(occurrence)
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

        let [left, right] = file.typescript_local_namespace_pair(name, position, work, |kind| {
            kind == StaticBindingKind::Local
        })?;
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
            if symbol.is_ecmascript_package_binding() {
                continue;
            }
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
                let occurrence =
                    self.javascript_declaration_edge(file, target, from, EdgeKind::Contains);
                if let Some(occurrence) = occurrence {
                    if target.is_scoped_javascript_lexical_binding() {
                        result.deferred_scoped_javascript_lexical.push(occurrence);
                    } else if target.is_physical_only_javascript_package_symbol() {
                        result.deferred_literal_containment.push(occurrence);
                    } else if target.is_scoped_typescript_union_property() {
                        result.deferred_typescript_union_properties.push(occurrence);
                    } else {
                        result.record(Some(occurrence));
                    }
                } else {
                    result.record(None);
                }
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
            names: None,
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
        let imported =
            super::javascript_exports::module_file(&self.files, &self.javascript, &path, true)?;
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
                super::javascript_exports::module_file(&self.files, &self.javascript, &path, true)?
            }
            None => return None,
        };
        self.javascript_import_module_occurrence(file, binding.name, written, work)
    }

    pub(super) fn javascript_module_occurrence(
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

    pub(super) fn javascript_identity(&self, target: Target<'source>) -> Option<SymbolId> {
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
            if symbol.is_ecmascript_package_binding() {
                continue;
            }
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
            let base = self.javascript_heritage_target(file, edge, work);
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

impl<'source> Index<'source> {
    pub(super) fn javascript_has_file_module(&self, file: &File<'source>) -> bool {
        matches!(
            &self.javascript.owner,
            rift_protocol::identity::SymbolOwner::Package { manager, .. } if manager == "npm"
        ) && file.document.placement.logical_identity(".").is_some()
            && !file.document.facts.has_errors()
    }

    /// Adds checked literal requires occupying an immediate argument expression.
    pub(super) fn resolve_commonjs_argument_imports(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        'files: for file in self.files.values().filter(|file| {
            file.document.facts.language().name == "javascript"
                && self.javascript_has_file_module(file)
                && file.document.facts.declaration_syntax_complete()
        }) {
            let mut sites = BTreeSet::new();
            for reference in file.facts.references().iter().filter(|reference| {
                reference.role == ReferenceRole::Import
                    && commonjs_argument_call(file, reference.target)
            }) {
                if !work.charge() {
                    break 'files;
                }
                sites.insert(reference.target);
            }
            if sites.is_empty() {
                continue;
            }
            for call in file.facts.calls() {
                if !work.charge() {
                    break 'files;
                }
                if sites.remove(&call.range)
                    && let Some(module) = Self::commonjs_literal_require(file, call)
                    && self.builtin_require(file, call.callee_range.start, work)
                    && let Some(imported) =
                        self.javascript_module_specifier(file, module, true, work)
                {
                    result.record(Some(imported));
                }
                if sites.is_empty() {
                    break;
                }
            }
        }
        result.commonjs_argument_import_start = (result.occurrences.len() > start).then_some(start);
    }

    /// Retains literal requires written directly in a checked `CommonJS` export object.
    pub(super) fn resolve_commonjs_export_imports(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        result.commonjs_export_import_start = Some(result.occurrences.len());
        for file in self.files.values().filter(|file| {
            file.document.facts.language().name == "javascript"
                && self.javascript_has_file_module(file)
                && file.document.facts.declaration_syntax_complete()
        }) {
            let Some(export) = file.facts.commonjs_export() else {
                continue;
            };
            let Some(literal) = export.literal else {
                continue;
            };
            let mut pairs = BTreeSet::new();
            for member in file.facts.body_members() {
                if !work.charge() {
                    return;
                }
                if member.body == literal && member.holder == export.assignment {
                    pairs.insert(member.member);
                }
            }
            if pairs.is_empty() {
                continue;
            }
            let mut calls = BTreeMap::new();
            for call in file.facts.calls() {
                if !work.charge() {
                    return;
                }
                if literal.encloses(call.range) {
                    calls
                        .entry(call.range)
                        .and_modify(|entry| *entry = None)
                        .or_insert(Some(call));
                }
            }
            for symbol in file.document.facts.symbols() {
                if !work.charge() {
                    return;
                }
                if symbol.syntax_element_kind == Some("pair")
                    && pairs.contains(&symbol.item_range)
                    && let Some(Some(call)) = symbol.body_range.and_then(|body| calls.get(&body))
                    && let Some(module) = Self::commonjs_literal_require(file, call)
                    && self.builtin_require(file, call.callee_range.start, work)
                    && let Some(imported) =
                        self.javascript_module_specifier(file, module, true, work)
                {
                    result.record(Some(imported));
                }
            }
        }
    }

    /// The unquoted relative specifier in a captured direct call with one string argument.
    fn commonjs_literal_require(file: &File<'source>, call: &StaticCall) -> Option<ByteRange> {
        if call.callee != StaticCallee::Direct || file.text(call.callee_range)? != "require" {
            return None;
        }
        let start = call.callee_range.end;
        let tail = file.text(ByteRange {
            start,
            end: call.range.end,
        })?;
        let arguments = tail
            .trim_ascii_start()
            .strip_prefix('(')?
            .strip_suffix(')')?;
        let value = arguments.trim_ascii();
        let [quote, bytes @ .., close] = value.as_bytes() else {
            return None;
        };
        if quote != close
            || !matches!(quote, b'\'' | b'"')
            || !(bytes.starts_with(b"./") || bytes.starts_with(b"../"))
            || bytes
                .iter()
                .any(|byte| byte == quote || *byte == b'\\' || byte.is_ascii_control())
        {
            return None;
        }
        let opening =
            tail.len() - arguments.len() + arguments.len() - arguments.trim_ascii_start().len();
        let start = start.checked_add(u64::try_from(opening).ok()?)?;
        Some(ByteRange {
            start,
            end: start.checked_add(u64::try_from(bytes.len()).ok()?)?,
        })
    }

    /// Retains a checked relative import at the written module specifier.
    pub(super) fn javascript_module_specifiers(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let mut selected = std::collections::BTreeSet::new();
        for binding in file.facts.bindings() {
            if !work.charge() {
                return;
            }
            let (module, require) = match binding.kind {
                StaticBindingKind::JavaScriptImport { module, .. }
                | StaticBindingKind::JavaScriptEdgeNamespace { module } => (module, false),
                StaticBindingKind::JavaScriptRequire {
                    module, require, ..
                } if self.builtin_require(file, require.start, work) => (module, true),
                _ => continue,
            };
            if file.inactive(binding.name, work) || !selected.insert(module) {
                continue;
            }
            let occurrence = self.javascript_module_specifier(file, module, require, work);
            if occurrence.is_some() {
                result.record(occurrence);
            }
        }
        for binding in file.document.facts.export_bindings().unwrap_or_default() {
            if !work.charge() {
                return;
            }
            let Some(source) = binding.source else {
                continue;
            };
            let module = ByteRange {
                start: source.start + 1,
                end: source.end.saturating_sub(1),
            };
            if binding.container.is_some()
                || module.start >= module.end
                || !file
                    .text(binding.range)
                    .is_some_and(|written| matches_export_form(written, ModuleMode::Esm))
                || !selected.insert(module)
            {
                continue;
            }
            let occurrence = self.javascript_module_specifier(file, module, false, work);
            if occurrence.is_some() {
                result.record(occurrence);
            }
        }
    }

    fn javascript_module_specifier(
        &self,
        file: &File<'source>,
        module: ByteRange,
        require: bool,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let range = ByteRange {
            start: module.start.checked_sub(1)?,
            end: module.end.checked_add(1)?,
        };
        let literal = file.text(range)?.as_bytes();
        if !matches!(literal.first().copied(), Some(b'\'' | b'"'))
            || literal.first() != literal.last()
        {
            return None;
        }
        let imported = self.javascript_specifier_file(file, module, require, work)?;
        self.javascript_import_module_occurrence(file, range, imported, work)
    }

    pub(super) fn javascript_specifier_file(
        &self,
        file: &File<'source>,
        module: ByteRange,
        require: bool,
        work: &mut Work,
    ) -> Option<&File<'source>> {
        let current = file.document.path.as_str();
        let specifier = file.text(module)?;
        if specifier.starts_with('#') {
            return javascript_exports::package_import_file(
                &self.files,
                &self.javascript,
                file,
                specifier,
                require,
                work,
            );
        }
        let path = javascript_exports::relative_path(current, specifier, work)?;
        let declaration_only = javascript_exports::is_declaration_file(current);
        let imported = if declaration_only {
            javascript_exports::declared_file(&self.files, &self.javascript, &path)
        } else {
            if !require && self.javascript.mode(current, work) == ModuleMode::CommonJs {
                return None;
            }
            javascript_exports::module_file_with_runtime_observations(
                &self.files,
                &self.javascript,
                &path,
                false,
                require,
            )
        }?;
        imported.document.placement.logical_identity(".")?;
        Some(imported)
    }
}

/// Recognizes the retained full call range in the import reference carrier.
fn commonjs_argument_call(file: &File<'_>, range: ByteRange) -> bool {
    file.text(range)
        .and_then(|text| text.strip_prefix("require"))
        .is_some_and(|tail| tail.trim_ascii_start().starts_with('(') && tail.ends_with(')'))
}

impl<'source> File<'source> {
    /// Reads both facets of one name from exactly one lexical scope.
    fn typescript_local_namespace_pair(
        &self,
        name: &str,
        position: u64,
        work: &mut Work,
        admits: impl Fn(StaticBindingKind) -> bool,
    ) -> Option<[Target<'source>; 2]> {
        let mut pair: [Option<&StaticBinding>; 2] = [None, None];
        let mut additional = false;
        self.innermost_bindings(
            name,
            position,
            work,
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
        let [Some(left), Some(right)] = pair else {
            return None;
        };
        if additional || left.scope != right.scope || !admits(left.kind) || !admits(right.kind) {
            return None;
        }
        Some([self.target_at(left.name)?, self.target_at(right.name)?])
    }
}

impl Target<'_> {
    /// A lexical variable added by the package's file module scope pass.
    pub(super) fn is_scoped_javascript_lexical_binding(self) -> bool {
        self.symbol.kind == "variable" && self.is_physical_only_javascript_package_symbol()
    }

    /// Package declarations admitted through a physical scope with no shared name anchor.
    pub(super) fn is_physical_only_javascript_package_symbol(self) -> bool {
        self.file.facts.language().name == "javascript" && self.is_physical_only_package_symbol()
    }

    fn is_scoped_typescript_union_property(self) -> bool {
        self.file.facts.language().name == "typescript"
            && self.symbol.kind == "property"
            && self.symbol.syntax_element_kind == Some("property_signature")
            && self.is_physical_only_package_symbol()
    }

    fn is_physical_only_package_symbol(self) -> bool {
        let placement = self.file.placement;
        matches!(
            placement.origin().location(),
            Some(SourceLocation::Dependency { .. })
        ) && placement
            .logical_identity(&self.symbol.qualified_name)
            .is_none()
            && placement.logical_identity_for_symbol(self.symbol).is_some()
    }
}
