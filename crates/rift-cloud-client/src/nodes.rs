use rift_error::RiftError;

use rift_protocol::{
    identity::{SymbolIdentity, SymbolOwner, parse_source_unit_identity},
    read::{NODES_SOURCE_BYTES_MAX, Node, NodeUnit, NodesParams, NodesResult, SymbolId},
};

use crate::{
    CURSOR_BYTES_MAX, GlobalClient, PAGE_LIMIT_MAX, active_response_body_bytes_max, contract,
    failure, response, serialize_body, smaller_bound, supports_feature,
    validate_body_for_capabilities,
};

/// The capability feature a global API advertises when it serves `/v1/nodes`.
const NODES_FEATURE: &str = "nodes";

/// The bounds one nodes page answers within: the advertised page and source bounds, each cut
/// to the client's own.
struct NodesPageBounds {
    nodes_max: usize,
    node_source_bytes_max: usize,
}

/// The symbol a nodes request lists, and the package or runtime release its identity names.
struct RequestedSymbol<'request> {
    id: &'request SymbolId,
    owner: SymbolOwner,
}

impl GlobalClient {
    /// Lists the nodes of one package or runtime symbol, with their source text, in its exact
    /// release.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the request or answer breaks the contract, the global API
    /// holds no declaration of the symbol, or the endpoint is unavailable.
    pub async fn nodes(&self, request: &NodesParams) -> Result<NodesResult, RiftError> {
        rift_tracing::traced!(component = "global", operation = "nodes", async {
            if !self.inner.enabled {
                return Err(failure::configuration_invalid(
                    "enabled",
                    "global client disabled",
                ));
            }
            let symbol = validate_request(request)?;
            let capabilities = self.get_capabilities().await?;
            if !supports_feature(&capabilities, NODES_FEATURE) {
                return Err(failure::configuration_invalid(
                    "feature",
                    &format!("global API does not advertise feature: {NODES_FEATURE}"),
                ));
            }
            if request.node_cursor.as_ref().is_some_and(|cursor| {
                cursor.len() > smaller_bound(capabilities.bounds.cursor_bytes_max, CURSOR_BYTES_MAX)
            }) {
                return Err(failure::request_invalid(
                    "nodes",
                    "node_cursor",
                    "field failed validation",
                ));
            }
            let body = serialize_body(request, self.inner.config.max_request)?;
            validate_body_for_capabilities(&body, &capabilities, self.inner.config.max_request)?;
            let raw = self
                .request(
                    contract::Endpoint::Nodes,
                    Some(body),
                    None,
                    active_response_body_bytes_max(&capabilities, self.inner.config.max_response),
                )
                .await;
            let raw = self.observed(raw).await?;
            let parsed = self.observed(response::nodes(raw, symbol.id)).await?;
            let page_items_max = usize::try_from(PAGE_LIMIT_MAX).unwrap_or(usize::MAX);
            let bounds = NodesPageBounds {
                nodes_max: smaller_bound(capabilities.bounds.page_limit_max, page_items_max),
                node_source_bytes_max: smaller_bound(
                    capabilities.bounds.source_bytes_max,
                    self.inner.config.max_source,
                ),
            };
            if !valid_result(request, &symbol, &parsed.value, &bounds) {
                return self
                    .observed(Err(failure::response_invalid(
                        "nodes",
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

/// The symbol `request` lists the nodes of, once the request holds the one form the global
/// API serves: a package or runtime `symbol`, with no `path` or `position`, and no `revision`
/// other than the release the symbol's identity names.
fn validate_request(request: &NodesParams) -> Result<RequestedSymbol<'_>, RiftError> {
    let invalid =
        |field| failure::request_invalid("validate_request", field, "field failed validation");
    let id = request.symbol.as_ref().ok_or_else(|| invalid("symbol"))?;
    let owner = SymbolIdentity::parse(id.as_str())?.owner().clone();
    if !is_release_owner(&owner) {
        return Err(invalid("symbol"));
    }
    let other_revision = request
        .revision
        .as_ref()
        .is_some_and(|revision| !revision.answers_for(&owner));
    let unserved = [
        ("path", request.path.is_some()),
        ("position", request.position.is_some()),
        ("revision", other_revision),
    ];
    if let Some((field, _)) = unserved.into_iter().find(|(_, present)| *present) {
        return Err(invalid(field));
    }
    if request
        .node_cursor
        .as_ref()
        .is_some_and(|cursor| cursor.is_empty() || cursor.len() > CURSOR_BYTES_MAX)
    {
        return Err(invalid("node_cursor"));
    }
    Ok(RequestedSymbol { id, owner })
}

fn is_release_owner(owner: &SymbolOwner) -> bool {
    matches!(
        owner,
        SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
    )
}

/// Whether `result` answers `request` within `bounds`: every node sits in a source unit of
/// the symbol's own release and carries the bytes its range spans, the answer reports that
/// release as its revision, the page stays within its node and source bounds, and the view
/// and cursor are the ones the request allows.
fn valid_result(
    request: &NodesParams,
    symbol: &RequestedSymbol<'_>,
    result: &NodesResult,
    bounds: &NodesPageBounds,
) -> bool {
    let within_page = result.nodes.len() <= bounds.nodes_max;
    let nodes_valid = result
        .nodes
        .iter()
        .all(|node| valid_node(node, symbol) && node.source.len() <= bounds.node_source_bytes_max);
    let source_bytes = result.nodes.iter().try_fold(0_u64, |total, node| {
        total.checked_add(u64::try_from(node.source.len()).ok()?)
    });
    let within_source = result.nodes.len() <= 1
        || source_bytes.is_some_and(|total| total <= NODES_SOURCE_BYTES_MAX);
    let view_valid = request
        .view
        .as_ref()
        .is_none_or(|id| result.view.as_ref().is_some_and(|view| &view.id == id));
    let cursor_valid = result.node_cursor.as_ref().is_none_or(|cursor| {
        !result.nodes.is_empty() && !cursor.is_empty() && cursor.len() <= CURSOR_BYTES_MAX
    });
    let revision_valid = result.revision.answers_for(&symbol.owner);
    within_page && nodes_valid && within_source && view_valid && cursor_valid && revision_valid
}

/// Whether `node` is a node of `symbol`: written in a source unit of the symbol's release,
/// naming no other symbol, and carrying exactly as many bytes as its range spans.
fn valid_node(node: &Node, symbol: &RequestedSymbol<'_>) -> bool {
    let NodeUnit::Source(unit) = &node.unit else {
        return false;
    };
    let in_release = parse_source_unit_identity(unit.as_str())
        .is_ok_and(|(unit_owner, _)| unit_owner.as_ref() == Some(&symbol.owner));
    let names_symbol = node.of.as_ref().is_none_or(|of| of == symbol.id);
    let spans_source = node
        .range
        .end
        .checked_sub(node.range.start)
        .is_some_and(|length| u64::try_from(node.source.len()).is_ok_and(|bytes| bytes == length));
    in_release && names_symbol && spans_source
}
