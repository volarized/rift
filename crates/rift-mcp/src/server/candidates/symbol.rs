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
        let ready = resolved.published.preparation.is_none();
        self.current_tree_read(&resolved, true, move |reads| {
            reads.exact_symbol(&params, ready)
        })
        .await
    }

    /// Reads the nodes of a package or runtime symbol from the global API, which holds the
    /// facts of that symbol's release.
    pub(in crate::server) async fn global_nodes(
        &self,
        params: &rift_protocol::read::NodesParams,
    ) -> Result<Json<rift_protocol::read::NodesResult>, super::ToolFailure> {
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
            selected.revision = selected.symbol.as_ref().and_then(|symbol| {
                let identity = SymbolIdentity::parse(symbol.as_str()).ok()?;
                crate::global::workspace_release(
                    resolved.published.reads.dependency_context(),
                    identity.owner(),
                )
            });
        }
        within_deadline(
            deadline,
            "nodes",
            self.global.symbol_nodes(&configuration, &selected),
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
