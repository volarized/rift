//! Validates explicit source sets without source acquisition or filesystem access.

use std::collections::BTreeSet;

use rift_core::{ProjectPath, SourceUnitId};
use rift_protocol::documentation::{
    DOCUMENTATION_LICENSE_FILES_MAX, NOTEBOOK_CELL_ID_BYTES_MAX, NOTEBOOK_SOURCE_RANGES_MAX,
};
use rift_protocol::documentation::{
    DOCUMENTATION_SOURCE_BYTES_MAX, DOCUMENTATION_SOURCES_MAX, DOCUMENTATION_TEXT_BYTES_MAX,
    DOCUMENTATION_TOTAL_BYTES_MAX, DocumentationContentIdentity, DocumentationDigest,
    DocumentationSelectionReason, DocumentationSource, DocumentationSourceFormat,
    DocumentationSourceIdentity, NotebookCellIdentity,
};
use rift_protocol::read::{Language, SourceKind, SourceLocationKind};

use super::failure::{DocumentationError, DocumentationViolation, refused};
use super::identity::{canonical_digest, content_digest, is_digest};

pub(super) fn slice<'a>(
    text: &'a str,
    range: &rift_protocol::read::TextRange,
) -> Result<&'a str, DocumentationError> {
    let start = usize::try_from(range.start)
        .map_err(|_| refused(DocumentationViolation::Range, "range"))?;
    let end =
        usize::try_from(range.end).map_err(|_| refused(DocumentationViolation::Range, "range"))?;
    text.get(start..end)
        .ok_or_else(|| refused(DocumentationViolation::Range, "range"))
}

/// One selected source record paired with the bytes it describes.
#[derive(Clone, Debug)]
pub struct DocumentationInput<'source> {
    source: DocumentationSource,
    text: &'source str,
    #[cfg(feature = "collector")]
    syntax: Option<&'source rift_syntax::SyntaxDocument>,
    chunks: Vec<rift_protocol::documentation::DocumentationChunk>,
}

impl<'source> DocumentationInput<'source> {
    /// Validates source facts against the caller's exact bytes.
    ///
    /// This function performs no I/O. Notebook cell ranges address decoded cell bytes;
    /// their physical ranges address original JSON tokens and are checked for ordering.
    ///
    /// # Errors
    ///
    /// Returns a typed refusal for invalid identities, origins, formats, digests, or bounds.
    pub fn new(
        source: DocumentationSource,
        text: &'source str,
    ) -> Result<Self, DocumentationError> {
        validate_source(&source, text)?;
        Ok(Self {
            source,
            text,
            #[cfg(feature = "collector")]
            syntax: None,
            chunks: Vec::new(),
        })
    }

    /// Returns the accepted source facts.
    #[must_use]
    pub const fn source(&self) -> &DocumentationSource {
        &self.source
    }

    /// Returns unmodified bytes as UTF-8 text.
    #[must_use]
    pub const fn text(&self) -> &'source str {
        self.text
    }

    /// Reuses facts from the syntax pass that owns this source's declarations.
    #[cfg(feature = "collector")]
    ///
    /// # Errors
    ///
    /// Returns a refusal if source path, language, or bytes do not match syntax facts.
    pub fn with_syntax(
        mut self,
        syntax: &'source rift_syntax::SyntaxDocument,
    ) -> Result<Self, DocumentationError> {
        if !syntax_matches_source(&self.source, self.text, syntax) {
            return Err(refused(DocumentationViolation::Format, "syntax_source"));
        }
        self.syntax = Some(syntax);
        Ok(self)
    }

    /// Records the existing baseline content partition for this owner.
    ///
    /// # Errors
    ///
    /// Refuses gaps, overlapping ranges, duplicate identities, or incomplete partitions.
    pub fn with_chunks(
        mut self,
        chunks: Vec<rift_protocol::documentation::DocumentationChunk>,
    ) -> Result<Self, DocumentationError> {
        if chunks.len() > rift_protocol::documentation::DOCUMENTATION_BLOCKS_MAX as usize {
            return Err(refused(DocumentationViolation::LimitExceeded, "chunks"));
        }
        let mut end = 0;
        let mut identities = BTreeSet::new();
        for chunk in &chunks {
            let accepted_identity = rift_ranking::DocumentIdentity::new(&chunk.identity).is_ok()
                && identities.insert(&chunk.identity);
            let start = usize::try_from(chunk.range.start).ok();
            let stop = usize::try_from(chunk.range.end).ok();
            let valid_range = chunk.range.start == end
                && chunk.range.end > end
                && chunk.range.end <= self.source.byte_length;
            let valid_boundary = start.is_some_and(|offset| self.text.is_char_boundary(offset))
                && stop.is_some_and(|offset| self.text.is_char_boundary(offset));
            if !accepted_identity || !valid_range || !valid_boundary {
                return Err(refused(DocumentationViolation::Range, "chunks"));
            }
            end = chunk.range.end;
        }
        if end != self.source.byte_length {
            return Err(refused(DocumentationViolation::Range, "chunks"));
        }
        self.chunks = chunks;
        Ok(self)
    }

    #[cfg(feature = "collector")]
    pub(super) fn syntax(&self) -> Option<&'source rift_syntax::SyntaxDocument> {
        self.syntax
    }

    #[cfg(feature = "collector")]
    pub(super) fn chunks(&self) -> &[rift_protocol::documentation::DocumentationChunk] {
        &self.chunks
    }
}

#[cfg(feature = "collector")]
fn syntax_matches_source(
    source: &DocumentationSource,
    text: &str,
    syntax: &rift_syntax::SyntaxDocument,
) -> bool {
    let source_path = match &source.identity.source {
        DocumentationSourceIdentity::Project { path } => path.0.clone(),
        DocumentationSourceIdentity::Package { unit } => {
            let Ok(parsed) = SourceUnitId::parse(&unit.0) else {
                return false;
            };
            let Some(package) = &source.origin.package else {
                return false;
            };
            let prefix = format!("{}@{}/", package.name, package.version);
            let Some(path) = parsed.key().as_str().strip_prefix(&prefix) else {
                return false;
            };
            path.to_owned()
        }
    };
    let source_digest_matches = syntax
        .source_digest()
        .is_some_and(|digest| *digest == rift_core::FileDigest::of(text.as_bytes()));
    source_path == syntax.path().as_str()
        && source
            .language
            .as_ref()
            .is_none_or(|language| language == syntax.language())
        && source_digest_matches
}

/// A validated source set sorted by canonical content identity.
#[derive(Debug)]
pub struct DocumentationSourceSet<'source> {
    sources: Vec<DocumentationInput<'source>>,
    selection_digest: DocumentationDigest,
}

impl<'source> DocumentationSourceSet<'source> {
    /// Validates aggregate bounds and rejects duplicate source identities.
    ///
    /// Work is O(n log n) for at most `DOCUMENTATION_SOURCES_MAX` sources.
    /// Empty source sets are accepted.
    ///
    /// # Errors
    ///
    /// Returns a typed refusal for duplicate identities or aggregate bounds.
    pub fn new(mut sources: Vec<DocumentationInput<'source>>) -> Result<Self, DocumentationError> {
        if sources.len() > DOCUMENTATION_SOURCES_MAX as usize {
            return Err(refused(DocumentationViolation::LimitExceeded, "sources"));
        }
        sources.sort_by(|left, right| left.source.identity.cmp(&right.source.identity));
        let mut bytes = 0_u64;
        let mut previous = None;
        for source in &sources {
            bytes = bytes
                .checked_add(source.source.byte_length)
                .filter(|total| *total <= DOCUMENTATION_TOTAL_BYTES_MAX)
                .ok_or_else(|| refused(DocumentationViolation::LimitExceeded, "source_bytes"))?;
            if previous == Some(&source.source.identity) {
                return Err(refused(DocumentationViolation::DuplicateSource, "identity"));
            }
            previous = Some(&source.source.identity);
        }
        let selection_digest = source_selection_digest(sources.iter().map(|input| &input.source))?;
        Ok(Self {
            sources,
            selection_digest,
        })
    }

    /// Returns selected sources in canonical identity order.
    #[must_use]
    pub fn sources(&self) -> &[DocumentationInput<'source>] {
        &self.sources
    }

    /// Returns the digest of canonical selected source facts.
    #[must_use]
    pub const fn selection_digest(&self) -> &DocumentationDigest {
        &self.selection_digest
    }
}

fn validate_source(source: &DocumentationSource, text: &str) -> Result<(), DocumentationError> {
    validate_source_metadata(source)?;
    let size_accepted = text.len() <= DOCUMENTATION_SOURCE_BYTES_MAX as usize;
    let length_matches = source.byte_length == text.len() as u64;
    let digest_matches = source.content_digest == content_digest(text.as_bytes());
    match () {
        () if !size_accepted => Err(refused(
            DocumentationViolation::LimitExceeded,
            "source_bytes",
        )),
        () if !length_matches => Err(refused(DocumentationViolation::Range, "byte_length")),
        () if !digest_matches => Err(refused(DocumentationViolation::Digest, "content_digest")),
        () => Ok(()),
    }
}

pub(super) fn validate_source_metadata(
    source: &DocumentationSource,
) -> Result<(), DocumentationError> {
    validate_identity(&source.identity)?;
    validate_origin(source)?;
    validate_format(source)?;
    validate_selection(source)?;
    validate_license(source)?;
    if source.byte_length > u64::from(DOCUMENTATION_SOURCE_BYTES_MAX) {
        return Err(refused(
            DocumentationViolation::LimitExceeded,
            "source_bytes",
        ));
    }
    if !is_digest(&source.revision) || !is_digest(&source.content_digest) {
        return Err(refused(DocumentationViolation::Digest, "source.digest"));
    }
    let language_valid = source.language.as_ref().is_none_or(|language| {
        Language::from_identity_segment(&language.identity_segment()).is_ok()
    });
    if !language_valid {
        return Err(refused(DocumentationViolation::Format, "source.language"));
    }
    validate_physical_ranges(source)
}

pub(super) fn source_selection_digest<'source>(
    sources: impl IntoIterator<Item = &'source DocumentationSource>,
) -> Result<DocumentationDigest, DocumentationError> {
    let selected: Vec<_> = sources
        .into_iter()
        .map(|source| {
            (
                &source.identity,
                &source.content_digest,
                source.format,
                &source.origin,
            )
        })
        .collect();
    canonical_digest(&selected)
}

pub(super) fn validate_identity(
    identity: &DocumentationContentIdentity,
) -> Result<(), DocumentationError> {
    let accepted = match &identity.source {
        DocumentationSourceIdentity::Project { path } => {
            !path.0.is_empty() && ProjectPath::new(&path.0).is_ok()
        }
        DocumentationSourceIdentity::Package { unit } => SourceUnitId::parse(&unit.0).is_ok(),
    };
    if !accepted {
        return Err(refused(DocumentationViolation::Identity, "source"));
    }
    let Some(cell) = &identity.cell else {
        return Ok(());
    };
    if let NotebookCellIdentity::Authored { id } = &cell.identity {
        let valid_length = !id.is_empty() && id.len() <= NOTEBOOK_CELL_ID_BYTES_MAX as usize;
        let valid_characters = id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte));
        if !valid_length || !valid_characters {
            return Err(refused(DocumentationViolation::Notebook, "cell.identity"));
        }
    }
    Ok(())
}

fn validate_origin(source: &DocumentationSource) -> Result<(), DocumentationError> {
    let origin = &source.origin;
    let authored_location =
        origin.source_kind != SourceKind::Synthetic && origin.location.is_some();
    let synthetic_location =
        origin.source_kind == SourceKind::Synthetic && origin.location.is_none();
    let package_location = matches!(
        origin.location,
        Some(SourceLocationKind::Project | SourceLocationKind::Dependency)
    );
    let package_required = origin.location == Some(SourceLocationKind::Dependency);
    if !(authored_location || synthetic_location)
        || (origin.package.is_some() && !package_location)
        || (package_required && origin.package.is_none())
    {
        return Err(refused(DocumentationViolation::Origin, "origin"));
    }
    match &source.identity.source {
        DocumentationSourceIdentity::Project { .. }
            if origin.location != Some(SourceLocationKind::Project) =>
        {
            Err(refused(DocumentationViolation::Origin, "origin.location"))
        }
        DocumentationSourceIdentity::Package { .. }
            if origin.location != Some(SourceLocationKind::Dependency) =>
        {
            Err(refused(DocumentationViolation::Origin, "origin.location"))
        }
        DocumentationSourceIdentity::Package { unit } => validate_package_origin(unit, source),
        DocumentationSourceIdentity::Project { .. } => Ok(()),
    }
}

fn validate_selection(source: &DocumentationSource) -> Result<(), DocumentationError> {
    use DocumentationSelectionReason as Selection;
    let valid = match source.selection {
        Selection::Workspace => {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Project { .. }
            ) && source.format != DocumentationSourceFormat::AttachedComment
        }
        Selection::PackageArchive | Selection::CloudResolver => {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Package { .. }
            ) && source.format != DocumentationSourceFormat::AttachedComment
        }
        Selection::AttachedComment => source.format == DocumentationSourceFormat::AttachedComment,
    };
    if valid {
        Ok(())
    } else {
        Err(refused(DocumentationViolation::Origin, "selection"))
    }
}

fn validate_package_origin(
    unit: &rift_protocol::read::SourceUnitId,
    source: &DocumentationSource,
) -> Result<(), DocumentationError> {
    let parsed = SourceUnitId::parse(&unit.0)
        .map_err(|_| refused(DocumentationViolation::Identity, "source.unit"))?;
    let Some(package) = &source.origin.package else {
        return Err(refused(DocumentationViolation::Origin, "origin.package"));
    };
    let key_prefix = format!("{}@{}/", package.name, package.version);
    let manager_matches = parsed.resolver().as_str() == package.manager;
    let package_matches = parsed.key().as_str().starts_with(&key_prefix);
    if !manager_matches || !package_matches {
        return Err(refused(DocumentationViolation::Origin, "origin.package"));
    }
    let path = parsed
        .key()
        .as_str()
        .strip_prefix(&key_prefix)
        .unwrap_or_default();
    ProjectPath::new(path).map_err(|_| refused(DocumentationViolation::Identity, "source.unit"))?;
    if path.is_empty() {
        return Err(refused(DocumentationViolation::Identity, "source.unit"));
    }
    Ok(())
}

fn validate_format(source: &DocumentationSource) -> Result<(), DocumentationError> {
    let path = source_path(&source.identity)?;
    let extension = path.rsplit_once('.').map_or("", |(_, extension)| extension);
    let accepted = match source.format {
        DocumentationSourceFormat::Markdown => {
            ["md", "markdown"].contains(&extension) && source.media_type == "text/markdown"
        }
        DocumentationSourceFormat::Mdx => extension == "mdx" && source.media_type == "text/mdx",
        DocumentationSourceFormat::RestructuredText => {
            extension == "rst" && source.media_type == "text/x-rst"
        }
        DocumentationSourceFormat::Text => extension == "txt" && source.media_type == "text/plain",
        DocumentationSourceFormat::Notebook => {
            extension == "ipynb" && source.media_type == "application/x-ipynb+json"
        }
        DocumentationSourceFormat::AttachedComment => {
            source.media_type == "text/markdown" || source.media_type == "text/plain"
        }
    };
    if !accepted {
        return Err(refused(DocumentationViolation::Format, "format"));
    }
    let cell_accepted =
        source.identity.cell.is_some() == (source.format == DocumentationSourceFormat::Notebook);
    if !cell_accepted {
        return Err(refused(DocumentationViolation::Notebook, "cell"));
    }
    Ok(())
}

pub(super) fn source_path(
    identity: &DocumentationContentIdentity,
) -> Result<String, DocumentationError> {
    match &identity.source {
        DocumentationSourceIdentity::Project { path } => Ok(path.0.clone()),
        DocumentationSourceIdentity::Package { unit } => SourceUnitId::parse(&unit.0)
            .map(|unit| unit.key().as_str().to_owned())
            .map_err(|_| refused(DocumentationViolation::Identity, "source.unit")),
    }
}

fn validate_physical_ranges(source: &DocumentationSource) -> Result<(), DocumentationError> {
    if source.physical_ranges.len() > NOTEBOOK_SOURCE_RANGES_MAX as usize {
        return Err(refused(
            DocumentationViolation::LimitExceeded,
            "physical_ranges",
        ));
    }
    if source.identity.cell.is_none() && !source.physical_ranges.is_empty() {
        return Err(refused(DocumentationViolation::Notebook, "physical_ranges"));
    }
    let mut previous_end = 0;
    for range in &source.physical_ranges {
        let ordered = range.start >= previous_end;
        let nonempty = range.end > range.start;
        let bounded = range.end <= u64::from(DOCUMENTATION_SOURCE_BYTES_MAX);
        if !ordered || !nonempty || !bounded {
            return Err(refused(DocumentationViolation::Range, "physical_ranges"));
        }
        previous_end = range.end;
    }
    Ok(())
}

fn validate_license(source: &DocumentationSource) -> Result<(), DocumentationError> {
    let Some(license) = &source.license else {
        return Ok(());
    };
    let valid_expression = license
        .expression
        .as_ref()
        .is_none_or(|text| !text.is_empty() && text.len() <= DOCUMENTATION_TEXT_BYTES_MAX as usize);
    if !valid_expression || license.files.len() > DOCUMENTATION_LICENSE_FILES_MAX as usize {
        return Err(refused(DocumentationViolation::LimitExceeded, "license"));
    }
    let mut paths = BTreeSet::new();
    for file in &license.files {
        let valid_path = !file.path.0.is_empty() && ProjectPath::new(&file.path.0).is_ok();
        let valid_digest = is_digest(&file.digest);
        if !valid_path || !valid_digest || !paths.insert(&file.path) {
            return Err(refused(DocumentationViolation::Identity, "license.files"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rift_protocol::documentation::DocumentationDigest;
    use rift_protocol::documentation::{
        DocumentationContentIdentity, DocumentationLicense, DocumentationLicenseFile,
        DocumentationSelectionReason, DocumentationSource, DocumentationSourceFormat,
        DocumentationSourceIdentity, NotebookCell, NotebookCellIdentity, NotebookCellKind,
    };
    use rift_protocol::read::{
        PackageIdentity, ProjectPath, SourceKind, SourceLocationKind, SourceUnitId, SymbolOrigin,
        TextRange,
    };

    use super::{DocumentationInput, DocumentationSourceSet};
    use crate::documentation::{DocumentationViolation, content_digest};

    fn source(path: &str, text: &str) -> DocumentationSource {
        DocumentationSource {
            identity: DocumentationContentIdentity {
                source: DocumentationSourceIdentity::Project {
                    path: ProjectPath(path.to_owned()),
                },
                cell: None,
            },
            revision: content_digest(b"tree"),
            content_digest: content_digest(text.as_bytes()),
            origin: SymbolOrigin {
                location: Some(SourceLocationKind::Project),
                package: None,
                source_kind: SourceKind::Authored,
            },
            format: DocumentationSourceFormat::Markdown,
            media_type: "text/markdown".to_owned(),
            selection: DocumentationSelectionReason::Workspace,
            byte_length: text.len() as u64,
            language: None,
            physical_ranges: Vec::new(),
            license: None,
        }
    }

    fn violation(source: DocumentationSource, text: &str) -> DocumentationViolation {
        DocumentationInput::new(source, text)
            .expect_err("input refused")
            .fault()
            .violation()
    }

    #[test]
    fn test_source_keeps_exact_bytes_and_line_endings() {
        for text in [
            "# Source\n\nText.\n",
            "# Source\r\n\r\nText.\r\n",
            "# Source\n\nText.",
            "",
        ] {
            let input =
                DocumentationInput::new(source("README.md", text), text).expect("valid source");
            assert_eq!(input.text().as_bytes(), text.as_bytes());
            assert_eq!(
                input.source().content_digest,
                content_digest(text.as_bytes())
            );
        }
    }

    #[test]
    fn test_selection_is_independent_of_input_order() {
        let first = DocumentationInput::new(source("a.md", "a"), "a").expect("first");
        let second = DocumentationInput::new(source("b.md", "b"), "b").expect("second");
        let forward =
            DocumentationSourceSet::new(vec![first.clone(), second.clone()]).expect("forward");
        let reverse = DocumentationSourceSet::new(vec![second, first]).expect("reverse");
        assert_eq!(forward.selection_digest(), reverse.selection_digest());
        assert_eq!(forward.sources()[0].source(), reverse.sources()[0].source());
        assert_eq!(forward.sources()[1].source(), reverse.sources()[1].source());
    }

    #[test]
    fn test_empty_set_is_valid_and_duplicate_source_is_refused() {
        let empty = DocumentationSourceSet::new(Vec::new()).expect("empty");
        assert!(empty.sources().is_empty());
        let input = DocumentationInput::new(source("a.md", "a"), "a").expect("input");
        let error = DocumentationSourceSet::new(vec![input.clone(), input]).expect_err("duplicate");
        assert_eq!(
            error.fault().violation(),
            DocumentationViolation::DuplicateSource
        );
    }

    #[test]
    fn test_source_refuses_noncanonical_and_empty_paths() {
        for path in [
            "",
            "../README.md",
            "/README.md",
            "docs\\README.md",
            "docs//README.md",
            ".rift/README.md",
            "docs/./README.md",
        ] {
            assert_eq!(
                violation(source(path, "a"), "a"),
                DocumentationViolation::Identity,
                "{path}"
            );
        }
    }

    #[test]
    fn test_source_refuses_digest_length_and_revision_mismatch() {
        let mut record = source("README.md", "a");
        record.content_digest = content_digest(b"b");
        assert_eq!(violation(record, "a"), DocumentationViolation::Digest);
        let mut record = source("README.md", "a");
        record.byte_length = 2;
        assert_eq!(violation(record, "a"), DocumentationViolation::Range);
        let mut record = source("README.md", "a");
        record.revision = DocumentationDigest("invalid".to_owned());
        assert_eq!(violation(record, "a"), DocumentationViolation::Digest);
    }

    #[test]
    fn test_source_byte_bound_accepts_exact_and_refuses_one_over() {
        let bound = rift_protocol::documentation::DOCUMENTATION_SOURCE_BYTES_MAX as usize;
        let exact = "a".repeat(bound);
        assert!(DocumentationInput::new(source("README.md", &exact), &exact).is_ok());
        let over = "a".repeat(bound + 1);
        assert_eq!(
            violation(source("README.md", &over), &over),
            DocumentationViolation::LimitExceeded
        );
    }

    #[test]
    fn test_format_requires_matching_extension_and_media_type() {
        let cases = [
            (
                "guide.md",
                DocumentationSourceFormat::Markdown,
                "text/markdown",
            ),
            (
                "guide.markdown",
                DocumentationSourceFormat::Markdown,
                "text/markdown",
            ),
            ("guide.mdx", DocumentationSourceFormat::Mdx, "text/mdx"),
            (
                "guide.rst",
                DocumentationSourceFormat::RestructuredText,
                "text/x-rst",
            ),
            ("guide.txt", DocumentationSourceFormat::Text, "text/plain"),
            (
                "guide.ipynb",
                DocumentationSourceFormat::Notebook,
                "application/x-ipynb+json",
            ),
            (
                "lib.rs",
                DocumentationSourceFormat::AttachedComment,
                "text/markdown",
            ),
        ];
        for (path, format, media_type) in cases {
            let mut record = source(path, "a");
            record.format = format;
            record.media_type = media_type.to_owned();
            if format == DocumentationSourceFormat::Notebook {
                record.identity.cell = Some(NotebookCell {
                    identity: NotebookCellIdentity::Indexed { index: 0 },
                    kind: NotebookCellKind::Markdown,
                });
            }
            if format == DocumentationSourceFormat::AttachedComment {
                record.selection = DocumentationSelectionReason::AttachedComment;
            }
            assert!(
                DocumentationInput::new(record.clone(), "a").is_ok(),
                "{path}"
            );
            record.media_type = "text/html".to_owned();
            assert_eq!(violation(record, "a"), DocumentationViolation::Format);
        }
        assert_eq!(
            violation(source("README.txt", "a"), "a"),
            DocumentationViolation::Format
        );
    }

    fn package_source(text: &str) -> DocumentationSource {
        let mut record = source("README.md", text);
        record.identity.source = DocumentationSourceIdentity::Package {
            unit: SourceUnitId("rift://source/cargo/beacon@1.0.0/README.md".to_owned()),
        };
        record.origin = SymbolOrigin {
            location: Some(SourceLocationKind::Dependency),
            package: Some(PackageIdentity {
                manager: "cargo".to_owned(),
                name: "beacon".to_owned(),
                version: "1.0.0".to_owned(),
            }),
            source_kind: SourceKind::Authored,
        };
        record.selection = DocumentationSelectionReason::PackageArchive;
        record
    }

    #[test]
    fn test_package_address_and_origin_must_describe_same_exact_package() {
        let record = package_source("a");
        assert!(DocumentationInput::new(record.clone(), "a").is_ok());
        let mut changed = record.clone();
        changed.origin.package.as_mut().expect("package").version = "2.0.0".to_owned();
        assert_eq!(violation(changed, "a"), DocumentationViolation::Origin);
        let mut changed = record.clone();
        changed.origin.package = None;
        assert_eq!(violation(changed, "a"), DocumentationViolation::Origin);
        let mut changed = record;
        changed.origin.source_kind = SourceKind::Synthetic;
        assert_eq!(violation(changed, "a"), DocumentationViolation::Origin);
    }

    #[test]
    fn test_notebook_cell_identity_and_physical_ranges_are_checked() {
        let mut record = source("guide.ipynb", "cell");
        record.format = DocumentationSourceFormat::Notebook;
        record.media_type = "application/x-ipynb+json".to_owned();
        record.identity.cell = Some(NotebookCell {
            identity: NotebookCellIdentity::Authored {
                id: "cell-1".to_owned(),
            },
            kind: NotebookCellKind::Markdown,
        });
        record.physical_ranges = vec![
            TextRange { start: 20, end: 26 },
            TextRange { start: 28, end: 36 },
        ];
        assert!(DocumentationInput::new(record.clone(), "cell").is_ok());
        let mut changed = record.clone();
        changed.physical_ranges.reverse();
        assert_eq!(violation(changed, "cell"), DocumentationViolation::Range);
        let mut changed = record.clone();
        changed.identity.cell.as_mut().expect("cell").identity = NotebookCellIdentity::Authored {
            id: "bad cell".to_owned(),
        };
        assert_eq!(violation(changed, "cell"), DocumentationViolation::Notebook);
        let mut changed = record;
        changed.identity.cell = None;
        assert_eq!(violation(changed, "cell"), DocumentationViolation::Notebook);
    }

    #[test]
    fn test_license_paths_and_digests_are_validated() {
        let mut record = source("README.md", "a");
        let file = DocumentationLicenseFile {
            path: ProjectPath("LICENSE".to_owned()),
            digest: content_digest(b"license"),
        };
        record.license = Some(DocumentationLicense {
            expression: Some("MIT".to_owned()),
            files: vec![file.clone()],
        });
        assert!(DocumentationInput::new(record.clone(), "a").is_ok());
        record.license.as_mut().expect("license").files.push(file);
        assert_eq!(violation(record, "a"), DocumentationViolation::Identity);
    }
}
