use crate::RiftError;
use std::sync::Arc;

/// Converts a complete registered error builder into its runtime error.
pub trait IntoRiftError: Sized {
    /// Returns the completed runtime error.
    fn into_rift_error(self) -> RiftError;

    /// Returns this error through a function's result type.
    fn fail<T>(self) -> Result<T, RiftError> {
        self.into_rift_error().fail()
    }
}

impl IntoRiftError for RiftError {
    fn into_rift_error(self) -> RiftError {
        self
    }
}

impl IntoRiftError for &RiftError {
    fn into_rift_error(self) -> RiftError {
        self.clone()
    }
}

impl IntoRiftError for Arc<RiftError> {
    fn into_rift_error(self) -> RiftError {
        Arc::unwrap_or_clone(self)
    }
}

impl IntoRiftError for &Arc<RiftError> {
    fn into_rift_error(self) -> RiftError {
        self.as_ref().clone()
    }
}
