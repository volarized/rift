//! Declaration and node removal, checked against a configured language engine's references.
//!
//! Each removal resolves the span its `replace_` neighbour resolves, widens it over the
//! separator that followed, and applies one `Edit::Replace` with empty text. Before it
//! writes, the server asks the language engine configured for the declaration what still
//! references it: a standing reference refuses unless `force` overrides the refusal, and an
//! engine that cannot answer the question at all is not the same as one that answered it
//! clean, so the two stay distinguishable on the result.

use std::path::Path;

use lsp_types::Location;
use rift_core::ProjectPath as CoreProjectPath;
use rift_core::line::{lines_inclusive, without_ending};
use rift_lsp::session::{EngineError, EngineFault, EngineSession};
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
use crate::rewrite::ReplacedRegion;

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
    pub(crate) replaced: ReplacedRegion,
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
        ReferenceCheck::NotChecked(reason) => Ok(built_plan(
            target,
            widened,
            addresses,
            Some(unchecked_diagnostic(&reason)),
        )),
    }
}

/// Compiles the target and its widened span into the plan the change lane writes.
fn built_plan(
    target: RemovalTarget,
    widened: ByteRange,
    addresses: Vec<PreconditionAddress>,
    diagnostic: Option<Diagnostic>,
) -> RemovePlan {
    let source_len = target.indexed_source.len();
    let start = usize::try_from(widened.start)
        .unwrap_or(source_len)
        .min(source_len);
    let end = usize::try_from(widened.end)
        .unwrap_or(source_len)
        .min(source_len);
    let mut next_source = String::with_capacity(source_len.saturating_sub(end - start));
    next_source.push_str(&target.indexed_source[..start]);
    next_source.push_str(&target.indexed_source[end..]);
    RemovePlan {
        path: target.path,
        base_source: target.indexed_source,
        next_source,
        replaced: ReplacedRegion {
            range: widened,
            text: String::new(),
        },
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
        Ok(locations) => Ok(reference_check_from_locations(&tree_root, &locations)),
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
) -> Result<Vec<Location>, EngineError> {
    // The boxed future may only borrow the session, so each attempt gets its own owned copy
    // of the request data.
    let request_path = path.clone();
    let request_language = language.clone();
    let request_source = indexed_source.to_owned();
    let request_positions = *positions;
    slot.request(move |session: &mut EngineSession| {
        let path = request_path.clone();
        let language = request_language.clone();
        let source = request_source.clone();
        Box::pin(async move {
            exchange_on_session(session, &path, &language, &source, &request_positions).await
        })
    })
    .await
}

/// One open-references-close conversation on a running session.
async fn exchange_on_session(
    session: &mut EngineSession,
    path: &CoreProjectPath,
    language: &Language,
    indexed_source: &str,
    positions: &NamePositions,
) -> Result<Vec<Location>, EngineError> {
    session
        .open(path, &language.name, indexed_source.to_owned())
        .await?;
    let position = positions.negotiated(session.capabilities().position_encoding);
    let locations = session.references(path, position).await?;
    session.close(path).await?;
    Ok(locations)
}

/// The engine's answered locations, resolved to a checked-clean or checked-found verdict.
fn reference_check_from_locations(tree_root: &TreeRoot, locations: &[Location]) -> ReferenceCheck {
    if locations.is_empty() {
        return ReferenceCheck::Clean;
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
fn is_blank_content(content: &str) -> bool {
    content.bytes().all(|byte| byte == b' ' || byte == b'\t')
}

/// Widens `span` over the whitespace-only bytes and separator that isolate a removed
/// declaration or node, so the removal leaves no dangling indentation and no blank-line run
/// where it stood.
///
/// The rule, in order:
/// 1. If every byte between the line start and `span.start` is blank, the widened span
///    starts at the line start: the removal takes its own leading indentation with it.
/// 2. If every byte between `span.end` and the end of its line is blank, the widened span
///    extends past that line's ending, then past every further wholly blank line: the
///    removal takes the separator that followed it, and the blank run beyond, with it.
/// 3. When step 2 widened the span all the way to the end of `source`, the widened span
///    also retreats past every wholly blank line immediately before its start, so a removed
///    trailing declaration leaves no blank run before the file's new end either.
///
/// A span that sits mid-line - its start preceded by more than indentation, or its end
/// followed by more than blanks - is returned unchanged.
pub(crate) fn widened_removal_span(source: &str, span: ByteRange) -> ByteRange {
    let source_len = source.len();
    let mut start = usize::try_from(span.start)
        .unwrap_or(source_len)
        .min(source_len);
    let mut end = usize::try_from(span.end)
        .unwrap_or(source_len)
        .min(source_len);
    let spans = line_spans(source);

    if let Some(line) = line_at(&spans, start) {
        let prefix = source.get(line.start..start).unwrap_or_default();
        if is_blank_content(prefix) {
            start = line.start;
        }
    }

    let mut widened_over_separator = false;
    if let Some(line) = line_at(&spans, end) {
        let suffix = source.get(end..line.content_end).unwrap_or_default();
        if is_blank_content(suffix) {
            end = line.end;
            widened_over_separator = true;
            while let Some(next) = line_at(&spans, end) {
                let content = source.get(next.start..next.content_end).unwrap_or_default();
                if !is_blank_content(content) {
                    break;
                }
                end = next.end;
            }
        }
    }

    if widened_over_separator && end == source_len {
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

    ByteRange {
        start: start as u64,
        end: end as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
