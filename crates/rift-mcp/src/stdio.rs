use std::fmt;
use std::path::Path;

use rift_core::{ErrorCode, ErrorDescriptor, ErrorName};
use rift_index::WorkspaceIndexLimits;
use rift_server::ReadError;
use rmcp::service::{QuitReason, ServerInitializeError};
use rmcp::{ServiceExt as _, transport};

use crate::{RiftMcp, RiftMcpOptions};

/// Failure while starting or running stdio MCP transport.
#[derive(Debug)]
pub enum StdioServeError {
    /// Workspace snapshot could not be built.
    Read(ReadError),
    /// MCP initialization failed.
    Initialize(Box<ServerInitializeError>),
    /// MCP service task failed.
    Task(tokio::task::JoinError),
    /// MCP service ended unexpectedly.
    UnexpectedQuit,
}

impl fmt::Display for StdioServeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Read(_) => "workspace read service failed",
            Self::Initialize(_) => "MCP initialization failed",
            Self::Task(_) => "MCP service task failed",
            Self::UnexpectedQuit => "MCP service ended unexpectedly",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for StdioServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(source) => Some(source),
            Self::Initialize(source) => Some(source.as_ref()),
            Self::Task(source) => Some(source),
            Self::UnexpectedQuit => None,
        }
    }
}

impl StdioServeError {
    /// Returns canonical registry metadata.
    ///
    /// A workspace read failure keeps the classification of the
    /// [`ReadError`] it wraps. Initialization keeps the transport failure
    /// transient; a task join or an unexpected quit are invariants the
    /// server did not classify.
    #[must_use]
    pub fn descriptor(&self) -> ErrorDescriptor {
        match self {
            Self::Read(source) => source.descriptor(),
            Self::Initialize(_) => ErrorName::Wire(ErrorCode::TemporarilyUnavailable).descriptor(),
            Self::Task(_) | Self::UnexpectedQuit => {
                ErrorName::Wire(ErrorCode::InternalError).descriptor()
            }
        }
    }
}

/// Serves current Rust workspace over newline-delimited stdio MCP.
///
/// # Errors
///
/// Returns [`StdioServeError`] for indexing, initialization, or service-task failure.
///
/// # Cancel safety
///
/// Dropping this future closes its owned MCP service. An admitted initial
/// index scan still finishes in the bounded blocking executor.
pub async fn serve_stdio(root: &Path, options: RiftMcpOptions) -> Result<(), StdioServeError> {
    let server = RiftMcp::build(root, WorkspaceIndexLimits::default(), options)
        .await
        .map_err(StdioServeError::Read)?;
    let service = server
        .serve(transport::stdio())
        .await
        .map_err(|error| StdioServeError::Initialize(Box::new(error)))?;
    let reason = service.waiting().await.map_err(StdioServeError::Task)?;
    quit_reason_result(reason)
}

/// Maps a service quit reason to its outcome.
///
/// Split from [`serve_stdio`] so the mapping is testable without live stdio I/O.
fn quit_reason_result(reason: QuitReason) -> Result<(), StdioServeError> {
    match reason {
        QuitReason::Closed | QuitReason::Cancelled => Ok(()),
        QuitReason::JoinError(error) => Err(StdioServeError::Task(error)),
        _ => Err(StdioServeError::UnexpectedQuit),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use rift_index::WorkspaceIndexLimits;
    use rmcp::service::{QuitReason, ServerInitializeError};

    use super::{StdioServeError, quit_reason_result};
    use crate::{RiftMcp, RiftMcpOptions};

    #[test]
    fn unexpected_quit_has_stable_message_without_source() {
        let error = StdioServeError::UnexpectedQuit;
        assert_eq!(error.to_string(), "MCP service ended unexpectedly");
        assert!(error.source().is_none());
    }

    #[tokio::test]
    async fn workspace_read_error_preserves_source() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing = directory.path().join("missing");
        let read = RiftMcp::build(
            &missing,
            WorkspaceIndexLimits::default(),
            RiftMcpOptions::default(),
        )
        .await
        .expect_err("missing workspace must fail");
        let expected = read.descriptor();
        let error = StdioServeError::Read(read);
        assert_eq!(error.to_string(), "workspace read service failed");
        assert!(error.source().is_some());
        assert_eq!(error.descriptor(), expected);
    }

    #[test]
    fn unexpected_quit_descriptor_is_internal_error() {
        let error = StdioServeError::UnexpectedQuit;
        assert_eq!(error.descriptor().code(), "internal_error");
    }

    #[test]
    fn initialization_error_preserves_source() {
        let error = StdioServeError::Initialize(Box::new(ServerInitializeError::Cancelled));
        assert_eq!(error.to_string(), "MCP initialization failed");
        assert!(error.source().is_some());
        assert_eq!(
            error.descriptor().code(),
            "temporarily_unavailable",
            "an initialization failure must classify as transient"
        );
    }

    #[tokio::test]
    async fn task_error_preserves_source() {
        let task = tokio::spawn(async { panic!("test join failure") });
        let join = task.await.expect_err("test task must fail");
        let error = StdioServeError::Task(join);
        assert_eq!(error.to_string(), "MCP service task failed");
        assert!(error.source().is_some());
    }

    #[tokio::test]
    async fn join_error_quit_reason_maps_to_task_error() {
        let task = tokio::spawn(async { panic!("test join failure") });
        let join = task.await.expect_err("test task must fail");
        let error = quit_reason_result(QuitReason::JoinError(join))
            .expect_err("join error quit reason must fail");
        assert_eq!(error.to_string(), "MCP service task failed");
        assert!(error.source().is_some());
    }
}
