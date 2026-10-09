//! File selection: which files of one package a package analysis reads.
//!
//! A package ships more than its source: build output, caches, and documentation sit beside
//! it. [`PackageFileSelection`] keeps the files a [`PackageLanguage`]'s providers parse and
//! the documentation files a [`DocumentationSelection`] keeps, and drops build and cache
//! output and the files a caller's `[source] exclude` patterns name. Tests, examples, and
//! benchmarks stay: they show how a package is used.
//!
//! The selection takes package-relative paths alone, so it reads no file and holds no I/O.
//! Its patterns match through [`PathMatcher`], the matcher the workspace's own `[source]`
//! table uses, so a pattern selects the same paths in a package as in a workspace.

mod context7;
mod documentation;

use std::path::Path;

use rift_core::ProjectPath;
use rift_protocol::read::PathPattern;
use rift_syntax::ShippedLanguage;

use crate::analyzer::PackageLanguage;
use crate::glob::PathMatcher;
use rift_error::RiftError;

pub use context7::{CONTEXT7_FILE, Context7};
pub use documentation::{
    DOCUMENTATION_EXCLUDED_FILES, DOCUMENTATION_EXCLUDED_FOLDERS, DocumentationSelection,
    documentation_format,
};

/// Folder names holding build and cache output, which no package analysis reads, at any
/// depth of the package.
pub const BUILD_OUTPUT_FOLDERS: [&str; 4] = ["node_modules", "__pycache__", "target", ".git"];

/// The root a package's relative paths are matched under. Every path a package holds is
/// relative, so the matcher's root only has to be one both platforms join a relative path
/// to without a drive or a `./` prefix.
const PACKAGE_ROOT: &str = "/";

/// The rules that select one package's files: its language's source extensions, the build
/// and cache output it skips, the caller's `[source] exclude` patterns, and the
/// documentation selection.
#[derive(Debug)]
pub struct PackageFileSelection {
    source_languages: &'static [ShippedLanguage],
    exclude: PathMatcher,
    documentation: DocumentationSelection,
}

impl PackageFileSelection {
    /// Compiles the selection for one package language.
    ///
    /// The source extensions are the ones the language's shipped definitions claim: `rs`
    /// for Rust, `py` and `pyi` for Python, and every JavaScript and TypeScript extension
    /// for a TypeScript package, whose builds ship `.js`, `.mjs`, and `.cjs` beside their
    /// declaration files. Source extensions match without regard to ASCII case;
    /// selected paths and source bytes keep their original spelling.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when an `exclude` pattern is not a valid glob.
    pub fn new(
        language: PackageLanguage,
        exclude: &[PathPattern],
        documentation: DocumentationSelection,
    ) -> Result<Self, RiftError> {
        let exclude = exclude
            .iter()
            .map(|pattern| pattern.0.clone())
            .collect::<Vec<_>>();
        Ok(Self {
            source_languages: language.source_languages(),
            exclude: PathMatcher::build(Path::new(PACKAGE_ROOT), &[], &exclude)?,
            documentation,
        })
    }

    /// The source and documentation files `paths` hold, each list in path order.
    ///
    /// A path under a [`BUILD_OUTPUT_FOLDERS`] folder, or one an `exclude` pattern matches,
    /// is never selected. Of the rest, a path with a source extension is source, and a path
    /// the documentation selection keeps is documentation. Work is one pattern match per
    /// path.
    #[must_use]
    pub fn select<'path>(
        &self,
        paths: impl IntoIterator<Item = &'path ProjectPath>,
    ) -> SelectedFiles {
        let mut selected = SelectedFiles::default();
        for path in paths {
            if in_build_output(path) || !self.exclude.includes(&package_path(path)) {
                continue;
            }
            if self.is_source(path) {
                selected.source.push(path.clone());
            } else if self.documentation.format(path.as_str()).is_some() {
                selected.documentation.push(path.clone());
            }
        }
        selected.source.sort();
        selected.documentation.sort();
        selected
    }

    fn is_source(&self, path: &ProjectPath) -> bool {
        Path::new(path.as_str())
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                self.source_languages
                    .iter()
                    .any(|language| language.definition().matches_extension(extension))
            })
    }
}

/// The files one [`PackageFileSelection`] kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectedFiles {
    source: Vec<ProjectPath>,
    documentation: Vec<ProjectPath>,
}

impl SelectedFiles {
    /// The source files the package language's providers parse, in path order.
    #[must_use]
    pub fn source(&self) -> &[ProjectPath] {
        &self.source
    }

    /// The documentation files the documentation selection kept, in path order.
    #[must_use]
    pub fn documentation(&self) -> &[ProjectPath] {
        &self.documentation
    }

    /// Every selected file, source and documentation, in path order.
    #[must_use]
    pub fn files(&self) -> Vec<&ProjectPath> {
        let mut files: Vec<&ProjectPath> = self.source.iter().chain(&self.documentation).collect();
        files.sort();
        files
    }
}

/// Whether a folder of `path` is build or cache output. The file name itself is never
/// one: a file named `target` is not a folder.
fn in_build_output(path: &ProjectPath) -> bool {
    let mut segments = path.as_str().split('/');
    segments.next_back();
    segments.any(|segment| BUILD_OUTPUT_FOLDERS.contains(&segment))
}

/// One package-relative path as the matcher under [`PACKAGE_ROOT`] reads it.
fn package_path(path: &ProjectPath) -> std::path::PathBuf {
    Path::new(PACKAGE_ROOT).join(path.as_str())
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;
    use rift_protocol::documentation::DocumentationConfiguration;
    use rift_protocol::read::PathPattern;

    use super::{DocumentationSelection, PackageFileSelection, SelectedFiles};
    use crate::analyzer::PackageLanguage;

    fn paths(values: &[&str]) -> Vec<ProjectPath> {
        values
            .iter()
            .map(|value| ProjectPath::new(*value).expect("a fixture path"))
            .collect()
    }

    fn selected(language: PackageLanguage, exclude: &[&str], values: &[&str]) -> SelectedFiles {
        let exclude: Vec<PathPattern> = exclude
            .iter()
            .map(|pattern| PathPattern((*pattern).to_owned()))
            .collect();
        let documentation = DocumentationSelection::new(&DocumentationConfiguration::default())
            .expect("the default documentation selection compiles");
        let selection = PackageFileSelection::new(language, &exclude, documentation)
            .expect("the exclude patterns compile");
        selection.select(&paths(values))
    }

    fn names(files: &[ProjectPath]) -> Vec<&str> {
        files.iter().map(ProjectPath::as_str).collect()
    }

    /// Tests, examples, and benchmarks stay: an agent reads how a package is tested and
    /// used.
    #[test]
    fn test_tests_examples_and_benchmarks_are_selected_by_default() {
        let files = selected(
            PackageLanguage::Rust,
            &[],
            &[
                "src/lib.rs",
                "tests/integration.rs",
                "examples/basic.rs",
                "benches/throughput.rs",
                "src/integration_tests/mod.rs",
            ],
        );
        assert_eq!(
            names(files.source()),
            [
                "benches/throughput.rs",
                "examples/basic.rs",
                "src/integration_tests/mod.rs",
                "src/lib.rs",
                "tests/integration.rs",
            ]
        );
    }

    #[test]
    fn test_build_and_cache_output_is_never_selected() {
        let files = selected(
            PackageLanguage::Python,
            &[],
            &[
                "pkg/__init__.py",
                "pkg/__pycache__/cached.py",
                "node_modules/dep/index.py",
                "target/debug/build.py",
                ".git/hooks/pre-commit.py",
                "pkg/node_modules/README.md",
                "pkg/target.py",
            ],
        );
        assert_eq!(names(files.source()), ["pkg/__init__.py", "pkg/target.py"]);
        assert!(files.documentation().is_empty());
    }

    /// The caller's `[source] exclude` patterns drop what they match, under the `[source]`
    /// glob semantics: `*` never crosses `/`, and `**` does.
    #[test]
    fn test_exclude_patterns_drop_what_they_match_with_the_source_glob_semantics() {
        let files = selected(
            PackageLanguage::Python,
            &["**/tests/**", "pkg/*.pyi", "docs/*.md"],
            &[
                "pkg/core.py",
                "pkg/core.pyi",
                "pkg/nested/core.pyi",
                "pkg/tests/test_core.py",
                "docs/guide.md",
                "docs/api/reference.md",
            ],
        );
        assert_eq!(
            names(files.source()),
            ["pkg/core.py", "pkg/nested/core.pyi"]
        );
        assert_eq!(names(files.documentation()), ["docs/api/reference.md"]);
    }

    /// A TypeScript package's builds are all source: the declaration files and each
    /// JavaScript build beside them.
    #[test]
    fn test_a_typescript_package_selects_every_build_beside_its_declaration_files() {
        let files = selected(
            PackageLanguage::TypeScript,
            &[],
            &[
                "dist/index.d.ts",
                "dist/index.d.mts",
                "dist/index.d.cts",
                "dist/index.js",
                "dist/index.mjs",
                "dist/index.cjs",
                "src/view.tsx",
                "src/component.jsx",
                "package.json",
                "README.md",
            ],
        );
        assert_eq!(
            names(files.source()),
            [
                "dist/index.cjs",
                "dist/index.d.cts",
                "dist/index.d.mts",
                "dist/index.d.ts",
                "dist/index.js",
                "dist/index.mjs",
                "src/component.jsx",
                "src/view.tsx",
            ]
        );
        assert_eq!(names(files.documentation()), ["README.md"]);
        assert_eq!(files.files().len(), 9);
    }

    #[test]
    fn test_a_stub_is_selected_beside_its_module() {
        let files = selected(
            PackageLanguage::Python,
            &[],
            &["attr/__init__.py", "attr/__init__.pyi", "attr/py.typed"],
        );
        assert_eq!(
            names(files.source()),
            ["attr/__init__.py", "attr/__init__.pyi"]
        );
    }

    #[test]
    fn test_an_invalid_exclude_pattern_is_refused() {
        let documentation = DocumentationSelection::new(&DocumentationConfiguration::default())
            .expect("the default documentation selection compiles");
        let error = PackageFileSelection::new(
            PackageLanguage::Rust,
            &[PathPattern("[".to_owned())],
            documentation,
        )
        .expect_err("an unclosed character class is refused");
        assert_eq!(
            crate::documentation::failure::context_value(&error, "pattern").as_deref(),
            Some("[")
        );
    }
}
