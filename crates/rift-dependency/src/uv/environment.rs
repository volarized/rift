//! The project environment: where `uv sync` installed the distributions, and its interpreter.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use rift_core::line::{lines_inclusive, without_ending};

use super::{NORMALIZED_SEPARATOR, python_language};
use crate::catalog::{CatalogEntry, PackageLocation, STDLIB_MANAGER, package_identity};
use crate::resolver::{DIRECTORY_ENTRIES_MAX, FileObservation, Inspector};

/// The environment variable naming the project environment directory: uv's own override.
const PROJECT_ENVIRONMENT_VARIABLE: &str = "UV_PROJECT_ENVIRONMENT";
/// The project environment directory beside the manifest when uv is not told otherwise.
const DEFAULT_ENVIRONMENT_DIRECTORY_NAME: &str = ".venv";
/// The file at an environment's root naming the interpreter it was created from.
const ENVIRONMENT_FILE_NAME: &str = "pyvenv.cfg";
/// Bytes one environment metadata file may hold before it is refused.
///
/// The bound covers `pyvenv.cfg`, `top_level.txt`, and `RECORD` alike.
const ENVIRONMENT_FILE_BYTES_MAX: u64 = 64 << 10;
/// The `pyvenv.cfg` separator between a key and its value.
const CONFIGURATION_SEPARATOR: char = '=';
/// The `pyvenv.cfg` key naming the interpreter's `bin` directory.
const ENVIRONMENT_HOME_KEY: &str = "home";
/// The `pyvenv.cfg` key naming the interpreter's version.
const ENVIRONMENT_VERSION_KEY: &str = "version_info";
/// The separator between the segments of a version.
const VERSION_SEGMENT_SEPARATOR: char = '.';
/// The directory holding one `python<X.Y>` directory: the POSIX layout.
///
/// It stands below an environment and below an interpreter's prefix alike.
const LIBRARY_DIRECTORY_NAME: &str = "lib";
/// The directory below an environment holding `site-packages` directly: the Windows layout.
const WINDOWS_LIBRARY_DIRECTORY_NAME: &str = "Lib";
/// The prefix of the per-version directory below `lib`.
///
/// `python3.14`, or `python3.14t` for a free-threaded build.
const PYTHON_DIRECTORY_PREFIX: &str = "python";
/// The directory holding installed distributions.
const SITE_PACKAGES_DIRECTORY_NAME: &str = "site-packages";
/// The suffix of an installed distribution's metadata directory.
const DIST_INFO_SUFFIX: &str = ".dist-info";
/// The metadata file listing a distribution's top-level import names.
const TOP_LEVEL_FILE_NAME: &str = "top_level.txt";
/// The metadata file listing every path the distribution installed.
const RECORD_FILE_NAME: &str = "RECORD";
/// The `RECORD` separator between a path and the hash and size after it.
const RECORD_FIELD_SEPARATOR: char = ',';
/// The separator between the segments of a `RECORD` path, whatever the platform.
const RECORD_PATH_SEPARATOR: char = '/';
/// The segment opening a `RECORD` path installed outside site-packages, such as a script.
const PARENT_DIRECTORY_SEGMENT: &str = "..";
/// The directory holding compiled bytecode beside the modules it was compiled from.
const BYTECODE_DIRECTORY_NAME: &str = "__pycache__";
/// The extension of a single-file module.
const MODULE_FILE_EXTENSION: &str = ".py";
/// The separator a distribution name takes in a metadata directory name and an import name.
const MODULE_SEPARATOR: &str = "_";
/// The standard library's package name under `STDLIB_MANAGER`.
const STDLIB_PACKAGE_NAME: &str = "python";

/// The project environment directory: `UV_PROJECT_ENVIRONMENT`, else `.venv` beside the manifest.
///
/// A relative override resolves against the manifest directory, which is the workspace
/// root the lockfile stands in; an absolute one stands alone. An empty value is unset.
pub(super) fn environment_directory(
    manifest_directory: &Path,
    inspector: &mut dyn Inspector,
) -> PathBuf {
    let directory = inspector
        .environment(PROJECT_ENVIRONMENT_VARIABLE)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_ENVIRONMENT_DIRECTORY_NAME.to_owned());
    manifest_directory.join(directory)
}

/// What the project environment beside a manifest could not answer.
#[derive(Debug)]
pub(super) enum EnvironmentFailure {
    /// No `pyvenv.cfg` stands below the environment directory; carries that directory.
    Absent { directory: PathBuf },
    /// `pyvenv.cfg` holds more bytes than `ENVIRONMENT_FILE_BYTES_MAX`.
    OverBound { directory: PathBuf, bytes: u64 },
    /// Neither layout holds a `site-packages` directory below the environment.
    MissingSitePackages { directory: PathBuf },
    /// `pyvenv.cfg` names no `version_info`, so the interpreter has no identity.
    MissingVersion { directory: PathBuf },
}

impl fmt::Display for EnvironmentFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent { directory } => {
                write!(formatter, "no environment at {}", directory.display())
            }
            Self::OverBound { directory, bytes } => write!(
                formatter,
                "{ENVIRONMENT_FILE_NAME} at {} holds {bytes} bytes, past the \
                 {ENVIRONMENT_FILE_BYTES_MAX} byte bound",
                directory.display()
            ),
            Self::MissingSitePackages { directory } => write!(
                formatter,
                "no {SITE_PACKAGES_DIRECTORY_NAME} below {}",
                directory.display()
            ),
            Self::MissingVersion { directory } => write!(
                formatter,
                "{ENVIRONMENT_FILE_NAME} at {} names no {ENVIRONMENT_VERSION_KEY}",
                directory.display()
            ),
        }
    }
}

/// What `pyvenv.cfg` states about the interpreter the environment was created from.
#[derive(Debug, Default)]
struct InterpreterFacts {
    /// The interpreter's `bin` directory, from `home`.
    home: Option<PathBuf>,
    /// The interpreter's version, from `version_info`.
    version: Option<String>,
}

impl InterpreterFacts {
    /// Reads the two keys from `pyvenv.cfg` text; every other line is ignored.
    fn parse(text: &str) -> Self {
        let mut facts = Self::default();
        for line in lines_inclusive(text) {
            match configuration_pair(without_ending(line)) {
                Some((ENVIRONMENT_HOME_KEY, value)) => facts.home = Some(PathBuf::from(value)),
                Some((ENVIRONMENT_VERSION_KEY, value)) => facts.version = Some(value.to_owned()),
                _ => {}
            }
        }
        facts
    }
}

/// The `key = value` pair one `pyvenv.cfg` line states, absent for any other line.
fn configuration_pair(line: &str) -> Option<(&str, &str)> {
    line.split_once(CONFIGURATION_SEPARATOR)
        .map(|(key, value)| (key.trim(), value.trim()))
}

/// The project environment: its interpreter facts and the layout it installs into.
#[derive(Debug)]
pub(super) struct ProjectEnvironment {
    directory: PathBuf,
    interpreter: InterpreterFacts,
    /// The `python<X.Y>` directory below `lib`, absent in the Windows layout.
    python_directory: Option<String>,
}

impl ProjectEnvironment {
    /// Reads `pyvenv.cfg` below `directory`, then looks for the `python<X.Y>` directory below `lib`.
    pub(super) fn observe(
        directory: PathBuf,
        inspector: &mut dyn Inspector,
    ) -> Result<Self, EnvironmentFailure> {
        let path = directory.join(ENVIRONMENT_FILE_NAME);
        let bytes = match inspector.read_file(&path, ENVIRONMENT_FILE_BYTES_MAX) {
            FileObservation::Absent => return Err(EnvironmentFailure::Absent { directory }),
            FileObservation::OverBound { bytes } => {
                return Err(EnvironmentFailure::OverBound { directory, bytes });
            }
            FileObservation::Bytes(bytes) => bytes,
        };
        let interpreter = InterpreterFacts::parse(&String::from_utf8_lossy(&bytes));
        let library = directory.join(LIBRARY_DIRECTORY_NAME);
        let python_directory = inspector
            .list_directory(&library, DIRECTORY_ENTRIES_MAX)
            .into_iter()
            .find(|entry| entry.starts_with(PYTHON_DIRECTORY_PREFIX));
        Ok(Self {
            directory,
            interpreter,
            python_directory,
        })
    }

    /// The `site-packages` listing: below `lib/python<X.Y>`, else below `Lib` when that stands.
    pub(super) fn site_packages(
        &self,
        inspector: &mut dyn Inspector,
    ) -> Result<SitePackages, EnvironmentFailure> {
        let directory = self.site_packages_directory(inspector).ok_or_else(|| {
            EnvironmentFailure::MissingSitePackages {
                directory: self.directory.clone(),
            }
        })?;
        Ok(SitePackages::observe(directory, inspector))
    }

    /// The `site-packages` directory: below `lib/python<X.Y>`, else below `Lib` when that stands.
    fn site_packages_directory(&self, inspector: &mut dyn Inspector) -> Option<PathBuf> {
        if let Some(python_directory) = &self.python_directory {
            return Some(
                self.directory
                    .join(LIBRARY_DIRECTORY_NAME)
                    .join(python_directory)
                    .join(SITE_PACKAGES_DIRECTORY_NAME),
            );
        }
        let windows = self
            .directory
            .join(WINDOWS_LIBRARY_DIRECTORY_NAME)
            .join(SITE_PACKAGES_DIRECTORY_NAME);
        inspector.directory_exists(&windows).then_some(windows)
    }

    /// The standard library entry, rooted at the interpreter's library when it stands.
    ///
    /// The identity takes `version_info`; without it there is no entry. The library is
    /// `lib/<python directory>` below the parent of `home`, named after the environment's
    /// own `python<X.Y>` directory since a free-threaded interpreter keeps its library
    /// under `python3.14t`; without that directory, the version's `python<X.Y>` stands in.
    pub(super) fn stdlib_entry(
        &self,
        inspector: &mut dyn Inspector,
    ) -> Result<CatalogEntry, EnvironmentFailure> {
        let version = self.interpreter.version.as_deref().ok_or_else(|| {
            EnvironmentFailure::MissingVersion {
                directory: self.directory.clone(),
            }
        })?;
        let identity = package_identity(STDLIB_MANAGER, STDLIB_PACKAGE_NAME, version);
        let mut entry = CatalogEntry::new(identity, PackageLocation::Stdlib, python_language());
        if let Some(library) = self.stdlib_directory(version)
            && inspector.directory_exists(&library)
        {
            entry = entry.with_source_root(library);
        }
        Ok(entry)
    }

    /// The directory holding the interpreter's standard library, absent without `home`.
    fn stdlib_directory(&self, version: &str) -> Option<PathBuf> {
        let prefix = self.interpreter.home.as_ref()?.parent()?;
        let python_directory = match &self.python_directory {
            Some(python_directory) => python_directory.clone(),
            None => format!("{PYTHON_DIRECTORY_PREFIX}{}", minor_version(version)?),
        };
        Some(prefix.join(LIBRARY_DIRECTORY_NAME).join(python_directory))
    }
}

/// The `X.Y` prefix of a version, absent without two dotted segments.
fn minor_version(version: &str) -> Option<String> {
    let (major, rest) = version.split_once(VERSION_SEGMENT_SEPARATOR)?;
    let minor = rest
        .split_once(VERSION_SEGMENT_SEPARATOR)
        .map_or(rest, |(minor, _)| minor);
    Some(format!("{major}{VERSION_SEGMENT_SEPARATOR}{minor}"))
}

/// The `site-packages` directory and its listing, read once per lockfile root.
#[derive(Debug)]
pub(super) struct SitePackages {
    directory: PathBuf,
    /// Every entry name, for exact membership.
    names: BTreeSet<String>,
    /// Every entry name by its ASCII-lowercase form, for the metadata directory match.
    by_lowercase: BTreeMap<String, String>,
}

impl SitePackages {
    /// Lists `directory` once, at most `DIRECTORY_ENTRIES_MAX` entries.
    fn observe(directory: PathBuf, inspector: &mut dyn Inspector) -> Self {
        let entries = inspector.list_directory(&directory, DIRECTORY_ENTRIES_MAX);
        let by_lowercase = entries
            .iter()
            .map(|entry| (entry.to_ascii_lowercase(), entry.clone()))
            .collect();
        Self {
            directory,
            names: entries.into_iter().collect(),
            by_lowercase,
        }
    }

    /// The source root of one distribution: its import name's directory or single-file module.
    ///
    /// The distribution is found by its metadata directory, matched without regard to
    /// case since a wheel keeps the project's own spelling (`PyYAML-6.0.3.dist-info`).
    /// The import name is the first `top_level.txt` lists, else the first `RECORD` names,
    /// else the normalized name with `_`. A single-file distribution has no directory:
    /// its root is the `<import name>.py` file itself. A distribution with no metadata
    /// directory is not installed here and has no root. Each found distribution costs at
    /// most two file reads and one probe.
    pub(super) fn source_root(
        &self,
        inspector: &mut dyn Inspector,
        name: &str,
        version: &str,
    ) -> Option<PathBuf> {
        let dist_info = self
            .by_lowercase
            .get(&dist_info_name(name, version).to_ascii_lowercase())?;
        let import_name = self
            .listed_import_name(inspector, dist_info)
            .or_else(|| self.recorded_import_name(inspector, dist_info))
            .unwrap_or_else(|| module_name(name));
        self.module_root(inspector, &import_name)
    }

    /// The first import name `top_level.txt` lists; absent when the file is absent, refused, or empty.
    fn listed_import_name(&self, inspector: &mut dyn Inspector, dist_info: &str) -> Option<String> {
        let text = self.metadata_text(inspector, dist_info, TOP_LEVEL_FILE_NAME)?;
        lines_inclusive(&text)
            .map(without_ending)
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned)
    }

    /// The first import name `RECORD` names; absent when the file names none.
    ///
    /// `RECORD` lists one installed path per line, relative to site-packages, before the
    /// hash and size. The first path that names a module decides; `recorded_module`
    /// states which paths do.
    fn recorded_import_name(
        &self,
        inspector: &mut dyn Inspector,
        dist_info: &str,
    ) -> Option<String> {
        let text = self.metadata_text(inspector, dist_info, RECORD_FILE_NAME)?;
        lines_inclusive(&text)
            .map(without_ending)
            .find_map(|line| {
                let path = line.split(RECORD_FIELD_SEPARATOR).next().unwrap_or(line);
                recorded_module(path, dist_info)
            })
            .map(str::to_owned)
    }

    /// One metadata file's text, absent when the file is absent or refused.
    fn metadata_text(
        &self,
        inspector: &mut dyn Inspector,
        dist_info: &str,
        file_name: &str,
    ) -> Option<String> {
        let path = self.directory.join(dist_info).join(file_name);
        match inspector.read_file(&path, ENVIRONMENT_FILE_BYTES_MAX) {
            FileObservation::Bytes(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            FileObservation::Absent | FileObservation::OverBound { .. } => None,
        }
    }

    /// The listed directory named `import_name`, else the listed `<import_name>.py` file.
    fn module_root(&self, inspector: &mut dyn Inspector, import_name: &str) -> Option<PathBuf> {
        let package = self.directory.join(import_name);
        if self.names.contains(import_name) && inspector.directory_exists(&package) {
            return Some(package);
        }
        let module = format!("{import_name}{MODULE_FILE_EXTENSION}");
        self.names
            .contains(&module)
            .then(|| self.directory.join(module))
    }
}

/// The import name one `RECORD` path names, absent for a path naming no module.
///
/// A path under `..` is a script installed outside site-packages, one under
/// `__pycache__` is bytecode, and one under the metadata directory is the metadata
/// itself; none names the import. A path with a directory names that directory, and a
/// bare `<name>.py` names a single-file module; any other bare file names nothing.
fn recorded_module<'a>(path: &'a str, dist_info: &str) -> Option<&'a str> {
    let outside = path.starts_with(PARENT_DIRECTORY_SEGMENT);
    let metadata = path.starts_with(dist_info);
    if outside || metadata {
        return None;
    }
    match path.split_once(RECORD_PATH_SEPARATOR) {
        Some((BYTECODE_DIRECTORY_NAME | "", _)) => None,
        Some((package, _)) => Some(package),
        None => path
            .strip_suffix(MODULE_FILE_EXTENSION)
            .filter(|stem| !stem.is_empty()),
    }
}

/// The import name a normalized distribution name derives: every `-` becomes `_`.
fn module_name(normalized: &str) -> String {
    normalized.replace(NORMALIZED_SEPARATOR, MODULE_SEPARATOR)
}

/// The metadata directory name of a distribution, escaped as a wheel spells it.
///
/// `markdown-it-py` 4.2.0 installs `markdown_it_py-4.2.0.dist-info`.
fn dist_info_name(normalized: &str, version: &str) -> String {
    format!("{}-{version}{DIST_INFO_SUFFIX}", module_name(normalized))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::super::fixture::{
        ENVIRONMENT, ENVIRONMENT_FILE, INTERPRETER_PREFIX, ROOT, SITE_PACKAGES, STDLIB_DIRECTORY,
        WORKSPACE_LOCKFILE, entry, environment_inspector, project, resolve,
        single_package_lockfile, with_environment,
    };
    use super::*;
    use crate::fixture::RecordedInspector;

    /// A `foo-bar` distribution with neither `top_level.txt` nor `RECORD`, installed as `foo_bar`.
    fn underscore_inspector() -> RecordedInspector {
        RecordedInspector::default()
            .with_file(
                format!("{ROOT}/uv.lock"),
                single_package_lockfile("foo-bar", "1.0.0"),
            )
            .with_file(format!("{ENVIRONMENT}/pyvenv.cfg"), ENVIRONMENT_FILE)
            .with_directory(format!("{SITE_PACKAGES}/foo_bar-1.0.0.dist-info"))
            .with_directory(format!("{SITE_PACKAGES}/foo_bar"))
    }

    #[test]
    fn test_resolve_dist_info_with_top_level_roots_at_the_listed_import_name() {
        let mut inspector = environment_inspector();

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "pytest").source_root(),
            Some(Path::new(
                "/workspace/.venv/lib/python3.14t/site-packages/_pytest"
            )),
            "the first listed name wins"
        );
        assert!(inspector.asked.contains(&format!(
            "read {SITE_PACKAGES}/pytest-9.1.1.dist-info/top_level.txt"
        )));
        assert!(
            !inspector.asked.contains(&format!(
                "read {SITE_PACKAGES}/pytest-9.1.1.dist-info/RECORD"
            )),
            "a listed name costs no RECORD read"
        );
        assert!(
            inspector
                .asked
                .contains(&format!("exists {SITE_PACKAGES}/_pytest"))
        );
        assert_eq!(
            entry(&resolution, "typer").source_root(),
            Some(Path::new(
                "/workspace/.venv/lib/python3.14t/site-packages/typer"
            ))
        );
    }

    #[test]
    fn test_resolve_dist_info_without_top_level_derives_the_import_name() {
        let mut inspector = environment_inspector();

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "mdurl").source_root(),
            Some(Path::new(
                "/workspace/.venv/lib/python3.14t/site-packages/mdurl"
            ))
        );
        assert!(inspector.asked.contains(&format!(
            "read {SITE_PACKAGES}/mdurl-0.1.2.dist-info/top_level.txt"
        )));
        assert_eq!(
            entry(&resolution, "markdown-it-py").source_root(),
            None,
            "without RECORD, the derived name markdown_it_py is not listed"
        );
    }

    #[test]
    fn test_resolve_dist_info_record_names_the_package_directory() {
        let mut inspector = RecordedInspector::default()
            .with_file(
                format!("{ROOT}/uv.lock"),
                single_package_lockfile("markdown-it-py", "4.2.0"),
            )
            .with_file(format!("{ENVIRONMENT}/pyvenv.cfg"), ENVIRONMENT_FILE)
            .with_file(
                format!("{SITE_PACKAGES}/markdown_it_py-4.2.0.dist-info/RECORD"),
                "../../../bin/markdown-it,sha256=RzfUHQZhl2KU5XSsKfxs6yfGd2kgOD7KkTbloKj-d9w,351\n\
                 markdown_it/__init__.py,sha256=rb0zsebNRqT8YvcnYnsQpV60bzn37bdR7omj9fooQ5U,114\n\
                 markdown_it/_compat.py,sha256=U4S_2y3zgLZVfMenHRaJFBW8yqh2mUBuI291LGQVOJ8,35\n\
                 markdown_it_py-4.2.0.dist-info/RECORD,,\n",
            )
            .with_directory(format!("{SITE_PACKAGES}/markdown_it"));

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "markdown-it-py").source_root(),
            Some(Path::new(
                "/workspace/.venv/lib/python3.14t/site-packages/markdown_it"
            )),
            "the script under .. is skipped and the package directory names the import"
        );
        assert!(inspector.asked.contains(&format!(
            "read {SITE_PACKAGES}/markdown_it_py-4.2.0.dist-info/top_level.txt"
        )));
        assert!(inspector.asked.contains(&format!(
            "read {SITE_PACKAGES}/markdown_it_py-4.2.0.dist-info/RECORD"
        )));
    }

    #[test]
    fn test_resolve_dist_info_record_names_the_single_file_module() {
        let mut inspector = RecordedInspector::default()
            .with_file(
                format!("{ROOT}/uv.lock"),
                single_package_lockfile("six", "1.17.0"),
            )
            .with_file(format!("{ENVIRONMENT}/pyvenv.cfg"), ENVIRONMENT_FILE)
            .with_file(
                format!("{SITE_PACKAGES}/six-1.17.0.dist-info/RECORD"),
                "__pycache__/six.cpython-312.pyc,,\n\
                 six-1.17.0.dist-info/INSTALLER,sha256=zuuue4knoyJ-UwPPXg8fezS7VCrXJQrAP7zeNuwvFQg,4\n\
                 six.py,sha256=xRyR9wPT1LNpbJI8tf7CE-BeddkhU5O--sfy-mo5BN8,34703\n",
            )
            .with_file(format!("{SITE_PACKAGES}/six.py"), "");

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "six").source_root(),
            Some(Path::new(
                "/workspace/.venv/lib/python3.14t/site-packages/six.py"
            )),
            "bytecode and the metadata directory are skipped; the module file names the import"
        );
        assert!(
            !inspector
                .asked
                .iter()
                .any(|line| line.starts_with(&format!("exists {SITE_PACKAGES}/"))),
            "neither six nor __pycache__ is probed as a package directory: {:?}",
            inspector.asked
        );
    }

    #[test]
    fn test_resolve_dist_info_record_absent_falls_back_to_the_underscore_spelling() {
        let mut inspector = underscore_inspector();

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "foo-bar").source_root(),
            Some(Path::new(
                "/workspace/.venv/lib/python3.14t/site-packages/foo_bar"
            ))
        );
        assert!(inspector.asked.contains(&format!(
            "read {SITE_PACKAGES}/foo_bar-1.0.0.dist-info/RECORD"
        )));
    }

    #[test]
    fn test_resolve_dist_info_record_over_bound_falls_back_to_the_underscore_spelling() {
        let oversized =
            vec![b'x'; usize::try_from(ENVIRONMENT_FILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector = underscore_inspector().with_file(
            format!("{SITE_PACKAGES}/foo_bar-1.0.0.dist-info/RECORD"),
            oversized,
        );

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "foo-bar").source_root(),
            Some(Path::new(
                "/workspace/.venv/lib/python3.14t/site-packages/foo_bar"
            ))
        );
        assert!(resolution.degradations.is_empty());
    }

    #[test]
    fn test_resolve_single_file_distribution_roots_at_the_module_file() {
        let mut inspector = environment_inspector();

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "py").source_root(),
            Some(Path::new(
                "/workspace/.venv/lib/python3.14t/site-packages/py.py"
            ))
        );
        assert!(
            !inspector
                .asked
                .contains(&format!("exists {SITE_PACKAGES}/py")),
            "an unlisted directory is never probed"
        );
    }

    #[test]
    fn test_resolve_dist_info_matches_case_insensitively() {
        let mut inspector = RecordedInspector::default()
            .with_file(
                format!("{ROOT}/uv.lock"),
                single_package_lockfile("pyyaml", "6.0.3"),
            )
            .with_file(format!("{ENVIRONMENT}/pyvenv.cfg"), ENVIRONMENT_FILE)
            .with_file(
                format!("{SITE_PACKAGES}/PyYAML-6.0.3.dist-info/top_level.txt"),
                "yaml\n_yaml\n",
            )
            .with_directory(format!("{SITE_PACKAGES}/yaml"));

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "pyyaml").source_root(),
            Some(Path::new(
                "/workspace/.venv/lib/python3.14t/site-packages/yaml"
            ))
        );
    }

    #[test]
    fn test_resolve_package_without_dist_info_has_no_root() {
        let mut inspector = environment_inspector();

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "colorama").source_root(),
            None,
            "a listed package directory without metadata is not this distribution"
        );
        assert!(
            !inspector.asked.iter().any(|line| line.contains("colorama")),
            "an uninstalled distribution costs no read and no probe"
        );
    }

    #[test]
    fn test_resolve_environment_absent_degrades_without_roots() {
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/uv.lock"), WORKSPACE_LOCKFILE);

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            [
                "pyproject.toml: no environment at /workspace/.venv; packages cataloged without \
                 source roots"
            ]
        );
        assert_eq!(resolution.entries.len(), 7, "no standard library entry");
        assert!(
            resolution
                .entries
                .iter()
                .all(|entry| entry.source_root().is_none())
        );
        assert!(entry(&resolution, "typer").is_direct());
        assert!(!inspector.asked.iter().any(|line| line.starts_with("list ")));
    }

    #[test]
    fn test_resolve_environment_file_over_bound_degrades_without_roots() {
        let oversized =
            vec![b'#'; usize::try_from(ENVIRONMENT_FILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/uv.lock"), WORKSPACE_LOCKFILE)
            .with_file(format!("{ENVIRONMENT}/pyvenv.cfg"), oversized);

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            [format!(
                "pyproject.toml: pyvenv.cfg at /workspace/.venv holds {} bytes, past the \
                 {ENVIRONMENT_FILE_BYTES_MAX} byte bound; packages cataloged without source roots",
                ENVIRONMENT_FILE_BYTES_MAX + 1
            )]
        );
        assert_eq!(resolution.entries.len(), 7);
        assert!(
            resolution
                .entries
                .iter()
                .all(|entry| entry.source_root().is_none())
        );
    }

    #[test]
    fn test_resolve_project_environment_variable_absolute_overrides_venv() {
        let inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/uv.lock"), WORKSPACE_LOCKFILE)
            .with_environment("UV_PROJECT_ENVIRONMENT", "/environments/rift");
        let mut inspector = with_environment(inspector, "/environments/rift");

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "typer").source_root(),
            Some(Path::new(
                "/environments/rift/lib/python3.14t/site-packages/typer"
            ))
        );
        assert!(resolution.degradations.is_empty());
        assert!(
            inspector
                .asked
                .contains(&"environment UV_PROJECT_ENVIRONMENT".to_owned())
        );
        assert!(
            !inspector
                .asked
                .contains(&format!("read {ENVIRONMENT}/pyvenv.cfg")),
            "the override replaces .venv outright"
        );
    }

    #[test]
    fn test_resolve_project_environment_variable_relative_resolves_against_manifest_directory() {
        let inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/tools/uv.lock"), WORKSPACE_LOCKFILE)
            .with_environment("UV_PROJECT_ENVIRONMENT", "envs/rift");
        let mut inspector = with_environment(inspector, "/workspace/tools/envs/rift");

        let resolution = resolve(&["tools/pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "typer").source_root(),
            Some(Path::new(
                "/workspace/tools/envs/rift/lib/python3.14t/site-packages/typer"
            ))
        );
        assert!(resolution.degradations.is_empty());
        assert_eq!(
            resolution.inputs,
            [project("tools/pyproject.toml"), project("tools/uv.lock")]
        );
    }

    #[test]
    fn test_resolve_windows_layout_probes_lib_site_packages() {
        let site_packages = format!("{ENVIRONMENT}/Lib/site-packages");
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/uv.lock"), WORKSPACE_LOCKFILE)
            .with_file(format!("{ENVIRONMENT}/pyvenv.cfg"), ENVIRONMENT_FILE)
            .with_file(
                format!("{site_packages}/typer-0.27.1.dist-info/top_level.txt"),
                "typer\n",
            )
            .with_directory(format!("{site_packages}/typer"));

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "typer").source_root(),
            Some(Path::new("/workspace/.venv/Lib/site-packages/typer"))
        );
        assert!(resolution.degradations.is_empty());
        assert!(inspector.asked.contains(&format!("list {ENVIRONMENT}/lib")));
        assert!(inspector.asked.contains(&format!("exists {site_packages}")));
        assert_eq!(
            entry(&resolution, "python").source_root(),
            None,
            "without a python directory the version's python3.14 is probed and is absent"
        );
        assert!(
            inspector
                .asked
                .contains(&format!("exists {INTERPRETER_PREFIX}/lib/python3.14"))
        );
    }

    #[test]
    fn test_resolve_site_packages_absent_degrades_without_roots() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/uv.lock"), WORKSPACE_LOCKFILE)
            .with_file(format!("{ENVIRONMENT}/pyvenv.cfg"), ENVIRONMENT_FILE)
            .with_directory(format!("{INTERPRETER_PREFIX}/lib/python3.14"));

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            [
                "pyproject.toml: no site-packages below /workspace/.venv; packages cataloged \
                 without source roots"
            ]
        );
        assert_eq!(resolution.entries.len(), 8);
        assert!(
            resolution
                .entries
                .iter()
                .filter(|entry| entry.location() == PackageLocation::Dependency)
                .all(|entry| entry.source_root().is_none())
        );
        assert_eq!(
            entry(&resolution, "python").source_root(),
            Some(Path::new("/toolchain/cpython-3.14.0/lib/python3.14")),
            "the version's python<X.Y> stands in for the environment's python directory"
        );
    }

    #[test]
    fn test_resolve_stdlib_present_catalogs_library_root() {
        let mut inspector = environment_inspector();

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        let stdlib = entry(&resolution, "python");
        assert_eq!(stdlib.identity().manager, "stdlib");
        assert_eq!(stdlib.identity().version, "3.14.0");
        assert_eq!(stdlib.location(), PackageLocation::Stdlib);
        assert_eq!(
            stdlib.source_root(),
            Some(Path::new(STDLIB_DIRECTORY)),
            "the environment's python3.14t names the interpreter's library directory"
        );
        assert!(!stdlib.is_direct());
        assert_eq!(stdlib.language().identity_segment(), "python");
    }

    #[test]
    fn test_resolve_stdlib_library_absent_catalogs_without_root() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/uv.lock"), WORKSPACE_LOCKFILE)
            .with_file(format!("{ENVIRONMENT}/pyvenv.cfg"), ENVIRONMENT_FILE)
            .with_directory(SITE_PACKAGES);

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        let stdlib = entry(&resolution, "python");
        assert_eq!(stdlib.identity().version, "3.14.0");
        assert_eq!(stdlib.source_root(), None);
        assert!(
            inspector
                .asked
                .contains(&format!("exists {STDLIB_DIRECTORY}"))
        );
    }

    #[test]
    fn test_resolve_environment_without_home_catalogs_stdlib_without_root() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/uv.lock"), WORKSPACE_LOCKFILE)
            .with_file(
                format!("{ENVIRONMENT}/pyvenv.cfg"),
                "version_info = 3.14.0\n",
            )
            .with_directory(SITE_PACKAGES);

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(entry(&resolution, "python").source_root(), None);
        assert!(
            !inspector
                .asked
                .iter()
                .any(|line| line.starts_with("exists /toolchain")),
            "no home, no library to probe"
        );
    }

    #[test]
    fn test_resolve_environment_without_version_degrades_without_stdlib_entry() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/uv.lock"), WORKSPACE_LOCKFILE)
            .with_file(
                format!("{ENVIRONMENT}/pyvenv.cfg"),
                "home = /toolchain/cpython-3.14.0/bin\n",
            )
            .with_directory(SITE_PACKAGES);

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            [
                "pyproject.toml: pyvenv.cfg at /workspace/.venv names no version_info; no \
                 standard library entry"
            ]
        );
        assert_eq!(resolution.entries.len(), 7);
        assert!(
            resolution
                .entries
                .iter()
                .all(|entry| entry.location() == PackageLocation::Dependency)
        );
    }

    #[test]
    fn test_recorded_module_names_packages_and_bare_modules_only() {
        let dist_info = "six-1.17.0.dist-info";
        assert_eq!(recorded_module("../../../bin/six", dist_info), None);
        assert_eq!(
            recorded_module("__pycache__/six.cpython-312.pyc", dist_info),
            None
        );
        assert_eq!(
            recorded_module("six-1.17.0.dist-info/RECORD", dist_info),
            None
        );
        assert_eq!(recorded_module("six.py", dist_info), Some("six"));
        assert_eq!(recorded_module("pkg/__init__.py", dist_info), Some("pkg"));
        assert_eq!(
            recorded_module("_editable_impl_six.pth", dist_info),
            None,
            "a bare file that is not a module names nothing"
        );
        assert_eq!(recorded_module("/absolute", dist_info), None);
        assert_eq!(recorded_module(".py", dist_info), None);
        assert_eq!(recorded_module("", dist_info), None);
    }

    #[test]
    fn test_dist_info_name_escapes_the_normalized_name() {
        assert_eq!(
            dist_info_name("markdown-it-py", "4.2.0"),
            "markdown_it_py-4.2.0.dist-info"
        );
        assert_eq!(dist_info_name("pytest", "9.1.1"), "pytest-9.1.1.dist-info");
    }

    #[test]
    fn test_minor_version_keeps_two_segments() {
        assert_eq!(minor_version("3.14.0").as_deref(), Some("3.14"));
        assert_eq!(minor_version("3.14").as_deref(), Some("3.14"));
        assert_eq!(minor_version("3"), None);
    }

    #[test]
    fn test_environment_directory_treats_an_empty_override_as_unset() {
        let mut inspector =
            RecordedInspector::default().with_environment("UV_PROJECT_ENVIRONMENT", "");

        let directory = environment_directory(Path::new(ROOT), &mut inspector);

        assert_eq!(directory, PathBuf::from("/workspace/.venv"));
    }
}
