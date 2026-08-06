"""The typed MCP surface and JSON Schema document."""

from . import core, mcp
from .surface import Axis, Document, Resource, Rpc, Service, Tool, ToolGroup

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

RPC_EDIT = Rpc(
    name="Edit",
    request=mcp.EditParams,
    response=mcp.CandidateResult,
    description=(
        "Builds a candidate from an atomic set of concrete edits. Nothing is published: the "
        "candidate is an immutable commit that `publish` can advance a ref to."
    ),
)

RPC_PATCH = Rpc(
    name="Patch",
    request=mcp.PatchParams,
    response=mcp.CandidateResult,
    description=(
        "Builds a candidate from a unified diff. Rift checks every hunk's context against the "
        "state it resolves against. A patch written for older bytes is refused before any hunk "
        "is applied."
    ),
)

RPC_REWRITE = Rpc(
    name="Rewrite",
    request=mcp.RewriteParams,
    response=mcp.CandidateResult,
    description=(
        "Builds a candidate by replacing every match of one query. The cardinality is checked "
        "before expansion, so a pattern that matches more places than intended refuses instead "
        "of rewriting them."
    ),
)

RPC_REVERT = Rpc(
    name="Revert",
    request=mcp.RevertParams,
    response=mcp.CandidateResult,
    description=(
        "Builds a candidate from the three-way inverse of one commit against a selected parent."
    ),
)

RPC_MERGE = Rpc(
    name="Merge",
    request=mcp.MergeParams,
    response=mcp.CandidateResult,
    description="Builds a candidate by merging one exact commit into the candidate state.",
)

RPC_RENAME = Rpc(
    name="Rename",
    request=mcp.RenameParams,
    response=mcp.CandidateResult,
    description=(
        "Changes what a declaration is called and rewrites every reference that names it. The "
        "adapter checks spelling, collisions, and binding changes. A reference outside the "
        "scope refuses the whole operation."
    ),
)

RPC_MOVE = Rpc(
    name="Move",
    request=mcp.MoveParams,
    response=mcp.CandidateResult,
    description=(
        "Moves a declaration or file to another container or path and updates the imports and "
        "references that reach it."
    ),
)

RPC_DELETE = Rpc(
    name="Delete",
    request=mcp.DeleteParams,
    response=mcp.CandidateResult,
    description=(
        "Removes a declaration. Without a policy this is a mechanical removal that analyses no "
        "references. With one, the adapter classifies every remaining use, applies the stated "
        "disposition, and refuses when reference coverage is incomplete."
    ),
)

RPC_CHANGE_SIGNATURE = Rpc(
    name="ChangeSignature",
    request=mcp.ChangeSignatureParams,
    response=mcp.CandidateResult,
    description=(
        "Changes the shape of a callable and propagates it to callers, overrides, and "
        "implementations. Unlike a rename, it rewrites argument lists, so it commonly raises a "
        "`behavior_unknown` confirmation."
    ),
)

RPC_ACT = Rpc(
    name="Act",
    request=mcp.ActParams,
    response=mcp.CandidateResult,
    description=(
        "Resolves one action an adapter offered — a quick fix, an extraction, an inline, or an "
        "adapter-specific family. Arguments are validated against the schema the offer "
        "advertises. A portable family is refused here and resolves through its own tool."
    ),
)

RPC_INTEGRATE = Rpc(
    name="Integrate",
    request=mcp.IntegrateParams,
    response=mcp.IntegrateResult,
    description=(
        "Merges the connection's accepted commit into a local branch and validates the result. "
        "Two commits that are each valid can merge into a broken tree, so the merge candidate "
        "is validated like any other. A conflict returns a parseable provisional merge."
    ),
)

RPC_REFRESH = Rpc(
    name="Refresh",
    request=mcp.RefreshParams,
    response=mcp.RefreshResult,
    description=(
        "Reruns a retained candidate's operation against a newer base and reports how much the "
        "result moved. The candidate holds the request it was built from, so nothing has to be "
        "restated."
    ),
)

RPC_PUBLISH = Rpc(
    name="Publish",
    request=mcp.PublishParams,
    response=mcp.PublishResult,
    description=(
        "Runs the declared command validators against a retained candidate and advances its "
        "destination by compare-and-swap: the accepted ref for an ordinary candidate, the "
        "target branch for an integration. A retry that finds the destination already holding "
        "the candidate returns the same result."
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
        RPC_EXECUTE,
        RPC_DEBUG_START,
        RPC_DEBUG_GET_FRAME,
        RPC_DEBUG_STOP,
        RPC_EDIT,
        RPC_PATCH,
        RPC_REWRITE,
        RPC_REVERT,
        RPC_MERGE,
        RPC_RENAME,
        RPC_MOVE,
        RPC_DELETE,
        RPC_CHANGE_SIGNATURE,
        RPC_ACT,
        RPC_INTEGRATE,
        RPC_REFRESH,
        RPC_PUBLISH,
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
    tool_groups=(
        ToolGroup(
            name="discovery",
            title="Discovery",
            summary=(
                "Find code and read what an adapter knows about it. Every answer carries the "
                "snapshot it resolved against and how much of the source it covered, so a "
                "later call can ask about the same state."
            ),
        ),
        ToolGroup(
            name="changes",
            title="Changes",
            summary=(
                "Build a candidate. Each tool resolves one operation against a base or against "
                "the candidate named by `on`, writes an immutable commit, and validates it. "
                "Nothing reaches a ref or the connection worktree until publication."
            ),
        ),
        ToolGroup(
            name="lifecycle",
            title="Lifecycle",
            summary=(
                "Continue a retained candidate. `refresh` reruns it on a newer base. `publish` "
                "advances a ref after replay and validation, while `persist` copies accepted "
                "paths into the connection worktree."
            ),
        ),
        ToolGroup(
            name="execution",
            title="Execution",
            summary=(
                "Evaluate caller-provided code against an exact snapshot in a disposable "
                "workspace, and inspect the frames a failed evaluation left behind. Host policy "
                "authorizes these per language before they are advertised at all."
            ),
        ),
    ),
    tools=(
        Tool(name="tree", rpc=RPC_TREE, group="discovery"),
        Tool(name="outline", rpc=RPC_OUTLINE, group="discovery"),
        Tool(name="search", rpc=RPC_SEARCH, group="discovery"),
        Tool(name="match", rpc=RPC_MATCH, group="discovery"),
        Tool(name="edit", rpc=RPC_EDIT, group="changes"),
        Tool(name="patch", rpc=RPC_PATCH, group="changes"),
        Tool(name="rewrite", rpc=RPC_REWRITE, group="changes"),
        Tool(name="revert", rpc=RPC_REVERT, group="changes"),
        Tool(name="merge", rpc=RPC_MERGE, group="changes"),
        Tool(name="rename", rpc=RPC_RENAME, group="changes"),
        Tool(name="move", rpc=RPC_MOVE, group="changes"),
        Tool(name="delete", rpc=RPC_DELETE, group="changes"),
        Tool(name="change_signature", rpc=RPC_CHANGE_SIGNATURE, group="changes"),
        Tool(name="act", rpc=RPC_ACT, group="changes"),
        Tool(name="integrate", rpc=RPC_INTEGRATE, group="changes"),
        Tool(name="refresh", rpc=RPC_REFRESH, group="lifecycle"),
        Tool(name="publish", rpc=RPC_PUBLISH, group="lifecycle"),
        Tool(name="persist", rpc=RPC_PERSIST, group="lifecycle"),
        Tool(name="execute", rpc=RPC_EXECUTE, group="execution"),
        Tool(name="debug_start", rpc=RPC_DEBUG_START, group="execution"),
        Tool(name="debug_get_frame", rpc=RPC_DEBUG_GET_FRAME, group="execution"),
        Tool(name="debug_stop", rpc=RPC_DEBUG_STOP, group="execution"),
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
                "Returns the retained plan behind one candidate, including the request it was "
                "built from, its concrete edits, candidate diff, adapter and validator evidence, "
                "and confirmations. Its `parent` reads back the chain the candidate belongs to."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.PreviewResourceUri,
            link=mcp.PreviewResourceLink,
        ),
        Resource(
            name="actions",
            description=(
                "Returns the fixes and refactors an adapter offers at one address, or across one "
                "file. Each entry carries the offer identity and what the action would do, and "
                "leaves out the argument schema so a page stays bounded however many offers it "
                "holds."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.ActionsResourceUri,
            link=mcp.ActionsResourceLink,
        ),
        Resource(
            name="action",
            description=(
                "Returns one discovered action with the JSON Schema of the arguments it takes. "
                "This is the read a caller makes for the offer it chose, before handing the same "
                "identity to `act`."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.ActionResourceUri,
            link=mcp.ActionResourceLink,
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
                core.CapturedText,
            ),
        ),
        Axis(
            name="Operations",
            summary="Addresses, discovered actions, resolved changes, and evidence tied to a snapshot.",
            identified_by=(
                core.Address,
                core.ActionDescriptor,
                core.ActionKind,
                core.MatchId,
                core.ActionOfferId,
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
                mcp.PreviewOperation,
                mcp.EditParams,
                mcp.PatchParams,
                mcp.RewriteParams,
                mcp.RevertParams,
                mcp.MergeParams,
                mcp.RenameParams,
                mcp.MoveParams,
                mcp.DeleteParams,
                mcp.ChangeSignatureParams,
                mcp.ActParams,
                mcp.ResolvedOperation,
                mcp.RefusalReason,
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
                core.CapturedText,
                core.ExecutionBudget,
                core.DebugBudget,
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
