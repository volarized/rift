//! Canonical documentation digests and structural identities.

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_protocol::canonical::canonical_json;
use rift_protocol::documentation::{
    DocumentationContentIdentity, DocumentationDigest, DocumentationSourceIdentity,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::failure::{DocumentationError, DocumentationFault, DocumentationViolation};
use super::input::validate_identity;

/// Returns one baseline document identity for a validated content owner.
///
/// A regular project path or package source unit keeps its existing identity. A notebook
/// cell uses a control-character namespace unavailable to project paths and percent-encoded
/// canonical identity bytes, so cell identity cannot collide with a regular source.
///
/// # Errors
///
/// Returns a typed refusal when identity is invalid, encoding fails, or the ranking identity
/// bound is exceeded.
pub fn content_owner_identity(
    identity: &DocumentationContentIdentity,
) -> Result<String, DocumentationError> {
    validate_identity(identity)?;
    let Some(_cell) = &identity.cell else {
        return Ok(match &identity.source {
            DocumentationSourceIdentity::Project { path } => path.0.clone(),
            DocumentationSourceIdentity::Package { unit } => unit.0.clone(),
        });
    };
    let serialized = canonical_json(identity).map_err(|error| {
        DocumentationFault::new(DocumentationViolation::Encoding, "owner_identity").caused_by(error)
    })?;
    let encoded = utf8_percent_encode(&serialized, NON_ALPHANUMERIC);
    let owner = format!("\u{1f}documentation-cell/{encoded}");
    rift_ranking::DocumentIdentity::new(owner.clone()).map_err(|error| {
        DocumentationFault::new(DocumentationViolation::LimitExceeded, "owner_identity")
            .caused_by(error)
    })?;
    Ok(owner)
}

/// Returns one bounded baseline chunk identity for a content owner.
///
/// Chunk ordinals follow source order. The notebook owner namespace keeps these identities
/// apart from regular file and source-unit identities.
///
/// # Errors
///
/// Returns a typed refusal when owner identity is invalid or the ranking identity bound is
/// exceeded.
pub fn content_chunk_identity(
    identity: &DocumentationContentIdentity,
    ordinal: u32,
) -> Result<String, DocumentationError> {
    let owner = content_owner_identity(identity)?;
    let chunk = format!("{owner}#chunk/{ordinal}");
    rift_ranking::DocumentIdentity::new(chunk.clone()).map_err(|error| {
        DocumentationFault::new(DocumentationViolation::LimitExceeded, "chunk_identity")
            .caused_by(error)
    })?;
    Ok(chunk)
}

/// Returns the full SHA-256 digest of exact source bytes.
#[must_use]
pub fn content_digest(bytes: &[u8]) -> DocumentationDigest {
    let full = format!("{:x}", Sha256::digest(bytes));
    DocumentationDigest(full)
}

pub(super) fn canonical_digest(
    value: &impl Serialize,
) -> Result<DocumentationDigest, DocumentationError> {
    let encoded = canonical_json(value).map_err(|error| {
        DocumentationFault::new(DocumentationViolation::Encoding, "documentation").caused_by(error)
    })?;
    Ok(content_digest(encoded.as_bytes()))
}

pub(super) fn is_digest(value: &DocumentationDigest) -> bool {
    value.0.len() == 64
        && value
            .0
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn is_revision_digest(value: &rift_protocol::read::Digest) -> bool {
    value.0.len() == DIGEST_WIRE_CHARS
        && value
            .0
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use rift_protocol::documentation::{
        DocumentationContentIdentity, DocumentationSourceIdentity, NotebookCell,
        NotebookCellIdentity, NotebookCellKind,
    };
    use rift_protocol::read::{ProjectPath, SourceUnitId};

    use super::{content_digest, content_owner_identity};

    #[test]
    fn documentation_digests_keep_full_sha256_after_wire_prefix_collision() {
        let first = content_digest(b"collision regression 70192");
        let second = content_digest(b"collision regression 134641");

        assert_eq!(&first.0[..8], &second.0[..8]);
        assert_ne!(first, second);
        assert_eq!(first.0.len(), 64);
        assert_eq!(second.0.len(), 64);
    }

    #[test]
    fn regular_content_owner_keeps_existing_identity() {
        let identity = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Package {
                unit: SourceUnitId("rift://source/cargo/beacon@1.0.0/README.md".to_owned()),
            },
            cell: None,
        };

        assert_eq!(
            content_owner_identity(&identity).expect("owner identity"),
            "rift://source/cargo/beacon@1.0.0/README.md"
        );
    }

    #[test]
    fn notebook_cells_use_canonical_distinct_identity_namespace() {
        let owner = |index| DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Project {
                path: ProjectPath("notebooks/guide.ipynb".to_owned()),
            },
            cell: Some(NotebookCell {
                identity: NotebookCellIdentity::Indexed { index },
                kind: NotebookCellKind::Markdown,
            }),
        };

        let first = content_owner_identity(&owner(1)).expect("first cell");
        let second = content_owner_identity(&owner(2)).expect("second cell");

        assert_ne!(first, second);
        assert!(first.starts_with('\u{1f}'));
        assert!(rift_core::ProjectPath::new(first).is_err());
    }
}
