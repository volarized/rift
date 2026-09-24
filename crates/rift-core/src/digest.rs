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

    /// The digest [`Self::as_bytes`] returned, read back from a store that recorded it.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::FileDigest;

    #[test]
    fn content_and_file_state_keep_bytes_and_executable_state_distinct() {
        for bytes in [b"".as_slice(), b"abc", "café\n".as_bytes()] {
            let content = FileDigest::of(bytes);
            let mut states = Vec::new();
            for executable in [false, true] {
                let (combined_content, combined_state) =
                    FileDigest::of_content_and_file_state(bytes, executable);
                assert_eq!(combined_content, content);
                assert_eq!(combined_state, FileDigest::of_file_state(bytes, executable));
                let mut state_bytes = bytes.to_vec();
                state_bytes.push(u8::from(executable));
                assert_eq!(combined_state, FileDigest::of(&state_bytes));
                assert_ne!(combined_state.as_bytes(), content.as_bytes());
                states.push(combined_state);
            }
            assert_ne!(states[0], states[1]);
        }
    }

    #[test]
    fn a_digest_read_back_from_its_bytes_is_the_digest_recorded() {
        let recorded = FileDigest::of(b"pub fn beacon() {}\n");
        assert_eq!(FileDigest::from_bytes(*recorded.as_bytes()), recorded);
    }
}
