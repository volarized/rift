//! Builds one workspace documentation collection from held file bytes.

use std::collections::BTreeMap;
use std::sync::Arc;

use rift_analysis::documentation::notebook::{
    NotebookCellContent, NotebookContent, decode_notebook,
};
use rift_analysis::documentation::{
    DocumentationCollection, DocumentationDeclaration, DocumentationError, DocumentationInput,
    DocumentationSourceSet, collect_documentation_incremental, content_chunk_identity,
    content_digest,
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
    let mut omissions = Vec::new();
    let notebooks = decode_notebooks(
        text_files,
        previous.map(|(_, notebooks)| notebooks),
        &mut omissions,
    );
    let mut input_bytes = 0_u64;

    let mut inputs = Vec::new();
    for (path, file) in text_files {
        let Some(source_format) = format(path) else {
            continue;
        };
        if source_format == DocumentationSourceFormat::Notebook {
            let Some(notebook) = notebooks.get(path).and_then(HeldNotebook::content) else {
                continue;
            };
            for cell in notebook.cells() {
                let identity = project_owner(path, Some(cell.cell().clone()));
                let source = source(&identity, cell, source_format, file.content());
                if !can_count_source(&omissions, inputs.len()) {
                    break;
                }
                let source_bytes = u64::try_from(cell.text().len()).unwrap_or(u64::MAX);
                if source_bytes > u64::from(DOCUMENTATION_SOURCE_BYTES_MAX)
                    || input_bytes.saturating_add(source_bytes) > DOCUMENTATION_TOTAL_BYTES_MAX
                {
                    omissions.push((identity, DocumentationWarningKind::SourceUnavailable));
                    continue;
                }
                let chunks = content_chunks(&identity, cell.text(), chunk_bytes_max)
                    .map_err(documentation_error)?;
                match DocumentationInput::new(source, cell.text())
                    .and_then(|input| input.with_chunks(chunks))
                {
                    Ok(input) => {
                        input_bytes = input_bytes.saturating_add(source_bytes);
                        inputs.push(input);
                    }
                    Err(error) => omissions.push((identity, warning_kind(&error))),
                }
            }
            continue;
        }

        let identity = project_owner(path, None);
        let source = source_text(&identity, file.content(), source_format);
        if !can_count_source(&omissions, inputs.len()) {
            break;
        }
        let source_bytes = u64::try_from(file.content().len()).unwrap_or(u64::MAX);
        if source_bytes > u64::from(DOCUMENTATION_SOURCE_BYTES_MAX)
            || input_bytes.saturating_add(source_bytes) > DOCUMENTATION_TOTAL_BYTES_MAX
        {
            omissions.push((identity, DocumentationWarningKind::SourceUnavailable));
            continue;
        }
        let chunks = regular_chunks(path, file.content(), chunk_bytes_max);
        let mut input = match DocumentationInput::new(source, file.content())
            .and_then(|input| input.with_chunks(chunks))
        {
            Ok(input) => input,
            Err(error) => {
                omissions.push((identity, warning_kind(&error)));
                continue;
            }
        };
        if matches!(
            source_format,
            DocumentationSourceFormat::Markdown | DocumentationSourceFormat::Mdx
        ) && let Some(indexed) = files.get(path)
        {
            match input.with_syntax(indexed.syntax()) {
                Ok(validated) => input = validated,
                Err(error) => {
                    omissions.push((identity, warning_kind(&error)));
                    continue;
                }
            }
        }
        input_bytes = input_bytes.saturating_add(source_bytes);
        inputs.push(input);
    }

    append_attached_inputs(
        files,
        declarations,
        chunk_bytes_max,
        &mut input_bytes,
        &mut omissions,
        &mut inputs,
    );

    let collection = finish_collection(
        inputs,
        declarations,
        omissions,
        previous.map(|(documentation, _)| documentation),
    )?;
    Ok((collection, notebooks))
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
) -> NotebookFiles {
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
            if let NotebookOutcome::Omitted(kind) = previous.outcome.as_ref()
                && can_count_source(omissions, 1)
            {
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
                if can_count_source(omissions, 1) {
                    omissions.push((identity, kind));
                }
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
    notebooks
}

fn append_attached_inputs<'source>(
    files: &'source BTreeMap<CoreProjectPath, Arc<IndexedFile>>,
    declarations: &[DeclarationFacts],
    chunk_bytes_max: usize,
    input_bytes: &mut u64,
    omissions: &mut Vec<(DocumentationContentIdentity, DocumentationWarningKind)>,
    inputs: &mut Vec<DocumentationInput<'source>>,
) {
    for (path, file) in files {
        let identity = project_owner(path, None);
        if !declarations.iter().any(|declaration| {
            declaration.source == identity
                && file.syntax().symbols().iter().any(|symbol| {
                    if symbol.documentation_ranges.is_empty() {
                        return false;
                    }
                    let symbol = rift_core::symbol_identity(
                        &file.syntax().language().identity_segment(),
                        path.as_str(),
                        &symbol.qualified_name,
                    );
                    declaration.symbol.0 == symbol
                })
        }) {
            continue;
        }
        if !can_count_source(omissions, inputs.len()) {
            break;
        }
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
}

fn can_count_source(
    omissions: &[(DocumentationContentIdentity, DocumentationWarningKind)],
    inputs: usize,
) -> bool {
    inputs.saturating_add(omissions.len()) < DOCUMENTATION_SOURCES_MAX as usize
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
    use super::{NotebookFiles, decode_notebooks};
    use crate::workspace::TextSourceFile;
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
        let first = decode_notebooks(&files, None, &mut first_omissions);
        assert_eq!(first_omissions.len(), 1);
        let mut next_omissions = Vec::new();
        let next = decode_notebooks(&files, Some(&first), &mut next_omissions);

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
}
