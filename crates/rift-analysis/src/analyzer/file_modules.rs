//! Physical declarations of complete package ECMAScript modules.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{SourceKind, SourceUnitId as CoreSourceUnitId, SymbolId as CoreSymbolId};

use crate::{Edge, WorkspaceSemantics};
use rift_error::RiftError;
use rift_protocol::{
    identity::{SymbolIdentity, SymbolOwner},
    index::{PACKAGE_SOURCE_BYTES_MAX, PackageAnalysisWarning, PackageSourceUnit, PackageSymbol},
    read::{Digest, EdgeKind, SymbolId, SymbolKind, TextRange},
};
use rift_provider::AssembledSymbol;

use super::{
    AnalyzedFile, ExactPackageInput, ExactPackageLimits, Records, bound, digest_of,
    javascript_module_source, truncation, wire_path, wire_unit,
};

impl Records {
    pub(super) fn retain_selected_ecmascript_facts(
        &mut self,
        input: &ExactPackageInput<'_>,
        analyzed: &[AnalyzedFile],
        semantics: &WorkspaceSemantics,
    ) -> Result<(), RiftError> {
        let owner = input.owner();
        let limits = input.limits();
        self.retain_ecmascript_modules(owner, analyzed, semantics, limits)?;
        self.retain_javascript_callback_argument_properties(owner, analyzed, limits)?;
        if matches!(owner, SymbolOwner::Package { manager, .. } if manager == "npm") {
            let candidates = self.checked_local_export_module_candidates(analyzed, semantics);
            self.retain_file_module_candidates(owner, &candidates, limits.declarations_max())?;
            self.retain_incomplete_local_export_references(semantics, limits.edges_max());
        }
        self.retain_checked_declaration_specifiers(owner, analyzed, semantics, limits)?;
        self.retain_imported_export_selectors(semantics, limits.edges_max());
        self.retain_checked_inline_import_type_edges(semantics, limits.edges_max());
        self.retain_commonjs_argument_imports(semantics, limits.edges_max());
        self.retain_checked_inline_import_modules(semantics, limits.edges_max());
        if super::namespace::typescript::checked_commonjs_package(&input.namespace_input()) {
            self.retain_commonjs_expression_entry(owner, analyzed, semantics, limits)?;
        }
        Ok(())
    }

    fn retain_commonjs_expression_entry(
        &mut self,
        owner: &SymbolOwner,
        analyzed: &[AnalyzedFile],
        semantics: &WorkspaceSemantics,
        limits: ExactPackageLimits,
    ) -> Result<(), RiftError> {
        if self.symbols_truncated {
            return Ok(());
        }
        let candidates = commonjs_expression_entries(owner, analyzed, &self.units);
        if candidates.values().all(Option::is_none) {
            return Ok(());
        }
        let previous = self.declarations.len();
        self.retain_file_module_candidates(owner, &candidates, limits.declarations_max())?;
        let entries = self.declarations[previous..]
            .iter()
            .filter(|row| row.source_complete)
            .map(|row| row.symbol.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        if entries.is_empty() {
            return Ok(());
        }
        let (loaders, loads) =
            checked_expression_loaders(owner, analyzed, &self.units, semantics, &entries);
        self.retain_file_module_candidates(owner, &loaders, limits.declarations_max())?;
        self.retain_file_module_edge_rows(loads.into_iter(), limits.edges_max());
        self.retain_javascript_callback_argument_properties(owner, analyzed, limits)
    }

    pub(super) fn retain_ecmascript_modules(
        &mut self,
        owner: &SymbolOwner,
        analyzed: &[AnalyzedFile],
        semantics: &WorkspaceSemantics,
        limits: ExactPackageLimits,
    ) -> Result<(), RiftError> {
        if matches!(owner, SymbolOwner::Package { manager, .. } if manager == "npm") {
            self.retain_ecmascript_file_modules(
                owner,
                analyzed,
                semantics,
                limits.declarations_max(),
            )?;
            self.retain_ecmascript_file_module_edges(
                semantics,
                limits.edges_max(),
                &[EdgeKind::Imports, EdgeKind::Reads],
                false,
            );
            self.retain_ecmascript_callback_edges(
                owner,
                analyzed,
                semantics,
                limits,
                EdgeKind::Calls,
            )?;
            self.retain_ecmascript_file_module_edges(
                semantics,
                limits.edges_max(),
                &[EdgeKind::References],
                false,
            );
            self.retain_ecmascript_callback_edges(
                owner,
                analyzed,
                semantics,
                limits,
                EdgeKind::Reads,
            )?;
            self.retain_ecmascript_file_module_edges(
                semantics,
                limits.edges_max(),
                &[EdgeKind::Imports],
                true,
            );
        }
        Ok(())
    }

    pub(super) fn retain_checked_declaration_specifiers(
        &mut self,
        owner: &SymbolOwner,
        analyzed: &[AnalyzedFile],
        semantics: &WorkspaceSemantics,
        limits: ExactPackageLimits,
    ) -> Result<(), RiftError> {
        if !matches!(owner, SymbolOwner::Package { manager, .. } if manager == "npm") {
            return Ok(());
        }
        let candidates = self.checked_declaration_source_candidates(analyzed);
        let before = self.declarations.len();
        self.retain_file_module_candidates(owner, &candidates, limits.declarations_max())?;
        let new_modules = self.declarations[before..]
            .iter()
            .map(|row| row.symbol.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        if !new_modules.is_empty() {
            let references = semantics.edges().edges().filter(|edge| {
                edge.kind() == EdgeKind::Imports
                    && edge.is_written_module_specifier()
                    && (new_modules.contains(edge.from().as_str())
                        || new_modules.contains(edge.to().as_str()))
            });
            self.retain_file_module_edge_rows(references, limits.edges_max());
        }
        Ok(())
    }

    pub(super) fn retain_imported_export_selectors(
        &mut self,
        semantics: &WorkspaceSemantics,
        edge_limit: usize,
    ) {
        let references = semantics
            .edges()
            .edges()
            .filter(|edge| edge.is_imported_export_selector());
        self.retain_file_module_edge_rows(references, edge_limit);
    }

    pub(super) fn retain_checked_inline_import_type_edges(
        &mut self,
        semantics: &WorkspaceSemantics,
        limit: usize,
    ) {
        let mut references = semantics
            .edges()
            .edges()
            .filter(|edge| edge.is_inline_import_type_reference())
            .peekable();
        if references.peek().is_none() {
            return;
        }
        let complete = self
            .units
            .iter()
            .filter(|unit| unit.source_complete)
            .map(|unit| unit.unit.as_str())
            .collect::<BTreeSet<_>>();
        let declarations = self
            .declarations
            .iter()
            .map(|row| (row.symbol.as_str(), row.unit.as_str()))
            .collect::<BTreeSet<_>>();
        let types = self
            .objects
            .iter()
            .filter(|row| row.kinds.contains(&SymbolKind::Type))
            .filter_map(|row| row.id.as_ref())
            .map(SymbolId::as_str)
            .collect::<BTreeSet<_>>();
        for edge in references {
            let from = edge.from().as_str();
            let unit = edge.occurrence().unit().to_string();
            if !complete.contains(unit.as_str())
                || !declarations.contains(&(from, unit.as_str()))
                || !types.contains(edge.to().as_str())
            {
                continue;
            }
            if self.edges.len() >= limit {
                self.warn(truncation(
                    "edges",
                    u64::try_from(limit).unwrap_or(u64::MAX),
                ));
                return;
            }
            self.edges.push(edge.to_protocol());
        }
    }

    pub(super) fn retain_checked_inline_import_modules(
        &mut self,
        semantics: &WorkspaceSemantics,
        limit: usize,
    ) {
        let mut references = semantics
            .edges()
            .edges()
            .filter(|edge| edge.is_inline_import_module())
            .peekable();
        if references.peek().is_none() {
            return;
        }
        let units = self
            .units
            .iter()
            .filter(|unit| unit.source_complete)
            .map(|unit| (unit.unit.as_str(), unit.source.as_str()))
            .collect::<BTreeMap<_, _>>();
        let modules = self
            .objects
            .iter()
            .filter(|object| object.kinds.contains(&SymbolKind::Module))
            .filter_map(|object| object.id.as_ref())
            .collect::<BTreeSet<_>>();
        let mut complete = BTreeMap::new();
        for row in &self.declarations {
            if modules.contains(&row.symbol)
                && row.source_complete
                && row.range.start == 0
                && units.get(row.unit.as_str()) == Some(&row.source.as_str())
            {
                complete
                    .entry(row.symbol.as_str())
                    .and_modify(|prior| *prior = None)
                    .or_insert(Some(row.unit.as_str()));
            }
        }
        for edge in references {
            let unit = edge.occurrence().unit().to_string();
            if complete.get(edge.from().as_str()).copied().flatten() != Some(unit.as_str())
                || complete
                    .get(edge.to().as_str())
                    .copied()
                    .flatten()
                    .is_none()
            {
                continue;
            }
            if self.edges.len() >= limit {
                self.warn(truncation(
                    "edges",
                    u64::try_from(limit).unwrap_or(u64::MAX),
                ));
                return;
            }
            self.edges.push(edge.to_protocol());
        }
    }

    pub(super) fn retain_commonjs_argument_imports(
        &mut self,
        semantics: &WorkspaceSemantics,
        edge_limit: usize,
    ) {
        let references = semantics
            .edges()
            .edges()
            .filter(|edge| edge.is_commonjs_argument_import());
        self.retain_file_module_edge_rows(references, edge_limit);
    }

    fn checked_declaration_source_candidates<'file>(
        &self,
        analyzed: &'file [AnalyzedFile],
    ) -> FileModuleCandidates<'file> {
        let complete = self
            .units
            .iter()
            .filter(|unit| unit.source_complete)
            .map(|unit| unit.unit.as_str())
            .collect::<BTreeSet<_>>();
        let mut modules = FileModuleCandidates::new();
        for file in analyzed {
            let Some(module) = file.placement.logical_identity(".") else {
                continue;
            };
            let syntax = file.file.syntax();
            let proven = syntax.language().name == "typescript"
                && crate::static_resolution::is_declaration_file(file.file.path().as_str())
                && !file.file.source().is_empty()
                && !syntax.has_errors()
                && syntax.left_out_declaration_count() == 0
                && complete.contains(file.placement.unit().to_string().as_str())
                && syntax
                    .static_facts()
                    .is_some_and(rift_syntax::StaticFacts::has_javascript_module_statement);
            modules
                .entry(module)
                .and_modify(|candidate| *candidate = None)
                .or_insert_with(|| proven.then_some((file, false)));
        }
        modules
    }

    fn checked_local_export_module_candidates<'file>(
        &self,
        analyzed: &'file [AnalyzedFile],
        semantics: &WorkspaceSemantics,
    ) -> FileModuleCandidates<'file> {
        let edges = semantics.edges();
        let mut selectors = edges
            .edges()
            .filter(|edge| edge.is_incomplete_local_export_reference())
            .peekable();
        if selectors.peek().is_none() {
            return BTreeMap::new();
        }
        let declared = self
            .declarations
            .iter()
            .map(|row| (row.symbol.as_str(), row.unit.as_str()))
            .collect::<BTreeSet<_>>();
        let sources = selectors
            .filter_map(|edge| {
                let unit = edge.occurrence().unit();
                declared
                    .contains(&(edge.to().as_str(), unit.to_string().as_str()))
                    .then_some((edge.from(), unit))
            })
            .collect::<BTreeSet<_>>();
        let complete = self
            .units
            .iter()
            .filter(|unit| unit.source_complete)
            .map(|unit| unit.unit.as_str())
            .collect::<BTreeSet<_>>();
        let mut modules = FileModuleCandidates::new();
        for file in analyzed {
            let Some(module) = file.placement.logical_identity(".") else {
                continue;
            };
            let unit = file.placement.unit();
            let proven = sources.contains(&(module, unit))
                && complete.contains(unit.to_string().as_str())
                && file.file.syntax().left_out_declaration_count() == 0;
            modules
                .entry(module)
                .and_modify(|candidate| *candidate = None)
                .or_insert_with(|| proven.then_some((file, false)));
        }
        modules
    }

    pub(super) fn retain_incomplete_local_export_references(
        &mut self,
        semantics: &WorkspaceSemantics,
        edge_limit: usize,
    ) {
        let references = semantics
            .edges()
            .edges()
            .filter(|edge| edge.is_incomplete_local_export_reference());
        self.retain_file_module_edge_rows(references, edge_limit);
    }

    /// Adds references with a retained authored module and target.
    fn retain_ecmascript_file_module_edges(
        &mut self,
        semantics: &WorkspaceSemantics,
        edge_limit: usize,
        kinds: &[EdgeKind],
        deferred_exports: bool,
    ) {
        let references = kinds.iter().copied().flat_map(|kind| {
            semantics.edges().edges().filter(move |edge| {
                edge.is_file_module_reference()
                    && !edge.is_incomplete_local_export_reference()
                    && !edge.is_imported_export_selector()
                    && !edge.is_commonjs_argument_import()
                    && !edge.is_inline_import_module()
                    && edge.kind() == kind
                    && edge.is_deferred_commonjs_export_import() == deferred_exports
            })
        });
        self.retain_file_module_edge_rows(references, edge_limit);
    }

    fn retain_file_module_edge_rows<'a>(
        &mut self,
        references: impl Iterator<Item = &'a Edge>,
        edge_limit: usize,
    ) {
        let mut references = references.peekable();
        let Some(first) = references.peek() else {
            return;
        };
        let complete_units = first.is_incomplete_local_export_reference().then(|| {
            self.units
                .iter()
                .filter(|unit| unit.source_complete)
                .map(|unit| unit.unit.as_str())
                .collect::<BTreeSet<_>>()
        });
        let local_targets = complete_units.as_ref().map(|_| {
            self.declarations
                .iter()
                .map(|row| (row.symbol.as_str(), row.unit.as_str()))
                .collect::<BTreeSet<_>>()
        });
        let modules = self
            .objects
            .iter()
            .filter(|object| object.kinds.contains(&SymbolKind::Module))
            .filter_map(|object| object.id.as_ref())
            .collect::<BTreeSet<_>>();
        let declared = self
            .declarations
            .iter()
            .filter(|declaration| modules.contains(&declaration.symbol))
            .map(|declaration| (declaration.symbol.as_str(), declaration.unit.as_str()))
            .collect::<BTreeMap<_, _>>();
        if declared.is_empty() {
            return;
        }
        let source_units = declared.values().copied().collect::<BTreeSet<_>>();
        let objects = self
            .objects
            .iter()
            .filter_map(|object| object.id.as_ref())
            .map(SymbolId::as_str)
            .collect::<BTreeSet<_>>();
        for edge in references {
            let unit = edge.occurrence().unit().to_string();
            let from = edge.from().as_str();
            let to = edge.to().as_str();
            if complete_units
                .as_ref()
                .is_some_and(|units| !units.contains(unit.as_str()))
                || local_targets
                    .as_ref()
                    .is_some_and(|targets| !targets.contains(&(to, unit.as_str())))
            {
                continue;
            }
            let retained = if edge.kind() == EdgeKind::References {
                declared.get(from).copied() == Some(unit.as_str()) && objects.contains(to)
            } else {
                let source_matches = if edge.is_commonjs_argument_import() {
                    declared.get(from).copied() == Some(unit.as_str())
                } else {
                    source_units.contains(unit.as_str())
                };
                source_matches && objects.contains(from) && declared.contains_key(to)
            };
            if !retained {
                continue;
            }
            if self.edges.len() >= edge_limit {
                self.warn(truncation(
                    "edges",
                    u64::try_from(edge_limit).unwrap_or(u64::MAX),
                ));
                return;
            }
            self.edges.push(edge.to_protocol());
        }
    }

    /// Admits whole-file declarations after the earlier publication rows.
    fn retain_ecmascript_file_modules(
        &mut self,
        owner: &SymbolOwner,
        analyzed: &[AnalyzedFile],
        semantics: &WorkspaceSemantics,
        declaration_limit: usize,
    ) -> Result<(), RiftError> {
        if self.symbols_truncated {
            return Ok(());
        }
        let modules = file_module_candidates(owner, analyzed, &self.units, semantics);
        self.retain_file_module_candidates(owner, &modules, declaration_limit)
    }

    fn retain_file_module_candidates(
        &mut self,
        owner: &SymbolOwner,
        modules: &FileModuleCandidates<'_>,
        declaration_limit: usize,
    ) -> Result<(), RiftError> {
        if self.symbols_truncated {
            return Ok(());
        }
        let declared = self
            .declarations
            .iter()
            .map(|row| row.symbol.clone())
            .collect::<BTreeSet<_>>();
        let objects = self
            .objects
            .iter()
            .enumerate()
            .filter_map(|(position, row)| Some((row.id.as_ref()?.0.clone(), position)))
            .collect::<BTreeMap<_, _>>();
        let declaration_limit = declaration_limit.min(bound(self.publication.symbols));
        let checked = modules
            .iter()
            .filter_map(|(&module, &file)| Some((module, file?)));
        let modules = checked
            .clone()
            .filter(|(_, (_, existing))| *existing)
            .chain(checked.filter(|(_, (_, existing))| !*existing));
        for (module, (file, _)) in modules {
            let Ok(identity) = SymbolIdentity::parse(module.as_str()) else {
                continue;
            };
            let [name] = identity.qualified_path() else {
                continue;
            };
            let language = file.file.syntax().language();
            let id = SymbolId(module.as_str().to_owned());
            if identity.owner() != owner
                || identity.language() != language
                || declared.contains(&id)
            {
                continue;
            }
            let position = objects.get(module.as_str()).copied();
            if position.is_some_and(|position| {
                let object = &self.objects[position];
                object.origin.source_kind != SourceKind::Synthetic
                    || !object.kinds.contains(&SymbolKind::Module)
            }) {
                continue;
            }
            if self.declarations.len() >= declaration_limit {
                self.symbols_truncated = true;
                self.warn(truncation(
                    "declarations",
                    u64::try_from(declaration_limit).unwrap_or(u64::MAX),
                ));
                return Ok(());
            }
            let source = file.file.source();
            let path = wire_path(file.file.path());
            let bytes = u64::try_from(source.len()).unwrap_or(u64::MAX);
            if !self.retained_source_fits(bytes) {
                self.warn(PackageAnalysisWarning::SourceTruncated {
                    path,
                    dropped: bytes,
                });
                continue;
            }
            let position = if let Some(position) = position {
                position
            } else {
                let position = self.objects.len();
                if !self.publish_file_module(language, name, module) {
                    return Ok(());
                }
                position
            };
            let retained = self.retained(source, &path, 1)?;
            let origin = AssembledSymbol::wire_origin(file.placement.origin());
            self.objects[position].origin = origin.clone();
            let mut declaration = PackageSymbol {
                symbol: id,
                origin,
                unit: wire_unit(file.placement.unit()),
                range: TextRange {
                    start: 0,
                    end: bytes,
                },
                syntax_element_kind: None,
                line: 1,
                signature_indices: Vec::new(),
                type_indices: Vec::new(),
                documentation_indices: Vec::new(),
                source: retained.text,
                source_complete: retained.complete,
                public: file.reached,
                digest: Digest(String::new()),
            };
            declaration.digest = digest_of(&declaration, owner)?;
            self.declarations.push(declaration);
        }
        Ok(())
    }

    pub(super) fn retained_source_fits(&self, bytes: u64) -> bool {
        self.retained_source
            .map_or(bytes <= u64::from(PACKAGE_SOURCE_BYTES_MAX), |limit| {
                bytes <= u64::from(limit.record_bytes_max)
                    && bytes
                        <= limit
                            .total_bytes_max
                            .saturating_sub(self.retained_source_bytes)
            })
    }
}

type FileModuleCandidates<'file> =
    BTreeMap<&'file CoreSymbolId, Option<(&'file AnalyzedFile, bool)>>;

fn commonjs_expression_entries<'file>(
    owner: &SymbolOwner,
    analyzed: &'file [AnalyzedFile],
    units: &[PackageSourceUnit],
) -> FileModuleCandidates<'file> {
    let SymbolOwner::Package { manager, name, .. } = owner else {
        return BTreeMap::new();
    };
    if manager != "npm" {
        return BTreeMap::new();
    }
    let complete = units
        .iter()
        .filter(|unit| unit.source_complete)
        .map(|unit| unit.unit.as_str())
        .collect::<BTreeSet<_>>();
    let mut candidates = BTreeMap::<_, Option<(&AnalyzedFile, bool)>>::new();
    for file in analyzed {
        let Some(module) = file.placement.logical_identity(".") else {
            continue;
        };
        let syntax = file.file.syntax();
        let path = file.file.path().as_str();
        let proven = syntax.language().name == "javascript"
            && !path.contains('/')
            && matches!(path.rsplit_once('.'), Some((_, "js")))
            && syntax.declaration_syntax_complete()
            && complete.contains(file.placement.unit().to_string().as_str())
            && SymbolIdentity::parse(module.as_str()).is_ok_and(|identity| {
                identity.owner() == owner && identity.qualified_path() == [name.as_str()]
            })
            && syntax.static_facts().is_some_and(|facts| {
                facts.has_commonjs_expression_export() && !facts.has_javascript_module_syntax()
            });
        candidates
            .entry(module)
            .and_modify(|candidate| *candidate = None)
            .or_insert(proven.then_some((file, false)));
    }
    candidates
}

/// Admits a complete secondary script only through its checked load of the entry.
fn checked_expression_loaders<'file, 'edge>(
    owner: &SymbolOwner,
    analyzed: &'file [AnalyzedFile],
    units: &[PackageSourceUnit],
    semantics: &'edge WorkspaceSemantics,
    entries: &BTreeSet<String>,
) -> (FileModuleCandidates<'file>, Vec<&'edge Edge>) {
    let SymbolOwner::Package { name, .. } = owner else {
        return (BTreeMap::new(), Vec::new());
    };
    let complete = units
        .iter()
        .filter(|unit| unit.source_complete)
        .map(|unit| unit.unit.as_str())
        .collect::<BTreeSet<_>>();
    let mut checked = BTreeMap::new();
    for file in analyzed {
        let Some(module) = file.placement.logical_identity(".") else {
            continue;
        };
        let unit = file.placement.unit();
        let path = file.file.path().as_str();
        let syntax = file.file.syntax();
        let complete_source = syntax.language().name == "javascript"
            && path.ends_with(".js")
            && syntax.declaration_syntax_complete()
            && complete.contains(unit.to_string().as_str())
            && SymbolIdentity::parse(module.as_str()).is_ok_and(|identity| {
                identity.owner() == owner && identity.qualified_path() == [format!("{name}/{path}")]
            });
        let mut loads = BTreeMap::<_, Option<&Edge>>::new();
        if complete_source {
            let ranges = written_relative_require_ranges(file);
            for load in semantics.edges().outgoing(module) {
                let site = load.occurrence();
                let range = (site.range().start(), site.range().end());
                if load.kind() == EdgeKind::Imports
                    && load.is_file_module_reference()
                    && !load.is_commonjs_argument_import()
                    && site.unit() == unit
                    && entries.contains(load.to().as_str())
                    && ranges.contains(&range)
                {
                    loads
                        .entry(range)
                        .and_modify(|edge| *edge = None)
                        .or_insert(Some(load));
                }
            }
        }
        let loads = loads.into_values().flatten().collect::<Vec<_>>();
        checked
            .entry(module)
            .and_modify(|candidate| *candidate = None)
            .or_insert_with(|| (!loads.is_empty()).then_some((file, loads)));
    }
    let mut candidates = BTreeMap::new();
    let mut loads = Vec::new();
    for (module, checked) in checked {
        if let Some((file, edges)) = checked {
            candidates.insert(module, Some((file, false)));
            loads.extend(edges);
        }
    }
    (candidates, loads)
}

fn written_relative_require_ranges(file: &AnalyzedFile) -> BTreeSet<(u64, u64)> {
    let source = file.file.source();
    let leading = source.len() - source.trim_start().len();
    let trailing = source.trim_end().len()..=source.len();
    file.file
        .syntax()
        .static_facts()
        .into_iter()
        .flat_map(rift_syntax::StaticFacts::bindings)
        .filter_map(|binding| {
            let rift_syntax::StaticBindingKind::JavaScriptRequire { module, .. } = binding.kind
            else {
                return None;
            };
            let start = usize::try_from(binding.scope.start).ok()?;
            let end = usize::try_from(binding.scope.end).ok()?;
            let literal = source
                .get(usize::try_from(module.start).ok()?..usize::try_from(module.end).ok()?)?;
            (start <= leading
                && trailing.contains(&end)
                && (literal.starts_with("./")
                    || literal.starts_with("../")
                    || matches!(literal, "." | "..")))
            .then_some(())?;
            Some((module.start.checked_sub(1)?, module.end.checked_add(1)?))
        })
        .collect()
}

fn file_module_candidates<'file>(
    owner: &SymbolOwner,
    analyzed: &'file [AnalyzedFile],
    units: &[PackageSourceUnit],
    semantics: &WorkspaceSemantics,
) -> FileModuleCandidates<'file> {
    let proven_receivers = commonjs_module_source_units(semantics);
    let complete = units
        .iter()
        .filter(|unit| unit.source_complete)
        .map(|unit| unit.unit.as_str())
        .collect::<BTreeSet<_>>();
    let mut modules = BTreeMap::<_, Option<(&AnalyzedFile, bool)>>::new();
    for file in analyzed {
        let syntax = file.file.syntax();
        let language = syntax.language().name.as_str();
        let path = file.file.path().as_str();
        let Some(module) = file.placement.logical_identity(".") else {
            continue;
        };
        let explicit = matches!(
            (language, path.rsplit_once('.')),
            ("javascript", Some((_, "mjs" | "cjs"))) | ("typescript", Some((_, "mts" | "cts")))
        );
        let previous = explicit || javascript_module_source(owner, file);
        let proven = matches!(language, "javascript" | "typescript")
            && (previous || proven_receivers.contains(file.placement.unit()))
            && !syntax.has_errors()
            && syntax.left_out_declaration_count() == 0
            && syntax.primary_symbols_len() > 0
            && complete.contains(file.placement.unit().to_string().as_str());
        modules
            .entry(module)
            .and_modify(|file| *file = None)
            .or_insert(proven.then_some((file, previous)));
    }
    modules
}

fn commonjs_module_source_units(semantics: &WorkspaceSemantics) -> BTreeSet<&CoreSourceUnitId> {
    let edges = semantics.edges();
    edges
        .edges()
        .filter(|edge| {
            edge.is_file_module_reference()
                && edge.kind() == EdgeKind::Reads
                && edges.edge_module(edge.occurrence().unit()) == Some(edge.to())
        })
        .map(|edge| edge.occurrence().unit())
        .collect()
}
