//! The analyzer manifest: what one publication's `analyzer_revision` is taken over.
//!
//! Two publications are comparable only when the analyzer that produced them is the same,
//! so the revision has to change whenever the analysis would. The manifest states what
//! the analysis depends on - each shipped grammar's package version and checksum, and the
//! content digest of the extraction, normalization, identity, and document-builder source
//! - and [`analyzer_revision`] is the digest of that document.
//!
//! The document is generated beside the served schemas and committed, so a change to any
//! named input that ships without a regenerated manifest fails `just generate-check`
//! before it reaches a reader.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_protocol::canonical::canonical_json;
use rift_protocol::read::Digest;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

/// The committed manifest, read at compile time so a publication's revision needs no file
/// at run time.
const ANALYZER_MANIFEST: &str = include_str!("analyzer-manifest.json");

/// The lockfile the grammar versions and checksums are read from, relative to the
/// repository root.
const LOCKFILE_PATH: &str = "Cargo.lock";

/// The package-name prefix every shipped grammar carries, including the runtime itself.
const GRAMMAR_PREFIX: &str = "tree-sitter";

/// The source the analysis depends on, relative to the repository root. A directory
/// stands for every `.rs` file below it, because a publication's content is decided by
/// the whole crate: which node a grammar calls a declaration, which name it exports, and
/// how a record is normalized and addressed.
///
/// Naming single files here was not enough. A grammar rule set in `rift-syntax` and the
/// export rule in `walk.rs` each change what a publication holds while `extract.rs` and
/// `analyzer.rs` stay byte-identical.
const ANALYZED_SOURCES: [&str; 7] = [
    "crates/rift-core/src/identity.rs",
    "crates/rift-index/src/dependency/analyzer.rs",
    "crates/rift-index/src/dependency/walk.rs",
    "crates/rift-index/src/lexical.rs",
    "crates/rift-index/src/semantic.rs",
    "crates/rift-provider/src",
    "crates/rift-syntax/src",
];

/// The extension every analyzed source file carries.
const SOURCE_EXTENSION: &str = "rs";

/// Deepest directory below one named root the source walk descends into.
const SOURCE_DEPTH_MAX: usize = 8;

/// Most entries the source walk examines below all named roots together.
const SOURCE_ENTRIES_MAX: usize = 4_096;

/// The analyzer this build publishes under.
///
/// Computed once per process from the committed manifest, since the manifest is fixed at
/// compile time.
#[must_use]
pub fn analyzer_revision() -> Digest {
    static REVISION: OnceLock<Digest> = OnceLock::new();
    REVISION
        .get_or_init(|| {
            let rendered = format!("{:x}", Sha256::digest(ANALYZER_MANIFEST.as_bytes()));
            Digest(rendered[..DIGEST_WIRE_CHARS].to_owned())
        })
        .clone()
}

/// One shipped grammar, as the workspace lockfile pins it.
#[derive(Debug, Serialize)]
struct GrammarPin {
    name: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    checksum: Option<String>,
}

/// One analyzed source file and the digest of its bytes.
#[derive(Debug, Serialize)]
struct SourcePin {
    path: String,
    digest: String,
}

/// The document `analyzer_revision` is taken over.
#[derive(Debug, Serialize)]
struct AnalyzerManifest {
    grammars: Vec<GrammarPin>,
    sources: Vec<SourcePin>,
}

/// Renders the analyzer manifest from the tree below `root`.
///
/// The rendering is RFC 8785 canonical JSON with a trailing newline, so the committed
/// document is byte-identical wherever it is generated. The work is one pass over the
/// lockfile plus one read per analyzed source file, and the walk that finds those files
/// examines at most 4096 entries at a depth of at most 8 directories below each named
/// root.
///
/// # Errors
///
/// Returns [`ManifestError`] when the lockfile or one analyzed source file cannot be
/// read, when a named root holds no analyzed source, or when the walk crosses its
/// entry bound.
pub fn render_analyzer_manifest(root: &Path) -> Result<String, ManifestError> {
    let lockfile_path = root.join(LOCKFILE_PATH);
    let lockfile = read_text(&lockfile_path)?;
    let grammars = grammar_pins(&lockfile);
    let mut sources = Vec::new();
    let mut examined = 0_usize;
    for named in ANALYZED_SOURCES {
        let found = analyzed_files(root, named, &mut examined)?;
        for relative in found {
            let text = read_text(&root.join(&relative))?;
            sources.push(SourcePin {
                digest: format!("{:x}", Sha256::digest(text.as_bytes())),
                path: relative,
            });
        }
    }
    sources.sort_by(|left, right| left.path.cmp(&right.path));
    let manifest = AnalyzerManifest { grammars, sources };
    let mut rendered = canonical_json(&manifest).map_err(|source| ManifestError::Render {
        path: lockfile_path,
        source,
    })?;
    rendered.push('\n');
    Ok(rendered)
}

/// Every analyzed source file one named root stands for, in path order: the file itself,
/// or every `.rs` file below the directory.
///
/// The walk keeps its own stack rather than recursing, and `examined` carries the entry
/// count across roots so one repository-wide bound holds. A root naming neither a file
/// nor a directory holding one analyzed source is a refusal: a manifest that silently
/// dropped an input would state an analyzer the tree does not have.
fn analyzed_files(
    root: &Path,
    named: &str,
    examined: &mut usize,
) -> Result<Vec<String>, ManifestError> {
    let start = root.join(named);
    if start.is_file() {
        return Ok(vec![named.to_owned()]);
    }
    let mut found: Vec<String> = Vec::new();
    let mut pending: Vec<(PathBuf, String, usize)> = vec![(start, named.to_owned(), 0)];
    while let Some((directory, relative, depth)) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|source| ManifestError::Unreadable {
            path: directory.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| ManifestError::Unreadable {
                path: directory.clone(),
                source,
            })?;
            *examined = examined.saturating_add(1);
            if *examined > SOURCE_ENTRIES_MAX {
                return Err(ManifestError::WalkExhausted {
                    path: directory,
                    examined: *examined,
                });
            }
            // A name this machine cannot spell as UTF-8 names no analyzed source. APFS
            // refuses to create one, so the arm is unreachable on macOS.
            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            let below = format!("{relative}/{name}");
            let path = entry.path();
            if path.is_dir() {
                if depth < SOURCE_DEPTH_MAX {
                    pending.push((path, below, depth + 1));
                }
                continue;
            }
            if path
                .extension()
                .is_some_and(|extension| extension == SOURCE_EXTENSION)
            {
                found.push(below);
            }
        }
    }
    if found.is_empty() {
        return Err(ManifestError::RootEmpty {
            path: root.join(named),
        });
    }
    found.sort();
    Ok(found)
}

/// The path the generated manifest is committed at, relative to the repository root.
#[must_use]
pub fn analyzer_manifest_path() -> PathBuf {
    PathBuf::from("crates/rift-index/src/dependency/analyzer-manifest.json")
}

/// Every shipped grammar the lockfile pins, in name order.
///
/// The lockfile is TOML, and every package table states `name` and `version` before the
/// next `[[package]]` header, so this reads the tables in one pass rather than parsing
/// the whole document into a model no other code needs.
fn grammar_pins(lockfile: &str) -> Vec<GrammarPin> {
    let mut pins: Vec<GrammarPin> = Vec::new();
    let mut current: Option<GrammarPin> = None;
    for line in lockfile.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            push_grammar(&mut pins, current.take());
            current = Some(GrammarPin {
                name: String::new(),
                version: String::new(),
                checksum: None,
            });
            continue;
        }
        let Some(pin) = current.as_mut() else {
            continue;
        };
        if let Some(value) = quoted_value(line, "name") {
            pin.name = value;
        } else if let Some(value) = quoted_value(line, "version") {
            pin.version = value;
        } else if let Some(value) = quoted_value(line, "checksum") {
            pin.checksum = Some(value);
        }
    }
    push_grammar(&mut pins, current);
    pins.sort_by(|left, right| (&left.name, &left.version).cmp(&(&right.name, &right.version)));
    pins
}

/// Keeps one package table when it names a shipped grammar.
fn push_grammar(pins: &mut Vec<GrammarPin>, pin: Option<GrammarPin>) {
    if let Some(pin) = pin
        && pin.name.starts_with(GRAMMAR_PREFIX)
    {
        pins.push(pin);
    }
}

/// The value of one `key = "value"` line, absent when the line states another key.
fn quoted_value(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    let rest = rest.strip_prefix('"')?;
    rest.strip_suffix('"').map(ToOwned::to_owned)
}

fn read_text(path: &Path) -> Result<String, ManifestError> {
    fs::read_to_string(path).map_err(|source| ManifestError::Unreadable {
        path: path.to_path_buf(),
        source,
    })
}

/// Why the analyzer manifest could not be rendered.
#[derive(Debug)]
pub enum ManifestError {
    /// One named input could not be read.
    Unreadable {
        /// The input the renderer reached for.
        path: PathBuf,
        /// What the filesystem said.
        source: io::Error,
    },
    /// One named root held no analyzed source file.
    RootEmpty {
        /// The root the walk found nothing below.
        path: PathBuf,
    },
    /// The source walk examined more entries than its bound allows.
    WalkExhausted {
        /// The directory the walk was reading when it crossed the bound.
        path: PathBuf,
        /// Entries examined when the walk stopped.
        examined: usize,
    },
    /// The manifest could not be rendered canonically.
    Render {
        /// The lockfile the render started from.
        path: PathBuf,
        /// What the renderer said.
        source: serde_json::Error,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { path, source } => write!(
                formatter,
                "the analyzer manifest input could not be read: path {}, {source}; run the \
                 generator from the repository root",
                path.display()
            ),
            Self::RootEmpty { path } => write!(
                formatter,
                "the analyzer manifest input holds no `{SOURCE_EXTENSION}` source: path {}; \
                 run the generator from the repository root",
                path.display()
            ),
            Self::WalkExhausted { path, examined } => write!(
                formatter,
                "the analyzer manifest walk examined {examined} entries, past its bound of \
                 {SOURCE_ENTRIES_MAX}: path {}",
                path.display()
            ),
            Self::Render { path, source } => write!(
                formatter,
                "the analyzer manifest could not be rendered: path {}, {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            Self::Render { source, .. } => Some(source),
            Self::RootEmpty { .. } | Self::WalkExhausted { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ANALYZER_MANIFEST, ManifestError, analyzer_revision, grammar_pins};

    #[test]
    fn test_analyzer_revision_is_the_wire_digest_of_the_committed_manifest() {
        let revision = analyzer_revision();
        assert_eq!(revision.0.len(), 8, "{revision:?}");
        assert!(revision.0.chars().all(|symbol| symbol.is_ascii_hexdigit()));
        assert_eq!(
            revision,
            analyzer_revision(),
            "the revision is fixed for one build"
        );
    }

    #[test]
    fn test_committed_manifest_names_every_shipped_grammar_and_source() {
        let manifest: serde_json::Value =
            serde_json::from_str(ANALYZER_MANIFEST).expect("the committed manifest is JSON");
        let grammars = manifest["grammars"]
            .as_array()
            .expect("the manifest lists grammars");
        assert!(
            grammars
                .iter()
                .any(|grammar| grammar["name"] == serde_json::json!("tree-sitter-rust")),
            "{grammars:?}"
        );
        let sources = manifest["sources"]
            .as_array()
            .expect("the manifest lists sources");
        let paths: Vec<&str> = sources
            .iter()
            .filter_map(|source| source["path"].as_str())
            .collect();
        // A grammar rule set decides which node is a declaration, and the export rule
        // decides which declaration is published: both change a publication while the
        // extraction source stays byte-identical.
        for expected in [
            "crates/rift-core/src/identity.rs",
            "crates/rift-index/src/dependency/analyzer.rs",
            "crates/rift-index/src/dependency/walk.rs",
            "crates/rift-provider/src/normalization.rs",
            "crates/rift-syntax/src/extract.rs",
            "crates/rift-syntax/src/rust.rs",
            "crates/rift-syntax/src/rust/attachment.rs",
            "crates/rift-syntax/src/typescript.rs",
        ] {
            assert!(
                paths.contains(&expected),
                "{expected} is not pinned: {paths:?}"
            );
        }
        assert!(
            paths.windows(2).all(|pair| pair[0] < pair[1]),
            "the manifest lists sources in path order: {paths:?}"
        );
    }

    #[test]
    fn test_grammar_pins_keep_only_grammar_packages_in_name_order() {
        let lockfile = "\
[[package]]\n\
name = \"tree-sitter-rust\"\n\
version = \"0.24.2\"\n\
checksum = \"abc\"\n\
\n\
[[package]]\n\
name = \"serde\"\n\
version = \"1.0.0\"\n\
\n\
[[package]]\n\
name = \"tree-sitter\"\n\
version = \"0.25.10\"\n\
checksum = \"def\"\n";

        let pins = grammar_pins(lockfile);

        let named: Vec<&str> = pins.iter().map(|pin| pin.name.as_str()).collect();
        assert_eq!(named, ["tree-sitter", "tree-sitter-rust"]);
        assert_eq!(pins[0].version, "0.25.10");
        assert_eq!(pins[0].checksum.as_deref(), Some("def"));
    }

    /// A tree carrying the lockfile and every analyzed source the renderer reads.
    fn manifest_root(grammar_version: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("a manifest root");
        std::fs::write(
            root.path().join("Cargo.lock"),
            format!(
                "version = 4\n\n[[package]]\nname = \"tree-sitter\"\n\
                 version = \"{grammar_version}\"\nchecksum = \"abc\"\n"
            ),
        )
        .expect("the lockfile is written");
        for named in super::ANALYZED_SOURCES {
            let named_path = std::path::Path::new(named);
            let path = if named_path
                .extension()
                .is_some_and(|extension| extension == "rs")
            {
                root.path().join(named)
            } else {
                root.path().join(named).join("probe.rs")
            };
            std::fs::create_dir_all(path.parent().expect("a source has a directory"))
                .expect("the source directory is created");
            std::fs::write(&path, format!("// {named}\n")).expect("the source is written");
        }
        root
    }

    #[test]
    fn test_one_tree_renders_the_same_manifest_every_time() {
        let root = manifest_root("0.25.10");

        let first = super::render_analyzer_manifest(root.path()).expect("the manifest renders");
        let second = super::render_analyzer_manifest(root.path()).expect("the manifest renders");

        assert_eq!(first, second);
        assert!(first.ends_with('\n'), "{first}");
        assert!(first.contains("\"tree-sitter\""), "{first}");
    }

    #[test]
    fn test_a_changed_input_renders_another_manifest() {
        let root = manifest_root("0.25.10");
        let rendered = super::render_analyzer_manifest(root.path()).expect("the manifest renders");

        let source = root.path().join(super::ANALYZED_SOURCES[0]);
        assert!(source.is_file(), "the first named root is one file");
        std::fs::write(&source, "// another analysis\n").expect("the source is rewritten");
        let after_source =
            super::render_analyzer_manifest(root.path()).expect("the manifest renders");
        assert_ne!(rendered, after_source);

        let bumped = manifest_root("0.25.11");
        let after_grammar =
            super::render_analyzer_manifest(bumped.path()).expect("the manifest renders");
        assert_ne!(rendered, after_grammar);
    }

    #[test]
    fn test_a_source_added_below_a_named_root_renders_another_manifest() {
        let root = manifest_root("0.25.10");
        let rendered = super::render_analyzer_manifest(root.path()).expect("the manifest renders");

        let grammar = root.path().join("crates/rift-syntax/src/another.rs");
        std::fs::write(&grammar, "// another grammar rule set\n").expect("the source is written");

        let after = super::render_analyzer_manifest(root.path()).expect("the manifest renders");
        assert_ne!(rendered, after);
        assert!(
            after.contains("crates/rift-syntax/src/another.rs"),
            "{after}"
        );
    }

    /// A named root's directories are walked to their bound, so a grammar rule set filed
    /// one directory below the crate root is pinned like any other source.
    #[test]
    fn test_the_walk_descends_into_a_directory_below_a_named_root() {
        let root = manifest_root("0.25.10");
        let nested = root.path().join("crates/rift-syntax/src/rust");
        std::fs::create_dir_all(&nested).expect("the nested directory is created");
        std::fs::write(nested.join("attachment.rs"), "// attachment\n")
            .expect("the source is written");

        let rendered = super::render_analyzer_manifest(root.path()).expect("the manifest renders");

        assert!(
            rendered.contains("crates/rift-syntax/src/rust/attachment.rs"),
            "{rendered}"
        );
    }

    #[test]
    fn test_the_manifest_is_committed_below_the_analyzer_it_states() {
        let path = super::analyzer_manifest_path();

        assert_eq!(
            path,
            std::path::PathBuf::from("crates/rift-index/src/dependency/analyzer-manifest.json")
        );
    }

    /// A lockfile's own header stands before the first package table, and the pins read
    /// past it rather than reading it as one.
    #[test]
    fn test_grammar_pins_read_past_a_lockfile_header() {
        let lockfile = "\
version = 4\n\
name = \"not-a-package\"\n\
\n\
[[package]]\n\
name = \"tree-sitter\"\n\
version = \"0.25.10\"\n";

        let pins = grammar_pins(lockfile);

        let named: Vec<&str> = pins.iter().map(|pin| pin.name.as_str()).collect();
        assert_eq!(named, ["tree-sitter"]);
        assert_eq!(pins[0].checksum, None);
    }

    /// A refusal the renderer raised itself names no underlying failure: nothing below it
    /// failed, the tree simply does not hold what the manifest states.
    #[test]
    fn test_a_renderer_refusal_names_no_underlying_failure() {
        let empty = ManifestError::RootEmpty {
            path: std::path::PathBuf::from("/workspace/crates/rift-syntax/src"),
        };
        let exhausted = ManifestError::WalkExhausted {
            path: std::path::PathBuf::from("/workspace/crates/rift-syntax/src"),
            examined: super::SOURCE_ENTRIES_MAX + 1,
        };

        for error in [empty, exhausted] {
            assert!(std::error::Error::source(&error).is_none(), "{error}");
        }
    }

    #[test]
    fn test_a_root_holding_no_source_names_the_path() {
        let root = tempfile::tempdir().expect("a manifest root");
        std::fs::create_dir_all(root.path().join("crates/rift-syntax/src"))
            .expect("the root directory is created");

        let error = super::analyzed_files(root.path(), "crates/rift-syntax/src", &mut 0)
            .expect_err("a root holding no source refuses");

        assert!(matches!(error, ManifestError::RootEmpty { .. }));
        assert!(
            error.to_string().contains("crates/rift-syntax/src"),
            "{error}"
        );
    }

    #[test]
    fn test_a_walk_past_its_entry_bound_names_the_bound() {
        let root = tempfile::tempdir().expect("a manifest root");
        let directory = root.path().join("crates/rift-syntax/src");
        std::fs::create_dir_all(&directory).expect("the root directory is created");
        std::fs::write(directory.join("probe.rs"), "// probe\n").expect("the source is written");
        let mut examined = super::SOURCE_ENTRIES_MAX;

        let error = super::analyzed_files(root.path(), "crates/rift-syntax/src", &mut examined)
            .expect_err("a walk past its bound refuses");

        assert!(matches!(error, ManifestError::WalkExhausted { .. }));
        let message = error.to_string();
        assert!(
            message.contains(&super::SOURCE_ENTRIES_MAX.to_string()),
            "{message}"
        );
    }

    #[test]
    fn test_unreadable_input_names_the_path_and_the_remedy() {
        let error = super::render_analyzer_manifest(std::path::Path::new("/no-such-root"))
            .expect_err("a missing lockfile refuses");
        assert!(matches!(error, ManifestError::Unreadable { .. }));
        let message = error.to_string();
        assert!(message.contains("Cargo.lock"), "{message}");
        assert!(message.contains("repository root"), "{message}");
        assert!(std::error::Error::source(&error).is_some());
    }
}
