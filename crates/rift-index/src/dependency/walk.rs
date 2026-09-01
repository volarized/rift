//! The walk that selects the files spelling one cataloged package's API.
//!
//! `PackageLanguage` holds each language's file selection and its
//! public-declaration rule; the package index consults the rule through
//! `public_qualified_names`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use ignore::{DirEntry, Walk, WalkBuilder};
use rift_core::{LoopBudget, ProjectPath, bounded_for};
use rift_dependency::CatalogEntry;
use rift_protocol::read::{Language, PackageIdentity};
use rift_syntax::{ShippedLanguage, SyntaxDocument, SyntaxSymbol};

use super::failure::{PackageIndexError, PackageIndexFault, PackageIndexViolation};
use super::{
    DIRECTORY_DEPTH_MAX_FIELD, DependencyIndexLimits, PACKAGE_BYTES_MAX_FIELD,
    PACKAGE_FILES_MAX_FIELD, WALK_ENTRIES_MAX_FIELD,
};
use crate::workspace::{TextSourceFile, relative_path};

/// Directory names a package walk never descends into.
const SKIPPED_DIRECTORY_NAMES: &[&str] = &[
    "tests",
    "benches",
    "examples",
    "target",
    "node_modules",
    "__pycache__",
    ".git",
];
/// Suffix a skipped directory name ends in once its surrounding underscores are trimmed.
/// `integration_tests` and `__tests__` both match.
const TESTS_DIRECTORY_SUFFIX: &str = "tests";
/// Rust source file suffix.
const RUST_SOURCE_SUFFIX: &str = ".rs";
/// Python module suffix, selected when the package ships no stub.
const PYTHON_MODULE_SUFFIX: &str = ".py";
/// Python stub suffix, preferred over modules when the package ships any.
const PYTHON_STUB_SUFFIX: &str = ".pyi";
/// TypeScript declaration-file suffix, the only TypeScript files selected.
const TYPESCRIPT_DECLARATION_SUFFIX: &str = ".d.ts";

/// The Rust provider's spelling of bare `pub`.
const RUST_PUBLIC_VISIBILITY: &str = "pub";
/// The Rust provider's spelling of a declaration without a visibility modifier.
const RUST_PRIVATE_VISIBILITY: &str = "private";
/// The Rust provider's kind word for a trait declaration.
const RUST_TRAIT_KIND: &str = "trait";
/// The prefix a Python name carries when the module keeps it private.
const PYTHON_PRIVATE_PREFIX: char = '_';
/// The TypeScript accessibility modifiers that hide a member from callers.
const TYPESCRIPT_HIDDEN_VISIBILITIES: &[&str] = &["private", "protected"];

/// A `usize` count as the `u64` a limit refusal reports.
fn count_u64(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// The languages the dependency index reads an API from, and each one's rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackageLanguage {
    /// Every `.rs` file; `pub` declarations and the items of a `pub` trait are public.
    Rust,
    /// Every `.pyi` stub when the package ships any, else every `.py` module;
    /// names without a leading underscore are public.
    Python,
    /// Every `.d.ts` declaration file; every member but `private` and `protected`
    /// ones is public.
    TypeScript,
}

impl PackageLanguage {
    /// The rules for `language`; `None` when no shipped definition serves it.
    fn for_language(language: &Language) -> Option<Self> {
        let shipped = rift_syntax::definitions()
            .iter()
            .map(|definition| definition.shipped())
            .find(|shipped| &shipped.language() == language)?;
        match shipped {
            ShippedLanguage::Rust => Some(Self::Rust),
            ShippedLanguage::Python => Some(Self::Python),
            ShippedLanguage::TypeScript | ShippedLanguage::TypeScriptTsx => Some(Self::TypeScript),
            ShippedLanguage::JavaScript
            | ShippedLanguage::Markdown
            | ShippedLanguage::Json
            | ShippedLanguage::Yaml
            | ShippedLanguage::Toml => None,
        }
    }

    /// Whether `file_name` is selected before the Python stub preference applies.
    fn is_candidate(self, file_name: &str) -> bool {
        match self {
            Self::Rust => file_name.ends_with(RUST_SOURCE_SUFFIX),
            Self::Python => {
                file_name.ends_with(PYTHON_MODULE_SUFFIX) || file_name.ends_with(PYTHON_STUB_SUFFIX)
            }
            Self::TypeScript => file_name.ends_with(TYPESCRIPT_DECLARATION_SUFFIX),
        }
    }

    /// The candidates that spell the API: Python keeps stubs alone when it ships any.
    fn api_files(self, candidates: Vec<Candidate>) -> Vec<Candidate> {
        if self != Self::Python {
            return candidates;
        }
        let ships_stubs = candidates.iter().any(Candidate::is_python_stub);
        candidates
            .into_iter()
            .filter(|candidate| candidate.is_python_stub() == ships_stubs)
            .collect()
    }

    /// Whether a caller outside the package can use `symbol`.
    ///
    /// `by_name` indexes the document's declarations by qualified name, so a
    /// Rust trait item can consult its container. Module reachability is not
    /// modeled: a `pub` item inside a private module counts as public.
    fn is_public(self, symbol: &SyntaxSymbol, by_name: &BTreeMap<&str, &SyntaxSymbol>) -> bool {
        match self {
            Self::Rust => is_public_rust(symbol, by_name),
            Self::Python => !symbol.name.starts_with(PYTHON_PRIVATE_PREFIX),
            Self::TypeScript => !symbol
                .visibility
                .as_deref()
                .is_some_and(|visibility| TYPESCRIPT_HIDDEN_VISIBILITIES.contains(&visibility)),
        }
    }
}

/// `pub` declarations, and the modifier-free items of a `pub` trait.
fn is_public_rust(symbol: &SyntaxSymbol, by_name: &BTreeMap<&str, &SyntaxSymbol>) -> bool {
    match symbol.visibility.as_deref() {
        Some(RUST_PUBLIC_VISIBILITY) => true,
        Some(RUST_PRIVATE_VISIBILITY) => symbol
            .container
            .as_deref()
            .and_then(|container| by_name.get(container))
            .is_some_and(|container| {
                container.kind == RUST_TRAIT_KIND
                    && container.visibility.as_deref() == Some(RUST_PUBLIC_VISIBILITY)
            }),
        _ => false,
    }
}

/// The qualified names of every declaration in `document` a caller can use.
///
/// A language outside [`PackageLanguage`] states no visibility this index reads,
/// so every declaration counts as public.
pub(super) fn public_qualified_names(
    language: &Language,
    document: &SyntaxDocument,
) -> BTreeSet<String> {
    let symbols = document.symbols();
    let Some(rules) = PackageLanguage::for_language(language) else {
        return symbols
            .iter()
            .map(|symbol| symbol.qualified_name.clone())
            .collect();
    };
    let by_name: BTreeMap<&str, &SyntaxSymbol> = symbols
        .iter()
        .map(|symbol| (symbol.qualified_name.as_str(), symbol))
        .collect();
    symbols
        .iter()
        .filter(|symbol| rules.is_public(symbol, &by_name))
        .map(|symbol| symbol.qualified_name.clone())
        .collect()
}

/// The files one cataloged package's API is read from: package-relative, UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageFiles {
    files: Vec<TextSourceFile>,
    skipped_binary: usize,
}

impl PackageFiles {
    /// The selected files, with the count of files skipped as binary.
    pub(crate) fn new(files: Vec<TextSourceFile>, skipped_binary: usize) -> Self {
        Self {
            files,
            skipped_binary,
        }
    }

    /// Every selected file, in walk order.
    #[must_use]
    pub fn files(&self) -> &[TextSourceFile] {
        &self.files
    }

    /// How many files were selected.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// The bytes every selected file holds together.
    #[must_use]
    pub fn byte_count(&self) -> u64 {
        self.files
            .iter()
            .map(|file| count_u64(file.content().len()))
            .fold(0, u64::saturating_add)
    }

    /// How many selected files held NUL bytes or invalid UTF-8 and were skipped.
    #[must_use]
    pub const fn skipped_binary(&self) -> usize {
        self.skipped_binary
    }
}

/// One selected file before it is read: where it sits and its package-relative path.
#[derive(Debug)]
struct Candidate {
    absolute: PathBuf,
    relative: ProjectPath,
}

impl Candidate {
    fn is_python_stub(&self) -> bool {
        self.relative.as_str().ends_with(PYTHON_STUB_SUFFIX)
    }
}

/// Reads the files that spell one cataloged package's API.
///
/// The source root is a directory walked in file-name order, or one file that
/// is the whole package. Directories named in `SKIPPED_DIRECTORY_NAMES`, or
/// ending in `tests`, and every symlink are left out. Files are selected by the
/// entry's language (`PackageLanguage`); a selected file holding NUL bytes or
/// invalid UTF-8 is skipped and counted. The walk stops at `directory_depth_max`
/// and `walk_entries_max`, and the package refuses past `package_files_max` and
/// `package_bytes_max`; every refusal names the package.
///
/// # Errors
///
/// Returns [`PackageIndexError`] when the entry has no source root or no
/// supported language, when the root or a file cannot be read, when a path
/// is not a valid project path, or when a bound is crossed.
pub fn package_files(
    entry: &CatalogEntry,
    limits: &DependencyIndexLimits,
) -> Result<PackageFiles, PackageIndexError> {
    let package = entry.identity();
    let language = PackageLanguage::for_language(entry.language()).ok_or_else(|| {
        PackageIndexFault::new(PackageIndexViolation::LanguageUnsupported, package)
    })?;
    let root = entry
        .source_root()
        .ok_or_else(|| PackageIndexFault::new(PackageIndexViolation::SourceRootMissing, package))?;
    let candidates = language.api_files(candidates_below(root, language, package, limits)?);
    if candidates.len() > limits.package_files_max {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::PackageFilesExceeded, package)
                .at(root)
                .breached(
                    PACKAGE_FILES_MAX_FIELD,
                    count_u64(limits.package_files_max),
                    count_u64(candidates.len()),
                )
                .into(),
        );
    }
    read_candidates(candidates, package, limits)
}

/// Every candidate file below `root`, or the root itself when it is one file.
fn candidates_below(
    root: &Path,
    language: PackageLanguage,
    package: &PackageIdentity,
    limits: &DependencyIndexLimits,
) -> Result<Vec<Candidate>, PackageIndexError> {
    let metadata = fs::metadata(root).map_err(|error| {
        PackageIndexFault::new(PackageIndexViolation::Unreadable, package)
            .at(root)
            .caused_by(error)
    })?;
    if metadata.is_file() {
        return Ok(single_file_candidate(root, language, package)?
            .into_iter()
            .collect());
    }
    let mut candidates = Vec::new();
    let walked = bounded_for!(
        entry in package_walk(root, limits.directory_depth_max),
        budget = LoopBudget::new(limits.walk_entries_max),
        {
            let entry = entry.map_err(|error| {
                PackageIndexFault::new(PackageIndexViolation::Unreadable, package)
                    .at(root)
                    .caused_by(error)
            })?;
            if let Some(candidate) = candidate_of(&entry, root, language, package, limits)? {
                candidates.push(candidate);
            }
        }
    );
    walked.map_err(|exhausted| {
        PackageIndexFault::new(PackageIndexViolation::WalkEntriesExceeded, package)
            .at(root)
            .breached(
                WALK_ENTRIES_MAX_FIELD,
                count_u64(limits.walk_entries_max),
                count_u64(exhausted.limit().saturating_add(1)),
            )
    })?;
    Ok(candidates)
}

/// A single-file source root as its own candidate, when its name is selected.
fn single_file_candidate(
    root: &Path,
    language: PackageLanguage,
    package: &PackageIdentity,
) -> Result<Option<Candidate>, PackageIndexError> {
    let Some(name) = root.file_name().and_then(OsStr::to_str) else {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::InvalidPath, package)
                .at(root)
                .into(),
        );
    };
    if !language.is_candidate(name) {
        return Ok(None);
    }
    let relative = ProjectPath::new(name).map_err(|error| {
        PackageIndexFault::new(PackageIndexViolation::InvalidPath, package)
            .at(root)
            .caused_by(error)
    })?;
    Ok(Some(Candidate {
        absolute: root.to_path_buf(),
        relative,
    }))
}

/// One walked entry as a candidate: a selected file, or nothing for anything else.
fn candidate_of(
    entry: &DirEntry,
    root: &Path,
    language: PackageLanguage,
    package: &PackageIdentity,
    limits: &DependencyIndexLimits,
) -> Result<Option<Candidate>, PackageIndexError> {
    let Some(file_type) = entry.file_type() else {
        return Ok(None);
    };
    if file_type.is_dir() {
        return directory_within_depth(entry, package, limits).map(|()| None);
    }
    if !file_type.is_file() {
        return Ok(None);
    }
    let Some(name) = entry.file_name().to_str() else {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::InvalidPath, package)
                .at(entry.path())
                .into(),
        );
    };
    if !language.is_candidate(name) {
        return Ok(None);
    }
    let relative = package_relative(root, entry.path(), package)?;
    Ok(Some(Candidate {
        absolute: entry.path().to_path_buf(),
        relative,
    }))
}

/// Refuses a directory the walk reached past `directory_depth_max`.
fn directory_within_depth(
    entry: &DirEntry,
    package: &PackageIdentity,
    limits: &DependencyIndexLimits,
) -> Result<(), PackageIndexError> {
    if entry.depth() > limits.directory_depth_max {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::DirectoryDepthExceeded, package)
                .at(entry.path())
                .breached(
                    DIRECTORY_DEPTH_MAX_FIELD,
                    count_u64(limits.directory_depth_max),
                    count_u64(entry.depth()),
                )
                .into(),
        );
    }
    Ok(())
}

/// The package-relative address of `absolute`, which the walk found below `root`.
fn package_relative(
    root: &Path,
    absolute: &Path,
    package: &PackageIdentity,
) -> Result<ProjectPath, PackageIndexError> {
    let relative = absolute.strip_prefix(root).map_err(|error| {
        PackageIndexFault::new(PackageIndexViolation::InvalidPath, package)
            .at(absolute)
            .caused_by(error)
    })?;
    relative_path(relative).map_err(|error| {
        PackageIndexFault::new(PackageIndexViolation::InvalidPath, package)
            .at(absolute)
            .caused_by(error)
            .into()
    })
}

/// Reads every candidate under `package_bytes_max`, skipping binary content.
///
/// The loop is bounded by the caller's `package_files_max` refusal.
fn read_candidates(
    candidates: Vec<Candidate>,
    package: &PackageIdentity,
    limits: &DependencyIndexLimits,
) -> Result<PackageFiles, PackageIndexError> {
    let mut files = Vec::with_capacity(candidates.len());
    let mut skipped_binary = 0_usize;
    let mut byte_count = 0_u64;
    for candidate in candidates {
        let remaining = limits.package_bytes_max.saturating_sub(byte_count);
        let bytes = read_within(&candidate.absolute, remaining, package)?;
        byte_count = byte_count.saturating_add(count_u64(bytes.len()));
        if byte_count > limits.package_bytes_max {
            return Err(PackageIndexFault::new(
                PackageIndexViolation::PackageBytesExceeded,
                package,
            )
            .at(&candidate.absolute)
            .breached(
                PACKAGE_BYTES_MAX_FIELD,
                limits.package_bytes_max,
                byte_count,
            )
            .into());
        }
        match text_content(bytes) {
            Some(content) => files.push(TextSourceFile::from_content(candidate.relative, content)),
            None => skipped_binary += 1,
        }
    }
    Ok(PackageFiles::new(files, skipped_binary))
}

/// Reads at most `remaining + 1` bytes: an oversized file is counted, never held whole.
fn read_within(
    path: &Path,
    remaining: u64,
    package: &PackageIdentity,
) -> Result<Vec<u8>, PackageIndexError> {
    let unreadable = |error: std::io::Error| {
        PackageIndexFault::new(PackageIndexViolation::Unreadable, package)
            .at(path)
            .caused_by(error)
    };
    let handle = fs::File::open(path).map_err(unreadable)?;
    let mut bytes = Vec::new();
    handle
        .take(remaining.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    Ok(bytes)
}

/// The bytes as UTF-8 text; `None` for a NUL-bearing or non-UTF-8 file, binary here.
fn text_content(bytes: Vec<u8>) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// One depth-bounded walk below a package root, in file-name order.
/// It never follows a symlink or enters a skipped directory.
fn package_walk(root: &Path, directory_depth_max: usize) -> Walk {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(directory_depth_max.saturating_add(1)))
        .sort_by_file_name(OsStr::cmp)
        .filter_entry(package_walk_includes);
    builder.build()
}

/// The root always; below it, nothing symlinked and no skipped directory name.
fn package_walk_includes(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if entry.path_is_symlink() {
        return false;
    }
    !entry
        .file_name()
        .to_str()
        .is_some_and(is_skipped_directory_name)
}

/// Whether `name` names a directory the walk leaves out.
fn is_skipped_directory_name(name: &str) -> bool {
    SKIPPED_DIRECTORY_NAMES.contains(&name)
        || name.trim_matches('_').ends_with(TESTS_DIRECTORY_SUFFIX)
}

#[cfg(test)]
mod tests {
    use rift_core::{ErrorCode, ErrorName, Fault as _};
    use rift_dependency::{CatalogEntry, PackageLocation};
    use rift_syntax::ShippedLanguage;

    use super::super::fixture::{
        identity, language, rooted, sorted_paths, tokio, violation_of, write,
    };
    use super::{DependencyIndexLimits, PackageIndexViolation, package_files};

    #[test]
    fn test_package_files_rust_selects_rs_files_and_skips_test_directories() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/lib.rs", b"pub fn spawn() {}\n");
        write(root.path(), "src/net/mod.rs", b"pub struct Socket;\n");
        write(root.path(), "src/notes.md", b"# not source\n");
        write(root.path(), "tests/it.rs", b"fn it() {}\n");
        write(root.path(), "benches/bench.rs", b"fn bench() {}\n");
        write(root.path(), "examples/demo.rs", b"fn main() {}\n");
        write(root.path(), "target/debug/build.rs", b"fn main() {}\n");
        write(root.path(), "integration_tests/flow.rs", b"fn flow() {}\n");
        write(root.path(), "__tests__/spec.rs", b"fn spec() {}\n");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["src/lib.rs", "src/net/mod.rs"]);
        assert_eq!(files.file_count(), 2);
        assert_eq!(files.skipped_binary(), 0);
        assert_eq!(files.byte_count(), 18 + 19);
    }

    #[test]
    fn test_package_files_python_prefers_stubs_when_the_package_ships_any() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "six.py", b"def public(): ...\n");
        write(root.path(), "six.pyi", b"def public() -> None: ...\n");
        write(root.path(), "pkg/__init__.py", b"");
        let entry = rooted(
            identity("uv", "six", "1.17.0"),
            ShippedLanguage::Python,
            root.path(),
        );

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["six.pyi"]);
    }

    #[test]
    fn test_package_files_python_takes_modules_when_no_stub_exists() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "pkg/__init__.py", b"");
        write(root.path(), "pkg/core.py", b"def run(): ...\n");
        write(
            root.path(),
            "pkg/__pycache__/core.cpython-313.pyc",
            b"\x00\x01",
        );
        let entry = rooted(
            identity("uv", "pkg", "0.1.0"),
            ShippedLanguage::Python,
            root.path(),
        );

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["pkg/__init__.py", "pkg/core.py"]);
    }

    #[test]
    fn test_package_files_typescript_takes_only_declaration_files() {
        let root = tempfile::tempdir().expect("package root");
        write(
            root.path(),
            "index.d.ts",
            b"export declare function connect(): void;\n",
        );
        write(root.path(), "index.js", b"module.exports = {};\n");
        write(root.path(), "index.ts", b"export function connect() {}\n");
        write(
            root.path(),
            "lib/util.d.ts",
            b"export declare const version: string;\n",
        );
        write(root.path(), "node_modules/dep/index.d.ts", b"export {};\n");
        let entry = rooted(
            identity("npm", "client", "2.0.0"),
            ShippedLanguage::TypeScript,
            root.path(),
        );

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["index.d.ts", "lib/util.d.ts"]);
    }

    #[test]
    fn test_package_files_refuses_past_package_bytes_max_naming_the_package() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/a.rs", b"0123456789");
        write(root.path(), "src/b.rs", b"0123456789");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());
        let limits = DependencyIndexLimits {
            package_bytes_max: 15,
            ..DependencyIndexLimits::default()
        };

        let error = package_files(&entry, &limits).expect_err("bytes past the bound refuse");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::PackageBytesExceeded
        );
        assert_eq!(error.fault().package(), &tokio());
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
        let evidence = error.fault().limit_evidence().expect("limit evidence");
        assert_eq!(evidence.field, "package_bytes_max");
        assert_eq!(evidence.limit, 15);
        assert_eq!(
            evidence.required, 16,
            "the second read stops one byte past the bound, so the requirement is \
             counted up to the ceiling plus one, never the whole package"
        );
    }

    #[test]
    fn test_package_files_accepts_exactly_package_bytes_max() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/a.rs", b"0123456789");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());
        let limits = DependencyIndexLimits {
            package_bytes_max: 10,
            ..DependencyIndexLimits::default()
        };

        let files = package_files(&entry, &limits).expect("exactly the bound is accepted");

        assert_eq!(files.byte_count(), 10);
    }

    #[test]
    fn test_package_files_refuses_past_package_files_max_naming_the_package() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/a.rs", b"");
        write(root.path(), "src/b.rs", b"");
        write(root.path(), "src/c.rs", b"");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());
        let limits = DependencyIndexLimits {
            package_files_max: 2,
            ..DependencyIndexLimits::default()
        };

        let error = package_files(&entry, &limits).expect_err("files past the bound refuse");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::PackageFilesExceeded
        );
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
        let evidence = error.fault().limit_evidence().expect("limit evidence");
        assert_eq!((evidence.limit, evidence.required), (2, 3));
    }

    #[test]
    fn test_package_files_refuses_a_directory_past_directory_depth_max() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/deep/lib.rs", b"");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());
        let limits = DependencyIndexLimits {
            directory_depth_max: 1,
            ..DependencyIndexLimits::default()
        };

        let error = package_files(&entry, &limits).expect_err("a deeper directory refuses");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::DirectoryDepthExceeded
        );
        assert_eq!(
            error.fault().path(),
            Some(root.path().join("src/deep").as_path())
        );
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
    }

    #[test]
    fn test_package_files_refuses_past_walk_entries_max() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/a.rs", b"");
        write(root.path(), "src/b.rs", b"");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());
        let limits = DependencyIndexLimits {
            walk_entries_max: 2,
            ..DependencyIndexLimits::default()
        };

        let error = package_files(&entry, &limits).expect_err("entries past the bound refuse");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::WalkEntriesExceeded
        );
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
        let evidence = error.fault().limit_evidence().expect("limit evidence");
        assert_eq!((evidence.limit, evidence.required), (2, 3));
    }

    #[test]
    fn test_package_files_skips_binary_and_non_utf8_files_and_counts_them() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/lib.rs", b"pub fn spawn() {}\n");
        write(root.path(), "src/latin.rs", b"// caf\xe9\n");
        write(root.path(), "src/nul.rs", b"fn a() {}\x00");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["src/lib.rs"]);
        assert_eq!(files.skipped_binary(), 2);
    }

    #[test]
    fn test_package_files_single_file_root_is_one_entry() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "six.py", b"def public(): ...\n");
        let entry = rooted(
            identity("uv", "six", "1.17.0"),
            ShippedLanguage::Python,
            &root.path().join("six.py"),
        );

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["six.py"]);
    }

    #[test]
    fn test_package_files_single_file_root_outside_the_selection_is_empty() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "notes.txt", b"nothing\n");
        let entry = rooted(
            identity("uv", "notes", "0.0.1"),
            ShippedLanguage::Python,
            &root.path().join("notes.txt"),
        );

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert!(files.files().is_empty());
    }

    #[test]
    fn test_package_files_refuses_an_entry_without_a_source_root() {
        let entry = CatalogEntry::new(
            tokio(),
            PackageLocation::Dependency,
            language(ShippedLanguage::Rust),
        );

        let error = package_files(&entry, &DependencyIndexLimits::default()).expect_err("no root");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::SourceRootMissing
        );
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::ResourceNotFound));
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
    }

    #[test]
    fn test_package_files_refuses_an_absent_source_root() {
        let root = tempfile::tempdir().expect("package root");
        let entry = rooted(tokio(), ShippedLanguage::Rust, &root.path().join("gone"));

        let error =
            package_files(&entry, &DependencyIndexLimits::default()).expect_err("absent root");

        assert_eq!(violation_of(&error), PackageIndexViolation::Unreadable);
        assert!(std::error::Error::source(&error).is_some());
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
    }

    #[test]
    fn test_package_files_refuses_a_language_without_an_api_selection() {
        let root = tempfile::tempdir().expect("package root");
        let entry = rooted(
            identity("npm", "docs", "1.0.0"),
            ShippedLanguage::Markdown,
            root.path(),
        );

        let error = package_files(&entry, &DependencyIndexLimits::default())
            .expect_err("markdown has no API selection");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::LanguageUnsupported
        );
        assert_eq!(
            error.name(),
            ErrorName::Wire(ErrorCode::CapabilityUnavailable)
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_package_files_skips_symlinked_files_and_directories() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/lib.rs", b"pub fn spawn() {}\n");
        write(root.path(), "elsewhere/other.rs", b"pub fn other() {}\n");
        std::os::unix::fs::symlink(
            root.path().join("elsewhere/other.rs"),
            root.path().join("src/link.rs"),
        )
        .expect("file symlink");
        std::os::unix::fs::symlink(
            root.path().join("elsewhere"),
            root.path().join("src/linked"),
        )
        .expect("directory symlink");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(
            sorted_paths(&files),
            ["elsewhere/other.rs", "src/lib.rs"],
            "the symlinked file and directory are left out; the real file stays"
        );
    }
}
