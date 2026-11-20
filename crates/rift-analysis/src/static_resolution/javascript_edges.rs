//! ECMAScript containment, access, type usage, and override facts.

use rift_core::{ReferenceRole, SourceRange, SymbolId};
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_provider::NormalizedTarget;
use rift_syntax::{ByteRange, StaticBindingKind, SyntaxSymbol};

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
            let target = self
                .javascript_target(file, reference.target, reference.role, work)
                .filter(|target| {
                    reference.role != ReferenceRole::Type
                        || target.symbol.kinds.contains(&SymbolKind::Type)
                        || target.symbol.kinds.contains(&SymbolKind::Variant)
                });
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
                occurrence.edge_from = self.javascript_assignment_reference_from(
                    file,
                    reference.target,
                    reference.role,
                    work,
                );
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
                result.record(self.javascript_namespace_import(file, export.range, module, work));
            }
        }
        self.javascript_contains(file, work, result);
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
        let callable = matches!(symbol.kind, "function" | "method");
        if assignment && callable {
            self.javascript_call_from(file, range, work)
        } else {
            None
        }
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
            let from = from.or_else(|| module.clone());
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
        let mut occurrence =
            self.javascript_module_occurrence(file, range, imported, ReferenceRole::Import, work)?;
        occurrence.edge_from = self.javascript_module_identity(file, work);
        Some(occurrence)
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
            let module =
                SymbolIdentity::new(parsed.owner().clone(), parsed.language().clone(), path)
                    .ok()?;
            return SymbolId::new(module.wire_identity()).ok();
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
            if Some(edge.from) != class.symbol.name_range {
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
