//! Points on the history axis that a read selects and an answer reports.

use std::fmt;

use rift_error::{RiftError, errors};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::identity::{SYMBOL_ID_BYTES_MAX, SymbolOwner, parse_release_owner};
use crate::read::RevisionId;

/// Prefix shared by every revision IRI.
pub const REVISION_URI_PREFIX: &str = "rift://revision/";

/// Longest revision IRI the wire accepts, in bytes.
pub const REVISION_BYTES_MAX: usize = SYMBOL_ID_BYTES_MAX;

/// Structural wire pattern. The codec also validates release coordinates and canonical
/// spelling.
pub const REVISION_PATTERN: &str = concat!(
    r"^rift://revision/(?:git/[A-Za-z0-9._/-]+(?:~[0-9]*)*",
    r"|tree/[0-9a-f]{64}",
    r"|stdlib/[a-z][a-z0-9_-]*@[^/?#]+",
    r"|[a-z][a-z0-9_-]*/[^/?#]+/(?:@[a-z0-9._-]+/)?[a-z0-9._-]+@[^/?#]+)$"
);

/// Form word of a git revision.
const GIT_FORM: &str = "git";

/// Form word of a served working tree revision.
const TREE_FORM: &str = "tree";

/// Hex width of a SHA-1 commit id.
const SHA1_HEX_BYTES: usize = 40;

/// Hex width of a SHA-256 commit id and of a working tree digest.
const SHA256_HEX_BYTES: usize = 64;

/// The field name a request carries a revision under, named by every refusal of one.
pub const REVISION_FIELD: &str = "revision";

/// One point on the history axis, spelled as an IRI under `rift://revision/`.
///
/// A commit, a release, and a working tree name a point exactly. A git reference names
/// a point through the repository it is resolved in, so only a request carries one: the
/// server resolves it to a commit and answers with that commit.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Revision(Point);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Point {
    Commit(String),
    Reference(String),
    Release(SymbolOwner, String),
    Tree(String),
}

/// The point a [`Revision`] names, borrowed from it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevisionPoint<'a> {
    /// A complete lowercase git commit id, SHA-1 or SHA-256.
    Commit(&'a str),
    /// A git branch, tag, or `HEAD`, optionally followed by `~N` ancestry suffixes.
    Reference(&'a str),
    /// One package or runtime release: the versionless owner and the release's version.
    Release(&'a SymbolOwner, &'a str),
    /// The complete lowercase SHA-256 digest of one served working tree.
    Tree(&'a str),
}

/// Reason a revision IRI breaks the form its schema advertises.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionViolation {
    /// The IRI is longer than [`REVISION_BYTES_MAX`] bytes.
    Length,
    /// The IRI lacks the `rift://revision/` prefix or a form after it.
    Structure,
    /// A git revision names neither a complete commit id nor a reference spelling.
    Reference,
    /// A tree revision carries no complete lowercase SHA-256 digest.
    Digest,
    /// A release revision carries invalid or noncanonical release coordinates.
    Release,
}

impl RevisionViolation {
    /// This violation's wire spelling, equal to its `Serialize` output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Length => "length",
            Self::Structure => "structure",
            Self::Reference => "reference",
            Self::Digest => "digest",
            Self::Release => "release",
        }
    }

    fn error(self) -> RiftError {
        errors::server::read_invalid()
            .field(REVISION_FIELD)
            .violation(self.as_str())
            .error()
    }
}

impl Revision {
    /// The revision of one git commit.
    ///
    /// # Errors
    /// Returns `read_invalid` naming `revision` when `id` is not a complete lowercase
    /// SHA-1 or SHA-256 commit id.
    pub fn commit(id: &str) -> Result<Self, RiftError> {
        if !is_commit_id(id) {
            return Err(RevisionViolation::Reference.error());
        }
        Ok(Self(Point::Commit(id.to_owned())))
    }

    /// The revision of one served working tree, by its complete digest.
    ///
    /// # Errors
    /// Returns `read_invalid` naming `revision` when `digest` is not a complete lowercase
    /// SHA-256 digest.
    pub fn tree(digest: &str) -> Result<Self, RiftError> {
        if !is_hex(digest, SHA256_HEX_BYTES) {
            return Err(RevisionViolation::Digest.error());
        }
        Ok(Self(Point::Tree(digest.to_owned())))
    }

    /// The revision of one release of a package or runtime owner, as
    /// `rift://revision/cargo/crates.io/demo@1.0.0`.
    ///
    /// # Errors
    /// Returns `read_invalid` naming `revision` for a local owner, invalid coordinates, or a
    /// version the owner's ecosystem does not spell canonically.
    pub fn release(owner: &SymbolOwner, version: &str) -> Result<Self, RiftError> {
        if !owner.accepts_release(version) {
            return Err(RevisionViolation::Release.error());
        }
        Ok(Self(Point::Release(owner.clone(), version.to_owned())))
    }

    /// Parses one revision IRI without resolving it.
    ///
    /// # Errors
    /// Returns `read_invalid` naming `revision` and the [`RevisionViolation`] it breaks.
    pub fn parse(value: &str) -> Result<Self, RiftError> {
        Self::classify(value).map_err(RevisionViolation::error)
    }

    fn classify(value: &str) -> Result<Self, RevisionViolation> {
        if value.len() > REVISION_BYTES_MAX {
            return Err(RevisionViolation::Length);
        }
        let rest = value
            .strip_prefix(REVISION_URI_PREFIX)
            .ok_or(RevisionViolation::Structure)?;
        let (form, body) = rest.split_once('/').ok_or(RevisionViolation::Structure)?;
        match form {
            GIT_FORM => git_point(body),
            TREE_FORM if is_hex(body, SHA256_HEX_BYTES) => Ok(Point::Tree(body.to_owned())),
            TREE_FORM => Err(RevisionViolation::Digest),
            _ => parse_release_owner(rest)
                .map(|(owner, version)| Point::Release(owner, version))
                .map_err(|_| RevisionViolation::Release),
        }
        .map(Self)
    }

    /// The point this revision names.
    #[must_use]
    pub fn point(&self) -> RevisionPoint<'_> {
        match &self.0 {
            Point::Commit(id) => RevisionPoint::Commit(id),
            Point::Reference(name) => RevisionPoint::Reference(name),
            Point::Release(owner, version) => RevisionPoint::Release(owner, version),
            Point::Tree(digest) => RevisionPoint::Tree(digest),
        }
    }

    /// Whether an answer about a symbol of `owner` may report this revision: a release of
    /// that package or runtime, a commit or a working tree for a project.
    #[must_use]
    pub fn answers_for(&self, owner: &SymbolOwner) -> bool {
        match &self.0 {
            Point::Release(release, _) => release == owner,
            Point::Commit(_) | Point::Tree(_) => owner.is_local(),
            Point::Reference(_) => false,
        }
    }

    /// The spelling git resolves for a commit or reference revision; absent for a release
    /// or a working tree.
    #[must_use]
    pub fn git_revision(&self) -> Option<RevisionId> {
        match &self.0 {
            Point::Commit(spelling) | Point::Reference(spelling) => {
                Some(RevisionId(spelling.clone()))
            }
            Point::Release(..) | Point::Tree(_) => None,
        }
    }

    /// The version a release revision names; absent for a commit, reference, or tree.
    #[must_use]
    pub fn release_version(&self) -> Option<&str> {
        match &self.0 {
            Point::Release(_, version) => Some(version),
            Point::Commit(_) | Point::Reference(_) | Point::Tree(_) => None,
        }
    }
}

fn git_point(body: &str) -> Result<Point, RevisionViolation> {
    if is_commit_id(body) {
        return Ok(Point::Commit(body.to_owned()));
    }
    let spelling = RevisionId(body.to_owned());
    let accepted =
        spelling.violation().is_none() && !body.contains('^') && !body.starts_with(['/', '.']);
    if !accepted {
        return Err(RevisionViolation::Reference);
    }
    Ok(Point::Reference(spelling.0))
}

fn is_commit_id(value: &str) -> bool {
    is_hex(value, SHA1_HEX_BYTES) || is_hex(value, SHA256_HEX_BYTES)
}

fn is_hex(value: &str, width: usize) -> bool {
    value.len() == width
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl fmt::Display for Revision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REVISION_URI_PREFIX)?;
        match &self.0 {
            Point::Commit(spelling) | Point::Reference(spelling) => {
                write!(formatter, "{GIT_FORM}/{spelling}")
            }
            Point::Tree(digest) => write!(formatter, "{TREE_FORM}/{digest}"),
            Point::Release(owner, version) => {
                formatter.write_str(&owner.release_spelling(version).unwrap_or_default())
            }
        }
    }
}

impl Serialize for Revision {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Revision {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for Revision {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Revision".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": REVISION_BYTES_MAX,
            "pattern": REVISION_PATTERN,
            "description": "One point on the history axis. `rift://revision/git/<commit>` names a complete git commit id, and `rift://revision/git/<reference>` a branch, tag, or `HEAD` with optional `~N` suffixes that the server resolves to a commit. `rift://revision/<manager>/<registry>/<name>@<version>` names a package release, `rift://revision/stdlib/<runtime>@<version>` a runtime release, and `rift://revision/tree/<digest>` one served working tree. An answer names a commit, a release, or a tree, never a reference.",
            "examples": [
                "rift://revision/git/1f2080e49da12fee4431e6872630509355cd62d1",
                "rift://revision/git/main",
                "rift://revision/cargo/crates.io/serde@1.0.228",
                "rift://revision/npm/npmjs.org/@types/node@26.6.4",
                "rift://revision/stdlib/cpython@3.12.9",
                "rift://revision/tree/4e1b6c1ab5d6ddbdd1d7e3a7e38fc1bf1d2a35c6a1b0a7c8a4e0bd0c7a9b6f11"
            ]
        })
    }
}

#[cfg(test)]
mod tests;
