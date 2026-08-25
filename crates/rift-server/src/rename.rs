//! Semantic rename through a configured language engine.
//!
//! The engine proposes a `WorkspaceEdit`; the server compiles the proposal
//! into whole-file rewrites, verifies each one against the tree, and writes
//! them through the same atomic path every change tool uses. Engines never
//! write. After an applied rename the changed tree is swept for surviving
//! word-boundary occurrences of the old name, and each survivor rides the
//! summary as a warning finding.
//!
//! The proposal compile kernel - URI conversion, version and bound checks,
//! bottom-up edit application - is shared: [`crate::move_file`] compiles
//! its will-rename proposals through the same functions under its own
//! [`ProposalContext`].

use std::collections::BTreeMap;
use std::path::Path;

use lsp_types::{
    AnnotatedTextEdit, DocumentChangeOperation, DocumentChanges, OneOf, Position, TextDocumentEdit,
    TextEdit, Uri, WorkspaceEdit,
};
use rift_core::ProjectPath as CoreProjectPath;
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::position::LineIndex;
use rift_lsp::session::{EngineError, EngineFault, EngineSession};
use rift_lsp::uri::TreeRoot;
use rift_protocol::change::{
    ChangeResult, OperationPrecondition, OperationPreconditionKind, OperationPreconditionStatus,
    PreconditionAddress, PreconditionValue, RefusalReason, RenameSymbolParams,
};
use rift_protocol::read::{
    Diagnostic, DiagnosticCode, DiagnosticContinuation, DiagnosticReliability, Extensions,
    Language, Severity, SourceSpan, SymbolId, TextRange,
};
use rift_syntax::SyntaxSymbol;

use crate::change::{SymbolAddress, SymbolResolution, parse_symbol_address, resolve_symbol};
use crate::engine::{EnginePool, EngineSlot};
use crate::read::{ReadError, ReadFault, ReadService, digest_hex8, file_id};

/// Most files one engine rename proposal may rewrite.
pub const RENAME_FILES_MAX: usize = 64;

/// Most text edits one rename proposal may carry for one file.
pub const RENAME_FILE_EDITS_MAX: usize = 4_096;

/// Largest source, in bytes, a rename may read or produce for one file:
/// the bound the wire `Edit` text advertises.
pub const RENAME_FILE_BYTES_MAX: usize = 1_048_576;

/// Most files the post-apply sweep scans.
pub const RENAME_SWEEP_FILES_MAX: usize = 2_048;

/// Most source bytes the post-apply sweep scans.
pub const RENAME_SWEEP_BYTES_MAX: usize = 16_777_216;

/// Most surviving-occurrence findings one summary carries.
pub const RENAME_SWEEP_FINDINGS_MAX: usize = 16;

/// One rename, proposed by the engine and compiled into whole-file
/// rewrites, not yet written. The change lane re-proves every base against
/// the disk before writing.
#[derive(Debug)]
pub struct RenamePlan {
    pub(crate) symbol: SymbolId,
    pub(crate) old_name: String,
    pub(crate) rewrites: Vec<PlannedRewrite>,
}

impl RenamePlan {
    /// The renamed declaration as a failed condition's address entry.
    pub(crate) fn symbol_addresses(&self) -> Vec<PreconditionAddress> {
        vec![PreconditionAddress::Symbol {
            symbol: self.symbol.clone(),
        }]
    }
}

/// One file's rewrite: the bytes the plan was compiled against, and the
/// bytes that replace them.
#[derive(Debug)]
pub(crate) struct PlannedRewrite {
    pub(crate) path: CoreProjectPath,
    pub(crate) base_source: String,
    pub(crate) next_source: String,
}

/// What planning decided: a plan ready for the change lane, or the refusal
/// that ends the request with the targeted tree untouched.
#[derive(Debug)]
pub enum RenameResolution {
    /// The proposal compiled; the change lane verifies and writes it.
    Planned(RenamePlan),
    /// Planning produced no edits; the targeted tree is untouched.
    Refused(ChangeResult),
}

/// Why one planning stage ended the request early.
#[derive(Debug)]
pub(crate) enum PlanEnd {
    /// A typed refusal; the targeted tree is untouched.
    Refused(ChangeResult),
    /// An operating failure.
    Failed(ReadError),
}

/// The operation prose opening every rename refusal detail.
pub(crate) const RENAME_OPERATION: &str = "semantic rename";

/// What the compile kernel needs to judge one engine proposal, whichever
/// operation asked for it.
#[derive(Debug)]
pub(crate) struct ProposalContext<'plan> {
    /// Operation prose opening every refusal detail, such as
    /// [`RENAME_OPERATION`].
    pub(crate) operation: &'static str,
    /// Addressed subjects a failed condition names; empty for a file move,
    /// which addresses paths alone.
    pub(crate) addresses: Vec<PreconditionAddress>,
    /// The document opened for the engine and the version its `didOpen`
    /// carried. A proposal for an operation that opened nothing accepts
    /// only version-free documents.
    pub(crate) opened: Option<(&'plan CoreProjectPath, i32)>,
    /// Paths whose compile base is served from held bytes instead of disk.
    pub(crate) bases: BTreeMap<&'plan CoreProjectPath, &'plan str>,
}

impl From<ReadError> for PlanEnd {
    fn from(error: ReadError) -> Self {
        Self::Failed(error)
    }
}

/// Plans one rename: resolves the address, asks the configured engine for
/// its proposal, and compiles the proposal into whole-file rewrites for
/// the change lane.
///
/// # Errors
///
/// Returns [`ReadError`] for an invalid request, a filesystem failure, or
/// an engine that failed to serve - a spawn, timeout, or protocol fault.
/// A request the server or the engine declines returns a refused
/// [`ChangeResult`] inside [`RenameResolution`] instead.
///
/// # Cancel safety
///
/// Dropping the future writes nothing; an engine request in flight is
/// discarded by the session, and the session stays in its slot.
pub async fn plan_rename(
    reads: &ReadService,
    engines: &EnginePool,
    workspace_root: &Path,
    params: &RenameSymbolParams,
) -> Result<RenameResolution, ReadError> {
    match planned_rename(reads, engines, workspace_root, params).await {
        Ok(plan) => Ok(RenameResolution::Planned(plan)),
        Err(PlanEnd::Refused(refusal)) => Ok(RenameResolution::Refused(refusal)),
        Err(PlanEnd::Failed(error)) => Err(error),
    }
}

/// The planning pipeline behind [`plan_rename`], with every early end as
/// one typed [`PlanEnd`].
async fn planned_rename(
    reads: &ReadService,
    engines: &EnginePool,
    workspace_root: &Path,
    params: &RenameSymbolParams,
) -> Result<RenamePlan, PlanEnd> {
    if let Some(violation) = params.new_name_violation() {
        return Err(ReadFault::invalid("new_name", violation.as_str()).into());
    }
    let address = parse_symbol_address(&params.symbol.0)?;
    let slot = claimed_engine(engines, &address)?;
    let target = resolved_target(reads, &address)?;
    verified_disk_target(workspace_root, &address, &target).await?;
    let positions = name_positions(&target.indexed_source, target.name_offset)?;
    let answer = engine_exchange(slot, &target, &positions, &params.new_name).await?;
    let (edit, encoding, version) = proposed_edit(answer, &address)?;
    compiled_plan(workspace_root, &edit, encoding, version, &address, &target).await
}

/// The resolved declaration a rename targets.
#[derive(Clone, Debug)]
struct RenameTarget {
    path: CoreProjectPath,
    language: Language,
    old_name: String,
    /// Byte offset of the declaration's name in the indexed source.
    name_offset: usize,
    /// The indexed source of the declaration's file: the exact bytes the
    /// engine is handed at `didOpen`.
    indexed_source: String,
}

/// The engine slot claiming the address's language segment, or the
/// capability refusal naming the unserved language.
fn claimed_engine<'pool>(
    engines: &'pool EnginePool,
    address: &SymbolAddress,
) -> Result<&'pool EngineSlot, PlanEnd> {
    let language = segment_language(&address.language_segment);
    engines.engine_for(&language).ok_or_else(|| {
        PlanEnd::Refused(unsupported_refusal(format!(
            "semantic rename (no engine configured for language {})",
            address.language_segment
        )))
    })
}

/// The `Language` one identity segment spells: `name` or `name:dialect`.
pub(crate) fn segment_language(segment: &str) -> Language {
    match segment.split_once(':') {
        Some((name, dialect)) => Language {
            name: name.to_owned(),
            dialect: Some(dialect.to_owned()),
        },
        None => Language {
            name: segment.to_owned(),
            dialect: None,
        },
    }
}

/// Resolves the address to its single declaration, keeping the shared
/// refusal shapes for a missing or ambiguous target.
fn resolved_target(reads: &ReadService, address: &SymbolAddress) -> Result<RenameTarget, PlanEnd> {
    match resolve_symbol(reads, address)? {
        SymbolResolution::Refused {
            reason,
            preconditions,
        } => Err(PlanEnd::Refused(ChangeResult::refused(
            reason,
            preconditions,
        ))),
        SymbolResolution::Declared { file, symbol } => Ok(RenameTarget {
            path: address.path.clone(),
            language: file.syntax().language().clone(),
            old_name: symbol.name.clone(),
            name_offset: declaration_name_offset(file.source(), symbol),
            indexed_source: file.source().to_owned(),
        }),
    }
}

/// Byte offset of the declaration's own name: the first word-boundary
/// occurrence of the short name inside the item's bytes, or the item start
/// when the provider's name is not spelled there - the engine's own
/// verdict then decides whether the position renames.
fn declaration_name_offset(source: &str, symbol: &SyntaxSymbol) -> usize {
    let start = bounded_offset(symbol.item_range.start, source.len());
    let end = bounded_offset(symbol.item_range.end, source.len());
    let item = source.get(start..end).unwrap_or_default();
    word_boundary_occurrences(item, &symbol.name, 1)
        .first()
        .map_or(start, |offset| start + offset)
}

/// Converts one wire byte offset into an in-memory index, clamped to the
/// source length.
fn bounded_offset(value: u64, source_len: usize) -> usize {
    usize::try_from(value).unwrap_or(source_len).min(source_len)
}

/// Proves the target's disk bytes still match the resolved snapshot: the
/// engine must see exactly the bytes resolution ran over.
async fn verified_disk_target(
    workspace_root: &Path,
    address: &SymbolAddress,
    target: &RenameTarget,
) -> Result<(), PlanEnd> {
    let absolute = workspace_root.join(target.path.as_str());
    let disk = tokio::fs::read_to_string(&absolute)
        .await
        .map_err(|error| PlanEnd::from(ReadFault::storage(target.path.as_str(), "read", &error)))?;
    if disk == target.indexed_source {
        return Ok(());
    }
    Err(PlanEnd::Refused(ChangeResult::refused(
        RefusalReason::UnmetPrecondition,
        vec![failed_precondition(
            OperationPreconditionKind::SourceUnchanged,
            &[PreconditionAddress::Symbol {
                symbol: address.wire_symbol(),
            }],
            &target.path,
            PreconditionValue::Text {
                value: digest_hex8(&target.indexed_source),
            },
            PreconditionValue::Text {
                value: digest_hex8(&disk),
            },
        )],
    )))
}

/// The declaration name's position in both offered encodings, decided by
/// the negotiated encoding once the engine has spawned.
#[derive(Clone, Copy, Debug)]
struct NamePositions {
    utf8: Position,
    utf16: Position,
}

impl NamePositions {
    /// The position in the encoding one session negotiated.
    fn negotiated(self, encoding: PositionEncoding) -> Position {
        match encoding {
            PositionEncoding::Utf8 => self.utf8,
            PositionEncoding::Utf16 => self.utf16,
        }
    }
}

/// Converts the name offset into both offered encodings. The offset came
/// from the same source bytes, so a conversion failure is an internal
/// fault, never a caller mistake.
fn name_positions(source: &str, name_offset: usize) -> Result<NamePositions, PlanEnd> {
    let index = LineIndex::new(source);
    let converted = |encoding| {
        index.position(encoding, name_offset).map_err(|error| {
            PlanEnd::Failed(ReadFault::task(
                "rename position conversion",
                error.to_string(),
            ))
        })
    };
    Ok(NamePositions {
        utf8: converted(PositionEncoding::Utf8)?,
        utf16: converted(PositionEncoding::Utf16)?,
    })
}

/// What the engine answered for one rename request.
#[derive(Debug)]
enum EngineAnswer {
    /// A proposal to compile, with the negotiated encoding and the version
    /// the opened document carried.
    Proposed {
        edit: WorkspaceEdit,
        encoding: PositionEncoding,
        version: i32,
    },
    /// The engine declined the rename; the detail keeps its own words.
    Declined { detail: String },
}

/// Runs the rename conversation on the claimed engine's session.
///
/// An engine without the rename capability answers the capability refusal;
/// every other engine failure - spawn, timeout, protocol - stays a typed
/// operating error.
async fn engine_exchange(
    slot: &EngineSlot,
    target: &RenameTarget,
    positions: &NamePositions,
    new_name: &str,
) -> Result<EngineAnswer, PlanEnd> {
    // The boxed future may only borrow the session, so each attempt gets
    // its own owned copy of the request data.
    let request_target = target.clone();
    let request_positions = *positions;
    let request_name = new_name.to_owned();
    let exchanged = slot
        .request(move |session: &mut EngineSession| {
            let target = request_target.clone();
            let name = request_name.clone();
            Box::pin(async move {
                exchange_on_session(session, &target, &request_positions, &name).await
            })
        })
        .await;
    match exchanged {
        Ok(answer) => Ok(answer),
        Err(error) => {
            if matches!(error.fault(), EngineFault::CapabilityAbsent { .. }) {
                return Err(PlanEnd::Refused(unsupported_refusal(format!(
                    "semantic rename (engine {} does not advertise textDocument/rename)",
                    slot.name()
                ))));
            }
            Err(PlanEnd::Failed(ReadFault::engine(error)))
        }
    }
}

/// One open-prepare-rename-close conversation on a running session.
///
/// An engine that answers the prepare or the rename with its verdict on
/// the request declines it and stays serving; the document is closed
/// before the verdict returns. Every other fault propagates and skips the
/// close: a refusal the engine invites again goes back to the slot, which
/// runs this conversation from its `didOpen` again, and a fault that ended
/// the session would refuse the close anyway.
async fn exchange_on_session(
    session: &mut EngineSession,
    target: &RenameTarget,
    positions: &NamePositions,
    new_name: &str,
) -> Result<EngineAnswer, EngineError> {
    let encoding = session.capabilities().position_encoding;
    let position = positions.negotiated(encoding);
    session
        .open(
            &target.path,
            &target.language.name,
            target.indexed_source.clone(),
        )
        .await?;
    let version = session.document_version();
    if let Some(declined) = prepare_declined(session, target, position).await? {
        session.close(&target.path).await?;
        return Ok(declined);
    }
    match session.rename(&target.path, position, new_name).await {
        Ok(edit) => {
            session.close(&target.path).await?;
            Ok(EngineAnswer::Proposed {
                edit,
                encoding,
                version,
            })
        }
        Err(error) => match declined_by(&error) {
            Some(declined) => {
                session.close(&target.path).await?;
                Ok(declined)
            }
            None => Err(error),
        },
    }
}

/// The engine's verdict on the request, as the decline to report.
///
/// Only a refusal the engine will not answer differently is a verdict.
/// A refusal it invites again is transient, so it goes back to the slot,
/// which sends the whole conversation again under the engine's
/// `[engines.<name>.retry]` table; so does every fault that is not a
/// refusal at all.
fn declined_by(error: &EngineError) -> Option<EngineAnswer> {
    let fault = error.fault();
    match fault {
        EngineFault::Refused { message, .. } if !fault.is_retryable_refusal() => {
            Some(EngineAnswer::Declined {
                detail: format!("the engine declined the rename: {message}"),
            })
        }
        _ => None,
    }
}

/// The engine's prepare verdict: a decline to report, or nothing when the
/// rename may proceed. An engine that never advertised prepared renames
/// proceeds straight to the rename request.
async fn prepare_declined(
    session: &mut EngineSession,
    target: &RenameTarget,
    position: Position,
) -> Result<Option<EngineAnswer>, EngineError> {
    if !session.capabilities().prepare_rename {
        return Ok(None);
    }
    match session.prepare_rename(&target.path, position).await {
        Ok(Some(_)) => Ok(None),
        Ok(None) => Ok(Some(EngineAnswer::Declined {
            detail: "the engine serves no rename at this declaration".to_owned(),
        })),
        Err(error) => match declined_by(&error) {
            Some(declined) => Ok(Some(declined)),
            None => Err(error),
        },
    }
}

/// Distills the engine's answer into the proposal to compile, or the
/// refusal a declined rename returns.
fn proposed_edit(
    answer: EngineAnswer,
    address: &SymbolAddress,
) -> Result<(WorkspaceEdit, PositionEncoding, i32), PlanEnd> {
    match answer {
        EngineAnswer::Proposed {
            edit,
            encoding,
            version,
        } => Ok((edit, encoding, version)),
        EngineAnswer::Declined { detail } => {
            Err(PlanEnd::Refused(declined_refusal(address, detail)))
        }
    }
}

/// Compiles one engine proposal into whole-file rewrites.
///
/// The opened target compiles against the exact bytes the engine saw;
/// every other file compiles against its current disk bytes. The change
/// lane re-proves each base against the disk before writing, so a file
/// that moves between compile and apply refuses instead of drifting.
async fn compiled_plan(
    workspace_root: &Path,
    edit: &WorkspaceEdit,
    encoding: PositionEncoding,
    version: i32,
    address: &SymbolAddress,
    target: &RenameTarget,
) -> Result<RenamePlan, PlanEnd> {
    let tree_root = workspace_tree_root(workspace_root)?;
    let context = ProposalContext {
        operation: RENAME_OPERATION,
        addresses: vec![PreconditionAddress::Symbol {
            symbol: address.wire_symbol(),
        }],
        opened: Some((&target.path, version)),
        bases: BTreeMap::from([(&target.path, target.indexed_source.as_str())]),
    };
    let documents = proposal_documents(edit, &tree_root, &context)?;
    let rewrites = compiled_rewrites(workspace_root, documents, encoding, &context).await?;
    if rewrites.is_empty() {
        return Err(PlanEnd::Refused(declined_refusal(
            address,
            "the engine proposed no edits".to_owned(),
        )));
    }
    Ok(RenamePlan {
        symbol: address.wire_symbol(),
        old_name: target.old_name.clone(),
        rewrites,
    })
}

/// The workspace root as a tree root for proposal URI conversion.
pub(crate) fn workspace_tree_root(workspace_root: &Path) -> Result<TreeRoot, PlanEnd> {
    TreeRoot::new(workspace_root).map_err(|error| {
        PlanEnd::Failed(ReadFault::task(
            "proposal root conversion",
            error.to_string(),
        ))
    })
}

/// Compiles per-file text edits into whole-file rewrites, dropping a file
/// whose edits change nothing.
pub(crate) async fn compiled_rewrites(
    workspace_root: &Path,
    documents: Vec<(CoreProjectPath, Vec<TextEdit>)>,
    encoding: PositionEncoding,
    context: &ProposalContext<'_>,
) -> Result<Vec<PlannedRewrite>, PlanEnd> {
    let mut rewrites = Vec::with_capacity(documents.len());
    for (path, edits) in documents {
        let base = document_base(workspace_root, context, &path).await?;
        refused_oversized(&path, base.len(), context.operation)?;
        let next = rewritten_source(context, &path, &base, &edits, encoding)?;
        refused_oversized(&path, next.len(), context.operation)?;
        if next != base {
            rewrites.push(PlannedRewrite {
                path,
                base_source: base,
                next_source: next,
            });
        }
    }
    Ok(rewrites)
}

/// The per-file text edits one proposal carries, in path order.
///
/// `documentChanges` is preferred over `changes` when both are present, as
/// the protocol specifies. A resource operation, an edit outside the tree
/// root, a version other than the opened document's, or a proposal past
/// the file and edit bounds refuses.
pub(crate) fn proposal_documents(
    edit: &WorkspaceEdit,
    tree_root: &TreeRoot,
    context: &ProposalContext<'_>,
) -> Result<Vec<(CoreProjectPath, Vec<TextEdit>)>, PlanEnd> {
    let mut documents: BTreeMap<CoreProjectPath, Vec<TextEdit>> = BTreeMap::new();
    match (&edit.document_changes, &edit.changes) {
        (Some(DocumentChanges::Edits(edits)), _) => {
            collect_document_edits(&mut documents, tree_root, context, edits)?;
        }
        (Some(DocumentChanges::Operations(operations)), _) => {
            collect_operations(&mut documents, tree_root, context, operations)?;
        }
        (None, Some(changes)) => collect_changes(&mut documents, tree_root, context, changes)?,
        (None, None) => {}
    }
    refused_past_bounds(&documents, context.operation)?;
    Ok(documents.into_iter().collect())
}

/// Collects every `TextDocumentEdit` into the per-file map.
fn collect_document_edits(
    documents: &mut BTreeMap<CoreProjectPath, Vec<TextEdit>>,
    tree_root: &TreeRoot,
    context: &ProposalContext<'_>,
    edits: &[TextDocumentEdit],
) -> Result<(), PlanEnd> {
    for document in edits {
        collect_document_edit(documents, tree_root, context, document)?;
    }
    Ok(())
}

/// Collects mixed operations, refusing every resource operation: a
/// proposal that creates, moves, or deletes files is not applied.
fn collect_operations(
    documents: &mut BTreeMap<CoreProjectPath, Vec<TextEdit>>,
    tree_root: &TreeRoot,
    context: &ProposalContext<'_>,
    operations: &[DocumentChangeOperation],
) -> Result<(), PlanEnd> {
    for operation in operations {
        match operation {
            DocumentChangeOperation::Op(_) => {
                return Err(PlanEnd::Refused(unsupported_refusal(format!(
                    "{} (the engine proposed a file operation this release does not \
                     apply)",
                    context.operation
                ))));
            }
            DocumentChangeOperation::Edit(document) => {
                collect_document_edit(documents, tree_root, context, document)?;
            }
        }
    }
    Ok(())
}

/// Collects the `changes` form's per-URI edit lists.
#[expect(
    clippy::mutable_key_type,
    reason = "lsp-types spells the changes map with `Uri` keys; the map is read only"
)]
fn collect_changes(
    documents: &mut BTreeMap<CoreProjectPath, Vec<TextEdit>>,
    tree_root: &TreeRoot,
    context: &ProposalContext<'_>,
    changes: &std::collections::HashMap<Uri, Vec<TextEdit>>,
) -> Result<(), PlanEnd> {
    for (uri, edits) in changes {
        let path = tree_path(tree_root, uri, context.operation)?;
        documents
            .entry(path)
            .or_default()
            .extend(edits.iter().cloned());
    }
    Ok(())
}

/// Collects one document's edits after its address and version pass.
fn collect_document_edit(
    documents: &mut BTreeMap<CoreProjectPath, Vec<TextEdit>>,
    tree_root: &TreeRoot,
    context: &ProposalContext<'_>,
    document: &TextDocumentEdit,
) -> Result<(), PlanEnd> {
    let path = tree_path(tree_root, &document.text_document.uri, context.operation)?;
    verified_document_version(&path, document.text_document.version, context)?;
    let edits = documents.entry(path).or_default();
    for edit in &document.edits {
        edits.push(text_edit(edit));
    }
    Ok(())
}

/// The plain edit either form carries; an annotated edit contributes its
/// text edit and its annotation is not consulted.
fn text_edit(edit: &OneOf<TextEdit, AnnotatedTextEdit>) -> TextEdit {
    match edit {
        OneOf::Left(edit) => edit.clone(),
        OneOf::Right(annotated) => annotated.text_edit.clone(),
    }
}

/// Accepts a version-free document, or the opened document at exactly the
/// version its `didOpen` carried; any other version refuses, because the
/// engine then edited bytes the server does not hold.
fn verified_document_version(
    path: &CoreProjectPath,
    version: Option<i32>,
    context: &ProposalContext<'_>,
) -> Result<(), PlanEnd> {
    let opened = context
        .opened
        .filter(|(opened_path, _)| path == *opened_path)
        .map(|(_, opened_version)| opened_version);
    match version {
        None => Ok(()),
        Some(version) if opened == Some(version) => Ok(()),
        Some(version) => Err(PlanEnd::Refused(unsupported_refusal(format!(
            "{} (the engine edited {} at document version {version}, which the server \
             does not hold)",
            context.operation,
            path.as_str()
        )))),
    }
}

/// The project path one proposal URI addresses, or the refusal for a URI
/// the workspace tree cannot address.
fn tree_path(
    tree_root: &TreeRoot,
    uri: &Uri,
    operation: &'static str,
) -> Result<CoreProjectPath, PlanEnd> {
    tree_root.project_path(uri).map_err(|error| {
        PlanEnd::Refused(unsupported_refusal(format!(
            "{operation} (the engine proposed an edit outside the workspace tree: \
             {}: {error})",
            uri.as_str()
        )))
    })
}

/// Refuses a proposal past the file or per-file edit bounds.
fn refused_past_bounds(
    documents: &BTreeMap<CoreProjectPath, Vec<TextEdit>>,
    operation: &'static str,
) -> Result<(), PlanEnd> {
    if documents.len() > RENAME_FILES_MAX {
        return Err(PlanEnd::Refused(unsupported_refusal(format!(
            "{operation} (the engine proposed edits to {} files; at most \
             {RENAME_FILES_MAX} are applied)",
            documents.len()
        ))));
    }
    let oversized = documents
        .iter()
        .find(|(_, edits)| edits.len() > RENAME_FILE_EDITS_MAX);
    match oversized {
        Some((path, edits)) => Err(PlanEnd::Refused(unsupported_refusal(format!(
            "{operation} (the engine proposed {} edits to {}; at most \
             {RENAME_FILE_EDITS_MAX} are applied)",
            edits.len(),
            path.as_str()
        )))),
        None => Ok(()),
    }
}

/// Refuses a file past the per-file byte bound.
pub(crate) fn refused_oversized(
    path: &CoreProjectPath,
    source_len: usize,
    operation: &'static str,
) -> Result<(), PlanEnd> {
    if source_len <= RENAME_FILE_BYTES_MAX {
        return Ok(());
    }
    Err(PlanEnd::Refused(unsupported_refusal(format!(
        "{operation} (file {} holds {source_len} bytes; at most \
         {RENAME_FILE_BYTES_MAX} are rewritten)",
        path.as_str()
    ))))
}

/// The bytes one edited file compiles against: the context's held bytes
/// for a path it carries a base for, current disk bytes for every other
/// file.
async fn document_base(
    workspace_root: &Path,
    context: &ProposalContext<'_>,
    path: &CoreProjectPath,
) -> Result<String, PlanEnd> {
    if let Some(held) = context.bases.get(path) {
        return Ok((*held).to_owned());
    }
    let disk = tokio::fs::read_to_string(workspace_root.join(path.as_str())).await;
    match disk {
        Ok(source) => Ok(source),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(PlanEnd::Refused(ChangeResult::refused(
                RefusalReason::UnmetPrecondition,
                vec![failed_precondition(
                    OperationPreconditionKind::TargetExists,
                    &context.addresses,
                    path,
                    PreconditionValue::Boolean { value: true },
                    PreconditionValue::Boolean { value: false },
                )],
            )))
        }
        Err(error) => Err(ReadFault::storage(path.as_str(), "read", &error).into()),
    }
}

/// Applies one file's text edits to its base bytes, bottom-up.
///
/// Edits are sorted by start position and applied last-first, so earlier
/// offsets stay stable; overlapping edits and positions that do not land
/// in the base refuse.
fn rewritten_source(
    context: &ProposalContext<'_>,
    path: &CoreProjectPath,
    base: &str,
    edits: &[TextEdit],
    encoding: PositionEncoding,
) -> Result<String, PlanEnd> {
    let spans = edit_spans(context, path, base, edits, encoding)?;
    let mut next = base.to_owned();
    for (range, text) in spans.iter().rev() {
        next.replace_range(range.clone(), text);
    }
    Ok(next)
}

/// Each edit's byte range and replacement, sorted ascending and proven
/// non-overlapping.
fn edit_spans<'edits>(
    context: &ProposalContext<'_>,
    path: &CoreProjectPath,
    base: &str,
    edits: &'edits [TextEdit],
    encoding: PositionEncoding,
) -> Result<Vec<(std::ops::Range<usize>, &'edits str)>, PlanEnd> {
    let index = LineIndex::new(base);
    let mut spans = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = converted_offset(&index, encoding, edit.range.start, context, path)?;
        let end = converted_offset(&index, encoding, edit.range.end, context, path)?;
        if start > end {
            return Err(PlanEnd::Refused(position_fault_refusal(
                context,
                path,
                "the edit's end position precedes its start",
            )));
        }
        spans.push((start..end, edit.new_text.as_str()));
    }
    spans.sort_by_key(|(range, _)| (range.start, range.end));
    for pair in spans.windows(2) {
        if pair[0].0.end > pair[1].0.start {
            return Err(PlanEnd::Refused(unsupported_refusal(format!(
                "{} (the engine proposed overlapping edits to {})",
                context.operation,
                path.as_str()
            ))));
        }
    }
    Ok(spans)
}

/// One position converted into a byte offset, or the refusal for a
/// position that does not land in the base.
fn converted_offset(
    index: &LineIndex<'_>,
    encoding: PositionEncoding,
    position: Position,
    context: &ProposalContext<'_>,
    path: &CoreProjectPath,
) -> Result<usize, PlanEnd> {
    index.byte_offset(encoding, position).map_err(|error| {
        PlanEnd::Refused(position_fault_refusal(context, path, &error.to_string()))
    })
}

/// Findings for word-boundary occurrences of the old name that survive in
/// the changed tree, bounded by files, bytes, and finding count.
///
/// The changed tree is the served snapshot's visible file set with each
/// rewritten file's new bytes substituted: a rename rewrites existing
/// files and never creates one, so the set is exact and no fresh scan
/// races the publish.
pub(crate) fn survivor_findings(reads: &ReadService, plan: &RenamePlan) -> Vec<Diagnostic> {
    let rewritten: BTreeMap<&CoreProjectPath, &str> = plan
        .rewrites
        .iter()
        .map(|rewrite| (&rewrite.path, rewrite.next_source.as_str()))
        .collect();
    let mut findings = Vec::new();
    let mut scanned_bytes: usize = 0;
    for file in reads.index().files().take(RENAME_SWEEP_FILES_MAX) {
        if findings.len() >= RENAME_SWEEP_FINDINGS_MAX || scanned_bytes >= RENAME_SWEEP_BYTES_MAX {
            break;
        }
        let text = rewritten
            .get(file.path())
            .copied()
            .unwrap_or_else(|| file.source());
        scanned_bytes = scanned_bytes.saturating_add(text.len());
        let remaining = RENAME_SWEEP_FINDINGS_MAX - findings.len();
        for offset in word_boundary_occurrences(text, &plan.old_name, remaining) {
            findings.push(survivor_diagnostic(&plan.old_name, file.path(), offset));
        }
    }
    findings
}

/// Byte offsets where `name` occurs in `text` between word boundaries, at
/// most `occurrences_max` of them.
///
/// A word byte is ASCII alphanumeric, `_`, or any non-ASCII byte, so a
/// multi-byte identifier character never manufactures a boundary. The walk
/// visits each byte of `text` at most once.
pub(crate) fn word_boundary_occurrences(
    text: &str,
    name: &str,
    occurrences_max: usize,
) -> Vec<usize> {
    if name.is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    text.match_indices(name)
        .filter(|(offset, matched)| {
            let clear_before = *offset == 0 || !is_word_byte(bytes[offset - 1]);
            let clear_after = bytes
                .get(offset + matched.len())
                .is_none_or(|byte| !is_word_byte(*byte));
            clear_before && clear_after
        })
        .map(|(offset, _)| offset)
        .take(occurrences_max)
        .collect()
}

/// Whether one byte continues a word for the boundary check.
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

/// One surviving occurrence of the old name, as a warning finding with the
/// stable rename-survivor code.
fn survivor_diagnostic(old_name: &str, path: &CoreProjectPath, offset: usize) -> Diagnostic {
    let start = offset as u64;
    Diagnostic {
        severity: Severity::Warning,
        code: Some(DiagnosticCode::RenameSurvivor.code()),
        message: format!(
            "a word-boundary occurrence of the old name {old_name} survives the rename"
        ),
        span: Some(SourceSpan {
            unit: file_id(path),
            range: TextRange {
                start,
                end: start + old_name.len() as u64,
            },
        }),
        related: Vec::new(),
        tags: Vec::new(),
        reliability: DiagnosticReliability::Reliable,
        continuation: DiagnosticContinuation::Unknown,
        extensions: Extensions(BTreeMap::new()),
        language: None,
    }
}

/// One finding a plan flow raises itself, without a source location.
pub(crate) fn plan_diagnostic(message: String) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        code: None,
        message,
        span: None,
        related: Vec::new(),
        tags: Vec::new(),
        reliability: DiagnosticReliability::Reliable,
        continuation: DiagnosticContinuation::Unknown,
        extensions: Extensions(BTreeMap::new()),
        language: None,
    }
}

/// A proposal this release does not apply, with the capability detail as a
/// warning finding.
pub(crate) fn unsupported_refusal(detail: String) -> ChangeResult {
    ChangeResult::Refused {
        reason: RefusalReason::Unsupported,
        preconditions: Vec::new(),
        diagnostics: vec![plan_diagnostic(detail)],
    }
}

/// The engine declined the rename: an unmet condition naming the addressed
/// declaration, with the engine's verdict as the finding.
fn declined_refusal(address: &SymbolAddress, detail: String) -> ChangeResult {
    ChangeResult::Refused {
        reason: RefusalReason::UnmetPrecondition,
        preconditions: vec![failed_precondition(
            OperationPreconditionKind::TargetExists,
            &[PreconditionAddress::Symbol {
                symbol: address.wire_symbol(),
            }],
            &address.path,
            PreconditionValue::Boolean { value: true },
            PreconditionValue::Boolean { value: false },
        )],
        diagnostics: vec![plan_diagnostic(detail)],
    }
}

/// The engine's positions do not land in the source the server holds: an
/// unmet `source_unchanged` condition, with the position fault as the
/// finding.
fn position_fault_refusal(
    context: &ProposalContext<'_>,
    path: &CoreProjectPath,
    detail: &str,
) -> ChangeResult {
    ChangeResult::Refused {
        reason: RefusalReason::UnmetPrecondition,
        preconditions: vec![failed_precondition(
            OperationPreconditionKind::SourceUnchanged,
            &context.addresses,
            path,
            PreconditionValue::Boolean { value: true },
            PreconditionValue::Boolean { value: false },
        )],
        diagnostics: vec![plan_diagnostic(format!(
            "the engine's edit does not land in the source Rift holds: {detail}"
        ))],
    }
}

/// One failed condition naming the addressed subjects and the path it was
/// checked against.
pub(crate) fn failed_precondition(
    kind: OperationPreconditionKind,
    addresses: &[PreconditionAddress],
    path: &CoreProjectPath,
    expected: PreconditionValue,
    observed: PreconditionValue,
) -> OperationPrecondition {
    OperationPrecondition::new(
        kind,
        OperationPreconditionStatus::Failed,
        addresses.to_vec(),
        vec![path.as_str().to_owned()],
        expected,
        observed,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use lsp_types::{OptionalVersionedTextDocumentIdentifier, Range as EditRange};
    use rift_core::{SourceVisibility, TextFileInclusion};
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::configuration::HistoryConfiguration;
    use rift_syntax::ByteRange;

    use super::*;

    fn project_path(value: &str) -> CoreProjectPath {
        CoreProjectPath::new(value).expect("fixture path is valid")
    }

    fn address() -> SymbolAddress {
        SymbolAddress {
            language_segment: "rust".to_owned(),
            path: project_path("lib.rs"),
            qualified_name: "beacon".to_owned(),
        }
    }

    fn tree() -> TreeRoot {
        TreeRoot::from_slash_form("/rift-ws").expect("fixture root is absolute")
    }

    fn uri(path: &str) -> Uri {
        tree()
            .document_uri(&project_path(path))
            .expect("fixture uri composes")
    }

    fn at(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn span(start: (u32, u32), end: (u32, u32)) -> EditRange {
        EditRange {
            start: at(start.0, start.1),
            end: at(end.0, end.1),
        }
    }

    fn edit(start: (u32, u32), end: (u32, u32), text: &str) -> TextEdit {
        TextEdit {
            range: span(start, end),
            new_text: text.to_owned(),
        }
    }

    fn refusal(end: PlanEnd) -> ChangeResult {
        match end {
            PlanEnd::Refused(result) => result,
            PlanEnd::Failed(error) => panic!("expected a refusal, got failure {error:?}"),
        }
    }

    fn refusal_reason(result: &ChangeResult) -> RefusalReason {
        match result {
            ChangeResult::Refused { reason, .. } => *reason,
            ChangeResult::Applied { .. } => panic!("expected a refusal, got an applied change"),
        }
    }

    fn refusal_detail(result: &ChangeResult) -> String {
        match result {
            ChangeResult::Refused { diagnostics, .. } => diagnostics
                .first()
                .map(|finding| finding.message.clone())
                .unwrap_or_default(),
            ChangeResult::Applied { .. } => panic!("expected a refusal, got an applied change"),
        }
    }

    fn first_precondition_kind(result: &ChangeResult) -> OperationPreconditionKind {
        match result {
            ChangeResult::Refused { preconditions, .. } => {
                preconditions
                    .first()
                    .expect("a failed condition rides")
                    .kind
            }
            ChangeResult::Applied { .. } => panic!("expected a refusal, got an applied change"),
        }
    }

    /// A rename-shaped context with no held bases and nothing opened.
    fn plain_context() -> ProposalContext<'static> {
        ProposalContext {
            operation: RENAME_OPERATION,
            addresses: vec![PreconditionAddress::Symbol {
                symbol: address().wire_symbol(),
            }],
            opened: None,
            bases: BTreeMap::new(),
        }
    }

    fn documents_of(
        edit: &WorkspaceEdit,
    ) -> Result<Vec<(CoreProjectPath, Vec<TextEdit>)>, PlanEnd> {
        let opened = project_path("lib.rs");
        let context = ProposalContext {
            opened: Some((&opened, 1)),
            ..plain_context()
        };
        proposal_documents(edit, &tree(), &context)
    }

    /// Builds a `changes`-form proposal; the key type is lsp-types' own.
    fn changes_proposal(entries: Vec<(Uri, Vec<TextEdit>)>) -> WorkspaceEdit {
        WorkspaceEdit {
            changes: Some(entries.into_iter().collect()),
            ..WorkspaceEdit::default()
        }
    }

    #[test]
    fn changes_form_collects_and_merges_per_file_edit_lists() {
        let proposal = changes_proposal(vec![
            (uri("lib.rs"), vec![edit((0, 7), (0, 13), "flare")]),
            (uri("main.rs"), vec![edit((0, 0), (0, 6), "flare")]),
        ]);
        let documents = documents_of(&proposal).expect("the proposal compiles");
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].0, project_path("lib.rs"));
        assert_eq!(documents[1].0, project_path("main.rs"));
    }

    #[test]
    fn document_changes_form_is_preferred_and_merges_duplicate_uris() {
        let document = |text: &str| TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: uri("lib.rs"),
                version: None,
            },
            edits: vec![OneOf::Left(edit((0, 0), (0, 1), text))],
        };
        let mut proposal =
            changes_proposal(vec![(uri("ignored.rs"), vec![edit((0, 0), (0, 1), "x")])]);
        proposal.document_changes =
            Some(DocumentChanges::Edits(vec![document("a"), document("b")]));
        let documents = documents_of(&proposal).expect("the proposal compiles");
        assert_eq!(documents.len(), 1, "the changes form must be ignored");
        assert_eq!(documents[0].1.len(), 2, "duplicate URIs merge their edits");
    }

    #[test]
    fn annotated_edit_contributes_its_text_edit() {
        let annotated = AnnotatedTextEdit {
            text_edit: edit((0, 0), (0, 6), "flare"),
            annotation_id: "refactor".to_owned(),
        };
        let proposal = WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri("lib.rs"),
                    version: Some(1),
                },
                edits: vec![OneOf::Right(annotated)],
            }])),
            ..WorkspaceEdit::default()
        };
        let documents = documents_of(&proposal).expect("the proposal compiles");
        assert_eq!(documents[0].1[0].new_text, "flare");
    }

    #[test]
    fn resource_operation_refuses_unsupported() {
        let proposal = WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(lsp_types::ResourceOp::Delete(lsp_types::DeleteFile {
                    uri: uri("lib.rs"),
                    options: None,
                })),
            ])),
            ..WorkspaceEdit::default()
        };
        let result = refusal(documents_of(&proposal).expect_err("file operations refuse"));
        assert_eq!(refusal_reason(&result), RefusalReason::Unsupported);
        assert!(refusal_detail(&result).contains("file operation"));
    }

    #[test]
    fn out_of_tree_uri_refuses_and_names_the_uri() {
        let outside: Uri = "file:///rift-elsewhere/out.rs".parse().expect("uri parses");
        let proposal = changes_proposal(vec![(outside, vec![edit((0, 0), (0, 1), "x")])]);
        let result = refusal(documents_of(&proposal).expect_err("an outside URI refuses"));
        assert_eq!(refusal_reason(&result), RefusalReason::Unsupported);
        assert!(refusal_detail(&result).contains("outside the workspace tree"));
        assert!(refusal_detail(&result).contains("rift-elsewhere"));
    }

    #[test]
    fn document_versions_accept_none_and_the_opened_version_only() {
        let versioned = |uri_path: &str, version: Option<i32>| WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri(uri_path),
                    version,
                },
                edits: vec![OneOf::Left(edit((0, 0), (0, 1), "x"))],
            }])),
            ..WorkspaceEdit::default()
        };
        assert!(documents_of(&versioned("lib.rs", None)).is_ok());
        assert!(documents_of(&versioned("lib.rs", Some(1))).is_ok());
        let stale = refusal(
            documents_of(&versioned("lib.rs", Some(7))).expect_err("a stale version refuses"),
        );
        assert_eq!(refusal_reason(&stale), RefusalReason::Unsupported);
        assert!(refusal_detail(&stale).contains("document version 7"));
        let unopened = refusal(
            documents_of(&versioned("main.rs", Some(1)))
                .expect_err("a version on an unopened document refuses"),
        );
        assert_eq!(refusal_reason(&unopened), RefusalReason::Unsupported);
        // An operation that opened nothing - a file move - accepts only
        // version-free documents.
        let nothing_opened = plain_context();
        assert!(proposal_documents(&versioned("lib.rs", None), &tree(), &nothing_opened).is_ok());
        let refused = refusal(
            proposal_documents(&versioned("lib.rs", Some(1)), &tree(), &nothing_opened)
                .expect_err("every versioned document refuses when nothing was opened"),
        );
        assert_eq!(refusal_reason(&refused), RefusalReason::Unsupported);
    }

    #[test]
    fn proposals_past_the_file_and_edit_bounds_refuse() {
        let entries: Vec<(Uri, Vec<TextEdit>)> = (0..=RENAME_FILES_MAX)
            .map(|index| {
                (
                    uri(&format!("file_{index}.rs")),
                    vec![edit((0, 0), (0, 1), "x")],
                )
            })
            .collect();
        let oversized_files = changes_proposal(entries);
        let result = refusal(documents_of(&oversized_files).expect_err("too many files refuse"));
        assert!(refusal_detail(&result).contains("files"));

        let oversized_edits = changes_proposal(vec![(
            uri("lib.rs"),
            vec![edit((0, 0), (0, 1), "x"); RENAME_FILE_EDITS_MAX + 1],
        )]);
        let result = refusal(documents_of(&oversized_edits).expect_err("too many edits refuse"));
        assert!(refusal_detail(&result).contains("edits"));
    }

    #[test]
    fn rewritten_source_applies_multiple_edits_bottom_up() {
        let base = "fn beacon() {}\nfn caller() { beacon(); }\n";
        let edits = vec![
            edit((0, 3), (0, 9), "flare"),
            edit((1, 14), (1, 20), "flare"),
        ];
        let next = rewritten_source(
            &plain_context(),
            &project_path("lib.rs"),
            base,
            &edits,
            PositionEncoding::Utf8,
        )
        .expect("the edits compile");
        assert_eq!(next, "fn flare() {}\nfn caller() { flare(); }\n");
    }

    #[test]
    fn utf16_and_utf8_positions_land_on_the_same_bytes() {
        let base = "let \u{e9}\u{20ac}\u{1d11e} = beacon;\n";
        let target = base.find("beacon").expect("the name exists");
        let utf16_column =
            u32::try_from(base[..target].chars().map(char::len_utf16).sum::<usize>())
                .expect("the fixture line fits in u32");
        let utf8_column = u32::try_from(target).expect("the fixture line fits in u32");
        let by_utf16 = rewritten_source(
            &plain_context(),
            &project_path("lib.rs"),
            base,
            &[edit((0, utf16_column), (0, utf16_column + 6), "flare")],
            PositionEncoding::Utf16,
        )
        .expect("the utf-16 edit compiles");
        let by_utf8 = rewritten_source(
            &plain_context(),
            &project_path("lib.rs"),
            base,
            &[edit((0, utf8_column), (0, utf8_column + 6), "flare")],
            PositionEncoding::Utf8,
        )
        .expect("the utf-8 edit compiles");
        assert_eq!(by_utf16, by_utf8);
        assert_eq!(by_utf16, "let \u{e9}\u{20ac}\u{1d11e} = flare;\n");
    }

    #[test]
    fn crlf_sources_keep_their_endings_through_an_edit() {
        let base = "fn beacon() {}\r\nfn caller() { beacon(); }\r\n";
        let next = rewritten_source(
            &plain_context(),
            &project_path("lib.rs"),
            base,
            &[edit((1, 14), (1, 20), "flare")],
            PositionEncoding::Utf8,
        )
        .expect("the edit compiles");
        assert_eq!(next, "fn beacon() {}\r\nfn caller() { flare(); }\r\n");
    }

    #[test]
    fn overlapping_edits_refuse() {
        let base = "fn beacon() {}\n";
        let edits = vec![edit((0, 3), (0, 9), "flare"), edit((0, 5), (0, 7), "xx")];
        let result = refusal(
            rewritten_source(
                &plain_context(),
                &project_path("lib.rs"),
                base,
                &edits,
                PositionEncoding::Utf8,
            )
            .expect_err("overlapping edits refuse"),
        );
        assert_eq!(refusal_reason(&result), RefusalReason::Unsupported);
        assert!(refusal_detail(&result).contains("overlapping"));
    }

    #[test]
    fn reversed_edit_range_refuses_as_a_position_fault() {
        let base = "fn beacon() {}\n";
        let result = refusal(
            rewritten_source(
                &plain_context(),
                &project_path("lib.rs"),
                base,
                &[edit((0, 9), (0, 3), "flare")],
                PositionEncoding::Utf8,
            )
            .expect_err("a reversed range refuses"),
        );
        assert_eq!(refusal_reason(&result), RefusalReason::UnmetPrecondition);
        assert_eq!(
            first_precondition_kind(&result),
            OperationPreconditionKind::SourceUnchanged
        );
    }

    #[test]
    fn position_past_the_source_refuses_with_the_fault_as_evidence() {
        let base = "fn beacon() {}\n";
        let result = refusal(
            rewritten_source(
                &plain_context(),
                &project_path("lib.rs"),
                base,
                &[edit((0, 90), (0, 95), "flare")],
                PositionEncoding::Utf8,
            )
            .expect_err("a position past the line refuses"),
        );
        assert_eq!(refusal_reason(&result), RefusalReason::UnmetPrecondition);
        assert_eq!(
            first_precondition_kind(&result),
            OperationPreconditionKind::SourceUnchanged
        );
        assert!(refusal_detail(&result).contains("does not land in the source"));
    }

    #[test]
    fn word_boundary_occurrences_respect_boundaries_and_the_bound() {
        let text = "rename renamed _rename rename_ (rename) rename";
        let found = word_boundary_occurrences(text, "rename", 16);
        assert_eq!(found, vec![0, 32, 40]);
        assert_eq!(word_boundary_occurrences(text, "rename", 2).len(), 2);
        assert_eq!(word_boundary_occurrences(text, "", 16), Vec::<usize>::new());
        assert_eq!(
            word_boundary_occurrences("rename\u{e9}", "rename", 16),
            Vec::<usize>::new(),
            "a multi-byte identifier character continues the word"
        );
        assert_eq!(word_boundary_occurrences("rename", "rename", 16), vec![0]);
    }

    #[test]
    fn declaration_name_offset_finds_the_name_or_falls_back_to_the_item() {
        let source = "/// The beacon.\npub fn beacon() {}\n";
        let item_start = u64::try_from(source.find("pub fn").expect("the item exists"))
            .expect("the fixture offset fits in u64");
        let symbol = SyntaxSymbol {
            name: "beacon".to_owned(),
            qualified_name: "beacon".to_owned(),
            container: None,
            kind: "function",
            facets: Vec::new(),
            visibility: Some("pub".to_owned()),
            range: ByteRange {
                start: 0,
                end: source.len() as u64,
            },
            item_range: ByteRange {
                start: item_start,
                end: source.len() as u64,
            },
            body_range: None,
        };
        assert_eq!(
            declaration_name_offset(source, &symbol),
            source.rfind("beacon").expect("the name exists"),
            "the doc comment's mention must not win over the declaration's own name"
        );
        let mut unnamed = symbol;
        unnamed.name = "phantom".to_owned();
        assert_eq!(
            declaration_name_offset(source, &unnamed),
            usize::try_from(item_start).expect("the fixture offset fits in usize"),
            "an unspelled name falls back to the item start"
        );
    }

    #[test]
    fn segment_language_parses_bare_names_and_dialects() {
        assert_eq!(
            segment_language("rust"),
            Language {
                name: "rust".to_owned(),
                dialect: None
            }
        );
        assert_eq!(
            segment_language("typescript:tsx"),
            Language {
                name: "typescript".to_owned(),
                dialect: Some("tsx".to_owned())
            }
        );
    }

    fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, ReadService) {
        let directory = tempfile::tempdir().expect("fixture directory");
        for (name, source) in files {
            fs::write(directory.path().join(name), source).expect("fixture file writes");
        }
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )
        .expect("fixture workspace indexes");
        (directory, reads)
    }

    fn plan(rewrites: Vec<PlannedRewrite>) -> RenamePlan {
        RenamePlan {
            symbol: SymbolId("rift://symbol/rust/lib.rs/beacon".to_owned()),
            old_name: "beacon".to_owned(),
            rewrites,
        }
    }

    #[test]
    fn survivor_findings_report_unrewritten_word_boundary_occurrences() {
        let (_directory, reads) = workspace(&[
            ("lib.rs", "pub fn beacon() {}\n"),
            (
                "notes.md",
                "# beacon\nThe beacon endures; renamed beacons aside.\n",
            ),
        ]);
        let renamed = plan(vec![PlannedRewrite {
            path: project_path("lib.rs"),
            base_source: "pub fn beacon() {}\n".to_owned(),
            next_source: "pub fn flare() {}\n".to_owned(),
        }]);
        let findings = survivor_findings(&reads, &renamed);
        assert_eq!(
            findings.len(),
            2,
            "notes.md holds two surviving occurrences"
        );
        for finding in &findings {
            assert_eq!(finding.severity, Severity::Warning);
            assert_eq!(finding.code.as_deref(), Some("rift.rename.survivor"));
            let span = finding.span.as_ref().expect("a survivor names its place");
            assert_eq!(span.unit.0, "rift://file/notes.md");
        }
    }

    #[test]
    fn survivor_findings_read_the_rewritten_bytes_and_stay_bounded() {
        let occurrences = "beacon ".repeat(RENAME_SWEEP_FINDINGS_MAX + 8);
        let (_directory, reads) = workspace(&[
            ("lib.rs", "pub fn beacon() {}\n"),
            ("notes.md", occurrences.as_str()),
            ("zed.rs", "pub fn zed() { beacon(); }\n"),
        ]);
        let clean = plan(vec![PlannedRewrite {
            path: project_path("notes.md"),
            base_source: occurrences.clone(),
            next_source: "all clear\n".to_owned(),
        }]);
        let findings = survivor_findings(&reads, &clean);
        assert_eq!(
            findings.len(),
            2,
            "the rewritten file must be swept by its new bytes; lib.rs and zed.rs still \
             spell beacon"
        );
        let unbounded = plan(Vec::new());
        let findings = survivor_findings(&reads, &unbounded);
        assert_eq!(
            findings.len(),
            RENAME_SWEEP_FINDINGS_MAX,
            "findings stop at the bound before the sweep reaches zed.rs"
        );
    }

    #[test]
    fn empty_workspace_edit_collects_no_documents() {
        let documents =
            documents_of(&WorkspaceEdit::default()).expect("an empty proposal compiles");
        assert!(documents.is_empty());
    }

    #[test]
    fn operations_form_collects_its_document_edits() {
        let proposal = WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: uri("lib.rs"),
                        version: None,
                    },
                    edits: vec![OneOf::Left(edit((0, 0), (0, 6), "flare"))],
                }),
            ])),
            ..WorkspaceEdit::default()
        };
        let documents = documents_of(&proposal).expect("the operations form compiles");
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].1[0].new_text, "flare");
    }

    #[test]
    fn refused_oversized_accepts_the_bound_and_refuses_past_it() {
        let path = project_path("lib.rs");
        assert!(refused_oversized(&path, RENAME_FILE_BYTES_MAX, RENAME_OPERATION).is_ok());
        let result = refusal(
            refused_oversized(&path, RENAME_FILE_BYTES_MAX + 1, RENAME_OPERATION)
                .expect_err("a file past the byte bound refuses"),
        );
        assert_eq!(refusal_reason(&result), RefusalReason::Unsupported);
        assert!(refusal_detail(&result).contains("bytes"));
    }

    #[test]
    fn name_positions_fails_when_the_offset_leaves_the_source() {
        let Err(PlanEnd::Failed(error)) = name_positions("pub fn beacon() {}\n", 999) else {
            panic!("an offset past the source must fail");
        };
        assert_eq!(error.descriptor().code(), "internal_error");
        assert!(error.to_string().contains("rename position conversion"));
    }

    #[tokio::test]
    async fn compiled_plan_fails_when_the_workspace_root_is_relative() {
        let outcome = compiled_plan(
            Path::new("relative-root"),
            &WorkspaceEdit::default(),
            PositionEncoding::Utf8,
            0,
            &address(),
            &target_over("pub fn beacon() {}\n"),
        )
        .await;
        let Err(PlanEnd::Failed(error)) = outcome else {
            panic!("a relative workspace root must fail");
        };
        assert!(error.to_string().contains("proposal root conversion"));
    }

    fn target_over(source: &str) -> RenameTarget {
        RenameTarget {
            path: project_path("lib.rs"),
            language: segment_language("rust"),
            old_name: "beacon".to_owned(),
            name_offset: 0,
            indexed_source: source.to_owned(),
        }
    }

    #[tokio::test]
    async fn verified_disk_target_accepts_matching_bytes_and_refuses_drift() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let source = "pub fn beacon() {}\n";
        fs::write(directory.path().join("lib.rs"), source).expect("fixture file writes");
        verified_disk_target(directory.path(), &address(), &target_over(source))
            .await
            .expect("matching bytes pass");
        let drifted = target_over("pub fn beacon() { let _old = 1; }\n");
        let result = refusal(
            verified_disk_target(directory.path(), &address(), &drifted)
                .await
                .expect_err("drifted bytes refuse"),
        );
        assert_eq!(refusal_reason(&result), RefusalReason::UnmetPrecondition);
        assert_eq!(
            first_precondition_kind(&result),
            OperationPreconditionKind::SourceUnchanged
        );
        let missing = verified_disk_target(
            directory.path(),
            &address(),
            &RenameTarget {
                path: project_path("vanished.rs"),
                ..target_over(source)
            },
        )
        .await
        .expect_err("a missing file is a storage failure");
        assert!(matches!(missing, PlanEnd::Failed(_)));
    }

    #[tokio::test]
    async fn document_base_serves_the_held_bytes_and_refuses_missing_files() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let opened = "pub fn beacon() {}\n";
        let held_path = project_path("lib.rs");
        let context = ProposalContext {
            bases: BTreeMap::from([(&held_path, opened)]),
            ..plain_context()
        };
        // The held base answers the exact bytes the engine saw, never a
        // fresh disk read.
        fs::write(directory.path().join("lib.rs"), "drifted\n").expect("fixture file writes");
        let base = document_base(directory.path(), &context, &held_path)
            .await
            .expect("a held path compiles against its held bytes");
        assert_eq!(base, opened);
        fs::write(directory.path().join("main.rs"), "fn caller() {}\n")
            .expect("fixture file writes");
        let disk = document_base(directory.path(), &context, &project_path("main.rs"))
            .await
            .expect("another file compiles against its disk bytes");
        assert_eq!(disk, "fn caller() {}\n");
        let missing = refusal(
            document_base(directory.path(), &context, &project_path("vanished.rs"))
                .await
                .expect_err("a missing edited file refuses"),
        );
        assert_eq!(refusal_reason(&missing), RefusalReason::UnmetPrecondition);
        assert_eq!(
            first_precondition_kind(&missing),
            OperationPreconditionKind::TargetExists
        );
        fs::create_dir(directory.path().join("nested")).expect("fixture directory creates");
        let unreadable = document_base(directory.path(), &context, &project_path("nested"))
            .await
            .expect_err("an unreadable path is a storage failure");
        assert!(matches!(unreadable, PlanEnd::Failed(_)));
    }
}
