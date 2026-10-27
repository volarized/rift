use rift_error::{RiftError, errors};
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{SearchParams, SearchScope};
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};
use rmcp::model::CallToolRequestParams;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::global::candidates::{CANDIDATE_BYTES_MAX, SavedCandidateBackend};

use super::RiftProxy;

mod query;
pub(crate) use query::{
    prepare_selected_services as prepare_query_services, replay as query_replay,
    retained as query_material, source as query_material_read,
};
mod read;
mod reservation;
#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(super) struct CandidateViews {
    rows: Mutex<Vec<SavedCandidateBytes>>,
    reservation_serial: std::sync::atomic::AtomicU64,
    views_max: u64,
    view_lifetime: Duration,
}

#[derive(Debug)]
struct SavedCandidateBytes {
    key: [u8; 32],
    expires: SystemTime,
    bytes: Vec<u8>,
    reservation: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "operation",
    content = "response",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum SavedCandidateRead<Backend = Box<SavedCandidateBackend>> {
    Symbol(Backend),
}

impl Default for CandidateViews {
    fn default() -> Self {
        Self::for_configuration(&rift_protocol::configuration::ServerConfiguration::default())
    }
}

impl CandidateViews {
    pub(super) fn for_configuration(
        server: &rift_protocol::configuration::ServerConfiguration,
    ) -> Self {
        Self {
            rows: Mutex::new(Vec::new()),
            reservation_serial: std::sync::atomic::AtomicU64::new(1),
            views_max: server.retained_views,
            view_lifetime: Duration::from_millis(server.view_lifetime.milliseconds()),
        }
    }
}

impl CandidateViews {
    pub(super) fn backend(
        &self,
        id: &CapturedViewId,
        now: SystemTime,
    ) -> Result<Result<Option<SavedCandidateBackend>, SymbolUnavailableReason>, RiftError> {
        self.backend_for(id, None, now)
    }

    pub(super) fn backend_for(
        &self,
        id: &CapturedViewId,
        owner: Option<&SymbolOwner>,
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
            let accepted = saved_backends(&row.bytes)?;
            let matching = accepted.into_iter().filter(|accepted| {
                accepted.view.id == *id
                    && owner.is_none_or(|owner| {
                        accepted.owner.as_ref().is_none_or(|actual| {
                            super::identity::public_owner(actual, &accepted.backend)
                                .is_ok_and(|actual| &actual == owner)
                        })
                    })
            });
            for accepted in matching {
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
                if owner.is_none() {
                    break;
                }
            }
        }
        if expired {
            return Ok(Err(SymbolUnavailableReason::ViewExpired));
        }
        Ok(Ok(selected))
    }
}

fn saved_backends(bytes: &[u8]) -> Result<Vec<SavedCandidateBackend>, RiftError> {
    let SavedCandidateRead::Symbol(backend) =
        serde_json::from_slice::<SavedCandidateRead>(bytes)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    Ok(vec![*backend])
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
            SearchScope::ProfileLocal { name } => {
                let key = format!("#{name}");
                self.backends
                    .get_key_value(&key)
                    .map(|(name, backend)| vec![(name.as_str(), backend)])
                    .ok_or_else(|| self.extra.refusal(&key, "search", "scope"))
            }
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
        if !matches!(request.name.as_ref(), "nodes" | "search") {
            return Ok(None);
        }
        let arguments = request.arguments.clone().unwrap_or_default();
        let _: rift_protocol::query::QueryParams = serde_json::from_value(arguments.into())
            .map_err(|source| {
                errors::mcp::parameter_invalid()
                    .tool(request.name.as_ref())
                    .field("arguments")
                    .accepted(source.to_string())
                    .error()
            })?;
        self.capture_query_read(request).await
    }
}

fn invalid(field: &str, detail: &str) -> RiftError {
    errors::server::read_invalid()
        .field(field)
        .violation(detail)
        .error()
}
