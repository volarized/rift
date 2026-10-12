use super::{RiftMcp, read_failure};
use crate::output::Json;
use rift_error::errors;
use rift_protocol::source_read::{GetSourceParams, GetSourceResult};
use rift_protocol::symbol_read::SymbolUnavailableReason;
use std::{path::Path, sync::Arc};

impl RiftMcp {
    #[cfg(test)]
    pub(in crate::server) async fn exact_source(
        &self,
        params: GetSourceParams,
    ) -> Result<Json<GetSourceResult>, super::ToolFailure> {
        self.exact_source_with_context(params, None).await
    }

    pub(in crate::server) async fn exact_source_with_context(
        &self,
        params: GetSourceParams,
        context: Option<
            &std::sync::Mutex<Option<crate::global::candidates::SavedCandidateBackend>>,
        >,
    ) -> Result<Json<GetSourceResult>, super::ToolFailure> {
        if !params.is_valid() {
            return Err(read_failure(
                errors::server::read_invalid()
                    .field("source")
                    .violation("source selectors or byte range are invalid")
                    .error(),
            ));
        }
        if let Some(view) = &params.view
            && (params.unit.is_project() || self.has_captured_context(view))
        {
            let context = self.captured_context(view, "get_source")?;
            let views = self.local_views.get().ok_or_else(|| {
                super::captured_read_refusal(
                    "get_source",
                    view,
                    SymbolUnavailableReason::ViewExpired,
                )
            })?;
            return views
                .get_source(&params, Path::new(&context.root), &context.owner)
                .map(Json)
                .map_err(read_failure);
        }
        if !params.unit.is_project() {
            let deadline = self.request_deadline().await;
            let resolved = self
                .published_workspace(super::wire::ErrorPhase::Read, deadline)
                .await?;
            resolved
                .published
                .configuration
                .accepted_for_tool(super::wire::ErrorPhase::Read)?;
            let configuration = resolved.published.configuration.global_configuration();
            let result = tokio::time::timeout_at(
                deadline.at(),
                self.global.exact_source(&configuration, &params),
            )
            .await
            .map_err(|_| {
                read_failure(
                    errors::server::read_unavailable()
                        .operation("get_source")
                        .detail("a request spent its whole readiness budget")
                        .error(),
                )
            })?
            .map_err(read_failure)?;
            return source_context(context, result, configuration.endpoint);
        }
        let rev = params.rev.clone();
        let server = self.clone();
        let operation =
            move |reads: &rift_server::ReadService,
                  ready: bool,
                  warnings: Vec<rift_protocol::read::ReadWarning>| {
                let mut selected = params.clone();
                selected.rev = None;
                let input = reads
                    .local_search_view_input(ready)?
                    .with_warnings(warnings.clone())?;
                let root = input.root().to_path_buf();
                let owner = input.owner().clone();
                let held_warnings = input.warnings()?;
                let views = server.local_views.get().ok_or_else(|| {
                    errors::server::read_unavailable()
                        .operation("get_source")
                        .detail("captured selection is unavailable")
                        .error()
                })?;
                let cache = Arc::clone(&server.candidate_responses);
                let view =
                    views.capture_with(input, |view| cache.save_symbol(&root, &owner, view))?;
                let Some(view) = view else {
                    return Ok(GetSourceResult::Unavailable {
                        unit: selected.unit,
                        view: None,
                        reason: SymbolUnavailableReason::IndexPreparing,
                        warnings: held_warnings,
                    });
                };
                selected.view = Some(view.id);
                views.get_source(&selected, &root, &owner)
            };
        if rev.is_some() {
            return self
                .read_at(rev, move |reads| operation(reads, true, Vec::new()))
                .await;
        }
        let deadline = self.request_deadline().await;
        let resolved = self
            .published_workspace(super::wire::ErrorPhase::Read, deadline)
            .await?;
        let ready = resolved.published.preparation.is_none();
        self.current_captured_read(&resolved, false, move |reads, warnings| {
            operation(reads, ready, warnings)
        })
        .await
    }
}

impl super::ReadAnswer for GetSourceResult {
    fn warnings_mut(&mut self) -> &mut Vec<rift_protocol::read::ReadWarning> {
        match self {
            Self::Found { warnings, .. }
            | Self::Missing { warnings, .. }
            | Self::Unavailable { warnings, .. } => warnings,
        }
    }
}

fn source_context(
    context: Option<&std::sync::Mutex<Option<crate::global::candidates::SavedCandidateBackend>>>,
    result: GetSourceResult,
    root: String,
) -> Result<Json<GetSourceResult>, super::ToolFailure> {
    let view = match &result {
        GetSourceResult::Found { view, .. } | GetSourceResult::Missing { view, .. } => Some(view),
        GetSourceResult::Unavailable { view, .. } => view.as_ref(),
    };
    if let (Some(context), Some(view)) = (context, view) {
        let selected = crate::global::candidates::SavedCandidateBackend {
            backend: "global".to_owned(),
            root,
            owner: None,
            view: view.clone(),
        };
        crate::proxy::encode_bounded(&selected, super::BYTES_MAX)
            .map_err(super::transport_failure)?;
        *context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(selected);
    }
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::symbol_read::CapturedView;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    type SourceServer = (
        tempfile::TempDir,
        RiftMcp,
        [Arc<AtomicUsize>; 2],
        tokio::task::JoinHandle<()>,
    );

    async fn source_server() -> Result<SourceServer, Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let count = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
        let counted = Arc::clone(&count[1]);
        let capabilities_count = Arc::clone(&count[0]);
        let router = axum::Router::new()
            .route(
                "/rift/v1/capabilities",
                axum::routing::get(move || {
                    let counted = Arc::clone(&capabilities_count);
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            axum::body::Body::from(capabilities().to_string()),
                        )
                    }
                }),
            )
            .route(
                "/rift/v1/source",
                axum::routing::post(move |request: axum::http::Request<axum::body::Body>| {
                    let counted = Arc::clone(&counted);
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        let bytes = axum::body::to_bytes(
                            request.into_body(),
                            rift_cloud_client::REQUEST_BODY_BYTES_MAX,
                        )
                        .await
                        .expect("bounded source request body");
                        let request: GetSourceParams =
                            serde_json::from_slice(&bytes).expect("typed source request");
                        let view = CapturedView {
                            id: request.view.expect("remote request selects a view"),
                            expires_at: "2099-10-10T10:05:00Z".to_owned(),
                        };
                        let answer = GetSourceResult::Unavailable {
                            unit: request.unit,
                            view: Some(view),
                            reason: SymbolUnavailableReason::InsufficientCoverage,
                            warnings: Vec::new(),
                        };
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            axum::body::Body::from(
                                serde_json::to_string(&answer).expect("typed source response"),
                            ),
                        )
                    }
                }),
            );
        let http_task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let configuration =
            format!("[global]\nendpoint='http://127.0.0.1:{port}/rift'\nattempts=1\n");
        let (directory, server, _) =
            crate::server::tests::canonical_fixture_with_configuration(&configuration).await?;
        Ok((directory, server, count, http_task))
    }

    fn capabilities() -> serde_json::Value {
        serde_json::json!({
            "supported_package_managers": ["cargo"],
            "supported_features": ["resolutions", "search", "symbols"],
            "publication_format": "rift-package-index-v2",
            "analyzer_revision": "analyzer-v1",
            "corpus_revision": "corpus-v1",
            "required_search_fields": [
                "name",
                "qualified_name",
                "documentation",
                "signature"
            ],
            "bounds": {
                "request_body_bytes_max": rift_cloud_client::REQUEST_BODY_BYTES_MAX,
                "response_body_bytes_max": rift_cloud_client::RESPONSE_BODY_BYTES_MAX,
                "dependency_entries_max": rift_cloud_client::DEPENDENCY_ENTRIES_MAX,
                "query_bytes_max": rift_cloud_client::QUERY_BYTES_MAX,
                "query_terms_max": rift_cloud_client::QUERY_TERMS_MAX,
                "query_term_bytes_max": rift_cloud_client::QUERY_TERM_BYTES_MAX,
                "identifiers_max": rift_cloud_client::IDENTIFIERS_MAX,
                "identifier_bytes_max": rift_cloud_client::IDENTIFIER_BYTES_MAX,
                "packages_max": rift_cloud_client::PACKAGES_MAX,
                "page_limit_min": rift_cloud_client::PAGE_LIMIT_MIN,
                "page_limit_max": rift_cloud_client::PAGE_LIMIT_MAX,
                "page_limit_default": 20,
                "cursor_bytes_max": rift_cloud_client::CURSOR_BYTES_MAX,
                "candidate_pool_max": rift_cloud_client::CANDIDATE_POOL_MAX,
                "warnings_max": rift_cloud_client::WARNINGS_MAX,
                "source_bytes_max": rift_cloud_client::SOURCE_BYTES_MAX
            }
        })
    }

    fn corrupt_saved_root(
        server: &crate::RiftMcp,
        view: &CapturedView,
        root: &std::path::Path,
    ) -> TestResult {
        {
            let mut rows = server
                .candidate_responses
                .bytes
                .lock()
                .expect("test cache lock");
            let row = rows
                .iter_mut()
                .find(|row| row.id == view.id.as_str())
                .ok_or("saved view")?;
            let mut saved: super::super::symbol::SavedRead = serde_json::from_slice(&row.bytes)?;
            let super::super::symbol::SavedRead::Symbol(ref mut symbol) = saved else {
                return Err("source capture stores symbol authority".into());
            };
            symbol.root = root.join("another-root").to_string_lossy().into_owned();
            row.bytes = serde_json::to_vec(&saved)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn held_local_source_refuses_foreign_units_before_global_dispatch() -> TestResult {
        let (directory, server, count, http_task) = source_server().await?;
        let request: GetSourceParams = serde_json::from_value(json!({
            "unit":"rift://source/project/lib.rs"
        }))?;
        let Json(answer) = server.exact_source(request.clone()).await?;
        let GetSourceResult::Found {
            source,
            view,
            source_complete,
            ..
        } = answer
        else {
            return Err("initial project source must be found".into());
        };
        assert!(source_complete);
        assert_eq!(source.text, "pub fn beacon() {}\n");
        std::fs::write(directory.path().join("lib.rs"), "pub fn changed() {}\n")?;
        let mut held = request;
        held.view = Some(view.id.clone());
        let Json(answer) = server.exact_source(held.clone()).await?;
        assert!(
            matches!(answer, GetSourceResult::Found { source, view: returned, .. }
            if source.text == "pub fn beacon() {}\n" && returned == view)
        );
        for unit in [
            "rift://source/custom/lib.rs",
            "rift://source/npm/npmjs.org/beacon@1.0.0/lib.rs",
        ] {
            let mut foreign = held.clone();
            foreign.unit = rift_protocol::read::SourceUnitId::parse(unit)?;
            let Json(answer) = server.exact_source(foreign).await?;
            assert!(matches!(answer, GetSourceResult::Unavailable {
                reason: SymbolUnavailableReason::ViewContext, view: Some(returned), ..
            } if returned == view));
        }
        let foreign_symbol: rift_protocol::symbol_read::GetSymbolParams =
            serde_json::from_value(json!({
                "id":"rift://symbol/npm/npmjs.org/@types/node@26.6.4/typescript/node/buffer/Buffer",
                "view":view.id
            }))?;
        let Json(answer) = server.exact_symbol(foreign_symbol).await?;
        assert!(matches!(
            answer,
            rift_protocol::symbol_read::GetSymbolResult::Unavailable {
                reason: SymbolUnavailableReason::ViewContext,
                ..
            }
        ));
        assert_eq!(
            count[0].load(Ordering::SeqCst) + count[1].load(Ordering::SeqCst),
            0
        );
        corrupt_saved_root(&server, &view, directory.path())?;
        let Json(answer) = server.exact_source(held.clone()).await?;
        assert!(matches!(
            answer,
            GetSourceResult::Unavailable {
                reason: SymbolUnavailableReason::ViewContext,
                ..
            }
        ));
        {
            let mut rows = server
                .candidate_responses
                .bytes
                .lock()
                .expect("test cache lock");
            let row = rows
                .iter_mut()
                .find(|row| row.id == view.id.as_str())
                .ok_or("saved view")?;
            row.expires = std::time::SystemTime::UNIX_EPOCH;
        }
        held.unit = rift_protocol::read::SourceUnitId::parse("rift://source/custom/lib.rs")?;
        let Err(error) = server.exact_source(held).await else {
            return Err("expired held context must refuse".into());
        };
        let crate::output::ToolFailure::Registered(error, _) = error else {
            return Err("expiry retains registered error".into());
        };
        assert_eq!(
            error.registered().slug().as_str(),
            "rift.server.search_view_expired"
        );
        assert_eq!(
            count[0].load(Ordering::SeqCst) + count[1].load(Ordering::SeqCst),
            0
        );
        http_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn remote_custom_source_keeps_remote_view_and_dispatch() -> TestResult {
        let (_directory, server, count, http_task) = source_server().await?;
        let request: GetSourceParams = serde_json::from_value(json!({
            "unit":"rift://source/custom/lib.rs", "view":"a".repeat(64)
        }))?;
        assert!(!server.has_captured_context(request.view.as_ref().ok_or("remote view")?));
        let Json(answer) = server.exact_source(request.clone()).await?;
        assert!(matches!(answer, GetSourceResult::Unavailable {
            unit, view: Some(view), reason: SymbolUnavailableReason::InsufficientCoverage, ..
        } if unit == request.unit && Some(&view.id) == request.view.as_ref()));
        assert_eq!(count[0].load(Ordering::SeqCst), 1);
        assert_eq!(count[1].load(Ordering::SeqCst), 1);
        http_task.abort();
        Ok(())
    }
}
