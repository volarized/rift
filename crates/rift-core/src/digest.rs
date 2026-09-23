use sha2::{Digest as _, Sha256};

/// One file's content identity: the SHA-256 of its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileDigest([u8; 32]);

impl FileDigest {
    /// Digests one file's bytes.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Digests file bytes and executable metadata for workspace observation.
    #[must_use]
    pub fn of_file_state(bytes: &[u8], executable: bool) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.update([u8::from(executable)]);
        Self(hasher.finalize().into())
    }

    /// Digests one file's bytes once and returns both forms: the content digest
    /// [`Self::of`] computes, then the file-state digest [`Self::of_file_state`] computes.
    /// A capture that needs both pays one pass over the bytes.
    #[must_use]
    pub fn of_content_and_file_state(bytes: &[u8], executable: bool) -> (Self, Self) {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let content = Self(hasher.clone().finalize().into());
        hasher.update([u8::from(executable)]);
        (content, Self(hasher.finalize().into()))
    }

    /// The digest's bytes, as workspace identity material absorbs them.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
