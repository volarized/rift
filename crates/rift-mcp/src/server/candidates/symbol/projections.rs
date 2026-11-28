//! Optional sections of an exact symbol read.

use std::collections::BTreeSet;

use rift_cloud_client::{GlobalClient, edges::EdgeReadLimits};
use rift_error::RiftError;
use rift_protocol::edge_read::{EdgeInclude, EdgeSearchRequest, EdgeSearchResult, EdgeSelection};
use rift_protocol::identity::SymbolOwner;
use rift_protocol::mcp_symbol::{
    FoundSymbol, GetSymbolResult, SymbolDeclaration, SymbolEdge, SymbolSection, SymbolSections,
};
use rift_protocol::read::{
    ExactKind, FACETS_DECLARATIONS_MAX, FACETS_SOURCE_BYTES_MAX, FacetsParams, GetSymbolInclude,
    PackageIdentity, RuntimeIdentity, SourceExcerpt, SourceUnitId, SourceUnitSpan, SymbolHistory,
    SymbolId, TextRange, TraversalDirection,
};
use rift_protocol::revision::{Revision, RevisionPoint};
use rift_protocol::symbol_read::{
    GET_SYMBOL_MAX_RESULTS, GetSymbolParams as ExactSymbolParams,
    GetSymbolResult as ExactSymbolResult,
};
use rift_server::ReadService;

use super::{super::ToolFailure, within_deadline};
use crate::server::RequestDeadline;

/// Most declaration pages read when the accepted facets page is smaller than the symbol page.
const SOURCE_PAGES_MAX: usize = 64;

pub(super) fn project_symbol(
    reads: &ReadService,
    params: &ExactSymbolParams,
    sections: &SymbolSections,
    publication_ready: bool,
    verified_current: bool,
) -> Result<GetSymbolResult, RiftError> {
    let exact =
        reads.exact_symbol_without_alternatives(params, publication_ready, verified_current)?;
    let mut answer = GetSymbolResult::from_exact(exact, sections);
    if let GetSymbolResult::Found(found) = &mut answer {
        if sections.contains(&SymbolSection::Source) {
            project_sources(reads, &mut found.declarations);
        }
        if sections.contains(&SymbolSection::History) {
            found.history = match reads.exact_symbol_history(&params.id) {
                Ok(history) => history,
                Err(error) => {
                    section_warning("history", &error.to_string());
                    None
                }
            };
        }
        if sections.contains(&SymbolSection::Edges) {
            found.edges = Some(project_edges(reads, &params.id)?);
        }
    }
    Ok(answer)
}

fn project_sources(reads: &ReadService, declarations: &mut [SymbolDeclaration]) {
    let mut remaining = FACETS_SOURCE_BYTES_MAX;
    let mut omitted = false;
    for declaration in declarations {
        let binding = &declaration.binding;
        declaration.source = binding.path.as_ref().and_then(|path| {
            let text = reads.declaration_source(binding)?;
            excerpt(
                SourceUnitId::project(&path.0),
                &binding.range,
                text,
                &mut remaining,
            )
        });
        omitted |= declaration.source.is_none();
    }
    if omitted {
        section_warning(
            "source",
            "some declaration source was unavailable within the source byte bound",
        );
    }
}

fn excerpt(
    unit: SourceUnitId,
    range: &TextRange,
    text: &str,
    remaining: &mut u64,
) -> Option<SourceExcerpt> {
    *remaining = remaining.checked_sub(u64::try_from(text.len()).ok()?)?;
    Some(SourceExcerpt {
        span: SourceUnitSpan {
            unit,
            range: range.clone(),
        },
        text: text.to_owned(),
    })
}

fn project_edges(reads: &ReadService, id: &SymbolId) -> Result<Vec<SymbolEdge>, RiftError> {
    let key = rift_core::SymbolId::new(id.as_str())?;
    let store = reads.edges();
    let mut unique = BTreeSet::new();
    let mut remaining = FACETS_SOURCE_BYTES_MAX;
    let mut cut = false;
    for edge in store.outgoing(&key) {
        let kind = edge.kind();
        let target = edge.to().as_str();
        if unique.contains(&(kind, target)) {
            continue;
        }
        if !u64::try_from(unique.len()).is_ok_and(|count| count < GET_SYMBOL_MAX_RESULTS) {
            cut = true;
            break;
        }
        let bytes = kind
            .as_ref()
            .len()
            .checked_add(target.len())
            .and_then(|size| u64::try_from(size).ok());
        let Some(left) = bytes.and_then(|size| remaining.checked_sub(size)) else {
            cut = true;
            break;
        };
        remaining = left;
        unique.insert((kind, target));
    }
    if cut || !store.is_complete() {
        section_warning(
            "edges",
            "the captured index or selected edge bound omits some outgoing edges",
        );
    }
    let mut edges: Vec<_> = unique
        .into_iter()
        .map(|(kind, target)| SymbolEdge {
            kind: ExactKind(kind.as_ref().to_owned()),
            target: SymbolId(target.to_owned()),
        })
        .collect();
    edges.sort_unstable();
    Ok(edges)
}

pub(super) async fn release_sections(
    client: &GlobalClient,
    deadline: RequestDeadline,
    sections: &SymbolSections,
    id: &SymbolId,
    found: &mut FoundSymbol,
) {
    let revision = &found.revision;
    let offset = found.pagination.offset();
    let declarations = &mut found.declarations;
    let sources = async {
        if sections.contains(&SymbolSection::Source) && !declarations.is_empty() {
            attach_release_sources(client, deadline, id, revision, offset, declarations).await;
        }
    };
    let history = async {
        if sections.contains(&SymbolSection::History) {
            release_history(client, deadline, id, revision).await
        } else {
            None
        }
    };
    let edges = async {
        if sections.contains(&SymbolSection::Edges) {
            release_edges(client, deadline, id, revision).await
        } else {
            None
        }
    };
    let ((), history, edges) = tokio::join!(sources, history, edges);
    found.history = history;
    found.edges = edges;
}

async fn attach_release_sources(
    client: &GlobalClient,
    deadline: RequestDeadline,
    id: &SymbolId,
    revision: &Revision,
    offset: usize,
    declarations: &mut [SymbolDeclaration],
) {
    let result = release_sources(client, deadline, id, revision, offset, declarations).await;
    if let Err(error) = result {
        section_warning("source", &format!("{error:?}"));
    } else if declarations
        .iter()
        .any(|declaration| declaration.source.is_none())
    {
        section_warning(
            "source",
            "some released declaration source was unavailable within the source bounds",
        );
    }
}

async fn release_sources(
    client: &GlobalClient,
    deadline: RequestDeadline,
    id: &SymbolId,
    revision: &Revision,
    offset: usize,
    declarations: &mut [SymbolDeclaration],
) -> Result<(), ToolFailure> {
    let capabilities = within_deadline(deadline, "get_symbol", client.get_capabilities()).await?;
    let per_page = usize::try_from(capabilities.bounds.page_limit_max)
        .unwrap_or(FACETS_DECLARATIONS_MAX)
        .clamp(1, FACETS_DECLARATIONS_MAX);
    let first = offset / per_page + 1;
    let last = offset.saturating_add(declarations.len().saturating_sub(1)) / per_page + 1;
    let mut remaining = FACETS_SOURCE_BYTES_MAX;
    let mut request = FacetsParams {
        symbol: id.clone(),
        revision: Some(revision.clone()),
        page: first as u64,
        max_results: per_page as u64,
    };
    for page in (first..=last).take(SOURCE_PAGES_MAX) {
        if remaining == 0 {
            break;
        }
        request.page = page as u64;
        let answer = within_deadline(deadline, "get_symbol", client.facets(&request)).await?;
        crate::output::log_warnings(&answer.warnings);
        for facet in answer.declarations {
            let position = usize::try_from(facet.declaration)
                .ok()
                .and_then(|position| position.checked_sub(offset));
            let Some(declaration) = position.and_then(|position| declarations.get_mut(position))
            else {
                continue;
            };
            declaration.source =
                release_excerpt(&declaration.binding, facet.source, &mut remaining);
        }
    }
    Ok(())
}

fn release_excerpt(
    binding: &rift_protocol::symbol_read::SymbolDeclaration,
    source: SourceExcerpt,
    remaining: &mut u64,
) -> Option<SourceExcerpt> {
    if binding.unit.as_ref()? != &source.span.unit || binding.range.end > source.span.range.end {
        return None;
    }
    let start = usize::try_from(binding.range.start.checked_sub(source.span.range.start)?).ok()?;
    let end = usize::try_from(binding.range.end.checked_sub(source.span.range.start)?).ok()?;
    let text = source.text.get(start..end)?;
    excerpt(source.span.unit, &binding.range, text, remaining)
}

async fn release_history(
    client: &GlobalClient,
    deadline: RequestDeadline,
    id: &SymbolId,
    revision: &Revision,
) -> Option<SymbolHistory> {
    let request = ExactSymbolParams {
        id: id.clone(),
        revision: Some(revision.clone()),
        page: 1,
        max_results: 1,
        include: GetSymbolInclude {
            history: true,
            documentation: false,
        },
    };
    match within_deadline(deadline, "get_symbol", client.get_symbol(&request)).await {
        Ok(ExactSymbolResult::Found {
            history, warnings, ..
        }) => {
            crate::output::log_warnings(&warnings);
            history
        }
        Ok(_) => {
            section_warning(
                "history",
                "the selected release did not supply symbol history",
            );
            None
        }
        Err(error) => {
            section_warning("history", &format!("{error:?}"));
            None
        }
    }
}

async fn release_edges(
    client: &GlobalClient,
    deadline: RequestDeadline,
    id: &SymbolId,
    revision: &Revision,
) -> Option<Vec<SymbolEdge>> {
    let RevisionPoint::Release(owner, version) = revision.point() else {
        return None;
    };
    let (packages, runtimes) = match owner {
        SymbolOwner::Package {
            manager,
            registry,
            name,
        } => (
            vec![PackageIdentity {
                manager: manager.clone(),
                registry: registry.clone(),
                name: name.clone(),
                version: version.to_owned(),
            }],
            Vec::new(),
        ),
        SymbolOwner::Runtime { runtime } => (
            Vec::new(),
            vec![RuntimeIdentity {
                runtime: runtime.clone(),
                version: version.to_owned(),
            }],
        ),
        _ => return None,
    };
    let request = EdgeSearchRequest {
        edges: EdgeSelection {
            seeds: vec![id.clone()],
            direction: TraversalDirection::Outgoing,
            kinds: Vec::new(),
        },
        packages,
        runtimes,
        include: EdgeInclude::default(),
        view: None,
    };
    let limits = EdgeReadLimits {
        requests_max: 1,
        records_max: GET_SYMBOL_MAX_RESULTS,
        bytes_max: FACETS_SOURCE_BYTES_MAX,
    };
    match within_deadline(deadline, "get_symbol", client.edge_pages(&request, limits)).await {
        Ok(EdgeSearchResult::Page(page)) => {
            crate::output::log_warnings(&page.warnings);
            let edges: BTreeSet<_> = page
                .groups
                .into_iter()
                .flat_map(|group| group.edges)
                .map(|record| SymbolEdge {
                    kind: record.edge.kind,
                    target: record.edge.to,
                })
                .collect();
            Some(edges.into_iter().collect())
        }
        Ok(EdgeSearchResult::Unavailable(reason)) => {
            section_warning("edges", &format!("{reason:?}"));
            None
        }
        Err(error) => {
            section_warning("edges", &format!("{error:?}"));
            None
        }
    }
}

fn section_warning(section: &'static str, detail: &str) {
    rift_tracing::warn!(
        component = "mcp",
        operation = "read.symbol.section",
        section,
        detail,
        "symbol section is unavailable"
    );
}
