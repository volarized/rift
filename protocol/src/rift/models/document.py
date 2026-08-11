"""The typed MCP surface and JSON Schema document."""

from . import core, mcp
from .surface import Axis, Document, Resource, Rpc, Service, Tool, ToolGroup

RPC_CONNECT = Rpc(
    name="Connect",
    request=mcp.ConnectRequest,
    response=mcp.Connected,
    response_stream=True,
    description=(
        "Opens the control stream that owns one logical client connection. For an MCP role, the "
        "first response returns the always-mounted persistent session "
        "projection. A session ID admits one live connection."
    ),
)

RPC_SESSION_LIST = Rpc(
    name="SessionList",
    request=mcp.SessionListParams,
    response=mcp.SessionListResult,
    description=(
        "Lists retained sessions for this workspace. Each entry reports its projection path, "
        "exact current state and base, and live owner."
    ),
)

RPC_SESSION_CONTINUE = Rpc(
    name="SessionContinue",
    request=mcp.SessionContinueParams,
    response=mcp.SessionContinueResult,
    description=(
        "Rebinds the current MCP connection to one retained session. The connection's initial "
        "session must still be unchanged; Rift then removes it and continues the selected "
        "projection. The MCP process remembers the selected ID for reconnects. "
        "A changed initial session returns `invalid_request`; an active session or retained "
        "debugger returns `temporarily_unavailable`."
    ),
)

RPC_SESSION_REMOVE = Rpc(
    name="SessionRemove",
    request=mcp.SessionRemoveParams,
    response=mcp.SessionRemoveResult,
    description=(
        "Previews or confirms removal of one inactive session. Confirmation removes its "
        "projection and compares the exact observed projection state. "
        "An active session returns `temporarily_unavailable`; open handles and in-flight "
        "filesystem mutations return `projection_busy`."
    ),
)

RPC_TREE = Rpc(
    name="Tree",
    request=mcp.TreeParams,
    response=mcp.TreeResult,
    description=(
        "Lists the project tree at one snapshot. Rift derives directories from source paths "
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
    response=mcp.ChangeResult,
    description=(
        "Checks the exact projection token, applies an atomic edit set in a private candidate, "
        "validates it, and conditionally swaps the public "
        "projection state."
    ),
)

RPC_PATCH = Rpc(
    name="Patch",
    request=mcp.PatchParams,
    response=mcp.ChangeResult,
    description=(
        "Checks the exact projection token, then applies a unified diff in a private candidate "
        "and checks every hunk's context."
    ),
)

RPC_REWRITE = Rpc(
    name="Rewrite",
    request=mcp.RewriteParams,
    response=mcp.ChangeResult,
    description=(
        "Applies replacements for every match of one query. The cardinality is checked "
        "before expansion, so a pattern that matches more places than intended refuses instead "
        "of rewriting them."
    ),
)

RPC_REVERT = Rpc(
    name="Revert",
    request=mcp.RevertParams,
    response=mcp.ChangeResult,
    description=(
        "Applies the three-way inverse of one commit against a selected parent."
    ),
)

RPC_RENAME = Rpc(
    name="Rename",
    request=mcp.RenameParams,
    response=mcp.ChangeResult,
    description=(
        "Changes what a declaration is called and rewrites every reference that names it. The "
        "adapter checks spelling, collisions, and binding changes. A reference outside the "
        "scope refuses the whole operation."
    ),
)

RPC_MOVE = Rpc(
    name="Move",
    request=mcp.MoveParams,
    response=mcp.ChangeResult,
    description=(
        "Moves a declaration or file to another container or path and updates the imports and "
        "references that reach it."
    ),
)

RPC_DELETE = Rpc(
    name="Delete",
    request=mcp.DeleteParams,
    response=mcp.ChangeResult,
    description=(
        "Removes a declaration. Without a policy this is a mechanical removal that analyses no "
        "references. With one, the adapter classifies every remaining use, applies the stated "
        "disposition, and refuses when reference coverage is incomplete."
    ),
)

RPC_CHANGE_SIGNATURE = Rpc(
    name="ChangeSignature",
    request=mcp.ChangeSignatureParams,
    response=mcp.ChangeResult,
    description=(
        "Changes the shape of a callable and propagates it to callers, overrides, and "
        "implementations. Unlike a rename, it rewrites argument lists, so it commonly raises a "
        "`behavior_unknown` confirmation."
    ),
)

RPC_ACT = Rpc(
    name="Act",
    request=mcp.ActParams,
    response=mcp.ChangeResult,
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
        "Merges the exact current projection from its retained base onto a guarded target, "
        "validates the merged snapshot, writes at most one squash commit, and conditionally "
        "advances the target. A conflict creates an exceptional conventional Git recovery worktree."
    ),
)

RPC_PROJECTION_OPEN = Rpc(
    name="ProjectionOpen",
    request=mcp.ProjectionOpenParams,
    response=mcp.ProjectionOpenResult,
    description="Mounts one exact revision as an explicitly pinned read-only filesystem projection.",
)

RPC_PROJECTION_CLOSE = Rpc(
    name="ProjectionClose",
    request=mcp.ProjectionCloseParams,
    response=mcp.ProjectionCloseResult,
    description="Withdraws an explicit read projection; open handles keep only their inode pins.",
)

RPC_PROJECTION_RESTORE = Rpc(
    name="ProjectionRestore",
    request=mcp.ProjectionRestoreParams,
    response=mcp.ProjectionRestoreResult,
    description="Discards reviewed unintegrated source changes and restores the pinned session base.",
)

RPC_RECOVERY_LIST = Rpc(
    name="RecoveryList",
    request=mcp.RecoveryListParams,
    response=mcp.RecoveryListResult,
    description="Lists durable unresolved integrations and rescans their exact recovery manifests.",
)

RPC_RECOVERY_CONTINUE = Rpc(
    name="RecoveryContinue",
    request=mcp.RecoveryContinueParams,
    response=mcp.RecoveryContinueResult,
    description=(
        "Imports and validates the staged tree of a resolved exceptional Git worktree, then "
        "performs the saved source and target compare-and-swaps."
    ),
)

RPC_RECOVERY_ABORT = Rpc(
    name="RecoveryAbort",
    request=mcp.RecoveryAbortParams,
    response=mcp.RecoveryAbortResult,
    description="Previews or removes a retained Git recovery against its exact manifest.",
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
        RPC_CONNECT,
        RPC_SESSION_LIST,
        RPC_SESSION_CONTINUE,
        RPC_SESSION_REMOVE,
        RPC_PROJECTION_OPEN,
        RPC_PROJECTION_CLOSE,
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
        RPC_RENAME,
        RPC_MOVE,
        RPC_DELETE,
        RPC_CHANGE_SIGNATURE,
        RPC_ACT,
        RPC_PROJECTION_RESTORE,
        RPC_INTEGRATE,
        RPC_RECOVERY_LIST,
        RPC_RECOVERY_CONTINUE,
        RPC_RECOVERY_ABORT,
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
        "The internal connection stream establishes the gRPC context used by later calls. Tools "
        "map MCP request and result JSON to Rift service methods. The MCP process queues those "
        "calls and sends one application RPC at a time. Each resource entry maps one URI family "
        "to the types that carry it."
    ),
    service=RIFT_SERVICE,
    tool_groups=(
        ToolGroup(
            name="sessions",
            title="Sessions",
            summary=(
                "List store-backed sessions, bind the current connection to one persistent "
                "projection, open read projections, or release retained state."
            ),
        ),
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
                "Check an exact projection token, resolve and validate in a private candidate, "
                "then swap the public projection. Git is touched only by integration; unresolved "
                "integration merges use explicit Git recovery."
            ),
        ),
        ToolGroup(
            name="execution",
            title="Execution",
            summary=(
                "Evaluate caller-provided code against an exact snapshot in a disposable "
                "workspace, and inspect the frames a failed evaluation left behind. Each call "
                "checks the current workspace configuration and per-language availability."
            ),
        ),
    ),
    tools=(
        Tool(name="session_list", rpc=RPC_SESSION_LIST, group="sessions"),
        Tool(name="session_continue", rpc=RPC_SESSION_CONTINUE, group="sessions"),
        Tool(name="session_remove", rpc=RPC_SESSION_REMOVE, group="sessions"),
        Tool(name="projection_open", rpc=RPC_PROJECTION_OPEN, group="sessions"),
        Tool(name="projection_close", rpc=RPC_PROJECTION_CLOSE, group="sessions"),
        Tool(name="tree", rpc=RPC_TREE, group="discovery"),
        Tool(name="outline", rpc=RPC_OUTLINE, group="discovery"),
        Tool(name="search", rpc=RPC_SEARCH, group="discovery"),
        Tool(name="match", rpc=RPC_MATCH, group="discovery"),
        Tool(name="edit", rpc=RPC_EDIT, group="changes"),
        Tool(name="patch", rpc=RPC_PATCH, group="changes"),
        Tool(name="rewrite", rpc=RPC_REWRITE, group="changes"),
        Tool(name="revert", rpc=RPC_REVERT, group="changes"),
        Tool(name="rename", rpc=RPC_RENAME, group="changes"),
        Tool(name="move", rpc=RPC_MOVE, group="changes"),
        Tool(name="delete", rpc=RPC_DELETE, group="changes"),
        Tool(name="change_signature", rpc=RPC_CHANGE_SIGNATURE, group="changes"),
        Tool(name="act", rpc=RPC_ACT, group="changes"),
        Tool(name="projection_restore", rpc=RPC_PROJECTION_RESTORE, group="changes"),
        Tool(name="integrate", rpc=RPC_INTEGRATE, group="changes"),
        Tool(name="recovery_list", rpc=RPC_RECOVERY_LIST, group="changes"),
        Tool(name="recovery_continue", rpc=RPC_RECOVERY_CONTINUE, group="changes"),
        Tool(name="recovery_abort", rpc=RPC_RECOVERY_ABORT, group="changes"),
        Tool(name="execute", rpc=RPC_EXECUTE, group="execution"),
        Tool(name="debug_start", rpc=RPC_DEBUG_START, group="execution"),
        Tool(name="debug_get_frame", rpc=RPC_DEBUG_GET_FRAME, group="execution"),
        Tool(name="debug_stop", rpc=RPC_DEBUG_STOP, group="execution"),
    ),
    resources=(
        Resource(
            name="repository",
            description=(
                "Reports current per-language availability, the resource families this workspace "
                "serves, the state answers resolve against, request limits, and configured command "
                "validators. Clients read it before calling another entry point."
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
                "side and the edits between them. The source store answers without an adapter, so it works "
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
            name="fs",
            description=(
                "Lists live Rift-provided filesystem projections, their mounted paths, exact "
                "source snapshots, availability, mutability, scratch usage, and open-handle "
                "counts. Pagination captures one connection-scoped inventory; reading grants no "
                "close authority and retains no projection history."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.FsResourceUri,
            link=mcp.ResourceLink,
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
            summary="Revision selectors, immutable snapshots, projection states, Git commits, and diffs.",
            identified_by=(
                core.GitRevision,
                core.Revision,
                core.SnapshotId,
                core.Snapshot,
                core.ResolvedSnapshot,
                core.ProjectionState,
                core.ProjectionHead,
                core.GitCommit,
                core.Projection,
                core.DiffId,
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
                mcp.ChangeValidation,
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
                mcp.EditParams,
                mcp.PatchParams,
                mcp.RewriteParams,
                mcp.RevertParams,
                mcp.RenameParams,
                mcp.MoveParams,
                mcp.DeleteParams,
                mcp.ChangeSignatureParams,
                mcp.ActParams,
                mcp.ResolvedOperation,
                mcp.RefusalReason,
                core.ActionSupport,
                mcp.CommandValidator,
                mcp.GitConflict,
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
            name="Connection",
            summary=(
                "Control-stream negotiation and the logical identities required before an "
                "application RPC."
            ),
            identified_by=(
                mcp.ConnectRequest,
                mcp.Connected,
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
