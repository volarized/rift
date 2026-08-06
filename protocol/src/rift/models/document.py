"""The typed MCP surface and JSON Schema document."""

from . import core, mcp
from .surface import Axis, Document, Resource, Rpc, Service, Tool

RPC_TREE = Rpc(
    name="Tree",
    request=mcp.TreeParams,
    response=mcp.TreeResult,
    description=(
        "Lists the project tree at one snapshot. Rift derives directories from Git paths "
        "and answers without starting a language adapter. A depth of one is an `ls`; an "
        "unbounded depth with path selectors is a recursive tree, glob, or file find."
    ),
)

RPC_OUTLINE = Rpc(
    name="Outline",
    request=mcp.OutlineParams,
    response=mcp.OutlineResult,
    description=(
        "Reads the adapter-owned declaration structure of one file. Results preserve "
        "source nesting and carry semantic coverage, so an empty outline distinguishes an "
        "empty file from an unsupported language."
    ),
)

RPC_SEARCH = Rpc(
    name="Search",
    request=mcp.SearchParams,
    response=mcp.SearchResult,
    description=(
        "Searches symbols, nodes and files in a stable ranked order. A lexical query finds names or text; "
        "a structured filter walks adapter facts. Path selectors narrow either form. "
        "Every page carries its total order, continuation cursor, and coverage."
    ),
)

RPC_MATCH = Rpc(
    name="Match",
    request=mcp.MatchParams,
    response=mcp.MatchResult,
    description=(
        "Finds byte ranges. Rift runs literal and regular-expression matching over every "
        "UTF-8 file. The owning adapter parses structural patterns because it defines the "
        "language's syntax. Every hit carries a key an edit can address."
    ),
)

RPC_ACTIONS = Rpc(
    name="Actions",
    request=mcp.ActionsParams,
    response=mcp.ActionsResult,
    description=(
        "Asks an adapter what it can offer at one address: the fixes and refactors it "
        "would suggest there. Each result carries the snapshot and adapter token needed "
        "for resolution. Rift refuses a token after its snapshot moves."
    ),
)

RPC_EXECUTE = Rpc(
    name="Execute",
    request=mcp.ExecuteParams,
    response=mcp.ExecuteResult,
    description=(
        "Evaluates a caller-provided code block with one configured language adapter against "
        "an exact snapshot. Rift prepares a disposable execution workspace and discards every "
        "filesystem write made by the evaluated code."
    ),
)

RPC_DEBUG_START = Rpc(
    name="DebugStart",
    request=mcp.DebugStartParams,
    response=mcp.DebugSession,
    description=(
        "Starts an inspect-only debugging evaluation. An unhandled failure retains bounded "
        "stack frames until the caller stops the connection-bound session."
    ),
)

RPC_DEBUG_GET_FRAME = Rpc(
    name="DebugGetFrame",
    request=mcp.DebugGetFrameParams,
    response=mcp.DebugGetFrameResult,
    description="Reads one retained stack frame without resuming evaluation.",
)

RPC_DEBUG_STOP = Rpc(
    name="DebugStop",
    request=mcp.DebugStopParams,
    response=mcp.DebugStopResult,
    description="Releases a debugging session, adapter state, and execution workspace.",
)

RPC_APPLY = Rpc(
    name="Apply",
    request=mcp.ApplyParams,
    response=mcp.ApplyResult,
    description=(
        "Builds, refreshes, or publishes a deterministic candidate. Preview retains each "
        "request with its checked preconditions, exact edits, semantic effects, guarantee "
        "evidence, diff, validation, and confirmations. Refresh repeats that contract on "
        "a newer base. Publish replays it and advances the accepted ref when the retained "
        "plan still agrees, fresh validation passes, and the accepted head still matches."
    ),
)

RPC_INTEGRATE = Rpc(
    name="Integrate",
    request=mcp.IntegrateParams,
    response=mcp.IntegrateResult,
    description=(
        "Previews, refreshes, or publishes a merge from the connection's accepted commit "
        "into one local target branch. Publication replays validation and "
        "compare-and-swaps the target head."
    ),
)

RPC_PERSIST = Rpc(
    name="Persist",
    request=mcp.PersistParams,
    response=mcp.PersistResult,
    description=(
        "Materializes an accepted commit into the connection worktree. Each requested "
        "path reports its own outcome, so drift, sparse checkout rules, nested "
        "repositories, and deletion policy remain visible to the caller."
    ),
)

RPC_READRESOURCE = Rpc(
    name="ReadResource",
    request=mcp.ResourceReadParams,
    response=mcp.ResourceReadResult,
)

RIFT_SERVICE = Service(
    name="Rift",
    description="The Protobuf service exposed by the Rift server. `rift mcp` maps its JSON entry points to these methods.",
    rpcs=(
        RPC_TREE,
        RPC_OUTLINE,
        RPC_SEARCH,
        RPC_MATCH,
        RPC_ACTIONS,
        RPC_EXECUTE,
        RPC_DEBUG_START,
        RPC_DEBUG_GET_FRAME,
        RPC_DEBUG_STOP,
        RPC_APPLY,
        RPC_INTEGRATE,
        RPC_PERSIST,
        RPC_READRESOURCE,
    ),
)

DOCUMENT = Document(
    schema="https://json-schema.org/draft/2020-12/schema",
    id="https://volar.sh/rift/protocol/mcp.json",
    title="Rift MCP surface",
    description=(
        "JSON values accepted and returned by the Rift MCP server. Each definition "
        "carries the Protobuf identity used after `rift mcp` decodes the request."
    ),
    entry_points_description=(
        "Tools map MCP request and result JSON to Rift service methods. Resources map each "
        "URI family to its template, link, and payload types. Every other schema definition "
        "is reachable from one of these entries."
    ),
    service=RIFT_SERVICE,
    tools=(
        Tool(name="tree", rpc=RPC_TREE),
        Tool(name="outline", rpc=RPC_OUTLINE),
        Tool(name="search", rpc=RPC_SEARCH),
        Tool(name="match", rpc=RPC_MATCH),
        Tool(name="actions", rpc=RPC_ACTIONS),
        Tool(name="execute", rpc=RPC_EXECUTE),
        Tool(name="debug_start", rpc=RPC_DEBUG_START),
        Tool(name="debug_get_frame", rpc=RPC_DEBUG_GET_FRAME),
        Tool(name="debug_stop", rpc=RPC_DEBUG_STOP),
        Tool(name="apply", rpc=RPC_APPLY),
        Tool(name="integrate", rpc=RPC_INTEGRATE),
        Tool(name="persist", rpc=RPC_PERSIST),
    ),
    resources=(
        Resource(
            name="repository",
            description=(
                "Reports the tools and resources this workspace serves, which languages have "
                "adapters, the state answers resolve against, request limits, retention, and the "
                "active conformance profile. Clients read it before calling another entry point."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.RepositoryResourceUri,
            link=mcp.ResourceLink,
        ),
        Resource(
            name="symbol",
            description=(
                "Returns one symbol at one snapshot with its declaration, source nodes, types, "
                "relationships, diagnostics, and coverage. The nodes and relationships identify "
                "the references a rename or deletion has to handle."
            ),
            template=mcp.ResourceTemplate,
            uri=core.SymbolId,
            link=mcp.ResourceLink,
        ),
        Resource(
            name="diff",
            description=(
                "What changed between two revisions, a page of files at a time: the entry on each "
                "side and the edits between them. Git answers it without an adapter, so it works "
                "for a language Rift has no adapter for."
            ),
            template=mcp.ResourceTemplate,
            uri=core.DiffId,
            link=mcp.ResourceLink,
        ),
        Resource(
            name="file",
            description=(
                "One file at one revision: the entry Git records, its language ownership, and one "
                "bounded content range. The next URI continues a large regular file."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.FileResourceUri,
            link=mcp.ResourceLink,
        ),
        Resource(
            name="preview",
            description=(
                "Returns the retained plan behind an apply preview, including its input changes, "
                "concrete edits, candidate diff, adapter and validator evidence, and confirmations. "
                "The tool result links here when the plan does not fit in its bounded summary."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.PreviewResourceUri,
            link=mcp.PreviewResourceLink,
        ),
    ),
    resource_read=RPC_READRESOURCE,
    error=mcp.ErrorData,
    axes_description=(
        "Each axis owns a set of definitions. `identifiedBy` supplies its roots, then the group "
        "closes over references without taking a definition already assigned to another axis. "
        "`holds` assigns explicit definitions and `residualOf` assigns the unclaimed definitions "
        "from one schema document."
    ),
    axes=(
        Axis(
            name="Versioning",
            summary="Revision selectors, resolved snapshots, commits, worktrees, diffs, and retained previews.",
            identified_by=(
                core.Revision,
                core.Snapshot,
                core.Commit,
                core.Worktree,
                core.DiffId,
                core.PreviewId,
            ),
        ),
        Axis(
            name="Filesystem",
            summary=(
                "Project paths, files, syntax-tree nodes, byte ranges, and proposed source edits."
            ),
            identified_by=(
                core.FileId,
                core.ProjectPath,
                core.File,
                core.ProjectEntry,
                core.Node,
                core.NodeId,
                core.TextRange,
                core.SourceSpan,
                core.Edit,
                core.TextEdit,
                core.NodeFacet,
                core.NodeRegion,
                core.RegionRole,
                core.FileChange,
                core.OriginMapping,
                mcp.PersistOutcome,
                core.Capture,
                core.StructuralMatchRanges,
                core.LanguageRegion,
            ),
        ),
        Axis(
            name="Semantic",
            summary="Adapter-resolved symbols, their types and documentation, and relationships between them.",
            identified_by=(
                core.SymbolId,
                core.Symbol,
                core.Relationship,
                core.Signature,
                core.TypeExpression,
                core.Documentation,
                core.ExactKind,
                core.SymbolFacet,
                core.RelationshipFacet,
                core.TypeBinding,
                core.SymbolOrigin,
                core.Language,
            ),
        ),
        Axis(
            name="Discovery",
            summary="Filters, path selectors, and text or structural queries used to find code.",
            identified_by=(
                core.Filter,
                core.FieldFilter,
                core.RelationFilter,
                core.ElementFilter,
                core.PathSelector,
                core.PathPattern,
                core.MatchQuery,
                core.TextQuery,
                core.StructuralQuery,
                core.StructuralCaptureConstraint,
                core.CaptureName,
            ),
        ),
        Axis(
            name="Reachability",
            summary="Coverage, diagnostics, validation reports, and validator output for returned facts.",
            identified_by=(
                core.Coverage,
                core.CoverageScope,
                core.SemanticCoverage,
                core.FactFamily,
                core.Diagnostic,
                core.DiagnosticRelated,
                core.Severity,
                core.ValidationReport,
                mcp.CandidateValidation,
                mcp.ValidatorResult,
                mcp.ValidatorOutput,
            ),
        ),
        Axis(
            name="Operations",
            summary="Addresses, discovered actions, resolved changes, and evidence tied to a snapshot.",
            identified_by=(
                core.Address,
                core.ActionDescriptor,
                core.ActionKind,
                core.MatchKey,
                core.ActionKey,
                core.FormattingPolicy,
                core.MatchCardinality,
                core.OperationScope,
                core.SafeDeletePolicy,
                core.SignatureChange,
                core.RenameArguments,
                core.MoveArguments,
                core.SafeDeleteArguments,
                core.ChangeSignatureArguments,
                core.ArgumentContract,
                core.OperationVerifier,
                core.PreconditionValue,
                core.OperationPrecondition,
                core.OperationBlocker,
                core.OperationEffect,
                core.GuaranteeKind,
                core.GuaranteeEvidence,
                core.ConfirmationRequirement,
                mcp.Change,
                mcp.DirectChange,
                mcp.PatchChange,
                mcp.ActionChange,
                mcp.RewriteChange,
                mcp.RevertChange,
                mcp.ResolvedChange,
                core.ActionSupport,
                mcp.CommandValidator,
            ),
        ),
        Axis(
            name="Execution",
            summary=(
                "Caller-provided code evaluation, bounded process output, and connection-bound "
                "post-mortem debugging frames."
            ),
            identified_by=(
                core.CodeBlock,
                core.ExecutionResult,
                core.ExecutionStatus,
                core.CapturedOutput,
                core.DebugFrame,
                core.DebugBinding,
                mcp.ExecuteParams,
                mcp.ExecuteResult,
                mcp.DebugSessionId,
                mcp.DebugSession,
                mcp.DebugStartParams,
                mcp.DebugGetFrameParams,
                mcp.DebugGetFrameResult,
                mcp.DebugStopParams,
                mcp.DebugStopResult,
            ),
        ),
        Axis(
            name="Protocol",
            summary="Protocol versions, content digests, and extension namespaces shared across seams.",
            holds=(
                core.ProtocolVersion,
                core.Digest,
                core.Extensions,
                core.ExtensionKey,
                core.ExtensionValue,
            ),
            residual_of="core",
        ),
        Axis(
            name="MCP",
            summary="Tool parameters, results, resource links, and payloads on the agent-facing surface.",
            residual_of="mcp",
        ),
    ),
)

__all__ = ["DOCUMENT", "RIFT_SERVICE"]
