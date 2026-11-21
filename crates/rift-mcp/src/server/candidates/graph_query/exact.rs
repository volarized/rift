//! Revision-pinned facts for an explicitly selected package or runtime symbol.

use std::collections::BTreeSet;

use rift_error::RiftError;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::{FacetsParams, ReadWarning, Symbol, SymbolId, SyntaxElement};
use rift_protocol::revision::Revision;
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::{GetSymbolParams, GetSymbolResult, SymbolUnavailableReason};
use rift_search::sparql::{
    GraphDatasetBuilder, GraphFact, GraphLimits, GraphQuery, RevisionIri, SymbolAddress,
    SymbolSelection,
};

use super::super::super::ResolvedWorkspace;
use super::{RiftMcp, edges::coverage_failure, unavailable};

const SYNTAX_ELEMENT_PAGES_MAX: usize = 64;

pub(super) struct ExactFacts {
    revision: RevisionIri,
    symbol: Option<Box<Symbol>>,
    history: Option<rift_protocol::read::SymbolHistory>,
    kinds: Vec<SyntaxElement>,
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
        if let Some(history) = &self.history {
            if !history.complete {
                return Err(coverage_failure());
            }
            dataset.insert_history(history)?;
        }
        for syntax_element in &self.kinds {
            dataset.insert_declared_syntax_element(id, syntax_element, self.source)?;
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
        let facets = query.syntax_element_selection();
        let selection = facets
            .map(|selected| selected.symbols)
            .or_else(|| query.history_selection())
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
        let mut request = exact_request(&id, &revision);
        request.include.history = query.history_selection().is_some();
        let answer = self.global.exact_symbol(&configuration, &request).await?;
        if answer.revision().is_some_and(|answer| answer != &revision) {
            return Ok(Err(unavailable(SymbolUnavailableReason::ViewContext)));
        }
        let mut result = ExactFacts {
            revision: RevisionIri::parse(&revision.to_string())?,
            symbol: None,
            history: None,
            kinds: Vec::new(),
            source: facets.is_some_and(|selected| selected.source),
            warnings: Vec::new(),
        };
        match answer {
            GetSymbolResult::Found {
                symbol,
                history,
                warnings,
                ..
            } => {
                if request.include.history && history.is_none() {
                    return Err(coverage_failure());
                }
                result.history = history;
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
        if facets.is_some()
            && let Some(reason) = self
                .acquire_remote_facets(&configuration, &id, &revision, limits, &mut result)
                .await?
        {
            return Ok(Err(reason));
        }
        Ok(Ok(Some(result)))
    }
    async fn acquire_remote_facets(
        &self,
        configuration: &rift_protocol::configuration::GlobalConfiguration,
        id: &SymbolId,
        revision: &Revision,
        limits: GraphLimits,
        result: &mut ExactFacts,
    ) -> Result<Option<SearchUnavailable>, RiftError> {
        let mut request = FacetsParams::of(id.clone());
        request.revision = Some(revision.clone());
        let mut remaining_bytes = limits.literal_bytes_max();
        for _ in 0..SYNTAX_ELEMENT_PAGES_MAX {
            let mut page = self.global.symbol_kinds(configuration, &request).await?;
            if &page.revision != revision || &page.symbol != id {
                return Ok(Some(unavailable(SymbolUnavailableReason::ViewContext)));
            }
            if page.warnings.iter().any(|warning| {
                matches!(
                    warning,
                    ReadWarning::SourceUnavailable { .. } | ReadWarning::ObjectUnavailable(_)
                )
            }) {
                return Ok(Some(SearchUnavailable::Unavailable {
                    reason: SymbolUnavailableReason::InsufficientCoverage,
                    view: None,
                    warnings: page.warnings,
                }));
            }
            // A producer bound on the facet page leaves the kinds it returned standing.
            if page
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::ResultsTruncated { .. }))
            {
                page.warnings
                    .retain(|warning| !matches!(warning, ReadWarning::ResultsTruncated { .. }));
                record_facet_cut("results_max");
            }
            let Ok(encoded) = crate::proxy::encode_bounded(&page, remaining_bytes) else {
                record_facet_cut("literal_bytes_max");
                return Ok(None);
            };
            remaining_bytes -= encoded.len();
            result.kinds.extend(
                page.declarations
                    .into_iter()
                    .flat_map(|declaration| declaration.syntax),
            );
            for warning in page.warnings {
                if !result.warnings.contains(&warning) {
                    result.warnings.push(warning);
                }
            }
            if request.page >= page.pagination.pages {
                return Ok(None);
            }
            request.revision = Some(page.revision);
            request.page += 1;
        }
        record_facet_cut("syntax_element_pages_max");
        Ok(None)
    }
}

/// Writes the one record naming the bound that ended a package facet acquisition: the
/// syntax kinds read before it stand, and the answer carries nothing for the cut.
fn record_facet_cut(bound: &'static str) {
    rift_tracing::warn!(
        component = "search",
        operation = "search.graph.acquire",
        bound = bound,
        "a package facet acquisition reached a bound; the answer holds the kinds it read"
    );
}

pub(super) fn exact_request(id: &SymbolId, revision: &Revision) -> GetSymbolParams {
    GetSymbolParams {
        id: id.clone(),
        include: rift_protocol::read::GetSymbolInclude::default(),
        revision: Some(revision.clone()),
        max_results: 1,
        page: 1,
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
