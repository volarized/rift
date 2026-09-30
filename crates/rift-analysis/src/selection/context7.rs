//! A package's own `context7.json`: where its documentation lives, and what it leaves out.
//!
//! The file follows the Context7 configuration format
//! (<https://context7.com/schema/context7.json>). Documentation selection reads four of its
//! keys: `folders`, `excludeFolders`, `excludeFiles`, and `disallow`. Every other key
//! describes the package and leaves the selection as it is.

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, fault_label};
use rift_protocol::read::PathPattern;
use serde::{Deserialize, Serialize};

/// The file name a package's `context7.json` sits under, at the package root.
pub const CONTEXT7_FILE: &str = "context7.json";

/// Bytes a `context7.json` may hold, at most. The format's own bounds keep a valid file
/// far below it.
const CONTEXT7_BYTES_MAX: usize = 128 << 10;
/// Entries `folders` or `excludeFolders` may hold, at most, as the format bounds them.
const CONTEXT7_FOLDERS_MAX: usize = 50;
/// Entries `excludeFiles` may hold, at most, as the format bounds them.
const CONTEXT7_FILES_MAX: usize = 100;
/// Characters one entry may hold, at most, as the format bounds it.
const CONTEXT7_ENTRY_CHARS_MAX: usize = 255;

/// Why a `context7.json` cannot narrow the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Context7Violation {
    /// The file holds more bytes than a `context7.json` can.
    Oversized,
    /// The file is not a JSON object, or a key the selection reads holds another shape.
    Malformed,
    /// A list holds more entries than the format allows.
    TooManyEntries,
    /// An entry is empty, too long, or not a relative folder pattern or file name.
    EntryInvalid,
}

/// One `context7.json` refusal: its violation, and the key and entry that broke it.
#[derive(Debug)]
pub struct Context7Fault {
    violation: Context7Violation,
    key: Option<&'static str>,
    entry: Option<String>,
    source: Option<serde_json::Error>,
}

impl Context7Fault {
    const fn new(violation: Context7Violation) -> Self {
        Self {
            violation,
            key: None,
            entry: None,
            source: None,
        }
    }

    /// Why the file cannot narrow the selection.
    #[must_use]
    pub const fn violation(&self) -> Context7Violation {
        self.violation
    }

    /// The key whose value broke the format, when one did.
    #[must_use]
    pub const fn key(&self) -> Option<&'static str> {
        self.key
    }
}

impl Fault for Context7Fault {
    fn name(&self) -> ErrorName {
        match self.violation {
            Context7Violation::Oversized | Context7Violation::TooManyEntries => {
                ErrorName::Wire(ErrorCode::LimitExceeded)
            }
            Context7Violation::Malformed | Context7Violation::EntryInvalid => {
                ErrorName::Wire(ErrorCode::ContentUnavailable)
            }
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![
            ErrorContext::new("violation", fault_label(&self.violation)),
            ErrorContext::new("file", CONTEXT7_FILE),
        ];
        if let Some(key) = self.key {
            context.push(ErrorContext::new("key", key));
        }
        if let Some(entry) = &self.entry {
            context.push(ErrorContext::new("entry", entry.clone()));
        }
        context
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// A `context7.json` that cannot narrow the selection.
pub type Context7Error = Error<Context7Fault>;

/// The keys of a `context7.json` documentation selection reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context7 {
    folders: Vec<String>,
    excluded_folders: Vec<String>,
    excluded_files: Vec<String>,
    disallow: bool,
}

/// The file as the format spells it; keys the selection does not read are ignored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Context7Document {
    #[serde(default)]
    folders: Vec<String>,
    #[serde(default)]
    exclude_folders: Vec<String>,
    #[serde(default)]
    exclude_files: Vec<String>,
    #[serde(default)]
    disallow: bool,
}

impl Context7 {
    /// Reads one `context7.json`.
    ///
    /// Folder entries are `[source]` glob patterns relative to the package root, and file
    /// entries are file names. Work is bounded by `CONTEXT7_BYTES_MAX` and the format's own
    /// list and entry bounds.
    ///
    /// # Errors
    ///
    /// Returns [`Context7Error`] when the file is too large, is not the format's shape, or
    /// breaks one of its list or entry bounds.
    pub fn parse(bytes: &[u8]) -> Result<Self, Context7Error> {
        if bytes.len() > CONTEXT7_BYTES_MAX {
            return Err(Error::new(Context7Fault::new(Context7Violation::Oversized)));
        }
        let document = document(bytes)?;
        checked_entries(
            "folders",
            &document.folders,
            CONTEXT7_FOLDERS_MAX,
            folder_accepted,
        )?;
        checked_entries(
            "excludeFolders",
            &document.exclude_folders,
            CONTEXT7_FOLDERS_MAX,
            folder_accepted,
        )?;
        checked_entries(
            "excludeFiles",
            &document.exclude_files,
            CONTEXT7_FILES_MAX,
            file_name_accepted,
        )?;
        Ok(Self {
            folders: document.folders,
            excluded_folders: document.exclude_folders,
            excluded_files: document.exclude_files,
            disallow: document.disallow,
        })
    }

    /// The folders the package's documentation lives in; empty names none.
    #[must_use]
    pub fn folders(&self) -> &[String] {
        &self.folders
    }

    /// The folders whose documentation the package leaves out.
    #[must_use]
    pub fn excluded_folders(&self) -> &[String] {
        &self.excluded_folders
    }

    /// The documentation file names the package leaves out, wherever they sit.
    #[must_use]
    pub fn excluded_files(&self) -> &[String] {
        &self.excluded_files
    }

    /// Whether the package asks not to have its documentation indexed.
    #[must_use]
    pub const fn disallows(&self) -> bool {
        self.disallow
    }
}

/// The file's keys, read from a JSON object alone: serde would read a struct from an
/// array too, and an array is no `context7.json`.
fn document(bytes: &[u8]) -> Result<Context7Document, Context7Error> {
    let malformed = |source| {
        Error::new(Context7Fault {
            source,
            ..Context7Fault::new(Context7Violation::Malformed)
        })
    };
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|source| malformed(Some(source)))?;
    if !value.is_object() {
        return Err(malformed(None));
    }
    serde_json::from_value(value).map_err(|source| malformed(Some(source)))
}

/// Refuses a list past `entries_max`, then its first entry `accepted` refuses.
fn checked_entries(
    key: &'static str,
    entries: &[String],
    entries_max: usize,
    accepted: fn(&str) -> bool,
) -> Result<(), Context7Error> {
    if entries.len() > entries_max {
        return Err(Error::new(Context7Fault {
            key: Some(key),
            ..Context7Fault::new(Context7Violation::TooManyEntries)
        }));
    }
    match entries
        .iter()
        .find(|entry| !entry_accepted(entry, accepted))
    {
        Some(entry) => Err(Error::new(Context7Fault {
            key: Some(key),
            entry: Some(entry.clone()),
            ..Context7Fault::new(Context7Violation::EntryInvalid)
        })),
        None => Ok(()),
    }
}

/// Whether one entry holds between one and `CONTEXT7_ENTRY_CHARS_MAX` characters and
/// passes the list's own rule.
fn entry_accepted(entry: &str, accepted: fn(&str) -> bool) -> bool {
    let within_length = (1..=CONTEXT7_ENTRY_CHARS_MAX).contains(&entry.chars().count());
    within_length && accepted(entry)
}

/// A folder entry is a relative, forward-slash `[source]` pattern.
fn folder_accepted(entry: &str) -> bool {
    PathPattern(entry.to_owned()).violation().is_none()
}

/// A file entry is one file name: no separator of either kind.
fn file_name_accepted(entry: &str) -> bool {
    !entry.contains(['/', '\\']) && !entry.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use rift_core::{ErrorCode, ErrorContext, ErrorName};

    use super::{CONTEXT7_BYTES_MAX, CONTEXT7_FILES_MAX, Context7, Context7Violation};

    #[test]
    fn test_the_selection_keys_are_read_and_the_others_ignored() {
        let context7 = Context7::parse(
            br#"{
                "$schema": "https://context7.com/schema/context7.json",
                "projectTitle": "Beacon",
                "description": "Beacon serves signals over one socket.",
                "folders": ["docs"],
                "excludeFolders": ["docs/internal", "**/drafts/**"],
                "excludeFiles": ["CONTRIBUTING.md"],
                "rules": ["Prefer the async client."],
                "previousVersions": [{"tag": "v1.0.0"}]
            }"#,
        )
        .expect("a valid file");
        assert_eq!(context7.folders(), ["docs"]);
        assert_eq!(
            context7.excluded_folders(),
            ["docs/internal", "**/drafts/**"]
        );
        assert_eq!(context7.excluded_files(), ["CONTRIBUTING.md"]);
        assert!(!context7.disallows());
        assert_eq!(
            Context7::parse(b"{}").expect("an empty object"),
            Context7::default()
        );
    }

    #[test]
    fn test_a_file_past_its_byte_bound_is_refused() {
        let oversized = vec![b' '; CONTEXT7_BYTES_MAX + 1];
        let error = Context7::parse(&oversized).expect_err("an oversized file is refused");
        assert_eq!(error.fault().violation(), Context7Violation::Oversized);
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::LimitExceeded));
    }

    #[test]
    fn test_a_file_of_another_shape_is_refused_with_the_parser_as_cause() {
        use std::error::Error as _;

        for bytes in [
            &br#"{"folders": "docs"}"#[..],
            b"{",
            br#"{"disallow": "yes"}"#,
        ] {
            let error = Context7::parse(bytes).expect_err("another shape is refused");
            assert_eq!(error.fault().violation(), Context7Violation::Malformed);
            assert_eq!(error.name(), ErrorName::Wire(ErrorCode::ContentUnavailable));
            assert!(error.source().is_some(), "the parser names the cause");
        }
        for bytes in [&b"[]"[..], b"\"docs\"", b"null"] {
            let error = Context7::parse(bytes).expect_err("a file that is no object is refused");
            assert_eq!(error.fault().violation(), Context7Violation::Malformed);
        }
    }

    #[test]
    fn test_a_list_past_its_entry_bound_is_refused() {
        let files: Vec<String> = (0..=CONTEXT7_FILES_MAX)
            .map(|index| format!("\"NOTES{index}.md\""))
            .collect();
        let bytes = format!("{{\"excludeFiles\": [{}]}}", files.join(","));
        let error = Context7::parse(bytes.as_bytes()).expect_err("too many entries");
        assert_eq!(error.fault().violation(), Context7Violation::TooManyEntries);
        assert_eq!(error.fault().key(), Some("excludeFiles"));
    }

    #[test]
    fn test_an_entry_the_format_refuses_names_its_key_and_value() {
        let long = "a".repeat(256);
        let cases = [
            (r#"{"folders": ["/docs"]}"#.to_owned(), "folders", "/docs"),
            (
                r#"{"excludeFolders": ["docs/../secrets"]}"#.to_owned(),
                "excludeFolders",
                "docs/../secrets",
            ),
            (r#"{"folders": [""]}"#.to_owned(), "folders", ""),
            (
                r#"{"excludeFiles": ["docs/NOTES.md"]}"#.to_owned(),
                "excludeFiles",
                "docs/NOTES.md",
            ),
            (
                format!(r#"{{"excludeFiles": ["{long}"]}}"#),
                "excludeFiles",
                long.as_str(),
            ),
        ];
        for (text, key, entry) in &cases {
            let error = Context7::parse(text.as_bytes()).expect_err("an invalid entry");
            assert_eq!(error.fault().violation(), Context7Violation::EntryInvalid);
            assert_eq!(error.fault().key(), Some(*key));
            assert!(
                error
                    .context()
                    .contains(&ErrorContext::new("entry", (*entry).to_owned())),
                "{text}"
            );
        }
    }

    #[test]
    fn test_disallow_is_read() {
        let context7 = Context7::parse(br#"{"disallow": true}"#).expect("a valid file");
        assert!(context7.disallows());
    }
}
