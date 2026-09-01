//! The resolvers Rift ships, in the order they run over a workspace.

use rift_protocol::read::ProjectPath;

use crate::bun::BunResolver;
use crate::cargo::CargoResolver;
use crate::catalog::file_name;
use crate::npm::NpmResolver;
use crate::resolver::DependencyResolver;
use crate::uv::UvResolver;

static CARGO: CargoResolver = CargoResolver::new();
static UV: UvResolver = UvResolver::new();
static NPM: NpmResolver = NpmResolver::new();
static BUN: BunResolver = BunResolver::new();

/// The shipped list, in run order.
static RESOLVERS: [&dyn DependencyResolver; 4] = [&CARGO, &UV, &NPM, &BUN];

/// Every shipped dependency resolver, in the order [`resolve_catalog`] runs them.
///
/// Each resolver claims one manifest file name, so two resolvers never read one
/// manifest; the list order only decides the order their entries are assembled in.
/// npm and Bun both claim `package.json`; each reads on when its own lockfile stands
/// beside the manifest, so one manifest still reaches one resolver's answer.
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
