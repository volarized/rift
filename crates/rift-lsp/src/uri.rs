//! Document URIs: conversion between project paths and file URIs.
//!
//! Engines address documents by `file:` URI; Rift addresses them by
//! [`ProjectPath`] below one workspace root. The root's forward-slash form
//! anchors both directions: emission percent-encodes root plus path, and
//! parsing refuses any URI that is not a hostless `file:` URI naming a
//! valid project path strictly under the root - escaped traversal decodes
//! first and is then refused by the path rules.
//! A URI under a cataloged package root addresses that package's file by
//! dependency unit through [`EngineRoots`]; only a URI under no root
//! refuses.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use lsp_types::Uri;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use rift_core::{
    Error, ErrorCode, ErrorContext, ErrorName, Fault, PackageIdentity, PathError, ProjectPath,
    SourceUnitId, SourceUnitIdError, fault_label,
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
    /// The relative path under a package root mints no source unit. The refusal is
    /// boxed so every other document fault stays small.
    UnitRefused {
        /// The unit identity's own refusal.
        #[serde(skip)]
        source: Box<SourceUnitIdError>,
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
            Self::UnitRefused { source } => context.extend(source.context()),
            _ => {}
        }
        context
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PathRefused { source } => Some(source),
            Self::UnitRefused { source } => Some(source.as_ref()),
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
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
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

    /// The root one project path below this one names. A project path holds no dot
    /// segment and no trailing separator, so the joined form stays normalized.
    fn joined(&self, path: &ProjectPath) -> Self {
        let slash_form = match (self.slash_form.as_str(), path.as_str()) {
            (root, "") => root.to_owned(),
            ("/", below) => format!("/{below}"),
            (root, below) => format!("{root}/{below}"),
        };
        Self { slash_form }
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

/// One cataloged package's source root; its files are addressed by dependency unit.
///
/// A root is the folder a package's files take their package path below, or one import
/// folder or module of a Python distribution below the `site-packages` folder the path
/// counts from: `jwt` holds `jwt/api.py` of the `PyJWT` distribution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageRoot {
    root: TreeRoot,
    package: PackageIdentity,
    within: Option<ProjectPath>,
}

impl PackageRoot {
    /// Pairs a package's source root with the identity its units carry.
    #[must_use]
    pub const fn new(root: TreeRoot, package: PackageIdentity) -> Self {
        Self {
            root,
            package,
            within: None,
        }
    }

    /// The root of one import folder or module `within` below `base`, the folder the
    /// package's paths count from: a file below it takes `within`, then its own path
    /// below the folder, as its package path. An empty `within` names `base` itself.
    #[must_use]
    pub fn within(base: &TreeRoot, within: ProjectPath, package: PackageIdentity) -> Self {
        let root = base.joined(&within);
        Self {
            root,
            package,
            within: (!within.as_str().is_empty()).then_some(within),
        }
    }

    /// The package's source root in forward-slash form.
    #[must_use]
    pub const fn root(&self) -> &TreeRoot {
        &self.root
    }

    /// The package as its manager identifies it.
    #[must_use]
    pub const fn package(&self) -> &PackageIdentity {
        &self.package
    }

    /// The package path of the file `relative` names below this root.
    fn package_path(&self, relative: ProjectPath) -> Result<ProjectPath, UriError> {
        let Some(within) = &self.within else {
            return Ok(relative);
        };
        if relative.as_str().is_empty() {
            return Ok(within.clone());
        }
        ProjectPath::new(format!("{}/{}", within.as_str(), relative.as_str()))
            .map_err(|source| Error::new(UriFault::PathRefused { source }))
    }
}

/// Every root an engine's URIs may fall under: the workspace tree and cataloged package roots.
///
/// The package roots are shared: every tree spelling one walk tries holds the same set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineRoots {
    tree: TreeRoot,
    packages: Arc<[PackageRoot]>,
}

impl EngineRoots {
    /// Roots holding the workspace tree alone.
    #[must_use]
    pub fn new(tree: TreeRoot) -> Self {
        Self {
            tree,
            packages: Arc::from([]),
        }
    }

    /// Adds the cataloged package roots a URI may fall under.
    #[must_use]
    pub fn with_packages(mut self, packages: impl Into<Arc<[PackageRoot]>>) -> Self {
        self.packages = packages.into();
        self
    }

    /// The workspace tree root.
    #[must_use]
    pub const fn tree(&self) -> &TreeRoot {
        &self.tree
    }

    /// How many package roots a URI may fall under.
    #[must_use]
    pub fn package_count(&self) -> usize {
        self.packages.len()
    }

    /// Resolves one engine URI to the project path or package file it addresses.
    ///
    /// The longest root holding the URI wins, the tree among them, and the tree wins a
    /// tie. A package root nested in the tree - an install under `node_modules` or
    /// `.venv` - therefore addresses its own files as that package, while a URI under the
    /// tree and under no deeper package root answers [`EngineAddress::Project`]. Only
    /// [`UriFault::OutsideRoot`] from the tree lets a package root answer; every other
    /// tree fault - scheme, host, decoding, path rules - is returned as is. Each root is
    /// tried through its own [`TreeRoot::project_path`], which decodes the URI first, so a
    /// pnpm folder the engine spells `nanoid%405.1.6` matches the resolved `nanoid@5.1.6`
    /// root. The matched package path mints the unit through
    /// [`SourceUnitId::for_package`]. The work is one pass over the package roots, whose
    /// count is the catalog's own package bound.
    ///
    /// # Errors
    ///
    /// Returns [`UriError`] with [`UriFault::OutsideRoot`] for a URI under no
    /// root, [`UriFault::UnitRefused`] when the matched path mints no unit,
    /// and the tree's own fault for every other refusal.
    pub fn address(&self, uri: &Uri) -> Result<EngineAddress, UriError> {
        let tree = match self.tree.project_path(uri) {
            Ok(path) => Some(path),
            Err(error) if is_outside_root(&error) => None,
            Err(error) => return Err(error),
        };
        let deeper = self.claimed_package(uri).filter(|(package, _)| {
            tree.is_none() || package.root.slash_form.len() > self.tree.slash_form.len()
        });
        match (deeper, tree) {
            (Some((package, relative)), _) => {
                let path = package.package_path(relative?)?;
                let unit =
                    SourceUnitId::for_package(&package.package, &path).map_err(|source| {
                        Error::new(UriFault::UnitRefused {
                            source: Box::new(source),
                        })
                    })?;
                Ok(EngineAddress::Package(PackageFile {
                    package: package.package.clone(),
                    path,
                    unit,
                }))
            }
            (None, Some(path)) => Ok(EngineAddress::Project(path)),
            (None, None) => Err(Error::new(UriFault::OutsideRoot)),
        }
    }

    /// The longest package root holding `uri`, with the path below it.
    fn claimed_package(&self, uri: &Uri) -> Option<(&PackageRoot, Result<ProjectPath, UriError>)> {
        self.packages
            .iter()
            .filter_map(|package| match package.root.project_path(uri) {
                Err(error) if is_outside_root(&error) => None,
                outcome => Some((package, outcome)),
            })
            .max_by_key(|(package, _)| package.root.slash_form.len())
    }
}

/// One engine URI's target: a project path in the tree, or a cataloged package's file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineAddress {
    /// A document below the workspace root.
    Project(ProjectPath),
    /// A file of a cataloged package.
    Package(PackageFile),
}

/// One file of a cataloged package: the package, the file's path in it, and its unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageFile {
    package: PackageIdentity,
    path: ProjectPath,
    unit: SourceUnitId,
}

impl PackageFile {
    /// The package that holds the file.
    #[must_use]
    pub const fn package(&self) -> &PackageIdentity {
        &self.package
    }

    /// The file's path below the package's root, such as `src/lib.rs`.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// The file's source unit, such as `rift://source/cargo/helper@0.1.0/src/lib.rs`.
    #[must_use]
    pub const fn unit(&self) -> &SourceUnitId {
        &self.unit
    }
}

/// Whether a refusal is the decoded path falling outside the tried root.
fn is_outside_root(error: &UriError) -> bool {
    matches!(error.fault(), UriFault::OutsideRoot)
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

    fn uri(text: &str) -> Uri {
        parse_uri(text).expect("fixture uri parses")
    }

    fn package_root(slash_form: &str, name: &str, version: &str) -> PackageRoot {
        PackageRoot::new(
            root(slash_form),
            PackageIdentity {
                manager: "cargo".to_owned(),
                name: name.to_owned(),
                version: version.to_owned(),
            },
        )
    }

    fn unit(address: &str) -> SourceUnitId {
        SourceUnitId::parse(address).expect("fixture unit is canonical")
    }

    fn npm_root(slash_form: &str, name: &str, version: &str) -> PackageRoot {
        PackageRoot::new(root(slash_form), identity("npm", name, version))
    }

    fn identity(manager: &str, name: &str, version: &str) -> PackageIdentity {
        PackageIdentity {
            manager: manager.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
        }
    }

    /// The unit a package file's address names, or the project path a tree address names.
    fn addressed(roots: &EngineRoots, text: &str) -> Result<String, UriError> {
        roots.address(&uri(text)).map(|address| match address {
            EngineAddress::Project(path) => format!("project {path}"),
            EngineAddress::Package(file) => file.unit().to_string(),
        })
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

    #[test]
    fn engine_roots_answer_a_package_root_nested_in_the_tree_before_the_tree() {
        let roots = EngineRoots::new(root("/work/ws")).with_packages(vec![
            package_root("/work", "outer", "1.0.0"),
            package_root("/work/ws", "self", "0.0.0"),
            npm_root("/work/ws/node_modules/nanoid", "nanoid", "5.1.6"),
            package_root("/work/ws/vendor/helper", "helper", "0.1.0"),
        ]);
        let answers = [
            (
                "file:///work/ws/node_modules/nanoid/index.d.ts",
                "rift://source/npm/nanoid@5.1.6/index.d.ts",
            ),
            (
                "file:///work/ws/vendor/helper/src/lib.rs",
                "rift://source/cargo/helper@0.1.0/src/lib.rs",
            ),
            ("file:///work/ws/src/lib.rs", "project src/lib.rs"),
            (
                "file:///work/ws/node_modules/other/index.js",
                "project node_modules/other/index.js",
            ),
            (
                "file:///work/elsewhere.rs",
                "rift://source/cargo/outer@1.0.0/elsewhere.rs",
            ),
        ];
        for (text, expected) in answers {
            assert_eq!(
                addressed(&roots, text),
                Ok(expected.to_owned()),
                "a root equal to the tree, or holding it, loses to the tree; an install no \
                 package root names stays a tree path: {text}"
            );
        }
    }

    #[test]
    fn engine_roots_decode_a_pnpm_folder_before_matching_its_resolved_root() {
        let roots = EngineRoots::new(root("/work/ws")).with_packages(vec![
            npm_root(
                "/work/ws/node_modules/.pnpm/nanoid@5.1.6/node_modules/nanoid",
                "nanoid",
                "5.1.6",
            ),
            npm_root(
                "/work/ws/node_modules/.pnpm/@types+node@26.6.2/node_modules/@types/node",
                "@types/node",
                "26.6.2",
            ),
        ]);
        assert_eq!(
            addressed(
                &roots,
                "file:///work/ws/node_modules/.pnpm/nanoid%405.1.6/node_modules/nanoid/index.d.ts"
            ),
            Ok("rift://source/npm/nanoid@5.1.6/index.d.ts".to_owned())
        );
        assert_eq!(
            addressed(
                &roots,
                "file:///work/ws/node_modules/.pnpm/%40types%2Bnode%4026.6.2/node_modules/%40types/node/fs.d.ts"
            ),
            Ok("rift://source/npm/@types/node@26.6.2/fs.d.ts".to_owned())
        );
    }

    #[test]
    fn an_import_root_prefixes_its_package_paths_with_its_own_path() {
        let site_packages = root("/work/ws/.venv/lib/python3.12/site-packages");
        let roots = EngineRoots::new(root("/work/ws")).with_packages(vec![
            PackageRoot::within(
                &site_packages,
                path("jwt"),
                identity("pypi", "pyjwt", "2.10.1"),
            ),
            PackageRoot::within(
                &site_packages,
                path("google/protobuf"),
                identity("pypi", "protobuf", "6.33.0"),
            ),
            PackageRoot::within(
                &site_packages,
                path("six.py"),
                identity("pypi", "six", "1.17.0"),
            ),
        ]);
        let site = "file:///work/ws/.venv/lib/python3.12/site-packages";
        let answers = [
            (
                "jwt/api_jwt.py",
                "rift://source/pypi/pyjwt@2.10.1/jwt/api_jwt.py",
            ),
            (
                "google/protobuf/message.py",
                "rift://source/pypi/protobuf@6.33.0/google/protobuf/message.py",
            ),
            ("six.py", "rift://source/pypi/six@1.17.0/six.py"),
            (
                "google/other.py",
                "project .venv/lib/python3.12/site-packages/google/other.py",
            ),
            (
                "six.pyi",
                "project .venv/lib/python3.12/site-packages/six.pyi",
            ),
        ];
        for (below, expected) in answers {
            assert_eq!(
                addressed(&roots, &format!("{site}/{below}")),
                Ok(expected.to_owned()),
                "{below}"
            );
        }
        let base = PackageRoot::within(&site_packages, path(""), identity("pypi", "six", "1.0"));
        assert_eq!(
            base.root(),
            &site_packages,
            "an empty path names the base itself"
        );
        let top = PackageRoot::within(&root("/"), path("opt"), identity("pypi", "six", "1.0"));
        assert_eq!(top.root(), &root("/opt"));
    }

    #[test]
    fn engine_roots_address_a_package_file_by_its_unit() {
        let roots = EngineRoots::new(root("/work/ws")).with_packages(vec![package_root(
            "/cache/helper-0.1.0",
            "helper",
            "0.1.0",
        )]);
        let address = roots
            .address(&uri("file:///cache/helper-0.1.0/src/lib.rs"))
            .expect("the package root holds the file");
        let EngineAddress::Package(file) = address else {
            panic!("a file under a package root addresses the package: {address:?}");
        };
        assert_eq!(
            file.unit(),
            &unit("rift://source/cargo/helper@0.1.0/src/lib.rs")
        );
        assert_eq!(file.package(), &identity("cargo", "helper", "0.1.0"));
        assert_eq!(file.path(), &path("src/lib.rs"));
    }

    #[test]
    fn engine_roots_prefer_the_longest_package_root_whatever_the_catalog_order() {
        let outer = package_root("/cache/outer", "outer", "1.0.0");
        let inner = package_root("/cache/outer/vendor/inner", "inner", "2.0.0");
        for packages in [
            vec![outer.clone(), inner.clone()],
            vec![inner.clone(), outer.clone()],
        ] {
            let roots = EngineRoots::new(root("/work/ws")).with_packages(packages);
            assert_eq!(
                addressed(&roots, "file:///cache/outer/vendor/inner/src/lib.rs"),
                Ok("rift://source/cargo/inner@2.0.0/src/lib.rs".to_owned())
            );
            assert_eq!(
                addressed(&roots, "file:///cache/outer/vendor/other.rs"),
                Ok("rift://source/cargo/outer@1.0.0/vendor/other.rs".to_owned())
            );
        }
    }

    #[test]
    fn engine_roots_refuse_a_uri_under_no_root_including_sibling_prefixes() {
        let roots = EngineRoots::new(root("/work/ws")).with_packages(vec![package_root(
            "/cache/helperx",
            "helperx",
            "0.1.0",
        )]);
        for outside in [
            "file:///cache/helper/src/lib.rs",
            "file:///work/other/src/lib.rs",
            "file:///cache",
        ] {
            let error = roots.address(&uri(outside)).expect_err("under no root");
            assert!(matches!(error.fault(), UriFault::OutsideRoot), "{outside}");
            assert_eq!(error.name(), ErrorName::Wire(ErrorCode::PermissionDenied));
        }
    }

    #[test]
    fn engine_roots_pass_scheme_host_decoding_and_path_faults_through_unchanged() {
        let roots = EngineRoots::new(root("/work/ws")).with_packages(vec![package_root(
            "/cache/helper",
            "helper",
            "0.1.0",
        )]);
        let scheme = roots
            .address(&uri("untitled:src/lib.rs"))
            .expect_err("scheme refused");
        assert!(matches!(
            scheme.fault(),
            UriFault::SchemeRefused { scheme } if scheme == "untitled"
        ));
        let host = roots
            .address(&uri("file://build-host/cache/helper/src/lib.rs"))
            .expect_err("host refused");
        assert!(matches!(
            host.fault(),
            UriFault::HostRefused { host } if host == "build-host"
        ));
        let undecodable = roots
            .address(&uri("file:///cache/helper/%FF.rs"))
            .expect_err("not unicode");
        assert!(matches!(undecodable.fault(), UriFault::PathNotDecodable));
        let traversal = roots
            .address(&uri("file:///cache/helper/%2E%2E/out.rs"))
            .expect_err("traversal under a package root is refused by the path rules");
        assert!(matches!(traversal.fault(), UriFault::PathRefused { .. }));
    }

    #[test]
    fn engine_roots_count_their_package_roots() {
        let tree = root("/work/ws");
        let roots = EngineRoots::new(tree.clone());
        assert_eq!(roots.package_count(), 0);
        assert_eq!(roots.tree(), &tree);
        let with_packages = roots.with_packages(vec![
            package_root("/cache/a", "a", "1.0.0"),
            package_root("/cache/b", "b", "1.0.0"),
        ]);
        assert_eq!(with_packages.package_count(), 2);
        assert_eq!(with_packages.tree(), &tree);
    }

    #[test]
    fn unit_refused_names_the_evidence_and_exposes_the_unit_source() {
        let package = package_root("/cache/helper", "helper", "0.1.0\\beta");
        assert_eq!(package.root(), &root("/cache/helper"));
        assert_eq!(package.package().version, "0.1.0\\beta");
        let roots = EngineRoots::new(root("/work/ws")).with_packages(vec![package]);
        let error = roots
            .address(&uri("file:///cache/helper/src/lib.rs"))
            .expect_err("the version spells no source path");
        assert!(matches!(error.fault(), UriFault::UnitRefused { .. }));
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::UnsupportedPath));
        let rendered = error.to_string();
        assert!(rendered.contains("fault unit_refused"), "{rendered}");
        assert!(rendered.contains("identity source_unit"), "{rendered}");
        assert!(rendered.contains("violation backslash"), "{rendered}");
        assert!(std::error::Error::source(&error).is_some());
    }
}
