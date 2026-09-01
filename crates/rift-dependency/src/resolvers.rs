//! The resolvers Rift ships, in the order they run over a workspace.

use rift_protocol::read::ProjectPath;

use crate::cargo::CargoResolver;
use crate::catalog::file_name;
use crate::resolver::DependencyResolver;
use crate::uv::UvResolver;

static CARGO: CargoResolver = CargoResolver::new();
static UV: UvResolver = UvResolver::new();

/// The shipped list, in run order.
static RESOLVERS: [&dyn DependencyResolver; 2] = [&CARGO, &UV];

/// Every shipped dependency resolver, in the order [`resolve_catalog`] runs them.
///
/// Each resolver claims one manifest file name, so two resolvers never read one
/// manifest; the list order only decides the order their entries are assembled in.
///
/// [`resolve_catalog`]: crate::resolve_catalog
#[must_use]
pub fn resolvers() -> &'static [&'static dyn DependencyResolver] {
    &RESOLVERS
}

/// Whether a shipped resolver claims `path` as a manifest, by its file name.
///
/// A claimed manifest appearing or changing is a resolution input even before any
/// catalog names it, so a rebuild that touches one resolves the catalog again.
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
    fn test_resolvers_lists_the_cargo_resolver_over_cargo_toml() {
        let claimed: Vec<(ResolverName, &str)> = resolvers()
            .iter()
            .map(|resolver| (resolver.name(), resolver.manifest_file_name()))
            .collect();
        assert_eq!(claimed, [(ResolverName::Cargo, "Cargo.toml")]);
    }
}
