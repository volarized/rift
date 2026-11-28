use super::RiftMcp;
use super::read_failure;
use crate::output::Json;
use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::mcp_symbol::{GetSymbolParams, GetSymbolResult, SymbolSections};
use rift_protocol::symbol_read::GetSymbolParams as ExactSymbolParams;

mod projections;

impl RiftMcp {
    /// Reads a symbol and its requested sections from one selected revision.
    pub(in crate::server) async fn read_mcp_symbol(
        &self,
        params: GetSymbolParams,
    ) -> Result<Json<GetSymbolResult>, super::ToolFailure> {
        let sections = params.selection();
        let mut exact = params.into_exact(&sections);
        // The history section cannot refuse an already established symbol.
        exact.include.history = false;
        let owner = SymbolIdentity::parse(exact.id.as_str()).map_err(read_failure)?;
        if matches!(
            owner.owner(),
            SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
        ) {
            return self.global_mcp_symbol(exact, &sections).await;
        }
        let git = exact
            .revision
            .as_ref()
            .and_then(rift_protocol::revision::Revision::git_revision);
        if git.is_some() {
            return self
                .read_at(git, move |reads| {
                    projections::project_symbol(reads, &exact, &sections, true, false)
                })
                .await;
        }
        let deadline = self.request_deadline().await;
        let resolved = self
            .published_workspace(super::wire::ErrorPhase::Read, deadline)
            .await?;
        self.mcp_symbol_in_publication(&resolved, exact, sections)
            .await
    }

    /// Reads an exact symbol from the publication this request already resolved.
    async fn mcp_symbol_in_publication(
        &self,
        resolved: &super::super::ResolvedWorkspace,
        exact: ExactSymbolParams,
        sections: SymbolSections,
    ) -> Result<Json<GetSymbolResult>, super::ToolFailure> {
        let ready = resolved.published.preparation.is_none();
        let verified_current = resolved.verified_current;
        self.current_tree_read(resolved, true, move |reads| {
            projections::project_symbol(reads, &exact, &sections, ready, verified_current)
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

    async fn global_mcp_symbol(
        &self,
        mut exact: ExactSymbolParams,
        sections: &SymbolSections,
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
        if exact.revision.is_none() {
            exact.revision = SymbolIdentity::parse(exact.id.as_str())
                .ok()
                .and_then(|identity| {
                    crate::global::workspace_release(
                        resolved.published.reads.dependency_context(),
                        identity.owner(),
                    )
                });
        }

        let client =
            within_deadline(deadline, "get_symbol", self.global.client(&configuration)).await?;
        let answer = within_deadline(
            deadline,
            "get_symbol",
            client.get_symbol_with_available_documentation(&exact),
        )
        .await?;
        let mut answer = GetSymbolResult::from_exact(answer, sections);
        if let GetSymbolResult::Found(found) = &mut answer {
            Box::pin(projections::release_sections(
                &client, deadline, sections, &exact.id, found,
            ))
            .await;
        }
        Ok(Json(answer))
    }
}

impl super::ReadAnswer for GetSymbolResult {
    fn warnings_mut(&mut self) -> &mut Vec<super::ReadWarning> {
        match self {
            Self::Found(found) => &mut found.warnings,
            Self::Missing { warnings } | Self::Unavailable { warnings, .. } => warnings,
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
            "id": "rift://symbol/local/typescript/.%2Findex.ts/known",
            "exclude": ["documentation"]
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
    async fn exact_source_uses_the_accepted_publication_after_a_file_edit() -> TestResult {
        let (directory, server) = fixture().await?;
        let phase = crate::server::wire::ErrorPhase::Read;
        let current = server
            .published_workspace(phase, server.request_deadline().await)
            .await?;
        assert!(current.verified_current);
        assert!(current.stale.is_none());
        assert!(current.published.preparation.is_none());
        let params = request()?;
        let identity = params.id.clone();
        let sections = params.selection();
        let accepted = server
            .mcp_symbol_in_publication(
                &current,
                params.clone().into_exact(&sections),
                sections.clone(),
            )
            .await?
            .0;
        let GetSymbolResult::Found(found) = &accepted else {
            return Err("the verified current snapshot must establish the symbol".into());
        };
        assert!(accepted.is_valid_for(&identity));
        assert_eq!(found.revision, current.published.reads.served_revision()?);
        assert_eq!(found.declarations.len(), 1);
        let declaration = &found.declarations[0];
        assert_eq!(
            declaration
                .binding
                .path
                .as_ref()
                .map(|path| path.0.as_str()),
            Some("index.ts")
        );
        let source = declaration.source.as_ref().ok_or("source is selected")?;
        assert_eq!(source.span.unit.as_str(), "rift://source/project/index.ts");
        assert_eq!(source.span.range, declaration.binding.range);
        assert_eq!(
            source.text,
            "export function known(x: number) { return x; }"
        );
        assert!(found.edges.is_none());
        assert_eq!(accepted, server.read_mcp_symbol(params.clone()).await?.0);

        let publication = Arc::clone(&current.published);
        let digests = publication.reads.workspace_digests();
        let changes = PathChanges::default();
        let reason = StaleIndexReason::RebuildSuperseded {
            epoch: publication.epoch + 1,
            changes: &changes,
        };
        let stale =
            ResolvedWorkspace::stale(publication, &digests, None, &reason, phase, Duration::ZERO)?;
        assert!(!stale.verified_current);
        assert!(stale.stale.is_none());
        std::fs::write(
            directory.path().join("index.ts"),
            "export function known() {}\n",
        )?;
        assert_eq!(
            accepted,
            server
                .mcp_symbol_in_publication(&stale, params.into_exact(&sections), sections)
                .await?
                .0
        );
        Ok(())
    }

    #[tokio::test]
    async fn exact_symbol_adds_unique_outgoing_edges_and_honors_exclusions() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        std::fs::write(
            directory.path().join("index.ts"),
            "export function helper() { return 1; }\nexport function known() { return helper() + helper(); }\n",
        )?;
        let server =
            RiftMcp::build_settled(directory.path(), WorkspaceIndexLimits::default()).await?;
        let params = serde_json::from_value(json!({
            "id": request()?.id,
            "include": ["edges", "source"],
            "exclude": ["documentation", "source"]
        }))?;
        let GetSymbolResult::Found(found) = server.read_mcp_symbol(params).await?.0 else {
            return Err("the captured function is found".into());
        };
        assert!(found.documentation.is_none());
        assert!(found.declarations[0].source.is_none());
        let edges = found.edges.ok_or("edges are selected")?;
        assert_eq!(
            edges
                .iter()
                .filter(|edge| edge.kind.0 == "calls" && edge.target.as_str().ends_with("/helper"))
                .count(),
            1,
            "repeated calls share a target and kind: {edges:#?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn exact_source_of_a_git_revision_uses_committed_bytes() -> TestResult {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        crate::server::hermetic_workspace(directory.path(), "")?;
        let path = directory.path().join("index.ts");
        std::fs::write(&path, "export function known() { return 1; }\n")?;
        rift_history::fixture::commit_all(directory.path(), "introduce known");
        std::fs::write(&path, "export function known() { return 2; }\n")?;
        let server =
            RiftMcp::build_settled(directory.path(), WorkspaceIndexLimits::default()).await?;
        let params = serde_json::from_value(json!({
            "id": request()?.id, "revision": "rift://revision/git/HEAD"
        }))?;
        let GetSymbolResult::Found(found) = server.read_mcp_symbol(params).await?.0 else {
            return Err("the committed function is found".into());
        };
        assert!(matches!(
            found.revision.point(),
            rift_protocol::revision::RevisionPoint::Commit(_)
        ));
        let source = found.declarations[0]
            .source
            .as_ref()
            .ok_or("committed source")?;
        assert_eq!(source.text, "export function known() { return 1; }");
        Ok(())
    }

    #[tokio::test]
    async fn preparing_exact_read_keeps_its_typed_response() -> TestResult {
        let (_directory, assembled) = crate::server::tests::starting_fixture().await?;
        let result = assembled.server.read_mcp_symbol(request()?).await?.0;
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

        let mut pending = Box::pin(server.read_mcp_symbol(request()?));
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
