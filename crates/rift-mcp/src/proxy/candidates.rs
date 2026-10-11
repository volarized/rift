use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::{SearchParams, SearchScope};
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, ServerResult,
};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::global::candidates::{CANDIDATE_BYTES_MAX, SearchCandidatePool};
use crate::server::candidates::{
    SearchCandidateRequest, SearchCandidateResponse, SearchCandidateResult,
};

use super::RiftProxy;

mod read;
mod reservation;

#[derive(Debug)]
pub(super) enum CandidateAnswer {
    Captured(CapturedCandidates),
    Response(CallToolResult),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CapturedCandidates {
    pub(super) pools: Vec<SearchCandidatePool>,
    pub(super) ranking: rift_protocol::configuration::RankingConfiguration,
    pub(super) results_max: usize,
}

#[derive(Debug)]
pub(super) struct CandidateViews {
    rows: Mutex<Vec<SavedCandidateBytes>>,
    reservation_serial: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
struct SavedCandidateBytes {
    key: [u8; 32],
    expires: SystemTime,
    bytes: Vec<u8>,
    reservation: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedCandidateSearch {
    params: SearchParams,
    view: CapturedView,
    captured: CapturedCandidates,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedCandidateBackend {
    owner: Option<SymbolOwner>,
    backend: String,
    root: String,
    view: CapturedView,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "operation",
    content = "response",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum SavedCandidateRead {
    Search(Box<SavedCandidateSearch>),
    Symbol(SavedCandidateBackend),
}

impl Default for CandidateViews {
    fn default() -> Self {
        Self {
            rows: Mutex::new(Vec::with_capacity(256)),
            reservation_serial: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

impl CandidateViews {
    fn save(
        &self,
        params: &SearchParams,
        captured: &CapturedCandidates,
        now: SystemTime,
    ) -> Result<CapturedView, RiftError> {
        let (view, expires) = captured_view(captured)?;
        if expires <= now {
            return Err(invalid("view", "captured search view has expired"));
        }
        let saved = SavedCandidateSearch {
            params: normalized_params(params),
            view: view.clone(),
            captured: captured.clone(),
        };
        let bytes = super::encode_bounded(
            &SavedCandidateRead::Search(Box::new(saved)),
            CANDIDATE_BYTES_MAX,
        )
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rows.retain(|row| row.expires > now);
        let retained = rows.iter().fold(
            rows.capacity()
                .checked_mul(std::mem::size_of::<SavedCandidateBytes>())
                .and_then(|size| size.checked_add(bytes.capacity())),
            |sum, row| sum.and_then(|sum| sum.checked_add(row.bytes.capacity())),
        );
        if rows.len() == 256 || retained.is_none_or(|size| size > CANDIDATE_BYTES_MAX) {
            return Err(invalid(
                "view",
                "captured search exceeds the byte or view bound",
            ));
        }
        let key = view_key(view.id.as_str());
        if rows.iter().any(|row| row.key == key) {
            return Err(invalid("view", "captured search view is already retained"));
        }
        rows.push(SavedCandidateBytes {
            key,
            expires,
            bytes,
            reservation: None,
        });
        Ok(view)
    }

    fn backend(
        &self,
        id: &CapturedViewId,
        now: SystemTime,
    ) -> Result<Result<Option<SavedCandidateBackend>, SymbolUnavailableReason>, RiftError> {
        let rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut selected = None;
        let mut expired = false;
        for row in rows.iter() {
            if row.reservation.is_some() {
                continue;
            }
            for accepted in saved_backends(&row.bytes)?
                .into_iter()
                .filter(|accepted| accepted.view.id == *id)
            {
                if row.expires <= now {
                    expired = true;
                    continue;
                }
                if selected
                    .as_ref()
                    .is_some_and(|previous| previous != &accepted)
                {
                    return Ok(Err(SymbolUnavailableReason::ViewContext));
                }
                selected = Some(accepted);
            }
        }
        if expired {
            return Ok(Err(SymbolUnavailableReason::ViewExpired));
        }
        Ok(Ok(selected))
    }
    fn selected(
        &self,
        params: &SearchParams,
        now: SystemTime,
    ) -> Result<
        Result<Option<SavedCandidateSearch>, rift_protocol::symbol_read::SymbolUnavailableReason>,
        RiftError,
    > {
        let Some(id) = &params.view else {
            return Ok(Ok(None));
        };
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = view_key(id.as_str());
        let Some(row) = rows.iter().find(|row| row.key == key) else {
            return Ok(Err(
                rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
            ));
        };
        if row.expires <= now {
            rows.retain(|row| row.expires > now);
            return Ok(Err(
                rift_protocol::symbol_read::SymbolUnavailableReason::ViewExpired,
            ));
        }
        if row.reservation.is_some() {
            return Ok(Err(SymbolUnavailableReason::ViewContext));
        }
        let saved: SavedCandidateRead = serde_json::from_slice(&row.bytes)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let SavedCandidateRead::Search(saved) = saved else {
            return Ok(Err(SymbolUnavailableReason::ViewContext));
        };
        if saved.view.id != *id || saved.params != normalized_params(params) {
            return Ok(Err(
                rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
            ));
        }
        Ok(Ok(Some(*saved)))
    }
}

fn saved_backends(bytes: &[u8]) -> Result<Vec<SavedCandidateBackend>, RiftError> {
    let saved: SavedCandidateRead = serde_json::from_slice(bytes)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    Ok(match saved {
        SavedCandidateRead::Symbol(backend) => vec![backend],
        SavedCandidateRead::Search(saved) => saved
            .captured
            .pools
            .into_iter()
            .map(|pool| SavedCandidateBackend {
                owner: pool.owner,
                backend: pool.backend,
                root: pool.root,
                view: pool.view,
            })
            .collect(),
    })
}

pub(crate) fn normalized_params(params: &SearchParams) -> SearchParams {
    let mut normalized = params.clone();
    normalized.view = None;
    normalized.page_index = 0;
    normalized
}

fn view_key(id: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(id.as_bytes()).into()
}

pub(crate) fn captured_expiry(view: &CapturedView) -> Result<SystemTime, RiftError> {
    let expires: jiff::Timestamp = view
        .expires_at
        .parse()
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    let seconds = u64::try_from(expires.as_second())
        .map_err(|_| invalid("view", "captured view expiry precedes the clock bound"))?;
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::new(
            seconds,
            u32::try_from(expires.subsec_nanosecond())
                .map_err(|_| invalid("view", "captured view expiry precedes the clock bound"))?,
        ))
        .ok_or_else(|| invalid("view", "captured view expiry exceeds the clock bound"))
}

fn captured_view(captured: &CapturedCandidates) -> Result<(CapturedView, SystemTime), RiftError> {
    let primary = captured
        .pools
        .iter()
        .find(|pool| pool.backend != "global")
        .ok_or_else(|| invalid("view", "captured project context is unavailable"))?;
    let mut view = primary.view.clone();
    let mut expires = captured_expiry(&view)?;
    let mut ids = std::collections::BTreeSet::new();
    for pool in &captured.pools {
        if !ids.insert(pool.view.id.as_str()) {
            return Err(invalid(
                "view",
                "captured backend view mapping is ambiguous",
            ));
        }
        let selected = captured_expiry(&pool.view)?;
        if selected < expires {
            expires = selected;
            view.expires_at.clone_from(&pool.view.expires_at);
        }
    }
    Ok((view, expires))
}

impl RiftProxy {
    fn candidate_backends<'a>(
        &'a self,
        params: &'a SearchParams,
    ) -> Result<Vec<(&'a str, &'a Self)>, RiftError> {
        match &params.scope {
            SearchScope::NamedLocal { name } => self
                .backends
                .get(name)
                .map(|backend| vec![(name.as_str(), backend)])
                .ok_or_else(|| self.extra.refusal(name, "search", "scope")),
            SearchScope::All => Ok(std::iter::once(("local", self))
                .chain(
                    self.backends
                        .iter()
                        .map(|(name, backend)| (name.as_str(), backend)),
                )
                .collect()),
            SearchScope::Local | SearchScope::Global => Ok(vec![("local", self)]),
        }
    }

    pub(super) async fn captured_search(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<rmcp::model::CallToolResponse, rmcp::ErrorData>>, RiftError> {
        if request.name != "search" {
            return Ok(None);
        }
        let arguments = request.arguments.clone().unwrap_or_default();
        super::encode_bounded(&arguments, CANDIDATE_BYTES_MAX)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let params: SearchParams = serde_json::from_value(arguments.into()).map_err(|source| {
            errors::mcp::parameter_invalid()
                .tool("search")
                .field("arguments")
                .accepted(source.to_string())
                .error()
        })?;
        if params.pattern.is_some()
            || params.traversal.is_some()
            || params.change.is_some()
            || params.rev.is_some()
            || params.target == rift_protocol::read::SearchParamsTarget::Commit
        {
            if params.view.is_some() {
                return Err(invalid("view", "captured search selector is unavailable"));
            }
            return Ok(None);
        }
        let previous = match self.candidate_views.selected(&params, SystemTime::now())? {
            Ok(previous) => previous,
            Err(reason) => return Ok(Some(unavailable_search(reason))),
        };
        let pools = previous
            .as_ref()
            .map_or(&[][..], |saved| saved.captured.pools.as_slice());
        if previous.is_some() && !self.candidate_context_matches(&params, pools)? {
            return Ok(Some(unavailable_search(
                SymbolUnavailableReason::ViewContext,
            )));
        }
        let captured = match self.captured_candidates(&params, pools).await? {
            Err(error) => return Ok(Some(Err(error))),
            Ok(CandidateAnswer::Response(result)) => return Ok(Some(Ok(result.into()))),
            Ok(CandidateAnswer::Captured(captured)) => captured,
        };
        let view = match previous {
            Some(saved) => {
                if super::encode_bounded(&saved.captured, CANDIDATE_BYTES_MAX)
                    .map_err(|source| errors::mcp::forward_failed().source(source).error())?
                    != super::encode_bounded(&captured, CANDIDATE_BYTES_MAX)
                        .map_err(|source| errors::mcp::forward_failed().source(source).error())?
                {
                    return Err(invalid("view", "captured backend context does not match"));
                }
                saved.view
            }
            None => self
                .candidate_views
                .save(&params, &captured, SystemTime::now())?,
        };
        let policy = &captured.ranking;
        let weights = rift_ranking::RankingWeights::new(
            policy.identifier_weight,
            policy.lexical_weight,
            policy.vector_weight,
            policy.fusion_k,
        )?;
        let mut merged = crate::global::candidates::merge_candidate_pools(
            &params,
            &captured.pools,
            weights,
            captured.results_max,
        )?;
        merged.view = Some(view);
        Ok(Some(crate::output::Json(merged).into_call_tool_result()))
    }
    fn candidate_context_matches(
        &self,
        params: &SearchParams,
        previous: &[SearchCandidatePool],
    ) -> Result<bool, RiftError> {
        if let SearchScope::NamedLocal { name } = &params.scope
            && !self.backends.contains_key(name)
        {
            return Ok(false);
        }
        for pool in previous.iter().filter(|pool| pool.backend != "global") {
            let selected = self
                .candidate_views
                .backend(&pool.view.id, SystemTime::now())?;
            let Ok(Some(selected)) = selected else {
                return Ok(false);
            };
            if selected.backend != pool.backend
                || selected.root != pool.root
                || selected.view != pool.view
                || selected.owner != pool.owner
            {
                return Ok(false);
            }
        }
        let accepted = self.candidate_backends(params)?;
        let projects = previous.iter().filter(|pool| pool.backend != "global");
        if projects.clone().count() != accepted.len() {
            return Ok(false);
        }
        for (name, backend) in accepted {
            let root = backend
                .accepted_root
                .as_deref()
                .ok_or_else(|| invalid("root", "accepted project root is unavailable"))?
                .to_str()
                .ok_or_else(|| invalid("root", "project root is not UTF-8"))?;
            if !projects
                .clone()
                .any(|pool| pool.backend == name && pool.root == root)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Acquires each accepted project once and retains its immutable candidate context.
    pub(super) async fn captured_candidates(
        &self,
        params: &SearchParams,
        previous: &[SearchCandidatePool],
    ) -> Result<Result<CandidateAnswer, rmcp::ErrorData>, RiftError> {
        let mut pools = Vec::new();
        let mut ranking = None;
        let mut results_max = 0;
        for (name, backend) in self.candidate_backends(params)? {
            let root = backend
                .accepted_root
                .as_deref()
                .ok_or_else(|| invalid("root", "accepted project root is unavailable"))?
                .to_str()
                .ok_or_else(|| invalid("root", "project root is not UTF-8"))?;
            let view = previous
                .iter()
                .find(|pool| pool.backend == name && pool.root == root)
                .map(|pool| pool.view.clone());
            if !previous.is_empty() && view.is_none() {
                return Err(invalid("view", "captured backend mapping does not match"));
            }
            let mut selected = params.clone();
            selected.view = None;
            selected.page_index = 0;
            selected.scope = if name == "local" {
                params.scope.clone()
            } else {
                selected.packages.clear();
                SearchScope::Local
            };
            let result = backend
                .candidate_response(SearchCandidateRequest {
                    params: selected,
                    view,
                })
                .await?;
            let mut response = match result {
                Err(error) => return Ok(Err(error)),
                Ok(result) if result.is_error == Some(true) => {
                    return Ok(Ok(CandidateAnswer::Response(result)));
                }
                Ok(result) => match decode_response(&result)? {
                    SearchCandidateResult::Captured(response) => *response,
                    SearchCandidateResult::Unavailable(answer) => {
                        validate_preparing_response(&answer)?;
                        return Ok(Ok(CandidateAnswer::Response(result)));
                    }
                },
            };
            validate_project_response(root, &response)?;
            for input in response
                .inputs
                .precise
                .iter_mut()
                .chain(&mut response.inputs.broad)
            {
                for item in &mut input.order {
                    super::identity::qualify_hit(&mut item.hit, name)?;
                }
            }
            super::identity::qualify_warnings(&mut response.inputs.warnings, name)?;
            if ranking.is_none() {
                ranking = Some(response.ranking);
                results_max = response.results_max;
                if let Some(global) = response.global {
                    validate_global_pool(&global)?;
                    pools.push(global);
                }
            } else if response.global.is_some() {
                return Err(invalid("scope", "named project returned global candidates"));
            }
            pools.push(SearchCandidatePool {
                backend: name.to_owned(),
                owner: Some(response.owner),
                root: root.to_owned(),
                view: response.view,
                inputs: response.inputs,
            });
            super::encode_bounded(&pools, CANDIDATE_BYTES_MAX)
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        }
        let ranking = ranking.ok_or_else(|| invalid("scope", "primary project is unavailable"))?;
        Ok(Ok(CandidateAnswer::Captured(CapturedCandidates {
            pools,
            ranking,
            results_max,
        })))
    }

    async fn candidate_response(
        &self,
        request: SearchCandidateRequest,
    ) -> Result<Result<CallToolResult, rmcp::ErrorData>, RiftError> {
        super::encode_bounded(&request, CANDIDATE_BYTES_MAX)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let value = serde_json::to_value(request)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let arguments = value
            .as_object()
            .cloned()
            .ok_or_else(|| invalid("arguments", "candidate request is not an object"))?;
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            CallToolRequestParams::new("search_candidates").with_arguments(arguments),
        ));
        self.forward_registered(request, |result| match result {
            ServerResult::CallToolResult(result) => Some(result),
            _ => None,
        })
        .await
    }
}

fn decode_response(result: &CallToolResult) -> Result<SearchCandidateResult, RiftError> {
    super::encode_bounded(&result, CANDIDATE_BYTES_MAX)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    let value = result.structured_content.clone().ok_or_else(|| {
        invalid(
            "structuredContent",
            "candidate response has no structured result",
        )
    })?;
    serde_json::from_value(value)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())
}

fn validate_preparing_response(
    answer: &rift_protocol::search_read::SearchUnavailable,
) -> Result<(), RiftError> {
    let rift_protocol::search_read::SearchUnavailable::Unavailable {
        reason,
        view,
        warnings,
    } = answer;
    if *reason != SymbolUnavailableReason::IndexPreparing
        || view.is_some()
        || !warnings.iter().any(|warning| {
            matches!(
                warning,
                rift_protocol::read::ReadWarning::LocalIndexPreparing { prepared, total, .. }
                    if total.is_none_or(|total| *prepared < total)
            )
        })
    {
        return Err(invalid("view", "captured search selector is unavailable"));
    }
    Ok(())
}

fn validate_project_response(
    root: &str,
    response: &SearchCandidateResponse,
) -> Result<(), RiftError> {
    if response.root != root {
        return Err(invalid(
            "root",
            "captured project root does not match accepted project",
        ));
    }
    match &response.owner {
        SymbolOwner::Local => {}
        _ => {
            return Err(invalid(
                "owner",
                "captured owner does not match accepted project",
            ));
        }
    }
    validate_inputs(&response.inputs, &response.owner)
}

fn validate_inputs(
    inputs: &rift_server::SearchCandidateInputs,
    owner: &SymbolOwner,
) -> Result<(), RiftError> {
    for input in inputs.precise.iter().chain(&inputs.broad) {
        for item in &input.order {
            if let rift_protocol::read::SearchHitTarget::Symbol { symbol } = &item.hit.hit {
                if let Some(id) = &symbol.id {
                    let identity = SymbolIdentity::parse(&id.0)?;
                    if identity.owner() != owner {
                        return Err(invalid(
                            "symbol.id",
                            "candidate owner does not match accepted project",
                        ));
                    }
                } else if item.hit.range.is_none()
                    || (item.hit.path.is_none() && item.hit.unit.is_none())
                {
                    return Err(invalid("range", "raw candidate has no physical binding"));
                }
            }
        }
    }
    Ok(())
}

fn validate_global_pool(pool: &SearchCandidatePool) -> Result<(), RiftError> {
    if pool.backend != "global" || pool.owner.is_some() {
        return Err(invalid("scope", "global candidate backend does not match"));
    }
    for input in pool.inputs.precise.iter().chain(&pool.inputs.broad) {
        for item in &input.order {
            if let rift_protocol::read::SearchHitTarget::Symbol { symbol } = &item.hit.hit {
                let id = symbol.id.as_ref().ok_or_else(|| {
                    invalid("symbol.id", "global candidate has no canonical identity")
                })?;
                if !matches!(
                    SymbolIdentity::parse(&id.0)?.owner(),
                    SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
                ) {
                    return Err(invalid("symbol.id", "global candidate has a project owner"));
                }
            }
        }
    }
    Ok(())
}

fn invalid(field: &str, detail: &str) -> RiftError {
    errors::server::read_invalid()
        .field(field)
        .violation(detail)
        .error()
}

fn unavailable_search(
    reason: SymbolUnavailableReason,
) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
    use rmcp::handler::server::tool::IntoCallToolResult;
    crate::output::Json(rift_protocol::search_read::SearchReadResult::Unavailable(
        rift_protocol::search_read::SearchUnavailable::Unavailable {
            reason,
            view: None,
            warnings: Vec::new(),
        },
    ))
    .into_call_tool_result()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::symbol_read::{CapturedViewId, SymbolUnavailableReason};
    use serde_json::json;
    use std::error::Error as _;

    fn params() -> SearchParams {
        serde_json::from_value(json!({"query":"beacon","scope":"all","limit":2})).expect("search")
    }

    #[test]
    fn preparing_response_rejects_wrong_reason_view_warning_and_extra_fields() {
        let valid = json!({
            "outcome": "unavailable",
            "reason": "index_preparing",
            "warnings": [{
                "code": "local_index_preparing", "prepared": 0,
                "detail": "selected local files are still being prepared"
            }]
        });
        let answer: rift_protocol::search_read::SearchUnavailable =
            serde_json::from_value(valid.clone()).expect("existing preparing outcome");
        validate_preparing_response(&answer).expect("warning matches preparing reason");
        for field in ["results", "root", "unexpected"] {
            let mut invalid = valid.clone();
            invalid[field] = json!([]);
            assert!(serde_json::from_value::<SearchCandidateResult>(invalid).is_err());
        }
        let mut wrong_reason = valid.clone();
        wrong_reason["reason"] = json!("read_failed");
        let wrong_reason =
            serde_json::from_value(wrong_reason).expect("existing read-failed outcome");
        assert!(validate_preparing_response(&wrong_reason).is_err());
        let mut no_warning = valid.clone();
        no_warning["warnings"] = json!([]);
        let no_warning = serde_json::from_value(no_warning).expect("existing empty warnings");
        assert!(validate_preparing_response(&no_warning).is_err());
        let mut complete_warning = valid.clone();
        complete_warning["warnings"][0]["total"] = json!(0);
        let complete_warning =
            serde_json::from_value(complete_warning).expect("existing complete progress fields");
        assert!(validate_preparing_response(&complete_warning).is_err());
        let mut other_warning = valid.clone();
        other_warning["warnings"][0]["code"] = json!("lexical_ranking_unavailable");
        other_warning["warnings"][0]
            .as_object_mut()
            .expect("existing warning object")
            .remove("prepared");
        let other_warning =
            serde_json::from_value(other_warning).expect("existing lexical warning");
        assert!(validate_preparing_response(&other_warning).is_err());
        let mut with_view = valid;
        with_view["view"] =
            serde_json::to_value(captured().pools[0].view.clone()).expect("existing captured view");
        let with_view = serde_json::from_value(with_view).expect("existing selected view");
        assert!(validate_preparing_response(&with_view).is_err());
    }

    fn pool(backend: &str, root: &str, key: &str, expiry: &str) -> SearchCandidatePool {
        SearchCandidatePool {
            backend: backend.to_owned(),
            owner: Some(SymbolOwner::Local),
            root: root.to_owned(),
            view: CapturedView {
                id: CapturedViewId::parse(key).expect("captured key"),
                expires_at: expiry.to_owned(),
            },
            inputs: serde_json::from_value(json!({"precise":[],"broad":[],"warnings":[]}))
                .expect("empty captured inputs"),
        }
    }

    fn captured() -> CapturedCandidates {
        CapturedCandidates {
            pools: vec![
                pool(
                    "local",
                    "/fixture/main",
                    "claims.primary",
                    "2030-01-01T00:15:00Z",
                ),
                pool(
                    "extra",
                    "/fixture/extra",
                    "claims.extra",
                    "2030-01-01T00:10:00Z",
                ),
            ],
            ranking: rift_protocol::configuration::RankingConfiguration::default(),
            results_max: 100,
        }
    }

    #[test]
    fn captured_search_project_owner_comes_from_same_root_and_view() {
        let captured = captured();
        let pool = &captured.pools[1];
        let id = SymbolIdentity::new(
            SymbolOwner::Local,
            serde_json::from_value(json!("rust")).expect("language"),
            vec!["fixture".to_owned(), "beacon".to_owned()],
        )
        .expect("modeled logical identity")
        .wire_identity();
        let hit = serde_json::from_value(json!({
            "hit": {"target": "symbol", "symbol": {
                "id": id, "language": "rust", "name": "beacon", "kind": "function"
            }}, "path": "lib.rs", "line": 1, "range": {"start": 0, "end": 10}
        }))
        .expect("actual modeled physical binding");
        let mut inputs = pool.inputs.clone();
        inputs.precise.push(rift_server::SearchCandidateInput {
            kind: rift_ranking::RankingInputKind::Identifier,
            order: vec![rift_server::SearchCandidateItem {
                hit,
                ranked: rift_ranking::RankedIdentity::new(
                    rift_ranking::DocumentIdentity::new("provider-record").expect("record"),
                    rift_ranking::FieldSet::of(rift_ranking::SearchableField::Name),
                ),
            }],
        });
        let mut response = SearchCandidateResponse {
            root: pool.root.clone(),
            owner: SymbolOwner::Local,
            view: pool.view.clone(),
            inputs,
            ranking: captured.ranking,
            results_max: captured.results_max,
            global: None,
        };
        validate_project_response("/fixture/extra", &response)
            .expect("actual local owner in named backend");
        response.owner = SymbolOwner::NamedLocal {
            name: "extra".to_owned(),
        };
        assert!(
            validate_project_response("/fixture/extra", &response).is_err(),
            "named owner must refuse a candidate established under Local"
        );
        response.inputs = rift_server::SearchCandidateInputs::default();
        assert!(
            validate_project_response("/fixture/extra", &response).is_err(),
            "an empty backend still retains its actual Local owner"
        );
        response.owner = SymbolOwner::NamedLocal {
            name: "foreign".to_owned(),
        };
        assert!(validate_project_response("/fixture/extra", &response).is_err());
        response.owner = SymbolOwner::Local;
        response.root = "/fixture/replaced".to_owned();
        assert!(validate_project_response("/fixture/extra", &response).is_err());
    }

    #[test]
    fn captured_search_backend_refuses_owner_replacement_under_same_view() {
        let store = CandidateViews::default();
        let now = SystemTime::UNIX_EPOCH;
        let first = captured();
        store.save(&params(), &first, now).expect("first capture");
        let mut second = first.clone();
        second.pools[0].view.id = CapturedViewId::parse("claims.second").expect("second view");
        second.pools[1].owner = Some(SymbolOwner::NamedLocal {
            name: "extra".to_owned(),
        });
        store.save(&params(), &second, now).expect("second capture");
        assert!(matches!(
            store.backend(&first.pools[1].view.id, now),
            Ok(Err(SymbolUnavailableReason::ViewContext))
        ));
    }

    #[test]
    fn captured_search_backend_preserves_exact_root_view_and_minimum_expiry() {
        let store = CandidateViews::default();
        let captured = captured();
        let now = SystemTime::UNIX_EPOCH;
        let view = store.save(&params(), &captured, now).expect("capture");
        let selected = store
            .backend(&captured.pools[1].view.id, now)
            .expect("decode")
            .expect("accepted")
            .expect("backend");
        assert_eq!(selected.backend, "extra");
        assert_eq!(selected.root, "/fixture/extra");
        assert_eq!(selected.view, captured.pools[1].view);
        assert!(matches!(
            store.backend(
                &captured.pools[0].view.id,
                captured_expiry(&view).expect("expiry")
            ),
            Ok(Err(SymbolUnavailableReason::ViewExpired))
        ));
        assert!(
            store
                .backend(
                    &CapturedViewId::parse("claims.independent").expect("view"),
                    now
                )
                .expect("decode")
                .expect("independent authority")
                .is_none()
        );
    }

    #[test]
    fn captured_search_backend_refuses_ambiguous_mapping_between_captures() {
        let store = CandidateViews::default();
        let now = SystemTime::UNIX_EPOCH;
        let first = captured();
        store.save(&params(), &first, now).expect("first capture");
        let mut second = first.clone();
        second.pools[0].view.id = CapturedViewId::parse("claims.second").expect("second view");
        second.pools[1].root = "/fixture/replaced".to_owned();
        store.save(&params(), &second, now).expect("second capture");
        assert!(matches!(
            store.backend(&first.pools[1].view.id, now),
            Ok(Err(SymbolUnavailableReason::ViewContext))
        ));
    }
    #[test]
    fn captured_search_keeps_backend_views_and_minimum_original_expiry() {
        let store = CandidateViews::default();
        let captured = captured();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let view = store.save(&params(), &captured, now).expect("capture");
        assert_eq!(view.id, captured.pools[0].view.id);
        assert_eq!(view.expires_at, captured.pools[1].view.expires_at);
        let mut request = params();
        request.view = Some(view.id.clone());
        request.page_index = 20;
        let saved = store
            .selected(&request, now)
            .expect("lookup")
            .expect("selected context")
            .expect("saved");
        assert_eq!(saved.view, view);
        assert_eq!(saved.captured.pools[0].view, captured.pools[0].view);
        assert_eq!(saved.captured.pools[1].view, captured.pools[1].view);
        assert!(matches!(
            store.selected(&request, captured_expiry(&view).expect("expiry")),
            Ok(Err(SymbolUnavailableReason::ViewExpired))
        ));
    }

    #[test]
    fn captured_search_refuses_changed_query_scope_limit_and_unknown_view() {
        let store = CandidateViews::default();
        let now = SystemTime::UNIX_EPOCH;
        let view = store.save(&params(), &captured(), now).expect("capture");
        for changed in [
            json!({"query":"other","scope":"all","limit":2}),
            json!({"query":"beacon","scope":"local","limit":2}),
            json!({"query":"beacon","scope":"all","limit":3}),
        ] {
            let mut request: SearchParams = serde_json::from_value(changed).expect("search");
            request.view = Some(view.id.clone());
            assert!(matches!(
                store.selected(&request, now),
                Ok(Err(SymbolUnavailableReason::ViewContext))
            ));
        }
        let mut request = params();
        request.view = Some(CapturedViewId::parse("claims.unknown").expect("key"));
        assert!(matches!(
            store.selected(&request, now),
            Ok(Err(SymbolUnavailableReason::ViewContext))
        ));
    }

    #[test]
    fn captured_search_refuses_duplicate_backend_view_ids_and_invalid_expiry() {
        let mut captured = captured();
        captured.pools[1].view.id = captured.pools[0].view.id.clone();
        assert_eq!(
            captured_view(&captured).expect_err("ambiguous view").slug(),
            errors::server::read_invalid::SLUG
        );
        captured.pools[1].view.id = CapturedViewId::parse("claims.extra").expect("key");
        captured.pools[1].view.expires_at = "not-a-time".to_owned();
        let error = captured_view(&captured).expect_err("expiry");
        assert_eq!(error.slug(), errors::mcp::forward_failed::SLUG);
        assert!(error.source().is_some());
    }

    #[test]
    fn captured_search_bounds_full_escaped_context_before_retention() {
        let mut captured = captured();
        captured.pools[1].root = "\u{0001}".repeat(CANDIDATE_BYTES_MAX / 4);
        let store = CandidateViews::default();
        assert!(
            store
                .save(&params(), &captured, SystemTime::UNIX_EPOCH)
                .is_err()
        );
        assert!(store.rows.lock().expect("rows").is_empty());
    }
}
