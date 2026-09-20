//! Dependency discovery: the resolvers that catalog what a workspace's toolchains resolved.
//!
//! A [`DependencyResolver`] reads the workspace's manifests and lockfiles, asks the
//! toolchain that resolved them for its package graph, and mints one
//! [`CatalogEntry`] per resolved package. The crate holds no I/O of its own: every
//! file read, directory probe, and toolchain run goes through the [`Inspector`] the
//! caller supplies, so the catalog is a function of what the inspector answered.
//! [`resolvers`] lists the resolvers Rift ships; [`resolve_catalog`] runs them over
//! one workspace.
//!
//! [`resolve_context`] is the second pass over the same resolvers. It reads the
//! manifests and lockfiles alone, through a [`StaticInputs`] that offers a file read
//! and nothing else, and reports what each states about the packages the workspace
//! depends on.

mod bun;
mod cargo;
mod catalog;
mod context;
mod manifest;
mod node;
mod npm;
mod resolver;
mod resolvers;
mod uv;

#[cfg(test)]
mod fixture;

pub use bun::BunResolver;
pub use cargo::CargoResolver;
pub use catalog::{
    CatalogEntry, Degradation, DependencyCatalog, PackageLocation, Resolution, resolve_catalog,
};
pub use context::{ContextAnswer, DependencyContext, resolve_context};
pub use npm::NpmResolver;
pub use resolver::{
    CommandFailure, CommandOutput, ContextRequest, DIRECTORY_ENTRIES_MAX, DependencyResolver,
    FileObservation, Inspector, LOCKFILE_BYTES_MAX, MANIFESTS_MAX, PACKAGES_MAX, ResolutionRequest,
    ResolverName, StaticInputs, TOOLCHAIN_OUTPUT_BYTES_MAX, ToolchainCommand,
};
pub use resolvers::{is_claimed_manifest, resolvers};
pub use uv::UvResolver;
