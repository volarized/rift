//! Standard library entries: the packages a language's code calls without a manifest
//! naming them.
//!
//! [`standard_library_answer`] names one entry per standard library the caller asks
//! for. The version comes from static pins first, then, under `resolution = "auto"`,
//! from one version probe run with `RUSTUP_AUTO_INSTALL=0` and `MISE_AUTO_INSTALL=0`
//! laid over its environment, so no toolchain manager installs what a pin names. A
//! probe that answers nothing leaves the static reading, or the requirement `>=0`, and
//! a degradation naming the entry. A Rust version the probe read also records the
//! standard library's install folder below `rustc --print sysroot`.

use std::path::{Path, PathBuf};

use rift_core::line::{lines_inclusive, without_ending};
use rift_protocol::dependencies::{
    PackageAvailability, PackageContextEntry, PackageSelector, REQUIREMENT_ANY,
};
use rift_protocol::read::ProjectPath;

use rift_protocol::read::PackageIdentity;

use crate::context::{InstallFolder, InstallLocation, is_whole_version};
use crate::resolver::{
    CommandOutput, ContextInputs, FileObservation, ResolverName, StaticInputs, ToolchainCommand,
};

/// The manager every standard library entry names, as local reads minted it.
pub const STANDARD_LIBRARY_MANAGER: &str = "stdlib";
/// Bytes one pin file may hold before the pass leaves it unread.
const PIN_FILE_BYTES_MAX: u64 = 64 << 10;
/// Bytes one `package.json` or `pyproject.toml` may hold before the pass leaves it unread.
const PROJECT_FILE_BYTES_MAX: u64 = 1 << 20;
/// The arguments of the version probe every program answers.
const VERSION_ARGUMENTS: [&str; 1] = ["--version"];
/// The arguments naming the toolchain's sysroot, which holds the standard library source.
const SYSROOT_ARGUMENTS: [&str; 2] = ["--print", "sysroot"];
/// The standard library source below the sysroot, as the `rust-src` component installs it.
const SYSROOT_LIBRARY_SEGMENTS: [&str; 5] = ["lib", "rustlib", "src", "rust", "library"];
/// The first rustup release that honors `RUSTUP_AUTO_INSTALL=0`.
const RUSTUP_AUTO_INSTALL_FLOOR: [u64; 3] = [1, 28, 1];
/// Laid over every probe's environment so no rustup proxy installs a toolchain.
const RUSTUP_AUTO_INSTALL_OFF: (&str, &str) = ("RUSTUP_AUTO_INSTALL", "0");
/// Laid over every probe's environment so no mise shim installs a pinned runtime.
const MISE_AUTO_INSTALL_OFF: (&str, &str) = ("MISE_AUTO_INSTALL", "0");
/// Removed from every probe's environment: rustup prefers it over the project's
/// toolchain file, so a server started under `cargo` would report its own toolchain.
const RUSTUP_TOOLCHAIN_VARIABLE: &str = "RUSTUP_TOOLCHAIN";
/// The npm package whose `lib.*.d.ts` files declare the ECMAScript built-ins.
const TYPESCRIPT_PACKAGE: &str = "typescript";
/// The npm package manager name.
const NPM_MANAGER: &str = "npm";

/// The Rust toolchain files rustup reads, in the order it prefers them.
const RUST_TOOLCHAIN_FILES: [&str; 2] = ["rust-toolchain.toml", "rust-toolchain"];
/// The Python environment file `uv venv` and `python3 -m venv` write.
const PYVENV_FILE: &str = ".venv/pyvenv.cfg";
/// The `pyvenv.cfg` keys naming the interpreter version: uv's, then `venv`'s.
const PYVENV_VERSION_KEYS: [&str; 2] = ["version_info", "version"];
/// The Python pin pyenv and uv read.
const PYTHON_VERSION_FILE: &str = ".python-version";
/// The project manifest naming `requires-python`.
const PYPROJECT_FILE: &str = "pyproject.toml";
/// The Node.js pins read before any probe runs, in order.
const NODE_PIN_FILES: [&str; 5] = [
    ".nvmrc",
    ".node-version",
    "package.json",
    ".tool-versions",
    "mise.toml",
];
/// The tool names `.tool-versions` and `mise.toml` spell Node.js with.
const NODE_TOOL_NAMES: [&str; 2] = ["nodejs", "node"];

/// One standard library a language's code relies on.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StandardLibrary {
    /// `stdlib/rust`, for Rust.
    Rust,
    /// `stdlib/node` and npm `typescript`, for JavaScript, TypeScript, and TSX.
    Node,
    /// `stdlib/python`, for Python.
    Python,
}

impl StandardLibrary {
    /// The standard library a shipped language's code relies on, by its exact
    /// identity segment; `None` for a language that has none, such as `markdown`.
    #[must_use]
    pub fn for_language(identity: &str) -> Option<Self> {
        match identity {
            "rust" => Some(Self::Rust),
            "javascript" | "typescript" | "typescript:tsx" => Some(Self::Node),
            "python" => Some(Self::Python),
            _ => None,
        }
    }

    /// The entry name under [`STANDARD_LIBRARY_MANAGER`].
    const fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Node => "node",
            Self::Python => "python",
        }
    }

    /// The name a degradation carries: `stdlib/<name>`.
    const fn resolver(self) -> ResolverName {
        match self {
            Self::Rust => ResolverName::StdlibRust,
            Self::Node => ResolverName::StdlibNode,
            Self::Python => ResolverName::StdlibPython,
        }
    }
}

/// What the standard library pass reads: the root, the libraries the workspace's
/// languages rely on with their `stdlib` key on, and whether a probe may run.
#[derive(Clone, Copy, Debug)]
pub struct StandardLibraryRequest<'a> {
    /// The workspace root, absolute.
    pub root: &'a Path,
    /// The libraries to name, each once.
    pub libraries: &'a [StandardLibrary],
    /// Whether a version probe may run: `resolution = "auto"`.
    pub execution: bool,
}

/// One library's entries, the pin files it read, and why its version fell back.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StandardLibraryAnswer {
    /// The entries, one per package.
    pub entries: Vec<PackageContextEntry>,
    /// Where a library whose exact version the probe read is installed: the Rust
    /// standard library below the sysroot.
    pub install_folders: Vec<InstallFolder>,
    /// The workspace paths the pass read or would read; a change to any makes the
    /// answer stale.
    pub inputs: Vec<ProjectPath>,
    /// Each fallback, by the entry it names.
    pub degradations: Vec<(ResolverName, String)>,
    /// The libraries the request named, each once.
    pub libraries: Vec<StandardLibrary>,
}

/// Names every requested library's entries.
pub fn standard_library_answer(
    request: &StandardLibraryRequest<'_>,
    inputs: &mut dyn ContextInputs,
) -> StandardLibraryAnswer {
    let mut answer = StandardLibraryAnswer {
        libraries: request.libraries.to_vec(),
        ..StandardLibraryAnswer::default()
    };
    for library in request.libraries {
        let outcome = match library {
            StandardLibrary::Rust => rust_version(request, inputs, &mut answer.inputs),
            StandardLibrary::Node => node_version(request, inputs, &mut answer.inputs),
            StandardLibrary::Python => python_version(request.root, inputs, &mut answer.inputs),
        };
        answer.degradations.extend(
            outcome
                .degradations
                .into_iter()
                .map(|reason| (library.resolver(), reason)),
        );
        if let (PackageSelector::Version(version), Some(folder)) =
            (&outcome.selector, outcome.install_folder)
        {
            answer.install_folders.push(InstallFolder {
                package: PackageIdentity {
                    manager: STANDARD_LIBRARY_MANAGER.to_owned(),
                    name: library.name().to_owned(),
                    version: version.clone(),
                },
                location: InstallLocation::Path(folder),
            });
        }
        answer.entries.push(PackageContextEntry::new(
            STANDARD_LIBRARY_MANAGER,
            library.name(),
            outcome.selector,
            PackageAvailability::Canonical,
        ));
        if *library == StandardLibrary::Node {
            answer.entries.push(PackageContextEntry::new(
                NPM_MANAGER,
                TYPESCRIPT_PACKAGE,
                PackageSelector::Requirement(REQUIREMENT_ANY.to_owned()),
                PackageAvailability::Canonical,
            ));
        }
    }
    answer
}

/// One library's selector, where it is installed when the probe found it, and why the
/// probe fell short when it did.
struct VersionOutcome {
    selector: PackageSelector,
    install_folder: Option<PathBuf>,
    degradations: Vec<String>,
}

impl VersionOutcome {
    const fn read(selector: PackageSelector) -> Self {
        Self {
            selector,
            install_folder: None,
            degradations: Vec::new(),
        }
    }

    fn degraded(selector: PackageSelector, reason: String) -> Self {
        Self {
            selector,
            install_folder: None,
            degradations: vec![reason],
        }
    }
}

/// A version pin as its file spells it, sorted into what goes out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Pin {
    /// A whole version: goes out exact.
    Exact(String),
    /// A partial version or a range: goes out as a requirement.
    Requirement(String),
    /// A named channel or alias no version rule parses: goes out as `>=0`.
    Named(String),
}

impl Pin {
    fn selector(self) -> PackageSelector {
        match self {
            Self::Exact(version) => PackageSelector::Version(version),
            Self::Requirement(requirement) => PackageSelector::Requirement(requirement),
            Self::Named(_) => any(),
        }
    }
}

fn any() -> PackageSelector {
    PackageSelector::Requirement(REQUIREMENT_ANY.to_owned())
}

// Rust

/// The Rust version: `rustc --version` under the rustup gate, else the toolchain file.
fn rust_version(
    request: &StandardLibraryRequest<'_>,
    inputs: &mut dyn ContextInputs,
    read: &mut Vec<ProjectPath>,
) -> VersionOutcome {
    let pinned = RUST_TOOLCHAIN_FILES.iter().find_map(|file| {
        read.push(ProjectPath((*file).to_owned()));
        let text = pin_text(inputs, &request.root.join(file))?;
        rust_toolchain_pin(&text, *file == RUST_TOOLCHAIN_FILES[1])
    });
    let static_selector = pinned.clone().map_or_else(any, Pin::selector);
    if !request.execution {
        return VersionOutcome::read(static_selector);
    }
    let version = match probe_rustc(request.root, inputs) {
        Ok(version) => version,
        Err(reason) => return VersionOutcome::degraded(static_selector, reason),
    };
    let mut outcome = VersionOutcome::read(PackageSelector::Version(version));
    match probe_sysroot(request.root, inputs) {
        Ok(sysroot) => {
            outcome.install_folder = Some(
                SYSROOT_LIBRARY_SEGMENTS
                    .iter()
                    .fold(sysroot, |folder, segment| folder.join(segment)),
            );
        }
        Err(reason) => outcome.degradations.push(reason),
    }
    outcome
}

/// Runs `rustc --print sysroot` in the root, under the same install guards as the
/// version probe: the folder below it holds the standard library source.
fn probe_sysroot(root: &Path, inputs: &mut dyn ContextInputs) -> Result<PathBuf, String> {
    let rustc = probe_command("rustc", &SYSROOT_ARGUMENTS, root.to_path_buf());
    let output = inputs.run(&rustc).map_err(|failure| failure.to_string())?;
    let sysroot = first_line(&output.stdout);
    if !output.succeeded() || sysroot.is_empty() {
        return Err(format!(
            "rustc --print sysroot: {}; no install folder",
            first_line(&output.stderr)
        ));
    }
    Ok(PathBuf::from(sysroot))
}

/// Runs `rustup --version` from a folder no toolchain file governs, then, when rustup
/// is absent or honors `RUSTUP_AUTO_INSTALL`, `rustc --version` in the root.
fn probe_rustc(root: &Path, inputs: &mut dyn ContextInputs) -> Result<String, String> {
    let neutral = root.ancestors().last().unwrap_or(root).to_path_buf();
    let rustup = probe_command("rustup", &VERSION_ARGUMENTS, neutral);
    // rustup exits 0 whether or not it reached the active toolchain, so its output
    // decides, never its exit code.
    if let Ok(output) = inputs.run(&rustup) {
        let version = rustup_version(&output.stdout);
        if version.is_none_or(|version| version < RUSTUP_AUTO_INSTALL_FLOOR) {
            let seen = first_line(&output.stdout);
            return Err(format!(
                "rustup below 1.28.1 ignores RUSTUP_AUTO_INSTALL ({seen}); rustc not run"
            ));
        }
    }
    let rustc = probe_command("rustc", &VERSION_ARGUMENTS, root.to_path_buf());
    let output = inputs.run(&rustc).map_err(|failure| failure.to_string())?;
    if output.exit_code != Some(0) {
        return Err(format!("rustc --version: {}", first_line(&output.stderr)));
    }
    rustc_version(&output.stdout).ok_or_else(|| {
        format!(
            "rustc --version printed no version: {}",
            first_line(&output.stdout)
        )
    })
}

/// The version triple of `rustup --version`'s first line, `rustup 1.29.1 (...)`.
#[must_use]
pub(crate) fn rustup_version(stdout: &str) -> Option<[u64; 3]> {
    let word = first_line(stdout).strip_prefix("rustup ")?;
    let version = word.split_whitespace().next()?;
    let mut numbers = version.split('.').map(|number| number.parse::<u64>().ok());
    let triple = [numbers.next()??, numbers.next()??, numbers.next()??];
    numbers.next().is_none().then_some(triple)
}

/// The version word of `rustc --version`'s first line, `rustc 1.98.1 (...)`.
#[must_use]
pub(crate) fn rustc_version(stdout: &str) -> Option<String> {
    let version = first_line(stdout)
        .strip_prefix("rustc ")?
        .split_whitespace()
        .next()?;
    is_whole_version(version).then(|| version.to_owned())
}

/// The channel a toolchain file names, sorted: `1.98.0` exact, `1.98` as `~1.98`, a
/// named channel such as `stable` or `nightly-2026-09-01` as named. A `path` toolchain
/// names no channel. `legacy` reads a plain `rust-toolchain` file, which holds either
/// the TOML form or one bare channel line.
#[must_use]
pub(crate) fn rust_toolchain_pin(text: &str, legacy: bool) -> Option<Pin> {
    let channel = match text.parse::<toml::Table>() {
        Ok(table) => table
            .get("toolchain")?
            .get("channel")?
            .as_str()?
            .trim()
            .to_owned(),
        Err(_) if legacy => first_line(text).trim().to_owned(),
        Err(_) => return None,
    };
    Some(rust_channel_pin(&channel))
}

/// A rustup channel sorted, its host suffix dropped: `1.98-aarch64-apple-darwin` is `1.98`.
fn rust_channel_pin(channel: &str) -> Pin {
    if !channel.starts_with(|first: char| first.is_ascii_digit()) {
        return Pin::Named(channel.to_owned());
    }
    let release = channel.split('-').next().unwrap_or(channel);
    match release.split('.').count() {
        3 if is_whole_version(release) => Pin::Exact(release.to_owned()),
        2 => Pin::Requirement(format!("~{release}")),
        _ => Pin::Named(channel.to_owned()),
    }
}

// Node.js

/// The Node.js version: the first pin, then `node --version` under `auto`, then
/// `engines.node`.
fn node_version(
    request: &StandardLibraryRequest<'_>,
    inputs: &mut dyn ContextInputs,
    read: &mut Vec<ProjectPath>,
) -> VersionOutcome {
    let mut engines = None;
    for file in NODE_PIN_FILES {
        read.push(ProjectPath(file.to_owned()));
        let bytes_max = if file == "package.json" {
            PROJECT_FILE_BYTES_MAX
        } else {
            PIN_FILE_BYTES_MAX
        };
        let Some(text) = file_text(inputs, &request.root.join(file), bytes_max) else {
            continue;
        };
        let pin = match file {
            ".nvmrc" | ".node-version" => node_pin_line(&text),
            "package.json" => {
                engines = package_engines_node(&text);
                package_volta_node(&text)
            }
            ".tool-versions" => tool_versions_node(&text),
            _ => mise_toml_node(&text),
        };
        if let Some(pin) = pin {
            return VersionOutcome::read(pin.selector());
        }
    }
    let fallback = engines.map_or_else(any, PackageSelector::Requirement);
    if !request.execution {
        return VersionOutcome::read(fallback);
    }
    let node = probe_command("node", &VERSION_ARGUMENTS, request.root.to_path_buf());
    let probed = inputs
        .run(&node)
        .map_err(|failure| failure.to_string())
        .and_then(|output| node_probe_version(&output));
    match probed {
        Ok(version) => VersionOutcome::read(PackageSelector::Version(version)),
        Err(reason) => VersionOutcome::degraded(fallback, reason),
    }
}

fn node_probe_version(output: &CommandOutput) -> Result<String, String> {
    if output.exit_code != Some(0) {
        return Err(format!("node --version: {}", first_line(&output.stderr)));
    }
    match node_version_word(first_line(&output.stdout)) {
        Pin::Exact(version) => Ok(version),
        _ => Err(format!(
            "node --version printed no version: {}",
            first_line(&output.stdout)
        )),
    }
}

/// One Node.js version word sorted: `v22.3.0` exact, `22` or `v22.3` a requirement,
/// `lts/*`, `lts/iron`, `node`, or `system` named.
#[must_use]
pub(crate) fn node_version_word(word: &str) -> Pin {
    let word = word.trim();
    let version = word.strip_prefix('v').unwrap_or(word);
    let numeric = !version.is_empty()
        && version
            .split('.')
            .all(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()));
    match version.split('.').count() {
        3 if numeric => Pin::Exact(version.to_owned()),
        1 | 2 if numeric => Pin::Requirement(version.to_owned()),
        _ => Pin::Named(word.to_owned()),
    }
}

/// The version `.nvmrc` or `.node-version` names: its first line that is not blank
/// or a `#` comment, with a trailing comment cut.
#[must_use]
pub(crate) fn node_pin_line(text: &str) -> Option<Pin> {
    lines_inclusive(text)
        .map(|line| uncommented(without_ending(line)))
        .find(|line| !line.is_empty())
        .map(node_version_word)
}

/// The version `.tool-versions` names for `nodejs` or `node`: the first of the
/// versions its line lists.
#[must_use]
pub(crate) fn tool_versions_node(text: &str) -> Option<Pin> {
    lines_inclusive(text).find_map(|line| {
        let mut words = uncommented(without_ending(line)).split_whitespace();
        let tool = words.next()?;
        NODE_TOOL_NAMES
            .contains(&tool)
            .then(|| words.next().map(node_version_word))
            .flatten()
    })
}

/// The version `mise.toml` names under `[tools]`: a string, the first of an array, or
/// a table's `version`.
#[must_use]
pub(crate) fn mise_toml_node(text: &str) -> Option<Pin> {
    let table = text.parse::<toml::Table>().ok()?;
    let tools = table.get("tools")?.as_table()?;
    let value = NODE_TOOL_NAMES.iter().find_map(|name| tools.get(*name))?;
    let word = match value {
        toml::Value::String(word) => word.as_str(),
        toml::Value::Array(words) => words.first()?.as_str()?,
        toml::Value::Table(table) => table.get("version")?.as_str()?,
        _ => return None,
    };
    Some(node_version_word(word))
}

/// `package.json`'s `volta.node`.
#[must_use]
pub(crate) fn package_volta_node(text: &str) -> Option<Pin> {
    let manifest: serde_json::Value = serde_json::from_str(text).ok()?;
    let word = manifest.get("volta")?.get("node")?.as_str()?;
    Some(node_version_word(word))
}

/// `package.json`'s `engines.node` range, as the requirement it spells.
#[must_use]
pub(crate) fn package_engines_node(text: &str) -> Option<String> {
    let manifest: serde_json::Value = serde_json::from_str(text).ok()?;
    let range = manifest.get("engines")?.get("node")?.as_str()?.trim();
    (!range.is_empty()).then(|| range.to_owned())
}

// Python

/// The Python version: the project environment's `pyvenv.cfg`, then `.python-version`,
/// then `requires-python`. No interpreter runs.
fn python_version(
    root: &Path,
    inputs: &mut dyn StaticInputs,
    read: &mut Vec<ProjectPath>,
) -> VersionOutcome {
    read.extend(
        [PYVENV_FILE, PYTHON_VERSION_FILE, PYPROJECT_FILE].map(|file| ProjectPath(file.to_owned())),
    );
    let pin = pin_text(inputs, &root.join(PYVENV_FILE))
        .and_then(|text| pyvenv_version(&text))
        .or_else(|| {
            pin_text(inputs, &root.join(PYTHON_VERSION_FILE))
                .and_then(|text| python_pin_line(&text))
        })
        .or_else(|| {
            file_text(inputs, &root.join(PYPROJECT_FILE), PROJECT_FILE_BYTES_MAX)
                .and_then(|text| requires_python(&text))
        });
    VersionOutcome::read(pin.map_or_else(any, Pin::selector))
}

/// The interpreter version `pyvenv.cfg` names: `version_info`, which uv writes, else
/// `version`, which `python3 -m venv` writes.
#[must_use]
pub(crate) fn pyvenv_version(text: &str) -> Option<Pin> {
    let pairs: Vec<(&str, &str)> = lines_inclusive(text)
        .filter_map(|line| {
            let (key, value) = without_ending(line).split_once('=')?;
            Some((key.trim(), value.trim()))
        })
        .collect();
    PYVENV_VERSION_KEYS.iter().find_map(|wanted| {
        pairs
            .iter()
            .find(|(key, _)| key == wanted)
            .map(|(_, value)| python_version_word(value))
    })
}

/// The version `.python-version` names: its first line that is not blank or a comment.
#[must_use]
pub(crate) fn python_pin_line(text: &str) -> Option<Pin> {
    lines_inclusive(text)
        .map(|line| uncommented(without_ending(line)))
        .find(|line| !line.is_empty())
        .map(python_version_word)
}

/// One Python version word sorted: `3.12.3` exact, `3.12` as `==3.12.*`, and
/// `pypy@3.10`, `3.14t`, or `cpython-3.12` named.
#[must_use]
pub(crate) fn python_version_word(word: &str) -> Pin {
    let numeric = word
        .split('.')
        .all(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()));
    match word.split('.').count() {
        3 if numeric => Pin::Exact(word.to_owned()),
        2 if numeric => Pin::Requirement(format!("=={word}.*")),
        _ => Pin::Named(word.to_owned()),
    }
}

/// `pyproject.toml`'s `[project] requires-python`, as the requirement it spells.
#[must_use]
pub(crate) fn requires_python(text: &str) -> Option<Pin> {
    let table = text.parse::<toml::Table>().ok()?;
    let range = table
        .get("project")?
        .get("requires-python")?
        .as_str()?
        .trim();
    (!range.is_empty()).then(|| Pin::Requirement(range.to_owned()))
}

// Shared

/// A probe command: `<program> <arguments>` with both install guards laid over its
/// environment and `RUSTUP_TOOLCHAIN` removed from it.
fn probe_command(
    program: &'static str,
    arguments: &[&str],
    working_directory: PathBuf,
) -> ToolchainCommand {
    ToolchainCommand {
        program,
        arguments: arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        working_directory,
        environment: vec![RUSTUP_AUTO_INSTALL_OFF, MISE_AUTO_INSTALL_OFF],
        environment_removed: vec![RUSTUP_TOOLCHAIN_VARIABLE],
    }
}

fn pin_text(inputs: &mut dyn StaticInputs, path: &Path) -> Option<String> {
    file_text(inputs, path, PIN_FILE_BYTES_MAX)
}

fn file_text(inputs: &mut dyn StaticInputs, path: &Path, bytes_max: u64) -> Option<String> {
    match inputs.read_file(path, bytes_max) {
        FileObservation::Bytes(bytes) => String::from_utf8(bytes).ok(),
        FileObservation::Absent | FileObservation::OverBound { .. } => None,
    }
}

fn first_line(text: &str) -> &str {
    lines_inclusive(text)
        .next()
        .map_or("", without_ending)
        .trim()
}

/// `line` with a `#` comment cut and its blanks trimmed.
fn uncommented(line: &str) -> &str {
    line.split_once('#').map_or(line, |(kept, _)| kept).trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::RecordedInspector;

    const ROOT: &str = "/workspace";

    fn exact(version: &str) -> Pin {
        Pin::Exact(version.to_owned())
    }

    fn requirement(requirement: &str) -> Pin {
        Pin::Requirement(requirement.to_owned())
    }

    fn named(name: &str) -> Pin {
        Pin::Named(name.to_owned())
    }

    fn answer(
        libraries: &[StandardLibrary],
        execution: bool,
        inputs: &mut RecordedInspector,
    ) -> StandardLibraryAnswer {
        let request = StandardLibraryRequest {
            root: Path::new(ROOT),
            libraries,
            execution,
        };
        standard_library_answer(&request, inputs)
    }

    fn selector_of(answer: &StandardLibraryAnswer, name: &str) -> (Option<String>, Option<String>) {
        let entry = answer
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("{name} named"));
        (entry.version.clone(), entry.requirement.clone())
    }

    fn version(text: &str) -> (Option<String>, Option<String>) {
        (Some(text.to_owned()), None)
    }

    fn range(text: &str) -> (Option<String>, Option<String>) {
        (None, Some(text.to_owned()))
    }

    // Spellings from real toolchain files under ~/projects and ~/sources, and from
    // rustup's documented forms.
    #[test]
    fn test_rust_toolchain_pin_sorts_every_channel_spelling() {
        let cases: [(&str, bool, Option<Pin>); 16] = [
            (
                "[toolchain]\nchannel = '1.78.0'\n",
                false,
                Some(exact("1.78.0")),
            ),
            (
                "[toolchain]\nprofile = \"minimal\"\nchannel = 'stable'",
                false,
                Some(named("stable")),
            ),
            (
                "[toolchain]\nchannel = \"miri\"\n",
                false,
                Some(named("miri")),
            ),
            ("[toolchain]\ncomponents = [\"rustfmt\"]\n", false, None),
            ("nightly\n", true, Some(named("nightly"))),
            (
                "[toolchain]\nchannel = \"1.98.0\"\n",
                false,
                Some(exact("1.98.0")),
            ),
            (
                "[toolchain]\nchannel = \"1.98\"\n",
                false,
                Some(requirement("~1.98")),
            ),
            (
                "[toolchain]\nchannel = \"stable\"\n",
                false,
                Some(named("stable")),
            ),
            (
                "[toolchain]\nchannel = \"nightly-2026-09-01\"\ncomponents = [\"rust-src\"]\n",
                false,
                Some(named("nightly-2026-09-01")),
            ),
            (
                "[toolchain]\nchannel = \"1.98-aarch64-apple-darwin\"\n",
                false,
                Some(requirement("~1.98")),
            ),
            (
                "[toolchain]\nchannel = \"1.80.0-aarch64-apple-darwin\"\n",
                false,
                Some(exact("1.80.0")),
            ),
            ("[toolchain]\npath = \"/opt/my-toolchain\"\n", false, None),
            (
                "nightly-2020-07-10\n",
                true,
                Some(named("nightly-2020-07-10")),
            ),
            ("1.45.0\n", true, Some(exact("1.45.0"))),
            (
                "[toolchain]\nchannel = \"1.70.0\"\n",
                true,
                Some(exact("1.70.0")),
            ),
            ("1.45.0\n", false, None),
        ];
        for (text, legacy, expected) in cases {
            assert_eq!(rust_toolchain_pin(text, legacy), expected, "{text:?}");
        }
    }

    /// A channel rustup does not spell, such as a bare major or a four-part release,
    /// names no version rule, so it goes out named.
    #[test]
    fn test_a_channel_rustup_does_not_spell_goes_out_named() {
        assert_eq!(
            rust_toolchain_pin("[toolchain]\nchannel = \"1\"\n", false),
            Some(named("1"))
        );
        assert_eq!(
            rust_toolchain_pin("1.98.0.1\n", true),
            Some(named("1.98.0.1"))
        );
    }

    #[test]
    fn test_rustup_and_rustc_version_lines_parse() {
        assert_eq!(
            rustup_version("rustup 1.29.1 (d95a37b6a 2026-08-13)\n"),
            Some([1, 29, 1])
        );
        assert_eq!(
            rustup_version("rustup 1.27.1 (54dd3d00f 2024-04-24)"),
            Some([1, 27, 1])
        );
        assert_eq!(rustup_version("info: no rustc"), None);
        assert_eq!(
            rustc_version("rustc 1.98.1 (48a229cea 2026-09-01)\n").as_deref(),
            Some("1.98.1")
        );
        assert_eq!(
            rustc_version("rustc 1.99.0-nightly (0123abcd 2026-09-20)").as_deref(),
            Some("1.99.0-nightly")
        );
        assert_eq!(rustc_version("error: toolchain not installed"), None);
    }

    #[test]
    fn test_rust_probe_carries_both_install_guards_and_reads_rustc() {
        let mut inputs = RecordedInspector::default()
            .with_command(
                "rustup --version",
                RecordedInspector::succeeded("rustup 1.29.1 (d95a37b6a 2026-08-13)\n"),
            )
            .with_command(
                "rustc --version",
                RecordedInspector::succeeded("rustc 1.98.1 (48a229cea 2026-09-01)\n"),
            )
            .with_command(
                "rustc --print sysroot",
                RecordedInspector::succeeded("/toolchains/1.98-aarch64-apple-darwin\n"),
            );

        let answer = answer(&[StandardLibrary::Rust], true, &mut inputs);

        assert_eq!(selector_of(&answer, "rust"), version("1.98.1"));
        assert!(answer.degradations.is_empty());
        for probe in ["rustc --version", "rustc --print sysroot"] {
            assert!(inputs.asked.contains(&format!(
                "run {probe} in {ROOT} with RUSTUP_AUTO_INSTALL=0 with MISE_AUTO_INSTALL=0 without RUSTUP_TOOLCHAIN"
            )));
        }
        assert!(
            inputs.asked.contains(
                &"run rustup --version in / with RUSTUP_AUTO_INSTALL=0 with MISE_AUTO_INSTALL=0 without RUSTUP_TOOLCHAIN"
                    .to_owned()
            )
        );
        assert_eq!(
            answer.install_folders,
            [InstallFolder {
                package: PackageIdentity {
                    manager: "stdlib".to_owned(),
                    name: "rust".to_owned(),
                    version: "1.98.1".to_owned(),
                },
                location: InstallLocation::Path(PathBuf::from(
                    "/toolchains/1.98-aarch64-apple-darwin/lib/rustlib/src/rust/library"
                )),
            }],
            "the folder below the sysroot holds the standard library source"
        );
    }

    #[test]
    fn test_a_failed_sysroot_probe_keeps_the_version_and_records_no_folder() {
        let mut inputs = RecordedInspector::default()
            .with_command(
                "rustc --version",
                RecordedInspector::succeeded("rustc 1.98.1 (48a229cea 2026-09-01)\n"),
            )
            .with_command(
                "rustc --print sysroot",
                RecordedInspector::failed("error: unknown print request\n"),
            );

        let answer = answer(&[StandardLibrary::Rust], true, &mut inputs);

        assert_eq!(selector_of(&answer, "rust"), version("1.98.1"));
        assert!(answer.install_folders.is_empty());
        assert_eq!(
            answer.degradations,
            [(
                ResolverName::StdlibRust,
                "rustc --print sysroot: error: unknown print request; no install folder".to_owned()
            )]
        );
    }

    #[test]
    fn test_rust_probe_without_rustup_runs_the_distro_rustc() {
        let mut inputs = RecordedInspector::default()
            .with_command(
                "rustc --version",
                RecordedInspector::succeeded("rustc 1.85.0 (4d91de4e4 2025-02-17) (Debian)\n"),
            )
            .with_command(
                "rustc --print sysroot",
                RecordedInspector::succeeded("/usr\n"),
            );

        let answer = answer(&[StandardLibrary::Rust], true, &mut inputs);

        assert_eq!(selector_of(&answer, "rust"), version("1.85.0"));
        assert!(answer.degradations.is_empty());
    }

    #[test]
    fn test_rust_probe_under_old_rustup_skips_rustc_and_reads_the_toolchain_file() {
        let mut inputs = RecordedInspector::default()
            .with_file(
                format!("{ROOT}/rust-toolchain.toml"),
                "[toolchain]\nchannel = \"1.80\"\n",
            )
            .with_command(
                "rustup --version",
                RecordedInspector::succeeded("rustup 1.27.1 (54dd3d00f 2024-04-24)\n"),
            )
            .with_command(
                "rustc --version",
                RecordedInspector::succeeded("rustc 1.80.0 (x 2024-07-21)\n"),
            );

        let answer = answer(&[StandardLibrary::Rust], true, &mut inputs);

        assert_eq!(selector_of(&answer, "rust"), range("~1.80"));
        assert_eq!(answer.degradations.len(), 1);
        assert_eq!(answer.degradations[0].0, ResolverName::StdlibRust);
        assert!(answer.degradations[0].1.contains("rustup 1.27.1"));
        assert!(
            !inputs
                .asked
                .iter()
                .any(|asked| asked.starts_with("run rustc"))
        );
    }

    #[test]
    fn test_rust_probe_refused_by_an_absent_toolchain_goes_out_as_any() {
        let mut inputs = RecordedInspector::default()
            .with_command(
                "rustup --version",
                RecordedInspector::succeeded("rustup 1.29.1 (d95a37b6a 2026-08-13)\n"),
            )
            .with_command(
                "rustc --version",
                RecordedInspector::failed(
                    "error: toolchain '1.80.0-aarch64-apple-darwin' is not installed\n",
                ),
            );

        let answer = answer(&[StandardLibrary::Rust], true, &mut inputs);

        assert_eq!(selector_of(&answer, "rust"), range(">=0"));
        assert_eq!(answer.degradations[0].0.as_str(), "stdlib/rust");
        assert!(answer.degradations[0].1.contains("is not installed"));
    }

    /// A `rustc` that answers without a version line, such as a wrapper printing its own
    /// banner, degrades to the toolchain file's channel and names what it printed.
    #[test]
    fn test_a_rustc_printing_no_version_degrades_to_the_toolchain_file() {
        let mut inputs = RecordedInspector::default()
            .with_file(
                format!("{ROOT}/rust-toolchain.toml"),
                "[toolchain]\nchannel = \"1.80\"\n",
            )
            .with_command(
                "rustc --version",
                RecordedInspector::succeeded("rustc-wrapper 0.4\n"),
            );

        let answer = answer(&[StandardLibrary::Rust], true, &mut inputs);

        assert_eq!(selector_of(&answer, "rust"), range("~1.80"));
        assert_eq!(
            answer.degradations,
            [(
                ResolverName::StdlibRust,
                "rustc --version printed no version: rustc-wrapper 0.4".to_owned()
            )]
        );
        assert!(answer.install_folders.is_empty());
    }

    #[test]
    fn test_static_resolution_runs_nothing_and_reads_the_channel() {
        let mut inputs = RecordedInspector::default()
            .with_file(format!("{ROOT}/rust-toolchain"), "nightly-2026-09-01\n");

        let answer = answer(
            &[StandardLibrary::Rust, StandardLibrary::Node],
            false,
            &mut inputs,
        );

        assert_eq!(selector_of(&answer, "rust"), range(">=0"));
        assert_eq!(selector_of(&answer, "node"), range(">=0"));
        assert!(answer.degradations.is_empty());
        assert!(!inputs.asked.iter().any(|asked| asked.starts_with("run ")));
    }

    #[test]
    fn test_node_pin_spellings_sort() {
        let cases: [(&str, Option<Pin>); 10] = [
            ("v10.12.0", Some(exact("10.12.0"))),
            ("12.22.11\n", Some(exact("12.22.11"))),
            ("22.3.0\n", Some(exact("22.3.0"))),
            ("v22\n", Some(requirement("22"))),
            ("v20.11\n", Some(requirement("20.11"))),
            ("lts/*\n", Some(named("lts/*"))),
            ("lts/iron # the LTS line\n", Some(named("lts/iron"))),
            ("# pinned for CI\n\nnode\n", Some(named("node"))),
            ("  v24.5.0  \r\n", Some(exact("24.5.0"))),
            ("\n\n", None),
        ];
        for (text, expected) in cases {
            assert_eq!(node_pin_line(text), expected, "{text:?}");
        }
    }

    #[test]
    fn test_tool_versions_mise_and_volta_name_node() {
        assert_eq!(
            tool_versions_node("python 3.12.3\nnodejs 22.3.0 20.1.0\n"),
            Some(exact("22.3.0"))
        );
        assert_eq!(
            tool_versions_node("# tools\nnode lts\n"),
            Some(named("lts"))
        );
        assert_eq!(tool_versions_node("ruby 3.3.0\n"), None);
        assert_eq!(
            mise_toml_node("[tools]\nnode = \"22\"\n"),
            Some(requirement("22"))
        );
        assert_eq!(
            mise_toml_node("[tools]\nnode = [\"22.3.0\", \"20\"]\npython = \"3.12\"\n"),
            Some(exact("22.3.0"))
        );
        assert_eq!(
            mise_toml_node("[tools]\nnode = { version = \"lts\" }\n"),
            Some(named("lts"))
        );
        assert_eq!(mise_toml_node("[env]\nNODE_ENV = \"dev\"\n"), None);
        assert_eq!(
            mise_toml_node("[tools]\nnode = 22\n"),
            None,
            "a bare number is no version word mise reads"
        );
        assert_eq!(
            package_volta_node(r#"{"volta": {"node": "22.3.0", "npm": "10.8.1"}}"#),
            Some(exact("22.3.0"))
        );
        assert_eq!(
            package_engines_node(r#"{"engines": {"node": ">=20.9.0"}}"#).as_deref(),
            Some(">=20.9.0")
        );
    }

    #[test]
    fn test_node_pin_goes_first_and_no_probe_runs() {
        let mut inputs = RecordedInspector::default()
            .with_file(format!("{ROOT}/.nvmrc"), "v22\n")
            .with_command("node --version", RecordedInspector::succeeded("v24.5.0\n"));

        let answer = answer(&[StandardLibrary::Node], true, &mut inputs);

        assert_eq!(selector_of(&answer, "node"), range("22"));
        assert_eq!(selector_of(&answer, "typescript"), range(">=0"));
        assert!(!inputs.asked.iter().any(|asked| asked.starts_with("run ")));
    }

    #[test]
    fn test_node_probe_runs_without_a_pin_and_degrades_to_engines() {
        let mut probed = RecordedInspector::default()
            .with_command("node --version", RecordedInspector::succeeded("v24.5.0\n"));
        let answer_probed = answer(&[StandardLibrary::Node], true, &mut probed);
        assert_eq!(selector_of(&answer_probed, "node"), version("24.5.0"));

        let mut absent = RecordedInspector::default().with_file(
            format!("{ROOT}/package.json"),
            r#"{"engines": {"node": ">=20"}}"#,
        );
        let answer_absent = answer(&[StandardLibrary::Node], true, &mut absent);
        assert_eq!(selector_of(&answer_absent, "node"), range(">=20"));
        assert_eq!(answer_absent.degradations[0].0.as_str(), "stdlib/node");
    }

    /// `.tool-versions` and `mise.toml` pin Node.js when no earlier file does, and no
    /// probe runs.
    #[test]
    fn test_tool_versions_and_mise_pins_name_the_node_entry() {
        let mut tool_versions = RecordedInspector::default()
            .with_file(format!("{ROOT}/.tool-versions"), "nodejs 22.3.0\n");
        let answer_tool_versions = answer(&[StandardLibrary::Node], true, &mut tool_versions);
        assert_eq!(
            selector_of(&answer_tool_versions, "node"),
            version("22.3.0")
        );

        let mut mise = RecordedInspector::default()
            .with_file(format!("{ROOT}/mise.toml"), "[tools]\nnode = \"22\"\n");
        let answer_mise = answer(&[StandardLibrary::Node], true, &mut mise);
        assert_eq!(selector_of(&answer_mise, "node"), range("22"));

        for asked in [&tool_versions.asked, &mise.asked] {
            assert!(!asked.iter().any(|asked| asked.starts_with("run ")));
        }
    }

    /// A `node` that exits nonzero, or answers without a whole version, degrades to
    /// `engines.node` and names what it printed.
    #[test]
    fn test_a_node_probe_without_a_version_degrades_naming_its_output() {
        let engines = r#"{"engines": {"node": ">=20"}}"#;
        let mut failed = RecordedInspector::default()
            .with_file(format!("{ROOT}/package.json"), engines)
            .with_command(
                "node --version",
                RecordedInspector::failed("node: bad option\n"),
            );
        let answer_failed = answer(&[StandardLibrary::Node], true, &mut failed);
        assert_eq!(selector_of(&answer_failed, "node"), range(">=20"));
        assert_eq!(
            answer_failed.degradations,
            [(
                ResolverName::StdlibNode,
                "node --version: node: bad option".to_owned()
            )]
        );

        let mut partial = RecordedInspector::default()
            .with_file(format!("{ROOT}/package.json"), engines)
            .with_command("node --version", RecordedInspector::succeeded("v22\n"));
        let answer_partial = answer(&[StandardLibrary::Node], true, &mut partial);
        assert_eq!(selector_of(&answer_partial, "node"), range(">=20"));
        assert_eq!(
            answer_partial.degradations,
            [(
                ResolverName::StdlibNode,
                "node --version printed no version: v22".to_owned()
            )]
        );
    }

    #[test]
    fn test_python_reads_both_pyvenv_spellings_then_the_pins() {
        let uv = "home = /x/bin\nimplementation = CPython\nuv = 0.9.5\nversion_info = 3.12.3\n";
        let venv = "home = /opt/bin\ninclude-system-site-packages = false\nversion = 3.14.7\n";
        assert_eq!(pyvenv_version(uv), Some(exact("3.12.3")));
        assert_eq!(pyvenv_version(venv), Some(exact("3.14.7")));
        assert_eq!(pyvenv_version("home = /x\n"), None);
        assert_eq!(python_pin_line("3.12\n"), Some(requirement("==3.12.*")));
        assert_eq!(python_pin_line("# pin\n3.13.1\n"), Some(exact("3.13.1")));
        assert_eq!(python_pin_line("pypy@3.10\n"), Some(named("pypy@3.10")));
        assert_eq!(python_pin_line("3.14t\n"), Some(named("3.14t")));
        assert_eq!(
            requires_python("[project]\nname = \"x\"\nrequires-python = \">=3.10\"\n"),
            Some(requirement(">=3.10"))
        );
        assert_eq!(
            requires_python("[project]\nrequires-python = '>=3.13'\n"),
            Some(requirement(">=3.13"))
        );
        assert_eq!(
            requires_python("[project]\nrequires-python = \">=3.10.6\"  # floor\n"),
            Some(requirement(">=3.10.6"))
        );
        assert_eq!(
            requires_python("[project]\nrequires-python = \">=3.11,<3.12\"\n"),
            Some(requirement(">=3.11,<3.12"))
        );

        let mut inputs = RecordedInspector::default()
            .with_file(format!("{ROOT}/.python-version"), "3.12\n")
            .with_file(format!("{ROOT}/.venv/pyvenv.cfg"), venv);
        let answer = answer(&[StandardLibrary::Python], true, &mut inputs);
        assert_eq!(selector_of(&answer, "python"), version("3.14.7"));
        assert!(!inputs.asked.iter().any(|asked| asked.starts_with("run ")));
    }

    #[test]
    fn test_language_identities_map_to_their_library() {
        assert_eq!(
            StandardLibrary::for_language("rust"),
            Some(StandardLibrary::Rust)
        );
        assert_eq!(
            StandardLibrary::for_language("typescript:tsx"),
            Some(StandardLibrary::Node)
        );
        assert_eq!(StandardLibrary::for_language("markdown"), None);
    }

    /// Each library's degradations carry `stdlib/<name>`, the entry name its entries go
    /// out under.
    #[test]
    fn test_each_library_names_its_degradations_after_its_entry() {
        for library in [
            StandardLibrary::Rust,
            StandardLibrary::Node,
            StandardLibrary::Python,
        ] {
            assert_eq!(
                library.resolver().as_str(),
                format!("stdlib/{}", library.name()),
                "{library:?}"
            );
        }
    }
}
