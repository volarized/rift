//! Why the history store refused an operation.

use std::path::{Path, PathBuf};

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault};

/// One history store failure: the operation that failed, the file it
/// touched, and the cause the filesystem or `SQLite` reported.
#[derive(Debug)]
pub enum StoreFault {
    /// The store's folder or one of its lock files could not be created,
    /// opened, locked, or deleted.
    Folder {
        /// The folder or file the operation touched.
        path: PathBuf,
        /// The operation that failed.
        operation: &'static str,
        /// The filesystem's own report.
        source: std::io::Error,
    },
    /// A live lock named another file each time the server took it: a sweep
    /// kept deleting the file between the open and the lock.
    LockUnstable {
        /// The live lock's path.
        path: PathBuf,
        /// The attempts the server made before refusing.
        attempts: usize,
    },
    /// `SQLite` refused a statement, including a failed full-text integrity
    /// check.
    Database {
        /// The store operation that failed.
        operation: &'static str,
        /// The driver's own report.
        source: rusqlite::Error,
    },
}

impl StoreFault {
    /// The filesystem's report when the store's folder refused an operation.
    #[must_use]
    pub const fn folder_cause(&self) -> Option<&std::io::Error> {
        match self {
            Self::Folder { source, .. } => Some(source),
            Self::LockUnstable { .. } | Self::Database { .. } => None,
        }
    }
}

impl Fault for StoreFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::StorageFailure)
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::Folder {
                path,
                operation,
                source,
            } => vec![
                ErrorContext::new("operation", *operation),
                ErrorContext::new("path", path.display().to_string()),
                ErrorContext::new("detail", source.to_string()),
            ],
            Self::LockUnstable { path, attempts } => vec![
                ErrorContext::new("operation", "lock live store"),
                ErrorContext::new("path", path.display().to_string()),
                ErrorContext::new("attempts", attempts.to_string()),
            ],
            Self::Database { operation, source } => vec![
                ErrorContext::new("operation", *operation),
                ErrorContext::new("detail", source.to_string()),
            ],
        }
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Folder { source, .. } => Some(source),
            Self::Database { source, .. } => Some(source),
            Self::LockUnstable { .. } => None,
        }
    }
}

/// Opaque history store failure.
pub type StoreError = Error<StoreFault>;

/// The failure a filesystem operation on `path` reported.
pub(crate) fn folder_error(
    path: &Path,
    operation: &'static str,
) -> impl FnOnce(std::io::Error) -> StoreError {
    let path = path.to_owned();
    move |source| {
        Error::new(StoreFault::Folder {
            path,
            operation,
            source,
        })
    }
}

/// The failure one `SQLite` statement of `operation` reported.
pub(crate) fn database_error(
    operation: &'static str,
) -> impl FnOnce(rusqlite::Error) -> StoreError {
    move |source| Error::new(StoreFault::Database { operation, source })
}
