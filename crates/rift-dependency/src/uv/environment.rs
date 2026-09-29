//! The project environment: the import folders `uv sync` installed each distribution into.
//!
//! A distribution's `.dist-info/RECORD` lists every path it installed below
//! `site-packages`, so the import folders come from there. `top_level.txt` is not read:
//! most wheels built by current tools ship none, and where it stands it can disagree with
//! what `RECORD` installed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rift_core::line::{lines_inclusive, without_ending};

use super::NORMALIZED_SEPARATOR;
use crate::resolver::{DIRECTORY_ENTRIES_MAX, FileObservation, StaticInputs};

/// The project environment directory uv creates beside the lockfile's manifest when
/// `UV_PROJECT_ENVIRONMENT` does not name another, which a static pass cannot read.
pub const PROJECT_ENVIRONMENT_DIRECTORY: &str = ".venv";
/// The file `uv venv` and `python3 -m venv` write at a project environment's root, naming
/// the interpreter the environment was created from.
pub const PROJECT_ENVIRONMENT_MARKER: &str = "pyvenv.cfg";
/// The directory holding one `python<X.Y>` directory: the POSIX layout.
const LIBRARY_DIRECTORY_NAME: &str = "lib";
/// The directory below an environment holding `site-packages` directly: the Windows layout.
const WINDOWS_LIBRARY_DIRECTORY_NAME: &str = "Lib";
/// The prefix of the per-version directory below `lib`: `python3.14`, or `python3.14t`
/// for a free-threaded build.
const PYTHON_DIRECTORY_PREFIX: &str = "python";
/// The directory holding installed distributions.
const SITE_PACKAGES_DIRECTORY_NAME: &str = "site-packages";
/// The suffix of an installed distribution's metadata directory.
const DIST_INFO_SUFFIX: &str = ".dist-info";
/// The metadata file listing every path the distribution installed.
const RECORD_FILE_NAME: &str = "RECORD";
/// Bytes one `RECORD` may hold before it is left unread. A large distribution lists tens
/// of thousands of files, each on one line of a few hundred bytes at most.
const RECORD_BYTES_MAX: u64 = 4 << 20;
/// The quote opening a `RECORD` path that holds a comma, as CSV writes it.
const RECORD_QUOTE: char = '"';
/// The `RECORD` separator between a path and the hash and size after it.
const RECORD_FIELD_SEPARATOR: char = ',';
/// The separator between the segments of a `RECORD` path, whatever the platform.
const RECORD_PATH_SEPARATOR: char = '/';
/// The segment opening a `RECORD` path installed outside site-packages, such as a script.
const PARENT_DIRECTORY_SEGMENT: &str = "..";
/// The directory holding compiled bytecode beside the modules it was compiled from.
const BYTECODE_DIRECTORY_NAME: &str = "__pycache__";
/// The extensions of a Python source module and its stub.
const MODULE_EXTENSIONS: [&str; 2] = [".py", ".pyi"];
/// The files that make a directory a regular package, source or stub.
const PACKAGE_INIT_FILES: [&str; 2] = ["__init__.py", "__init__.pyi"];
/// The separator a distribution name takes in a metadata directory name.
const MODULE_SEPARATOR: &str = "_";

/// One environment's `site-packages` directory and its listing, read once per lockfile.
#[derive(Debug)]
pub(super) struct SitePackages {
    directory: PathBuf,
    /// Every entry name by its ASCII-lowercase form, for the metadata directory match.
    by_lowercase: BTreeMap<String, String>,
}

impl SitePackages {
    /// The `site-packages` of the environment beside `directory`, the folder holding the
    /// lockfile: below `lib/python<X.Y>`, else below `Lib`. Absent when neither stands.
    pub(super) fn observe(directory: &Path, inputs: &mut dyn StaticInputs) -> Option<Self> {
        let environment = directory.join(PROJECT_ENVIRONMENT_DIRECTORY);
        let library = environment.join(LIBRARY_DIRECTORY_NAME);
        let posix = inputs
            .list_directory(&library, DIRECTORY_ENTRIES_MAX)
            .into_iter()
            .find(|entry| entry.starts_with(PYTHON_DIRECTORY_PREFIX))
            .map(|python| library.join(python).join(SITE_PACKAGES_DIRECTORY_NAME));
        let windows = || {
            let library = environment.join(WINDOWS_LIBRARY_DIRECTORY_NAME);
            inputs
                .list_directory(&library, DIRECTORY_ENTRIES_MAX)
                .iter()
                .any(|entry| entry == SITE_PACKAGES_DIRECTORY_NAME)
                .then(|| library.join(SITE_PACKAGES_DIRECTORY_NAME))
        };
        let directory = posix.or_else(windows)?;
        let by_lowercase = inputs
            .list_directory(&directory, DIRECTORY_ENTRIES_MAX)
            .into_iter()
            .map(|entry| (entry.to_ascii_lowercase(), entry))
            .collect();
        Some(Self {
            directory,
            by_lowercase,
        })
    }

    /// The `site-packages` folder this listing read.
    pub(super) fn directory(&self) -> &Path {
        &self.directory
    }

    /// The import folders and single-file modules the distribution `normalized` at
    /// `version` installed, relative to [`Self::directory`] with forward slashes, in path
    /// order. Empty when its metadata directory is not listed or its `RECORD` is absent,
    /// over its bound, or names no module.
    ///
    /// The metadata directory matches without regard to case, since a wheel keeps the
    /// project's own spelling (`PyYAML-6.0.3.dist-info`). The work is one read of at most
    /// `RECORD_BYTES_MAX` bytes and one pass over its lines.
    pub(super) fn import_roots(
        &self,
        normalized: &str,
        version: &str,
        inputs: &mut dyn StaticInputs,
    ) -> BTreeSet<String> {
        let wanted = dist_info_name(normalized, version).to_ascii_lowercase();
        let Some(dist_info) = self.by_lowercase.get(&wanted) else {
            return BTreeSet::new();
        };
        let record = self.directory.join(dist_info).join(RECORD_FILE_NAME);
        let text = match inputs.read_file(&record, RECORD_BYTES_MAX) {
            FileObservation::Bytes(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            FileObservation::Absent | FileObservation::OverBound { .. } => return BTreeSet::new(),
        };
        record_import_roots(&text, dist_info)
    }
}

/// The import roots `RECORD` lists, relative to `site-packages`, in path order.
///
/// Every listed `.py` or `.pyi` module decides one root: the shallowest folder above it
/// that the listing makes a regular package with an `__init__` file, else the module
/// itself when it stands at the top, else its top folder. A namespace portion such as
/// `google/protobuf` therefore roots below the namespace `google`, which another
/// distribution may share. Scripts under `..`, bytecode, and the metadata directory name
/// no module.
fn record_import_roots(record: &str, dist_info: &str) -> BTreeSet<String> {
    let modules: Vec<&str> = lines_inclusive(record)
        .map(without_ending)
        .filter_map(record_path)
        .filter(|path| is_module(path, dist_info))
        .collect();
    let packages: BTreeSet<&str> = modules
        .iter()
        .filter_map(|path| {
            let (folder, file) = path.rsplit_once(RECORD_PATH_SEPARATOR)?;
            PACKAGE_INIT_FILES.contains(&file).then_some(folder)
        })
        .collect();
    modules
        .iter()
        .map(|path| import_root(path, &packages).to_owned())
        .collect()
}

/// The root one module path decides, as [`record_import_roots`] states.
fn import_root<'a>(path: &'a str, packages: &BTreeSet<&str>) -> &'a str {
    let folders = path
        .match_indices(RECORD_PATH_SEPARATOR)
        .map(|(index, _)| &path[..index]);
    let mut top = None;
    for folder in folders {
        if packages.contains(folder) {
            return folder;
        }
        top.get_or_insert(folder);
    }
    top.unwrap_or(path)
}

/// Whether one `RECORD` path is a module installed below `site-packages`, outside the
/// metadata directory and bytecode.
fn is_module(path: &str, dist_info: &str) -> bool {
    let outside = path.starts_with(PARENT_DIRECTORY_SEGMENT);
    let metadata = path
        .strip_prefix(dist_info)
        .is_some_and(|rest| rest.starts_with(RECORD_PATH_SEPARATOR));
    let bytecode = path
        .split(RECORD_PATH_SEPARATOR)
        .any(|segment| segment == BYTECODE_DIRECTORY_NAME);
    let module = MODULE_EXTENSIONS
        .iter()
        .any(|extension| path.ends_with(extension));
    module && !outside && !metadata && !bytecode
}

/// The path field of one `RECORD` line: the text before the first comma, or a quoted
/// field up to its closing quote, as CSV writes a path holding a comma. Absent for a
/// blank line.
fn record_path(line: &str) -> Option<&str> {
    let path = match line.strip_prefix(RECORD_QUOTE) {
        Some(quoted) => quoted.split(RECORD_QUOTE).next()?,
        None => line.split(RECORD_FIELD_SEPARATOR).next()?,
    };
    (!path.is_empty()).then_some(path)
}

/// The metadata directory name of a distribution, escaped as a wheel spells it.
///
/// `markdown-it-py` 4.2.0 installs `markdown_it_py-4.2.0.dist-info`.
fn dist_info_name(normalized: &str, version: &str) -> String {
    let escaped = normalized.replace(NORMALIZED_SEPARATOR, MODULE_SEPARATOR);
    format!("{escaped}-{version}{DIST_INFO_SUFFIX}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::RecordedInspector;

    const ROOT: &str = "/workspace";
    const SITE_PACKAGES: &str = "/workspace/.venv/lib/python3.14t/site-packages";

    fn roots(record: &str, dist_info: &str) -> Vec<String> {
        record_import_roots(record, dist_info).into_iter().collect()
    }

    #[test]
    fn test_record_names_the_package_folder_and_skips_scripts_and_metadata() {
        let record = "../../../bin/markdown-it,sha256=RzfUHQZhl2KU5XSsKfxs6yfGd2kgOD7KkTbloKj-d9w,351\n\
                      markdown_it/__init__.py,sha256=rb0zsebNRqT8YvcnYnsQpV60bzn37bdR7omj9fooQ5U,114\n\
                      markdown_it/rules_core/block.py,sha256=U4S_2y3zgLZVfMenHRaJFBW8yqh2mUBuI291LGQVOJ8,35\n\
                      markdown_it/__pycache__/_compat.cpython-314.pyc,,\n\
                      markdown_it_py-4.2.0.dist-info/RECORD,,\n\
                      markdown_it_py-4.2.0.dist-info/entry_points.py,,\n";
        assert_eq!(
            roots(record, "markdown_it_py-4.2.0.dist-info"),
            ["markdown_it"]
        );
    }

    #[test]
    fn test_record_names_a_single_file_module_and_every_top_level_package() {
        let six = "__pycache__/six.cpython-312.pyc,,\n\
                   six-1.17.0.dist-info/INSTALLER,sha256=zuuue4knoyJ-UwPPXg8fezS7VCrXJQrAP7zeNuwvFQg,4\n\
                   six.py,sha256=xRyR9wPT1LNpbJI8tf7CE-BeddkhU5O--sfy-mo5BN8,34703\n";
        assert_eq!(roots(six, "six-1.17.0.dist-info"), ["six.py"]);
        let pyyaml = "_yaml/__init__.py,,\nyaml/__init__.py,,\nyaml/composer.py,,\n\
                      yaml/_yaml.cpython-312-darwin.so,,\nPyYAML-6.0.3.dist-info/RECORD,,\n";
        assert_eq!(
            roots(pyyaml, "PyYAML-6.0.3.dist-info"),
            ["_yaml", "yaml"],
            "a distribution installing two import names roots both"
        );
    }

    #[test]
    fn test_a_namespace_portion_roots_below_the_shared_namespace() {
        let protobuf = "google/protobuf/__init__.py,,\ngoogle/protobuf/descriptor.py,,\n\
                        google/protobuf/internal/api.py,,\ngoogle/_upb/_message.abi3.so,,\n";
        assert_eq!(
            roots(protobuf, "protobuf-6.33.0.dist-info"),
            ["google/protobuf"]
        );
        let bare = "vendored/plain.py,,\nstubs.pyi,,\n";
        assert_eq!(
            roots(bare, "bare-1.0.0.dist-info"),
            ["stubs.pyi", "vendored"],
            "a folder with no `__init__` roots at its top, and a stub is a module"
        );
    }

    #[test]
    fn test_a_quoted_record_path_reads_whole() {
        assert_eq!(
            record_path("\"odd,name.py\",sha256=x,1"),
            Some("odd,name.py")
        );
        assert_eq!(record_path("plain.py,sha256=x,1"), Some("plain.py"));
        assert_eq!(record_path(""), None);
    }

    #[test]
    fn test_import_roots_match_the_metadata_directory_without_regard_to_case() {
        let mut inputs = RecordedInspector::default()
            .with_directory(format!("{SITE_PACKAGES}/PyJWT-2.10.1.dist-info"))
            .with_file(
                format!("{SITE_PACKAGES}/PyJWT-2.10.1.dist-info/RECORD"),
                "jwt/__init__.py,,\njwt/api_jwt.py,,\nPyJWT-2.10.1.dist-info/RECORD,,\n",
            )
            .with_directory(format!("{SITE_PACKAGES}/jwt"));

        let site_packages =
            SitePackages::observe(Path::new(ROOT), &mut inputs).expect("the POSIX layout");

        assert_eq!(site_packages.directory(), Path::new(SITE_PACKAGES));
        assert_eq!(
            Vec::from_iter(site_packages.import_roots("pyjwt", "2.10.1", &mut inputs)),
            ["jwt"],
            "PyJWT installs as `jwt`, a name the distribution name does not spell"
        );
        assert!(
            site_packages
                .import_roots("pyjwt", "2.9.0", &mut inputs)
                .is_empty(),
            "another version is not installed here"
        );
    }

    #[test]
    fn test_the_windows_layout_and_an_absent_environment() {
        let windows = format!("{ROOT}/.venv/Lib/site-packages");
        let mut inputs = RecordedInspector::default().with_file(
            format!("{windows}/six-1.17.0.dist-info/RECORD"),
            "six.py,,\n",
        );
        let site_packages =
            SitePackages::observe(Path::new(ROOT), &mut inputs).expect("the Windows layout");
        assert_eq!(site_packages.directory(), Path::new(&windows));
        assert_eq!(
            Vec::from_iter(site_packages.import_roots("six", "1.17.0", &mut inputs)),
            ["six.py"]
        );

        let mut empty = RecordedInspector::default().with_directory(ROOT);
        assert!(SitePackages::observe(Path::new(ROOT), &mut empty).is_none());
    }

    #[test]
    fn test_an_oversized_record_locates_nothing() {
        let oversized = "x.py,,\n".repeat(usize::try_from(RECORD_BYTES_MAX).expect("fits") / 7 + 1);
        let mut inputs = RecordedInspector::default().with_file(
            format!("{SITE_PACKAGES}/big-1.0.0.dist-info/RECORD"),
            oversized,
        );
        let site_packages =
            SitePackages::observe(Path::new(ROOT), &mut inputs).expect("the POSIX layout");
        assert!(
            site_packages
                .import_roots("big", "1.0.0", &mut inputs)
                .is_empty()
        );
    }
}
