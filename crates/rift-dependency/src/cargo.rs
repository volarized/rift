//! The Cargo resolver: Rust packages as `Cargo.lock` pins them and `Cargo.toml` declares them.

mod context;
mod lockfile;

pub(crate) use context::vendored_folders;

use crate::context::ContextAnswer;
use crate::resolver::{ContextRequest, DependencyResolver, ResolverName, StaticInputs};

/// The package namespace every Cargo dependency entry belongs to.
const CARGO_MANAGER: &str = "cargo";
/// The manifest file name this resolver claims.
const CARGO_MANIFEST_FILE_NAME: &str = "Cargo.toml";
/// The lockfile Cargo keeps beside a workspace root manifest.
const CARGO_LOCK_FILE_NAME: &str = "Cargo.lock";

/// The resolver for Rust packages, answering from `Cargo.lock` and `Cargo.toml`.
#[derive(Debug, Default)]
pub struct CargoResolver;

impl CargoResolver {
    /// The Cargo resolver. It holds no state, so one instance serves every workspace.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DependencyResolver for CargoResolver {
    fn name(&self) -> ResolverName {
        ResolverName::Cargo
    }

    fn manifest_file_name(&self) -> &'static str {
        CARGO_MANIFEST_FILE_NAME
    }

    fn context(
        &self,
        request: &ContextRequest<'_>,
        inputs: &mut dyn StaticInputs,
    ) -> ContextAnswer {
        context::cargo_context(request, inputs)
    }
}
