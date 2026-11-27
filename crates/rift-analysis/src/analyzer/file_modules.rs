//! Physical declarations of complete package ECMAScript modules.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{SourceKind, SourceUnitId as CoreSourceUnitId, SymbolId as CoreSymbolId};

use crate::WorkspaceSemantics;
use rift_error::RiftError;
use rift_protocol::{
    identity::{SymbolIdentity, SymbolOwner},
    index::{PACKAGE_SOURCE_BYTES_MAX, PackageAnalysisWarning, PackageSourceUnit, PackageSymbol},
    read::{Digest, EdgeKind, SymbolId, SymbolKind, TextRange},
};
use rift_provider::AssembledSymbol;

use super::{
    AnalyzedFile, ExactPackageLimits, Records, bound, digest_of, javascript_module_source,
    truncation, wire_path, wire_unit,
};

impl Records {
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

    /// Adds references with a retained authored module and target.
    fn retain_ecmascript_file_module_edges(
        &mut self,
        semantics: &WorkspaceSemantics,
        edge_limit: usize,
        kinds: &[EdgeKind],
        deferred_exports: bool,
    ) {
        let mut references = kinds
            .iter()
            .copied()
            .flat_map(|kind| {
                semantics.edges().edges().filter(move |edge| {
                    edge.is_file_module_reference()
                        && edge.kind() == kind
                        && edge.is_deferred_commonjs_export_import() == deferred_exports
                })
            })
            .peekable();
        if references.peek().is_none() {
            return;
        }
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
            let retained = if edge.kind() == EdgeKind::References {
                declared.get(from).copied() == Some(unit.as_str()) && objects.contains(to)
            } else {
                objects.contains(from)
                    && declared.contains_key(to)
                    && source_units.contains(unit.as_str())
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

fn file_module_candidates<'file>(
    owner: &SymbolOwner,
    analyzed: &'file [AnalyzedFile],
    units: &[PackageSourceUnit],
    semantics: &WorkspaceSemantics,
) -> BTreeMap<&'file CoreSymbolId, Option<(&'file AnalyzedFile, bool)>> {
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
