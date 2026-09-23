//! Builds one workspace documentation collection from held file bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_analysis::documentation::notebook::{
    NotebookCellContent, NotebookContent, decode_notebook,
};
use rift_analysis::documentation::{
    DocumentationCollection, DocumentationDeclaration, DocumentationError, DocumentationInput,
    DocumentationSourceSet, check_documentation_source_count, collect_documentation_incremental,
    content_chunk_identity, content_digest,
};
use rift_core::ProjectPath as CoreProjectPath;
use rift_protocol::documentation::{
    DOCUMENTATION_SOURCE_BYTES_MAX, DOCUMENTATION_SOURCES_MAX, DOCUMENTATION_TOTAL_BYTES_MAX,
    DocumentationChunk, DocumentationContentIdentity, DocumentationDigest,
    DocumentationSelectionReason, DocumentationSource, DocumentationSourceFormat,
    DocumentationSourceIdentity, DocumentationWarningKind,
};
use rift_protocol::read::{
    Language, ProjectPath, SourceKind, SourceLocationKind, SymbolId, SymbolOrigin, TextRange,
};
use rift_ranking::{
    DocumentFields, DocumentKind, IDENTIFIER_TERMS_BYTES_MAX, IndexDocument,
    SearchableField as RankingField, identifier_terms,
};

use crate::chunk::text_chunks;
use crate::semantic::WorkspaceSemantics;
use crate::workspace::{IndexedFile, LeftOut, TextSourceFile, document, file_name};
use crate::{WorkspaceIndexError, WorkspaceIndexViolation};

/// Held notebook cell content, keyed by its canonical owner.
pub(crate) type NotebookFiles = BTreeMap<CoreProjectPath, HeldNotebook>;

#[derive(Clone, Debug)]
pub(crate) struct HeldNotebook {
    digest: DocumentationDigest,
    outcome: Arc<NotebookOutcome>,
}

#[derive(Clone, Debug)]
enum NotebookOutcome {
    Decoded(NotebookContent),
    Omitted(DocumentationWarningKind),
}

impl HeldNotebook {
    fn content(&self) -> Option<&NotebookContent> {
        match self.outcome.as_ref() {
            NotebookOutcome::Decoded(content) => Some(content),
            NotebookOutcome::Omitted(_) => None,
        }
    }
}

/// Collects declarations the semantic publication accepted.
pub(crate) fn declarations(
    files: &BTreeMap<CoreProjectPath, Arc<IndexedFile>>,
    semantics: &WorkspaceSemantics,
) -> Vec<DeclarationFacts> {
    let mut declarations = Vec::new();
    for file in files.values() {
        let language = file.syntax().language();
        for symbol in file.syntax().symbols() {
            let identity = rift_core::symbol_identity(
                &language.identity_segment(),
                file.path().as_str(),
                &symbol.qualified_name,
            );
            if crate::workspace::ReadableSymbol::assembled_by(semantics, &identity).is_some() {
                declarations.push(DeclarationFacts::new(file, symbol));
            }
        }
    }
    declarations
}

/// Builds a validated documentation collection and decoded notebook cells.
pub(crate) fn build(
    files: &BTreeMap<CoreProjectPath, Arc<IndexedFile>>,
    text_files: &BTreeMap<CoreProjectPath, Arc<TextSourceFile>>,
    declarations: &[DeclarationFacts],
    chunk_bytes_max: usize,
    previous: Option<(&DocumentationCollection, &NotebookFiles)>,
) -> Result<(DocumentationCollection, NotebookFiles), WorkspaceIndexError> {
    check_regular_source_count(text_files)?;
    let mut omissions = Vec::new();
    let notebooks = decode_notebooks(
        text_files,
        previous.map(|(_, notebooks)| notebooks),
        &mut omissions,
    )?;
    let mut collected = CollectedInputs {
        input_bytes: 0,
        inputs: Vec::new(),
        omissions,
    };
    for (path, file) in text_files {
        let Some(source_format) = format(path) else {
            continue;
        };
        if source_format == DocumentationSourceFormat::Notebook {
            let Some(notebook) = notebooks.get(path).and_then(HeldNotebook::content) else {
                continue;
            };
            append_notebook_inputs(
                path,
                file,
                notebook,
                source_format,
                chunk_bytes_max,
                &mut collected,
            )?;
            continue;
        }
        append_regular_input(
            path,
            file,
            files.get(path).map(Arc::as_ref),
            source_format,
            chunk_bytes_max,
            &mut collected,
        )?;
    }

    append_attached_inputs(
        files,
        declarations,
        chunk_bytes_max,
        &mut collected.input_bytes,
        &mut collected.omissions,
        &mut collected.inputs,
    )?;

    let collection = finish_collection(
        collected.inputs,
        declarations,
        collected.omissions,
        previous.map(|(documentation, _)| documentation),
    )?;
    Ok((collection, notebooks))
}

#[derive(Default)]
struct CollectedInputs<'source> {
    input_bytes: u64,
    inputs: Vec<DocumentationInput<'source>>,
    omissions: Vec<(DocumentationContentIdentity, DocumentationWarningKind)>,
}

fn append_notebook_inputs<'source>(
    path: &CoreProjectPath,
    file: &'source TextSourceFile,
    notebook: &'source NotebookContent,
    source_format: DocumentationSourceFormat,
    chunk_bytes_max: usize,
    collected: &mut CollectedInputs<'source>,
) -> Result<(), WorkspaceIndexError> {
    for cell in notebook.cells() {
        let identity = project_owner(path, Some(cell.cell().clone()));
        let source = source(&identity, cell, source_format, file.content());
        check_next_source_count(path, collected.inputs.len(), collected.omissions.len())?;
        let source_bytes = u64::try_from(cell.text().len()).unwrap_or(u64::MAX);
        if source_bytes > u64::from(DOCUMENTATION_SOURCE_BYTES_MAX)
            || collected.input_bytes.saturating_add(source_bytes) > DOCUMENTATION_TOTAL_BYTES_MAX
        {
            collected
                .omissions
                .push((identity, DocumentationWarningKind::SourceUnavailable));
            continue;
        }
        let chunks =
            content_chunks(&identity, cell.text(), chunk_bytes_max).map_err(documentation_error)?;
        match DocumentationInput::new(source, cell.text())
            .and_then(|input| input.with_chunks(chunks))
        {
            Ok(input) => {
                collected.input_bytes = collected.input_bytes.saturating_add(source_bytes);
                collected.inputs.push(input);
            }
            Err(error) => collected.omissions.push((identity, warning_kind(&error))),
        }
    }
    Ok(())
}

fn append_regular_input<'source>(
    path: &CoreProjectPath,
    file: &'source TextSourceFile,
    indexed: Option<&'source IndexedFile>,
    source_format: DocumentationSourceFormat,
    chunk_bytes_max: usize,
    collected: &mut CollectedInputs<'source>,
) -> Result<(), WorkspaceIndexError> {
    let identity = project_owner(path, None);
    let text = file.content();
    check_next_source_count(path, collected.inputs.len(), collected.omissions.len())?;
    let source_bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
    if source_bytes > u64::from(DOCUMENTATION_SOURCE_BYTES_MAX)
        || collected.input_bytes.saturating_add(source_bytes) > DOCUMENTATION_TOTAL_BYTES_MAX
    {
        collected
            .omissions
            .push((identity, DocumentationWarningKind::SourceUnavailable));
        return Ok(());
    }
    let source = source_text(&identity, text, source_format);
    let chunks = regular_chunks(path, text, chunk_bytes_max);
    let mut input =
        match DocumentationInput::new(source, text).and_then(|input| input.with_chunks(chunks)) {
            Ok(input) => input,
            Err(error) => {
                collected.omissions.push((identity, warning_kind(&error)));
                return Ok(());
            }
        };
    if matches!(
        source_format,
        DocumentationSourceFormat::Markdown | DocumentationSourceFormat::Mdx
    ) && let Some(indexed) = indexed
    {
        match input.with_syntax(indexed.syntax()) {
            Ok(validated) => input = validated,
            Err(error) => {
                collected.omissions.push((identity, warning_kind(&error)));
                return Ok(());
            }
        }
    }
    collected.input_bytes = collected.input_bytes.saturating_add(source_bytes);
    collected.inputs.push(input);
    Ok(())
}

fn finish_collection(
    inputs: Vec<DocumentationInput<'_>>,
    declarations: &[DeclarationFacts],
    omissions: Vec<(DocumentationContentIdentity, DocumentationWarningKind)>,
    previous: Option<&DocumentationCollection>,
) -> Result<DocumentationCollection, WorkspaceIndexError> {
    let sources = DocumentationSourceSet::new(inputs).map_err(documentation_error)?;
    let declarations = declarations
        .iter()
        .map(DeclarationFacts::validated)
        .collect::<Result<Vec<_>, _>>()
        .map_err(documentation_error)?;
    collect_documentation_incremental(previous, &sources, &declarations)
        .map_err(documentation_error)?
        .with_source_omissions(omissions)
        .map_err(documentation_error)
}

fn decode_notebooks(
    text_files: &BTreeMap<CoreProjectPath, Arc<TextSourceFile>>,
    previous: Option<&NotebookFiles>,
    omissions: &mut Vec<(DocumentationContentIdentity, DocumentationWarningKind)>,
) -> Result<NotebookFiles, WorkspaceIndexError> {
    let mut notebooks = NotebookFiles::new();
    for (path, file) in text_files
        .iter()
        .filter(|(path, _)| format(path) == Some(DocumentationSourceFormat::Notebook))
    {
        let identity = project_owner(path, None);
        let digest = content_digest(file.content().as_bytes());
        if let Some(previous) = previous
            .and_then(|notebooks| notebooks.get(path))
            .filter(|notebook| notebook.digest == digest)
        {
            notebooks.insert(path.clone(), previous.clone());
            if let NotebookOutcome::Omitted(kind) = previous.outcome.as_ref() {
                check_next_source_count(path, 0, omissions.len())?;
                omissions.push((identity, *kind));
            }
            continue;
        }
        match decode_notebook(file.content(), &identity) {
            Ok(notebook) => {
                notebooks.insert(
                    path.clone(),
                    HeldNotebook {
                        digest,
                        outcome: Arc::new(NotebookOutcome::Decoded(notebook)),
                    },
                );
            }
            Err(error) => {
                let kind = warning_kind(&error);
                check_next_source_count(path, 0, omissions.len())?;
                omissions.push((identity, kind));
                notebooks.insert(
                    path.clone(),
                    HeldNotebook {
                        digest,
                        outcome: Arc::new(NotebookOutcome::Omitted(kind)),
                    },
                );
            }
        }
    }
    Ok(notebooks)
}

fn append_attached_inputs<'source>(
    files: &'source BTreeMap<CoreProjectPath, Arc<IndexedFile>>,
    declarations: &[DeclarationFacts],
    chunk_bytes_max: usize,
    input_bytes: &mut u64,
    omissions: &mut Vec<(DocumentationContentIdentity, DocumentationWarningKind)>,
    inputs: &mut Vec<DocumentationInput<'source>>,
) -> Result<(), WorkspaceIndexError> {
    let attached_symbols = attached_symbols(declarations);
    for (path, file) in files {
        let identity = project_owner(path, None);
        if !has_attached_declaration(file, path, &identity, &attached_symbols) {
            continue;
        }
        check_next_source_count(path, inputs.len(), omissions.len())?;
        let text = file.source();
        let source_bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
        if source_bytes > u64::from(DOCUMENTATION_SOURCE_BYTES_MAX)
            || (*input_bytes).saturating_add(source_bytes) > DOCUMENTATION_TOTAL_BYTES_MAX
        {
            omissions.push((identity, DocumentationWarningKind::SourceUnavailable));
            continue;
        }
        let mut source = source_text(&identity, text, DocumentationSourceFormat::AttachedComment);
        source.language = Some(file.syntax().language().clone());
        let chunks = regular_chunks(path, text, chunk_bytes_max);
        let input = DocumentationInput::new(source, text)
            .and_then(|input| input.with_chunks(chunks))
            .and_then(|input| input.with_syntax(file.syntax()));
        match input {
            Ok(input) => {
                *input_bytes = (*input_bytes).saturating_add(source_bytes);
                inputs.push(input);
            }
            Err(error) => omissions.push((identity, warning_kind(&error))),
        }
    }
    Ok(())
}

type AttachedSymbols<'declarations> =
    BTreeMap<&'declarations DocumentationContentIdentity, BTreeSet<&'declarations str>>;

fn attached_symbols(declarations: &[DeclarationFacts]) -> AttachedSymbols<'_> {
    let mut by_source = BTreeMap::new();
    for declaration in declarations {
        by_source
            .entry(&declaration.source)
            .or_insert_with(BTreeSet::new)
            .insert(declaration.symbol.0.as_str());
    }
    by_source
}

fn has_attached_declaration(
    file: &IndexedFile,
    path: &CoreProjectPath,
    identity: &DocumentationContentIdentity,
    attached_symbols: &AttachedSymbols<'_>,
) -> bool {
    let Some(accepted_symbols) = attached_symbols.get(identity) else {
        return false;
    };
    file.syntax().symbols().iter().any(|symbol| {
        !symbol.documentation_ranges.is_empty()
            && accepted_symbols.contains(
                rift_core::symbol_identity(
                    &file.syntax().language().identity_segment(),
                    path.as_str(),
                    &symbol.qualified_name,
                )
                .as_str(),
            )
    })
}

fn check_regular_source_count(
    text_files: &BTreeMap<CoreProjectPath, Arc<TextSourceFile>>,
) -> Result<(), WorkspaceIndexError> {
    let mut count = 0_usize;
    for path in text_files.keys().filter(|path| {
        format(path)
            .is_some_and(|source_format| source_format != DocumentationSourceFormat::Notebook)
    }) {
        count = count.saturating_add(1);
        check_documentation_source_count(count).map_err(|_| source_limit_error(path, count))?;
    }
    Ok(())
}

fn check_next_source_count(
    path: &CoreProjectPath,
    inputs: usize,
    omissions: usize,
) -> Result<(), WorkspaceIndexError> {
    let count = inputs.saturating_add(omissions).saturating_add(1);
    check_documentation_source_count(count).map_err(|_| source_limit_error(path, count))
}

fn source_limit_error(path: &CoreProjectPath, observed: usize) -> WorkspaceIndexError {
    crate::workspace::index_error_over_limit(
        WorkspaceIndexViolation::WorkspaceTooLarge,
        std::path::Path::new(path.as_str()),
        "sources",
        observed,
        DOCUMENTATION_SOURCES_MAX as usize,
    )
}

/// Derives searchable text documents for decoded notebook cells.
pub(crate) fn cell_documents(
    notebooks: &NotebookFiles,
    chunk_bytes_max: usize,
    left_out: &mut LeftOut,
) -> Vec<IndexDocument> {
    cell_documents_where(notebooks, chunk_bytes_max, None, left_out)
}

/// Derives notebook cell documents for a bounded set of changed paths.
pub(crate) fn cell_documents_for(
    notebooks: &NotebookFiles,
    chunk_bytes_max: usize,
    paths: &std::collections::BTreeSet<CoreProjectPath>,
    left_out: &mut LeftOut,
) -> Vec<IndexDocument> {
    cell_documents_where(notebooks, chunk_bytes_max, Some(paths), left_out)
}

fn cell_documents_where(
    notebooks: &NotebookFiles,
    chunk_bytes_max: usize,
    paths: Option<&std::collections::BTreeSet<CoreProjectPath>>,
    left_out: &mut LeftOut,
) -> Vec<IndexDocument> {
    let mut documents = Vec::new();
    for (path, notebook) in notebooks {
        if paths.is_some_and(|paths| !paths.contains(path)) {
            continue;
        }
        let name = file_name(path);
        let Some(content) = notebook.content() else {
            continue;
        };
        for cell in content.cells() {
            let owner = project_owner(path, Some(cell.cell().clone()));
            let chunks = text_chunks(cell.text(), chunk_bytes_max);
            for (index, chunk) in chunks.iter().enumerate() {
                let ordinal = u32::try_from(index).unwrap_or_else(|_| {
                    unreachable!("one bounded notebook cell cannot have u32 chunks: index={index}")
                });
                let identity = content_chunk_identity(&owner, ordinal).unwrap_or_else(|error| {
                    unreachable!(
                        "validated notebook cell must have a ranking identity: error={error}"
                    )
                });
                let terms = name.as_deref().map_or_else(String::new, |name| {
                    identifier_terms([name], IDENTIFIER_TERMS_BYTES_MAX)
                });
                let fields = DocumentFields::empty()
                    .with_optional(RankingField::Name, name.as_deref())
                    .with(RankingField::IdentifierTerms, terms)
                    .with(RankingField::FileContent, chunk.content());
                if let Some(document) =
                    document(identity, path, DocumentKind::TextFile, fields, left_out)
                {
                    documents.push(document);
                }
            }
        }
    }
    documents
}

/// Returns cell text by content owner, without returning parent notebook JSON.
pub(crate) fn content<'a>(
    notebooks: &'a NotebookFiles,
    identity: &DocumentationContentIdentity,
) -> Option<&'a str> {
    let DocumentationSourceIdentity::Project { path } = &identity.source else {
        return None;
    };
    let cell_identity = identity.cell.as_ref()?;
    let path = CoreProjectPath::new(path.0.as_str()).ok()?;
    notebooks
        .get(&path)?
        .content()?
        .cells()
        .iter()
        .find(|cell| cell.cell() == cell_identity)
        .map(NotebookCellContent::text)
}

/// Exact syntax declarations accepted by workspace semantic publication.
pub(crate) struct DeclarationFacts {
    symbol: SymbolId,
    language: Language,
    name: String,
    qualified_name: String,
    source: DocumentationContentIdentity,
    range: TextRange,
}

impl DeclarationFacts {
    pub(crate) fn new(file: &IndexedFile, symbol: &rift_syntax::SyntaxSymbol) -> Self {
        let language = file.syntax().language().clone();
        let qualified_name = symbol.qualified_name.clone();
        let identity = rift_core::symbol_identity(
            &language.identity_segment(),
            file.path().as_str(),
            &qualified_name,
        );
        Self {
            symbol: SymbolId(identity),
            language,
            name: symbol.name.clone(),
            qualified_name,
            source: project_owner(file.path(), None),
            range: TextRange {
                start: symbol.range.start,
                end: symbol.range.end,
            },
        }
    }

    fn validated(&self) -> Result<DocumentationDeclaration<'_>, DocumentationError> {
        DocumentationDeclaration::new(
            &self.symbol,
            &self.language,
            &self.name,
            &self.qualified_name,
            &self.source,
            self.range.clone(),
        )
    }
}

fn source_text(
    identity: &DocumentationContentIdentity,
    text: &str,
    format: DocumentationSourceFormat,
) -> DocumentationSource {
    let digest = content_digest(text.as_bytes());
    DocumentationSource {
        identity: identity.clone(),
        revision: digest.clone(),
        content_digest: digest,
        origin: project_origin(),
        format,
        media_type: media_type(format).to_owned(),
        selection: if format == DocumentationSourceFormat::AttachedComment {
            DocumentationSelectionReason::AttachedComment
        } else {
            DocumentationSelectionReason::Workspace
        },
        byte_length: u64::try_from(text.len()).unwrap_or(u64::MAX),
        language: None,
        physical_ranges: Vec::new(),
        license: None,
    }
}

fn source(
    identity: &DocumentationContentIdentity,
    cell: &NotebookCellContent,
    format: DocumentationSourceFormat,
    notebook_text: &str,
) -> DocumentationSource {
    let digest = content_digest(cell.text().as_bytes());
    DocumentationSource {
        identity: identity.clone(),
        revision: content_digest(notebook_text.as_bytes()),
        content_digest: digest,
        origin: project_origin(),
        format,
        media_type: media_type(format).to_owned(),
        selection: DocumentationSelectionReason::Workspace,
        byte_length: u64::try_from(cell.text().len()).unwrap_or(u64::MAX),
        language: cell.declared_language().cloned(),
        physical_ranges: cell.physical_ranges().to_vec(),
        license: None,
    }
}

fn project_owner(
    path: &CoreProjectPath,
    cell: Option<rift_protocol::documentation::NotebookCell>,
) -> DocumentationContentIdentity {
    DocumentationContentIdentity {
        source: DocumentationSourceIdentity::Project {
            path: ProjectPath(path.to_string()),
        },
        cell,
    }
}

fn project_origin() -> SymbolOrigin {
    SymbolOrigin {
        location: Some(SourceLocationKind::Project),
        package: None,
        source_kind: SourceKind::Authored,
    }
}

fn format(path: &CoreProjectPath) -> Option<DocumentationSourceFormat> {
    let extension = std::path::Path::new(path.as_str()).extension()?.to_str()?;
    match extension {
        "md" | "markdown" => Some(DocumentationSourceFormat::Markdown),
        "mdx" => Some(DocumentationSourceFormat::Mdx),
        "rst" => Some(DocumentationSourceFormat::RestructuredText),
        "txt" => Some(DocumentationSourceFormat::Text),
        "ipynb" => Some(DocumentationSourceFormat::Notebook),
        _ => None,
    }
}

fn media_type(format: DocumentationSourceFormat) -> &'static str {
    match format {
        DocumentationSourceFormat::Markdown | DocumentationSourceFormat::AttachedComment => {
            "text/markdown"
        }
        DocumentationSourceFormat::Mdx => "text/mdx",
        DocumentationSourceFormat::RestructuredText => "text/x-rst",
        DocumentationSourceFormat::Text => "text/plain",
        DocumentationSourceFormat::Notebook => "application/x-ipynb+json",
    }
}

fn regular_chunks(
    path: &CoreProjectPath,
    text: &str,
    chunk_bytes_max: usize,
) -> Vec<DocumentationChunk> {
    text_chunks(text, chunk_bytes_max)
        .iter()
        .enumerate()
        .map(|(index, chunk)| DocumentationChunk {
            identity: if text.len() <= chunk_bytes_max {
                path.as_str().to_owned()
            } else {
                format!("{}#{index}", path.as_str())
            },
            range: chunk_range(chunk.byte_offset(), chunk.content()),
        })
        .collect()
}

fn content_chunks(
    owner: &DocumentationContentIdentity,
    text: &str,
    chunk_bytes_max: usize,
) -> Result<Vec<DocumentationChunk>, DocumentationError> {
    text_chunks(text, chunk_bytes_max)
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            let ordinal = u32::try_from(index).unwrap_or_else(|_| {
                unreachable!(
                    "one bounded documentation source cannot have u32 chunks: index={index}"
                )
            });
            let identity = content_chunk_identity(owner, ordinal)?;
            Ok(DocumentationChunk {
                identity,
                range: chunk_range(chunk.byte_offset(), chunk.content()),
            })
        })
        .collect()
}

fn chunk_range(start: u64, content: &str) -> TextRange {
    TextRange {
        start,
        end: start.saturating_add(u64::try_from(content.len()).unwrap_or(u64::MAX)),
    }
}

fn documentation_error(error: DocumentationError) -> WorkspaceIndexError {
    crate::workspace::index_error_caused_by(WorkspaceIndexViolation::Syntax, None, error)
}

fn warning_kind(error: &DocumentationError) -> DocumentationWarningKind {
    match error.fault().violation() {
        rift_analysis::documentation::DocumentationViolation::LimitExceeded => {
            DocumentationWarningKind::SourceUnavailable
        }
        _ => DocumentationWarningKind::MalformedSource,
    }
}

#[cfg(test)]
mod tests {
    use super::{NotebookFiles, cell_documents, cell_documents_for, decode_notebooks};
    use crate::workspace::TextSourceFile;
    use crate::{WorkspaceIndex, WorkspaceIndexLimits};
    use rift_core::{SourceVisibility, TextFileInclusion};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[test]
    fn notebook_decode_reuses_success_and_failure_for_unchanged_bytes() {
        let mut files = BTreeMap::new();
        files.insert(
            rift_core::ProjectPath::new("valid.ipynb").expect("valid path"),
            Arc::new(TextSourceFile::from_content(
                rift_core::ProjectPath::new("valid.ipynb").expect("valid path"),
                r#"{"cells":[],"metadata":{},"nbformat":4,"nbformat_minor":5}"#.into(),
            )),
        );
        files.insert(
            rift_core::ProjectPath::new("invalid.ipynb").expect("valid path"),
            Arc::new(TextSourceFile::from_content(
                rift_core::ProjectPath::new("invalid.ipynb").expect("valid path"),
                "{".into(),
            )),
        );

        let mut first_omissions = Vec::new();
        let first = decode_notebooks(&files, None, &mut first_omissions).expect("first decode");
        assert_eq!(first_omissions.len(), 1);
        let mut next_omissions = Vec::new();
        let next =
            decode_notebooks(&files, Some(&first), &mut next_omissions).expect("cached decode");

        assert_eq!(next_omissions, first_omissions);
        for path in files.keys() {
            assert!(Arc::ptr_eq(&first[path].outcome, &next[path].outcome,));
        }
        assert!(matches!(
            next[&rift_core::ProjectPath::new("invalid.ipynb").expect("valid path")]
                .outcome
                .as_ref(),
            super::NotebookOutcome::Omitted(_)
        ));
        assert!(matches!(
            next[&rift_core::ProjectPath::new("valid.ipynb").expect("valid path")]
                .outcome
                .as_ref(),
            super::NotebookOutcome::Decoded(_)
        ));
        let _: NotebookFiles = next;
    }

    #[test]
    fn documentation_source_count_helper_accepts_exact_bound_and_refuses_one_over() {
        let maximum = rift_protocol::documentation::DOCUMENTATION_SOURCES_MAX as usize;

        assert!(rift_analysis::documentation::check_documentation_source_count(maximum).is_ok());
        let error = rift_analysis::documentation::check_documentation_source_count(maximum + 1)
            .expect_err("one source past the bound refuses");
        assert_eq!(
            error.fault().violation(),
            rift_analysis::documentation::DocumentationViolation::LimitExceeded
        );
        assert_eq!(error.fault().field(), "sources");
    }

    #[test]
    fn malformed_notebook_build_keeps_omission_and_emits_no_cell_documents() {
        let path = rift_core::ProjectPath::new("broken.ipynb").expect("valid path");
        let text_file = Arc::new(TextSourceFile::from_content(path.clone(), "{".into()));
        let text_files = BTreeMap::from([(path.clone(), text_file)]);
        let (collection, notebooks) = super::build(&BTreeMap::new(), &text_files, &[], 1_024, None)
            .expect("malformed notebook remains a bounded omission");

        assert_eq!(collection.index().coverage.omitted, 1);
        assert_eq!(
            warning_kind(collection.index(), "broken.ipynb"),
            Some((
                rift_protocol::documentation::DocumentationStage::Source,
                rift_protocol::documentation::DocumentationWarningKind::MalformedSource,
            ))
        );
        assert!(
            cell_documents(&notebooks, 1_024, &mut crate::workspace::LeftOut::default()).is_empty()
        );
        assert!(
            cell_documents_for(
                &notebooks,
                1_024,
                &std::collections::BTreeSet::new(),
                &mut crate::workspace::LeftOut::default(),
            )
            .is_empty()
        );
    }

    #[test]
    fn workspace_documentation_build_refuses_past_source_count() {
        use crate::WorkspaceIndexViolation;
        use rift_core::Fault as _;

        let mut text_files = BTreeMap::new();
        for index in 0..=rift_protocol::documentation::DOCUMENTATION_SOURCES_MAX {
            let path =
                rift_core::ProjectPath::new(format!("docs/{index:06}.txt")).expect("valid path");
            text_files.insert(
                path.clone(),
                Arc::new(TextSourceFile::from_content(path, String::new())),
            );
        }

        let error = super::build(&BTreeMap::new(), &text_files, &[], 1_024, None)
            .expect_err("source count over bound refuses collection");

        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::WorkspaceTooLarge
        );
        let evidence = error
            .fault()
            .limit_evidence()
            .expect("source limit evidence");
        assert_eq!(
            (evidence.field.as_str(), evidence.limit, evidence.required),
            (
                "sources",
                u64::from(rift_protocol::documentation::DOCUMENTATION_SOURCES_MAX),
                u64::from(rift_protocol::documentation::DOCUMENTATION_SOURCES_MAX) + 1,
            )
        );
    }

    #[test]
    fn attached_documentation_matches_accepted_source_and_symbol() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        std::fs::create_dir_all(directory.path().join("src")).expect("source directory");
        std::fs::write(
            directory.path().join("src/lib.rs"),
            "/// Accepted comment.\npub fn accepted() {}\nfn plain() {}\n",
        )
        .expect("source file");
        let workspace = WorkspaceIndex::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("workspace index");
        let path = rift_core::ProjectPath::new("src/lib.rs").expect("valid source path");
        let file = workspace.file(&path).expect("indexed source");
        let accepted = file
            .syntax()
            .symbols()
            .iter()
            .find(|symbol| symbol.qualified_name == "accepted")
            .expect("accepted declaration");
        let plain = file
            .syntax()
            .symbols()
            .iter()
            .find(|symbol| symbol.qualified_name == "plain")
            .expect("plain declaration");
        let accepted_facts = super::DeclarationFacts::new(file, accepted);
        let attached = super::attached_symbols(std::slice::from_ref(&accepted_facts));
        let owner = super::project_owner(&path, None);

        assert!(super::has_attached_declaration(
            file, &path, &owner, &attached
        ));

        let other_owner = super::project_owner(
            &rift_core::ProjectPath::new("src/other.rs").expect("valid source path"),
            None,
        );
        assert!(!super::has_attached_declaration(
            file,
            &path,
            &other_owner,
            &attached,
        ));

        let mut other_symbol_facts = super::DeclarationFacts::new(file, accepted);
        other_symbol_facts.symbol = rift_protocol::read::SymbolId("other-symbol".into());
        let other_symbol = super::attached_symbols(std::slice::from_ref(&other_symbol_facts));
        assert!(!super::has_attached_declaration(
            file,
            &path,
            &owner,
            &other_symbol
        ));

        let plain_facts = super::DeclarationFacts::new(file, plain);
        let no_comment = super::attached_symbols(std::slice::from_ref(&plain_facts));
        assert!(!super::has_attached_declaration(
            file,
            &path,
            &owner,
            &no_comment
        ));
    }

    #[test]
    fn workspace_build_reports_malformed_and_oversized_documentation_sources() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let source_max = rift_protocol::documentation::DOCUMENTATION_SOURCE_BYTES_MAX as usize;
        let oversized = "x".repeat(source_max + 1);
        std::fs::write(directory.path().join("broken.ipynb"), "{").expect("malformed notebook");
        std::fs::write(directory.path().join("oversized.rst"), &oversized)
            .expect("oversized reStructuredText source");
        std::fs::write(directory.path().join("oversized.ipynb"), &oversized)
            .expect("oversized notebook source");

        let capture_file_max = source_max + 2;
        let limits = WorkspaceIndexLimits::new(8, capture_file_max, capture_file_max * 3, 8, 32)
            .expect("capture limits above documentation source bound");
        let inclusion = TextFileInclusion::new(vec!["**".to_owned()], 1_024);
        let workspace = WorkspaceIndex::build(
            directory.path(),
            limits,
            &SourceVisibility::default(),
            &inclusion,
        )
        .expect("workspace capture accepts files above documentation bound");

        assert_eq!(workspace.text_file_count(), 3);
        let index = workspace.documentation().index();
        assert_eq!(index.coverage.selected, 3);
        assert_eq!(index.coverage.parsed, 0);
        assert_eq!(index.coverage.omitted, 3);
        assert_eq!(
            warning_kind(index, "broken.ipynb"),
            Some((
                rift_protocol::documentation::DocumentationStage::Source,
                rift_protocol::documentation::DocumentationWarningKind::MalformedSource,
            ))
        );
        assert_eq!(
            warning_kind(index, "oversized.rst"),
            Some((
                rift_protocol::documentation::DocumentationStage::Source,
                rift_protocol::documentation::DocumentationWarningKind::SourceUnavailable,
            ))
        );
        assert_eq!(
            warning_kind(index, "oversized.ipynb"),
            Some((
                rift_protocol::documentation::DocumentationStage::Source,
                rift_protocol::documentation::DocumentationWarningKind::SourceUnavailable,
            ))
        );
    }

    fn warning_kind(
        index: &rift_protocol::documentation::DocumentationIndex,
        path: &str,
    ) -> Option<(
        rift_protocol::documentation::DocumentationStage,
        rift_protocol::documentation::DocumentationWarningKind,
    )> {
        let identity = rift_protocol::documentation::DocumentationContentIdentity {
            source: rift_protocol::documentation::DocumentationSourceIdentity::Project {
                path: rift_protocol::read::ProjectPath(path.to_owned()),
            },
            cell: None,
        };
        index
            .warnings
            .iter()
            .find(|warning| warning.source == identity)
            .map(|warning| (warning.stage, warning.kind))
    }
}
