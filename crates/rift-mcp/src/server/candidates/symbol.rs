use super::RiftMcp;
use super::read_failure;
use crate::output::Json;
use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::symbol_read::{GetSymbolParams, GetSymbolResult};

impl RiftMcp {
    /// Reads one symbol by its canonical identity: a package or runtime symbol from the
    /// global API, a project symbol from the publication its revision selects.
    pub(in crate::server) async fn exact_symbol(
        &self,
        params: GetSymbolParams,
    ) -> Result<Json<GetSymbolResult>, super::ToolFailure> {
        let owner = SymbolIdentity::parse(params.id.as_str())
            .map_err(read_failure)?
            .owner()
            .clone();
        if matches!(
            owner,
            SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
        ) {
            return self.global_symbol(&params).await;
        }
        let git = params
            .revision
            .as_ref()
            .and_then(rift_protocol::revision::Revision::git_revision);
        if git.is_some() {
            return self
                .read_at(git, move |reads| reads.exact_symbol(&params, true))
                .await;
        }
        let deadline = self.request_deadline().await;
        let resolved = self
            .published_workspace(super::wire::ErrorPhase::Read, deadline)
            .await?;
        self.exact_symbol_in_publication(&resolved, params).await
    }

    /// Reads an exact symbol from the publication this request already resolved.
    pub(in crate::server) async fn exact_symbol_in_publication(
        &self,
        resolved: &super::super::ResolvedWorkspace,
        params: GetSymbolParams,
    ) -> Result<Json<GetSymbolResult>, super::ToolFailure> {
        let ready = resolved.published.preparation.is_none();
        let verified_current = ready && resolved.verified_current;
        self.current_tree_read(resolved, true, move |reads| {
            if verified_current {
                reads.exact_current_symbol(&params)
            } else {
                reads.exact_symbol(&params, ready)
            }
        })
        .await
    }

    /// Reads the facets of a package or runtime symbol from the global API, which holds the
    /// facts of that symbol's release.
    pub(in crate::server) async fn global_facets(
        &self,
        params: &rift_protocol::read::FacetsParams,
    ) -> Result<Json<rift_protocol::read::FacetsResult>, super::ToolFailure> {
        let deadline = self.request_deadline().await;
        let resolved = self
            .published_workspace(super::wire::ErrorPhase::Read, deadline)
            .await?;
        resolved
            .published
            .configuration
            .accepted_for_tool(super::wire::ErrorPhase::Read)?;
        let configuration = resolved.published.configuration.global_configuration();
        let mut selected = params.clone();
        if selected.revision.is_none() {
            selected.revision = Some(&selected.symbol).and_then(|symbol| {
                let identity = SymbolIdentity::parse(symbol.as_str()).ok()?;
                crate::global::workspace_release(
                    resolved.published.reads.dependency_context(),
                    identity.owner(),
                )
            });
        }
        within_deadline(
            deadline,
            "facets",
            self.global.symbol_kinds(&configuration, &selected),
        )
        .await
        .map(Json)
    }

    async fn global_symbol(
        &self,
        params: &GetSymbolParams,
    ) -> Result<Json<GetSymbolResult>, super::ToolFailure> {
        let deadline = self.request_deadline().await;
        let resolved = self
            .published_workspace(super::wire::ErrorPhase::Read, deadline)
            .await?;
        resolved
            .published
            .configuration
            .accepted_for_tool(super::wire::ErrorPhase::Read)?;
        let configuration = resolved.published.configuration.global_configuration();
        let mut selected = params.clone();
        if selected.revision.is_none() {
            selected.revision =
                SymbolIdentity::parse(selected.id.as_str())
                    .ok()
                    .and_then(|identity| {
                        crate::global::workspace_release(
                            resolved.published.reads.dependency_context(),
                            identity.owner(),
                        )
                    });
        }
        within_deadline(
            deadline,
            "get_symbol",
            self.global.exact_symbol(&configuration, &selected),
        )
        .await
        .map(Json)
    }
}

impl super::ReadAnswer for GetSymbolResult {
    fn warnings_mut(&mut self) -> &mut Vec<super::ReadWarning> {
        match self {
            Self::Found { warnings, .. }
            | Self::Missing { warnings, .. }
            | Self::Unavailable { warnings, .. } => warnings,
        }
    }
}

/// Awaits one global API read within the request's remaining time, refusing
/// `read_unavailable` naming `operation` once that time is spent.
async fn within_deadline<T>(
    deadline: crate::server::RequestDeadline,
    operation: &'static str,
    read: impl Future<Output = Result<T, RiftError>>,
) -> Result<T, super::ToolFailure> {
    tokio::time::timeout_at(deadline.at(), read)
        .await
        .map_err(|_| {
            read_failure(
                errors::server::read_unavailable()
                    .operation(operation)
                    .detail("a request spent its whole readiness budget")
                    .error(),
            )
        })?
        .map_err(read_failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{BlockingExecutor, ResolvedWorkspace, StaleIndexReason, ToolFailure};
    use rift_index::{PathChanges, WorkspaceIndexLimits};
    use rift_protocol::symbol_read::SymbolUnavailableReason;
    use serde_json::json;
    use std::future::{Future as _, poll_fn};
    use std::sync::Arc;
    use std::task::Poll;
    use std::time::Duration;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn request() -> TestResult<GetSymbolParams> {
        Ok(serde_json::from_value(json!({
            "id": "rift://symbol/local/typescript/.%2Findex.ts/known"
        }))?)
    }

    async fn fixture() -> TestResult<(tempfile::TempDir, RiftMcp)> {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        std::fs::write(
            directory.path().join("index.ts"),
            "export function known(x: number) { return x; }\n",
        )?;
        let server =
            RiftMcp::build_settled(directory.path(), WorkspaceIndexLimits::default()).await?;
        Ok((directory, server))
    }

    #[tokio::test]
    async fn accepted_exact_symbol_proof_requires_current_provenance() -> TestResult {
        let (_directory, server) = fixture().await?;
        let (_recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .capture("debug")
            .install()?;
        let accepted = server.exact_symbol(request()?).await?.0;
        assert!(matches!(accepted, GetSymbolResult::Found { .. }));

        let publication = Arc::clone(&server.published.read().await.current);
        let digests = publication.reads.workspace_digests();
        let changes = PathChanges::default();
        let reason = StaleIndexReason::RebuildSuperseded {
            epoch: publication.epoch + 1,
            changes: &changes,
        };
        let stale = ResolvedWorkspace::stale(
            publication,
            &digests,
            None,
            &reason,
            crate::server::wire::ErrorPhase::Read,
            Duration::ZERO,
        )?;
        assert!(stale.stale.is_none());
        assert_eq!(
            accepted,
            server
                .exact_symbol_in_publication(&stale, request()?)
                .await?
                .0
        );
        assert_eq!(accepted, server.exact_symbol(request()?).await?.0);
        assert_eq!(
            drain
                .queued_records()
                .iter()
                .filter(|record| record.operation() == "read.symbol.current_direct")
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn preparing_exact_read_keeps_its_typed_response() -> TestResult {
        let (_directory, assembled) = crate::server::tests::starting_fixture().await?;
        let result = assembled.server.exact_symbol(request()?).await?.0;
        assert!(matches!(
            result,
            GetSymbolResult::Unavailable {
                reason: SymbolUnavailableReason::IndexPreparing,
                ..
            }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn exact_symbol_waiting_for_capture_capacity_observes_cancellation() -> TestResult {
        let (_directory, mut server) = fixture().await?;
        server.blocking = BlockingExecutor::isolated(1, 1_000);
        let pool = server.blocking.clone();
        let (started, admitted) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::sync_channel(0);
        let task = tokio::spawn(async move {
            pool.run("hold exact symbol capture permit", move || {
                let _ = started.send(());
                let _ = held.recv_timeout(Duration::from_secs(2));
                Ok(())
            })
            .await
        });
        admitted.await?;

        let mut pending = Box::pin(server.exact_symbol(request()?));
        poll_fn(|context| {
            assert!(pending.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        server.validation.cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), pending).await;
        let _ = release.send(());
        task.await??;
        let Err(ToolFailure::Registered(failure, _)) = result? else {
            return Err("a cancelled capture must refuse the exact read".into());
        };
        assert_eq!(
            failure.registered().slug(),
            errors::server::read_cancelled::SLUG
        );
        Ok(())
    }
}
