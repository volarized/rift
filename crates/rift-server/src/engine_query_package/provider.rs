//! Provider records captured by the same admitted package analysis.

use std::collections::BTreeMap;

use lsp_types::Uri;
use rift_analysis::PackageAnalysis;
use rift_core::{FileDigest, WorkspaceId};
use rift_protocol::identity::{SourceDigest, SymbolIdentity, SymbolOwner};
use rift_protocol::read::{SourceUnitId, SourceUnitSpan, TextRange};
use rift_provider::{AssembledSymbol, NormalizedTarget};
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
    let selection = match selection {
        Some(selection) => selection.clone(),
        None => {
            let bytes = rift_protocol::canonical::canonical_json(&inputs)
                .map_err(|error| unavailable(error.to_string()))?;
            rift_search::query::QueryProviderSelection {
                root: WorkspaceId::new(root.as_str())?,
                owner: owner.clone(),
                index_revision: graph.index_revision(),
                source_revision: graph.source_revision(),
                tree_revision: graph.tree_revision(),
                fingerprint: SourceDigest::parse(
                    &data_encoding::HEXLOWER.encode(FileDigest::of(bytes.as_bytes()).as_bytes()),
                )?,
                configuration: rift_search::query::QueryProviderSelection::configuration_digest(
                    configuration,
                )?,
            }
        }
    };
    let context = QueryProviderContext {
        selection,
        root: WorkspaceId::new(root.as_str())?,
        owner,
        index_revision: graph.index_revision(),
        source_revision: graph.source_revision(),
        tree_revision: graph.tree_revision(),
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
    let mut names = Vec::new();
    let mut selected = None;
    let mut selected_size = u64::MAX;
    for record in graph.records() {
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        if let Some(budget) = &mut budget {
            use get_size2::GetSize;
            for key in record.contributions() {
                if let Some(contribution) = graph.contribution(key) {
                    budget.charge_material(contribution.get_size())?;
                }
            }
        }
        let Some(assembled) = semantics.assembled_record(record) else {
            continue;
        };
        let Some(combined) = assembled.facts() else {
            continue;
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
        let symbol_index = existing.unwrap_or(facts.symbols.len());
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
            context: Some(std::sync::Arc::clone(&shared_context)),
            target,
        };
        for key in record.contributions() {
            let Some(contribution) = graph.contribution(key) else {
                continue;
            };
            let Some(binding) = contribution.source() else {
                continue;
            };
            let Some(supplied) = contribution.facts() else {
                continue;
            };
            let Some(file) = files
                .iter()
                .find(|file| file.placement().unit() == binding.unit())
            else {
                continue;
            };
            let file = file.file();
            let Some(uri) = uris.get(file.path().as_str()) else {
                continue;
            };
            let range = binding.range();
            let Some(syntax) = file.enclosing_symbol(range.start(), range.end()) else {
                continue;
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
            let starts = rift_core::line::line_starts(file.source());
            let missing =
                || unavailable("the provider declaration facts have no original association");
            let declaration = rift_protocol::symbol_read::SymbolDeclaration {
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
            };
            let declaration_index = payload.declarations.len();
            payload.declarations.push(declaration);
            bindings.push(QueryProviderDeclaration {
                declaration_index,
                contribution: key.clone(),
                binding: binding.clone(),
            });
            if let Some(name) = &syntax.name_range {
                names.push((
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
                if file.path() == &source.path
                    && range.start() <= source.offset as u64
                    && (source.offset as u64) < range.end()
                    && (enclosing
                        || (name.start <= source.offset as u64
                            && (source.offset as u64) < name.end))
                {
                    let size = range.end() - range.start();
                    if size < selected_size {
                        selected_size = size;
                        selected = Some(selected_target.clone());
                    }
                }
            }
        }
        if bindings.is_empty() {
            continue;
        }
        if let Some(index) = existing {
            let original = &mut facts.symbols[index];
            for binding in &mut bindings {
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
    }
    for reference in graph.references() {
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        if let Some(budget) = &mut budget {
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
        if let Some(budget) = &mut budget {
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
    facts.providers.push(snapshot);
    Ok(CapturedProviders { names, selected })
}
