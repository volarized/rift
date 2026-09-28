//! Dependency discovery: the resolvers that read what a workspace's manifests and lockfiles
//! state about the packages it depends on.
//!
//! A [`DependencyResolver`] reads the workspace's manifests and lockfiles through the
//! [`StaticInputs`] the caller supplies, which offer a file read and a directory listing
//! and nothing else, so the answer is a function of what the inputs answered.
//! [`resolvers`] lists the resolvers Rift ships; [`resolve_context`] runs them over one
//! workspace into a [`DependencyContext`]. [`standard_library_answer`] names the standard
//! library entries the workspace's languages rely on, and is the one pass that runs a
//! program: a version probe, through [`ContextInputs`].

mod bun;
mod cargo;
mod context;
mod manifest;
mod node;
mod npm;
mod resolver;
mod resolvers;
mod stdlib;
mod uv;

#[cfg(test)]
mod fixture;

pub use bun::BunResolver;
pub use cargo::CargoResolver;
pub use context::{
    ContextAnswer, Degradation, Degraded, DependencyContext, InstallFolder, InstallLocation,
    resolve_context,
};
pub use npm::NpmResolver;
pub use resolver::{
    CommandFailure, CommandOutput, ContextInputs, ContextRequest, DIRECTORY_ENTRIES_MAX,
    DependencyResolver, FileObservation, LOCKFILE_BYTES_MAX, MANIFESTS_MAX, PACKAGES_MAX,
    ResolverName, StaticInputs, TOOLCHAIN_OUTPUT_BYTES_MAX, ToolchainCommand,
};
pub use resolvers::{is_claimed_manifest, resolvers};
pub use stdlib::{
    STANDARD_LIBRARY_MANAGER, StandardLibrary, StandardLibraryAnswer, StandardLibraryRequest,
    standard_library_answer,
};
pub use uv::UvResolver;
