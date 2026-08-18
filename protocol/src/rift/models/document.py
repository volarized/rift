"""The typed MCP surface and JSON Schema document."""

from . import core, mcp
from .surface import Axis, Document, Resource, Rpc, Service, Tool, ToolGroup

RPC_GET_SYMBOL = Rpc(
    name="GetSymbol",
    request=mcp.GetSymbolParams,
    response=mcp.GetSymbolResult,
    description=(
        "Gets project, dependency, or standard-library declarations by name. Each hit "
        "includes declaration source when the provider can read it; `include_body: false` "
        "omits it, and `include_history` adds project version-control history. Use `search` "
        "for lexical, filtered, or relationship discovery."
    ),
)

RPC_SEARCH = Rpc(
    name="Search",
    request=mcp.SearchParams,
    response=mcp.SearchResult,
    description=(
        "Searches symbols, nodes, and files by lexical `query`, provider `filter`, or "
        "bounded relationship `traversal`. `scope` selects project, dependency, or all "
        "sources. Use `traversal` for callers, callees, tests, edit ripple, or review "
        "context; use `get_symbol` when the declaration name is known."
    ),
)

RPC_NODES = Rpc(
    name="Nodes",
    request=mcp.NodesParams,
    response=mcp.NodesResult,
    description=(
        "Lists syntax nodes covering one byte position and returns witnessed addresses for "
        "`replace_node`. Use it when the target is smaller than a declaration."
    ),
)

RPC_EXECUTE = Rpc(
    name="Execute",
    request=mcp.ExecuteParams,
    response=mcp.ExecuteResult,
    description=(
        "Evaluates code in the targeted tree's execution copy, where installed "
        "dependencies persist between calls. The server does not synchronize copy writes "
        "back, but the runtime retains the server's OS permissions outside that directory."
    ),
)

RPC_REPLACE_SYMBOL = Rpc(
    name="ReplaceSymbol",
    request=mcp.ReplaceSymbolParams,
    response=mcp.ChangeResult,
    description=(
        "Replaces one declaration addressed by symbol, with no caller-supplied offsets. "
        "The symbol must resolve to project source; dependency, standard-library, external, "
        "and synthetic declarations refuse as unsupported. Use `replace_node` for a smaller "
        "syntax region or `patch` for scattered hunks."
    ),
)

RPC_INSERT_SYMBOL = Rpc(
    name="InsertSymbol",
    request=mcp.InsertSymbolParams,
    response=mcp.ChangeResult,
    description=(
        "Inserts a new declaration immediately before or after an existing one. The parser "
        "decides the exact bytes, so the insertion cannot land inside a neighboring "
        "declaration."
    ),
)

RPC_REPLACE_NODE = Rpc(
    name="ReplaceNode",
    request=mcp.ReplaceNodeParams,
    response=mcp.ChangeResult,
    description=(
        "Replaces one syntax node, addressed by a witnessed identity a `nodes` listing "
        "returned. A stale listing refuses instead of splicing into moved bytes."
    ),
)

RPC_PATCH = Rpc(
    name="Patch",
    request=mcp.PatchParams,
    response=mcp.ChangeResult,
    description=(
        "Applies every hunk of one unified diff atomically. Use it for scattered hunks, "
        "file creation, or changes no symbol or node address expresses."
    ),
)

RPC_PROJECTION_CREATE = Rpc(
    name="ProjectionCreate",
    request=mcp.ProjectionCreateParams,
    response=mcp.Projection,
    description=(
        "Creates a pinned workspace snapshot. Changes targeting its id write the projection "
        "directory until `publish` copies them into the workspace."
    ),
)

RPC_PROJECTION_LIST = Rpc(
    name="ProjectionList",
    request=mcp.ProjectionListParams,
    response=mcp.ProjectionListResult,
    description=("Lists projections and the state of each."),
)

RPC_PROJECTION_REMOVE = Rpc(
    name="ProjectionRemove",
    request=mcp.ProjectionRemoveParams,
    response=mcp.ProjectionRemoveResult,
    description=(
        "Removes one projection and deletes its directory, unpublished changes included."
    ),
)

RPC_PROJECTION_RESTORE = Rpc(
    name="ProjectionRestore",
    request=mcp.ProjectionRestoreParams,
    response=core.ProjectionState,
    description=(
        "Restores a projection's changed paths from the workspace and drops the changes "
        "that touched them."
    ),
)

RPC_PUBLISH = Rpc(
    name="Publish",
    request=mcp.PublishParams,
    response=mcp.PublishResult,
    description=(
        "Reconciles direct filesystem writes, then publishes a projection's changeset into "
        "the workspace. A changed write path, an unaccepted confirmation, or a changed read "
        "dependency refuses the whole publication."
    ),
)

RPC_READRESOURCE = Rpc(
    name="ReadResource",
    request=mcp.ResourceReadParams,
    response=mcp.ResourceReadResult,
)

RIFT_SERVICE = Service(
    name="Rift",
    description=(
        "The operations the Rift server exposes. The server terminates MCP itself, over "
        "Streamable HTTP; `rift mcp` forwards stdio frames to it unchanged, so every method "
        "behaves the same whichever way the call arrived."
    ),
    rpcs=(
        RPC_GET_SYMBOL,
        RPC_SEARCH,
        RPC_NODES,
        RPC_REPLACE_SYMBOL,
        RPC_INSERT_SYMBOL,
        RPC_REPLACE_NODE,
        RPC_PATCH,
        RPC_PROJECTION_CREATE,
        RPC_PROJECTION_LIST,
        RPC_PROJECTION_REMOVE,
        RPC_PROJECTION_RESTORE,
        RPC_PUBLISH,
        RPC_EXECUTE,
        RPC_READRESOURCE,
    ),
)

DOCUMENT = Document(
    schema="https://json-schema.org/draft/2020-12/schema",
    id="https://volar.sh/rift/protocol/mcp.json",
    title="Rift MCP surface",
    description=(
        "JSON values accepted and returned by the Rift MCP server. Each definition "
        "carries its Protobuf identity."
    ),
    entry_points_description=(
        "Requests do not open connection-scoped sessions. Each call names a projection or "
        "the workspace tree and carries every witnessed address it uses. A cursor names a "
        "process-local captured result; a projection retains its changes and recorded read "
        "dependencies across calls. The typed result rides `structuredContent` against the "
        "tool's declared output schema, mirrored as one "
        "canonical-JSON `text` block, and a refusal is such a result — never `isError`. "
        "`ErrorData` travels as the JSON-RPC error object's `data` with code -32000. The "
        "server serializes change application per targeted tree; a cancelled queued change "
        "is dropped, and a cancelled running change completes or rolls back whole. Each "
        "resource entry maps one URI family to the types that carry it."
    ),
    service=RIFT_SERVICE,
    tool_groups=(
        ToolGroup(
            name="discovery",
            title="Discovery",
            summary=("Find code and read facts the providers resolved."),
        ),
        ToolGroup(
            name="changes",
            title="Changes",
            summary=(
                "Change the workspace tree or a projection through declaration addresses, "
                "witnessed node addresses, or context-guarded patches."
            ),
        ),
        ToolGroup(
            name="projections",
            title="Projections",
            summary=(
                "Create, list, and remove projections; restore their changed paths or "
                "publish them into the workspace."
            ),
        ),
        ToolGroup(
            name="execution",
            title="Execution",
            summary=("Evaluate code in the targeted tree's execution copy."),
        ),
    ),
    tools=(
        Tool(name="get_symbol", rpc=RPC_GET_SYMBOL, group="discovery"),
        Tool(name="search", rpc=RPC_SEARCH, group="discovery"),
        Tool(name="nodes", rpc=RPC_NODES, group="discovery"),
        Tool(name="replace_symbol", rpc=RPC_REPLACE_SYMBOL, group="changes"),
        Tool(name="insert_symbol", rpc=RPC_INSERT_SYMBOL, group="changes"),
        Tool(name="replace_node", rpc=RPC_REPLACE_NODE, group="changes"),
        Tool(name="patch", rpc=RPC_PATCH, group="changes"),
        Tool(name="projection_create", rpc=RPC_PROJECTION_CREATE, group="projections"),
        Tool(name="projection_list", rpc=RPC_PROJECTION_LIST, group="projections"),
        Tool(name="projection_remove", rpc=RPC_PROJECTION_REMOVE, group="projections"),
        Tool(
            name="projection_restore", rpc=RPC_PROJECTION_RESTORE, group="projections"
        ),
        Tool(name="publish", rpc=RPC_PUBLISH, group="projections"),
        Tool(name="execute", rpc=RPC_EXECUTE, group="execution"),
    ),
    resources=(
        Resource(
            name="workspace",
            description=(
                "Reports the providers serving each language, limits, and hooks."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.WorkspaceResourceUri,
            link=mcp.ResourceLink,
        ),
        Resource(
            name="projection",
            description=(
                "Returns the filesystem path and state of one projection, for tools that "
                "can only work through a directory."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.ProjectionResourceUri,
            link=mcp.ResourceLink,
        ),
        Resource(
            name="changes",
            description=(
                "Returns one changeset — the workspace journal, or a projection's — each "
                "change with its edits, effects, advisories, and the confirmations "
                "publication will check."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.ChangesResourceUri,
            link=mcp.ResourceLink,
        ),
        Resource(
            name="fs",
            description=(
                "Returns one page of a directory's entries — subdirectories and files — or a "
                "bounded file-content range from the workspace tree."
            ),
            template=mcp.ResourceTemplate,
            uri=mcp.FsResourceUri,
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
                "What a change did to its tree, what vouched for it, and what still has to "
                "be accepted."
            ),
            identified_by=(
                core.ProjectionState,
                core.Advisory,
                mcp.ChangeId,
                mcp.ChangeSummary,
                mcp.ChangesResourcePayload,
            ),
        ),
        Axis(
            name="Projection",
            summary=(
                "Opt-in pinned copies of the workspace: their identities, lifecycle, "
                "and publication."
            ),
            identified_by=(
                core.ProjectionId,
                mcp.Projection,
                mcp.ProjectionCreateParams,
                mcp.ProjectionListParams,
                mcp.ProjectionListResult,
                mcp.ProjectionRemoveParams,
                mcp.ProjectionRemoveResult,
                mcp.ProjectionRestoreParams,
                mcp.DependencyConflict,
                mcp.PublishParams,
                mcp.PublishResult,
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
                core.LanguageRegion,
            ),
        ),
        Axis(
            name="Sources",
            summary=(
                "Project, dependency, standard-library, and generated source discovered "
                "before analysis."
            ),
            identified_by=(
                core.SourceUnitId,
                core.SourceResolverId,
                core.SourcePath,
                core.PackageIdentity,
                core.SourceLocation,
                core.SourceLocationKind,
                core.SourceKind,
                core.SourceUnit,
                core.SourceUnitSpan,
                core.SourceMapping,
                core.SourceMappingPrecision,
                mcp.SourceResolverSummary,
                mcp.SourceResolverState,
            ),
        ),
        Axis(
            name="Semantic",
            summary="Provider-resolved symbols, their types and documentation, and relationships between them.",
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
            name="History",
            summary=(
                "Symbol timelines and co-change coupling read from the workspace's "
                "version-control history."
            ),
            identified_by=(
                core.RevisionId,
                core.SymbolVersion,
                core.SymbolHistory,
                core.CoChange,
            ),
        ),
        Axis(
            name="Discovery",
            summary="Filters and path selectors used to find code.",
            identified_by=(
                core.Filter,
                core.FieldFilter,
                core.RelationFilter,
                core.ElementFilter,
                core.PathSelector,
                core.PathPattern,
                mcp.SearchScope,
            ),
        ),
        Axis(
            name="Reachability",
            summary="Coverage, diagnostics, and hook output for returned facts.",
            identified_by=(
                core.Coverage,
                core.CoverageScope,
                core.SemanticCoverage,
                core.FactFamily,
                core.Diagnostic,
                core.DiagnosticRelated,
                core.Severity,
                core.CapturedText,
            ),
        ),
        Axis(
            name="Operations",
            summary="Addresses, changes, and their evidence.",
            identified_by=(
                core.Address,
                core.OperationVerifier,
                core.PreconditionValue,
                core.OperationPrecondition,
                core.OperationBlocker,
                core.OperationEffect,
                core.GuaranteeKind,
                core.GuaranteeEvidence,
                core.ConfirmationRequirement,
                mcp.ReplaceSymbolParams,
                mcp.InsertSymbolParams,
                mcp.ReplaceNodeParams,
                mcp.PatchParams,
                core.RefusalReason,
                mcp.Hook,
                mcp.CommandHook,
            ),
        ),
        Axis(
            name="Execution",
            summary=("Caller-provided code evaluation and bounded process output."),
            identified_by=(
                core.CodeBlock,
                core.ExecutionResult,
                core.ExecutionStatus,
                core.CapturedText,
                core.ExecutionBudget,
                mcp.ExecuteParams,
                mcp.ExecuteResult,
            ),
        ),
        Axis(
            name="Server",
            summary=(
                "The lock file a process reads to find the workspace server and authorize "
                "against it."
            ),
            identified_by=(mcp.ServerLock,),
        ),
        Axis(
            name="Protocol",
            summary="Content digests and extension namespaces.",
            holds=(
                core.Digest,
                core.Extensions,
                core.ExtensionKey,
                core.ExtensionValue,
            ),
            residual_of="core",
        ),
        Axis(
            name="MCP",
            summary="Tool parameters, results, resource links, and payloads on the caller-facing surface.",
            residual_of="mcp",
        ),
    ),
)

__all__ = ["DOCUMENT", "RIFT_SERVICE"]
