//! Private TypeScript bindings in distinct physically admitted callback bodies.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::PortableSymbolFacts;
use rift_error::RiftError;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::index::PackageSymbol;
use rift_protocol::read::{Digest, EdgeKind, ExactKind, SymbolId, SymbolKind, TextRange};
use rift_provider::AssembledSymbol;
use rift_syntax::{ByteRange, StaticBindingKind, SyntaxSymbol};

use crate::ExactPackageLimits;
use crate::analyzer::{AnalyzedFile, Records, bound, digest_of, line_of, truncation, wire_path};

use super::{CallbackFile, source_text};

struct Local<'source> {
    symbol: &'source SyntaxSymbol,
    source: &'source str,
    identity: SymbolId,
    callback: SymbolId,
}

impl Records {
    /// Splits equally spelled local declarations after their physical callbacks are published.
    pub(in crate::analyzer) fn retain_typescript_callback_bindings(
        &mut self,
        owner: &SymbolOwner,
        analyzed: &[AnalyzedFile],
        limits: ExactPackageLimits,
    ) -> Result<(), RiftError> {
        if self.symbols_truncated
            || !matches!(owner, SymbolOwner::Package { manager, .. } if manager == "npm")
            || analyzed
                .iter()
                .all(|file| file.file.syntax().language().name != "typescript")
        {
            return Ok(());
        }
        let mut occupied = self
            .objects
            .iter()
            .filter_map(|object| object.id.clone())
            .collect::<BTreeSet<_>>();
        let placed = self
            .declarations
            .iter()
            .fold(BTreeMap::new(), |mut by_unit, row| {
                by_unit
                    .entry(row.unit.clone())
                    .or_insert_with(BTreeSet::new)
                    .insert(ByteRange {
                        start: row.range.start,
                        end: row.range.end,
                    });
                by_unit
            });
        let files = self.ecmascript_callback_files(analyzed, EdgeKind::Calls);
        for file in files.values().filter(|file| {
            file.file.file.syntax().language().name == "typescript"
                && !crate::static_resolution::is_declaration_file(file.file.file.path().as_str())
        }) {
            let Some(ranges) = placed.get(&file.unit) else {
                continue;
            };
            for group in self.checked_typescript_callbacks(file, ranges, &occupied) {
                if group.iter().any(|local| occupied.contains(&local.identity)) {
                    continue;
                }
                if !self.callback_binding_group_fits(file, &group, limits) {
                    if self.symbols_truncated {
                        return Ok(());
                    }
                    continue;
                }
                for local in group {
                    occupied.insert(local.identity.clone());
                    self.publish_typescript_callback_binding(owner, file, local)?;
                }
            }
        }
        Ok(())
    }

    fn checked_typescript_callbacks<'source>(
        &self,
        file: &CallbackFile<'source>,
        placed: &BTreeSet<ByteRange>,
        occupied: &BTreeSet<SymbolId>,
    ) -> Vec<Vec<Local<'source>>> {
        let syntax = file.file.file.syntax();
        let Some(facts) = syntax.static_facts() else {
            return Vec::new();
        };
        let Ok(module) = SymbolIdentity::parse(file.module.as_str()) else {
            return Vec::new();
        };
        let source = file.file.file.source();
        let mut bindings = BTreeMap::new();
        for binding in facts.bindings() {
            bindings
                .entry(binding.name)
                .and_modify(|scope| *scope = None)
                .or_insert((binding.kind == StaticBindingKind::Local).then_some(binding.scope));
        }
        let mut groups = BTreeMap::<_, Vec<_>>::new();
        for symbol in syntax.symbols() {
            groups.entry(symbol.name.as_str()).or_default().push(symbol);
        }
        groups
            .into_values()
            .filter_map(|group| {
                if group.len() < 2 {
                    return None;
                }
                let mut scopes = BTreeSet::new();
                let mut identities = BTreeSet::new();
                group
                    .into_iter()
                    .map(|symbol| {
                        if symbol.kind != "variable"
                            || symbol.syntax_element_kind != Some("variable_declarator")
                            || symbol.qualified_name != symbol.name
                            || symbol.container.is_some()
                            || !symbol.documentation.is_empty()
                            || symbol.kinds.iter().any(|kind| {
                                matches!(
                                    kind,
                                    SymbolKind::Public | SymbolKind::Callable | SymbolKind::Type
                                )
                            })
                            || placed.contains(&symbol.range)
                        {
                            return None;
                        }
                        let name = symbol.name_range?;
                        let scope = (*bindings.get(&name)?)?;
                        if source_text(source, name)? != symbol.name
                            || !symbol.range.encloses(name)
                            || !scope.encloses(symbol.item_range)
                            || !scopes.insert(scope)
                        {
                            return None;
                        }
                        let callback = (*file.scopes.resolve(scope.start, scope.end)?)?;
                        let callback =
                            local_identity(&module, format!("$callback#{}", callback.ordinal))
                                .filter(|id| {
                                    occupied.contains(id)
                                        && self.retained_callback(file, callback, id)
                                })?;
                        let identity = local_identity(
                            &module,
                            format!("{}#{}", symbol.name, symbol.declaration_ordinal),
                        )?;
                        if occupied.contains(&identity) || !identities.insert(identity.clone()) {
                            return None;
                        }
                        Some(Local {
                            symbol,
                            source: source_text(source, symbol.range)?,
                            identity,
                            callback,
                        })
                    })
                    .collect()
            })
            .collect()
    }

    fn callback_binding_group_fits(
        &mut self,
        file: &CallbackFile<'_>,
        group: &[Local<'_>],
        limits: ExactPackageLimits,
    ) -> bool {
        let symbols = bound(self.publication.symbols);
        let declarations = limits.declarations_max().min(symbols);
        if self.objects.len().saturating_add(group.len()) > symbols
            || self.declarations.len().saturating_add(group.len()) > declarations
        {
            self.symbols_truncated = true;
            self.warn(truncation(
                "declarations",
                u64::try_from(declarations).unwrap_or(u64::MAX),
            ));
            return false;
        }
        let sizes = group
            .iter()
            .map(|local| local.symbol.range.end - local.symbol.range.start);
        let exceeds_total = self.retained_source.is_some_and(|limit| {
            sizes
                .clone()
                .fold(self.retained_source_bytes, u64::saturating_add)
                > limit.total_bytes_max
        });
        if exceeds_total || sizes.clone().any(|bytes| !self.retained_source_fits(bytes)) {
            self.warn(
                rift_protocol::index::PackageAnalysisWarning::SourceTruncated {
                    path: wire_path(file.file.file.path()),
                    dropped: sizes.fold(0, u64::saturating_add),
                },
            );
            return false;
        }
        true
    }

    fn publish_typescript_callback_binding(
        &mut self,
        owner: &SymbolOwner,
        file: &CallbackFile<'_>,
        local: Local<'_>,
    ) -> Result<(), RiftError> {
        let Local {
            symbol,
            source,
            identity,
            callback,
        } = local;
        let facts = PortableSymbolFacts::new(
            file.file.file.syntax().language().clone(),
            &symbol.name,
            &symbol.qualified_name,
            ExactKind(symbol.kind.to_owned()),
        )
        .kinds(symbol.kinds.clone());
        let mut object = AssembledSymbol::wire_facts(&facts, file.file.placement.origin());
        object.id = Some(identity.clone());
        object.container = Some(callback);
        object.document_local = true;
        let retained = self.retained(source, &wire_path(file.file.file.path()), 1)?;
        let mut declaration = PackageSymbol {
            symbol: identity,
            origin: object.origin.clone(),
            unit: file.unit.clone(),
            range: TextRange {
                start: symbol.range.start,
                end: symbol.range.end,
            },
            syntax_element_kind: symbol.syntax_element_kind.map(str::to_owned),
            line: line_of(&file.lines, symbol.range.start),
            signature_indices: Vec::new(),
            type_indices: Vec::new(),
            documentation_indices: Vec::new(),
            source: retained.text,
            source_complete: retained.complete,
            public: false,
            digest: Digest(String::new()),
        };
        declaration.digest = digest_of(&declaration, owner)?;
        self.objects.push(object);
        self.declarations.push(declaration);
        Ok(())
    }
}

fn local_identity(module: &SymbolIdentity, name: String) -> Option<SymbolId> {
    let mut path = module.qualified_path().to_vec();
    path.push(name);
    let identity =
        SymbolIdentity::new(module.owner().clone(), module.language().clone(), path).ok()?;
    Some(SymbolId(identity.wire_identity()))
}
