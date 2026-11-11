//! Revision-pinned facts for an explicitly selected package or runtime symbol.

use std::collections::BTreeSet;

use rift_error::RiftError;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::{Node, NodesParams, ReadWarning, Symbol, SymbolId};
use rift_protocol::revision::Revision;
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::{GetSymbolParams, GetSymbolResult, SymbolUnavailableReason};
use rift_search::sparql::{
    GraphDatasetBuilder, GraphFact, GraphLimits, GraphQuery, RevisionIri, SymbolAddress,
    SymbolSelection,
};

use super::super::super::ResolvedWorkspace;
use super::{RiftMcp, relationships::coverage_failure, unavailable};

const NODE_PAGES_MAX: usize = 64;

pub(super) struct ExactFacts {
    revision: RevisionIri,
    symbol: Option<Box<Symbol>>,
    nodes: Vec<Node>,
    source: bool,
    warnings: Vec<ReadWarning>,
}

impl ExactFacts {
    pub(super) fn take_warnings(&mut self) -> Vec<ReadWarning> {
        std::mem::take(&mut self.warnings)
    }

    pub(super) fn project(&self, dataset: &mut GraphDatasetBuilder) -> Result<(), RiftError> {
        let Some(symbol) = &self.symbol else {
            return Ok(());
        };
        let id = symbol.id.as_ref().ok_or_else(coverage_failure)?;
        dataset.insert(GraphFact::SymbolDetails {
            address: SymbolAddress::Established(id),
            name: &symbol.name,
            kind: &symbol.kind.0,
            language: &symbol.language,
            revision: &self.revision,
        })?;
        for node in &self.nodes {
            dataset.insert_declared_node(id, node, self.source)?;
        }
        Ok(())
    }
}

impl RiftMcp {
    pub(super) async fn acquire_exact_graph(
        &self,
        resolved: &ResolvedWorkspace,
        query: &GraphQuery,
        limits: GraphLimits,
    ) -> Result<Result<Option<ExactFacts>, SearchUnavailable>, RiftError> {
        let nodes = query.node_selection();
        let selection = nodes
            .map(|selected| selected.symbols)
            .or_else(|| query.symbol_selection());
        let Some(SymbolSelection::Identified(id)) = selection else {
            return Ok(Ok(None));
        };
        let identity = SymbolIdentity::parse(id)?;
        if identity.owner().is_local() {
            return Ok(Ok(None));
        }
        let configuration = resolved.published.configuration.global_configuration();
        let context = resolved.published.reads.dependency_context();
        let Some(revision) = selected_release(context, identity.owner())
            .filter(|_| configuration.enabled && !query.names_local_graph())
        else {
            return Ok(Err(unavailable(
                SymbolUnavailableReason::InsufficientCoverage,
            )));
        };
        let id = SymbolId(id.to_owned());
        let answer = self
            .global
            .exact_symbol(&configuration, &exact_request(&id, &revision))
            .await?;
        if answer.revision().is_some_and(|answer| answer != &revision) {
            return Ok(Err(unavailable(SymbolUnavailableReason::ViewContext)));
        }
        let mut result = ExactFacts {
            revision: RevisionIri::parse(&revision.to_string())?,
            symbol: None,
            nodes: Vec::new(),
            source: nodes.is_some_and(|selected| selected.source),
            warnings: Vec::new(),
        };
        match answer {
            GetSymbolResult::Found {
                symbol, warnings, ..
            } => {
                result.symbol = Some(symbol);
                result.warnings = warnings;
            }
            GetSymbolResult::Missing { warnings, .. } => {
                result.warnings = warnings;
                return Ok(Ok(Some(result)));
            }
            GetSymbolResult::Unavailable {
                reason, warnings, ..
            } => {
                return Ok(Err(SearchUnavailable::Unavailable {
                    reason,
                    view: None,
                    warnings,
                }));
            }
        }
        if nodes.is_some()
            && let Some(reason) = self
                .acquire_remote_nodes(&configuration, &id, &revision, limits, &mut result)
                .await?
        {
            return Ok(Err(reason));
        }
        Ok(Ok(Some(result)))
    }
    async fn acquire_remote_nodes(
        &self,
        configuration: &rift_protocol::configuration::GlobalConfiguration,
        id: &SymbolId,
        revision: &Revision,
        limits: GraphLimits,
        result: &mut ExactFacts,
    ) -> Result<Option<SearchUnavailable>, RiftError> {
        let mut request = NodesParams::of(id.clone());
        request.revision = Some(revision.clone());
        let mut cursors = BTreeSet::new();
        let mut remaining_bytes = limits.literal_bytes_max();
        for _ in 0..NODE_PAGES_MAX {
            let page = self.global.symbol_nodes(configuration, &request).await?;
            if &page.revision != revision || page.symbol.as_ref() != Some(id) {
                return Ok(Some(unavailable(SymbolUnavailableReason::ViewContext)));
            }
            if page.warnings.iter().any(|warning| {
                matches!(
                    warning,
                    ReadWarning::SourceUnavailable { .. }
                        | ReadWarning::ObjectUnavailable(_)
                        | ReadWarning::ResultsTruncated { .. }
                )
            }) {
                return Ok(Some(SearchUnavailable::Unavailable {
                    reason: SymbolUnavailableReason::InsufficientCoverage,
                    view: None,
                    warnings: page.warnings,
                }));
            }
            let charge = crate::proxy::encode_bounded(&page, remaining_bytes)
                .map_err(|_| coverage_failure())?
                .len();
            remaining_bytes -= charge;
            result.nodes.extend(page.nodes);
            for warning in page.warnings {
                if !result.warnings.contains(&warning) {
                    result.warnings.push(warning);
                }
            }
            let Some(cursor) = page.node_cursor else {
                return Ok(None);
            };
            if !cursors.insert(cursor.clone()) {
                return Err(coverage_failure());
            }
            request.node_cursor = Some(cursor);
        }
        Err(coverage_failure())
    }
}

pub(super) fn exact_request(id: &SymbolId, revision: &Revision) -> GetSymbolParams {
    GetSymbolParams {
        id: id.clone(),
        include: Vec::new(),
        revision: Some(revision.clone()),
        declaration_limit: 1,
        declaration_cursor: None,
    }
}

pub(super) fn selected_release(
    context: &rift_dependency::DependencyContext,
    owner: &SymbolOwner,
) -> Option<Revision> {
    let SymbolOwner::Runtime { runtime } = owner else {
        return crate::global::workspace_release(context, owner);
    };
    let versions: BTreeSet<_> = context
        .entries()
        .iter()
        .filter(|entry| {
            entry.manager == rift_dependency::STANDARD_LIBRARY_MANAGER
                && entry.availability == rift_protocol::dependencies::PackageAvailability::Canonical
                && rift_dependency::StandardLibrary::from_name(&entry.name)
                    .is_some_and(|library| library.runtime() == runtime)
        })
        .filter_map(|entry| entry.version.as_deref())
        .collect();
    if versions.len() != 1 {
        return None;
    }
    Revision::release(owner, versions.first()?).ok()
}

#[cfg(test)]
mod tests {
    use rift_dependency::{DependencyContext, StandardLibraryAnswer};
    use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry, PackageSelector};

    use super::*;

    #[test]
    fn runtime_owner_reads_only_an_exact_standard_library_version() {
        let runtime = SymbolOwner::Runtime {
            runtime: "cpython".to_owned(),
        };
        let mut context = DependencyContext::default();
        let entry = |name, selector| {
            PackageContextEntry::new(
                rift_dependency::STANDARD_LIBRARY_MANAGER,
                name,
                selector,
                PackageAvailability::Canonical,
            )
        };
        context.add_standard_libraries(StandardLibraryAnswer {
            entries: vec![
                entry("python", PackageSelector::Version("3.12.9".to_owned())),
                entry("node", PackageSelector::Requirement(">=0".to_owned())),
            ],
            ..StandardLibraryAnswer::default()
        });
        let release = selected_release(&context, &runtime).expect("exact CPython pin");
        assert_eq!(release.to_string(), "rift://revision/stdlib/cpython@3.12.9");
        assert!(
            selected_release(
                &context,
                &SymbolOwner::Runtime {
                    runtime: "node".to_owned()
                }
            )
            .is_none()
        );
        assert!(
            selected_release(
                &context,
                &SymbolOwner::Runtime {
                    runtime: "python".to_owned()
                }
            )
            .is_none()
        );
    }
}
