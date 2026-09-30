//! The resolvers Rift ships, in the order they run over a workspace.

use rift_protocol::read::ProjectPath;

use crate::bun::BunResolver;
use crate::cargo::CargoResolver;
use crate::manifest::file_name;
use crate::npm::NpmResolver;
use crate::resolver::DependencyResolver;
use crate::uv::UvResolver;

static CARGO: CargoResolver = CargoResolver::new();
static UV: UvResolver = UvResolver::new();
static NPM: NpmResolver = NpmResolver::new();
static BUN: BunResolver = BunResolver::new();

/// The shipped list, in run order.
static RESOLVERS: [&dyn DependencyResolver; 4] = [&CARGO, &UV, &NPM, &BUN];

/// Every shipped dependency resolver, in the order [`resolve_context`] runs them.
///
/// npm and Bun both claim `package.json`: npm reads its declarations and
/// `package-lock.json`, and Bun reads `bun.lock` alone, so a declaration reaches the
/// context once. The list order decides the order their answers merge in.
///
/// [`resolve_context`]: crate::resolve_context
#[must_use]
pub fn resolvers() -> &'static [&'static dyn DependencyResolver] {
    &RESOLVERS
}

/// Whether a shipped resolver claims `path` as a manifest, by its file name.
///
/// A claimed manifest appearing or changing is a context input even before the context
/// names it, so a rebuild that touches one reads the context again.
#[must_use]
pub fn is_claimed_manifest(path: &ProjectPath) -> bool {
    let file_name = file_name(path);
    resolvers()
        .iter()
        .any(|resolver| resolver.manifest_file_name() == file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::ResolverName;

    #[test]
    fn test_is_claimed_manifest_matches_by_file_name_at_any_depth() {
        assert!(is_claimed_manifest(&ProjectPath("Cargo.toml".to_owned())));
        assert!(is_claimed_manifest(&ProjectPath(
            "crates/rift-core/Cargo.toml".to_owned()
        )));
        assert!(!is_claimed_manifest(&ProjectPath("Cargo.lock".to_owned())));
        assert!(!is_claimed_manifest(&ProjectPath("src/lib.rs".to_owned())));
    }

    #[test]
    fn test_resolvers_lists_every_shipped_resolver_in_run_order() {
        let claimed: Vec<(ResolverName, &str)> = resolvers()
            .iter()
            .map(|resolver| (resolver.name(), resolver.manifest_file_name()))
            .collect();
        assert_eq!(
            claimed,
            [
                (ResolverName::Cargo, "Cargo.toml"),
                (ResolverName::Uv, "pyproject.toml"),
                (ResolverName::Npm, "package.json"),
                (ResolverName::Bun, "package.json"),
            ]
        );
    }
}
