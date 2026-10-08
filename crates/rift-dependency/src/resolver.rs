//! The resolver contract, and the inputs a resolver reads the workspace through.

use std::fmt;
use std::path::{Path, PathBuf};

use rift_protocol::read::ProjectPath;
use serde::Serialize;
use strum::VariantArray;

use crate::context::ContextAnswer;

/// Bytes one version probe may write to standard output before the inputs stop keeping
/// it. A version line or a sysroot path takes well under a kilobyte, so an output that
/// reaches this bound answers as a failed probe. Held below the server's stream drain
/// ceiling, so an output past it is still counted and reported as truncated.
pub const TOOLCHAIN_OUTPUT_BYTES_MAX: u64 = 64 << 10;
// At the ceiling itself a drained stream stops counting, so an output that filled the
// capture exactly could not be told from one cut short.
const _: () = assert!(
    TOOLCHAIN_OUTPUT_BYTES_MAX < rift_core::STREAM_TOTAL_BYTES_MAX,
    "the toolchain capture must sit below the stream drain ceiling"
);
/// Bytes one lockfile may hold before a resolver refuses to read it.
pub const LOCKFILE_BYTES_MAX: u64 = 16 << 20;
/// Manifests one resolver reads per workspace, at most. The rest are dropped and the
/// drop reported as a degradation.
pub const MANIFESTS_MAX: usize = 256;
/// Packages the dependency context carries per workspace, at most. The rest are dropped
/// and the drop reported as a degradation.
pub const PACKAGES_MAX: usize = 20_000;
/// Directory entries one listing returns, at most. A `site-packages` directory is listed
/// whole, so the bound sits above what an installed application holds.
pub const DIRECTORY_ENTRIES_MAX: usize = 16_384;

/// Identity of one shipped resolver, or of one standard library entry's version probe.
///
/// The lowercase spelling names what degraded in degradation text, the `resolver` field
/// of a `package_context_degraded` warning.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum ResolverName {
    /// Rust packages, as `Cargo.lock` pins them and `Cargo.toml` declares them.
    Cargo,
    /// Python distributions, as `uv.lock` pins them and `pyproject.toml` declares them.
    Uv,
    /// npm packages, as `package-lock.json` pins them and `package.json` declares them.
    Npm,
    /// npm packages, as `bun.lock` pins them.
    Bun,
    /// The Rust standard library, as `rustc --version` or the toolchain file names it.
    #[serde(rename = "stdlib/rust")]
    StdlibRust,
    /// The Node.js runtime modules, as a version pin or `node --version` names them.
    #[serde(rename = "stdlib/node")]
    StdlibNode,
    /// The Python standard library, as the project environment's `pyvenv.cfg` names it.
    #[serde(rename = "stdlib/python")]
    StdlibPython,
}

impl ResolverName {
    /// The lowercase spelling, as the wire serializes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Uv => "uv",
            Self::Npm => "npm",
            Self::Bun => "bun",
            Self::StdlibRust => "stdlib/rust",
            Self::StdlibNode => "stdlib/node",
            Self::StdlibPython => "stdlib/python",
        }
    }
}

impl fmt::Display for ResolverName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One version probe the standard library pass asks its inputs to run.
///
/// The program is a bare name the inputs resolve on their own `PATH`; the pass never
/// names an absolute executable. The run is bounded by the inputs' own wall-clock
/// timeout, which the `[dependencies]` table's `command_timeout` sets, by
/// [`TOOLCHAIN_OUTPUT_BYTES_MAX`], and by the inputs' cancellation, which ends a run
/// the server is stopping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolchainCommand {
    /// The program to run, resolved on the inputs' `PATH`.
    pub program: &'static str,
    /// The arguments, each one literal: no shell parses them.
    pub arguments: Vec<String>,
    /// The directory the program starts in.
    pub working_directory: PathBuf,
    /// Variables laid over the inherited environment, each winning over the inherited
    /// value, such as `RUSTUP_AUTO_INSTALL=0`.
    pub environment: Vec<(&'static str, &'static str)>,
    /// Variables removed from the inherited environment, such as `RUSTUP_TOOLCHAIN`,
    /// which would otherwise override the project's toolchain file.
    pub environment_removed: Vec<&'static str>,
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

/// Why the inputs produced no output for one probe: the program was not found, the run
/// overstayed its bound, or the process could not be observed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandFailure {
    /// The program the inputs tried to run.
    pub program: String,
    /// What stopped the run, in the inputs' own words.
    pub reason: String,
}

impl fmt::Display for CommandFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.program, self.reason)
    }
}

/// What the inputs found at one file path.
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

/// The workspace files a static pass reads.
///
/// A pass holding only this trait reads files and directory listings and nothing else:
/// it cannot run a toolchain or read the environment, because the trait declares no way
/// to ask for either. A resolver's [`DependencyResolver::context`] takes
/// `&mut dyn StaticInputs` for exactly that reason. The server supplies filesystem-backed
/// inputs; tests supply recorded ones.
pub trait StaticInputs {
    /// The collection bounds accepted from workspace configuration and environment.
    fn collection(&self) -> rift_protocol::dependencies::DependenciesCollectionConfiguration {
        rift_protocol::dependencies::DependenciesCollectionConfiguration::default()
    }

    /// The content of one file, refused past `bytes_max`.
    fn read_file(&mut self, path: &Path, bytes_max: u64) -> FileObservation;

    /// The path with every symlink resolved, absent when nothing stands there or the
    /// inputs resolve no link; the caller then compares the lexical path.
    fn canonical_path(&mut self, _path: &Path) -> Option<PathBuf> {
        None
    }

    /// The entry names directly below `path`, at most `entries_max`, in name order. Empty
    /// when no directory stands there or the inputs list none.
    fn list_directory(&mut self, _path: &Path, _entries_max: usize) -> Vec<String> {
        Vec::new()
    }
}

/// The inputs the dependency context reads: static files, and the version probes the
/// standard library entries run.
///
/// A resolver's [`DependencyResolver::context`] still takes [`StaticInputs`] alone; only
/// the standard library pass holds this trait, so the one place a context runs a
/// program is the version probe.
pub trait ContextInputs: StaticInputs {
    /// Runs one probe to completion under the inputs' bounds.
    ///
    /// # Errors
    ///
    /// Returns [`CommandFailure`] when the inputs run no program, when the program
    /// cannot be started, overstays the timeout, is cut by the inputs' cancellation, or
    /// cannot be observed to its end.
    fn run(&mut self, command: &ToolchainCommand) -> Result<CommandOutput, CommandFailure>;
}

/// One resolver's view of a workspace for the static context pass: the absolute root and
/// every visible manifest carrying the resolver's manifest file name, in path order.
#[derive(Clone, Copy, Debug)]
pub struct ContextRequest<'a> {
    /// The workspace root, absolute.
    pub root: &'a Path,
    /// The visible manifests the resolver claims, project-relative, in path order.
    pub manifests: &'a [ProjectPath],
}

/// One shipped resolver: the ecosystem it serves and what its manifests and lockfiles
/// state.
pub trait DependencyResolver: fmt::Debug + Send + Sync {
    /// The resolver's identity.
    fn name(&self) -> ResolverName;

    /// The manifest file name this resolver claims. Every visible file so named reaches
    /// [`DependencyResolver::context`]; npm and Bun both claim `package.json`, and every
    /// other name is claimed by one resolver alone.
    fn manifest_file_name(&self) -> &'static str;

    /// Reports what the request's manifests and lockfiles state about each package the
    /// workspace depends on, reading only through `inputs`.
    ///
    /// A lockfile entry contributes the exact version it pins; a manifest entry no
    /// lockfile pins contributes the requirement it declares. No toolchain runs, because
    /// [`StaticInputs`] offers none. The only installed package files read are Python
    /// `.dist-info/RECORD` lists, for the install folders the answer records.
    fn context(&self, request: &ContextRequest<'_>, inputs: &mut dyn StaticInputs)
    -> ContextAnswer;
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

    /// Inputs that read files alone resolve no link and list no directory, so a caller
    /// compares the lexical path and finds no installed entry.
    #[test]
    fn test_inputs_reading_files_alone_resolve_no_link_and_list_nothing() {
        struct FilesAlone;

        impl StaticInputs for FilesAlone {
            fn read_file(&mut self, _path: &Path, _bytes_max: u64) -> FileObservation {
                FileObservation::Absent
            }
        }

        let mut inputs = FilesAlone;
        let folder = Path::new("/workspace/.venv/lib");
        assert_eq!(inputs.read_file(folder, 1), FileObservation::Absent);
        assert_eq!(inputs.canonical_path(folder), None);
        assert!(inputs.list_directory(folder, 8).is_empty());
    }

    #[test]
    fn test_command_rendering_joins_program_and_arguments() {
        let command = ToolchainCommand {
            program: "rustc",
            arguments: vec!["--print".to_owned(), "sysroot".to_owned()],
            working_directory: PathBuf::from("/workspace"),
            environment: Vec::new(),
            environment_removed: Vec::new(),
        };
        assert_eq!(command.rendered(), "rustc --print sysroot");
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
            program: "rustc".to_owned(),
            reason: "failed to launch".to_owned(),
        };
        assert_eq!(failure.to_string(), "rustc: failed to launch");
    }
}
