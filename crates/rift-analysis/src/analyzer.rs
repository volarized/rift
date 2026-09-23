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

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::ffi::OsStr;
use std::path::Path;

use crate::documentation::{
    DocumentationDeclaration, DocumentationError, DocumentationInput, DocumentationSourceSet,
    collect_documentation, content_chunk_identity, content_digest,
};
use crate::input::ExactPackageInput;
use crate::revision::analyzer_revision;
use crate::semantic::{PlacedDocument, WorkspaceSemantics};
use crate::source::{FileDigest, IndexedFile};
use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_core::line::{line_of, line_starts};
use rift_core::{
    ContributionOrigin, Error, ErrorCode, ErrorContext, ErrorName, Fault,
    ProjectPath as CoreProjectPath, SourceKind, SourceUnitId as CoreSourceUnitId, fault_label,
    symbol_identity,
};
use rift_protocol::canonical::canonical_json;
use rift_protocol::documentation::{
    DocumentationChunk, DocumentationContentIdentity, DocumentationSelectionReason,
    DocumentationSource, DocumentationSourceFormat, DocumentationSourceIdentity, NotebookCellKind,
};
use rift_protocol::index::{
    PACKAGE_DOCUMENTS_MAX, PACKAGE_IDENTIFIER_TERMS_MAX, PACKAGE_PUBLICATION_FORMAT_REVISION,
    PACKAGE_SOURCE_BYTES_MAX, PACKAGE_SYMBOLS_MAX, PACKAGE_UNITS_MAX, PACKAGE_WARNINGS_MAX,
    PackageAnalysisWarning, PackageDocument, PackageDocumentKind, PackagePublication,
    PackageSourceUnit, PackageSymbol,
};
use rift_protocol::read::{
    Digest, ExactKind, Language, PackageIdentity, ProjectPath, SourceLocationKind, SourceUnitId,
    SymbolId, SymbolOrigin, TextRange,
};
use rift_provider::CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT;
use rift_syntax::{DocumentPlacement, ShippedLanguage, SyntaxDocument, SyntaxSymbol};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use rift_ranking::split_identifier_words;

/// Package analysis failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageAnalysisViolation {
    /// Package identity cannot form a source unit, resolver, or symbol.
    Identity,
    /// No shipped provider parses a selected file, or its parse failed.
    Syntax,
    /// Semantic publication or normalization failed.
    Provider,
    /// A package file exceeded the declaration publication bound.
    PackageDeclarationsExceeded,
}

/// One package analysis failure with package, path, and source evidence.
#[derive(Debug)]
pub struct PackageAnalysisFault {
    violation: PackageAnalysisViolation,
    package: Box<PackageIdentity>,
    path: Option<CoreProjectPath>,
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl PackageAnalysisFault {
    fn new(violation: PackageAnalysisViolation, package: &PackageIdentity) -> Self {
        Self {
            violation,
            package: Box::new(package.clone()),
            path: None,
            source: None,
        }
    }

    fn at(mut self, path: &CoreProjectPath) -> Self {
        self.path = Some(path.clone());
        self
    }

    fn caused_by(mut self, source: impl StdError + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    fn into_error(self) -> PackageAnalysisError {
        Error::new(self)
    }

    /// Failure classification.
    #[must_use]
    pub const fn violation(&self) -> PackageAnalysisViolation {
        self.violation
    }

    /// Package identity.
    #[must_use]
    pub const fn package(&self) -> &PackageIdentity {
        &self.package
    }

    /// Package-relative path, when one caused the failure.
    #[must_use]
    pub const fn path(&self) -> Option<&CoreProjectPath> {
        self.path.as_ref()
    }
}

impl Fault for PackageAnalysisFault {
    fn name(&self) -> ErrorName {
        if self.violation == PackageAnalysisViolation::Syntax {
            return self
                .source
                .as_deref()
                .and_then(|source| source.downcast_ref::<rift_syntax::SyntaxError>())
                .map_or(ErrorName::Wire(ErrorCode::InternalError), |error| {
                    error.name()
                });
        }
        ErrorName::Wire(ErrorCode::InternalError)
    }

    fn context(&self) -> Vec<ErrorContext> {
        let package = format!(
            "{}/{}@{}",
            self.package.manager, self.package.name, self.package.version
        );
        let mut context = vec![
            ErrorContext::new("violation", fault_label(&self.violation)),
            ErrorContext::new("package", package),
        ];
        if let Some(path) = &self.path {
            context.push(ErrorContext::new("path", path.as_str().to_owned()));
        }
        context
    }

    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

/// Opaque package analysis failure.
pub type PackageAnalysisError = Error<PackageAnalysisFault>;

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
    notebook_cells: BTreeMap<DocumentationContentIdentity, String>,
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
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        PackagePublication,
        Vec<AnalyzedFile>,
        WorkspaceSemantics,
        BTreeMap<DocumentationContentIdentity, String>,
    ) {
        (
            self.publication,
            self.files,
            self.semantics,
            self.notebook_cells,
        )
    }
}

/// Turns one package's source bytes into one canonical publication.
#[derive(Debug, Default)]
pub struct PackageAnalyzer;

impl PackageAnalyzer {
    /// Analyzes one exact package's selected files.
    ///
    /// Each file is placed under `rift://source/<manager>/<name>@<version>/<path>` with
    /// the identity path `<manager>/<name>@<version>/<path>`, and its origin is the
    /// input origin: a dependency carrying the package identity, or the standard library.
    /// Records are emitted in unit, symbol, and document identity order, and a
    /// collection that reaches its bound stops there and reports the stop as a warning.
    ///
    /// The work is proportional to the selected bytes: one parse per file, one scan per
    /// file for its line starts, one assembly pass over the parsed declarations, and one
    /// canonical rendering per record.
    ///
    /// # Errors
    ///
    /// Returns [`PackageAnalysisError`] when the identity cannot spell a resolver or unit,
    /// when no provider parses a file or its parse fails, or when publication or
    /// normalization refuses the package graph.
    pub fn analyze(
        input: ExactPackageInput<'_>,
        revision: u64,
    ) -> Result<PackageAnalysis, PackageAnalysisError> {
        let package = input.package();
        let mut analyzed = Vec::with_capacity(input.files().len());
        for file in input.files() {
            let parsed = parsed_file(*file, package, input.language())?;
            let placement = placement_of(package, input.origin(), file.path())?;
            let public_names = public_qualified_names(parsed.syntax().language(), parsed.syntax());
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
        let built = WorkspaceSemantics::build_placed(
            &placed,
            CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT,
            revision,
            None,
        )
        .map_err(|error| {
            PackageAnalysisFault::new(PackageAnalysisViolation::Provider, package)
                .caused_by(error)
                .into_error()
        })?;
        if let Some(path) = built.beyond_declaration_bound.first() {
            return Err(PackageAnalysisFault::new(
                PackageAnalysisViolation::PackageDeclarationsExceeded,
                package,
            )
            .at(path)
            .into_error());
        }
        let (publication, notebook_cells) = publish(
            package,
            input.origin(),
            input.language(),
            &analyzed,
            &built.semantics,
        )?;
        Ok(PackageAnalysis {
            publication,
            files: analyzed,
            semantics: built.semantics,
            notebook_cells,
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
    package: &PackageIdentity,
    source_origin: &ContributionOrigin,
    package_language: &Language,
    analyzed: &[AnalyzedFile],
    semantics: &WorkspaceSemantics,
) -> Result<
    (
        PackagePublication,
        BTreeMap<DocumentationContentIdentity, String>,
    ),
    PackageAnalysisError,
> {
    let origin = symbol_origin(package, source_origin)?;
    let mut records = Records::default();
    for held in analyzed {
        if records.units.len() >= bound(PACKAGE_UNITS_MAX) {
            records.warn(truncation("units", u64::from(PACKAGE_UNITS_MAX)));
            break;
        }
        records.file(package, &origin, semantics, held)?;
    }
    let (documentation, notebook_cells) =
        package_documentation(package, package_language, &origin, analyzed, &mut records)?;
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
    Ok((
        PackagePublication {
            format_revision: PACKAGE_PUBLICATION_FORMAT_REVISION,
            analyzer_revision: analyzer_revision(),
            package: package.clone(),
            source_digest,
            units: records.units,
            symbols: records.symbols,
            documents: records.documents,
            documentation,
            warnings: records.warnings,
        },
        notebook_cells,
    ))
}

fn package_documentation(
    package: &PackageIdentity,
    package_language: &Language,
    origin: &SymbolOrigin,
    analyzed: &[AnalyzedFile],
    records: &mut Records,
) -> Result<
    (
        rift_protocol::documentation::DocumentationIndex,
        BTreeMap<DocumentationContentIdentity, String>,
    ),
    PackageAnalysisError,
> {
    let notebooks = decode_package_notebooks(package, analyzed)?;
    let mut inputs = Vec::new();
    for (held, notebook_content) in analyzed.iter().zip(&notebooks) {
        let Some(format) = documentation_format(held.file.path().as_str()) else {
            continue;
        };
        if format == DocumentationSourceFormat::Notebook {
            let unit = wire_unit(held.placement.unit());
            let owner = DocumentationContentIdentity {
                source: DocumentationSourceIdentity::Package { unit: unit.clone() },
                cell: None,
            };
            let notebook = notebook_content
                .as_ref()
                .ok_or_else(|| documentation_error_for_package(package))?;
            for cell in notebook.cells() {
                let cell_identity = DocumentationContentIdentity {
                    source: owner.source.clone(),
                    cell: Some(cell.cell().clone()),
                };
                let source = documentation_source(
                    cell_identity.clone(),
                    cell.text(),
                    origin,
                    format,
                    cell.declared_language().cloned(),
                    cell.physical_ranges().to_vec(),
                );
                let mut input = DocumentationInput::new(source, cell.text())
                    .map_err(|error| documentation_error(error, package))?;
                let document_id = content_chunk_identity(&cell_identity, 0)
                    .map_err(|error| documentation_error(error, package))?;
                if cell.text().len() <= bound(PACKAGE_SOURCE_BYTES_MAX) && !cell.text().is_empty() {
                    input = input
                        .with_chunks(vec![DocumentationChunk {
                            identity: document_id.clone(),
                            range: TextRange {
                                start: 0,
                                end: u64::try_from(cell.text().len()).unwrap_or(u64::MAX),
                            },
                        }])
                        .map_err(|error| documentation_error(error, package))?;
                }
                inputs.push(input);

                let language = match cell.cell().kind {
                    NotebookCellKind::Markdown => ShippedLanguage::Markdown.language(),
                    NotebookCellKind::Code => cell
                        .declared_language()
                        .cloned()
                        .unwrap_or_else(|| package_language.clone()),
                };
                let path = wire_path(held.file.path());
                let retained = records.retained(cell.text(), &path);
                let name = file_name(&path);
                records.document(PackageDocument {
                    identity: document_id,
                    kind: PackageDocumentKind::File,
                    unit: unit.clone(),
                    language,
                    package: package.clone(),
                    content_digest: text_digest(&retained.text),
                    identifier_terms: identifier_terms(&[&name]),
                    name,
                    qualified_name: None,
                    signature: None,
                    documentation: None,
                    declaration_source: None,
                    file_content: Some(retained.text),
                    digest: Digest(String::new()),
                })?;
            }
            continue;
        }
        inputs.push(package_file_input(package, origin, held, format)?);
    }
    append_attached_comment_inputs(package, origin, analyzed, &mut inputs)?;
    let sources =
        DocumentationSourceSet::new(inputs).map_err(|error| documentation_error(error, package))?;
    let declarations = package_declarations(package, analyzed, records)?;
    let notebook_cells = package_notebook_cells(analyzed, &notebooks);
    collect_documentation(&sources, &declarations)
        .map(|collection| (collection.into_index(), notebook_cells))
        .map_err(|error| documentation_error(error, package))
}

fn package_file_input<'source>(
    package: &PackageIdentity,
    origin: &SymbolOrigin,
    held: &'source AnalyzedFile,
    format: DocumentationSourceFormat,
) -> Result<DocumentationInput<'source>, PackageAnalysisError> {
    let unit = wire_unit(held.placement.unit());
    let identity = DocumentationContentIdentity {
        source: DocumentationSourceIdentity::Package { unit: unit.clone() },
        cell: None,
    };
    let text = held.file.source();
    let source = documentation_source(identity, text, origin, format, None, Vec::new());
    let mut input = DocumentationInput::new(source, text)
        .map_err(|error| documentation_error(error, package))?;
    if matches!(
        format,
        DocumentationSourceFormat::Markdown | DocumentationSourceFormat::Mdx
    ) {
        input = input
            .with_syntax(held.file.syntax())
            .map_err(|error| documentation_error(error, package))?;
    }
    if text.len() <= bound(PACKAGE_SOURCE_BYTES_MAX) && !text.is_empty() {
        input = input
            .with_chunks(vec![DocumentationChunk {
                identity: unit.0,
                range: TextRange {
                    start: 0,
                    end: u64::try_from(text.len()).unwrap_or(u64::MAX),
                },
            }])
            .map_err(|error| documentation_error(error, package))?;
    }
    Ok(input)
}

fn decode_package_notebooks(
    package: &PackageIdentity,
    analyzed: &[AnalyzedFile],
) -> Result<Vec<Option<crate::documentation::notebook::NotebookContent>>, PackageAnalysisError> {
    let mut notebooks = Vec::with_capacity(analyzed.len());
    for held in analyzed {
        if documentation_format(held.file.path().as_str())
            != Some(DocumentationSourceFormat::Notebook)
        {
            notebooks.push(None);
            continue;
        }
        let identity = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Package {
                unit: wire_unit(held.placement.unit()),
            },
            cell: None,
        };
        let notebook =
            crate::documentation::notebook::decode_notebook(held.file.source(), &identity)
                .map_err(|error| documentation_error(error, package))?;
        notebooks.push(Some(notebook));
    }
    Ok(notebooks)
}

fn append_attached_comment_inputs<'source>(
    package: &PackageIdentity,
    origin: &SymbolOrigin,
    analyzed: &'source [AnalyzedFile],
    inputs: &mut Vec<DocumentationInput<'source>>,
) -> Result<(), PackageAnalysisError> {
    for held in analyzed {
        if !held
            .file
            .syntax()
            .symbols()
            .iter()
            .any(|symbol| !symbol.documentation_ranges.is_empty())
        {
            continue;
        }
        let source_text = held.file.source();
        let unit = wire_unit(held.placement.unit());
        let source = documentation_source(
            DocumentationContentIdentity {
                source: DocumentationSourceIdentity::Package { unit: unit.clone() },
                cell: None,
            },
            source_text,
            origin,
            DocumentationSourceFormat::AttachedComment,
            Some(held.file.syntax().language().clone()),
            Vec::new(),
        );
        let mut input = DocumentationInput::new(source, source_text)
            .map_err(|error| documentation_error(error, package))?
            .with_syntax(held.file.syntax())
            .map_err(|error| documentation_error(error, package))?;
        if source_text.len() <= bound(PACKAGE_SOURCE_BYTES_MAX) && !source_text.is_empty() {
            input = input
                .with_chunks(vec![DocumentationChunk {
                    identity: unit.0,
                    range: TextRange {
                        start: 0,
                        end: u64::try_from(source_text.len()).unwrap_or(u64::MAX),
                    },
                }])
                .map_err(|error| documentation_error(error, package))?;
        }
        inputs.push(input);
    }
    Ok(())
}

fn package_declarations<'declaration>(
    package: &PackageIdentity,
    analyzed: &'declaration [AnalyzedFile],
    records: &'declaration Records,
) -> Result<Vec<DocumentationDeclaration<'declaration>>, PackageAnalysisError> {
    let languages: BTreeMap<_, _> = analyzed
        .iter()
        .map(|held| {
            (
                wire_unit(held.placement.unit()).0,
                held.file.syntax().language(),
            )
        })
        .collect();
    let mut declarations = Vec::with_capacity(records.symbols.len());
    for symbol in &records.symbols {
        let language = languages
            .get(&symbol.unit.0)
            .copied()
            .ok_or_else(|| documentation_error_for_package(package))?;
        let identity = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Package {
                unit: symbol.unit.clone(),
            },
            cell: None,
        };
        declarations.push(
            DocumentationDeclaration::new(
                &symbol.symbol,
                language,
                &symbol.name,
                &symbol.qualified_name,
                &identity,
                symbol.range.clone(),
            )
            .map_err(|error| documentation_error(error, package))?,
        );
    }
    Ok(declarations)
}

fn package_notebook_cells(
    analyzed: &[AnalyzedFile],
    notebooks: &[Option<crate::documentation::notebook::NotebookContent>],
) -> BTreeMap<DocumentationContentIdentity, String> {
    let mut cells = BTreeMap::new();
    for (held, notebook) in analyzed.iter().zip(notebooks) {
        let Some(notebook) = notebook else {
            continue;
        };
        let owner = DocumentationSourceIdentity::Package {
            unit: wire_unit(held.placement.unit()),
        };
        for cell in notebook.cells() {
            cells.insert(
                DocumentationContentIdentity {
                    source: owner.clone(),
                    cell: Some(cell.cell().clone()),
                },
                cell.text().to_owned(),
            );
        }
    }
    cells
}

fn documentation_source(
    identity: DocumentationContentIdentity,
    text: &str,
    origin: &SymbolOrigin,
    format: DocumentationSourceFormat,
    language: Option<Language>,
    physical_ranges: Vec<TextRange>,
) -> DocumentationSource {
    let media_type = match format {
        DocumentationSourceFormat::Markdown | DocumentationSourceFormat::AttachedComment => {
            "text/markdown"
        }
        DocumentationSourceFormat::Mdx => "text/mdx",
        DocumentationSourceFormat::RestructuredText => "text/x-rst",
        DocumentationSourceFormat::Text => "text/plain",
        DocumentationSourceFormat::Notebook => "application/x-ipynb+json",
    };
    let digest = content_digest(text.as_bytes());
    DocumentationSource {
        identity,
        revision: digest.clone(),
        content_digest: digest,
        origin: origin.clone(),
        format,
        media_type: media_type.to_owned(),
        selection: if format == DocumentationSourceFormat::AttachedComment {
            DocumentationSelectionReason::AttachedComment
        } else {
            DocumentationSelectionReason::PackageArchive
        },
        byte_length: u64::try_from(text.len()).unwrap_or(u64::MAX),
        language,
        physical_ranges,
        license: None,
    }
}

fn documentation_error(
    error: DocumentationError,
    package: &PackageIdentity,
) -> PackageAnalysisError {
    PackageAnalysisFault::new(PackageAnalysisViolation::Provider, package)
        .caused_by(error)
        .into_error()
}

fn documentation_error_for_package(package: &PackageIdentity) -> PackageAnalysisError {
    PackageAnalysisFault::new(PackageAnalysisViolation::Provider, package).into_error()
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
        semantics: &WorkspaceSemantics,
        held: &AnalyzedFile,
    ) -> Result<(), PackageAnalysisError> {
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
        if documentation_format(held.file.path().as_str())
            != Some(DocumentationSourceFormat::Notebook)
        {
            let document_digest = text_digest(&retained.text);
            self.file_document(package, &language, &unit, &path, retained, document_digest)?;
        }
        let context = FileContext {
            language: &language,
            unit: &unit,
            path: &path,
            line_starts: &line_starts(source),
        };
        for declaration in held.file.syntax().symbols() {
            self.declaration(package, origin, semantics, held, &context, declaration)?;
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
    ) -> Result<(), PackageAnalysisError> {
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
        semantics: &WorkspaceSemantics,
        held: &AnalyzedFile,
        context: &FileContext<'_>,
        declaration: &SyntaxSymbol,
    ) -> Result<(), PackageAnalysisError> {
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
            PackageAnalysisFault::new(PackageAnalysisViolation::Provider, package)
                .at(held.file.path())
                .into_error()
        })?;
        let retained = self.retained(declared, path);
        let content_digest = text_digest(&retained.text);
        let symbol = SymbolId(symbol_identity(
            &language.identity_segment(),
            held.placement.identity_path(),
            &declaration.qualified_name,
        ));
        let presentation = semantics
            .assembled(&symbol.0)
            .and_then(|assembled| {
                assembled
                    .facts()
                    .map(|facts| assembled.to_protocol_symbol(facts))
            })
            .ok_or_else(|| {
                PackageAnalysisFault::new(PackageAnalysisViolation::Provider, package)
                    .at(held.file.path())
                    .into_error()
            })?;
        let mut record = PackageSymbol {
            symbol,
            presentation,
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
    fn document(&mut self, mut document: PackageDocument) -> Result<(), PackageAnalysisError> {
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
fn symbol_origin(
    package: &PackageIdentity,
    origin: &ContributionOrigin,
) -> Result<SymbolOrigin, PackageAnalysisError> {
    let result = match origin.location() {
        Some(rift_core::SourceLocation::Dependency { package: owner }) if owner == package => {
            SymbolOrigin {
                location: Some(SourceLocationKind::Dependency),
                package: Some(package.clone()),
                source_kind: SourceKind::Authored,
            }
        }
        Some(rift_core::SourceLocation::Stdlib {}) => SymbolOrigin {
            location: Some(SourceLocationKind::Stdlib),
            package: None,
            source_kind: SourceKind::Authored,
        },
        _ => {
            return Err(
                PackageAnalysisFault::new(PackageAnalysisViolation::Identity, package).into_error(),
            );
        }
    };
    Ok(result)
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
) -> Result<Digest, PackageAnalysisError> {
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
fn canonical_failure(error: serde_json::Error, package: &PackageIdentity) -> PackageAnalysisError {
    PackageAnalysisFault::new(PackageAnalysisViolation::Provider, package)
        .caused_by(error)
        .into_error()
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
    file: crate::input::PackageSource<'_>,
    package: &PackageIdentity,
    package_language: &Language,
) -> Result<IndexedFile, PackageAnalysisError> {
    let context = Path::new(file.path().as_str());
    let extension = context
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let language = source_language(extension, package_language);
    let syntax = if matches!(extension, "rst" | "txt" | "ipynb") {
        SyntaxDocument::empty(language, file.path().clone())
    } else {
        let provider =
            rift_syntax::registry::provider_for_extension(extension).ok_or_else(|| {
                PackageAnalysisFault::new(PackageAnalysisViolation::Syntax, package)
                    .at(file.path())
                    .into_error()
            })?;
        provider
            .analyze(rift_syntax::SyntaxSource {
                path: file.path(),
                text: file.text(),
            })
            .map_err(|error| {
                PackageAnalysisFault::new(PackageAnalysisViolation::Syntax, package)
                    .at(file.path())
                    .caused_by(error)
                    .into_error()
            })?
    };
    Ok(IndexedFile::new(
        file.path().clone(),
        file.text().to_owned(),
        FileDigest::of(file.text().as_bytes()),
        false,
        syntax,
    ))
}

fn source_language(extension: &str, package_language: &Language) -> Language {
    match extension {
        "md" | "markdown" | "mdx" => ShippedLanguage::Markdown.language(),
        "rst" => Language {
            name: "rst".to_owned(),
            dialect: None,
        },
        "txt" => Language {
            name: "text".to_owned(),
            dialect: None,
        },
        "ipynb" => Language {
            name: "json".to_owned(),
            dialect: None,
        },
        _ => package_language.clone(),
    }
}

/// Returns the documentation format selected by a supported package source extension.
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

/// The placement of one package file: its unit, identity path, and origin.
fn placement_of(
    package: &PackageIdentity,
    origin: &ContributionOrigin,
    path: &CoreProjectPath,
) -> Result<DocumentPlacement, PackageAnalysisError> {
    let identity_fault = || PackageAnalysisFault::new(PackageAnalysisViolation::Identity, package);
    let unit = CoreSourceUnitId::for_package(package, path)
        .map_err(|error| identity_fault().at(path).caused_by(error).into_error())?;
    let identity_path = format!("{}/{path}", package_segment(package));
    Ok(DocumentPlacement::new(origin.clone(), unit, identity_path))
}

/// The languages the package index reads an API from, and each one's rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageLanguage {
    /// Rust declarations use `pub`; items of a public trait also count.
    Rust,
    /// Python names without a leading underscore count.
    Python,
    /// TypeScript declarations exclude private and protected members.
    TypeScript,
}

impl PackageLanguage {
    /// Rules for one language, when a shipped definition serves it.
    #[must_use]
    pub fn for_language(language: &Language) -> Option<Self> {
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

    /// Whether one file name is a source candidate for this package language.
    #[must_use]
    pub fn is_candidate(self, file_name: &str) -> bool {
        let path = std::path::Path::new(file_name);
        let extension = path.extension().and_then(std::ffi::OsStr::to_str);
        match self {
            Self::Rust => extension.is_some_and(|value| value.eq_ignore_ascii_case("rs")),
            Self::Python => extension.is_some_and(|value| {
                value.eq_ignore_ascii_case("py") || value.eq_ignore_ascii_case("pyi")
            }),
            Self::TypeScript => {
                extension.is_some_and(|value| value.eq_ignore_ascii_case("ts"))
                    && path.file_stem().is_some_and(|stem| {
                        stem.to_string_lossy().to_ascii_lowercase().ends_with(".d")
                    })
            }
        }
    }

    fn is_public(self, symbol: &SyntaxSymbol, by_name: &BTreeMap<&str, &SyntaxSymbol>) -> bool {
        match self {
            Self::Rust => match symbol.visibility.as_deref() {
                Some("pub") => true,
                Some("private") => symbol
                    .container
                    .as_deref()
                    .and_then(|container| by_name.get(container))
                    .is_some_and(|container| {
                        container.kind == "trait" && container.visibility.as_deref() == Some("pub")
                    }),
                _ => false,
            },
            Self::Python => !symbol.name.starts_with('_'),
            Self::TypeScript => !symbol
                .visibility
                .as_deref()
                .is_some_and(|visibility| matches!(visibility, "private" | "protected")),
        }
    }
}

/// Qualified names of declarations a package language exposes.
#[must_use]
pub fn public_qualified_names(language: &Language, document: &SyntaxDocument) -> BTreeSet<String> {
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

fn package_segment(identity: &PackageIdentity) -> String {
    format!(
        "{}/{}@{}",
        identity.manager, identity.name, identity.version
    )
}

#[cfg(test)]
mod tests {
    use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
    use rift_protocol::canonical::canonical_json;
    use rift_protocol::index::{
        PACKAGE_IDENTIFIER_TERMS_MAX, PACKAGE_PUBLICATION_FORMAT_REVISION,
        PACKAGE_SOURCE_BYTES_MAX, PackageAnalysisWarning, PackageDocumentKind,
    };
    use rift_protocol::read::{Language, PackageIdentity};
    use rift_syntax::ShippedLanguage;

    use super::{PackageAnalysis, PackageAnalyzer, bound};
    use crate::{ExactPackageInput, ExactPackageLimits, PackageSource};

    fn identity() -> PackageIdentity {
        PackageIdentity {
            manager: "cargo".to_owned(),
            name: "beacon".to_owned(),
            version: "1.0.0".to_owned(),
        }
    }

    fn language(shipped: ShippedLanguage) -> Language {
        shipped.language()
    }

    fn package_analysis(shipped: ShippedLanguage, files: Vec<(&str, &str)>) -> PackageAnalysis {
        let package = identity();
        let language = language(shipped);
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency {
                package: package.clone(),
            }),
            SourceKind::Authored,
        )
        .expect("origin");
        let files: Vec<(ProjectPath, &str)> = files
            .into_iter()
            .map(|(path, text)| (ProjectPath::new(path).expect("path"), text))
            .collect();
        let sources: Vec<PackageSource<'_>> = files
            .iter()
            .map(|(path, text)| PackageSource::new(path, text))
            .collect();
        let files_max = u32::try_from(files.len()).expect("test file count");
        let bytes_max = files
            .iter()
            .map(|(_, text)| u64::try_from(text.len()).expect("test byte count"))
            .sum();
        let input = ExactPackageInput::new(
            &package,
            &language,
            &origin,
            &sources,
            ExactPackageLimits::new(files_max, bytes_max),
        )
        .expect("bounded package input");
        PackageAnalyzer::analyze(input, 1).expect("the package analyzes")
    }

    /// One package of `files` in `shipped`, analyzed.
    fn analyzed(
        shipped: ShippedLanguage,
        files: Vec<(&str, &str)>,
    ) -> rift_protocol::index::PackagePublication {
        package_analysis(shipped, files).publication().clone()
    }

    /// One package of `files` in `shipped`, analyzed, keeping every part of the analysis.
    fn analysis(shipped: ShippedLanguage, files: Vec<(&str, &str)>) -> super::PackageAnalysis {
        package_analysis(shipped, files)
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

    #[test]
    fn test_attached_comment_publishes_exact_range_and_symbol_owner() {
        let source = "/// Spawns a task.\npub fn spawn_task() {}\n";
        let analysis = package_analysis(ShippedLanguage::Rust, vec![("src/lib.rs", source)]);
        let index = &analysis.publication().documentation;
        let block = index.blocks.first().expect("attached comment block");
        let expected_end = u64::try_from("/// Spawns a task.\n".len()).expect("fixture length");

        assert_eq!(
            block.kind,
            rift_protocol::documentation::DocumentationBlockKind::Prose
        );
        assert_eq!(
            block.range,
            rift_protocol::read::TextRange {
                start: 0,
                end: expected_end
            }
        );
        assert_eq!(
            &source[usize::try_from(block.range.start).expect("range start")
                ..usize::try_from(block.range.end).expect("range end")],
            "/// Spawns a task.\n"
        );
        assert_eq!(
            block.symbol.as_ref().map(|symbol| symbol.0.as_str()),
            Some("rift://symbol/rust/cargo/beacon@1.0.0/src/lib.rs/spawn_task")
        );
        assert_eq!(
            block.chunks[0].identity,
            "rift://source/cargo/beacon@1.0.0/src/lib.rs"
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
