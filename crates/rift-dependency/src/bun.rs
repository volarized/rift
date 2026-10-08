//! The Bun resolver: npm packages as `bun.lock` pins them.

mod context;

use std::collections::BTreeMap;
use std::str::Split;

use serde::Deserialize;
use serde_json_lenient::Value;

use crate::context::ContextAnswer;
use crate::manifest::StaticFileFailure;
use crate::node::{
    ALIAS_VERSION_PREFIX, NODE_MODULES_DIRECTORY_NAME, PACKAGE_MANIFEST_FILE_NAME, SCOPE_PREFIX,
    split_reference,
};
use crate::resolver::{ContextRequest, DependencyResolver, ResolverName, StaticInputs};

/// The lockfile Bun keeps beside a workspace root manifest.
const BUN_LOCK_FILE_NAME: &str = "bun.lock";
/// The version prefix naming one of the workspace's own packages, never reported.
const WORKSPACE_VERSION_PREFIX: &str = "workspace:";
/// The character between parent and child in a nested `packages` key, and between a
/// scope and its name.
const NESTING_SEPARATOR: char = '/';
/// Names one `packages` key may nest, at most; a deeper key spells no install path.
#[cfg(test)]
const NESTING_DEPTH_MAX: usize = 32;

/// The resolver for npm packages Bun installed, answering from `bun.lock`.
#[derive(Debug, Default)]
pub struct BunResolver;

impl BunResolver {
    /// The Bun resolver. It holds no state, so one instance serves every workspace.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DependencyResolver for BunResolver {
    fn name(&self) -> ResolverName {
        ResolverName::Bun
    }

    fn manifest_file_name(&self) -> &'static str {
        PACKAGE_MANIFEST_FILE_NAME
    }

    fn context(
        &self,
        request: &ContextRequest<'_>,
        inputs: &mut dyn StaticInputs,
    ) -> ContextAnswer {
        context::bun_context(request, inputs)
    }
}

/// Parses the JSONC document Bun writes: comments and trailing commas are accepted.
///
/// The document is read into a [`Value`] first. The lenient reader accepts a trailing comma
/// only through a map or sequence visitor, and a field this resolver does not declare, such
/// as `overrides`, is otherwise skipped by a reader that refuses the comma before its `}`.
fn parse_lockfile(bytes: &[u8]) -> Result<BunLock, StaticFileFailure> {
    let unparsable = |error: serde_json_lenient::Error| {
        StaticFileFailure::unparsable(BUN_LOCK_FILE_NAME, error.to_string())
    };
    let document: Value = serde_json_lenient::from_slice(bytes).map_err(unparsable)?;
    serde_json_lenient::from_value(document).map_err(unparsable)
}

/// The `bun.lock` document, the field this resolver reads.
#[derive(Deserialize)]
struct BunLock {
    /// Every pinned package keyed by its install location below `node_modules`.
    #[serde(default)]
    packages: BTreeMap<String, LockedPackage>,
}

/// One `packages` tuple: its first element spells `<name>@<version>`, and for a registry
/// package the second names the registry, empty for the default one. The metadata and
/// integrity after it are not read.
#[derive(Deserialize)]
#[serde(transparent)]
struct LockedPackage(Vec<Value>);

impl LockedPackage {
    /// What the tuple pins.
    fn pin(&self) -> Pin<'_> {
        match self
            .0
            .first()
            .and_then(Value::as_str)
            .and_then(split_reference)
        {
            Some((spelled, version)) => pinned(spelled, version),
            None => Pin::Malformed,
        }
    }

    /// The registry a registry package was fetched from: the tuple's second element,
    /// absent when it is empty or not a string, as it is for a git or path package.
    fn registry(&self) -> Option<&str> {
        self.0
            .get(1)
            .and_then(Value::as_str)
            .filter(|registry| !registry.is_empty())
    }
}

/// What one `packages` tuple pins.
#[derive(Debug, Eq, PartialEq)]
enum Pin<'a> {
    /// The tuple opens with no `<name>@<version>` reference; never reported.
    Malformed,
    /// One of the workspace's own packages; never reported.
    Workspace,
    /// A package to report; an alias resolves to the package it names.
    Package { name: &'a str, version: &'a str },
}

/// What a reference's version text pins under `spelled`.
fn pinned<'a>(spelled: &'a str, version: &'a str) -> Pin<'a> {
    if version.starts_with(WORKSPACE_VERSION_PREFIX) {
        return Pin::Workspace;
    }
    let Some(aliased) = version.strip_prefix(ALIAS_VERSION_PREFIX) else {
        return Pin::Package {
            name: spelled,
            version,
        };
    };
    match split_reference(aliased) {
        Some((name, version)) => Pin::Package { name, version },
        None => Pin::Malformed,
    }
}

/// The install path a `packages` key spells: `node_modules/<name>` for a hoisted package,
/// and `node_modules/<parent>/node_modules/<name>` below each parent for a nested one.
///
/// The key alone spells the location, whether or not the parent is itself a key.
#[cfg(test)]
fn install_path(key: &str) -> Option<String> {
    install_path_with_limit(key, NESTING_DEPTH_MAX)
}

fn install_path_with_limit(key: &str, nesting_depth_max: usize) -> Option<String> {
    let names = nested_names_with_limit(key, nesting_depth_max)?;
    let nesting = format!("{NESTING_SEPARATOR}{NODE_MODULES_DIRECTORY_NAME}{NESTING_SEPARATOR}");
    Some(format!(
        "{NODE_MODULES_DIRECTORY_NAME}{NESTING_SEPARATOR}{}",
        names.join(&nesting)
    ))
}

/// The names a `packages` key nests, outermost first; a scoped name keeps its own `/`.
///
/// Absent when a segment is empty, a scope has no name after it, or the key nests more
/// than `NESTING_DEPTH_MAX` names.
#[cfg(test)]
fn nested_names(key: &str) -> Option<Vec<String>> {
    nested_names_with_limit(key, NESTING_DEPTH_MAX)
}

fn nested_names_with_limit(key: &str, nesting_depth_max: usize) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut segments = key.split(NESTING_SEPARATOR);
    while let Some(segment) = segments.next() {
        if names.len() >= nesting_depth_max {
            return None;
        }
        names.push(nested_name(segment, &mut segments)?);
    }
    Some(names)
}

/// One name from a key: `segment` alone, or a scope joined with the segment after it.
fn nested_name(segment: &str, segments: &mut Split<'_, char>) -> Option<String> {
    if segment.is_empty() {
        return None;
    }
    if !segment.starts_with(SCOPE_PREFIX) {
        return Some(segment.to_owned());
    }
    let name = segments.next().filter(|name| !name.is_empty())?;
    Some(format!("{segment}{NESTING_SEPARATOR}{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bun_resolver_identity_names_bun_and_its_manifest() {
        let resolver = BunResolver::new();
        assert_eq!(resolver.name(), ResolverName::Bun);
        assert_eq!(resolver.manifest_file_name(), "package.json");
    }

    #[test]
    fn test_comments_and_trailing_commas_parse() {
        let text = "{\n  // Bun writes JSONC.\n  \"lockfileVersion\": 1, /* block */\n  \
                    \"workspaces\": { \"\": { \"dependencies\": { \"dep\": \"^1.0.0\", }, }, },\n  \
                    \"overrides\": {\n    \"sharp\": \"^0.35.4\",\n  },\n  \
                    \"packages\": {\n    \"dep\": [\"dep@1.0.0\", \"\", {}, \"\",],\n  },\n}\n";

        let lockfile = parse_lockfile(text.as_bytes()).expect("Bun's JSONC parses");

        let package = &lockfile.packages["dep"];
        assert_eq!(
            package.pin(),
            Pin::Package {
                name: "dep",
                version: "1.0.0"
            }
        );
        assert_eq!(package.registry(), None, "the default registry is empty");
        let failure = parse_lockfile(b"{\"packages\": [")
            .map(|_| ())
            .expect_err("unparsable");
        assert!(
            failure
                .to_string()
                .starts_with("bun.lock could not be parsed: "),
            "{failure}"
        );
    }

    #[test]
    fn test_split_reference_splits_past_the_scope_and_keeps_url_versions() {
        assert_eq!(
            split_reference("@types/react@19.2.18"),
            Some(("@types/react", "19.2.18"))
        );
        assert_eq!(
            split_reference("foo@git+ssh://git@host/x"),
            Some(("foo", "git+ssh://git@host/x"))
        );
        assert_eq!(split_reference("@scope@1.0.0"), Some(("@scope", "1.0.0")));
        assert_eq!(split_reference("@1.0.0"), None, "no name");
        assert_eq!(split_reference("foo@"), None, "no version");
        assert_eq!(split_reference("foo"), None, "no separator");
        assert_eq!(split_reference(""), None);
    }

    #[test]
    fn test_pinned_classifies_workspace_alias_and_plain_versions() {
        assert_eq!(pinned("app", "workspace:."), Pin::Workspace);
        assert_eq!(
            pinned("alias", "npm:real@1.2.3"),
            Pin::Package {
                name: "real",
                version: "1.2.3"
            }
        );
        assert_eq!(pinned("alias", "npm:real"), Pin::Malformed);
        assert_eq!(
            pinned("dep", "1.0.0"),
            Pin::Package {
                name: "dep",
                version: "1.0.0"
            }
        );
    }

    #[test]
    fn test_nested_names_groups_scopes_and_refuses_empty_segments() {
        assert_eq!(nested_names("a"), Some(vec!["a".to_owned()]));
        assert_eq!(
            nested_names("a/b/c"),
            Some(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()])
        );
        assert_eq!(
            nested_names("@s/n/@t/m"),
            Some(vec!["@s/n".to_owned(), "@t/m".to_owned()])
        );
        assert_eq!(nested_names("@s"), None, "a scope needs a name");
        assert_eq!(nested_names("@s/"), None, "a scope needs a nonempty name");
        assert_eq!(nested_names("a//b"), None, "an empty segment names nothing");
        assert_eq!(nested_names(""), None);
    }

    #[test]
    fn test_install_path_stops_at_the_nesting_bound() {
        let at_bound = vec!["p"; NESTING_DEPTH_MAX].join("/");
        let path = install_path(&at_bound).expect("a key at the bound spells a path");
        assert!(path.starts_with("node_modules/p/node_modules/p/"));
        assert_eq!(path.matches("node_modules").count(), NESTING_DEPTH_MAX);

        let past_bound = vec!["p"; NESTING_DEPTH_MAX + 1].join("/");
        assert_eq!(install_path(&past_bound), None);
        assert_eq!(
            install_path("@babel/core/@babel/types").as_deref(),
            Some("node_modules/@babel/core/node_modules/@babel/types")
        );
    }
}
