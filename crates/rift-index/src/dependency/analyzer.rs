//! The package analyzer: one package's source bytes into one canonical publication.
//!
//! [`PackageAnalyzer`] holds no I/O. It takes the bytes a caller already read, parses
//! them with the shipped syntax providers, places each file under the package's own
//! identity, assembles the declarations through [`WorkspaceSemantics`], and renders the
//! result as a [`PackagePublication`]. The local package index consumes that publication,
//! and a later global ingestion consumes the same shape, so one extraction serves both.
//!
//! Every record carries the digest of its own canonical content, and the publication as a
//! whole renders as RFC 8785 canonical JSON, so two runs over the same bytes under the
//! same analyzer revision compare byte for byte.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;

use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_core::line::lines_inclusive;
use rift_core::{
    ContributionOrigin, ProjectPath as CoreProjectPath, SourceKind as CoreSourceKind,
    SourceLocation, SourceUnitId as CoreSourceUnitId, symbol_identity,
};
use rift_dependency::{CatalogEntry, PackageLocation};
use rift_protocol::canonical::canonical_json;
use rift_protocol::index::{
    PACKAGE_DOCUMENTS_MAX, PACKAGE_IDENTIFIER_TERMS_MAX, PACKAGE_PUBLICATION_FORMAT_REVISION,
    PACKAGE_SOURCE_BYTES_MAX, PACKAGE_SYMBOLS_MAX, PACKAGE_UNITS_MAX, PACKAGE_WARNINGS_MAX,
    PackageAnalysisWarning, PackageDocument, PackageDocumentKind, PackagePublication,
    PackageSourceUnit, PackageSymbol,
};
use rift_protocol::read::{
    Digest, ExactKind, Language, PackageIdentity, ProjectPath, SourceKind, SourceLocationKind,
    SourceUnitId, SymbolId, SymbolOrigin, TextRange,
};
use rift_syntax::{DocumentPlacement, SyntaxSymbol};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::failure::{
    PackageIndexError, PackageIndexFault, PackageIndexViolation, package_segment,
};
use super::manifest::analyzer_revision;
use super::walk::{PackageFiles, public_qualified_names};
use crate::lexical::split_identifier_words;
use crate::semantic::{PlacedDocument, WorkspaceSemantics};
use crate::workspace::{IndexedFile, TextSourceFile, indexed_file_from_catalog};

/// One package file as the analyzer holds it: the parsed document, where it is filed, and
/// the public declarations it carries.
#[derive(Debug)]
pub struct AnalyzedFile {
    file: IndexedFile,
    placement: DocumentPlacement,
    public_names: BTreeSet<String>,
}

impl AnalyzedFile {
    /// The parsed file.
    #[must_use]
    pub const fn file(&self) -> &IndexedFile {
        &self.file
    }

    /// The unit, origin, and identity path the file's declarations carry.
    #[must_use]
    pub const fn placement(&self) -> &DocumentPlacement {
        &self.placement
    }

    /// Whether the package exports the declaration spelled by `qualified_name`.
    #[must_use]
    pub fn is_public(&self, qualified_name: &str) -> bool {
        self.public_names.contains(qualified_name)
    }
}

/// What one analyzer run produced: the canonical publication, and the parsed material the
/// same pass built it from.
///
/// The publication is the artifact a consumer stores and compares. The parsed files and
/// the assembled graph are the same pass's working values, handed on so the local package
/// index answers reads without parsing the package a second time.
#[derive(Debug)]
pub struct PackageAnalysis {
    publication: PackagePublication,
    files: Vec<AnalyzedFile>,
    semantics: WorkspaceSemantics,
}

impl PackageAnalysis {
    /// The canonical publication this run produced.
    #[must_use]
    pub const fn publication(&self) -> &PackagePublication {
        &self.publication
    }

    /// Every analyzed file, in path order.
    #[must_use]
    pub fn files(&self) -> &[AnalyzedFile] {
        &self.files
    }

    /// Takes the run apart for a consumer that keeps every part.
    pub(super) fn into_parts(self) -> (PackagePublication, Vec<AnalyzedFile>, WorkspaceSemantics) {
        (self.publication, self.files, self.semantics)
    }
}

/// Turns one package's source bytes into one canonical publication.
#[derive(Debug, Default)]
pub struct PackageAnalyzer;

impl PackageAnalyzer {
    /// Analyzes one cataloged package's selected files.
    ///
    /// Each file is placed under `rift://source/<manager>/<name>@<version>/<path>` with
    /// the identity path `<manager>/<name>@<version>/<path>`, and its origin is the
    /// entry's location: a dependency carrying the package identity, or the standard
    /// library. Records are emitted in unit, symbol, and document identity order, and a
    /// collection that reaches its bound stops there and reports the stop as a warning.
    ///
    /// The work is proportional to the selected bytes: one parse per file, one scan per
    /// file for its line starts, one assembly pass over the parsed declarations, and one
    /// canonical rendering per record.
    ///
    /// # Errors
    ///
    /// Returns [`PackageIndexError`] when the identity cannot spell a resolver or unit,
    /// when no provider parses a file or its parse fails, or when publication or
    /// normalization refuses the package graph.
    pub fn analyze(
        entry: &CatalogEntry,
        files: &PackageFiles,
        revision: u64,
    ) -> Result<PackageAnalysis, PackageIndexError> {
        let package = entry.identity();
        let mut analyzed = Vec::with_capacity(files.file_count());
        for file in files.files() {
            let parsed = parsed_file(file, package)?;
            let placement = placement_of(entry, file.path())?;
            let public_names = public_qualified_names(entry.language(), parsed.syntax());
            analyzed.push(AnalyzedFile {
                file: parsed,
                placement,
                public_names,
            });
        }
        analyzed.sort_by(|left, right| left.file.path().cmp(right.file.path()));
        let placed: Vec<PlacedDocument<'_>> = analyzed
            .iter()
            .map(|held| PlacedDocument {
                document: held.file.syntax(),
                placement: held.placement.clone(),
            })
            .collect();
        let semantics =
            WorkspaceSemantics::build_placed(&placed, revision, None).map_err(|error| {
                PackageIndexFault::new(PackageIndexViolation::Provider, package).caused_by(error)
            })?;
        let publication = publish(entry, &analyzed)?;
        Ok(PackageAnalysis {
            publication,
            files: analyzed,
            semantics,
        })
    }
}

/// Renders the canonical publication over the analyzed files.
///
/// One pass over the files emits every record: a file's unit, the declarations it
/// carries, and the documents a ranking reads. Each collection is sorted by its own
/// identity afterwards, so the publication's order is the identities' order and not the
/// order the files arrived in.
fn publish(
    entry: &CatalogEntry,
    analyzed: &[AnalyzedFile],
) -> Result<PackagePublication, PackageIndexError> {
    let package = entry.identity();
    let origin = symbol_origin(entry);
    let mut records = Records::default();
    for held in analyzed {
        if records.units.len() >= bound(PACKAGE_UNITS_MAX) {
            records.warn(truncation("units", u64::from(PACKAGE_UNITS_MAX)));
            break;
        }
        records.file(package, &origin, held)?;
    }
    records
        .units
        .sort_by(|left, right| left.unit.0.cmp(&right.unit.0));
    records
        .symbols
        .sort_by(|left, right| left.symbol.0.cmp(&right.symbol.0));
    records
        .documents
        .sort_by(|left, right| (left.kind, &left.identity).cmp(&(right.kind, &right.identity)));
    let source_digest = digest_of(
        &SourceFingerprint {
            units: records
                .units
                .iter()
                .map(|unit| (unit.path.0.as_str(), unit.content_digest.0.as_str()))
                .collect(),
        },
        package,
    )?;
    Ok(PackagePublication {
        format_revision: PACKAGE_PUBLICATION_FORMAT_REVISION,
        analyzer_revision: analyzer_revision(),
        package: package.clone(),
        source_digest,
        units: records.units,
        symbols: records.symbols,
        documents: records.documents,
        warnings: records.warnings,
    })
}

/// The bytes a publication's `source_digest` covers: every unit's path and content digest,
/// in unit order.
#[derive(Serialize)]
struct SourceFingerprint<'analysis> {
    units: Vec<(&'analysis str, &'analysis str)>,
}

/// The records one publication is assembled from, and what the assembly could not do.
///
/// A collection that reaches its bound reports the cut once. Every record past the bound
/// meets the same full collection, so warning per record would fill the warning list with
/// one repeated entry and drop everything else the analysis met.
#[derive(Default)]
struct Records {
    units: Vec<PackageSourceUnit>,
    symbols: Vec<PackageSymbol>,
    documents: Vec<PackageDocument>,
    warnings: Vec<PackageAnalysisWarning>,
    symbols_truncated: bool,
    documents_truncated: bool,
}

impl Records {
    /// Records one warning, dropping it once the publication's warning bound is reached:
    /// the bound is what a reader can act on, and the log carries the rest.
    fn warn(&mut self, warning: PackageAnalysisWarning) {
        if self.warnings.len() < bound(PACKAGE_WARNINGS_MAX) {
            self.warnings.push(warning);
        }
    }

    /// Emits one file's unit record, its declarations, and the documents they rank under.
    fn file(
        &mut self,
        package: &PackageIdentity,
        origin: &SymbolOrigin,
        held: &AnalyzedFile,
    ) -> Result<(), PackageIndexError> {
        let language = held.file.syntax().language().clone();
        let path = wire_path(held.file.path());
        let unit = wire_unit(held.placement.unit());
        let source = held.file.source();
        let retained = self.retained(source, &path);
        let content_digest = text_digest(source);
        let mut record = PackageSourceUnit {
            unit: unit.clone(),
            path: path.clone(),
            language: language.clone(),
            content_digest: content_digest.clone(),
            source: retained.text.clone(),
            source_complete: retained.complete,
            digest: Digest(String::new()),
        };
        record.digest = digest_of(&record, package)?;
        self.units.push(record);
        let document_digest = text_digest(&retained.text);
        self.file_document(package, &language, &unit, &path, retained, document_digest)?;
        let context = FileContext {
            language: &language,
            unit: &unit,
            path: &path,
            line_starts: &line_starts(source),
        };
        for declaration in held.file.syntax().symbols() {
            self.declaration(package, origin, held, &context, declaration)?;
        }
        Ok(())
    }

    /// Emits one file's own search document, ranked by its content.
    fn file_document(
        &mut self,
        package: &PackageIdentity,
        language: &Language,
        unit: &SourceUnitId,
        path: &ProjectPath,
        retained: RetainedSource,
        content_digest: Digest,
    ) -> Result<(), PackageIndexError> {
        let name = file_name(path);
        let document = PackageDocument {
            identity: unit.0.clone(),
            kind: PackageDocumentKind::File,
            unit: unit.clone(),
            language: language.clone(),
            package: package.clone(),
            content_digest,
            identifier_terms: identifier_terms(&[&name]),
            name,
            qualified_name: None,
            signature: None,
            documentation: None,
            declaration_source: None,
            file_content: Some(retained.text),
            digest: Digest(String::new()),
        };
        self.document(document)
    }

    /// Emits one declaration's record, and its search document when the package exports
    /// it.
    fn declaration(
        &mut self,
        package: &PackageIdentity,
        origin: &SymbolOrigin,
        held: &AnalyzedFile,
        context: &FileContext<'_>,
        declaration: &SyntaxSymbol,
    ) -> Result<(), PackageIndexError> {
        let FileContext {
            language,
            unit,
            path,
            line_starts,
        } = *context;
        if self.symbols.len() >= bound(PACKAGE_SYMBOLS_MAX) {
            if !self.symbols_truncated {
                self.symbols_truncated = true;
                self.warn(truncation("symbols", u64::from(PACKAGE_SYMBOLS_MAX)));
            }
            return Ok(());
        }
        let source = held.file.source();
        let declared = declaration_source(source, declaration).ok_or_else(|| {
            PackageIndexError::from(
                PackageIndexFault::new(PackageIndexViolation::Provider, package)
                    .at(Path::new(path.0.as_str())),
            )
        })?;
        let retained = self.retained(declared, path);
        let content_digest = text_digest(&retained.text);
        let mut record = PackageSymbol {
            symbol: SymbolId(symbol_identity(
                &language.identity_segment(),
                held.placement.identity_path(),
                &declaration.qualified_name,
            )),
            origin: origin.clone(),
            unit: unit.clone(),
            name: declaration.name.clone(),
            qualified_name: declaration.qualified_name.clone(),
            kind: ExactKind(format!(
                "{}.{}",
                language.identity_segment(),
                declaration.kind
            )),
            range: TextRange {
                start: declaration.range.start,
                end: declaration.range.end,
            },
            line: line_of(line_starts, declaration.range.start),
            signature: declaration.signatures.first().cloned(),
            documentation: declaration.documentation.first().cloned(),
            source: retained.text,
            source_complete: retained.complete,
            public: held.is_public(&declaration.qualified_name),
            digest: Digest(String::new()),
        };
        record.digest = digest_of(&record, package)?;
        if record.public {
            let document = PackageDocument {
                identity: record.symbol.0.clone(),
                kind: PackageDocumentKind::Symbol,
                unit: unit.clone(),
                language: language.clone(),
                package: package.clone(),
                content_digest,
                identifier_terms: identifier_terms(&[&record.name, &record.qualified_name]),
                name: record.name.clone(),
                qualified_name: Some(record.qualified_name.clone()),
                signature: record
                    .signature
                    .as_ref()
                    .map(|signature| signature.display.clone()),
                documentation: record
                    .documentation
                    .as_ref()
                    .map(|documentation| documentation.text.clone()),
                declaration_source: Some(record.source.clone()),
                file_content: None,
                digest: Digest(String::new()),
            };
            self.document(document)?;
        }
        self.symbols.push(record);
        Ok(())
    }

    /// Takes one document, or reports the bound once the collection reaches it.
    fn document(&mut self, mut document: PackageDocument) -> Result<(), PackageIndexError> {
        if self.documents.len() >= bound(PACKAGE_DOCUMENTS_MAX) {
            if !self.documents_truncated {
                self.documents_truncated = true;
                self.warn(truncation("documents", u64::from(PACKAGE_DOCUMENTS_MAX)));
            }
            return Ok(());
        }
        document.digest = digest_of(&document, &document.package.clone())?;
        self.documents.push(document);
        Ok(())
    }

    /// The bytes one record retains, reporting a cut once for the record that made it.
    fn retained(&mut self, source: &str, path: &ProjectPath) -> RetainedSource {
        let bound = bound(PACKAGE_SOURCE_BYTES_MAX);
        if source.len() <= bound {
            return RetainedSource {
                text: source.to_owned(),
                complete: true,
            };
        }
        let mut kept = bound;
        while kept > 0 && !source.is_char_boundary(kept) {
            kept -= 1;
        }
        self.warn(PackageAnalysisWarning::SourceTruncated {
            path: path.clone(),
            dropped: u64::try_from(source.len() - kept).unwrap_or(u64::MAX),
        });
        RetainedSource {
            text: source[..kept].to_owned(),
            complete: false,
        }
    }
}

/// What every record of one file shares: how it is addressed, and where its lines start.
#[derive(Clone, Copy)]
struct FileContext<'file> {
    language: &'file Language,
    unit: &'file SourceUnitId,
    path: &'file ProjectPath,
    line_starts: &'file [usize],
}

/// One record's retained source: the bytes it keeps, and whether they are the whole
/// content.
#[derive(Clone)]
struct RetainedSource {
    text: String,
    complete: bool,
}

/// The declaration's own bytes, absent when its range lies outside the file it names.
///
/// The caller refuses the package rather than publishing the empty string: a record
/// carrying no source and claiming to be complete would verify against a digest over
/// nothing, and the address it names would resolve to a declaration no reader can see.
fn declaration_source<'source>(
    source: &'source str,
    declaration: &SyntaxSymbol,
) -> Option<&'source str> {
    let start = offset_in(declaration.range.start);
    let end = offset_in(declaration.range.end);
    source.get(start..end)
}

/// One byte offset into a file this process holds in memory. An offset past `usize` names
/// bytes no in-memory file carries, so it reads as the end of every file.
fn offset_in(offset: u64) -> usize {
    usize::try_from(offset).unwrap_or(usize::MAX)
}

/// Where each line of one file starts, in ascending offset order, taken once per file.
///
/// One pass over the file, so a file's declarations cost one scan between them rather
/// than one scan each.
fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut consumed = 0_usize;
    for text in lines_inclusive(source) {
        starts.push(consumed);
        consumed += text.len();
    }
    starts
}

/// The one-based line `offset` falls on, from the file's own line starts.
///
/// `starts` ascends, so the line is the count of starts at or before the offset; an
/// offset past the last line reads as that line, and an empty file as line one.
fn line_of(starts: &[usize], offset: u64) -> u64 {
    let offset = offset_in(offset);
    let line = starts.partition_point(|start| *start <= offset);
    u64::try_from(line.max(1)).unwrap_or(u64::MAX)
}

/// The file name a document ranks under, including its extension.
fn file_name(path: &ProjectPath) -> String {
    Path::new(path.0.as_str())
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(path.0.as_str())
        .to_owned()
}

/// The words a ranking splits identifiers into: each name split on case and separator
/// boundaries, lowercased, deduplicated, in first-seen order, at most
/// [`PACKAGE_IDENTIFIER_TERMS_MAX`] of them.
///
/// The cut is a work bound, not a decision: the terms a ranking reads come from a
/// declaration's own name, and a name splitting into more than a thousand words ranks on
/// the first thousand.
fn identifier_terms(names: &[&str]) -> Vec<String> {
    let terms_max = bound(PACKAGE_IDENTIFIER_TERMS_MAX);
    let mut terms: Vec<String> = Vec::new();
    for name in names {
        for word in split_identifier_words(name) {
            if terms.len() >= terms_max {
                return terms;
            }
            let word = word.to_lowercase();
            if !terms.contains(&word) {
                terms.push(word);
            }
        }
    }
    terms
}

/// The origin every declaration of one cataloged package carries.
fn symbol_origin(entry: &CatalogEntry) -> SymbolOrigin {
    match entry.location() {
        PackageLocation::Dependency => SymbolOrigin {
            location: Some(SourceLocationKind::Dependency),
            package: Some(entry.identity().clone()),
            source_kind: SourceKind::Authored,
        },
        PackageLocation::Stdlib => SymbolOrigin {
            location: Some(SourceLocationKind::Stdlib),
            package: None,
            source_kind: SourceKind::Authored,
        },
    }
}

/// The warning a collection that reached its bound reports.
fn truncation(collection: &str, bound: u64) -> PackageAnalysisWarning {
    PackageAnalysisWarning::PublicationTruncated {
        collection: collection.to_owned(),
        bound,
    }
}

/// The digest of one record's canonical content: its own JSON without the `digest`
/// member, rendered as RFC 8785 canonical JSON.
///
/// The member is dropped rather than left empty so a record's digest covers what the
/// record says and nothing about the digest field itself.
fn digest_of<T: Serialize>(
    record: &T,
    package: &PackageIdentity,
) -> Result<Digest, PackageIndexError> {
    let failure = |error| canonical_failure(error, package);
    let mut value = serde_json::to_value(record).map_err(failure)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("digest");
    }
    let rendered = canonical_json(&value).map_err(failure)?;
    Ok(text_digest(&rendered))
}

/// The refusal a record that cannot be rendered canonically draws. Every record this
/// analyzer builds is a plain struct of strings, integers and booleans, so no value it
/// hands the renderer can fail; the arm exists because the renderer's contract admits it.
fn canonical_failure(error: serde_json::Error, package: &PackageIdentity) -> PackageIndexError {
    PackageIndexFault::new(PackageIndexViolation::Provider, package)
        .caused_by(error)
        .into()
}

/// One publication bound as the in-memory count a collection compares against.
///
/// Every bound this module reads is a `u32` the protocol states, and `u32` always fits
/// `usize` on every platform Rift targets.
const fn bound(value: u32) -> usize {
    value as usize
}

/// The wire digest of one text, in the eight-character form every wire digest takes.
fn text_digest(text: &str) -> Digest {
    let rendered = format!("{:x}", Sha256::digest(text.as_bytes()));
    Digest(rendered[..DIGEST_WIRE_CHARS].to_owned())
}

fn wire_unit(unit: &CoreSourceUnitId) -> SourceUnitId {
    SourceUnitId(unit.to_string())
}

fn wire_path(path: &CoreProjectPath) -> ProjectPath {
    ProjectPath(path.as_str().to_owned())
}

/// One package file parsed by the provider its extension names.
fn parsed_file(
    file: &TextSourceFile,
    package: &PackageIdentity,
) -> Result<IndexedFile, PackageIndexError> {
    let context = Path::new(file.path().as_str());
    let extension = context
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let provider = rift_syntax::registry::provider_for_extension(extension).ok_or_else(|| {
        PackageIndexFault::new(PackageIndexViolation::Syntax, package).at(context)
    })?;
    indexed_file_from_catalog(file, context, provider).map_err(|error| {
        PackageIndexFault::new(PackageIndexViolation::Syntax, package)
            .at(context)
            .caused_by(error)
            .into()
    })
}

/// The placement of one package file: its unit, identity path, and origin.
fn placement_of(
    entry: &CatalogEntry,
    path: &CoreProjectPath,
) -> Result<DocumentPlacement, PackageIndexError> {
    let package = entry.identity();
    let identity_fault = || PackageIndexFault::new(PackageIndexViolation::Identity, package);
    let origin = ContributionOrigin::new(Some(source_location(entry)), CoreSourceKind::Authored)
        .map_err(|error| identity_fault().caused_by(error))?;
    let unit = CoreSourceUnitId::for_package(package, path).map_err(|error| {
        identity_fault()
            .at(Path::new(path.as_str()))
            .caused_by(error)
    })?;
    let identity_path = format!("{}/{path}", package_segment(package));
    Ok(DocumentPlacement::new(origin, unit, identity_path))
}

/// The source location an entry's declarations carry.
fn source_location(entry: &CatalogEntry) -> SourceLocation {
    match entry.location() {
        PackageLocation::Dependency => SourceLocation::Dependency {
            package: entry.identity().clone(),
        },
        PackageLocation::Stdlib => SourceLocation::Stdlib {},
    }
}

#[cfg(test)]
mod tests {
    use rift_dependency::CatalogEntry;
    use rift_protocol::canonical::canonical_json;
    use rift_protocol::index::{
        PACKAGE_IDENTIFIER_TERMS_MAX, PACKAGE_PUBLICATION_FORMAT_REVISION,
        PACKAGE_SOURCE_BYTES_MAX, PackageAnalysisWarning, PackageDocumentKind,
    };
    use rift_syntax::ShippedLanguage;

    use super::super::fixture::{identity, language, text};
    use super::super::walk::PackageFiles;
    use super::{PackageAnalyzer, bound};

    /// One package of `files` in `shipped`, analyzed.
    fn analyzed(
        shipped: ShippedLanguage,
        files: Vec<(&str, &str)>,
    ) -> rift_protocol::index::PackagePublication {
        let entry = CatalogEntry::dependency(
            identity("cargo", "beacon", "1.0.0"),
            language(shipped),
            None,
            true,
        );
        let files = PackageFiles::new(
            files
                .into_iter()
                .map(|(path, source)| text(path, source))
                .collect(),
            0,
        );
        PackageAnalyzer::analyze(&entry, &files, 1)
            .expect("the package analyzes")
            .publication()
            .clone()
    }

    /// One package of `files` in `shipped`, analyzed, keeping every part of the analysis.
    fn analysis(shipped: ShippedLanguage, files: Vec<(&str, &str)>) -> super::PackageAnalysis {
        let entry = CatalogEntry::dependency(
            identity("cargo", "beacon", "1.0.0"),
            language(shipped),
            None,
            true,
        );
        let files = PackageFiles::new(
            files
                .into_iter()
                .map(|(path, source)| text(path, source))
                .collect(),
            0,
        );
        PackageAnalyzer::analyze(&entry, &files, 1).expect("the package analyzes")
    }

    /// The `identity` of every document of one kind, in publication order.
    fn document_identities(
        publication: &rift_protocol::index::PackagePublication,
        kind: PackageDocumentKind,
    ) -> Vec<&str> {
        publication
            .documents
            .iter()
            .filter(|document| document.kind == kind)
            .map(|document| document.identity.as_str())
            .collect()
    }

    #[test]
    fn test_rust_package_publishes_units_symbols_and_public_documents() {
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![("src/lib.rs", "pub fn spawn() {}\nfn hidden() {}\n")],
        );

        assert_eq!(
            publication.format_revision,
            PACKAGE_PUBLICATION_FORMAT_REVISION
        );
        assert_eq!(publication.units.len(), 1);
        assert_eq!(publication.units[0].path.0, "src/lib.rs");
        assert!(publication.units[0].source_complete);
        let named: Vec<&str> = publication
            .symbols
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect();
        assert_eq!(named, ["hidden", "spawn"]);
        let public: Vec<&str> = publication
            .symbols
            .iter()
            .filter(|symbol| symbol.public)
            .map(|symbol| symbol.qualified_name.as_str())
            .collect();
        assert_eq!(public, ["spawn"], "the export rule keeps `pub` alone");
        assert_eq!(
            document_identities(&publication, PackageDocumentKind::Symbol),
            ["rift://symbol/rust/cargo/beacon@1.0.0/src/lib.rs/spawn"],
            "a document stands for every public declaration and no other"
        );
        assert_eq!(
            document_identities(&publication, PackageDocumentKind::File),
            ["rift://source/cargo/beacon@1.0.0/src/lib.rs"]
        );
    }

    #[test]
    fn test_symbol_document_carries_the_fields_a_ranking_reads() {
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![(
                "src/lib.rs",
                "/// Spawns a task.\npub fn spawn_task(count: u8) {}\n",
            )],
        );

        let document = publication
            .documents
            .iter()
            .find(|document| document.kind == PackageDocumentKind::Symbol)
            .expect("the public declaration has a document");
        assert_eq!(document.name, "spawn_task");
        assert_eq!(document.qualified_name.as_deref(), Some("spawn_task"));
        assert_eq!(
            document.identifier_terms,
            ["spawn", "task"],
            "the terms are the words the names split into; the names themselves are \
             their own fields"
        );
        assert!(
            document
                .documentation
                .as_deref()
                .is_some_and(|text| text.contains("Spawns a task")),
            "{document:?}"
        );
        assert!(
            document
                .declaration_source
                .as_deref()
                .is_some_and(|source| source.contains("pub fn spawn_task")),
            "{document:?}"
        );
        assert!(
            document.file_content.is_none(),
            "a symbol document ranks the declaration, not the file"
        );
    }

    /// Every shipped package language reaches the publication through the same pass,
    /// each under its own export rule: JavaScript ships none, so every declaration is
    /// public, while TypeScript and TSX drop a `private` member.
    #[test]
    fn test_every_package_language_publishes_its_public_declarations() {
        let cases = [
            (
                ShippedLanguage::JavaScript,
                "index.js",
                "export function open() {}\nfunction helper() {}\n",
                vec!["helper", "open"],
            ),
            (
                ShippedLanguage::TypeScript,
                "index.ts",
                "export class Client {\n  private secret(): void {}\n  open(): void {}\n}\n",
                vec!["Client", "Client.open"],
            ),
            (
                ShippedLanguage::TypeScriptTsx,
                "index.tsx",
                "export class Panel {\n  private state(): void {}\n  render(): null {\n    return null;\n  }\n}\n",
                vec!["Panel", "Panel.render"],
            ),
        ];
        for (shipped, path, source, expected) in cases {
            let publication = analyzed(shipped, vec![(path, source)]);
            let public: Vec<&str> = publication
                .symbols
                .iter()
                .filter(|symbol| symbol.public)
                .map(|symbol| symbol.qualified_name.as_str())
                .collect();
            assert_eq!(public, expected, "{path}");
            assert_eq!(publication.units.len(), 1, "{path}");
        }
    }

    #[test]
    fn test_equal_package_bytes_produce_an_equal_publication() {
        let source = vec![("src/lib.rs", "pub fn spawn() {}\n")];
        let first = analyzed(ShippedLanguage::Rust, source.clone());
        let second = analyzed(ShippedLanguage::Rust, source);

        assert_eq!(first, second);
        assert_eq!(
            canonical_json(&first).expect("canonical"),
            canonical_json(&second).expect("canonical")
        );
    }

    #[test]
    fn test_input_file_order_does_not_change_the_publication() {
        let forward = analyzed(
            ShippedLanguage::Rust,
            vec![
                ("src/a.rs", "pub fn alpha() {}\n"),
                ("src/b.rs", "pub fn beta() {}\n"),
            ],
        );
        let reversed = analyzed(
            ShippedLanguage::Rust,
            vec![
                ("src/b.rs", "pub fn beta() {}\n"),
                ("src/a.rs", "pub fn alpha() {}\n"),
            ],
        );

        assert_eq!(
            canonical_json(&forward).expect("canonical"),
            canonical_json(&reversed).expect("canonical")
        );
    }

    #[test]
    fn test_changing_one_declaration_changes_only_its_own_records() {
        let before = analyzed(
            ShippedLanguage::Rust,
            vec![
                ("src/a.rs", "pub fn alpha() {}\n"),
                ("src/b.rs", "pub fn beta() {}\n"),
            ],
        );
        let after = analyzed(
            ShippedLanguage::Rust,
            vec![
                ("src/a.rs", "pub fn alpha(count: u8) {}\n"),
                ("src/b.rs", "pub fn beta() {}\n"),
            ],
        );

        let changed_units: Vec<&str> = before
            .units
            .iter()
            .zip(&after.units)
            .filter(|(before, after)| before.digest != after.digest)
            .map(|(before, _)| before.path.0.as_str())
            .collect();
        assert_eq!(changed_units, ["src/a.rs"]);
        let changed_symbols: Vec<&str> = before
            .symbols
            .iter()
            .zip(&after.symbols)
            .filter(|(before, after)| before.digest != after.digest)
            .map(|(before, _)| before.qualified_name.as_str())
            .collect();
        assert_eq!(changed_symbols, ["alpha"]);
        let changed_documents: Vec<&str> = before
            .documents
            .iter()
            .zip(&after.documents)
            .filter(|(before, after)| before.digest != after.digest)
            .map(|(before, _)| before.identity.as_str())
            .collect();
        assert_eq!(
            changed_documents,
            [
                "rift://symbol/rust/cargo/beacon@1.0.0/src/a.rs/alpha",
                "rift://source/cargo/beacon@1.0.0/src/a.rs",
            ],
            "the changed declaration's own document and its file's document move"
        );
        assert_ne!(before.source_digest, after.source_digest);
    }

    /// Two publications compare record by record through their stable identities and
    /// digests, which is what an adapter computing added, replaced, and removed records
    /// reads.
    #[test]
    fn test_publications_compare_by_stable_identity_and_digest() {
        let before = analyzed(
            ShippedLanguage::Rust,
            vec![("src/a.rs", "pub fn alpha() {}\n")],
        );
        let after = analyzed(
            ShippedLanguage::Rust,
            vec![
                ("src/a.rs", "pub fn alpha() {}\n"),
                ("src/b.rs", "pub fn beta() {}\n"),
            ],
        );

        let held: Vec<(&str, &str)> = before
            .symbols
            .iter()
            .map(|symbol| (symbol.symbol.0.as_str(), symbol.digest.0.as_str()))
            .collect();
        let arrived: Vec<(&str, &str)> = after
            .symbols
            .iter()
            .map(|symbol| (symbol.symbol.0.as_str(), symbol.digest.0.as_str()))
            .collect();
        let added: Vec<&str> = arrived
            .iter()
            .filter(|(identity, _)| !held.iter().any(|(kept, _)| kept == identity))
            .map(|(identity, _)| *identity)
            .collect();
        assert_eq!(
            added,
            ["rift://symbol/rust/cargo/beacon@1.0.0/src/b.rs/beta"]
        );
        let unchanged: Vec<&str> = arrived
            .iter()
            .filter(|entry| held.contains(entry))
            .map(|(identity, _)| *identity)
            .collect();
        assert_eq!(
            unchanged,
            ["rift://symbol/rust/cargo/beacon@1.0.0/src/a.rs/alpha"],
            "an untouched declaration keeps its digest"
        );
    }

    /// A publication addresses source by package-relative path and unit identity, so no
    /// machine's directory layout reaches a consumer.
    #[test]
    fn test_publication_carries_no_host_absolute_path() {
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![("src/lib.rs", "pub fn spawn() {}\n")],
        );

        let rendered = canonical_json(&publication).expect("canonical");
        assert!(!rendered.contains("/Users/"), "{rendered}");
        assert!(!rendered.contains("\\\\"), "{rendered}");
        assert!(rendered.contains("src/lib.rs"));
    }

    /// The analysis hands back the parsed files beside the publication, so the local
    /// package index reads what the publication was built from rather than parsing again.
    #[test]
    fn test_analysis_carries_the_parsed_files_beside_the_publication() {
        let analysis = analysis(
            ShippedLanguage::Rust,
            vec![("src/lib.rs", "pub fn spawn() {}\n")],
        );

        let files = analysis.files();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file().path().as_str(), "src/lib.rs");
        assert!(files[0].is_public("spawn"));
        assert_eq!(analysis.publication().symbols.len(), 1);
    }

    /// A declaration past the retained-source bound keeps the bytes that fit, reports the
    /// count it dropped, and states that its source is not the whole declaration.
    #[test]
    fn test_a_declaration_past_the_source_bound_is_cut_and_reported() {
        // Three-byte characters, padded so the bound falls inside one: the cut then walks
        // back to the boundary rather than splitting the character.
        let mut prefix = String::from("pub fn spawn() {\n    // ");
        while (bound(PACKAGE_SOURCE_BYTES_MAX) - prefix.len()).is_multiple_of("\u{4e16}".len()) {
            prefix.push(' ');
        }
        let filler = "\u{4e16}".repeat(bound(PACKAGE_SOURCE_BYTES_MAX));
        let source = format!("{prefix}{filler}\n}}\n");
        assert!(
            !source.is_char_boundary(bound(PACKAGE_SOURCE_BYTES_MAX)),
            "the bound falls inside a character"
        );

        let publication = analyzed(ShippedLanguage::Rust, vec![("src/lib.rs", &source)]);

        let symbol = publication
            .symbols
            .iter()
            .find(|symbol| symbol.name == "spawn")
            .expect("the declaration is published");
        assert!(!symbol.source_complete, "{symbol:?}");
        assert!(
            symbol.source.len() <= bound(PACKAGE_SOURCE_BYTES_MAX),
            "the retained source fits the bound: {}",
            symbol.source.len()
        );
        assert!(
            symbol.source.len() > bound(PACKAGE_SOURCE_BYTES_MAX) - 4,
            "the cut walks back to a character boundary, not further: {}",
            symbol.source.len()
        );
        let dropped: Vec<&PackageAnalysisWarning> = publication
            .warnings
            .iter()
            .filter(|warning| matches!(warning, PackageAnalysisWarning::SourceTruncated { .. }))
            .collect();
        assert!(!dropped.is_empty(), "{:?}", publication.warnings);
    }

    /// The ranking terms stop at their bound: a name splitting into more words than a
    /// document may carry ranks on the ones that fit.
    #[test]
    fn test_identifier_terms_stop_at_their_bound() {
        let terms_max = bound(PACKAGE_IDENTIFIER_TERMS_MAX);
        let name: String = (0..=terms_max)
            .map(|index| format!("w{index}"))
            .collect::<Vec<String>>()
            .join("_");

        let terms = super::identifier_terms(&[&name]);

        assert_eq!(terms.len(), terms_max);
        assert_eq!(terms[0], "w0");
    }
}
