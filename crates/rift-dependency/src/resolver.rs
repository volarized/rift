//! The resolver contract, and the inspector a resolver observes the workspace through.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rift_protocol::read::{Language, ProjectPath};
use serde::Serialize;
use strum::VariantArray;

use crate::catalog::Resolution;

/// Bytes one toolchain run may write to standard output before the inspector stops
/// keeping it. Held below the server's stream drain ceiling, so an output that reaches
/// this bound is still counted past it and reported as truncated; a package graph past
/// it answers as a command failure.
pub const TOOLCHAIN_OUTPUT_BYTES_MAX: u64 = 32 << 20;
// At the ceiling itself a drained stream stops counting, so an output that filled the
// capture exactly could not be told from one cut short.
const _: () = assert!(
    TOOLCHAIN_OUTPUT_BYTES_MAX < rift_core::STREAM_TOTAL_BYTES_MAX,
    "the toolchain capture must sit below the stream drain ceiling"
);
/// Wall clock one toolchain run may take before the inspector kills it.
pub const TOOLCHAIN_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// Bytes one lockfile may hold before a resolver refuses to read it.
pub const LOCKFILE_BYTES_MAX: u64 = 16 << 20;
/// Manifests one resolver reads per workspace, at most. The rest are dropped and the
/// drop reported as a degradation.
pub const MANIFESTS_MAX: usize = 256;
/// Packages one resolver catalogs per workspace, at most. The rest are dropped and the
/// drop reported as a degradation.
pub const PACKAGES_MAX: usize = 20_000;
/// Directory entries one listing returns, at most. A flat `node_modules` or a
/// `site-packages` directory is listed whole, so the bound sits above what an installed
/// application holds.
pub const DIRECTORY_ENTRIES_MAX: usize = 16_384;

/// Identity of one shipped resolver.
///
/// The lowercase spelling names the resolver in degradation text. The resolver segment
/// of a source unit is the package namespace instead, [`DependencyResolver::manager`],
/// so two resolvers over one namespace mint one spelling.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum ResolverName {
    /// Rust packages, as `cargo metadata` resolved them.
    Cargo,
    /// Python distributions, as `uv.lock` pins them and the workspace environment holds them.
    Uv,
}

impl ResolverName {
    /// The lowercase spelling, as the wire serializes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Uv => "uv",
        }
    }
}

impl fmt::Display for ResolverName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One toolchain invocation a resolver asks the inspector to run.
///
/// The program is a bare name the inspector resolves on its own `PATH`; a resolver
/// never names an absolute executable. The run is bounded by
/// [`TOOLCHAIN_COMMAND_TIMEOUT`] and [`TOOLCHAIN_OUTPUT_BYTES_MAX`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolchainCommand {
    /// The program to run, resolved on the inspector's `PATH`.
    pub program: &'static str,
    /// The arguments, each one literal: no shell parses them.
    pub arguments: Vec<String>,
    /// The directory the program starts in.
    pub working_directory: PathBuf,
}

impl ToolchainCommand {
    /// One line naming the invocation, for degradation text.
    #[must_use]
    pub fn rendered(&self) -> String {
        std::iter::once(self.program.to_owned())
            .chain(self.arguments.iter().cloned())
            .collect::<Vec<String>>()
            .join(" ")
    }
}

/// What one toolchain run produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    /// The exit code, where the platform reported one.
    pub exit_code: Option<i32>,
    /// Standard output, decoded as UTF-8 with replacement.
    pub stdout: String,
    /// Standard error, decoded as UTF-8 with replacement.
    pub stderr: String,
    /// Whether standard output ran past [`TOOLCHAIN_OUTPUT_BYTES_MAX`] and was cut.
    pub stdout_truncated: bool,
}

impl CommandOutput {
    /// Whether the run exited zero with its whole standard output captured.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.stdout_truncated
    }
}

/// Why the inspector produced no output for one toolchain run: the program was not
/// found, the run overstayed its bound, or the process could not be observed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandFailure {
    /// The program the inspector tried to run.
    pub program: String,
    /// What stopped the run, in the inspector's own words.
    pub reason: String,
}

impl fmt::Display for CommandFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.program, self.reason)
    }
}

/// What the inspector found at one file path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileObservation {
    /// No readable file stands at the path.
    Absent,
    /// The file's whole content, within the requested bound.
    Bytes(Vec<u8>),
    /// The file exists but holds more bytes than the requested bound.
    OverBound {
        /// The file's size on disk.
        bytes: u64,
    },
}

/// The workspace and machine facts a resolver reads.
///
/// Resolvers hold no I/O: every file, directory, environment value, and toolchain run
/// comes through this trait, so a catalog is a function of the inspector's answers.
/// The server supplies a filesystem-backed inspector; tests supply a recorded one.
pub trait Inspector {
    /// The content of one file, refused past `bytes_max`.
    fn read_file(&mut self, path: &Path, bytes_max: u64) -> FileObservation;

    /// Whether a directory stands at `path`.
    fn directory_exists(&mut self, path: &Path) -> bool;

    /// The entry names directly below `path`, at most `entries_max`, in name order.
    /// Empty when no directory stands there.
    fn list_directory(&mut self, path: &Path, entries_max: usize) -> Vec<String>;

    /// Runs one toolchain command to completion under the crate's bounds.
    ///
    /// # Errors
    ///
    /// Returns [`CommandFailure`] when the program cannot be started, overstays
    /// [`TOOLCHAIN_COMMAND_TIMEOUT`], or cannot be observed to its end.
    fn run(&mut self, command: &ToolchainCommand) -> Result<CommandOutput, CommandFailure>;

    /// The value of one environment variable, absent when unset.
    fn environment(&mut self, name: &str) -> Option<String>;

    /// The current user's home directory, absent when the platform names none.
    fn home_directory(&mut self) -> Option<PathBuf>;
}

/// One resolver's view of a workspace: the absolute root and every visible manifest
/// carrying the resolver's manifest file name, in path order.
#[derive(Clone, Copy, Debug)]
pub struct ResolutionRequest<'a> {
    /// The workspace root, absolute.
    pub root: &'a Path,
    /// The visible manifests the resolver claims, project-relative, in path order.
    pub manifests: &'a [ProjectPath],
}

/// One shipped resolver: the ecosystem it serves and how it catalogs that ecosystem's packages.
pub trait DependencyResolver: fmt::Debug + Send + Sync {
    /// The resolver's identity.
    fn name(&self) -> ResolverName;

    /// The package namespace its entries belong to, as `PackageIdentity.manager` spells it
    /// and as the resolver segment of every source unit those entries mint.
    fn manager(&self) -> &'static str;

    /// The language whose syntax provider parses the cataloged packages' source.
    fn language(&self) -> Language;

    /// The manifest file name this resolver claims. Every visible file so named reaches
    /// [`DependencyResolver::resolve`]; no other resolver claims the same name.
    fn manifest_file_name(&self) -> &'static str;

    /// Catalogs the packages the request's manifests resolve to, reading only through
    /// `inspector`. A toolchain the inspector cannot run degrades the answer to what the
    /// static inputs state; it never fails the resolution.
    fn resolve(&self, request: &ResolutionRequest<'_>, inspector: &mut dyn Inspector)
    -> Resolution;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolver_name_spelling_matches_its_wire_form() {
        for name in ResolverName::VARIANTS {
            let wire = serde_json::to_value(name).expect("a resolver name serializes");
            assert_eq!(wire, serde_json::Value::String(name.as_str().to_owned()));
            assert_eq!(name.to_string(), name.as_str());
        }
    }

    #[test]
    fn test_command_rendering_joins_program_and_arguments() {
        let command = ToolchainCommand {
            program: "cargo",
            arguments: vec!["metadata".to_owned(), "--locked".to_owned()],
            working_directory: PathBuf::from("/workspace"),
        };
        assert_eq!(command.rendered(), "cargo metadata --locked");
    }

    #[test]
    fn test_command_output_succeeds_only_on_zero_exit_with_whole_stdout() {
        let whole = CommandOutput {
            exit_code: Some(0),
            stdout: "{}".to_owned(),
            stderr: String::new(),
            stdout_truncated: false,
        };
        assert!(whole.succeeded());
        let cut = CommandOutput {
            stdout_truncated: true,
            ..whole.clone()
        };
        assert!(!cut.succeeded());
        let failed = CommandOutput {
            exit_code: Some(101),
            ..whole
        };
        assert!(!failed.succeeded());
        let failure = CommandFailure {
            program: "cargo".to_owned(),
            reason: "failed to launch".to_owned(),
        };
        assert_eq!(failure.to_string(), "cargo: failed to launch");
    }
}
