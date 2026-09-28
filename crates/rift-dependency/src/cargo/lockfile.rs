//! The `Cargo.lock` document: the packages it pins and where each came from.

use serde::Deserialize;

use super::CARGO_LOCK_FILE_NAME;
use crate::manifest::StaticFileFailure;

/// The lockfile `source` prefix of a package fetched from a git repository.
pub(super) const GIT_SOURCE_PREFIX: &str = "git+";

/// The `Cargo.lock` document, the fields the static context reads.
#[derive(Deserialize)]
pub(super) struct Lockfile {
    #[serde(default)]
    pub(super) package: Vec<LockedPackage>,
}

/// One `[[package]]` table of the lockfile.
///
/// A package without `source` is a workspace member or a path dependency; the lockfile
/// does not tell the two apart, so the manifests' dependency tables decide.
#[derive(Deserialize)]
pub(super) struct LockedPackage {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) source: Option<String>,
}

/// Parses `Cargo.lock` bytes, naming the parser's message when they are not its document.
pub(super) fn parse_lockfile(bytes: &[u8]) -> Result<Lockfile, StaticFileFailure> {
    toml::from_slice(bytes).map_err(|error| {
        StaticFileFailure::unparsable(CARGO_LOCK_FILE_NAME, error.message().to_owned())
    })
}
