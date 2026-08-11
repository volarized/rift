"""The typed MCP surface and JSON Schema document."""

from . import core, mcp
from .surface import Axis, Document, Resource, Rpc, Service, Tool, ToolGroup

RPC_CONNECT = Rpc(
    name="Connect",
    request=mcp.ConnectRequest,
    response=mcp.Connected,
    response_stream=True,
    description=(
        "Opens one client session and returns its writable projection. A session ID admits one "
        "live connection."
    ),
)

RPC_SESSION_LIST = Rpc(
    name="SessionList",
    request=mcp.SessionListParams,
    response=mcp.SessionListResult,
    description=("Lists retained sessions and their projection state."),
)

RPC_SESSION_CONTINUE = Rpc(
    name="SessionContinue",
    request=mcp.SessionContinueParams,
    response=mcp.SessionContinueResult,
    description=("Rebinds the current connection to one retained session."),
)

RPC_SESSION_REMOVE = Rpc(
    name="SessionRemove",
    request=mcp.SessionRemoveParams,
    response=mcp.SessionRemoveResult,
    description=("Removes one inactive session and deletes its projection."),
)

RPC_OUTLINE = Rpc(
    name="Outline",
    request=mcp.OutlineParams,
    response=mcp.OutlineResult,
    description=("Reads the declaration structure of one file from its adapter."),
)

RPC_SEARCH = Rpc(
    name="Search",
    request=mcp.SearchParams,
    response=mcp.SearchResult,
    description=("Searches symbols, nodes, and files in the session projection."),
)

RPC_MATCH = Rpc(
    name="Match",
    request=mcp.MatchParams,
    response=mcp.MatchResult,
    description=("Finds literal, regular-expression, or structural matches."),
)

RPC_EXECUTE = Rpc(
    name="Execute",
    request=mcp.ExecuteParams,
    response=mcp.ExecuteResult,
    description=(
        "Evaluates code against the session projection in a disposable workspace."
    ),
)

RPC_DEBUG_START = Rpc(
    name="DebugStart",
    request=mcp.DebugStartParams,
    response=mcp.DebugSession,
    description=(
        "Starts an inspect-only evaluation and retains bounded frames after a failure."
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
        "Applies and validates an atomic edit set, and records the result in the changeset."
    ),
)

RPC_PATCH = Rpc(
    name="Patch",
    request=mcp.PatchParams,
    response=mcp.ChangeResult,
    description=("Applies a unified diff, and records the result in the changeset."),
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
        "advertises. An offer carrying a portable argument contract is refused here and "
        "resolves through its own tool."
    ),
)

RPC_PROJECTION_RESTORE = Rpc(
    name="ProjectionRestore",
    request=mcp.ProjectionRestoreParams,
    response=core.ProjectionState,
    description=(
        "Restores changed paths from the workspace and drops the changes that touched them."
    ),
)

RPC_PUBLISH = Rpc(
    name="Publish",
    request=mcp.PublishParams,
    response=mcp.PublishResult,
    description=(
        "Publishes the changeset into the workspace. A change carrying a confirmation is "
        "published only when the call accepts it, and a workspace path changed since the "
        "projection took it refuses the whole publication."
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
        RPC_CONNECT,
        RPC_SESSION_LIST,
        RPC_SESSION_CONTINUE,
        RPC_SESSION_REMOVE,
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
        RPC_RENAME,
        RPC_MOVE,
        RPC_DELETE,
        RPC_CHANGE_SIGNATURE,
        RPC_ACT,
        RPC_PROJECTION_RESTORE,
        RPC_PUBLISH,
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
            summary="List, continue, or remove sessions.",
        ),
        ToolGroup(
            name="discovery",
            title="Discovery",
            summary=("Find code and read semantic information from adapters."),
        ),
        ToolGroup(
            name="changes",
            title="Changes",
            summary=(
                "Change a projection, discard its changes, or publish them to the workspace."
            ),
        ),
        ToolGroup(
            name="execution",
            title="Execution",
            summary=(
                "Evaluate code in a disposable workspace and inspect failed evaluations."
            ),
        ),
    ),
    tools=(
        Tool(name="session_list", rpc=RPC_SESSION_LIST, group="sessions"),
        Tool(name="session_continue", rpc=RPC_SESSION_CONTINUE, group="sessions"),
        Tool(name="session_remove", rpc=RPC_SESSION_REMOVE, group="sessions"),
        Tool(name="outline", rpc=RPC_OUTLINE, group="discovery"),
        Tool(name="search", rpc=RPC_SEARCH, group="discovery"),
        Tool(name="match", rpc=RPC_MATCH, group="discovery"),
        Tool(name="edit", rpc=RPC_EDIT, group="changes"),
        Tool(name="patch", rpc=RPC_PATCH, group="changes"),
        Tool(name="rewrite", rpc=RPC_REWRITE, group="changes"),
        Tool(name="rename", rpc=RPC_RENAME, group="changes"),
        Tool(name="move", rpc=RPC_MOVE, group="changes"),
        Tool(name="delete", rpc=RPC_DELETE, group="changes"),
        Tool(name="change_signature", rpc=RPC_CHANGE_SIGNATURE, group="changes"),
        Tool(name="act", rpc=RPC_ACT, group="changes"),
        Tool(name="projection_restore", rpc=RPC_PROJECTION_RESTORE, group="changes"),
        Tool(name="publish", rpc=RPC_PUBLISH, group="changes"),
        Tool(name="execute", rpc=RPC_EXECUTE, group="execution"),
        Tool(name="debug_start", rpc=RPC_DEBUG_START, group="execution"),
        Tool(name="debug_get_frame", rpc=RPC_DEBUG_GET_FRAME, group="execution"),
        Tool(name="debug_stop", rpc=RPC_DEBUG_STOP, group="execution"),
    ),
    resources=(
        Resource(
            name="workspace",
            description=(
                "Reports projection state, adapter availability, limits, and validators."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.WorkspaceResourceUri,
            link=mcp.ResourceLink,
        ),
        Resource(
            name="projection",
            description=(
                "Returns the filesystem path of this session's projection, for tools that can "
                "only work through a directory."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.ProjectionResourceUri,
            link=mcp.ResourceLink,
        ),
        Resource(
            name="changes",
            description=(
                "Returns the changes this session has applied, each with its edits, effects, "
                "validation evidence, and the confirmations publication will check."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.ChangesResourceUri,
            link=mcp.ResourceLink,
        ),
        Resource(
            name="symbol",
            description=(
                "Returns one symbol and its semantic facts from the session projection."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.SymbolResourceUri,
            link=mcp.ResourceLink,
        ),
        Resource(
            name="fs",
            description=(
                "Returns one page of a directory's entries — subdirectories and files — or a "
                "bounded file-content range from the session projection."
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
            link=mcp.ResourceLink,
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
            link=mcp.ResourceLink,
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
            name="Changeset",
            summary=(
                "What a session has changed in its projection, and what vouched for each change."
            ),
            identified_by=(
                core.ProjectionState,
                mcp.ChangeId,
                mcp.ChangeSummary,
                mcp.ChangesResourcePayload,
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
                core.CapturedText,
            ),
        ),
        Axis(
            name="Operations",
            summary="Addresses, actions, changes, and their validation evidence.",
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
                mcp.RenameParams,
                mcp.MoveParams,
                mcp.DeleteParams,
                mcp.ChangeSignatureParams,
                mcp.ActParams,
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
