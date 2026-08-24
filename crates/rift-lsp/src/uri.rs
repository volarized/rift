//! Document URIs: conversion between project paths and file URIs.
//!
//! Engines address documents by `file:` URI; Rift addresses them by
//! [`ProjectPath`] below one workspace root. The root's forward-slash form
//! anchors both directions: emission percent-encodes root plus path, and
//! parsing refuses any URI that is not a hostless `file:` URI naming a
//! valid project path strictly under the root - escaped traversal decodes
//! first and is then refused by the path rules.

use std::path::Path;
use std::str::FromStr;

use lsp_types::Uri;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use rift_core::{
    Error, ErrorCode, ErrorContext, ErrorName, Fault, PathError, ProjectPath, fault_label,
};
use serde::Serialize;

/// The sole URI scheme a document may carry.
const FILE_URI_SCHEME: &str = "file";

/// The scheme-and-authority prefix every emitted document URI starts with.
const FILE_URI_PREFIX: &str = "file://";

/// ASCII bytes percent-encoded in an emitted file URI path.
///
/// The kept characters are RFC 3986 unreserved marks plus the path
/// separator and the colon, so a Windows drive prefix stays literal; every
/// other byte, including each byte of a multi-byte UTF-8 sequence, is
/// `%XX`-escaped.
const FILE_URI_ESCAPE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/')
    .remove(b':');

/// A URI or root that cannot address a workspace document.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UriFault {
    /// The workspace root is not an absolute path.
    RootNotAbsolute {
        /// The root as given.
        root: String,
    },
    /// The workspace root is not valid Unicode.
    RootNotUnicode,
    /// The URI does not parse under RFC 3986.
    UriMalformed {
        /// The URI as received.
        uri: String,
    },
    /// The URI carries a scheme other than `file`.
    SchemeRefused {
        /// The scheme as received.
        scheme: String,
    },
    /// The URI carries a host; documents are always local.
    HostRefused {
        /// The authority as received.
        host: String,
    },
    /// The URI path does not percent-decode to Unicode.
    PathNotDecodable,
    /// The decoded path is not under the workspace root.
    OutsideRoot,
    /// The decoded relative path broke a project path rule.
    PathRefused {
        /// The path rule's own refusal.
        #[serde(skip)]
        source: PathError,
    },
}

impl Fault for UriFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::OutsideRoot => ErrorName::Wire(ErrorCode::PermissionDenied),
            Self::PathRefused { source } => source.name(),
            _ => ErrorName::Wire(ErrorCode::UnsupportedPath),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("fault", fault_label(self))];
        match self {
            Self::RootNotAbsolute { root } => {
                context.push(ErrorContext::new("root", root.clone()));
            }
            Self::UriMalformed { uri } => context.push(ErrorContext::new("uri", uri.clone())),
            Self::SchemeRefused { scheme } => {
                context.push(ErrorContext::new("scheme", scheme.clone()));
            }
            Self::HostRefused { host } => context.push(ErrorContext::new("host", host.clone())),
            Self::PathRefused { source } => context.extend(source.context()),
            _ => {}
        }
        context
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PathRefused { source } => Some(source),
            _ => None,
        }
    }
}

/// A URI or root that cannot address a workspace document.
pub type UriError = Error<UriFault>;

/// One workspace root in forward-slash form, anchoring URI conversion.
///
/// The form is `/abs/dir` on Unix and `C:/abs/dir` on Windows, with the
/// drive letter held uppercase so a lowercase-drive URI still matches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeRoot {
    slash_form: String,
}

impl TreeRoot {
    /// Converts an absolute filesystem root into its forward-slash form.
    ///
    /// # Errors
    ///
    /// Returns [`UriError`] for a relative or non-Unicode root.
    pub fn new(root: &Path) -> Result<Self, UriError> {
        let text = root
            .to_str()
            .ok_or_else(|| Error::new(UriFault::RootNotUnicode))?;
        Self::from_slash_form(text.replace('\\', "/"))
    }

    /// Accepts a root already in forward-slash form.
    ///
    /// # Errors
    ///
    /// Returns [`UriError`] when the form is not absolute.
    pub fn from_slash_form(value: impl Into<String>) -> Result<Self, UriError> {
        let mut slash_form: String = value.into();
        while slash_form.len() > 1 && slash_form.ends_with('/') {
            slash_form.pop();
        }
        match slash_form.as_bytes() {
            [b'/', ..] => {}
            [drive, b':', b'/', ..] if drive.is_ascii_alphabetic() => {
                slash_form.replace_range(..1, &slash_form[..1].to_ascii_uppercase());
            }
            _ => {
                return Err(Error::new(UriFault::RootNotAbsolute { root: slash_form }));
            }
        }
        Ok(Self { slash_form })
    }

    /// The file URI addressing the root itself.
    ///
    /// # Errors
    ///
    /// Returns [`UriError`] only when the composed text does not parse,
    /// which encoding rules out; the arm exists so no failure is unwrapped.
    pub fn root_uri(&self) -> Result<Uri, UriError> {
        self.compose_uri("")
    }

    /// The file URI addressing one project path below this root.
    ///
    /// # Errors
    ///
    /// Returns [`UriError`] only when the composed text does not parse,
    /// which encoding rules out; the arm exists so no failure is unwrapped.
    pub fn document_uri(&self, path: &ProjectPath) -> Result<Uri, UriError> {
        self.compose_uri(path.as_str())
    }

    /// Composes and parses the URI text for one relative path.
    fn compose_uri(&self, relative: &str) -> Result<Uri, UriError> {
        let mut text = String::from(FILE_URI_PREFIX);
        if !self.slash_form.starts_with('/') {
            text.push('/');
        }
        text.push_str(&utf8_percent_encode(&self.slash_form, FILE_URI_ESCAPE_SET).to_string());
        if !relative.is_empty() {
            text.push('/');
            text.push_str(&utf8_percent_encode(relative, FILE_URI_ESCAPE_SET).to_string());
        }
        Uri::from_str(&text).map_err(|_| Error::new(UriFault::UriMalformed { uri: text }))
    }

    /// The project path one document URI addresses below this root.
    ///
    /// The empty path names the root itself.
    ///
    /// # Errors
    ///
    /// Returns [`UriError`] for a non-`file` scheme, a host-carrying URI,
    /// an undecodable path, a path outside this root, or a decoded
    /// relative path the project path rules refuse.
    pub fn project_path(&self, uri: &Uri) -> Result<ProjectPath, UriError> {
        refuse_scheme_and_host(uri)?;
        let decoded = percent_decode_str(uri.path().as_str())
            .decode_utf8()
            .map_err(|_| Error::new(UriFault::PathNotDecodable))?;
        let absolute = normalize_drive(&decoded);
        let Some(remainder) = absolute.strip_prefix(self.slash_form.as_str()) else {
            return Err(Error::new(UriFault::OutsideRoot));
        };
        let relative = match remainder.as_bytes() {
            [] => "",
            [b'/', ..] => &remainder[1..],
            _ => return Err(Error::new(UriFault::OutsideRoot)),
        };
        ProjectPath::new(relative).map_err(|source| Error::new(UriFault::PathRefused { source }))
    }
}

/// Parses one URI string the wire handed over, with the malformed refusal.
///
/// # Errors
///
/// Returns [`UriError`] when the text does not parse under RFC 3986.
pub fn parse_uri(text: &str) -> Result<Uri, UriError> {
    Uri::from_str(text).map_err(|_| {
        Error::new(UriFault::UriMalformed {
            uri: text.to_owned(),
        })
    })
}

/// Refuses any scheme but `file` and any non-empty authority.
fn refuse_scheme_and_host(uri: &Uri) -> Result<(), UriError> {
    let scheme = uri
        .scheme()
        .map(|scheme| scheme.as_str().to_owned())
        .unwrap_or_default();
    if !scheme.eq_ignore_ascii_case(FILE_URI_SCHEME) {
        return Err(Error::new(UriFault::SchemeRefused { scheme }));
    }
    let host = uri
        .authority()
        .map(|authority| authority.as_str().to_owned())
        .unwrap_or_default();
    if !host.is_empty() {
        return Err(Error::new(UriFault::HostRefused { host }));
    }
    Ok(())
}

/// Strips the URI-path slash before a Windows drive and uppercases the drive.
fn normalize_drive(decoded: &str) -> String {
    match decoded.as_bytes() {
        [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => {
            let mut normalized = decoded[1..].to_owned();
            normalized.replace_range(..1, &normalized[..1].to_ascii_uppercase());
            normalized
        }
        _ => decoded.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(slash_form: &str) -> TreeRoot {
        TreeRoot::from_slash_form(slash_form).expect("fixture root is absolute")
    }

    fn path(value: &str) -> ProjectPath {
        ProjectPath::new(value).expect("fixture path is valid")
    }

    #[test]
    fn unix_roots_emit_and_parse_percent_encoded_document_uris() {
        let tree = root("/work space/ws/");
        let uri = tree.document_uri(&path("src/caf\u{e9}.rs")).expect("uri");
        assert_eq!(uri.as_str(), "file:///work%20space/ws/src/caf%C3%A9.rs");
        assert_eq!(tree.project_path(&uri), Ok(path("src/caf\u{e9}.rs")));
        let tree_uri = tree.document_uri(&path("")).expect("root uri");
        assert_eq!(tree_uri.as_str(), "file:///work%20space/ws");
        assert_eq!(tree.project_path(&tree_uri), Ok(path("")));
    }

    #[test]
    fn windows_drive_roots_round_trip_and_accept_encoded_lowercase_drives() {
        let tree = root("c:/work/ws");
        let uri = tree.document_uri(&path("src/lib.rs")).expect("uri");
        assert_eq!(uri.as_str(), "file:///C:/work/ws/src/lib.rs");
        for spelling in [
            "file:///C:/work/ws/src/lib.rs",
            "file:///c:/work/ws/src/lib.rs",
            "file:///c%3A/work/ws/src/lib.rs",
        ] {
            let parsed = parse_uri(spelling).expect("uri parses");
            assert_eq!(
                tree.project_path(&parsed),
                Ok(path("src/lib.rs")),
                "{spelling}"
            );
        }
    }

    #[test]
    fn relative_roots_are_refused_and_backslash_roots_normalize() {
        let error = TreeRoot::from_slash_form("work/ws").expect_err("relative root");
        assert!(matches!(error.fault(), UriFault::RootNotAbsolute { .. }));
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::UnsupportedPath));
        let tree = TreeRoot::new(Path::new("/work/ws")).expect("absolute root");
        assert_eq!(tree, root("/work/ws"));
    }

    #[test]
    fn non_file_schemes_and_hosts_are_refused() {
        let tree = root("/work/ws");
        let untitled = parse_uri("untitled:src/lib.rs").expect("uri parses");
        let scheme = tree.project_path(&untitled).expect_err("scheme refused");
        assert!(matches!(
            scheme.fault(),
            UriFault::SchemeRefused { scheme } if scheme == "untitled"
        ));
        let hosted = parse_uri("file://build-host/work/ws/src/lib.rs").expect("uri parses");
        let host = tree.project_path(&hosted).expect_err("host refused");
        assert!(matches!(
            host.fault(),
            UriFault::HostRefused { host } if host == "build-host"
        ));
    }

    #[test]
    fn paths_outside_the_root_are_refused_including_sibling_prefixes() {
        let tree = root("/work/ws");
        for outside in [
            "file:///work/other/src/lib.rs",
            "file:///work/wsx/src/lib.rs",
            "file:///work",
        ] {
            let uri = parse_uri(outside).expect("uri parses");
            let error = tree.project_path(&uri).expect_err("outside the root");
            assert!(matches!(error.fault(), UriFault::OutsideRoot), "{outside}");
            assert_eq!(error.name(), ErrorName::Wire(ErrorCode::PermissionDenied));
        }
    }

    #[test]
    fn escaped_traversal_decodes_first_and_is_then_refused() {
        let tree = root("/work/ws");
        let traversal = parse_uri("file:///work/ws/%2E%2E/outside.rs").expect("uri parses");
        let error = tree
            .project_path(&traversal)
            .expect_err("traversal refused");
        assert!(matches!(error.fault(), UriFault::PathRefused { .. }));
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::UnsupportedPath));
        assert!(error.to_string().contains("dot_segment"));
    }

    #[test]
    fn undecodable_percent_escapes_are_refused() {
        let tree = root("/work/ws");
        let invalid = parse_uri("file:///work/ws/%FF.rs").expect("uri parses");
        let error = tree.project_path(&invalid).expect_err("not unicode");
        assert!(matches!(error.fault(), UriFault::PathNotDecodable));
    }

    #[test]
    fn malformed_uri_text_is_refused_by_parse() {
        let error = parse_uri("file://work ws/lib.rs").expect_err("space is not a URI byte");
        assert!(matches!(error.fault(), UriFault::UriMalformed { .. }));
    }

    #[test]
    fn fault_rendering_names_the_evidence_and_exposes_the_path_source() {
        let relative = TreeRoot::from_slash_form("work/ws").expect_err("relative root");
        assert!(relative.to_string().contains("root work/ws"));
        let malformed = parse_uri("file://work ws/lib.rs").expect_err("malformed");
        assert!(malformed.to_string().contains("uri file://work ws/lib.rs"));
        let tree = root("/work/ws");
        let scheme = tree
            .project_path(&parse_uri("untitled:src/lib.rs").expect("uri parses"))
            .expect_err("scheme refused");
        assert!(scheme.to_string().contains("scheme untitled"));
        let host = tree
            .project_path(&parse_uri("file://build-host/work/ws/a.rs").expect("uri parses"))
            .expect_err("host refused");
        assert!(host.to_string().contains("host build-host"));
        let refused = tree
            .project_path(&parse_uri("file:///work/ws/%2E%2E/out.rs").expect("uri parses"))
            .expect_err("traversal refused");
        assert!(std::error::Error::source(&refused).is_some());
        let outside = tree
            .project_path(&parse_uri("file:///work/other/a.rs").expect("uri parses"))
            .expect_err("outside the root");
        assert!(std::error::Error::source(&outside).is_none());
    }
}
