//! The uv resolver: Python distributions as `uv.lock` pins them and `pyproject.toml` declares them.

mod context;
mod environment;

use serde::Deserialize;

use crate::context::ContextAnswer;
use crate::manifest::StaticFileFailure;
use crate::resolver::{ContextRequest, DependencyResolver, ResolverName, StaticInputs};

/// The package namespace every uv dependency entry belongs to.
const PYPI_MANAGER: &str = "pypi";
/// The manifest file name this resolver claims.
const UV_MANIFEST_FILE_NAME: &str = "pyproject.toml";
/// The lockfile uv keeps beside a workspace root manifest.
const UV_LOCK_FILE_NAME: &str = "uv.lock";
/// The characters PEP 503 folds into one `-` when normalizing a distribution name.
const NAME_SEPARATORS: [char; 3] = ['-', '_', '.'];
/// The separator a normalized distribution name keeps.
const NORMALIZED_SEPARATOR: char = '-';

/// The resolver for Python distributions, answering from `uv.lock` and `pyproject.toml`.
#[derive(Debug, Default)]
pub struct UvResolver;

impl UvResolver {
    /// The uv resolver. It holds no state, so one instance serves every workspace.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DependencyResolver for UvResolver {
    fn name(&self) -> ResolverName {
        ResolverName::Uv
    }

    fn manifest_file_name(&self) -> &'static str {
        UV_MANIFEST_FILE_NAME
    }

    fn context(
        &self,
        request: &ContextRequest<'_>,
        inputs: &mut dyn StaticInputs,
    ) -> ContextAnswer {
        context::uv_context(request, inputs)
    }
}

/// Parses `uv.lock` bytes, naming the parser's message when they are not its document.
fn parse_lockfile(bytes: &[u8]) -> Result<Lockfile, StaticFileFailure> {
    toml::from_slice(bytes).map_err(|error| {
        StaticFileFailure::unparsable(UV_LOCK_FILE_NAME, error.message().to_owned())
    })
}

/// The `uv.lock` document, the fields this resolver reads.
///
/// The top-level `[manifest]` table and every package field past these are ignored.
#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockedPackage>,
}

/// One `[[package]]` table of the lockfile. A project with a dynamic version carries no
/// `version`.
#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: Option<String>,
    source: LockedSource,
}

/// Where a locked package came from: a workspace project, a directory, the index a
/// distribution was fetched from, a git repository, or the URL of a direct one. A source
/// naming none of these, such as `path` to an archive, names a path.
#[derive(Deserialize)]
struct LockedSource {
    editable: Option<String>,
    #[serde(rename = "virtual")]
    virtual_directory: Option<String>,
    directory: Option<String>,
    registry: Option<String>,
    git: Option<String>,
    url: Option<String>,
}

/// The PEP 503 normalized form of a distribution name: lowercase, every separator run one `-`.
fn normalized_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut separator_pending = false;
    for character in name.to_lowercase().chars() {
        if NAME_SEPARATORS.contains(&character) {
            separator_pending = true;
            continue;
        }
        if separator_pending {
            normalized.push(NORMALIZED_SEPARATOR);
            separator_pending = false;
        }
        normalized.push(character);
    }
    if separator_pending {
        normalized.push(NORMALIZED_SEPARATOR);
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uv_resolver_identity_names_uv_and_its_manifest() {
        let resolver = UvResolver::new();
        assert_eq!(resolver.name(), ResolverName::Uv);
        assert_eq!(resolver.manifest_file_name(), "pyproject.toml");
    }

    #[test]
    fn test_normalized_name_folds_separator_runs_and_case() {
        assert_eq!(normalized_name("Markdown_It.py"), "markdown-it-py");
        assert_eq!(normalized_name("typing-extensions"), "typing-extensions");
        assert_eq!(normalized_name("Zope.Interface"), "zope-interface");
        assert_eq!(normalized_name("foo__.--bar"), "foo-bar");
        assert_eq!(normalized_name("trailing_"), "trailing-");
    }

    #[test]
    fn test_parse_lockfile_names_the_file_when_it_is_not_uv_s_document() {
        let failure = parse_lockfile(b"[[package]]\nname = ")
            .map(|_| ())
            .expect_err("unparsable");
        assert!(
            failure
                .to_string()
                .starts_with("uv.lock could not be parsed: "),
            "{failure}"
        );
    }
}
