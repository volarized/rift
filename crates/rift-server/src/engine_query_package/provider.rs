//! Provider records captured by the same admitted package analysis.

use std::collections::BTreeMap;
use std::sync::Arc;

use lsp_types::Uri;
use rift_analysis::{AnalyzedFile, PackageAnalysis, WorkspaceSemantics};
use rift_core::{
    Contribution, ContributionKey, DeclarationBinding, FileDigest, PortableSymbolFacts,
    SymbolRecord, WorkspaceId,
};
use rift_protocol::identity::{SourceDigest, SymbolIdentity, SymbolOwner};
use rift_protocol::read::{SourceUnitId, SourceUnitSpan, TextRange};
use rift_provider::{AssembledSymbol, NormalizedGraph, NormalizedTarget};
use rift_search::query::{
    QueryFacts, QueryProviderContext, QueryProviderDeclaration, QueryProviderRecord,
    QueryProviderReference, QueryProviderRelationship, QueryProviderSnapshot, QueryProviderTarget,
    QuerySymbol,
};

use super::{DeclarationLocation, RiftError, SelectedSource, unavailable};

pub(super) struct CapturedProviders {
    pub(super) names: Vec<(QueryProviderTarget, DeclarationLocation)>,
    pub(super) selected: Option<QueryProviderTarget>,
}

pub(super) struct ProviderCapture<'capture> {
    pub(super) source: &'capture SelectedSource,
    pub(super) owner: SymbolOwner,
    pub(super) root: Uri,
    pub(super) digest: FileDigest,
    pub(super) inputs: &'capture [(Uri, rift_core::ProjectPath, String)],
    pub(super) captured: &'capture [(Uri, rift_core::ProjectPath, String)],
    pub(super) enclosing: bool,
    pub(super) uris: &'capture BTreeMap<String, Uri>,
    pub(super) configuration: &'capture rift_protocol::configuration::WorkspaceConfiguration,
    pub(super) selection: Option<&'capture rift_search::query::QueryProviderSelection>,
}

pub(super) fn capture(
    analysis: PackageAnalysis,
    facts: &mut QueryFacts,
    input: ProviderCapture<'_>,
    cancelled: &(dyn Fn() -> bool + Sync),
    mut budget: Option<&mut crate::QueryFactBudget<'_>>,
) -> Result<CapturedProviders, RiftError> {
    let ProviderCapture {
        source,
        owner,
        root,
        digest,
        inputs,
        captured,
        enclosing,
        uris,
        configuration,
        selection,
    } = input;
    let (_, files, semantics, _) = analysis.into_parts();
    let graph = semantics.graph();
    let inputs = captured
        .iter()
        .chain(inputs)
        .map(|(uri, _, text)| {
            Ok(rift_search::query::QueryProviderInput {
                uri: uri.as_str().to_owned(),
                digest: SourceDigest::parse(
                    &data_encoding::HEXLOWER.encode(FileDigest::of(text.as_bytes()).as_bytes()),
                )?,
            })
        })
        .collect::<Result<Vec<_>, RiftError>>()?;
    let bytes = rift_protocol::canonical::canonical_json(&inputs)
        .map_err(|error| unavailable(error.to_string()))?;
    let fingerprint = SourceDigest::parse(
        &data_encoding::HEXLOWER.encode(FileDigest::of(bytes.as_bytes()).as_bytes()),
    )?;
    let selection = if let Some(selection) = selection {
        selection.clone()
    } else {
        rift_search::query::QueryProviderSelection {
            root: WorkspaceId::new(root.as_str())?,
            owner: owner.clone(),
            index_revision: graph.index_revision(),
            source_revision: graph.source_revision(),
            tree_revision: graph.tree_revision(),
            fingerprint: fingerprint.clone(),
            configuration: rift_search::query::QueryProviderSelection::configuration_digest(
                configuration,
            )?,
        }
    };
    let context = QueryProviderContext {
        selection,
        root: WorkspaceId::new(root.as_str())?,
        owner,
        index_revision: graph.index_revision(),
        source_revision: graph.source_revision(),
        tree_revision: graph.tree_revision(),
        fingerprint,
        source_digest: SourceDigest::parse(&data_encoding::HEXLOWER.encode(digest.as_bytes()))?,
        inputs,
    };
    if let Some(budget) = &mut budget {
        budget.charge(&context)?;
    }
    let mut snapshot = QueryProviderSnapshot {
        context: context.clone(),
        records: Vec::new(),
        source_indices: (0..facts.sources.len()).collect(),
        references: Vec::new(),
        relationships: Vec::new(),
    };
    let shared_context = std::sync::Arc::new(context);
    let mut records = RecordCapture {
        graph,
        semantics: &semantics,
        files: &files,
        source,
        enclosing,
        uris,
        context: shared_context,
        names: Vec::new(),
        selected: None,
        selected_size: u64::MAX,
    };
    records.capture_records(facts, &mut snapshot, cancelled, &mut budget)?;
    capture_edges(graph, &mut snapshot, cancelled, &mut budget)?;
    facts.providers.push(snapshot);
    Ok(CapturedProviders {
        names: records.names,
        selected: records.selected,
    })
}

struct RecordCapture<'capture> {
    graph: &'capture NormalizedGraph,
    semantics: &'capture WorkspaceSemantics,
    files: &'capture [AnalyzedFile],
    source: &'capture SelectedSource,
    enclosing: bool,
    uris: &'capture BTreeMap<String, Uri>,
    context: Arc<QueryProviderContext>,
    names: Vec<(QueryProviderTarget, DeclarationLocation)>,
    selected: Option<QueryProviderTarget>,
    selected_size: u64,
}

impl RecordCapture<'_> {
    fn capture_records(
        &mut self,
        facts: &mut QueryFacts,
        snapshot: &mut QueryProviderSnapshot,
        cancelled: &(dyn Fn() -> bool + Sync),
        budget: &mut Option<&mut crate::QueryFactBudget<'_>>,
    ) -> Result<(), RiftError> {
        let graph = self.graph;
        for record in graph.records() {
            if cancelled() {
                return rift_error::errors::server::read_cancelled().fail();
            }
            self.capture_record(record, facts, snapshot, budget)?;
        }
        Ok(())
    }

    fn capture_record(
        &mut self,
        record: &SymbolRecord,
        facts: &mut QueryFacts,
        snapshot: &mut QueryProviderSnapshot,
        budget: &mut Option<&mut crate::QueryFactBudget<'_>>,
    ) -> Result<(), RiftError> {
        if let Some(budget) = budget {
            use get_size2::GetSize;
            for key in record.contributions() {
                if let Some(contribution) = self.graph.contribution(key) {
                    budget.charge_material(contribution.get_size())?;
                }
            }
        }
        let Some(assembled) = self.semantics.assembled_record(record) else {
            return Ok(());
        };
        let Some(combined) = assembled.facts() else {
            return Ok(());
        };
        let mut symbol = assembled.to_protocol_symbol(combined);
        if symbol
            .id
            .as_ref()
            .is_some_and(|id| SymbolIdentity::parse(id.as_str()).is_err())
        {
            symbol.id = None;
        }
        if symbol
            .container
            .as_ref()
            .is_some_and(|id| SymbolIdentity::parse(id.as_str()).is_err())
        {
            symbol.container = None;
        }
        let existing = symbol.id.as_ref().and_then(|identity| {
            facts
                .symbols
                .iter()
                .position(|entry| entry.symbol.id.as_ref() == Some(identity))
        });
        let mut payload = QuerySymbol {
            symbol,
            declarations: Vec::new(),
        };
        let mut bindings = Vec::new();
        let target = record
            .identity()
            .cloned()
            .map(NormalizedTarget::Symbol)
            .or_else(|| {
                record
                    .contributions()
                    .first()
                    .map(|key| NormalizedTarget::Contribution(key.reference().clone()))
            })
            .ok_or_else(|| unavailable("the captured provider record has no contribution"))?;
        let selected_target = QueryProviderTarget {
            context: Some(Arc::clone(&self.context)),
            target,
        };
        for key in record.contributions() {
            self.capture_declaration(key, combined, &selected_target, &mut payload, &mut bindings)?;
        }
        if bindings.is_empty() {
            return Ok(());
        }
        let symbol_index = merge_payload(facts, payload, existing, &mut bindings);
        snapshot.records.push(QueryProviderRecord {
            symbol_index,
            identity: record.identity().cloned(),
            contributions: record.contributions().to_vec(),
            declarations: bindings,
            container: combined
                .container_reference()
                .cloned()
                .map(NormalizedTarget::Contribution),
        });
        Ok(())
    }

    fn capture_declaration(
        &mut self,
        key: &ContributionKey,
        combined: &PortableSymbolFacts,
        selected_target: &QueryProviderTarget,
        payload: &mut QuerySymbol,
        bindings: &mut Vec<QueryProviderDeclaration>,
    ) -> Result<(), RiftError> {
        let Some(contribution) = self.graph.contribution(key) else {
            return Ok(());
        };
        let Some(binding) = contribution.source() else {
            return Ok(());
        };
        let Some(supplied) = contribution.facts() else {
            return Ok(());
        };
        let Some(file) = self
            .files
            .iter()
            .find(|file| file.placement().unit() == binding.unit())
        else {
            return Ok(());
        };
        let file = file.file();
        let Some(uri) = self.uris.get(file.path().as_str()) else {
            return Ok(());
        };
        let range = binding.range();
        let Some(syntax) = file.enclosing_symbol(range.start(), range.end()) else {
            return Ok(());
        };
        if syntax.item_range.start != range.start()
            || syntax.item_range.end != range.end()
            || supplied.name() != syntax.name
        {
            return unavailable(
                "the captured declaration differs from its original syntax binding",
            )
            .fail();
        }
        let unit = SourceUnitId(binding.unit().to_string());
        let declaration = declaration(
            contribution,
            binding,
            combined,
            supplied,
            file.source(),
            &unit,
        )?;
        let declaration_index = payload.declarations.len();
        payload.declarations.push(declaration);
        bindings.push(QueryProviderDeclaration {
            declaration_index,
            contribution: key.clone(),
            binding: binding.clone(),
        });
        if let Some(name) = &syntax.name_range {
            self.names.push((
                selected_target.clone(),
                DeclarationLocation {
                    source: SourceUnitSpan {
                        unit,
                        range: TextRange {
                            start: name.start,
                            end: name.end,
                        },
                    },
                    uri: uri.clone(),
                },
            ));
            if file.path() == &self.source.path
                && range.start() <= self.source.offset as u64
                && (self.source.offset as u64) < range.end()
                && (self.enclosing
                    || (name.start <= self.source.offset as u64
                        && (self.source.offset as u64) < name.end))
            {
                let size = range.end() - range.start();
                if size < self.selected_size {
                    self.selected_size = size;
                    self.selected = Some(selected_target.clone());
                }
            }
        }
        Ok(())
    }
}

fn declaration(
    contribution: &Contribution,
    binding: &DeclarationBinding,
    combined: &PortableSymbolFacts,
    supplied: &PortableSymbolFacts,
    source: &str,
    unit: &SourceUnitId,
) -> Result<rift_protocol::symbol_read::SymbolDeclaration, RiftError> {
    let range = binding.range();
    let starts = rift_core::line::line_starts(source);
    let missing = || unavailable("the provider declaration facts have no original association");
    Ok(rift_protocol::symbol_read::SymbolDeclaration {
        origin: AssembledSymbol::wire_origin(contribution.origin()),
        path: None,
        unit: Some(unit.clone()),
        range: TextRange {
            start: range.start(),
            end: range.end(),
        },
        line: rift_core::line::line_of(&starts, range.start()),
        node: binding.node().cloned(),
        source: None,
        source_complete: false,
        signature_indices: rift_core::fact_indices(
            combined.signatures_slice(),
            supplied.signatures_slice(),
            missing,
        )?,
        type_indices: rift_core::fact_indices(
            combined.type_bindings(),
            supplied.type_bindings(),
            missing,
        )?,
        documentation_indices: rift_core::fact_indices(
            combined.documentation_blocks(),
            supplied.documentation_blocks(),
            missing,
        )?,
    })
}

fn merge_payload(
    facts: &mut QueryFacts,
    payload: QuerySymbol,
    existing: Option<usize>,
    bindings: &mut [QueryProviderDeclaration],
) -> usize {
    let symbol_index = existing.unwrap_or(facts.symbols.len());
    if let Some(index) = existing {
        let original = &mut facts.symbols[index];
        for binding in bindings {
            let declaration = &payload.declarations[binding.declaration_index];
            let index = original
                .declarations
                .iter()
                .position(|held| held == declaration)
                .unwrap_or_else(|| {
                    let index = original.declarations.len();
                    original.declarations.push(declaration.clone());
                    index
                });
            binding.declaration_index = index;
        }
    } else {
        facts.symbols.push(payload);
    }
    symbol_index
}

fn capture_edges(
    graph: &NormalizedGraph,
    snapshot: &mut QueryProviderSnapshot,
    cancelled: &(dyn Fn() -> bool + Sync),
    budget: &mut Option<&mut crate::QueryFactBudget<'_>>,
) -> Result<(), RiftError> {
    for reference in graph.references() {
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        if let Some(budget) = budget {
            use get_size2::GetSize;
            budget.charge_material(reference.get_size())?;
        }
        snapshot.references.push(QueryProviderReference {
            source: reference.source().clone(),
            binding: reference.binding().clone(),
            role: reference.role(),
            facet: rift_analysis::role_facet(reference.role()),
            targets: reference.targets().to_vec(),
        });
    }
    for relationship in graph.relationships() {
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        if let Some(budget) = budget {
            use get_size2::GetSize;
            budget.charge_material(relationship.get_size())?;
        }
        snapshot.relationships.push(QueryProviderRelationship {
            source: relationship.source().clone(),
            kind: relationship.kind(),
            facet: rift_analysis::relationship_facet(relationship.kind()),
            target: relationship.target().clone(),
            derivation: relationship.derivation(),
            occurrence: relationship.occurrence().cloned(),
        });
    }
    Ok(())
}
