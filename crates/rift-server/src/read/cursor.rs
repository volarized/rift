//! Continuations bound to the answer they resume and the revision it was read at.
//!
//! A cursor spells `<offset>.<tag>`: the items a preceding page already answered, and the
//! first `CURSOR_TAG_CHARS` lowercase hex characters of the SHA-256 over the answer's key and
//! the revision. The server holds nothing between pages, so a cursor resumes only while the
//! same key reads at the same revision.

use rift_error::{RiftError, errors};
use sha2::{Digest as _, Sha256};

/// The separator between a cursor's offset and the tag binding it to one read.
const CURSOR_SEPARATOR: char = '.';

/// Characters of the tag that binds a cursor to its answer and revision.
const CURSOR_TAG_CHARS: usize = 16;

/// The tag binding a cursor to the answer `key` names and the `revision` it was read at.
fn tag(key: &str, revision: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hasher.update([0]);
    hasher.update(revision.as_bytes());
    let mut tag = data_encoding::HEXLOWER.encode(&hasher.finalize());
    tag.truncate(CURSOR_TAG_CHARS);
    tag
}

/// The cursor that resumes the answer `key` names over `revision` at `offset`.
pub(crate) fn mint(offset: usize, key: &str, revision: &str) -> String {
    format!("{offset}{CURSOR_SEPARATOR}{}", tag(key, revision))
}

/// The offset `cursor` resumes at.
///
/// # Errors
///
/// Returns `read_invalid` naming `field` when `cursor` is malformed or was minted for
/// another answer or revision.
pub(crate) fn offset(
    cursor: &str,
    field: &'static str,
    key: &str,
    revision: &str,
) -> Result<usize, RiftError> {
    let invalid = |violation: &str| {
        errors::server::read_invalid()
            .field(field)
            .violation(violation)
            .error()
    };
    let (offset, found) = cursor
        .split_once(CURSOR_SEPARATOR)
        .ok_or_else(|| invalid("send the cursor a preceding read returned"))?;
    if found != tag(key, revision) {
        return Err(invalid(
            "the cursor belongs to another answer or revision; restart the read without it",
        ));
    }
    offset
        .parse()
        .map_err(|_| invalid("send the cursor a preceding read returned"))
}

#[cfg(test)]
mod tests {
    use super::{mint, offset};

    #[test]
    fn a_cursor_resumes_only_its_own_answer_and_revision() {
        let cursor = mint(3, "rift://symbol/local/rust/app/run", "rev-a");
        assert_eq!(
            offset(
                &cursor,
                "declaration_cursor",
                "rift://symbol/local/rust/app/run",
                "rev-a"
            )
            .ok(),
            Some(3)
        );
        for (key, revision) in [
            ("rift://symbol/local/rust/app/run", "rev-b"),
            ("rift://symbol/local/rust/app/walk", "rev-a"),
        ] {
            let error = offset(&cursor, "declaration_cursor", key, revision)
                .expect_err("a cursor for another answer or revision refuses");
            assert!(
                error
                    .context()
                    .any(|(name, value)| name == "field" && value == "declaration_cursor")
            );
        }
        for malformed in ["3", "x.0123456789abcdef", ""] {
            assert!(offset(malformed, "node_cursor", "key", "rev-a").is_err());
        }
    }
}
