use crate::RiftError;

/// Converts a complete registered error builder into its runtime error.
pub trait IntoRiftError: Sized {
    /// Returns the completed runtime error.
    fn into_rift_error(self) -> RiftError;

    /// Returns this error through a function's result type.
    fn fail<T>(self) -> Result<T, RiftError> {
        Err(self.into_rift_error())
    }
}

impl IntoRiftError for RiftError {
    fn into_rift_error(self) -> RiftError {
        self
    }
}
