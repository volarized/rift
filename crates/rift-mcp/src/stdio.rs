use std::fmt;
use std::path::Path;

use rift_index::WorkspaceIndexLimits;
use rift_server::ReadError;
use rmcp::service::{QuitReason, ServerInitializeError};
use rmcp::{ServiceExt as _, transport};

use crate::RiftMcp;

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

/// Serves current Rust workspace over newline-delimited stdio MCP.
///
/// # Errors
///
/// Returns [`StdioServeError`] for indexing, initialization, or service-task failure.
pub async fn serve_stdio(root: &Path) -> Result<(), StdioServeError> {
    let server =
        RiftMcp::build(root, WorkspaceIndexLimits::default()).map_err(StdioServeError::Read)?;
    let service = server
        .serve(transport::stdio())
        .await
        .map_err(|error| StdioServeError::Initialize(Box::new(error)))?;
    match service.waiting().await.map_err(StdioServeError::Task)? {
        QuitReason::Closed | QuitReason::Cancelled => Ok(()),
        QuitReason::JoinError(error) => Err(StdioServeError::Task(error)),
        _ => Err(StdioServeError::UnexpectedQuit),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use rift_index::WorkspaceIndexLimits;
    use rmcp::service::ServerInitializeError;

    use super::StdioServeError;
    use crate::RiftMcp;

    #[test]
    fn unexpected_quit_has_stable_message_without_source() {
        let error = StdioServeError::UnexpectedQuit;
        assert_eq!(error.to_string(), "MCP service ended unexpectedly");
        assert!(error.source().is_none());
    }

    #[test]
    fn workspace_read_error_preserves_source() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing = directory.path().join("missing");
        let read = RiftMcp::build(&missing, WorkspaceIndexLimits::default())
            .expect_err("missing workspace must fail");
        let error = StdioServeError::Read(read);
        assert_eq!(error.to_string(), "workspace read service failed");
        assert!(error.source().is_some());
    }

    #[test]
    fn initialization_error_preserves_source() {
        let error = StdioServeError::Initialize(Box::new(ServerInitializeError::Cancelled));
        assert_eq!(error.to_string(), "MCP initialization failed");
        assert!(error.source().is_some());
    }

    #[tokio::test]
    async fn task_error_preserves_source() {
        let task = tokio::spawn(async { panic!("test join failure") });
        let join = task.await.expect_err("test task must fail");
        let error = StdioServeError::Task(join);
        assert_eq!(error.to_string(), "MCP service task failed");
        assert!(error.source().is_some());
    }
}
