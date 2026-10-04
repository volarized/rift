//! Ambient context constructors.

use std::{fmt::Display, path::Path};

use crate::{ErrorContext, ErrorValue};

/// Names the workspace associated with an operation.
#[must_use]
pub fn workspace(value: impl AsRef<Path>) -> ErrorContext {
    ErrorContext::new("workspace", ErrorValue::path(value))
}

/// Names the operation that failed.
#[must_use]
pub fn operation(value: impl Display) -> ErrorContext {
    ErrorContext::new("operation", ErrorValue::display(value))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{operation, workspace};
    use crate::{ErrorSlug, RiftError};

    #[test]
    fn ambient_context_uses_registered_context_keys() {
        let error = RiftError::new(
            ErrorSlug::new("rift.test.context"),
            "failed",
            "retry",
            vec![],
        )
        .with(workspace(Path::new("/repo")))
        .with(operation("persist index"));

        assert_eq!(
            error.detail(),
            "failed: workspace /repo, operation persist index"
        );
    }
}
