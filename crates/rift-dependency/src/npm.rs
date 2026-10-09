//! The npm resolver: npm packages as `package-lock.json` pins them and `package.json` declares them.

mod context;

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::context::ContextAnswer;
use crate::manifest::StaticFileFailure;
use crate::node::PACKAGE_MANIFEST_FILE_NAME;
use crate::resolver::{ContextRequest, DependencyResolver, ResolverName, StaticInputs};

/// The lockfile npm keeps beside the manifest it resolved.
const PACKAGE_LOCK_FILE_NAME: &str = "package-lock.json";
/// The `packages` key of the workspace's own root package.
const ROOT_PACKAGE_KEY: &str = "";
/// The directory segment every installed package key carries, with its separator.
const NODE_MODULES_SEGMENT: &str = "node_modules/";
/// The separator that closes the key segment before a `node_modules/` segment.
const KEY_SEPARATOR: char = '/';

/// The resolver for npm packages, answering from `package-lock.json` and `package.json`.
#[derive(Debug, Default)]
pub struct NpmResolver;

impl NpmResolver {
    /// The npm resolver. It holds no state, so one instance serves every workspace.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DependencyResolver for NpmResolver {
    fn name(&self) -> ResolverName {
        ResolverName::Npm
    }

    fn manifest_file_name(&self) -> &'static str {
        PACKAGE_MANIFEST_FILE_NAME
    }

    fn context(
        &self,
        request: &ContextRequest<'_>,
        inputs: &mut dyn StaticInputs,
    ) -> ContextAnswer {
        context::npm_context(request, inputs)
    }
}

/// The `package-lock.json` document, the field this resolver reads. A `lockfileVersion`
/// 1 document carries no `packages` map, so it pins nothing.
#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    packages: BTreeMap<String, LockedPackage>,
}

/// One entry of the `packages` map, keyed by its install path below the manifest.
///
/// The root package sits at the empty key and its three dependency maps name the
/// packages the workspace declares directly. A `link` entry is a symlink to one of the
/// workspace's own packages and pins no version of its own.
#[derive(Deserialize)]
struct LockedPackage {
    version: Option<String>,
    link: Option<bool>,
    /// The locator the package was fetched from. Absent for one the default registry
    /// served and for a package bundled inside another.
    resolved: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
}

/// Parses one lockfile document, naming the parser's message when it is not npm's.
fn parse_lockfile(bytes: &[u8]) -> Result<Lockfile, StaticFileFailure> {
    serde_json::from_slice(bytes)
        .map_err(|error| StaticFileFailure::unparsable(PACKAGE_LOCK_FILE_NAME, error.to_string()))
}

/// One installed package the lockfile pins: its name and version.
#[derive(Debug, Eq, PartialEq)]
struct InstalledPackage<'a> {
    name: &'a str,
    version: &'a str,
}

/// Classifies one `packages` entry; `None` when it is not an installed package.
///
/// An installed package's key carries a `node_modules/` segment: a hoisted package sits
/// directly under it, a nested one under a further segment, and a workspace package's
/// own install under `<package>/node_modules/`. The name is the text after the last
/// segment, and a scoped name keeps its `@scope/` prefix. A key with no such segment is
/// one of the workspace's own packages, a `link` entry is a symlink to one of them, and
/// an entry without a version pins nothing.
fn installed_package<'a>(key: &'a str, package: &'a LockedPackage) -> Option<InstalledPackage<'a>> {
    let (parent, name) = key.rsplit_once(NODE_MODULES_SEGMENT)?;
    let at_segment = parent.is_empty() || parent.ends_with(KEY_SEPARATOR);
    let linked = package.link == Some(true);
    let named = !name.is_empty();
    match package.version.as_deref() {
        Some(version) if at_segment && !linked && named => Some(InstalledPackage { name, version }),
        _ => None,
    }
}

/// Reads installed npm package versions keyed by their lockfile installation path.
/// Workspace links and entries without a version are left out.
///
/// # Errors
///
/// Returns the JSON parser's error when the lockfile is invalid.
pub fn npm_package_versions(bytes: &[u8]) -> Result<BTreeMap<String, String>, serde_json::Error> {
    let lockfile: Lockfile = serde_json::from_slice(bytes)?;
    Ok(lockfile
        .packages
        .iter()
        .filter_map(|(key, package)| {
            installed_package(key, package).map(|package| (key.clone(), package.version.to_owned()))
        })
        .collect())
}

#[cfg(test)]
mod framework_tests {
    #[test]
    fn installed_framework_versions_preserve_workspace_installation_paths() {
        let versions = super::npm_package_versions(
            br#"{"packages":{
            "node_modules/tailwindcss":{"version":"3.4.0"},
            "apps/new/node_modules/tailwindcss":{"version":"4.0.0"},
            "node_modules/linked":{"link":true,"resolved":"apps/linked"},
            "apps/linked":{"version":"1.0.0"},
            "node_modules/missing":{}
        }}"#,
        )
        .expect("valid lockfile");
        assert_eq!(
            versions.get("node_modules/tailwindcss").map(String::as_str),
            Some("3.4.0")
        );
        assert_eq!(
            versions
                .get("apps/new/node_modules/tailwindcss")
                .map(String::as_str),
            Some("4.0.0")
        );
        assert_eq!(versions.len(), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_npm_resolver_identity_names_npm_and_its_manifest() {
        let resolver = NpmResolver::new();
        assert_eq!(resolver.name(), ResolverName::Npm);
        assert_eq!(resolver.manifest_file_name(), "package.json");
    }

    #[test]
    fn test_installed_package_names_the_text_after_the_last_node_modules_segment() {
        let pinned = LockedPackage {
            version: Some("1.0.0".to_owned()),
            link: None,
            resolved: None,
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
        };
        let expected = |name| {
            Some(InstalledPackage {
                name,
                version: "1.0.0",
            })
        };
        assert_eq!(
            installed_package("node_modules/@adobe/css-tools", &pinned),
            expected("@adobe/css-tools")
        );
        assert_eq!(
            installed_package(
                "node_modules/@babel/highlight/node_modules/ansi-styles",
                &pinned
            ),
            expected("ansi-styles")
        );
        assert_eq!(installed_package("packages/app", &pinned), None);
        assert_eq!(
            installed_package("packages/app/node_modules/x", &pinned),
            expected("x"),
            "an install below a workspace package is reported"
        );
        assert_eq!(
            installed_package("my_node_modules/x", &pinned),
            None,
            "the segment is a whole path segment"
        );
        assert_eq!(
            installed_package("node_modules/", &pinned),
            None,
            "an empty name pins nothing"
        );
        let linked = LockedPackage {
            link: Some(true),
            ..pinned
        };
        assert_eq!(installed_package("node_modules/app-lib", &linked), None);
        let unversioned = LockedPackage {
            version: None,
            ..linked
        };
        assert_eq!(
            installed_package("node_modules/app-lib", &unversioned),
            None
        );
    }
}
