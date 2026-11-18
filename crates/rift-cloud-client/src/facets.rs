use rift_core::SourceUnitId;
use rift_error::RiftError;

use rift_protocol::{
    identity::{SymbolIdentity, SymbolOwner},
    read::{FACETS_SOURCE_BYTES_MAX, FacetsParams, FacetsResult, SymbolId, SyntaxElement},
};

use crate::{
    GlobalClient, PAGE_LIMIT_MAX, active_response_body_bytes_max, contract, failure, response,
    serialize_body, smaller_bound, supports_feature, validate_body_for_capabilities,
};

/// The capability feature a global API advertises when it serves `/v1/facets`.
const FACETS_FEATURE: &str = "facets";

/// The bounds one facets page answers within: the advertised page and source bounds, each cut
/// to the client's own.
struct FacetsPageBounds {
    facets_max: usize,
    syntax_element_source_bytes_max: usize,
}

/// The symbol a facets request lists, and the package or runtime release its identity names.
struct RequestedSymbol<'request> {
    id: &'request SymbolId,
    identity: SymbolIdentity,
}

impl GlobalClient {
    /// Lists the facets of one package or runtime symbol, with their source text, in its exact
    /// release.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the request or answer breaks the contract, the global API
    /// holds no declaration of the symbol, or the endpoint is unavailable.
    pub async fn facets(&self, request: &FacetsParams) -> Result<FacetsResult, RiftError> {
        rift_tracing::traced!(component = "global", operation = "facets", async {
            if !self.inner.enabled {
                return Err(failure::configuration_invalid(
                    "enabled",
                    "global client disabled",
                ));
            }
            let symbol = validate_request(request)?;
            let capabilities = self.get_capabilities().await?;
            if !supports_feature(&capabilities, FACETS_FEATURE) {
                return Err(failure::configuration_invalid(
                    "feature",
                    &format!("global API does not advertise feature: {FACETS_FEATURE}"),
                ));
            }
            let body = serialize_body(request, self.inner.config.max_request)?;
            validate_body_for_capabilities(&body, &capabilities, self.inner.config.max_request)?;
            let raw = self
                .request(
                    contract::Endpoint::Facets,
                    Some(body),
                    None,
                    active_response_body_bytes_max(&capabilities, self.inner.config.max_response),
                )
                .await;
            let raw = self.observed(raw).await?;
            let parsed = self.observed(response::facets(raw, symbol.id)).await?;
            let page_items_max = usize::try_from(PAGE_LIMIT_MAX).unwrap_or(usize::MAX);
            let bounds = FacetsPageBounds {
                facets_max: smaller_bound(capabilities.bounds.page_limit_max, page_items_max),
                syntax_element_source_bytes_max: smaller_bound(
                    capabilities.bounds.source_bytes_max,
                    self.inner.config.max_source,
                ),
            };
            if !valid_result(request, &symbol, &parsed.value, &bounds) {
                return self
                    .observed(Err(failure::response_invalid(
                        "facets",
                        "body",
                        "field failed validation",
                        Some(parsed.meta.status),
                    )))
                    .await;
            }
            Ok(parsed.value)
        })
        .await
    }
}

/// The symbol `request` lists the facets of, once the request holds the one form the global
/// API serves: a package or runtime `symbol`, with no `path` or `position`, and no `revision`
/// other than a release of the symbol's owner.
fn validate_request(request: &FacetsParams) -> Result<RequestedSymbol<'_>, RiftError> {
    let invalid =
        |field| failure::request_invalid("validate_request", field, "field failed validation");
    let id = &request.symbol;
    let identity = SymbolIdentity::parse(id.as_str())?;
    let owner = identity.owner();
    if !is_release_owner(owner) {
        return Err(invalid("symbol"));
    }
    let other_revision = request
        .revision
        .as_ref()
        .is_some_and(|revision| !revision.answers_for(owner));
    let unserved = [("revision", other_revision)];
    if let Some((field, _)) = unserved.into_iter().find(|(_, present)| *present) {
        return Err(invalid(field));
    }
    if request.page == 0 {
        return Err(invalid("page"));
    }
    if !(1..=rift_protocol::read::FACETS_MAX_RESULTS).contains(&request.max_results) {
        return Err(invalid("max_results"));
    }
    Ok(RequestedSymbol { id, identity })
}

fn is_release_owner(owner: &SymbolOwner) -> bool {
    matches!(
        owner,
        SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
    )
}

/// Validates the selected symbol, exact release, syntax element sources, page bounds and pagination.
fn valid_result(
    request: &FacetsParams,
    symbol: &RequestedSymbol<'_>,
    result: &FacetsResult,
    bounds: &FacetsPageBounds,
) -> bool {
    let within_page = result.declarations.len() <= bounds.facets_max
        && result.declarations.len() as u64 <= request.max_results;
    let ordered = result
        .declarations
        .windows(2)
        .all(|pair| pair[0].declaration < pair[1].declaration);
    let declarations_valid = result.declarations.iter().all(|declaration| {
        let syntax = &declaration.syntax;
        let Some(root) = syntax.first() else {
            return false;
        };
        let root_valid = root.index == 0
            && root.parent.is_none()
            && root.declaration == Some(declaration.declaration)
            && root.kind == declaration.kind
            && root.range == declaration.range
            && root.unit == declaration.source.span.unit
            && root.range == declaration.source.span.range
            && root.source == declaration.source.text;
        root_valid
            && syntax.iter().all(|element| {
                element.declaration == Some(declaration.declaration)
                    && element.unit == root.unit
                    && element.range.start >= root.range.start
                    && element.range.end <= root.range.end
                    && source_matches(root, element)
                    && valid_syntax_element(element, symbol)
                    && element.source.len() <= bounds.syntax_element_source_bytes_max
            })
    });
    let source_bytes = result
        .declarations
        .iter()
        .flat_map(|declaration| &declaration.syntax)
        .try_fold(0_u64, |total, element| {
            total.checked_add(u64::try_from(element.source.len()).ok()?)
        });
    let within_source = result.declarations.len() <= 1
        || source_bytes.is_some_and(|total| total <= FACETS_SOURCE_BYTES_MAX);
    let pagination_valid = super::validate_pagination(
        &result.pagination,
        request.max_results,
        request.page,
        result.declarations.len(),
    )
    .is_ok()
        && result.pagination.page == request.page
        && result.pagination.max_results == request.max_results
        && result.pagination
            == rift_protocol::read::Pagination::new(
                request.page,
                request.max_results,
                result.pagination.total,
            );
    let revision_valid = result.revision.answers_for(symbol.identity.owner())
        && request
            .revision
            .as_ref()
            .is_none_or(|requested| requested == &result.revision);
    within_page
        && ordered
        && declarations_valid
        && within_source
        && pagination_valid
        && revision_valid
        && result.symbol == *symbol.id
}

/// Every syntax element quotes the same bytes as the declaration's complete source.
fn source_matches(root: &SyntaxElement, element: &SyntaxElement) -> bool {
    let range = element
        .range
        .start
        .checked_sub(root.range.start)
        .and_then(|start| usize::try_from(start).ok())
        .zip(
            element
                .range
                .end
                .checked_sub(root.range.start)
                .and_then(|end| usize::try_from(end).ok()),
        );
    range.is_some_and(|(start, end)| root.source.get(start..end) == Some(element.source.as_str()))
}

/// Checks the syntax element's source identity and the exact number of bytes its range spans.
fn valid_syntax_element(syntax_element: &SyntaxElement, symbol: &RequestedSymbol<'_>) -> bool {
    let identity = &symbol.identity;
    let owner = identity.owner();
    let in_release = SourceUnitId::parse(syntax_element.unit.as_str()).is_ok_and(|unit| {
        unit.source_owner() == Some(owner)
            || (&syntax_element.language == identity.language()
                && crate::symbol::typeshed_stdlib_unit(&unit, owner, identity.language()))
    });
    let spans_source = syntax_element
        .range
        .end
        .checked_sub(syntax_element.range.start)
        .is_some_and(|length| {
            u64::try_from(syntax_element.source.len()).is_ok_and(|bytes| bytes == length)
        });
    in_release && spans_source
}
