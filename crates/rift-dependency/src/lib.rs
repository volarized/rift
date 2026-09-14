//! Dependency discovery: the resolvers that catalog what a workspace's toolchains resolved.
//!
//! A [`DependencyResolver`] reads the workspace's manifests and lockfiles, asks the
//! toolchain that resolved them for its package graph, and mints one
//! [`CatalogEntry`] per resolved package. The crate holds no I/O of its own: every
//! file read, directory probe, and toolchain run goes through the [`Inspector`] the
//! caller supplies, so the catalog is a function of what the inspector answered.
//! [`resolvers`] lists the resolvers Rift ships; [`resolve_catalog`] runs them over
//! one workspace.

mod bun;
mod cargo;
mod catalog;
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
pub use npm::NpmResolver;
pub use resolver::{
    CommandFailure, CommandOutput, DIRECTORY_ENTRIES_MAX, DependencyResolver, FileObservation,
    Inspector, LOCKFILE_BYTES_MAX, MANIFESTS_MAX, PACKAGES_MAX, ResolutionRequest, ResolverName,
    TOOLCHAIN_COMMAND_TIMEOUT, TOOLCHAIN_OUTPUT_BYTES_MAX, ToolchainCommand,
};
pub use resolvers::{is_claimed_manifest, resolvers};
pub use uv::UvResolver;
