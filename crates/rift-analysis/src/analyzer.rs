//! The package analyzer: one package's source bytes into one canonical publication.
//!
//! [`PackageAnalyzer`] holds no I/O. It takes the bytes a caller already read, parses
//! them with the shipped syntax providers, places each file under the package's own
//! identity, assembles the declarations through [`WorkspaceSemantics`], and renders the
//! result as a [`PackagePublication`], the shape global ingestion consumes.
//!
//! Every record carries the digest of its own canonical content, and the publication as a
//! whole renders as RFC 8785 canonical JSON, so two runs over the same bytes under the
//! same analyzer revision compare byte for byte.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::Path;

use crate::documentation::{
    DocumentationDeclaration, DocumentationInput, DocumentationSourceSet, collect_documentation,
    content_chunk_identity, content_digest,
};
use crate::input::{ExactPackageInput, RetainedSourceLimits};
use crate::revision::analyzer_revision;
use crate::selection::documentation_format;
use crate::semantic::{PlacedFacts, WorkspaceSemantics};
use crate::source::IndexedFile;
use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_core::line::{line_of, line_starts};
use rift_core::{
    ContributionOrigin, ProjectPath as CoreProjectPath, SourceKind,
    SourceUnitId as CoreSourceUnitId, symbol_identity,
};
use rift_error::{RiftError, errors};
use rift_protocol::canonical::canonical_json;
use rift_protocol::documentation::{
    DocumentationChunk, DocumentationContentIdentity, DocumentationSelectionReason,
    DocumentationSource, DocumentationSourceFormat, DocumentationSourceIdentity,
    DocumentationWarningKind, NotebookCellKind,
};
use rift_protocol::identity::SymbolOwner;
use rift_protocol::index::{
    PACKAGE_PUBLICATION_FORMAT_REVISION, PACKAGE_SOURCE_BYTES_MAX, PackageAnalysisWarning,
    PackageDocument, PackageDocumentKind, PackagePublication, PackageSourceUnit, PackageSymbol,
};
use rift_protocol::read::{
    Digest, ExactKind, Language, ProjectPath, SourceLocationKind, SourceUnitId, SymbolFacet,
    SymbolId, SymbolOrigin, TextRange,
};
use rift_syntax::{DocumentPlacement, ShippedLanguage, SyntaxFacts, SyntaxSymbol};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use rift_ranking::split_identifier_words;

#[cfg(test)]
mod fixture;
mod join;
#[cfg(test)]
mod join_tests;
#[cfg(test)]
mod limits_tests;
#[cfg(test)]
mod retained_tests;

use join::ModuleRole;
pub use join::StubForm;

pub(super) fn package_label(owner: &SymbolOwner) -> String {
    match owner {
        SymbolOwner::Package {
            manager,
            name,
            version,
            ..
        } => format!("{manager}/{name}@{version}"),
        SymbolOwner::Runtime { runtime, version } => format!("stdlib/{runtime}@{version}"),
        SymbolOwner::Local => "local".to_owned(),
        SymbolOwner::NamedLocal { name } => format!("local@{name}"),
    }
}

/// One package file as the analyzer holds it: the parsed document, where it is filed, the
/// public declarations it carries, and what the stub and module join decided for it.
#[derive(Debug)]
pub struct AnalyzedFile {
    file: IndexedFile,
    placement: DocumentPlacement,
    public_names: BTreeSet<String>,
    role: ModuleRole,
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

    /// The stub declarations the declaration spelled by `qualified_name` answers for, in the
    /// stub's source order: each one's identity, stub path, and range. Empty unless this
    /// file is an implementation whose declaration joined its stub.
    #[must_use]
    pub fn stub_forms(&self, qualified_name: &str) -> &[StubForm] {
        self.role.stub_forms(qualified_name)
    }
}

/// What one analyzer run produced: the canonical publication, and the parsed material the
/// same pass built it from.
///
/// The publication is the artifact a consumer stores and compares. The parsed files and
/// the assembled graph are the same pass's working values, handed on so a consumer reads
/// them without parsing the package a second time.
#[derive(Debug)]
pub struct PackageAnalysis {
    publication: PackagePublication,
    files: Vec<AnalyzedFile>,
    semantics: WorkspaceSemantics,
    notebook_cells: BTreeMap<DocumentationContentIdentity, String>,
    syntax_work: crate::PackageSyntaxWork,
    warnings: Vec<rift_protocol::read::ReadWarning>,
}

impl PackageAnalysis {
    /// The canonical publication this run produced.
    #[must_use]
    pub const fn publication(&self) -> &PackagePublication {
        &self.publication
    }

    /// Returns actual syntax provider calls and accepted reuse for this analysis.
    #[must_use]
    pub const fn syntax_work(&self) -> crate::PackageSyntaxWork {
        self.syntax_work
    }

    /// Framework context that could not be resolved from captured sources.
    #[must_use]
    pub fn warnings(&self) -> &[rift_protocol::read::ReadWarning] {
        &self.warnings
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

fn analyzed_file(
    input: &ExactPackageInput<'_>,
    file: crate::PackageSource<'_>,
    supplied: &mut impl FnMut(&crate::PackageSyntaxSource<'_>) -> Option<crate::PackageSyntax>,
    work: &mut crate::PackageSyntaxWork,
    context: &crate::FrameworkContext,
    warnings: &mut Vec<rift_protocol::read::ReadWarning>,
) -> Result<AnalyzedFile, RiftError> {
    let source = crate::PackageSyntaxSource::new(
        file,
        input.owner(),
        input.language(),
        input.limits().syntax(),
    );
    let candidate = supplied(&source).filter(|facts| source.accepts(facts));
    let accepted = candidate.is_some();
    let syntax = match candidate {
        Some(syntax) => syntax,
        None => source.parse()?,
    };
    work.provider_calls = work
        .provider_calls
        .checked_add(source.provider_calls())
        .expect("package syntax provider call count must fit u64");
    if accepted && source.provider_calls() == 0 && context.for_path(file.path()).is_none() {
        work.reused_files += 1;
    }
    let facts = if context.for_path(file.path()).is_some() {
        let document = source.parse_document()?;
        let (document, found, calls) = context.apply(
            rift_syntax::SyntaxSource {
                path: file.path(),
                text: file.text(),
            },
            input.limits().syntax(),
            document,
        )?;
        work.provider_calls += 1 + calls;
        warnings.extend(found);
        document.shared_facts()
    } else {
        std::sync::Arc::clone(syntax.facts())
    };
    let parsed = IndexedFile::new_with_shared_syntax(
        file.path().clone(),
        file.text().to_owned().into(),
        source.identity().source_digest,
        false,
        facts,
    );
    let placement = placement_of(input.owner(), input.origin(), file.path())?;
    let public_names = public_qualified_names(parsed.syntax().language(), parsed.syntax());
    Ok(AnalyzedFile {
        file: parsed,
        placement,
        public_names,
        role: ModuleRole::Unpaired,
    })
}

impl PackageAnalyzer {
    /// Analyzes one exact package's selected files.
    ///
    /// Each file retains its original path under the exact package or runtime source
    /// owner. Its origin is the input origin with the same complete owner.
    /// A stub and the module it declares (`mod.pyi` and `mod.py`, `index.d.ts` and
    /// `index.js`) join: each name both declare answers once, at the module, with the
    /// stub's signatures and types. Records are emitted in unit, symbol, and document
    /// identity order, and a collection that reaches its bound stops there and reports the
    /// stop as a warning.
    ///
    /// The work is proportional to the selected bytes: raw parsing and framework
    /// context passes, one scan per file for its line starts, one assembly pass over the
    /// parsed declarations, and one canonical rendering per record. Files with framework
    /// context parse current source before applying framework facts.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the identity cannot spell a resolver or unit,
    /// when no provider parses a file or its parse fails, or when publication or
    /// normalization refuses the package graph.
    pub fn analyze(
        input: ExactPackageInput<'_>,
        revision: u64,
    ) -> Result<PackageAnalysis, RiftError> {
        Self::analyze_with_syntax(input, revision, |_| None)
    }

    /// Assembles a package with optional per-file syntax supplied by the caller.
    ///
    /// The hook runs once per selected file in input order. Its source exposes the actual
    /// provider identity and bounds before parsing. A missing or incompatible value runs
    /// the current provider. Call `source.parse()` inside the hook to retain successful
    /// file work before a later failure; parser calls remain included in `syntax_work()`.
    ///
    /// Every call rebuilds current placement, module joins, semantics, documentation
    /// relationships, and publication. Retention, encoding, and lookup remain caller-owned.
    /// The work is proportional to selected bytes and declarations plus caller hook work.
    ///
    /// # Errors
    ///
    /// Returns the same source and assembly failures as [`Self::analyze`].
    pub fn analyze_with_syntax(
        input: ExactPackageInput<'_>,
        revision: u64,
        mut supplied: impl FnMut(&crate::PackageSyntaxSource<'_>) -> Option<crate::PackageSyntax>,
    ) -> Result<PackageAnalysis, RiftError> {
        let package = input.owner();
        let sources = input
            .files()
            .iter()
            .chain(input.context_sources())
            .copied()
            .collect::<Vec<_>>();
        let context = crate::FrameworkContext::resolve(
            &sources,
            input.frameworks(),
            input.limits().syntax(),
            &|| false,
        )?;
        let mut warnings = context.warnings().to_vec();
        let mut syntax_work = crate::PackageSyntaxWork {
            provider_calls: context.provider_calls(),
            ..crate::PackageSyntaxWork::default()
        };
        let mut analyzed = Vec::with_capacity(input.files().len());
        for file in input.files() {
            analyzed.push(analyzed_file(
                &input,
                *file,
                &mut supplied,
                &mut syntax_work,
                &context,
                &mut warnings,
            )?);
        }
        analyzed.sort_by(|left, right| left.file.path().cmp(right.file.path()));
        join::join_modules(&mut analyzed);
        let placed = analyzed
            .iter()
            .map(|held| PlacedFacts {
                facts: held.file.syntax(),
                path: held.file.path(),
                placement: held.placement.clone(),
            })
            .collect::<Vec<_>>();
        let built = WorkspaceSemantics::build_facts_placed_with_relationships(
            &placed,
            input.limits().declarations_max(),
            input.limits().relationships_max(),
            revision,
            None,
        )
        .map_err(|error| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .cause(error)
                .error()
        })?;
        if let Some(path) = built.beyond_declaration_bound.first() {
            return errors::analysis::package_declarations_exceeded()
                .package(package_label(package))
                .path(path.as_str())
                .fail();
        }
        let (publication, notebook_cells) = publish(
            package,
            input.origin(),
            input.language(),
            &analyzed,
            &built.semantics,
            input.limits(),
        )?;
        Ok(PackageAnalysis {
            publication,
            files: analyzed,
            semantics: built.semantics,
            notebook_cells,
            syntax_work,
            warnings,
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
    package: &SymbolOwner,
    source_origin: &ContributionOrigin,
    package_language: &Language,
    analyzed: &[AnalyzedFile],
    semantics: &WorkspaceSemantics,
    limits: crate::ExactPackageLimits,
) -> Result<
    (
        PackagePublication,
        BTreeMap<DocumentationContentIdentity, String>,
    ),
    RiftError,
> {
    let origin = symbol_origin(package, source_origin)?;
    let mut records = Records {
        retained_source: limits.retained_source(),
        publication: limits.publication(),
        documentation: limits.documentation(),
        ..Records::default()
    };
    for held in analyzed {
        if records.units.len() >= bound(records.publication.units) {
            records.warn(truncation("units", u64::from(records.publication.units)));
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
            owner: package.clone(),
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
    package: &SymbolOwner,
    package_language: &Language,
    origin: &SymbolOrigin,
    analyzed: &[AnalyzedFile],
    records: &mut Records,
) -> Result<
    (
        rift_protocol::documentation::DocumentationIndex,
        BTreeMap<DocumentationContentIdentity, String>,
    ),
    RiftError,
> {
    let DecodedPackageNotebooks {
        notebooks,
        omissions,
    } = decode_package_notebooks(analyzed, &records.documentation).map_err(|error| {
        errors::analysis::package_provider_failed()
            .package(package_label(package))
            .cause(error)
            .error()
    })?;
    let mut inputs = PackageDocumentationInputs {
        inputs: Vec::new(),
        omissions,
        input_bytes: 0,
        limits: records.documentation,
        retained_bytes_max: u64::from(
            records
                .retained_source
                .map_or(PACKAGE_SOURCE_BYTES_MAX, |limits| limits.record_bytes_max),
        ),
    };
    append_notebook_inputs(
        package,
        package_language,
        origin,
        analyzed,
        &notebooks,
        records,
        &mut inputs,
    )?;
    append_regular_inputs(package, origin, analyzed, &mut inputs)?;
    append_attached_comment_inputs(analyzed, origin, package, &mut inputs)?;
    let sources =
        DocumentationSourceSet::with_limits(inputs.inputs, &inputs.limits).map_err(|error| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .cause(error)
                .error()
        })?;
    let declarations = package_declarations(package, analyzed, records)?;
    let notebook_cells = package_notebook_cells(analyzed, &notebooks);
    let collection = collect_documentation(&sources, &declarations)
        .map_err(|error| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .cause(error)
                .error()
        })?
        .with_source_omissions(inputs.omissions)
        .map_err(|error| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .cause(error)
                .error()
        })?;
    Ok((collection.into_index(), notebook_cells))
}

struct PackageDocumentationInputs<'source> {
    inputs: Vec<DocumentationInput<'source>>,
    omissions: Vec<(DocumentationContentIdentity, DocumentationWarningKind)>,
    input_bytes: u64,
    limits: crate::documentation::DocumentationLimits,
    retained_bytes_max: u64,
}

impl PackageDocumentationInputs<'_> {
    fn ensure_next(&self) -> Result<(), RiftError> {
        self.limits.check_source_count(
            self.inputs
                .len()
                .saturating_add(self.omissions.len())
                .saturating_add(1),
        )
    }

    fn omit(
        &mut self,
        identity: DocumentationContentIdentity,
        kind: DocumentationWarningKind,
    ) -> Result<(), RiftError> {
        self.ensure_next()?;
        self.omissions.push((identity, kind));
        Ok(())
    }
}

fn append_notebook_inputs<'source>(
    package: &SymbolOwner,
    package_language: &Language,
    origin: &SymbolOrigin,
    analyzed: &[AnalyzedFile],
    notebooks: &'source [Option<crate::documentation::notebook::NotebookContent>],
    records: &mut Records,
    inputs: &mut PackageDocumentationInputs<'source>,
) -> Result<(), RiftError> {
    for (held, notebook) in analyzed.iter().zip(notebooks) {
        let Some(notebook) = notebook else {
            continue;
        };
        for cell in notebook.cells() {
            append_notebook_cell(
                package,
                package_language,
                origin,
                held,
                cell,
                records,
                inputs,
            )?;
        }
    }
    Ok(())
}

fn append_notebook_cell<'source>(
    package: &SymbolOwner,
    package_language: &Language,
    origin: &SymbolOrigin,
    held: &AnalyzedFile,
    cell: &'source crate::documentation::notebook::NotebookCellContent,
    records: &mut Records,
    inputs: &mut PackageDocumentationInputs<'source>,
) -> Result<(), RiftError> {
    inputs.ensure_next().map_err(|error| {
        errors::analysis::package_provider_failed()
            .package(package_label(package))
            .cause(error)
            .error()
    })?;
    let unit = wire_unit(held.placement.unit());
    let identity = DocumentationContentIdentity {
        source: DocumentationSourceIdentity::Package { unit: unit.clone() },
        cell: Some(cell.cell().clone()),
    };
    let source_bytes = u64::try_from(cell.text().len()).unwrap_or(u64::MAX);
    if source_bytes > inputs.limits.source_bytes_max()
        || inputs.input_bytes.saturating_add(source_bytes) > inputs.limits.total_bytes_max()
    {
        inputs
            .omit(identity, DocumentationWarningKind::SourceUnavailable)
            .map_err(|error| {
                errors::analysis::package_provider_failed()
                    .package(package_label(package))
                    .cause(error)
                    .error()
            })?;
        return Ok(());
    }
    let source = documentation_source(
        identity.clone(),
        cell.text(),
        origin,
        DocumentationSourceFormat::Notebook,
        cell.declared_language().cloned(),
        cell.physical_ranges().to_vec(),
    );
    let mut input = match DocumentationInput::with_limits(source, cell.text(), &inputs.limits) {
        Ok(input) => input,
        Err(error) => {
            inputs
                .omit(identity, documentation_warning_kind(&error))
                .map_err(|error| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(package))
                        .cause(error)
                        .error()
                })?;
            return Ok(());
        }
    };
    let document_id = match content_chunk_identity(&identity, 0) {
        Ok(document_id) => document_id,
        Err(error) => {
            inputs
                .omit(identity, documentation_warning_kind(&error))
                .map_err(|error| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(package))
                        .cause(error)
                        .error()
                })?;
            return Ok(());
        }
    };
    if source_bytes <= inputs.retained_bytes_max && !cell.text().is_empty() {
        input = match input.with_chunks(vec![DocumentationChunk {
            identity: document_id.clone(),
            range: TextRange {
                start: 0,
                end: u64::try_from(cell.text().len()).unwrap_or(u64::MAX),
            },
        }]) {
            Ok(input) => input,
            Err(error) => {
                inputs
                    .omit(identity, documentation_warning_kind(&error))
                    .map_err(|error| {
                        errors::analysis::package_provider_failed()
                            .package(package_label(package))
                            .cause(error)
                            .error()
                    })?;
                return Ok(());
            }
        };
    }
    inputs.inputs.push(input);
    inputs.input_bytes = inputs.input_bytes.saturating_add(source_bytes);
    add_notebook_document(
        package,
        package_language,
        held,
        cell,
        &unit,
        document_id,
        records,
    )
}

fn add_notebook_document(
    package: &SymbolOwner,
    package_language: &Language,
    held: &AnalyzedFile,
    cell: &crate::documentation::notebook::NotebookCellContent,
    unit: &SourceUnitId,
    document_id: String,
    records: &mut Records,
) -> Result<(), RiftError> {
    if !records.document_capacity() {
        records.warn_document_full();
        return Ok(());
    }
    let language = match cell.cell().kind {
        NotebookCellKind::Markdown => ShippedLanguage::Markdown.language(),
        NotebookCellKind::Code => cell
            .declared_language()
            .cloned()
            .unwrap_or_else(|| package_language.clone()),
    };
    let path = wire_path(held.file.path());
    let retained = records.retained(cell.text(), &path, 1)?;
    let name = file_name(&path);
    records.document(PackageDocument {
        identity: document_id,
        kind: PackageDocumentKind::File,
        unit: unit.clone(),
        language,
        owner: package.clone(),
        content_digest: text_digest(&retained.text),
        identifier_terms: identifier_terms_with_limit(
            &[&name],
            records.publication.identifier_terms,
        ),
        name,
        qualified_name: None,
        signature: None,
        documentation: None,
        declaration_source: None,
        file_content: Some(retained.text),
        digest: Digest(String::new()),
    })
}

fn append_regular_inputs<'source>(
    package: &SymbolOwner,
    origin: &SymbolOrigin,
    analyzed: &'source [AnalyzedFile],
    inputs: &mut PackageDocumentationInputs<'source>,
) -> Result<(), RiftError> {
    for held in analyzed {
        let Some(format) = documentation_format(held.file.path().as_str()) else {
            continue;
        };
        if format == DocumentationSourceFormat::Notebook {
            continue;
        }
        inputs.ensure_next().map_err(|error| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .cause(error)
                .error()
        })?;
        let identity = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Package {
                unit: wire_unit(held.placement.unit()),
            },
            cell: None,
        };
        let source_bytes = u64::try_from(held.file.source().len()).unwrap_or(u64::MAX);
        if source_bytes > inputs.limits.source_bytes_max()
            || inputs.input_bytes.saturating_add(source_bytes) > inputs.limits.total_bytes_max()
        {
            inputs
                .omit(identity, DocumentationWarningKind::SourceUnavailable)
                .map_err(|error| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(package))
                        .cause(error)
                        .error()
                })?;
            continue;
        }
        match package_file_input(
            origin,
            held,
            format,
            &inputs.limits,
            inputs.retained_bytes_max,
        ) {
            Ok(input) => {
                inputs.inputs.push(input);
                inputs.input_bytes = inputs.input_bytes.saturating_add(source_bytes);
            }
            Err(error) => inputs
                .omit(identity, documentation_warning_kind(&error))
                .map_err(|error| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(package))
                        .cause(error)
                        .error()
                })?,
        }
    }
    Ok(())
}

fn package_file_input<'source>(
    origin: &SymbolOrigin,
    held: &'source AnalyzedFile,
    format: DocumentationSourceFormat,
    limits: &crate::documentation::DocumentationLimits,
    retained_bytes_max: u64,
) -> Result<DocumentationInput<'source>, RiftError> {
    let unit = wire_unit(held.placement.unit());
    let identity = DocumentationContentIdentity {
        source: DocumentationSourceIdentity::Package { unit: unit.clone() },
        cell: None,
    };
    let text = held.file.source();
    let source = documentation_source(identity, text, origin, format, None, Vec::new());
    let mut input = DocumentationInput::with_limits(source, text, limits)?;
    if matches!(
        format,
        DocumentationSourceFormat::Markdown | DocumentationSourceFormat::Mdx
    ) {
        input = input.with_indexed_syntax(held.file.path(), held.file.syntax_facts())?;
    }
    if u64::try_from(text.len()).unwrap_or(u64::MAX) <= retained_bytes_max && !text.is_empty() {
        input = input.with_chunks(vec![DocumentationChunk {
            identity: unit.0,
            range: TextRange {
                start: 0,
                end: u64::try_from(text.len()).unwrap_or(u64::MAX),
            },
        }])?;
    }
    Ok(input)
}

struct DecodedPackageNotebooks {
    notebooks: Vec<Option<crate::documentation::notebook::NotebookContent>>,
    omissions: Vec<(DocumentationContentIdentity, DocumentationWarningKind)>,
}

fn decode_package_notebooks(
    analyzed: &[AnalyzedFile],
    limits: &crate::documentation::DocumentationLimits,
) -> Result<DecodedPackageNotebooks, RiftError> {
    let mut notebooks = Vec::with_capacity(analyzed.len());
    let mut omissions = Vec::new();
    let mut selected = 0_usize;
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
        match crate::documentation::notebook::decode_notebook_with_limits(
            held.file.source(),
            &identity,
            limits,
        ) {
            Ok(notebook) => {
                selected = selected.saturating_add(notebook.cells().len());
                limits.check_source_count(selected)?;
                notebooks.push(Some(notebook));
            }
            Err(error) => {
                selected = selected.saturating_add(1);
                limits.check_source_count(selected)?;
                omissions.push((identity, documentation_warning_kind(&error)));
                notebooks.push(None);
            }
        }
    }
    Ok(DecodedPackageNotebooks {
        notebooks,
        omissions,
    })
}

fn append_attached_comment_inputs<'source>(
    analyzed: &'source [AnalyzedFile],
    origin: &SymbolOrigin,
    package: &SymbolOwner,
    inputs: &mut PackageDocumentationInputs<'source>,
) -> Result<(), RiftError> {
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
        inputs.ensure_next().map_err(|error| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .cause(error)
                .error()
        })?;
        append_attached_comment(held, origin, package, inputs)?;
    }
    Ok(())
}

fn append_attached_comment<'source>(
    held: &'source AnalyzedFile,
    origin: &SymbolOrigin,
    package: &SymbolOwner,
    inputs: &mut PackageDocumentationInputs<'source>,
) -> Result<(), RiftError> {
    let source_text = held.file.source();
    let unit = wire_unit(held.placement.unit());
    let identity = DocumentationContentIdentity {
        source: DocumentationSourceIdentity::Package { unit: unit.clone() },
        cell: None,
    };
    let source_bytes = u64::try_from(source_text.len()).unwrap_or(u64::MAX);
    if source_bytes > inputs.limits.source_bytes_max()
        || inputs.input_bytes.saturating_add(source_bytes) > inputs.limits.total_bytes_max()
    {
        inputs
            .omit(identity, DocumentationWarningKind::SourceUnavailable)
            .map_err(|error| {
                errors::analysis::package_provider_failed()
                    .package(package_label(package))
                    .cause(error)
                    .error()
            })?;
        return Ok(());
    }
    let source = documentation_source(
        identity.clone(),
        source_text,
        origin,
        DocumentationSourceFormat::AttachedComment,
        Some(held.file.syntax().language().clone()),
        Vec::new(),
    );
    let mut input = match DocumentationInput::with_limits(source, source_text, &inputs.limits)
        .and_then(|input| input.with_indexed_syntax(held.file.path(), held.file.syntax_facts()))
    {
        Ok(input) => input,
        Err(error) => {
            inputs
                .omit(identity, documentation_warning_kind(&error))
                .map_err(|error| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(package))
                        .cause(error)
                        .error()
                })?;
            return Ok(());
        }
    };
    if source_bytes <= inputs.retained_bytes_max && !source_text.is_empty() {
        input = match input.with_chunks(vec![DocumentationChunk {
            identity: unit.0,
            range: TextRange {
                start: 0,
                end: u64::try_from(source_text.len()).unwrap_or(u64::MAX),
            },
        }]) {
            Ok(input) => input,
            Err(error) => {
                inputs
                    .omit(identity, documentation_warning_kind(&error))
                    .map_err(|error| {
                        errors::analysis::package_provider_failed()
                            .package(package_label(package))
                            .cause(error)
                            .error()
                    })?;
                return Ok(());
            }
        };
    }
    inputs.inputs.push(input);
    inputs.input_bytes = inputs.input_bytes.saturating_add(source_bytes);
    Ok(())
}

fn package_declarations<'declaration>(
    package: &SymbolOwner,
    analyzed: &'declaration [AnalyzedFile],
    records: &'declaration Records,
) -> Result<Vec<DocumentationDeclaration<'declaration>>, RiftError> {
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
        let language = languages.get(&symbol.unit.0).copied().ok_or_else(|| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .error()
        })?;
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
            .map_err(|error| {
                errors::analysis::package_provider_failed()
                    .package(package_label(package))
                    .cause(error)
                    .error()
            })?,
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

fn documentation_warning_kind(error: &RiftError) -> DocumentationWarningKind {
    if error.slug() == errors::analysis::documentation_limit_exceeded::SLUG {
        DocumentationWarningKind::SourceUnavailable
    } else {
        DocumentationWarningKind::MalformedSource
    }
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
    retained_source: Option<RetainedSourceLimits>,
    retained_source_bytes: u64,
    publication: rift_protocol::configuration::PackageConfiguration,
    documentation: crate::documentation::DocumentationLimits,
}

impl Records {
    /// Records one warning, dropping it once the publication's warning bound is reached:
    /// the bound is what a reader can act on, and the log carries the rest.
    fn warn(&mut self, warning: PackageAnalysisWarning) {
        if self.warnings.len() < bound(self.publication.warnings) {
            self.warnings.push(warning);
        }
    }

    /// Emits one file's unit record, its declarations, and the documents they rank under.
    fn file(
        &mut self,
        package: &SymbolOwner,
        origin: &SymbolOrigin,
        semantics: &WorkspaceSemantics,
        held: &AnalyzedFile,
    ) -> Result<(), RiftError> {
        let language = held.file.syntax().language().clone();
        let path = wire_path(held.file.path());
        let unit = wire_unit(held.placement.unit());
        let source = held.file.source();
        let needs_document = documentation_format(held.file.path().as_str())
            != Some(DocumentationSourceFormat::Notebook);
        let publishes_document = needs_document && self.document_capacity();
        let mut retained = self.retained(source, &path, if publishes_document { 2 } else { 1 })?;
        let content_digest = text_digest(source);
        let mut record = PackageSourceUnit {
            unit: unit.clone(),
            path: path.clone(),
            language: language.clone(),
            content_digest: content_digest.clone(),
            source: if publishes_document {
                retained.text.clone()
            } else {
                std::mem::take(&mut retained.text)
            },
            source_complete: retained.complete,
            digest: Digest(String::new()),
        };
        record.digest = digest_of(&record, package)?;
        self.units.push(record);
        if publishes_document {
            let document_digest = text_digest(&retained.text);
            self.file_document(package, &language, &unit, &path, retained, document_digest)?;
        } else if needs_document {
            self.warn_document_full();
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
        package: &SymbolOwner,
        language: &Language,
        unit: &SourceUnitId,
        path: &ProjectPath,
        retained: RetainedSource,
        content_digest: Digest,
    ) -> Result<(), RiftError> {
        let name = file_name(path);
        let document = PackageDocument {
            identity: unit.0.clone(),
            kind: PackageDocumentKind::File,
            unit: unit.clone(),
            language: language.clone(),
            owner: package.clone(),
            content_digest,
            identifier_terms: identifier_terms_with_limit(
                &[&name],
                self.publication.identifier_terms,
            ),
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
    // The record and its document share one retained-source budget decision.
    #[allow(clippy::too_many_lines)]
    fn declaration(
        &mut self,
        package: &SymbolOwner,
        origin: &SymbolOrigin,
        semantics: &WorkspaceSemantics,
        held: &AnalyzedFile,
        context: &FileContext<'_>,
        declaration: &SyntaxSymbol,
    ) -> Result<(), RiftError> {
        let FileContext {
            language,
            unit,
            path,
            line_starts,
        } = *context;
        if held.role.answers_elsewhere(&declaration.qualified_name) {
            return Ok(());
        }
        if self.symbols.len() >= bound(self.publication.symbols) {
            if !self.symbols_truncated {
                self.symbols_truncated = true;
                self.warn(truncation("symbols", u64::from(self.publication.symbols)));
            }
            return Ok(());
        }
        let source = held.file.source();
        let declared = declaration_source(source, declaration).ok_or_else(|| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .path(Path::new(held.file.path().as_str()))
                .error()
        })?;
        let public = held.is_public(&declaration.qualified_name);
        let publishes_document = public && self.document_capacity();
        let retained = self.retained(declared, path, if publishes_document { 2 } else { 1 })?;
        let content_digest = text_digest(&retained.text);
        let symbol = SymbolId(symbol_identity(
            &language.identity_segment(),
            held.placement.identity_path(),
            &declaration.qualified_name,
        ));
        let mut presentation = semantics
            .assembled(&symbol.0)
            .and_then(|assembled| {
                assembled
                    .facts()
                    .map(|facts| assembled.to_protocol_symbol(facts))
            })
            .ok_or_else(|| {
                errors::analysis::package_provider_failed()
                    .package(package_label(package))
                    .path(Path::new(held.file.path().as_str()))
                    .error()
            })?;
        let signature = if join::lay_join(&mut presentation, semantics, &held.role, declaration) {
            presentation.signatures.first().cloned()
        } else {
            declaration.signatures.first().cloned()
        };
        let mut record = PackageSymbol {
            symbol,
            presentation,
            origin: origin.clone(),
            unit: unit.clone(),
            name: declaration.name.clone(),
            qualified_name: declaration.qualified_name.clone(),
            kind: ExactKind(declaration.kind.to_owned()),
            range: TextRange {
                start: declaration.range.start,
                end: declaration.range.end,
            },
            line: line_of(line_starts, declaration.range.start),
            signature,
            documentation: declaration.documentation.first().cloned(),
            source: retained.text,
            source_complete: retained.complete,
            public,
            digest: Digest(String::new()),
        };
        record.digest = digest_of(&record, package)?;
        if publishes_document {
            let document = PackageDocument {
                identity: record.symbol.0.clone(),
                kind: PackageDocumentKind::Symbol,
                unit: unit.clone(),
                language: language.clone(),
                owner: package.clone(),
                content_digest,
                identifier_terms: identifier_terms_with_limit(
                    &[&record.name, &record.qualified_name],
                    self.publication.identifier_terms,
                ),
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
        } else if public {
            self.warn_document_full();
        }
        self.symbols.push(record);
        Ok(())
    }

    /// Takes one document, or reports the bound once the collection reaches it.
    fn document(&mut self, mut document: PackageDocument) -> Result<(), RiftError> {
        if !self.document_capacity() {
            self.warn_document_full();
            return Ok(());
        }
        document.digest = digest_of(&document, &document.owner.clone())?;
        self.documents.push(document);
        Ok(())
    }

    fn document_capacity(&self) -> bool {
        self.documents.len() < bound(self.publication.documents)
    }

    fn warn_document_full(&mut self) {
        if !self.documents_truncated {
            self.documents_truncated = true;
            self.warn(truncation(
                "documents",
                u64::from(self.publication.documents),
            ));
        }
    }

    /// The bytes one record retains, reporting a cut once for the record that made it.
    fn retained(
        &mut self,
        source: &str,
        path: &ProjectPath,
        copies: u64,
    ) -> Result<RetainedSource, RiftError> {
        let maximum = self
            .retained_source
            .map_or(PACKAGE_SOURCE_BYTES_MAX, |limits| limits.record_bytes_max);
        let mut kept = source.len().min(bound(maximum));
        while kept > 0 && !source.is_char_boundary(kept) {
            kept -= 1;
        }
        if let Some(limits) = self.retained_source {
            let observed = u64::try_from(kept)
                .ok()
                .and_then(|bytes| bytes.checked_mul(copies))
                .and_then(|bytes| self.retained_source_bytes.checked_add(bytes));
            let bytes = observed
                .filter(|bytes| *bytes <= limits.total_bytes_max)
                .ok_or_else(|| {
                    errors::analysis::package_retained_source_bytes_exceeded()
                        .bound(limits.total_bytes_max)
                        .observed(observed.unwrap_or(u64::MAX))
                        .error()
                })?;
            self.retained_source_bytes = bytes;
        }
        let complete = kept == source.len();
        if !complete {
            self.warn(PackageAnalysisWarning::SourceTruncated {
                path: path.clone(),
                dropped: u64::try_from(source.len() - kept).unwrap_or(u64::MAX),
            });
        }
        Ok(RetainedSource {
            text: source[..kept].to_owned(),
            complete,
        })
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
/// `PACKAGE_IDENTIFIER_TERMS_MAX` of them by default.
///
/// The cut is a work bound, not a decision: the terms a ranking reads come from a
/// declaration's own name, and a name splitting into more than a thousand words ranks on
/// the first thousand.
#[cfg(test)]
fn identifier_terms(names: &[&str]) -> Vec<String> {
    identifier_terms_with_limit(names, rift_protocol::index::PACKAGE_IDENTIFIER_TERMS_MAX)
}

/// Splits identifier words under the accepted publication bound.
fn identifier_terms_with_limit(names: &[&str], maximum: u32) -> Vec<String> {
    let terms_max = bound(maximum);
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

/// The origin every declaration of one analyzed package carries.
fn symbol_origin(
    owner: &SymbolOwner,
    origin: &ContributionOrigin,
) -> Result<SymbolOrigin, RiftError> {
    let result = match origin.location() {
        Some(rift_core::SourceLocation::Dependency { package })
            if package.owner().ok().as_ref() == Some(owner) =>
        {
            SymbolOrigin {
                location: Some(SourceLocationKind::Dependency),
                package: Some(package.clone()),
                runtime: None,
                source_kind: SourceKind::Authored,
            }
        }
        Some(rift_core::SourceLocation::Stdlib {
            runtime: Some(runtime),
        }) if runtime.owner().ok().as_ref() == Some(owner) => SymbolOrigin {
            location: Some(SourceLocationKind::Stdlib),
            package: None,
            runtime: Some(runtime.clone()),
            source_kind: SourceKind::Authored,
        },
        _ => {
            return errors::analysis::package_identity_invalid()
                .package(package_label(owner))
                .fail();
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
fn digest_of<T: Serialize>(record: &T, package: &SymbolOwner) -> Result<Digest, RiftError> {
    let mut value = serde_json::to_value(record).map_err(|error| {
        errors::analysis::package_provider_failed()
            .package(package_label(package))
            .source(error)
            .error()
    })?;
    if let Some(object) = value.as_object_mut() {
        object.remove("digest");
    }
    let rendered = canonical_json(&value).map_err(|error| {
        errors::analysis::package_provider_failed()
            .package(package_label(package))
            .source(error)
            .error()
    })?;
    Ok(text_digest(&rendered))
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

/// One package file parsed by the provider its extension names, under `syntax_limits`.
#[cfg(test)]
fn parsed_file(
    file: crate::input::PackageSource<'_>,
    package: &SymbolOwner,
    package_language: &Language,
    syntax_limits: rift_syntax::SyntaxLimits,
) -> Result<IndexedFile, RiftError> {
    Ok(IndexedFile::new_with_shared_syntax(
        file.path().clone(),
        file.text().to_owned().into(),
        crate::FileDigest::of(file.text().as_bytes()),
        false,
        std::sync::Arc::clone(
            crate::PackageSyntaxSource::new(file, package, package_language, syntax_limits)
                .parse()?
                .facts(),
        ),
    ))
}

pub(super) fn source_language(extension: &str, package_language: &Language) -> Language {
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

/// The placement of one package file: its unit, identity path, and origin.
fn placement_of(
    package: &SymbolOwner,
    origin: &ContributionOrigin,
    path: &CoreProjectPath,
) -> Result<DocumentPlacement, RiftError> {
    let unit = CoreSourceUnitId::for_owner(package.clone(), path.as_str()).map_err(|error| {
        errors::analysis::package_identity_invalid()
            .package(package_label(package))
            .path(Path::new(path.as_str()))
            .cause(error)
            .error()
    })?;
    let address = unit.to_string();
    let identity_path = address
        .strip_prefix("rift://source/")
        .ok_or_else(|| {
            errors::analysis::package_identity_invalid()
                .package(package_label(package))
                .path(Path::new(path.as_str()))
                .error()
        })?
        .to_owned();
    Ok(DocumentPlacement::new(origin.clone(), unit, identity_path))
}

/// The languages package analysis reads an API from, and each one's rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageLanguage {
    /// Rust declarations use `pub`; items of a public trait and variants of a public enum
    /// also count.
    Rust,
    /// Python names without a leading underscore count.
    Python,
    /// TypeScript declarations exclude private and protected members. A TypeScript package
    /// ships its JavaScript builds beside its declaration files.
    TypeScript,
    /// HTML elements and attributes are public declarations.
    Html,
    /// Angular template elements and attributes are public declarations.
    HtmlAngular,
    /// CSS selectors and properties are public declarations.
    Css,
    /// Vue elements and script declarations are public unless marked private or protected.
    Vue,
    /// Svelte elements and script declarations are public unless marked private or protected.
    Svelte,
    /// C declarations are public.
    C,
    /// C++ declarations are public.
    Cpp,
    /// Cython names without a leading underscore count.
    Cython,
    /// JSONC properties are public declarations.
    Jsonc,
}

/// Rust container kinds whose private members a `pub` container exports: a trait's
/// items and an enum's variants carry no visibility of their own.
const PUBLIC_MEMBER_CONTAINERS: [&str; 2] = ["trait", "enum"];

/// JavaScript container kinds whose members an exported container exports: a class's
/// methods carry no `export` of their own.
const JAVASCRIPT_PUBLIC_MEMBER_CONTAINERS: [&str; 1] = ["class"];

/// The prefix of an ES private element's name (`#secret`), which nothing outside its
/// class reaches.
const PRIVATE_ELEMENT_PREFIX: char = '#';

/// The shipped language a definition serves `language` under.
fn shipped_language(language: &Language) -> Option<ShippedLanguage> {
    rift_syntax::definitions()
        .iter()
        .map(|definition| definition.shipped())
        .find(|shipped| &shipped.language() == language)
}

impl PackageLanguage {
    /// Rules for one language, when a shipped definition serves it.
    #[must_use]
    pub fn for_language(language: &Language) -> Option<Self> {
        match shipped_language(language)? {
            ShippedLanguage::Rust => Some(Self::Rust),
            ShippedLanguage::Python => Some(Self::Python),
            ShippedLanguage::TypeScript | ShippedLanguage::TypeScriptTsx => Some(Self::TypeScript),
            ShippedLanguage::Html => Some(Self::Html),
            ShippedLanguage::HtmlAngular => Some(Self::HtmlAngular),
            ShippedLanguage::Css => Some(Self::Css),
            ShippedLanguage::Vue => Some(Self::Vue),
            ShippedLanguage::Svelte => Some(Self::Svelte),
            ShippedLanguage::C => Some(Self::C),
            ShippedLanguage::Cpp => Some(Self::Cpp),
            ShippedLanguage::Cython => Some(Self::Cython),
            ShippedLanguage::Jsonc => Some(Self::Jsonc),
            ShippedLanguage::JavaScript
            | ShippedLanguage::Markdown
            | ShippedLanguage::Json
            | ShippedLanguage::Yaml
            | ShippedLanguage::Toml => None,
        }
    }

    /// The shipped languages whose files a package in this language holds as source.
    pub(crate) const fn source_languages(self) -> &'static [ShippedLanguage] {
        match self {
            Self::Rust => &[ShippedLanguage::Rust],
            Self::Python => &[ShippedLanguage::Python, ShippedLanguage::Cython],
            Self::TypeScript => &[
                ShippedLanguage::JavaScript,
                ShippedLanguage::TypeScript,
                ShippedLanguage::TypeScriptTsx,
                ShippedLanguage::Html,
                ShippedLanguage::Css,
                ShippedLanguage::Vue,
                ShippedLanguage::Svelte,
            ],
            Self::Html | Self::HtmlAngular => &[ShippedLanguage::Html, ShippedLanguage::Css],
            Self::Css => &[ShippedLanguage::Css],
            Self::Vue => &[
                ShippedLanguage::Vue,
                ShippedLanguage::Html,
                ShippedLanguage::Css,
                ShippedLanguage::JavaScript,
                ShippedLanguage::TypeScript,
                ShippedLanguage::TypeScriptTsx,
            ],
            Self::Svelte => &[
                ShippedLanguage::Svelte,
                ShippedLanguage::Html,
                ShippedLanguage::Css,
                ShippedLanguage::JavaScript,
                ShippedLanguage::TypeScript,
                ShippedLanguage::TypeScriptTsx,
            ],
            Self::C | Self::Cpp => &[ShippedLanguage::C, ShippedLanguage::Cpp],
            Self::Cython => &[ShippedLanguage::Cython, ShippedLanguage::Python],
            Self::Jsonc => &[ShippedLanguage::Jsonc],
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
                        PUBLIC_MEMBER_CONTAINERS.contains(&container.kind)
                            && container.visibility.as_deref() == Some("pub")
                    }),
                _ => false,
            },
            Self::Python | Self::Cython => !symbol.name.starts_with('_'),
            Self::TypeScript | Self::Vue | Self::Svelte => !symbol
                .visibility
                .as_deref()
                .is_some_and(|visibility| matches!(visibility, "private" | "protected")),
            Self::Html | Self::HtmlAngular | Self::Css | Self::C | Self::Cpp | Self::Jsonc => true,
        }
    }
}

/// The rule deciding which declarations of one package file are public.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportRule {
    /// A package language's own rule.
    Language(PackageLanguage),
    /// A JavaScript module exports a declaration the provider marks `Public`, one an
    /// `export` wraps or an export names (`export { a }`, `module.exports = { a }`), and a
    /// member of a class so marked other than an ES private element.
    JavaScriptModule,
}

impl ExportRule {
    /// The rule for files in `language`. Absent for a language with no export rule, whose
    /// every declaration is public.
    fn for_language(language: &Language) -> Option<Self> {
        if shipped_language(language)? == ShippedLanguage::JavaScript {
            return Some(Self::JavaScriptModule);
        }
        PackageLanguage::for_language(language).map(Self::Language)
    }

    fn is_public(self, symbol: &SyntaxSymbol, by_name: &BTreeMap<&str, &SyntaxSymbol>) -> bool {
        match self {
            Self::Language(rules) => rules.is_public(symbol, by_name),
            Self::JavaScriptModule => is_javascript_export(symbol, by_name),
        }
    }
}

/// Whether a JavaScript module exports `symbol`: the provider marks it `Public`, or it is a
/// member of a class so marked and no ES private element.
fn is_javascript_export(symbol: &SyntaxSymbol, by_name: &BTreeMap<&str, &SyntaxSymbol>) -> bool {
    let exported = is_exported(symbol);
    let private_element = symbol.name.starts_with(PRIVATE_ELEMENT_PREFIX);
    let member_of_exported_class = symbol
        .container
        .as_deref()
        .and_then(|container| by_name.get(container))
        .is_some_and(|container| {
            JAVASCRIPT_PUBLIC_MEMBER_CONTAINERS.contains(&container.kind) && is_exported(container)
        });
    exported || (member_of_exported_class && !private_element)
}

/// Whether the provider marks `symbol` public, as an export of it does.
fn is_exported(symbol: &SyntaxSymbol) -> bool {
    symbol.facets.contains(&SymbolFacet::Public)
}

/// Qualified names of declarations a package file exposes under its language's export
/// rule.
///
/// A paired module's set is replaced afterwards by the one its stub defines, so this rule
/// decides an unpaired file alone.
#[must_use]
pub fn public_qualified_names(language: &Language, facts: &SyntaxFacts) -> BTreeSet<String> {
    let symbols = facts.symbols();
    let Some(rule) = ExportRule::for_language(language) else {
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
        .filter(|symbol| rule.is_public(symbol, &by_name))
        .map(|symbol| symbol.qualified_name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
    use rift_protocol::canonical::canonical_json;
    use rift_protocol::documentation::{
        DocumentationContentIdentity, DocumentationSourceIdentity, DocumentationStage,
        DocumentationWarningKind,
    };
    use rift_protocol::index::{
        PACKAGE_IDENTIFIER_TERMS_MAX, PACKAGE_PUBLICATION_FORMAT_REVISION,
        PACKAGE_SOURCE_BYTES_MAX, PackageAnalysisWarning, PackageDocumentKind,
    };
    use rift_protocol::schema::package_index_schema_document;
    use rift_syntax::{ShippedLanguage, SyntaxLimits};

    use super::fixture::{analyzed, identity, language, package_analysis, package_result};
    use super::{PackageAnalyzer, RiftError, bound, package_label};
    use crate::{ExactPackageInput, ExactPackageLimits, PackageSource};
    use rift_error::errors;

    /// One package of `files` in `shipped`, analyzed, keeping every part of the analysis.
    fn analysis(shipped: ShippedLanguage, files: Vec<(&str, &str)>) -> super::PackageAnalysis {
        package_analysis(shipped, files)
    }

    #[test]
    fn runtime_publication_preserves_exact_owner_and_document_origin() {
        let runtime = rift_protocol::read::RuntimeIdentity {
            runtime: "cpython".to_owned(),
            version: "3.14.3".to_owned(),
        };
        let owner = runtime.owner().expect("runtime owner");
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Stdlib {
                runtime: Some(runtime.clone()),
            }),
            SourceKind::Authored,
        )
        .expect("exact runtime origin");
        let source_language = language(ShippedLanguage::Python);
        let path = ProjectPath::new("sys/__init__.pyi").expect("runtime unit");
        let readme_path = ProjectPath::new("README.md").expect("runtime documentation");
        let source = "def exit(status: int) -> None: ...\n";
        let readme = "# Runtime\n\nExact selected runtime documentation.\n";
        let files = [
            PackageSource::new(&path, source),
            PackageSource::new(&readme_path, readme),
        ];
        let input = ExactPackageInput::new(
            &owner,
            &source_language,
            &origin,
            &files,
            ExactPackageLimits::new(
                2,
                u64::try_from(source.len() + readme.len()).expect("source bytes"),
            ),
        )
        .expect("runtime input");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("runtime publication");
        let publication = analysis.publication();
        assert_eq!(publication.owner, owner);
        assert_eq!(publication.units.len(), 2);
        assert!(publication.units.iter().any(|unit| {
            unit.unit.as_str() == "rift://source/stdlib/cpython@3.14.3/sys/__init__.pyi"
        }));
        assert!(!publication.symbols.is_empty());
        assert!(publication.symbols.iter().all(|binding| {
            binding.origin.package.is_none() && binding.origin.runtime.as_ref() == Some(&runtime)
        }));
        assert!(!publication.documents.is_empty());
        assert!(
            publication
                .documents
                .iter()
                .all(|document| document.owner == owner)
        );
        assert!(publication.documentation.sources.iter().any(|source| {
            source.origin.package.is_none()
                && source.origin.runtime.as_ref() == Some(&runtime)
                && matches!(&source.identity.source,
                    DocumentationSourceIdentity::Package { unit }
                    if unit.as_str() == "rift://source/stdlib/cpython@3.14.3/README.md")
        }));
    }

    #[test]
    fn package_failures_keep_registered_identity_and_ambient_context() {
        let package = identity();
        let child = RiftError::new(
            rift_error::ErrorSlug::new("rift.analysis.documentation_limit_exceeded"),
            "documentation limit exceeded",
            "reduce documentation and retry",
            vec![],
        );
        let provider = errors::analysis::package_provider_failed()
            .package(package_label(&package.owner().expect("fixture owner")))
            .cause(child)
            .error();
        assert_eq!(
            provider.slug().as_str(),
            "rift.analysis.package_provider_failed"
        );
        assert!(
            provider
                .context()
                .any(|(key, value)| key == "package" && value == "cargo/beacon@1.0.0")
        );
        let child = std::error::Error::source(&provider)
            .and_then(|source| source.downcast_ref::<RiftError>())
            .expect("provider failure retains registered child");
        assert_eq!(
            child.slug().as_str(),
            "rift.analysis.documentation_limit_exceeded"
        );

        let path = super::CoreProjectPath::new("src/lib.rs").expect("path");
        let syntax = RiftError::new(
            rift_error::ErrorSlug::new("rift.syntax.parse_cancelled"),
            "parse cancelled",
            "retry",
            vec![],
        );
        let wrapped = syntax
            .with(rift_error::ErrorContext::new(
                "package",
                rift_error::ErrorValue::formatted(package_label(
                    &package.owner().expect("fixture owner"),
                )),
            ))
            .with(rift_error::ErrorContext::new(
                "path",
                rift_error::ErrorValue::path(path.as_str()),
            ));
        assert_eq!(wrapped.slug().as_str(), "rift.syntax.parse_cancelled");
        assert!(
            wrapped
                .context()
                .any(|(key, value)| key == "path" && value == "src/lib.rs")
        );

        let unsupported = super::parsed_file(
            PackageSource::new(&ProjectPath::new("guide.unknown").expect("path"), "source"),
            &package.owner().expect("fixture owner"),
            &language(ShippedLanguage::Rust),
            SyntaxLimits::default(),
        )
        .expect_err("unsupported source extension");
        assert_eq!(
            unsupported.slug().as_str(),
            "rift.analysis.package_syntax_unavailable"
        );
    }

    #[test]
    fn a_package_past_the_declaration_bound_names_the_limit() {
        use std::fmt::Write as _;

        let per_file = rift_provider::CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT / 2 + 1;
        let source = (0..per_file).fold(String::new(), |mut source, index| {
            writeln!(source, "def f{index}():\n    pass").expect("a string write succeeds");
            source
        });
        let files = vec![("pkg/a.py", source.as_str()), ("pkg/b.py", source.as_str())];
        let raised = rift_syntax::SyntaxLimits::new(8 << 20, 4_000_000, 512).expect("bounds");
        let Err(error) = package_result(ShippedLanguage::Python, files, Some(raised)) else {
            panic!("a package past the declaration bound must be refused");
        };
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_declarations_exceeded"
        );
        assert!(
            error
                .context()
                .any(|(key, value)| key == "path" && value == "pkg/b.py")
        );
    }

    #[test]
    fn caller_syntax_limits_replace_each_provider_declared_bounds() {
        let generated = format!("values = [{}]\n", "1, ".repeat(300_000));
        let files = vec![("pkg/generated.py", generated.as_str())];
        let Err(declared) = package_result(ShippedLanguage::Python, files.clone(), None) else {
            panic!("a source past the declared node bound must be refused");
        };
        assert!(declared.slug().as_str().starts_with("rift.syntax."));

        let raised = rift_syntax::SyntaxLimits::new(4 << 20, 1_000_000, 512).expect("bounds");
        assert!(package_result(ShippedLanguage::Python, files, Some(raised)).is_ok());

        let tight = rift_syntax::SyntaxLimits::new(4 << 20, 1, 512).expect("bounds");
        let small = vec![("pkg/small.py", "value = 1\n")];
        assert!(package_result(ShippedLanguage::Python, small.clone(), None).is_ok());
        assert!(package_result(ShippedLanguage::Python, small, Some(tight)).is_err());
    }

    #[test]
    fn plain_text_package_source_keeps_text_language_and_documentation() {
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![("guide.txt", "Package documentation.\n")],
        );
        let document = publication
            .documents
            .iter()
            .find(|document| document.kind == PackageDocumentKind::File)
            .expect("text file document");
        assert_eq!(document.language.name, "text");
        let documentation = &publication.documentation;
        assert_eq!(documentation.sources[0].media_type, "text/plain");
        assert_eq!(documentation.blocks.len(), 1);
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
            ["rift://symbol/rust/cargo/crates.io/beacon@1.0.0/src/lib.rs/spawn"],
            "a document stands for every public declaration and no other"
        );
        assert_eq!(
            document_identities(&publication, PackageDocumentKind::File),
            ["rift://source/cargo/crates.io/beacon@1.0.0/src/lib.rs"]
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
            Some("rift://symbol/rust/cargo/crates.io/beacon@1.0.0/src/lib.rs/spawn_task")
        );
        assert_eq!(
            block.chunks[0].identity,
            "rift://source/cargo/crates.io/beacon@1.0.0/src/lib.rs"
        );
    }

    /// Every shipped package language reaches the publication through the same pass,
    /// each under its own export rule: JavaScript exports what `export` wraps, while
    /// TypeScript and TSX drop a `private` member.
    #[test]
    fn test_every_package_language_publishes_its_public_declarations() {
        let cases = [
            (
                ShippedLanguage::JavaScript,
                "index.js",
                "export function open() {}\nfunction helper() {}\n",
                vec!["open"],
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

    /// A dialect's identity segment carries a colon (`typescript:tsx`), which `ExactKind`'s
    /// pattern refuses, so a declaration's kind is the provider's own word and the language
    /// rides in `presentation.language`. The whole publication then validates against the
    /// package index schema Rift serves.
    #[test]
    fn test_a_tsx_declaration_kind_is_the_provider_word_the_schema_accepts() {
        let publication = analyzed(
            ShippedLanguage::TypeScriptTsx,
            vec![(
                "index.tsx",
                "export class Panel {\n  render(): null {\n    return null;\n  }\n}\n\
                 export function mount(): void {}\n",
            )],
        );

        let kinds: Vec<(&str, &str)> = publication
            .symbols
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.kind.0.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("Panel", "class"),
                ("Panel.render", "method"),
                ("mount", "function")
            ]
        );
        for symbol in &publication.symbols {
            assert_eq!(
                symbol.kind, symbol.presentation.kind,
                "the record and its presentation carry one kind: {}",
                symbol.qualified_name
            );
            assert_eq!(
                symbol.presentation.language,
                language(ShippedLanguage::TypeScriptTsx)
            );
        }
        let schema: serde_json::Value = serde_json::from_str(&package_index_schema_document())
            .expect("the package index schema parses");
        let validator = jsonschema::validator_for(&schema).expect("the schema compiles");
        let instance = serde_json::to_value(&publication).expect("the publication serializes");
        let refusals: Vec<String> = validator
            .iter_errors(&instance)
            .map(|refusal| format!("{}: {refusal}", refusal.instance_path()))
            .collect();
        assert!(
            refusals.is_empty(),
            "a TSX publication validates against the package index schema: {refusals:#?}"
        );
    }

    /// The public names of the one file `path` holds, analyzed alone under `shipped`.
    fn public_names(shipped: ShippedLanguage, path: &str, source: &str) -> Vec<String> {
        analyzed(shipped, vec![(path, source)])
            .symbols
            .iter()
            .filter(|symbol| symbol.public)
            .map(|symbol| symbol.qualified_name.clone())
            .collect()
    }

    /// An unpaired JavaScript module exports what an ES `export` wraps or names, a default
    /// export included, and the members of an exported class other than a `#` private
    /// element. A helper no export wraps or names stays private.
    #[test]
    fn test_an_unpaired_javascript_module_exports_what_export_wraps_or_names() {
        let source = "export function open() {}\n\
                      function helper() {}\n\
                      export class Client {\n  connect() {}\n  static make() {}\n  #hidden() {}\n}\n\
                      class Internal {\n  run() {}\n}\n\
                      export const answer = 42, other = 1;\n\
                      export default function main() {}\n\
                      function later() {}\n\
                      class Session {\n  close() {}\n  #token() {}\n}\n\
                      export { later, Session as Connection };\n";
        let mut public = public_names(ShippedLanguage::JavaScript, "index.js", source);
        public.sort();
        assert_eq!(
            public,
            [
                "Client",
                "Client.connect",
                "Client.make",
                "Session",
                "Session.close",
                "answer",
                "later",
                "main",
                "open",
                "other"
            ]
        );
        for shipped in [ShippedLanguage::TypeScript, ShippedLanguage::TypeScriptTsx] {
            let publication = analyzed(shipped, vec![("lib/index.mjs", source)]);
            let module_public = publication
                .symbols
                .iter()
                .filter(|symbol| symbol.public)
                .count();
            assert_eq!(
                module_public,
                public.len(),
                "a TypeScript package's JavaScript build takes the same rule"
            );
        }
    }

    /// A module exporting through `module.exports` and `exports.name` assignments exports
    /// the declarations they name, the members of an exported class other than a `#`
    /// private element, and a method written in the exported object. The provider declares
    /// nothing for an `exports.name = function` assignment, so it adds no record.
    #[test]
    fn test_a_module_exports_assignment_exports_the_declarations_it_names() {
        let source = "function helper() {}\n\
                      class Runner {\n  run() {}\n  #secret() {}\n}\n\
                      function internal() {}\n\
                      function load() {}\n\
                      module.exports = { helper, Runner, start() {} };\n\
                      exports.read = load;\n\
                      exports.extra = function extra() {};\n";
        let publication = analyzed(ShippedLanguage::JavaScript, vec![("index.cjs", source)]);
        let mut declared: Vec<(&str, bool)> = publication
            .symbols
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.public))
            .collect();
        declared.sort_unstable();
        assert_eq!(
            declared,
            [
                ("Runner", true),
                ("Runner.#secret", false),
                ("Runner.run", true),
                ("helper", true),
                ("internal", false),
                ("load", true),
                ("start", true),
            ]
        );
    }

    /// A paired JavaScript module keeps the public set its declaration file defines: an
    /// `export` the stub does not declare leaves the set.
    #[test]
    fn test_a_paired_javascript_module_keeps_the_stub_public_set() {
        let publication = analyzed(
            ShippedLanguage::TypeScript,
            vec![
                ("index.d.ts", "export declare function open(): void;\n"),
                (
                    "index.js",
                    "export function open() {}\nexport function internal() {}\n",
                ),
            ],
        );
        let public: Vec<&str> = publication
            .symbols
            .iter()
            .filter(|symbol| symbol.public)
            .map(|symbol| symbol.qualified_name.as_str())
            .collect();
        assert_eq!(public, ["open"]);
        assert!(
            publication
                .symbols
                .iter()
                .any(|symbol| symbol.qualified_name == "internal" && !symbol.public)
        );
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
                "rift://symbol/rust/cargo/crates.io/beacon@1.0.0/src/a.rs/alpha",
                "rift://source/cargo/crates.io/beacon@1.0.0/src/a.rs",
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
            ["rift://symbol/rust/cargo/crates.io/beacon@1.0.0/src/b.rs/beta"]
        );
        let unchanged: Vec<&str> = arrived
            .iter()
            .filter(|entry| held.contains(entry))
            .map(|(identity, _)| *identity)
            .collect();
        assert_eq!(
            unchanged,
            ["rift://symbol/rust/cargo/crates.io/beacon@1.0.0/src/a.rs/alpha"],
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

    /// The analysis hands back the parsed files beside the publication, so a consumer
    /// reads what the publication was built from rather than parsing again.
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

    #[test]
    fn test_notebook_code_cell_uses_package_language_when_cell_language_is_absent() {
        let source =
            r#"{"cells":[{"cell_type":"code","source":"def spawn(): pass\n"}],"metadata":{}}"#;
        let publication = analyzed(
            ShippedLanguage::Python,
            vec![("notebooks/guide.ipynb", source)],
        );
        let document = publication
            .documents
            .iter()
            .find(|document| document.kind == PackageDocumentKind::File)
            .expect("notebook code cell document");

        assert_eq!(document.language.name, "python");
        assert_eq!(
            document.file_content.as_deref(),
            Some("def spawn(): pass\n")
        );
    }

    #[test]
    fn malformed_notebook_omission_keeps_package_documentation_and_code() {
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![
                (
                    "README.md",
                    "# Package guide\n\nValid package documentation.\n",
                ),
                ("src/lib.rs", "/// Code documentation.\npub fn serve() {}\n"),
                ("notebooks/broken.ipynb", "{"),
            ],
        );
        let documentation = &publication.documentation;

        assert!(
            publication
                .documents
                .iter()
                .all(|document| !document.unit.0.ends_with("/notebooks/broken.ipynb"))
        );
        assert!(documentation.coverage.selected >= 3);
        assert_eq!(documentation.coverage.omitted, 1);
        assert!(documentation.sources.iter().any(|source| {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0.ends_with("/README.md")
            )
        }));
        assert!(documentation.blocks.iter().any(|block| {
            block.source.source
                == DocumentationSourceIdentity::Package {
                    unit: rift_protocol::read::SourceUnitId(
                        "rift://source/cargo/crates.io/beacon@1.0.0/README.md".to_owned(),
                    ),
                }
        }));
        assert!(
            publication
                .symbols
                .iter()
                .any(|symbol| symbol.name == "serve")
        );
        assert!(documentation.warnings.iter().any(|warning| {
            warning.stage == DocumentationStage::Source
                && warning.kind == DocumentationWarningKind::MalformedSource
                && warning.count == 1
                && matches!(
                    &warning.source.source,
                    DocumentationSourceIdentity::Package { unit }
                        if unit.0.ends_with("/notebooks/broken.ipynb")
                )
        }));
    }

    #[test]
    fn long_package_identity_omits_notebook_cell_and_keeps_readme() {
        let mut package = identity();
        package.name = "p".repeat(4_040);
        package.registry = format!("r.example/{}", "a".repeat(4_086));
        let owner = package.owner().expect("bounded canonical owner");
        let language = language(ShippedLanguage::Rust);
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency {
                package: package.clone(),
            }),
            SourceKind::Authored,
        )
        .expect("package origin");
        let readme_path = ProjectPath::new("README.md").expect("README path");
        let notebook_path = ProjectPath::new("notebooks/guide.ipynb").expect("notebook path");
        let readme = "# Package guide\n\nValid package documentation.\n";
        let notebook = r#"{"cells":[{"cell_type":"markdown","id":"guide","source":"Notebook text."}],"metadata":{}}"#;
        let files = [
            PackageSource::new(&readme_path, readme),
            PackageSource::new(&notebook_path, notebook),
        ];
        let unit = rift_core::SourceUnitId::for_package(&package, &notebook_path)
            .expect("package source unit accepts exact package name and path");
        assert_eq!(
            unit.to_string(),
            format!(
                "rift://source/cargo/r.example%2F{}/{}@1.0.0/notebooks/guide.ipynb",
                "a".repeat(4_086),
                package.name
            )
        );
        let byte_limit = u64::try_from(readme.len() + notebook.len()).expect("byte count");
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(2, byte_limit),
        )
        .expect("exact package input accepts bounded package identity");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("package analysis");
        let publication = analysis.publication();
        let notebook_source = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Package {
                unit: rift_protocol::read::SourceUnitId(unit.to_string()),
            },
            cell: Some(rift_protocol::documentation::NotebookCell {
                identity: rift_protocol::documentation::NotebookCellIdentity::Authored {
                    id: "guide".to_owned(),
                },
                kind: rift_protocol::documentation::NotebookCellKind::Markdown,
            }),
        };

        assert_eq!(publication.documentation.coverage.selected, 2);
        assert_eq!(publication.documentation.coverage.parsed, 1);
        assert_eq!(publication.documentation.coverage.omitted, 1);
        assert!(publication.documentation.blocks.iter().any(|block| {
            matches!(
                &block.source.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0.ends_with("/README.md")
            )
        }));
        assert!(
            !publication
                .documentation
                .sources
                .iter()
                .any(|source| source.identity == notebook_source)
        );
        assert!(
            publication
                .documents
                .iter()
                .all(|document| document.unit.0 != unit.to_string())
        );
        assert!(publication.documentation.warnings.iter().any(|warning| {
            warning.source == notebook_source
                && warning.stage == DocumentationStage::Source
                && warning.kind == DocumentationWarningKind::SourceUnavailable
                && warning.count == 1
        }));
    }

    #[test]
    fn oversized_selected_text_source_is_omitted_with_other_docs_retained() {
        let oversized =
            "x".repeat(rift_protocol::documentation::DOCUMENTATION_SOURCE_BYTES_MAX as usize + 1);
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![
                (
                    "README.md",
                    "# Package guide\n\nValid package documentation.\n",
                ),
                ("docs/large.txt", &oversized),
            ],
        );
        let documentation = &publication.documentation;

        assert_eq!(documentation.coverage.omitted, 1);
        assert!(documentation.sources.iter().any(|source| {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0.ends_with("/README.md")
            )
        }));
        assert!(documentation.warnings.iter().any(|warning| {
            warning.stage == DocumentationStage::Source
                && warning.kind == DocumentationWarningKind::SourceUnavailable
                && matches!(
                    &warning.source.source,
                    DocumentationSourceIdentity::Package { unit }
                        if unit.0.ends_with("/docs/large.txt")
                )
        }));
    }

    #[test]
    fn package_markdown_and_rst_near_path_bound_keep_documentation() {
        let markdown_path = format!("{}README.md", "a".repeat(982));
        let rst_path = format!("{}guide.rst", "b".repeat(982));
        let attached_path = format!("{}lib.rs", "c".repeat(982));
        ProjectPath::new(&markdown_path).expect("bounded Markdown path");
        ProjectPath::new(&rst_path).expect("bounded RST path");
        ProjectPath::new(&attached_path).expect("bounded Rust path");
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![
                (&markdown_path, "# Package guide\n\nMarkdown retained.\n"),
                (&rst_path, "Package guide\n=============\n\nRST retained.\n"),
                (
                    &attached_path,
                    "/// Attached retained.\npub fn serve() {}\n",
                ),
            ],
        );
        let documentation = &publication.documentation;
        let markdown_unit = format!("rift://source/cargo/crates.io/beacon@1.0.0/{markdown_path}");
        let rst_unit = format!("rift://source/cargo/crates.io/beacon@1.0.0/{rst_path}");
        let attached_unit = format!("rift://source/cargo/crates.io/beacon@1.0.0/{attached_path}");

        assert_eq!(documentation.coverage.selected, 3);
        assert_eq!(documentation.coverage.parsed, 3);
        assert_eq!(documentation.coverage.omitted, 0);
        assert!(documentation.sources.iter().any(|source| {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0 == markdown_unit
            )
        }));
        assert!(documentation.sources.iter().any(|source| {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0 == rst_unit
            )
        }));
        assert!(documentation.sources.iter().any(|source| {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0 == attached_unit
            )
        }));
        assert!(documentation.blocks.iter().any(|block| {
            matches!(
                &block.source.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0.ends_with("README.md")
            )
        }));
        assert!(documentation.blocks.iter().any(|block| {
            matches!(
                &block.source.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0.ends_with("guide.rst")
            )
        }));
        assert!(documentation.blocks.iter().any(|block| {
            block.symbol.is_some()
                && matches!(
                    &block.source.source,
                    DocumentationSourceIdentity::Package { unit }
                        if unit.0 == attached_unit
                )
        }));
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
