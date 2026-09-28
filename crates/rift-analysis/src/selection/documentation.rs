//! Documentation selection: which text files the index and package analysis collect as
//! documentation.
//!
//! One selection serves a workspace's own files and a package's. A file is documentation
//! when its extension names a documentation format, and the selection then leaves out change
//! logs, licenses, codes of conduct, and archived, deprecated, or translated copies. The
//! `[documentation]` table overrides these defaults for a workspace; a package's own
//! `context7.json` narrows them for that package.

use std::collections::BTreeSet;
use std::path::Path;

use rift_protocol::documentation::{DocumentationConfiguration, DocumentationSourceFormat};
use rift_protocol::read::PathPattern;

use super::PACKAGE_ROOT;
use super::context7::Context7;
use crate::glob::{PathMatcher, PathVerdict, SourcePatternError};

/// Documentation file names left out by default, wherever they sit: change logs,
/// licenses, and codes of conduct.
pub const DOCUMENTATION_EXCLUDED_FILES: [&str; 8] = [
    "CHANGELOG.md",
    "changelog.md",
    "CHANGELOG.mdx",
    "changelog.mdx",
    "LICENSE.md",
    "license.md",
    "CODE_OF_CONDUCT.md",
    "code_of_conduct.md",
];

/// Folders whose documentation is left out by default: archived, deprecated, legacy, and
/// superseded copies, and translations. A value without `/` names a folder at any depth; a
/// value with `/` names a path from the root.
pub const DOCUMENTATION_EXCLUDED_FOLDERS: [&str; 31] = [
    "*archive*",
    "*archived*",
    "old",
    "docs/old",
    "*deprecated*",
    "*legacy*",
    "*previous*",
    "*outdated*",
    "*superseded*",
    "i18n/zh*",
    "i18n/es*",
    "i18n/fr*",
    "i18n/de*",
    "i18n/ja*",
    "i18n/ko*",
    "i18n/ru*",
    "i18n/pt*",
    "i18n/it*",
    "i18n/ar*",
    "i18n/hi*",
    "i18n/tr*",
    "i18n/nl*",
    "i18n/pl*",
    "i18n/sv*",
    "i18n/vi*",
    "i18n/th*",
    "zh-cn",
    "zh-tw",
    "zh-hk",
    "zh-mo",
    "zh-sg",
];

/// Extensions a root-level file keeps its place under when a `context7.json` names the
/// folders its documentation lives in: a package's own README stays.
const ROOT_DOCUMENTATION_EXTENSIONS: [&str; 3] = ["md", "mdx", "markdown"];

/// Returns the documentation format selected by a supported file extension.
#[must_use]
pub fn documentation_format(file_name: &str) -> Option<DocumentationSourceFormat> {
    let extension = Path::new(file_name).extension()?.to_str()?;
    match extension {
        "md" | "markdown" => Some(DocumentationSourceFormat::Markdown),
        "mdx" => Some(DocumentationSourceFormat::Mdx),
        "rst" => Some(DocumentationSourceFormat::RestructuredText),
        "txt" => Some(DocumentationSourceFormat::Text),
        "ipynb" => Some(DocumentationSourceFormat::Notebook),
        _ => None,
    }
}

/// The compiled rules deciding which files are collected as documentation.
///
/// A file whose extension names no documentation format is never documentation. Of the
/// rest, the rules decide in this order:
///
/// 1. `enabled = false` collects nothing.
/// 2. A `[documentation] exclude` match is left out.
/// 3. A `[documentation] force_include` match is collected.
/// 4. A default or `context7.json` excluded file name, or a file under an excluded folder,
///    is left out.
/// 5. When a `context7.json` names the folders its documentation lives in, a file outside
///    them is left out, unless it is a Markdown file at the root.
/// 6. Everything else is collected.
#[derive(Debug)]
pub struct DocumentationSelection {
    enabled: bool,
    exclude: GlobList,
    force_include: GlobList,
    excluded_files: BTreeSet<String>,
    excluded_folders: GlobList,
    folders: Option<GlobList>,
}

impl DocumentationSelection {
    /// Compiles the defaults under one `[documentation]` table.
    ///
    /// # Errors
    ///
    /// Returns [`SourcePatternError`] when an `exclude` or `force_include` pattern is not a
    /// valid glob.
    pub fn new(configuration: &DocumentationConfiguration) -> Result<Self, SourcePatternError> {
        Ok(Self {
            enabled: configuration.enabled,
            exclude: GlobList::compile(&pattern_strings(&configuration.exclude))?,
            force_include: GlobList::compile(&pattern_strings(&configuration.force_include))?,
            excluded_files: DOCUMENTATION_EXCLUDED_FILES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            excluded_folders: GlobList::compile(&folder_patterns(
                DOCUMENTATION_EXCLUDED_FOLDERS.iter().copied(),
            ))?,
            folders: None,
        })
    }

    /// This selection narrowed by a package's own `context7.json`: its excluded files and
    /// folders join the defaults, its folders name where the package's documentation lives,
    /// and a file that disallows indexing leaves no documentation selected.
    ///
    /// # Errors
    ///
    /// Returns [`SourcePatternError`] when a folder the file names is not a valid glob.
    pub fn narrowed_by(mut self, context7: &Context7) -> Result<Self, SourcePatternError> {
        if context7.disallows() {
            self.enabled = false;
        }
        self.excluded_files
            .extend(context7.excluded_files().iter().cloned());
        let excluded = DOCUMENTATION_EXCLUDED_FOLDERS
            .iter()
            .copied()
            .chain(context7.excluded_folders().iter().map(String::as_str));
        self.excluded_folders = GlobList::compile(&folder_patterns(excluded))?;
        if !context7.folders().is_empty() {
            let folders = folder_patterns(context7.folders().iter().map(String::as_str));
            self.folders = Some(GlobList::compile(&folders)?);
        }
        Ok(self)
    }

    /// The documentation format `path` is collected under, or `None` when the file is not
    /// documentation or the selection leaves it out.
    #[must_use]
    pub fn format(&self, path: &str) -> Option<DocumentationSourceFormat> {
        let format = documentation_format(path)?;
        self.selects(path).then_some(format)
    }

    fn selects(&self, path: &str) -> bool {
        if !self.enabled || self.exclude.matches(path) {
            return false;
        }
        if self.force_include.matches(path) {
            return true;
        }
        !self.excluded_by_default(path) && self.within_folders(path)
    }

    fn excluded_by_default(&self, path: &str) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        self.excluded_files.contains(name) || self.excluded_folders.matches(path)
    }

    fn within_folders(&self, path: &str) -> bool {
        let Some(folders) = &self.folders else {
            return true;
        };
        is_root_documentation(path) || folders.matches(path)
    }
}

/// Whether `path` is a Markdown file at the package root, such as `README.md`.
fn is_root_documentation(path: &str) -> bool {
    !path.contains('/')
        && Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| ROOT_DOCUMENTATION_EXTENSIONS.contains(&extension))
}

/// The `[source]` glob patterns one folder value names: a value without `/` names a folder
/// of that name at any depth (`**/old/**`), and a value with `/` names the path from the
/// root and everything below it (`docs/old`, `docs/old/**`).
fn folder_patterns<'value>(folders: impl Iterator<Item = &'value str>) -> Vec<String> {
    let mut patterns = Vec::new();
    for folder in folders {
        let folder = folder.trim_end_matches('/');
        if folder.contains('/') {
            patterns.push(folder.to_owned());
            patterns.push(format!("{folder}/**"));
        } else {
            patterns.push(format!("**/{folder}/**"));
        }
    }
    patterns
}

fn pattern_strings(patterns: &[PathPattern]) -> Vec<String> {
    patterns.iter().map(|pattern| pattern.0.clone()).collect()
}

/// One compiled glob list, asking whether any of its patterns matches a relative path.
#[derive(Debug)]
struct GlobList(Option<PathMatcher>);

impl GlobList {
    fn compile(patterns: &[String]) -> Result<Self, SourcePatternError> {
        if patterns.is_empty() {
            return Ok(Self(None));
        }
        PathMatcher::build(Path::new(PACKAGE_ROOT), &[], patterns)
            .map(|matcher| Self(Some(matcher)))
    }

    fn matches(&self, path: &str) -> bool {
        self.0.as_ref().is_some_and(|matcher| {
            matcher.verdict(&Path::new(PACKAGE_ROOT).join(path)) == PathVerdict::Excluded
        })
    }
}

#[cfg(test)]
mod tests {
    use rift_protocol::documentation::{DocumentationConfiguration, DocumentationSourceFormat};
    use rift_protocol::read::PathPattern;

    use super::{DOCUMENTATION_EXCLUDED_FOLDERS, DocumentationSelection, documentation_format};
    use crate::selection::context7::Context7;

    fn compiled(configuration: &DocumentationConfiguration) -> DocumentationSelection {
        DocumentationSelection::new(configuration).expect("the selection compiles")
    }

    fn patterns(values: &[&str]) -> Vec<PathPattern> {
        values
            .iter()
            .map(|value| PathPattern((*value).to_owned()))
            .collect()
    }

    fn collected<'path>(
        selection: &DocumentationSelection,
        paths: &[&'path str],
    ) -> Vec<&'path str> {
        paths
            .iter()
            .copied()
            .filter(|path| selection.format(path).is_some())
            .collect()
    }

    #[test]
    fn test_documentation_format_maps_each_supported_extension() {
        let cases = [
            ("README.md", Some(DocumentationSourceFormat::Markdown)),
            ("guide.markdown", Some(DocumentationSourceFormat::Markdown)),
            ("page.mdx", Some(DocumentationSourceFormat::Mdx)),
            (
                "index.rst",
                Some(DocumentationSourceFormat::RestructuredText),
            ),
            ("NOTES.txt", Some(DocumentationSourceFormat::Text)),
            ("tour.ipynb", Some(DocumentationSourceFormat::Notebook)),
            ("src/lib.rs", None),
            ("Makefile", None),
        ];
        for (path, format) in cases {
            assert_eq!(documentation_format(path), format, "{path}");
        }
    }

    /// Every documentation file is collected except the default exclusions: change logs,
    /// licenses, codes of conduct, and archived, deprecated, or translated folders.
    #[test]
    fn test_the_defaults_leave_out_change_logs_licenses_and_superseded_folders() {
        let selection = compiled(&DocumentationConfiguration::default());
        let paths = [
            "README.md",
            "docs/guide.md",
            "docs/api.rst",
            "notes.txt",
            "tour.ipynb",
            "CHANGELOG.md",
            "packages/core/changelog.md",
            "LICENSE.md",
            "CODE_OF_CONDUCT.md",
            "docs/archive/v1.md",
            "docs/old-archived/v2.md",
            "old/guide.md",
            "docs/old/guide.md",
            "guides/deprecated-api/intro.md",
            "i18n/zh-CN/guide.md",
            "docs/zh-cn/guide.md",
            "docs/i18n/zh-CN/guide.md",
            "src/lib.rs",
        ];
        assert_eq!(
            collected(&selection, &paths),
            [
                "README.md",
                "docs/guide.md",
                "docs/api.rst",
                "notes.txt",
                "tour.ipynb",
                "docs/i18n/zh-CN/guide.md",
            ]
        );
    }

    #[test]
    fn test_every_default_folder_compiles_and_matches_a_file_below_it() {
        let selection = compiled(&DocumentationConfiguration::default());
        for folder in DOCUMENTATION_EXCLUDED_FOLDERS {
            let named = folder.replace('*', "x");
            let path = format!("{named}/guide.md");
            assert!(selection.format(&path).is_none(), "{path}");
        }
    }

    #[test]
    fn test_the_documentation_table_excludes_adds_back_and_turns_collection_off() {
        let configuration = DocumentationConfiguration {
            enabled: true,
            exclude: patterns(&["docs/internal/**", "CHANGELOG.md"]),
            force_include: patterns(&["CHANGELOG.md", "docs/archive/**"]),
        };
        let selection = compiled(&configuration);
        assert_eq!(
            collected(
                &selection,
                &[
                    "docs/internal/notes.md",
                    "CHANGELOG.md",
                    "docs/archive/v1.md",
                    "LICENSE.md",
                    "docs/guide.md",
                ]
            ),
            ["docs/archive/v1.md", "docs/guide.md"],
            "exclude wins over force_include, which adds back a default exclusion"
        );

        let disabled = compiled(&DocumentationConfiguration {
            enabled: false,
            ..DocumentationConfiguration::default()
        });
        assert!(collected(&disabled, &["README.md", "docs/guide.md"]).is_empty());
    }

    #[test]
    fn test_an_invalid_table_pattern_is_refused() {
        let error = DocumentationSelection::new(&DocumentationConfiguration {
            force_include: patterns(&["docs/[guide"]),
            ..DocumentationConfiguration::default()
        })
        .expect_err("an unclosed character class is refused");
        assert_eq!(error.fault().pattern(), Some("docs/[guide"));
    }

    /// A package's `context7.json` narrows the selection within the defaults: its folders
    /// name where the documentation lives, a root Markdown file stays, and its exclusions
    /// join the defaults.
    #[test]
    fn test_a_context7_file_narrows_the_selection_within_the_defaults() {
        let context7 = Context7::parse(
            br#"{
                "projectTitle": "Beacon",
                "folders": ["docs", "guides/*"],
                "excludeFolders": ["docs/internal"],
                "excludeFiles": ["CONTRIBUTING.md"]
            }"#,
        )
        .expect("a valid context7.json");
        let narrowed = compiled(&DocumentationConfiguration::default())
            .narrowed_by(&context7)
            .expect("the folders compile");
        assert_eq!(
            collected(
                &narrowed,
                &[
                    "README.md",
                    "CONTRIBUTING.md",
                    "notes.txt",
                    "docs/guide.md",
                    "docs/CONTRIBUTING.md",
                    "docs/internal/design.md",
                    "packages/core/docs/api.md",
                    "guides/start/intro.md",
                    "examples/usage.md",
                    "docs/archive/v1.md",
                ]
            ),
            [
                "README.md",
                "docs/guide.md",
                "packages/core/docs/api.md",
                "guides/start/intro.md",
            ]
        );
    }

    #[test]
    fn test_a_context7_file_that_disallows_indexing_leaves_no_documentation() {
        let context7 = Context7::parse(br#"{"disallow": true}"#).expect("a valid context7.json");
        let narrowed = compiled(&DocumentationConfiguration::default())
            .narrowed_by(&context7)
            .expect("the defaults compile");
        assert!(collected(&narrowed, &["README.md", "docs/guide.md"]).is_empty());
    }

    #[test]
    fn test_a_context7_folder_that_is_no_glob_is_refused() {
        let context7 =
            Context7::parse(br#"{"folders": ["docs/[api"]}"#).expect("the value is a path pattern");
        let error = compiled(&DocumentationConfiguration::default())
            .narrowed_by(&context7)
            .expect_err("an unclosed character class is refused");
        assert_eq!(error.fault().pattern(), Some("docs/[api"));
    }
}
