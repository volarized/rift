//! Declaration and node removal, checked against a configured language engine's references.
//!
//! Each removal resolves the span its `replace_` neighbour resolves, widens it over the
//! separator that followed, and rewrites the file without those bytes. Before it
//! writes, the server asks the language engine configured for the declaration what still
//! references it: a standing reference refuses unless `force` overrides the refusal, and an
//! engine that cannot answer the question at all is not the same as one that answered it
//! clean, so the two stay distinguishable on the result. An empty answer from an engine that
//! has never confirmed its own readiness is a third case, distinct from both: it refuses
//! unless `force` overrides it too, because nothing tells that answer apart from the answer
//! of an engine that has not read the file yet.

use std::path::Path;

use lsp_types::Location;
use rift_core::ProjectPath as CoreProjectPath;
use rift_core::line::{lines_inclusive, without_ending};
use rift_lsp::session::{EngineError, EngineFault, EngineReadiness, EngineSession};
use rift_lsp::uri::TreeRoot;
use rift_protocol::change::{
    ChangeResult, OperationPrecondition, OperationPreconditionKind, OperationPreconditionStatus,
    PreconditionAddress, PreconditionValue, RefusalReason, RemoveNodeParams, RemoveSymbolParams,
};
use rift_protocol::read::{Diagnostic, DiagnosticCode, Language, NodeId, Severity};
use rift_syntax::ByteRange;

use crate::change::{
    NodeResolution, SymbolAddress, SymbolResolution, parse_symbol_address, resolve_node,
    resolve_symbol,
};
use crate::engine::{EnginePool, EngineSlot};
use crate::read::{ReadError, ReadFault, ReadService, digest_hex8, symbol_for_range};
use crate::rename::{
    NamePositions, PlanEnd, declaration_name_offset, failed_precondition, name_positions,
    plan_diagnostic, workspace_tree_root,
};

/// Most reference paths a removal's checked-reference finding names, deduplicated and
/// sorted. A removal touching more references than this still refuses, or still applies
/// under `force`; only the named path list stops at this bound.
pub const REMOVE_REFERENCES_MAX: usize = 64;

/// One removal, checked against references and compiled into a single-file rewrite, not yet
/// written. The change lane re-proves the base against the disk before writing.
#[derive(Debug)]
pub struct RemovePlan {
    pub(crate) path: CoreProjectPath,
    pub(crate) base_source: String,
    pub(crate) next_source: String,
    pub(crate) addresses: Vec<PreconditionAddress>,
    pub(crate) diagnostic: Option<Diagnostic>,
}

/// What planning decided: a plan ready for the change lane, or the refusal that ends the
/// request with the targeted tree untouched.
#[derive(Debug)]
pub enum RemoveResolution {
    /// The removal resolved; the change lane verifies and writes it.
    Planned(Box<RemovePlan>),
    /// A standing reference refused the removal, or the address did not resolve; the
    /// targeted tree is untouched.
    Refused(ChangeResult),
}

/// The resolved subject of one removal: its file, language, and indexed source, the range to
/// remove before widening, and the byte offset of the declaration's own name to check
/// references at - absent when the address names no declaration, `remove_node`'s
/// not-checked case.
struct RemovalTarget {
    path: CoreProjectPath,
    language: Language,
    indexed_source: String,
    remove_range: ByteRange,
    name_offset: Option<usize>,
}

/// Plans one symbol removal: resolves the address, checks its references against the
/// configured engine, and compiles the widened removal for the change lane.
///
/// # Errors
///
/// Returns [`ReadError`] for an invalid request, a filesystem failure, or an engine that
/// failed to serve outside its reference check - a spawn, timeout, or protocol fault reached
/// while opening the document. A request the server declines returns a refused
/// [`ChangeResult`] inside [`RemoveResolution`] instead.
///
/// # Cancel safety
///
/// Dropping the future writes nothing; an engine request in flight is discarded by the
/// session, and the session stays in its slot.
pub async fn plan_remove_symbol(
    reads: &ReadService,
    engines: &EnginePool,
    workspace_root: &Path,
    params: &RemoveSymbolParams,
) -> Result<RemoveResolution, ReadError> {
    match planned_remove_symbol(reads, engines, workspace_root, params).await {
        Ok(plan) => Ok(RemoveResolution::Planned(Box::new(plan))),
        Err(PlanEnd::Refused(refusal)) => Ok(RemoveResolution::Refused(refusal)),
        Err(PlanEnd::Failed(error)) => Err(error),
    }
}

/// Plans one node removal: verifies the witness, checks references against the configured
/// engine when the node names a declaration, and compiles the widened removal for the
/// change lane.
///
/// # Errors
///
/// Returns [`ReadError`] for an invalid request, a filesystem failure, or an engine that
/// failed to serve outside its reference check. A request the server declines returns a
/// refused [`ChangeResult`] inside [`RemoveResolution`] instead.
///
/// # Cancel safety
///
/// Dropping the future writes nothing; an engine request in flight is discarded by the
/// session, and the session stays in its slot.
pub async fn plan_remove_node(
    reads: &ReadService,
    engines: &EnginePool,
    workspace_root: &Path,
    params: &RemoveNodeParams,
) -> Result<RemoveResolution, ReadError> {
    match planned_remove_node(reads, engines, workspace_root, params).await {
        Ok(plan) => Ok(RemoveResolution::Planned(Box::new(plan))),
        Err(PlanEnd::Refused(refusal)) => Ok(RemoveResolution::Refused(refusal)),
        Err(PlanEnd::Failed(error)) => Err(error),
    }
}

async fn planned_remove_symbol(
    reads: &ReadService,
    engines: &EnginePool,
    workspace_root: &Path,
    params: &RemoveSymbolParams,
) -> Result<RemovePlan, PlanEnd> {
    let address = parse_symbol_address(&params.symbol.0)?;
    let target = resolved_symbol_target(reads, &address)?;
    let addresses = vec![PreconditionAddress::Symbol {
        symbol: address.wire_symbol(),
    }];
    verified_disk_source(
        workspace_root,
        &target.path,
        &target.indexed_source,
        &addresses,
    )
    .await?;
    concluded_plan(workspace_root, engines, target, addresses, params.force).await
}

async fn planned_remove_node(
    reads: &ReadService,
    engines: &EnginePool,
    workspace_root: &Path,
    params: &RemoveNodeParams,
) -> Result<RemovePlan, PlanEnd> {
    let target = resolved_node_target(reads, &params.node)?;
    let addresses = vec![PreconditionAddress::Node {
        node: params.node.clone(),
    }];
    verified_disk_source(
        workspace_root,
        &target.path,
        &target.indexed_source,
        &addresses,
    )
    .await?;
    concluded_plan(workspace_root, engines, target, addresses, params.force).await
}

/// Resolves the address to its declaration, keeping the shared refusal shapes for a missing
/// or ambiguous target.
fn resolved_symbol_target(
    reads: &ReadService,
    address: &SymbolAddress,
) -> Result<RemovalTarget, PlanEnd> {
    match resolve_symbol(reads, address)? {
        SymbolResolution::Refused {
            reason,
            preconditions,
        } => Err(PlanEnd::Refused(ChangeResult::refused(
            reason,
            preconditions,
        ))),
        SymbolResolution::Declared { file, symbol } => Ok(RemovalTarget {
            path: address.path.clone(),
            language: file.syntax().language().clone(),
            indexed_source: file.source().to_owned(),
            remove_range: symbol.range,
            name_offset: Some(declaration_name_offset(file.source(), symbol)),
        }),
    }
}

/// Resolves and witness-verifies the node address, keeping the shared refusal shapes for a
/// missing file or a stale witness. A node whose range names no declaration carries no name
/// offset, which [`checked_references`] reads as the not-checked case.
fn resolved_node_target(reads: &ReadService, node: &NodeId) -> Result<RemovalTarget, PlanEnd> {
    match resolve_node(reads, node)? {
        NodeResolution::Refused {
            reason,
            preconditions,
        } => Err(PlanEnd::Refused(ChangeResult::refused(
            reason,
            preconditions,
        ))),
        NodeResolution::Verified { file, address } => {
            let name_offset = symbol_for_range(file, address.range)
                .map(|symbol| declaration_name_offset(file.source(), symbol));
            Ok(RemovalTarget {
                path: address.path,
                language: file.syntax().language().clone(),
                indexed_source: file.source().to_owned(),
                remove_range: address.range,
                name_offset,
            })
        }
    }
}

/// Proves the target's disk bytes still match the resolved snapshot: the reference check,
/// and the removal it guards, must see exactly the bytes resolution ran over.
async fn verified_disk_source(
    workspace_root: &Path,
    path: &CoreProjectPath,
    indexed_source: &str,
    addresses: &[PreconditionAddress],
) -> Result<(), PlanEnd> {
    let absolute = workspace_root.join(path.as_str());
    let disk = tokio::fs::read_to_string(&absolute)
        .await
        .map_err(|error| PlanEnd::from(ReadFault::storage(path.as_str(), "read", &error)))?;
    if disk == indexed_source {
        return Ok(());
    }
    Err(PlanEnd::Refused(ChangeResult::refused(
        RefusalReason::UnmetPrecondition,
        vec![failed_precondition(
            OperationPreconditionKind::SourceUnchanged,
            addresses,
            path,
            PreconditionValue::Text {
                value: digest_hex8(indexed_source),
            },
            PreconditionValue::Text {
                value: digest_hex8(&disk),
            },
        )],
    )))
}

/// Widens the removal span, checks references, and builds the plan the outcome carries.
async fn concluded_plan(
    workspace_root: &Path,
    engines: &EnginePool,
    target: RemovalTarget,
    addresses: Vec<PreconditionAddress>,
    force: bool,
) -> Result<RemovePlan, PlanEnd> {
    let widened = widened_removal_span(&target.indexed_source, target.remove_range);
    let check = checked_references(workspace_root, engines, &target).await?;
    match check {
        ReferenceCheck::Clean => Ok(built_plan(target, widened, addresses, None)),
        ReferenceCheck::Found { count, paths } if force => Ok(built_plan(
            target,
            widened,
            addresses,
            Some(forced_reference_diagnostic(count, &paths)),
        )),
        ReferenceCheck::Found { count, paths } => {
            Err(PlanEnd::Refused(reference_refusal(addresses, count, paths)))
        }
        ReferenceCheck::Unconfirmed { engine } if force => Ok(built_plan(
            target,
            widened,
            addresses,
            Some(unchecked_diagnostic(&NotChecked::Unconfirmed { engine })),
        )),
        ReferenceCheck::Unconfirmed { .. } => Err(PlanEnd::Failed(ReadFault::unavailable(
            "remove reference check",
            "the engine has not confirmed it is ready",
        ))),
        ReferenceCheck::NotChecked(reason) => Ok(built_plan(
            target,
            widened,
            addresses,
            Some(unchecked_diagnostic(&reason)),
        )),
    }
}

/// Compiles the target and its widened span into the plan the change lane writes.
///
/// `widened` is already resolved: `resolved_symbol_target` and `resolved_node_target` only
/// ever hand [`widened_removal_span`] a range proven to land inside `target.indexed_source`,
/// and widening only moves along that same source's own line boundaries. Nothing here
/// clamps the range again.
///
/// # Panics
///
/// Panics when `widened` does not land inside `target.indexed_source` - a programmer error,
/// since every caller resolves the range first.
fn built_plan(
    target: RemovalTarget,
    widened: ByteRange,
    addresses: Vec<PreconditionAddress>,
    diagnostic: Option<Diagnostic>,
) -> RemovePlan {
    let start = usize::try_from(widened.start).expect("widened span fits this platform's usize");
    let end = usize::try_from(widened.end).expect("widened span fits this platform's usize");
    assert!(
        end <= target.indexed_source.len(),
        "removal span must land inside its already-resolved source: start={start}, end={end}, \
         source_len={}",
        target.indexed_source.len()
    );
    let mut next_source =
        String::with_capacity(target.indexed_source.len().saturating_sub(end - start));
    next_source.push_str(&target.indexed_source[..start]);
    next_source.push_str(&target.indexed_source[end..]);
    RemovePlan {
        path: target.path,
        base_source: target.indexed_source,
        next_source,
        addresses,
        diagnostic,
    }
}

/// What the reference check found for one removal target.
enum ReferenceCheck {
    /// The engine answered and named nothing.
    Clean,
    /// The engine named at least one reference: `count` is the number the engine answered,
    /// `paths` the deduplicated, bounded project paths it named.
    Found { count: u64, paths: Vec<String> },
    /// The engine answered with no references while it had never confirmed its own
    /// readiness: nothing distinguishes that answer from the answer of an engine that has
    /// not read the file yet, so it is not proof the declaration is unreferenced.
    Unconfirmed { engine: String },
    /// No check ran, or the one that did could not be read as an answer.
    NotChecked(NotChecked),
}

/// Why a removal's reference check did not run to a verdict.
enum NotChecked {
    /// The address names no declaration - `remove_node` targeting a node no symbol covers.
    NodeNamesNoSymbol,
    /// No engine is configured for the declaration's language.
    NoEngine { language_segment: String },
    /// The engine does not advertise `textDocument/references`.
    CapabilityAbsent { engine: String },
    /// The engine failed to answer the request.
    RequestFailed { engine: String },
    /// The engine answered with no references while it had never confirmed its own
    /// readiness; `force` let the removal proceed anyway.
    Unconfirmed { engine: String },
}

impl NotChecked {
    /// The reason, in words a warning finding can carry.
    fn detail(&self) -> String {
        match self {
            Self::NodeNamesNoSymbol => {
                "the node names no declaration to check references for".to_owned()
            }
            Self::NoEngine { language_segment } => {
                format!("no engine is configured for language {language_segment}")
            }
            Self::CapabilityAbsent { engine } => {
                format!("engine {engine} does not advertise textDocument/references")
            }
            Self::RequestFailed { engine } => {
                format!("engine {engine} did not answer the reference check")
            }
            Self::Unconfirmed { engine } => format!(
                "engine {engine} answered with no references and has announced no work of \
                 its own, so it may not have read the file yet"
            ),
        }
    }
}

/// Runs the reference check for one removal target, downgrading every engine failure - a
/// spawn, a timeout, a protocol fault, the capability itself being absent - to the
/// not-checked case rather than failing the request: a removal the engine cannot check is
/// still a removal the caller asked for, and the not-checked warning says why the tree was
/// not proven clean.
async fn checked_references(
    workspace_root: &Path,
    engines: &EnginePool,
    target: &RemovalTarget,
) -> Result<ReferenceCheck, PlanEnd> {
    let Some(name_offset) = target.name_offset else {
        return Ok(ReferenceCheck::NotChecked(NotChecked::NodeNamesNoSymbol));
    };
    let Some(slot) = engines.engine_for(&target.language) else {
        return Ok(ReferenceCheck::NotChecked(NotChecked::NoEngine {
            language_segment: target.language.identity_segment(),
        }));
    };
    let positions = name_positions(&target.indexed_source, name_offset)?;
    let tree_root = workspace_tree_root(workspace_root)?;
    let exchanged = engine_exchange(
        slot,
        &target.path,
        &target.language,
        &target.indexed_source,
        &positions,
    )
    .await;
    match exchanged {
        Ok((locations, readiness)) => Ok(reference_check_from_locations(
            &tree_root,
            &locations,
            readiness,
            slot.name(),
        )),
        Err(error) => {
            let engine = slot.name().to_owned();
            if matches!(error.fault(), EngineFault::CapabilityAbsent { .. }) {
                Ok(ReferenceCheck::NotChecked(NotChecked::CapabilityAbsent {
                    engine,
                }))
            } else {
                Ok(ReferenceCheck::NotChecked(NotChecked::RequestFailed {
                    engine,
                }))
            }
        }
    }
}

/// Runs the references conversation on the claimed engine's session.
async fn engine_exchange(
    slot: &EngineSlot,
    path: &CoreProjectPath,
    language: &Language,
    indexed_source: &str,
    positions: &NamePositions,
) -> Result<(Vec<Location>, EngineReadiness), EngineError> {
    let open_path = path.clone();
    let open_language = language.name.clone();
    let open_source = indexed_source.to_owned();
    let request_path = path.clone();
    let request_positions = *positions;
    let close_path = path.clone();
    slot.request_exchange(
        move |session: &mut EngineSession| {
            let path = open_path.clone();
            let language = open_language.clone();
            let source = open_source.clone();
            Box::pin(async move { session.open(&path, &language, source).await })
        },
        move |session: &mut EngineSession| {
            let path = request_path.clone();
            Box::pin(async move { references_on_session(session, &path, &request_positions).await })
        },
        move |session: &mut EngineSession| {
            let path = close_path.clone();
            Box::pin(async move {
                let _ = session.close(&path).await;
            })
        },
    )
    .await
}

/// One references request on an open document.
///
/// The engine's readiness is read right after `references` answers: whatever the answer
/// says, this is what the engine had proven about itself when it said it.
async fn references_on_session(
    session: &mut EngineSession,
    path: &CoreProjectPath,
    positions: &NamePositions,
) -> Result<(Vec<Location>, EngineReadiness), EngineError> {
    let position = positions.negotiated(session.capabilities().position_encoding);
    let locations = session.references(path, position).await?;
    let readiness = session.readiness();
    Ok((locations, readiness))
}

/// The engine's answered locations, resolved to a checked-clean, checked-found, or
/// unconfirmed verdict. The slot first spends its bounded retry schedule. Its final empty
/// answer reads as clean only when `readiness` proves the engine has confirmed it is ready
/// or is still analyzing; an engine that has never announced any work at all gets
/// [`ReferenceCheck::Unconfirmed`] instead.
fn reference_check_from_locations(
    tree_root: &TreeRoot,
    locations: &[Location],
    readiness: EngineReadiness,
    engine: &str,
) -> ReferenceCheck {
    if locations.is_empty() {
        return if readiness == EngineReadiness::Unconfirmed {
            ReferenceCheck::Unconfirmed {
                engine: engine.to_owned(),
            }
        } else {
            ReferenceCheck::Clean
        };
    }
    let mut paths = std::collections::BTreeSet::new();
    for location in locations {
        let spelling = tree_root.project_path(&location.uri).map_or_else(
            |_| location.uri.as_str().to_owned(),
            |path| path.as_str().to_owned(),
        );
        paths.insert(spelling);
    }
    ReferenceCheck::Found {
        count: locations.len() as u64,
        paths: paths.into_iter().take(REMOVE_REFERENCES_MAX).collect(),
    }
}

/// The removal refuses: a standing reference is an unmet `no_references` condition, naming
/// the engine's own count and the paths it named.
fn reference_refusal(
    addresses: Vec<PreconditionAddress>,
    count: u64,
    paths: Vec<String>,
) -> ChangeResult {
    ChangeResult::refused(
        RefusalReason::UnmetPrecondition,
        vec![OperationPrecondition::new(
            OperationPreconditionKind::NoReferences,
            OperationPreconditionStatus::Failed,
            addresses,
            paths,
            PreconditionValue::Count { value: 0 },
            PreconditionValue::Count { value: count },
        )],
    )
}

/// The removal applies without a clean reference check: a warning naming why.
fn unchecked_diagnostic(reason: &NotChecked) -> Diagnostic {
    let mut diagnostic = plan_diagnostic(format!(
        "the declaration was removed without a reference check: {}",
        reason.detail()
    ));
    diagnostic.severity = Severity::Warning;
    diagnostic.code = Some(DiagnosticCode::RemoveUnchecked.code());
    diagnostic
}

/// The removal applies under `force` while references still stand: a warning naming them.
fn forced_reference_diagnostic(count: u64, paths: &[String]) -> Diagnostic {
    let mut diagnostic = plan_diagnostic(format!(
        "the declaration was removed with {count} reference(s) still standing: {}",
        paths.join(", ")
    ));
    diagnostic.severity = Severity::Warning;
    diagnostic.code = Some(DiagnosticCode::RemoveReference.code());
    diagnostic
}

/// One line's byte span, its ending included; `content_end` excludes it.
#[derive(Clone, Copy, Debug)]
struct LineSpan {
    start: usize,
    end: usize,
    content_end: usize,
}

/// `source` segmented into [`LineSpan`]s in byte order, through
/// [`rift_core::line::lines_inclusive`] so a CRLF ending is kept with the line it terminates.
fn line_spans(source: &str) -> Vec<LineSpan> {
    let mut spans = Vec::new();
    let mut offset = 0_usize;
    for line in lines_inclusive(source) {
        let content_len = without_ending(line).len();
        spans.push(LineSpan {
            start: offset,
            end: offset + line.len(),
            content_end: offset + content_len,
        });
        offset += line.len();
    }
    spans
}

/// The line whose span contains `offset`, absent past the source's last line.
fn line_at(spans: &[LineSpan], offset: usize) -> Option<LineSpan> {
    let index = spans.partition_point(|line| line.end <= offset);
    spans
        .get(index)
        .filter(|line| line.start <= offset)
        .copied()
}

/// The line whose span ends exactly at `offset`: the line immediately before it.
fn line_ending_at(spans: &[LineSpan], offset: usize) -> Option<LineSpan> {
    if offset == 0 {
        return None;
    }
    let index = spans.partition_point(|line| line.end <= offset);
    index
        .checked_sub(1)
        .map(|previous| spans[previous])
        .filter(|line| line.end == offset)
}

/// Whether a line's content - its bytes with the ending stripped - holds nothing but spaces
/// and tabs.
pub(crate) fn is_blank_content(content: &str) -> bool {
    content.bytes().all(|byte| byte == b' ' || byte == b'\t')
}

/// Whether line content carries only blank bytes and structural punctuation.
fn is_separator_content(content: &str) -> bool {
    let has_punctuation = content.bytes().any(|byte| byte.is_ascii_punctuation());
    let contains_only_separator_bytes = content
        .bytes()
        .all(|byte| byte == b' ' || byte == b'\t' || byte.is_ascii_punctuation());
    has_punctuation && contains_only_separator_bytes
}

/// The start a removal takes when real source follows it on its own line and a blank run
/// stands before that line: the end of the last non-blank line's content, so the source left
/// behind rejoins the line it sat on before whatever put it here separated the two.
///
/// A removal whose line is preceded directly by real source retreats nowhere: those two
/// lines were separate before the removal and stay separate after it. Only the blank run an
/// insertion leaves behind is taken back, which is what makes insert-then-remove return the
/// original bytes when the anchor shared its line with another declaration.
fn rejoined_start(source: &str, spans: &[LineSpan], start: usize) -> usize {
    let mut retreated = start;
    while let Some(previous) = line_ending_at(spans, retreated) {
        let content = source
            .get(previous.start..previous.content_end)
            .unwrap_or_default();
        if !is_blank_content(content) {
            break;
        }
        retreated = previous.start;
    }
    if retreated == start {
        return start;
    }
    line_ending_at(spans, retreated).map_or(retreated, |previous| previous.content_end)
}

/// Widens `span` over bytes that isolate a removed declaration or node.
///
/// The rule, in order:
/// 1. If every byte between the line start and `span.start` is blank, the widened span
///    starts at the line start: the removal takes its own leading indentation with it.
/// 2. If the line suffix is blank, or carries only blank bytes and structural punctuation,
///    the widened span extends past that line's ending.
/// 3. Past that ending, the widened span takes one further adjacent blank-line run: from the
///    trailing side when one stands there, otherwise from the leading side, otherwise neither.
///    A removal between two declarations therefore keeps one separator.
/// 4. A span that began its own line but is followed on that line by real source takes the
///    blank run before it and the preceding line ending, so following source rejoins its line.
///
/// A span whose start is preceded by more than indentation is returned unchanged.
///
/// `span` must already land inside `source`: both removal callers resolve it first. Widening
/// only moves `start` and `end` along `source`'s own line boundaries.
///
/// # Panics
///
/// Panics when `span` does not land inside `source`.
pub(crate) fn widened_removal_span(source: &str, span: ByteRange) -> ByteRange {
    let source_len = source.len();
    let mut start = usize::try_from(span.start).expect("span fits this platform's usize");
    let mut end = usize::try_from(span.end).expect("span fits this platform's usize");
    assert!(
        start <= end && end <= source_len,
        "removal span must land inside its own source: start={start}, end={end}, \
         source_len={source_len}"
    );
    let spans = line_spans(source);

    let mut began_its_line = false;
    if let Some(line) = line_at(&spans, start) {
        let prefix = source.get(line.start..start).unwrap_or_default();
        if is_blank_content(prefix) {
            start = line.start;
            began_its_line = true;
        }
    }

    if let Some(line) = line_at(&spans, end) {
        let suffix = source.get(end..line.content_end).unwrap_or_default();
        let separator_follows = began_its_line && is_separator_content(suffix);
        if !is_blank_content(suffix) && !separator_follows && began_its_line {
            start = rejoined_start(source, &spans, start);
        }
        if is_blank_content(suffix) || separator_follows {
            let own_ending_end = line.end;
            let mut trailing_end = own_ending_end;
            while let Some(next) = line_at(&spans, trailing_end) {
                let content = source.get(next.start..next.content_end).unwrap_or_default();
                if !is_blank_content(content) {
                    break;
                }
                trailing_end = next.end;
            }
            if trailing_end > own_ending_end {
                end = trailing_end;
            } else {
                end = own_ending_end;
                while let Some(previous) = line_ending_at(&spans, start) {
                    let content = source
                        .get(previous.start..previous.content_end)
                        .unwrap_or_default();
                    if !is_blank_content(content) {
                        break;
                    }
                    start = previous.start;
                }
            }
        }
    }

    ByteRange {
        start: start as u64,
        end: end as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_pool(
        root: &Path,
        configuration: rift_protocol::configuration::LspConfiguration,
    ) -> EnginePool {
        let key = crate::engine::LspProcessKey::named("fake");
        EnginePool::new(
            root,
            std::collections::BTreeMap::from([(key.clone(), configuration)]),
            std::collections::BTreeMap::from([("rust".to_owned(), key)]),
        )
    }

    fn range(source: &str, needle: &str) -> ByteRange {
        let start = source.find(needle).expect("needle exists in fixture");
        ByteRange {
            start: start as u64,
            end: (start + needle.len()) as u64,
        }
    }

    fn removed(source: &str, span: ByteRange) -> String {
        let start = usize::try_from(span.start).expect("fixture offset fits in usize");
        let end = usize::try_from(span.end).expect("fixture offset fits in usize");
        let mut next = source.to_owned();
        next.replace_range(start..end, "");
        next
    }

    /// One temporary workspace holding `files`, indexed, with an empty engine pool: no
    /// configured language claims an engine.
    fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, ReadService, EnginePool) {
        let directory = tempfile::tempdir().expect("fixture directory");
        for (name, source) in files {
            std::fs::write(directory.path().join(name), source).expect("fixture file writes");
        }
        let reads = ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )
        .expect("fixture workspace indexes");
        let engines = EnginePool::new(
            directory.path(),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
        );
        (directory, reads, engines)
    }

    fn symbol(qualified_name: &str) -> rift_protocol::read::SymbolId {
        rift_protocol::read::SymbolId(format!("rift://symbol/rust/lib.rs/{qualified_name}"))
    }

    #[test]
    fn widens_the_first_declaration_over_its_trailing_separator() {
        let source = "fn a() {}\n\nfn b() {}\n\nfn c() {}\n";
        let widened = widened_removal_span(source, range(source, "fn a() {}"));
        assert_eq!(removed(source, widened), "fn b() {}\n\nfn c() {}\n");
    }

    #[test]
    fn widens_a_middle_declaration_collapsing_both_separators_into_one() {
        let source = "fn a() {}\n\nfn b() {}\n\nfn c() {}\n";
        let widened = widened_removal_span(source, range(source, "fn b() {}"));
        assert_eq!(removed(source, widened), "fn a() {}\n\nfn c() {}\n");
    }

    #[test]
    fn widens_the_last_declaration_and_retreats_over_the_leading_blank_line() {
        let source = "fn a() {}\n\nfn b() {}\n";
        let widened = widened_removal_span(source, range(source, "fn b() {}"));
        assert_eq!(removed(source, widened), "fn a() {}\n");
    }

    #[test]
    fn widens_the_only_declaration_to_an_empty_file() {
        let source = "fn a() {}\n";
        let widened = widened_removal_span(source, range(source, "fn a() {}"));
        assert_eq!(removed(source, widened), "");
    }

    #[test]
    fn widens_a_declaration_indented_inside_an_impl_block() {
        let source =
            "impl Foo {\n    fn a(&self) {}\n\n    fn b(&self) {}\n\n    fn c(&self) {}\n}\n";
        let widened = widened_removal_span(source, range(source, "fn b(&self) {}"));
        assert_eq!(
            removed(source, widened),
            "impl Foo {\n    fn a(&self) {}\n\n    fn c(&self) {}\n}\n"
        );
    }

    #[test]
    fn leaves_a_mid_line_node_exactly_as_addressed() {
        let source = "let x = foo(bar, baz);\n";
        let span = range(source, "baz");
        assert_eq!(widened_removal_span(source, span), span);
    }

    #[test]
    fn widens_a_declaration_over_crlf_separators() {
        let source = "fn a() {}\r\n\r\nfn b() {}\r\n\r\nfn c() {}\r\n";
        let widened = widened_removal_span(source, range(source, "fn b() {}"));
        assert_eq!(removed(source, widened), "fn a() {}\r\n\r\nfn c() {}\r\n");
    }

    /// A declaration with a blank-line run only on its leading side - the shape an
    /// `insert_symbol` `after` insertion into a packed file produces - takes that run back
    /// instead of leaving it standing, so insert then remove returns the original bytes.
    #[test]
    fn widens_a_declaration_over_its_leading_blank_line_when_the_trailing_side_has_none() {
        let source = "fn a() {}\n\nfn x() {}\nfn b() {}\n";
        let widened = widened_removal_span(source, range(source, "fn x() {}"));
        assert_eq!(removed(source, widened), "fn a() {}\nfn b() {}\n");
    }

    /// A declaration that begins its own line but is followed there by real source, with
    /// the line directly before it also real source rather than blank, retreats nowhere:
    /// `rejoined_start` finds no blank run to reclaim, so the removal takes only its own
    /// span and the split line stays exactly as the removal left it.
    #[test]
    fn widens_a_declaration_sharing_its_trailing_line_with_no_leading_blank_line_to_reclaim() {
        let source = "fn a() {}\nfn x() {} fn c() {}\n";
        let widened = widened_removal_span(source, range(source, "fn x() {}"));
        assert_eq!(removed(source, widened), "fn a() {}\n fn c() {}\n");
    }

    #[test]
    fn not_checked_detail_names_each_reason() {
        let cases = [
            (NotChecked::NodeNamesNoSymbol, "names no declaration"),
            (
                NotChecked::NoEngine {
                    language_segment: "rust".to_owned(),
                },
                "no engine is configured for language rust",
            ),
            (
                NotChecked::CapabilityAbsent {
                    engine: "fake".to_owned(),
                },
                "does not advertise textDocument/references",
            ),
            (
                NotChecked::RequestFailed {
                    engine: "fake".to_owned(),
                },
                "did not answer the reference check",
            ),
            (
                NotChecked::Unconfirmed {
                    engine: "fake".to_owned(),
                },
                "has announced no work of its own",
            ),
        ];
        for (reason, expected) in cases {
            assert!(reason.detail().contains(expected), "{}", reason.detail());
        }
    }

    #[test]
    fn unchecked_diagnostic_carries_the_stable_code_and_severity() {
        let diagnostic = unchecked_diagnostic(&NotChecked::NoEngine {
            language_segment: "rust".to_owned(),
        });
        assert_eq!(diagnostic.severity, Severity::Warning);
        assert_eq!(diagnostic.code.as_deref(), Some("rift.remove.unchecked"));
    }

    #[test]
    fn forced_reference_diagnostic_carries_the_stable_code_and_paths() {
        let diagnostic = forced_reference_diagnostic(2, &["caller.rs".to_owned()]);
        assert_eq!(diagnostic.severity, Severity::Warning);
        assert_eq!(diagnostic.code.as_deref(), Some("rift.remove.reference"));
        assert!(diagnostic.message.contains("caller.rs"));
        assert!(diagnostic.message.contains('2'));
    }

    #[test]
    fn reference_refusal_names_no_references_with_expected_and_observed_counts() {
        let result = reference_refusal(Vec::new(), 3, vec!["caller.rs".to_owned()]);
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("a standing reference must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::NoReferences
        );
        assert_eq!(
            preconditions[0].expected,
            PreconditionValue::Count { value: 0 }
        );
        assert_eq!(
            preconditions[0].observed,
            PreconditionValue::Count { value: 3 }
        );
    }

    #[tokio::test]
    async fn plan_remove_symbol_with_a_malformed_address_fails() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        let error = plan_remove_symbol(
            &reads,
            &engines,
            directory.path(),
            &RemoveSymbolParams {
                symbol: rift_protocol::read::SymbolId("not-an-address".to_owned()),
                force: false,
            },
        )
        .await
        .expect_err("malformed symbol address must error");
        assert_eq!(error.descriptor().code(), "invalid_request");
    }

    #[tokio::test]
    async fn plan_remove_node_with_a_malformed_address_fails() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        let error = plan_remove_node(
            &reads,
            &engines,
            directory.path(),
            &RemoveNodeParams {
                node: NodeId("rift://node/rust/lib.rs@9-3#zzzzzzzz".to_owned()),
                force: false,
            },
        )
        .await
        .expect_err("inverted span must error");
        assert_eq!(error.descriptor().code(), "invalid_request");
    }

    /// A listed node's real id, read back through `nodes` the way a caller would.
    fn listed_node_id(reads: &ReadService, path: &str, position: u64) -> String {
        let listing = reads
            .nodes(rift_protocol::read::NodesParams {
                path: rift_protocol::read::ProjectPath(path.to_owned()),
                position,
                rev: None,
            })
            .expect("fixture position must list a node");
        listing.nodes[0].id.0.clone()
    }

    #[tokio::test]
    async fn plan_remove_node_with_a_forged_out_of_bounds_range_fails_untouched() {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, engines) = workspace(&[("lib.rs", source)]);
        let end = source.len() as u64 + 10;
        // A forged witness for a range wholly past the file, exactly what `replace_node`
        // refuses for the identical address: the two tools must agree.
        let witness = crate::read::digest_hex8("");
        let error = plan_remove_node(
            &reads,
            &engines,
            directory.path(),
            &RemoveNodeParams {
                node: NodeId(format!("rift://node/rust/lib.rs@0-{end}#{witness}")),
                force: false,
            },
        )
        .await
        .expect_err("a range past the file end must error");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error.to_string().contains("outside the addressed file"),
            "message must name the span fault: {error}"
        );
        let untouched =
            std::fs::read_to_string(directory.path().join("lib.rs")).expect("fixture file reads");
        assert_eq!(
            untouched, source,
            "a refused removal must leave the tree untouched"
        );
    }

    #[tokio::test]
    async fn plan_remove_node_with_a_range_naming_no_syntax_node_fails_naming_the_range() {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, engines) = workspace(&[("lib.rs", source)]);
        let start = source.find("beacon").expect("fixture names beacon");
        let end = start + "bea".len();
        let witness = crate::read::digest_hex8(&source[start..end]);
        let error = plan_remove_node(
            &reads,
            &engines,
            directory.path(),
            &RemoveNodeParams {
                node: NodeId(format!("rift://node/rust/lib.rs@{start}-{end}#{witness}")),
                force: false,
            },
        )
        .await
        .expect_err("a range naming no syntax node must error");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error.to_string().contains("outside the addressed file"),
            "message must name the range, not a witness mismatch: {error}"
        );
        let untouched =
            std::fs::read_to_string(directory.path().join("lib.rs")).expect("fixture file reads");
        assert_eq!(
            untouched, source,
            "a refused removal must leave the tree untouched"
        );
    }

    #[tokio::test]
    async fn plan_remove_node_with_a_stale_witness_refuses_source_unchanged() {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, engines) = workspace(&[("lib.rs", source)]);
        let listed = listed_node_id(&reads, "lib.rs", 3);
        let mut stale = listed;
        stale.replace_range(stale.len() - 8.., "00000000");
        let resolution = plan_remove_node(
            &reads,
            &engines,
            directory.path(),
            &RemoveNodeParams {
                node: NodeId(stale),
                force: false,
            },
        )
        .await
        .expect("a stale witness is a typed refusal, not an error");
        let RemoveResolution::Refused(result) = resolution else {
            panic!("a stale witness must refuse");
        };
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("a stale witness must refuse with preconditions");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
    }

    #[tokio::test]
    async fn plan_remove_symbol_for_a_missing_declaration_refuses_target_exists() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        let resolution = plan_remove_symbol(
            &reads,
            &engines,
            directory.path(),
            &RemoveSymbolParams {
                symbol: symbol("vanished"),
                force: false,
            },
        )
        .await
        .expect("the refusal is typed");
        let RemoveResolution::Refused(result) = resolution else {
            panic!("a missing declaration must refuse");
        };
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("a missing declaration must refuse with preconditions");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetExists
        );
    }

    #[tokio::test]
    async fn plan_remove_symbol_refuses_when_disk_drifted_from_snapshot() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() { }\n")
            .expect("fixture file writes");
        let resolution = plan_remove_symbol(
            &reads,
            &engines,
            directory.path(),
            &RemoveSymbolParams {
                symbol: symbol("beacon"),
                force: false,
            },
        )
        .await
        .expect("the refusal is typed");
        let RemoveResolution::Refused(result) = resolution else {
            panic!("drifted disk must refuse");
        };
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("drifted disk must refuse with preconditions");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
        assert_ne!(preconditions[0].expected, preconditions[0].observed);
        let untouched =
            std::fs::read_to_string(directory.path().join("lib.rs")).expect("fixture file reads");
        assert_eq!(
            untouched, "pub fn beacon() { }\n",
            "refusal leaves the tree untouched"
        );
    }

    /// One framed JSON-RPC message.
    fn framed(body: &str) -> String {
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    /// A canned `sh` engine configuration claiming `rust`, answering exactly the bytes
    /// `script` writes and nothing else. The script never reads its stdin.
    fn references_engine(
        script: String,
        retry: rift_protocol::retry::RetryPolicy,
    ) -> rift_protocol::configuration::LspConfiguration {
        rift_protocol::configuration::LspConfiguration {
            command: Some(
                rift_protocol::configuration::CommandInput::ProgramAndArguments(vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    script,
                ]),
            ),
            embedded: None,
            environment: std::collections::BTreeMap::new(),
            initialization_options: None,
            startup_timeout: rift_protocol::configuration::Duration::from_millis(10_000),
            request_timeout: rift_protocol::configuration::Duration::from_millis(10_000),
            output_limit: rift_protocol::configuration::ByteSize::from_bytes(4_096),
            retry,
            restart: rift_protocol::retry::RestartPolicy::default(),
        }
    }

    /// A workspace served by a canned `sh` engine that advertises
    /// `textDocument/references` and never announces work through `$/progress`: its
    /// readiness stays [`rift_lsp::session::EngineReadiness::Unconfirmed`] throughout. Two
    /// references answers ride the script, both empty. Configured retry attempts bound
    /// resends before outer exchange settles on final answer. Retry wait is 1ms so second
    /// request lands inside script's own `sleep` window. Script never reads stdin; it writes
    /// both answers regardless of what session sends.
    fn workspace_with_unconfirmed_references_engine(
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, ReadService, EnginePool) {
        let (directory, reads, _unused_engines) = workspace(files);
        let capabilities = framed(
            r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":{"referencesProvider":true}}}"#,
        );
        let no_references =
            |id: u64| framed(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#));
        let script = format!(
            "printf '%s' '{capabilities}{}{}'; sleep 0.2",
            no_references(1),
            no_references(2),
        );
        let retry = rift_protocol::retry::RetryPolicy {
            attempts: 2,
            delay: rift_protocol::configuration::Duration::from_millis(1),
            delay_limit: rift_protocol::configuration::Duration::from_millis(1),
        };
        let engines = engine_pool(directory.path(), references_engine(script, retry));
        (directory, reads, engines)
    }

    /// The engine announces and ends unrelated work, then answers no references before its
    /// settled reference arrives. The transcript records whether retries kept one document
    /// exchange open.
    fn workspace_with_delayed_references_engine(
        files: &[(&str, &str)],
    ) -> (
        tempfile::TempDir,
        ReadService,
        EnginePool,
        std::path::PathBuf,
    ) {
        let (directory, reads, _unused_engines) = workspace(files);
        let transcript = directory.path().join("engine-transcript");
        let capabilities = framed(
            r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":{"referencesProvider":true}}}"#,
        );
        let progress_begin = framed(
            r#"{"jsonrpc":"2.0","method":"$/progress","params":{"token":"warm","value":{"kind":"begin","title":"loading"}}}"#,
        );
        let progress_end = framed(
            r#"{"jsonrpc":"2.0","method":"$/progress","params":{"token":"warm","value":{"kind":"end"}}}"#,
        );
        let no_references = framed(r#"{"jsonrpc":"2.0","id":1,"result":null}"#);
        let caller_uri = format!("file://{}/caller.rs", directory.path().display());
        let reference = framed(&format!(
            r#"{{"jsonrpc":"2.0","id":2,"result":[{{"uri":"{caller_uri}","range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":1}}}}}}]}}"#
        ));
        let shutdown = framed(r#"{"jsonrpc":"2.0","id":3,"result":null}"#);
        let script = format!(
            "printf '%s' '{capabilities}{progress_begin}{progress_end}{no_references}{reference}{shutdown}' & exec cat > \"$1\""
        );
        let mut engine = references_engine(
            script,
            rift_protocol::retry::RetryPolicy {
                attempts: 2,
                delay: rift_protocol::configuration::Duration::from_millis(1),
                delay_limit: rift_protocol::configuration::Duration::from_millis(1),
            },
        );
        let Some(rift_protocol::configuration::CommandInput::ProgramAndArguments(command)) =
            engine.command.as_mut()
        else {
            unreachable!("references engine uses a command list")
        };
        command.extend(["rift-engine".to_owned(), transcript.display().to_string()]);
        let engines = engine_pool(directory.path(), engine);
        (directory, reads, engines, transcript)
    }

    /// Reproduces the defect an unread readiness would leave behind: an unconfirmed
    /// engine's empty references answer used to read as `Clean`. It must instead refuse -
    /// as `temporarily_unavailable`, distinct from the `unmet_precondition` refusal a
    /// standing reference gets - and leave the tree untouched.
    #[tokio::test]
    async fn plan_remove_symbol_against_an_unconfirmed_engines_empty_answer_refuses() {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, engines) =
            workspace_with_unconfirmed_references_engine(&[("lib.rs", source)]);
        let error = plan_remove_symbol(
            &reads,
            &engines,
            directory.path(),
            &RemoveSymbolParams {
                symbol: symbol("beacon"),
                force: false,
            },
        )
        .await
        .expect_err("an unconfirmed engine's empty answer must refuse, not apply");
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");
        let untouched =
            std::fs::read_to_string(directory.path().join("lib.rs")).expect("fixture file reads");
        assert_eq!(
            untouched, source,
            "a refusal over an unconfirmed check leaves the tree untouched"
        );
        engines.shutdown().await;
    }

    /// `force` overrides the unconfirmed-check refusal the same way it overrides a
    /// standing-reference refusal: the removal applies, and the result carries a warning
    /// naming why the check did not run to a verdict.
    #[tokio::test]
    async fn plan_remove_symbol_against_an_unconfirmed_engine_with_force_applies_with_a_warning() {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, engines) =
            workspace_with_unconfirmed_references_engine(&[("lib.rs", source)]);
        let resolution = plan_remove_symbol(
            &reads,
            &engines,
            directory.path(),
            &RemoveSymbolParams {
                symbol: symbol("beacon"),
                force: true,
            },
        )
        .await
        .expect("force lets an unconfirmed engine's empty answer proceed");
        let RemoveResolution::Planned(plan) = resolution else {
            panic!("force must plan the removal: {resolution:?}");
        };
        let diagnostic = plan
            .diagnostic
            .expect("an applied removal over an unconfirmed check carries a warning");
        assert_eq!(diagnostic.severity, Severity::Warning);
        assert_eq!(diagnostic.code.as_deref(), Some("rift.remove.unchecked"));
        assert!(
            diagnostic.message.contains("fake"),
            "{}",
            diagnostic.message
        );
        engines.shutdown().await;
    }

    /// Progress from unrelated work cannot make an empty semantic answer final. Retry keeps
    /// one open document until the engine returns its standing reference.
    #[tokio::test]
    async fn plan_remove_symbol_retries_a_ready_empty_answer_in_one_document_exchange() {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, engines, transcript) =
            workspace_with_delayed_references_engine(&[("lib.rs", source)]);
        let resolution = plan_remove_symbol(
            &reads,
            &engines,
            directory.path(),
            &RemoveSymbolParams {
                symbol: symbol("beacon"),
                force: false,
            },
        )
        .await
        .expect("the engine answers the reference check");
        let RemoveResolution::Refused(result) = resolution else {
            panic!("the settled standing reference must refuse: {resolution:?}");
        };
        let ChangeResult::Refused { preconditions, .. } = result else {
            panic!("standing reference carries failed condition");
        };
        assert_eq!(
            preconditions[0].paths,
            vec![rift_protocol::read::ProjectPath("caller.rs".to_owned())]
        );
        engines.shutdown().await;
        let transcript = std::fs::read_to_string(transcript).expect("transcript reads");
        assert_eq!(transcript.matches("textDocument/didOpen").count(), 1);
        assert_eq!(transcript.matches("textDocument/didClose").count(), 1);
    }

    #[test]
    fn reference_check_from_locations_falls_back_to_the_raw_uri_outside_the_tree_root() {
        let tree_root = TreeRoot::new(Path::new("/workspace")).expect("root parses");
        let outside = rift_lsp::uri::parse_uri("file:///outside/other.rs").expect("uri parses");
        let expected = outside.as_str().to_owned();
        let location = Location {
            uri: outside,
            range: lsp_types::Range::default(),
        };
        let check = reference_check_from_locations(
            &tree_root,
            std::slice::from_ref(&location),
            EngineReadiness::Ready,
            "fake",
        );
        let ReferenceCheck::Found { count, paths } = check else {
            panic!("a located reference must be found");
        };
        assert_eq!(count, 1);
        assert_eq!(paths, vec![expected]);
    }

    #[test]
    fn an_empty_answer_from_an_unconfirmed_engine_is_not_read_as_clean() {
        let tree_root = TreeRoot::new(Path::new("/workspace")).expect("root parses");
        let check =
            reference_check_from_locations(&tree_root, &[], EngineReadiness::Unconfirmed, "fake");
        assert!(
            matches!(check, ReferenceCheck::Unconfirmed { ref engine } if engine == "fake"),
            "an unconfirmed engine's empty answer must not read as clean"
        );
    }

    #[test]
    fn an_empty_answer_from_a_settled_engine_still_reads_as_clean() {
        let tree_root = TreeRoot::new(Path::new("/workspace")).expect("root parses");
        for readiness in [EngineReadiness::Ready, EngineReadiness::Analyzing] {
            let check = reference_check_from_locations(&tree_root, &[], readiness, "fake");
            assert!(
                matches!(check, ReferenceCheck::Clean),
                "readiness {readiness:?} must still read an empty answer as clean"
            );
        }
    }

    #[test]
    fn widens_an_empty_span_in_an_empty_source_without_locating_a_line() {
        let span = ByteRange { start: 0, end: 0 };
        assert_eq!(widened_removal_span("", span), span);
    }
}
