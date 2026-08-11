from __future__ import annotations

import base64
from urllib.parse import parse_qs, quote, unquote_to_bytes

from pydantic import model_validator

from . import core
from .base import *


def _raw_resource_query(uri: str) -> dict[str, str]:
    query = uri.partition("?")[2]
    if not query:
        return {}
    values: dict[str, str] = {}
    for item in query.split("&"):
        key, separator, value = item.partition("=")
        if not separator or key in values:
            raise ValueError("resource query must contain unique key/value pairs")
        values[key] = value
    return values


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^[A-Za-z0-9_-]+$",
    min_length=1,
    max_length=4096,
)
class Cursor(ProtocolRoot):
    """An opaque base64url string that continues a paginated answer from where the last page
    ended. It binds the request, state, order, and page size. Padding is omitted. A mismatch
    returns `cursor_invalid`."""

    @model_validator(mode="after")
    def value_is_canonical_base64url(self) -> Cursor:
        core.validate_base64url(self.root)
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SourceExcerpt(ClosedModel):
    "A copy of some source, and where it was copied from. A span points into a file that may change under you; an excerpt is the bytes as they were when the answer was produced."

    span: Field[core.SourceSpan] = proto_field(
        description="The file and byte range the text was taken from.", number=1
    )
    text: Field[str] = proto_field(
        description="The source bytes returned by the request.", number=2
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Include",
        (
            EnumValue("symbol", "SYMBOL", 1),
            EnumValue("source", "SOURCE", 2),
            EnumValue("diagnostics", "DIAGNOSTICS", 3),
        ),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "symbol": "Attach the complete Symbol record, including signatures, types, documentation, and modifiers.",
            "source": "Attach the source covered by the outline node.",
            "diagnostics": "Attach adapter findings whose primary span falls inside the outline node.",
        }
    },
)
class OutlineInclude(str, Enum):
    SYMBOL = "symbol"
    SOURCE = "source"
    DIAGNOSTICS = "diagnostics"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class OutlineParams(ClosedModel):
    "Selects the adapter outline for one file. Outline nodes include declarations, definitions, imports, exports, and the parent nodes needed to preserve their nesting."

    path: Field[core.ProjectPath] = proto_field(
        description="Project-relative file to outline.", min_length=1, number=1
    )
    depth: Field[int | None] = proto_field(
        description=(
            "Most outline nesting levels to return. Zero keeps top-level items; null includes "
            "every nested item."
        ),
        number=2,
    )
    include: Field[list[OutlineInclude]] = proto_field(
        description=(
            "Optional payload attached to each outline item. Symbol identity and source "
            "structure are always present."
        ),
        number=3,
        json_schema_extra={"uniqueItems": True},
    )
    limit: Field[int | None] = proto_field(
        default=None,
        description="Most outline items in one page. The server may stop earlier at the response-byte limit.",
        ge=1,
        le=10000,
        number=4,
    )
    cursor: Field[Cursor | None] = proto_field(
        default=None,
        description="Continues the same file outline with every other parameter unchanged.",
        number=5,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class OutlineItem(ClosedModel):
    "One declaration-oriented source node. Items are ordered by range start, then widest range, then node identity, producing a stable source preorder."

    node: Field[core.Node] = proto_field(
        description="Source structure and parent identity for this outline node.",
        number=1,
    )
    symbol: Field[core.Symbol | None] = proto_field(
        default=None,
        description="Complete symbol data, present when `include` contains `symbol` and `node.symbol` exists.",
        number=2,
    )
    source: Field[SourceExcerpt | None] = proto_field(
        default=None,
        description="Source covered by the node, present when `include` contains `source`.",
        number=3,
    )
    diagnostics: Field[list[DiagnosticContext] | None] = proto_field(
        default=None,
        description="Findings inside the node, present when `include` contains `diagnostics`.",
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class OutlineResult(ClosedModel):
    """One page of a file outline with explicit semantic coverage."""

    file: Field[core.File] = proto_field(
        description="File entry being outlined.", number=2
    )
    items: Field[list[OutlineItem]] = proto_field(
        description="Outline items on this page in stable source preorder.", number=3
    )
    coverage: Field[core.SemanticCoverage] = proto_field(
        description="Completeness of nodes, symbols, types, relationships, and diagnostics used to build the outline.",
        number=4,
    )
    next_cursor: Field[Cursor | None] = proto_field(
        description="Cursor for the next outline page, or null after the final item.",
        number=5,
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Target",
        (
            EnumValue("symbol", "TARGET_SYMBOL", 1),
            EnumValue("node", "TARGET_NODE", 2),
            EnumValue("file", "TARGET_FILE", 3),
            EnumValue("all", "TARGET_ALL", 4),
        ),
        placement=Placement("target", 1),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "symbol": "Declarations the adapter resolved — a function, a class, a trait.",
            "node": "Places in a syntax tree where a symbol is written. One symbol has many.",
            "file": "Entries of the workspace tree. The only target that answers with no adapter installed.",
            "all": "Every kind above, in one ranked list.",
        }
    },
)
class SearchParamsTarget(str, Enum):
    "Which entity kinds may be returned. Type data is attached to the Symbol and Node records that bind it, and filters can search those attachments."

    SYMBOL = "symbol"
    NODE = "node"
    FILE = "file"
    ALL = "all"


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Include",
        (
            EnumValue("source", "INCLUDE_SOURCE", 1),
            EnumValue("signature", "INCLUDE_SIGNATURE", 2),
            EnumValue("relationships", "INCLUDE_RELATIONSHIPS", 3),
            EnumValue("diagnostics", "INCLUDE_DIAGNOSTICS", 4),
        ),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "source": "The source around the hit, with the span it was copied from.",
            "signature": (
                "The rendered signatures of a symbol hit, filled into `Symbol.signatures`. "
                "Nothing to add for a node or a file."
            ),
            "relationships": "The edges leading out of the hit.",
            "diagnostics": "What the adapter reported at the hit.",
        }
    },
)
class SearchInclude(str, Enum):
    SOURCE = "source"
    SIGNATURE = "signature"
    RELATIONSHIPS = "relationships"
    DIAGNOSTICS = "diagnostics"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SearchParams(ClosedModel):
    "What to search for. At least one of `query` and `filter` is required: `query` is lexical, while `filter` is a predicate over adapter facts. `paths` narrows either form before matching."

    model_config = closed_config(
        {
            "anyOf": [
                {
                    "description": "Satisfied by a text query, with or without a filter alongside it.",
                    "required": ["query"],
                },
                {
                    "description": "Satisfied by a filter alone, for a search with no text to match.",
                    "required": ["filter"],
                },
            ]
        }
    )
    target: Field[SearchParamsTarget] = proto_field(
        description=(
            "Which entity kinds may be returned. Type data is attached to the Symbol and Node "
            "records that bind it, and filters can search those attachments."
        ),
        number=1,
    )
    order: Field[ResultOrder] = proto_field(
        description=(
            "Which total order the page comes back in. The cursor is bound to it, so it "
            "cannot change between pages of one query."
        ),
        number=2,
    )
    query: Field[str | None] = proto_field(
        default=None,
        description=(
            "Text to match against file contents, symbol names, and rendered signatures. "
            "`parse` matches that word in those fields. Caller lookup uses a relationship "
            "filter."
        ),
        number=3,
    )
    filter: Field[core.Filter | None] = proto_field(
        default=None,
        description=(
            "A predicate over resolved fields and relationships. This is where adapter "
            "knowledge enters a search — implements this trait, called by that function, "
            "declared under `src/api`."
        ),
        number=4,
    )
    paths: Field[core.PathSelector | None] = proto_field(
        default=None,
        description=(
            "Files eligible for the search, selected by project-relative globs. Omitted "
            "selects every visible file."
        ),
        number=5,
    )
    include: Field[list[SearchInclude] | None] = proto_field(
        default=None,
        description=(
            "Extra payload to attach to every hit. Each entry costs a lookup per hit, so ask "
            "for what you will read."
        ),
        number=6,
    )
    limit: Field[int | None] = proto_field(
        default=None,
        description=(
            "Most hits to return in one page. `max_page_items` from the workspace resource "
            "caps it, and fewer may come back."
        ),
        ge=1,
        le=10000,
        number=7,
    )
    cursor: Field[Cursor | None] = proto_field(
        default=None,
        description=(
            "Continues a previous search where its last page ended. Omit it for the first "
            "page; everything else in the request has to match what the cursor was minted "
            "for."
        ),
        number=8,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SearchHit(ClosedModel):
    """One search hit. Its file, node, or symbol payload carries the canonical identity."""

    hit: Field[SearchHitTarget] = proto_field(
        description="What was found. A symbol, a node, or a file — whichever `target` allowed.",
        number=1,
    )
    score: Field[float] = proto_field(
        description=(
            "How well this hit matched. Scores are comparable across every page of one "
            "request and nowhere else."
        ),
        number=2,
    )
    matched_by: Field[list[str]] = proto_field(
        description="Which fields produced the match — `name`, `signature`, the text of the file.",
        number=3,
    )
    relationships: Field[list[core.Relationship] | None] = proto_field(
        default=None,
        description='Edges from this hit, requested with `include: ["relationships"]`.',
        number=4,
    )
    source: Field[SourceExcerpt | None] = proto_field(
        default=None,
        description=(
            'The source around the hit, requested with `include: ["source"]`. Carries its '
            "span, so an agent can act on what it read without searching for it again."
        ),
        number=5,
    )
    diagnostics: Field[list[DiagnosticContext] | None] = proto_field(
        default=None,
        description='What the adapter reported here, requested with `include: ["diagnostics"]`.',
        number=6,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SymbolResourcePayload(ClosedModel):
    """JSON payload for one symbol in the session projection."""

    uri: Field[SymbolResourceUri] = proto_field(
        description="The symbol this payload answers for, echoed back so a link and its content carry the same address.",
        number=1,
    )
    symbol: Field[core.Symbol] = proto_field(
        description=(
            "The declaration itself: its name, its kind in the language's own words, its "
            "origin, its types and its signatures."
        ),
        number=3,
    )
    origin_mappings: Field[list[core.OriginMapping]] = proto_field(
        description=(
            "Relations from produced declaration bytes to source ranges a caller can inspect "
            "or edit. Empty for a declaration read directly from physical source."
        ),
        number=4,
    )
    nodes: Field[list[core.Node]] = proto_field(
        description=(
            "Every place this symbol is written — the declaration and each mention. This is "
            "the list a rename has to rewrite."
        ),
        number=5,
    )
    relationships: Field[list[core.Relationship]] = proto_field(
        description="Edges into and out of this symbol, each carrying the nodes from which the adapter derived it.",
        number=6,
    )
    source: Field[list[SourceExcerpt]] = proto_field(
        description=(
            "The source at each node, so the declaration and its call sites can be read "
            "without a second round trip."
        ),
        number=7,
    )
    diagnostics: Field[list[DiagnosticContext]] = proto_field(
        description="What the adapter reported at this symbol's nodes.", number=8
    )
    coverage: Field[core.SemanticCoverage] = proto_field(
        description=(
            "How complete each fact family is for this symbol. An empty `relationships` means "
            "the symbol has no edges only where that family is complete."
        ),
        number=9,
    )
    next: Field[SymbolResourceUri | None] = proto_field(
        description=(
            "The same symbol URI carrying the cursor for the next page, or null on the last "
            "one. Nodes, edges and diagnostics are what page."
        ),
        number=10,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class Contract(ClosedModel):
    "One generated wire contract. Majors have to be equal, because a major changes when a peer can no longer decode the next message safely. Within one major, revisions only add, so a peer can speak any minor at or below its own and the connection settles on the lower of the two."

    major: Field[core.ProtocolVersion] = proto_field(
        description="Breaking protocol generation selected before the socket is opened.",
        number=1,
    )
    minor: Field[int] = proto_field(
        description=(
            "Additive revision within the major. A peer admits any minor at or below its own "
            "and emits nothing added after the minor the connection settled on, so the client "
            "checks this one number to know it can decode everything it will be sent."
        ),
        ge=0,
        le=4294967295,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
    )
    schema_digest: Field[core.Digest] = proto_field(
        description=(
            "SHA-256 of the generated descriptor and its MCP conversion metadata. It names the "
            "exact generation a peer was built from, which is what makes a bug report "
            "reproducible; admission is decided by `major` and `minor`."
        ),
        number=3,
    )


@scalar(
    owner=MCP,
    public=False,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    min_length=1,
    max_length=32768,
    examples=["/home/alice/projects/rift", "C:\\Users\\alice\\projects\\rift"],
)
class WorkspacePath(ProtocolRoot):
    """Canonical absolute path that identifies one workspace on this host."""


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^chg_[a-z2-7]{26}$",
    examples=["chg_bbbbbbbbbbbbbbbbbbbbbbbbbb"],
)
class ChangeId(ProtocolRoot):
    """Identity of one change recorded in the projection's changeset. Rift mints it when the
    change lands and keeps it until publication or restore removes the change."""


@scalar(
    owner=MCP,
    public=False,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^ses_[a-z2-7]{26}$",
)
class SessionId(ProtocolRoot):
    """Random 128-bit identity of one persistent session projection. The server admits at most
    one live MCP connection for the identity."""


@scalar(
    owner=MCP,
    public=False,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^con_[a-z2-7]{26}$",
)
class ConnectionId(ProtocolRoot):
    """Random 128-bit identity of one live control stream. Subsequent RPCs carry it in
    `rift-connection-id` metadata."""


@scalar(
    owner=MCP,
    public=False,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^[a-z][a-z0-9_.-]{0,127}$",
)
class FeatureId(ProtocolRoot):
    """One optional behavior implemented by a client or server."""


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Role",
        (
            EnumValue("mcp", "ROLE_MCP", 1),
            EnumValue("scip", "ROLE_SCIP", 2),
        ),
        placement=Placement("role", 3),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "mcp": "An MCP bridge that creates a session and may reconnect to it while the process lives.",
            "scip": "A read-only SCIP projection client with no session.",
        }
    },
)
class ConnectRole(str, Enum):
    """How this connection will use the workspace server."""

    MCP = "mcp"
    SCIP = "scip"


@definition(
    owner=MCP,
    public=False,
    proto=Proto.message(),
    schema_extra={
        "allOf": [
            {
                "if": {
                    "properties": {"role": {"const": "mcp"}},
                    "required": ["role"],
                },
                "then": {
                    "properties": {"session": {"not": {"type": "null"}}},
                    "required": ["session"],
                },
                "else": {"properties": {"session": {"type": "null"}}},
            }
        ]
    },
)
class ConnectRequest(ClosedModel):
    """Opens one logical client connection before another server RPC. The control stream
    remains open for the connection lifetime."""

    contracts: Field[list[Contract]] = proto_field(
        description="Supported contracts in client preference order.",
        min_length=1,
        number=1,
    )
    features: Field[list[FeatureId]] = proto_field(
        description="Optional behaviors implemented by the client, sorted by identifier.",
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
    role: Field[ConnectRole] = proto_field(
        description="Whether the connection serves MCP requests or a read-only SCIP projection.",
        number=3,
    )
    session: Field[SessionId | None] = proto_field(
        default=None,
        description=(
            "Process-generated persistent session to create or reconnect to. Required for an MCP "
            "role and null for a SCIP role. A live connection already using it makes Connect "
            "temporarily unavailable."
        ),
        number=4,
    )
    canonical_root: Field[WorkspacePath] = proto_field(
        description="Canonical absolute UTF-8 path of the workspace served by this endpoint.",
        number=5,
    )
    client_build: Field[str] = proto_field(
        description="Client build as it names itself in diagnostics.",
        min_length=1,
        max_length=256,
        number=6,
    )

    @model_validator(mode="after")
    def role_has_valid_session_fields(self) -> ConnectRequest:
        if self.role == ConnectRole.MCP and self.session is None:
            raise ValueError("MCP connections require a process-generated session")
        if self.role == ConnectRole.SCIP and self.session is not None:
            raise ValueError("SCIP connections cannot carry a session")
        return self


@definition(
    owner=MCP,
    public=False,
    proto=Proto.message(),
    schema_extra={
        "allOf": [
            {
                "if": {
                    "anyOf": [
                        {"not": {"required": ["session"]}},
                        {
                            "properties": {"session": {"type": "null"}},
                            "required": ["session"],
                        },
                    ]
                },
                "then": {"properties": {"state": {"type": "null"}}},
                "else": {
                    "properties": {"state": {"not": {"type": "null"}}},
                    "required": ["state"],
                },
            }
        ]
    },
)
class Connected(ClosedModel):
    """The first response on an accepted control stream, including persistent projection state
    for an MCP role."""

    contract: Field[Contract] = proto_field(
        description=(
            "Contract this connection speaks: the shared major, and the lower of the two minors."
        ), number=1
    )
    features: Field[list[FeatureId]] = proto_field(
        description="Features implemented by both peers, sorted by identifier.",
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
    workspace: Field[WorkspacePath] = proto_field(
        description="Canonical physical path that identifies this workspace.", number=3
    )
    session: Field[SessionId | None] = proto_field(
        default=None,
        description="Created or reconnected session for an MCP role; null for a SCIP role.",
        number=4,
    )
    state: Field[core.ProjectionState | None] = proto_field(
        default=None,
        description="Exact current projection state, or null for a SCIP role.",
        number=8,
    )
    connection: Field[ConnectionId] = proto_field(
        description="Identity required in metadata on every later RPC.", number=7
    )

    @model_validator(mode="after")
    def session_state_is_correlated(self) -> Connected:
        if self.session is None:
            if self.state is not None:
                raise ValueError("SCIP connections cannot carry session state")
        elif self.state is None:
            raise ValueError("MCP connections require session state")
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SessionSummary(ClosedModel):
    """One retained MCP session and its projection."""

    session: Field[SessionId] = proto_field(
        description="Persistent session identity.",
        number=1,
    )
    state: Field[core.ProjectionState] = proto_field(
        description="Current projection state.",
        number=5,
    )
    active: Field[bool] = proto_field(
        description="Whether one live MCP connection currently owns this session.",
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SessionListParams(ClosedModel):
    """Selects one page from a captured list of retained sessions in session-ID order."""

    limit: Field[int] = proto_field(
        default=100,
        description="Most retained sessions to return on this page.",
        ge=1,
        le=1000,
        number=1,
    )
    cursor: Field[Cursor | None] = proto_field(
        default=None,
        description=(
            "Continues a captured session list with the same page size. The cursor expires when "
            "the server can no longer retain that list."
        ),
        number=2,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SessionListResult(ClosedModel):
    """One page of retained sessions."""

    sessions: Field[list[SessionSummary]] = proto_field(
        description="Retained sessions on this page, sorted by session ID.", number=1
    )
    next_cursor: Field[Cursor | None] = proto_field(
        description="Cursor for the next page, or null after the final session.",
        number=2,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SessionContinueParams(ClosedModel):
    """Selects a retained session for the current MCP connection to continue. The current initial
    session must have its original clean projection and no retained debugging session. The
    retained session must have no live connection. A changed initial session returns
    `invalid_request`; an active session or retained debugger returns `temporarily_unavailable`."""

    session: Field[SessionId] = proto_field(
        description="Retained session to continue.", number=1
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SessionContinueResult(ClosedModel):
    """The active retained session now bound to the current MCP connection. The MCP process
    replaces its in-memory session ID with this identity before it sends another call."""

    session: Field[SessionSummary] = proto_field(
        description="Session state after the connection attached.", number=1
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SessionRemoveParams(ClosedModel):
    """Removes one inactive session and the projection it retains."""

    session: Field[SessionId] = proto_field(
        description="Retained session to remove.", number=1
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SessionRemoveResult(ClosedModel):
    """The removed session and the projection state it held."""

    session: Field[SessionId] = proto_field(
        description="Session the result describes.", number=1
    )
    state: Field[core.ProjectionState] = proto_field(
        description="Exact projection state the session held when it was removed.",
        number=2,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DebugLimits(ClosedModel):
    """Workspace ceilings for retained debugging sessions. Null at
    `ExecutionLimits.debug` means `rift.toml` disables debugging."""

    max_sessions: Field[int] = proto_field(
        description="Debugging sessions retained across all connections in this workspace.",
        ge=1,
        le=16,
        number=1,
    )
    idle_timeout_ms: Field[int] = proto_field(
        description=(
            "Milliseconds without debug_get_frame or debug_stop after which Rift stops the session "
            "and releases its execution workspace."
        ),
        ge=1,
        le=86400000,
        number=2,
    )
    max_frames: Field[int] = proto_field(
        description="Stack frames one failed debugging evaluation may retain.",
        ge=1,
        le=256,
        number=3,
    )
    max_bindings_per_frame: Field[int] = proto_field(
        description="Arguments plus locals one debug frame may return.",
        ge=1,
        le=128,
        number=4,
    )
    max_value_bytes: Field[int] = proto_field(
        description="Captured UTF-8 bytes in each rendered binding value.",
        ge=0,
        le=8192,
        number=5,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ExecutionLimits(ClosedModel):
    """Workspace ceilings for caller-provided code. Null at `Limits.execution` means
    `rift.toml` disables execution, regardless of adapter capability."""

    max_code_bytes: Field[int] = proto_field(
        description="UTF-8 bytes accepted in one CodeBlock.source.",
        ge=1,
        le=32768,
        number=1,
    )
    max_timeout_ms: Field[int] = proto_field(
        description="Wall-clock milliseconds allowed for one evaluation.",
        ge=1,
        le=86400000,
        number=2,
    )
    max_output_bytes: Field[int] = proto_field(
        description="Captured prefix bytes allowed separately for stdout and stderr.",
        ge=0,
        le=16384,
        number=3,
    )
    max_concurrent: Field[int] = proto_field(
        description="Evaluations running concurrently across all connections in the workspace.",
        ge=1,
        le=64,
        number=4,
    )
    debug: Field[DebugLimits | None] = proto_field(
        description="Debugging ceilings, or null when debugging is disabled in `rift.toml`.",
        number=5,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class Limits(ClosedModel):
    "The ceilings this server enforces on MCP requests and responses. Rift chooses its internal bounds, while `rift.toml` supplies the caller-code limits. A request or response that crosses one fails with `limit_exceeded` carrying `LimitEvidence`."

    max_request_bytes: Field[int] = proto_field(
        description=(
            "Largest request body the server accepts, in bytes. A deep filter tree or a long "
            "structural pattern is the usual way to exceed it."
        ),
        ge=1024,
        le=49152,
        number=1,
    )
    max_response_bytes: Field[Literal[65536]] = proto_field(
        description=(
            "Largest serialized tool result or resource page Rift returns. Paginated answers "
            "stop before this boundary and provide a cursor; an indivisible result that "
            "cannot fit fails with `limit_exceeded`. Conforming `read` implementations "
            "advertise at most 65536 bytes, below the truncation boundary of common MCP "
            "harnesses."
        ),
        number=2,
    )
    max_record_bytes: Field[int] = proto_field(
        description=(
            "Largest RFC 8785 JSON encoding of one indivisible item in a paginated answer, "
            "such as one Change, Edit, diagnostic, or validator result. Resolution fails with "
            "`limit_exceeded` before publishing a change when one item exceeds this value. "
            "Rift keeps it at or below 49152 bytes, leaving page space for identity and cursors."
        ),
        ge=1024,
        le=49152,
        number=3,
    )
    max_file_chunk_bytes: Field[int] = proto_field(
        description=(
            "Most source bytes one file resource page carries before UTF-8 or base64 "
            "serialization. Rift may return fewer bytes to preserve a UTF-8 boundary and stay "
            "within `max_response_bytes`."
        ),
        ge=1024,
        le=32768,
        number=4,
    )
    max_page_items: Field[int] = proto_field(
        description=(
            "Ceiling on `limit` for every paginated tool. Asking for more is "
            "`invalid_request`; asking for less than this still permits a shorter page."
        ),
        ge=1,
        le=10000,
        number=5,
    )
    max_relation_depth: Field[int] = proto_field(
        description=(
            "How far a relationship filter may walk. Transitive callers of a widely used "
            "function fan out fast, and this is what stops one query from touching the whole "
            "graph."
        ),
        ge=1,
        le=100,
        number=6,
    )
    max_edits: Field[int] = proto_field(
        description="Most concrete `Edit` values one resolved operation may contain across every change.",
        ge=1,
        le=1000000,
        number=8,
    )
    max_validators: Field[int] = proto_field(
        description=(
            "How many workspace command checks may run before publication. "
            "Zero when `rift.toml` declares none."
        ),
        ge=0,
        le=4294967295,
        number=9,
    )
    max_rewrite_expansions: Field[int] = proto_field(
        description="Most concrete edits one atomic `rewrite` (`RewriteParams`) may produce after matching.",
        ge=1,
        le=100000,
        number=10,
    )
    execution: Field[ExecutionLimits | None] = proto_field(
        description=(
            "Caller-code execution ceilings. Null means `rift.toml` disables execute and all "
            "debug tools; adapter capability alone never enables them."
        ),
        number=11,
    )


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://workspace(?:\?cursor=[A-Za-z0-9_-]{1,4096})?$",
    max_length=8192,
)
class WorkspaceResourceUri(ProtocolRoot):
    """Paginated workspace metadata and capabilities."""

    @model_validator(mode="after")
    def query_is_canonical(self) -> WorkspaceResourceUri:
        query = _raw_resource_query(self.root)
        cursor = query.get("cursor")
        if cursor is not None:
            Cursor.model_validate(cursor)
        return self


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://projection$",
)
class ProjectionResourceUri(ProtocolRoot):
    """The session's projection directory on this host. The name is deliberate: MCP's own
    `roots` capability runs the other way, a client granting a server directories, so this
    resource does not call itself a root."""


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://changes(?:\?cursor=[A-Za-z0-9_-]{1,4096})?$",
    max_length=8192,
)
class ChangesResourceUri(ProtocolRoot):
    """Paginated changeset of the session projection."""

    @model_validator(mode="after")
    def query_is_canonical(self) -> ChangesResourceUri:
        cursor = _raw_resource_query(self.root).get("cursor")
        if cursor is not None:
            Cursor.model_validate(cursor)
        return self


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://symbol/[A-Za-z][A-Za-z0-9._-]*(?::[A-Za-z][A-Za-z0-9._-]*)?/(?:[A-Za-z0-9._~!$&'()*+,;=:/@-]|%[0-9A-F]{2}){1,1000}(?:\?cursor=[A-Za-z0-9_-]{1,4096})?$",
    min_length=17,
    max_length=12288,
    examples=[
        "rift://symbol/python/pkg.util.load_config~1",
        "rift://symbol/rust/config::Loader?cursor=eyJwYWdlIjoyfQ",
    ],
)
class SymbolResourceUri(ProtocolRoot):
    """URI for one symbol read. It is the symbol's own `SymbolId`, optionally carrying the
    cursor that continues its paged nodes, edges, and diagnostics."""

    @model_validator(mode="after")
    def identity_is_canonical(self) -> SymbolResourceUri:
        core.SymbolId.model_validate(self.root.partition("?")[0])
        cursor = _raw_resource_query(self.root).get("cursor")
        if cursor is not None:
            Cursor.model_validate(cursor)
        return self


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://fs(?:/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-F]{2}){1,1000})?(?:\?(?:start=[0-9]+&length=[1-9][0-9]*|cursor=[A-Za-z0-9_-]{1,4096}))?$",
    min_length=9,
    max_length=8192,
)
class FsResourceUri(ProtocolRoot):
    """URI for one filesystem entry. File ranges use `start` and `length`; directory pages use
    `cursor`."""

    @model_validator(mode="after")
    def address_is_canonical(self) -> FsResourceUri:
        address = self.root.partition("?")[0]
        encoded_path = address.removeprefix("rift://fs").removeprefix("/")
        if encoded_path:
            decoded_path = unquote_to_bytes(encoded_path).decode("utf-8")
            core.ProjectPath.model_validate(decoded_path)
            canonical = quote(decoded_path, safe="/!$&'()*+,;=:@-._~")
            if canonical != encoded_path:
                raise ValueError("filesystem path must use canonical URI encoding")
        query = self.root.partition("?")[2]
        if not query:
            return self
        values = parse_qs(query, keep_blank_values=True, strict_parsing=True)
        if "cursor" in values:
            Cursor.model_validate(values["cursor"][0])
            return self
        if "start" in values:
            if values["start"][0] != str(int(values["start"][0])):
                raise ValueError("file range start must use canonical decimal")
            if values["length"][0] != str(int(values["length"][0])):
                raise ValueError("file range length must use canonical decimal")
            start = int(values["start"][0])
            length = int(values["length"][0])
            if start > 9007199254740991 or length > 9007199254740991:
                raise ValueError("file range exceeds exact protocol integers")
            if start + length > 9007199254740991:
                raise ValueError("file range end exceeds exact protocol integers")
        return self


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "ResourceFamily",
        (
            EnumValue("workspace", "RESOURCE_FAMILY_WORKSPACE", 1),
            EnumValue("symbol", "RESOURCE_FAMILY_SYMBOL", 2),
            EnumValue("fs", "RESOURCE_FAMILY_FS", 3),
            EnumValue("actions", "RESOURCE_FAMILY_ACTIONS", 4),
            EnumValue("action", "RESOURCE_FAMILY_ACTION", 5),
            EnumValue("projection", "RESOURCE_FAMILY_PROJECTION", 6),
            EnumValue("changes", "RESOURCE_FAMILY_CHANGES", 7),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "workspace": "Workspace capabilities, projection state, and request limits.",
            "symbol": "One symbol, its nodes, its edges and its diagnostics.",
            "fs": "One directory page or file-content range.",
            "actions": "The fixes and refactors an adapter offers at one address, or across one file.",
            "action": "One discovered action, with the schema of the arguments it takes.",
            "projection": "Where this session's projection lives on the filesystem.",
            "changes": "The changes this session has made, and what vouched for each.",
        }
    },
)
class ResourceFamily(str, Enum):
    "One family of Rift resources. The family fixes the URI shape a read accepts, the media type it returns, and the payload model inside it."

    WORKSPACE = "workspace"
    SYMBOL = "symbol"
    FS = "fs"
    ACTIONS = "actions"
    ACTION = "action"
    PROJECTION = "projection"
    CHANGES = "changes"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class WorkspaceResourcePayload(ClosedModel):
    """Current workspace state and request limits. Each configured language reports its
    adapter support and effective caller-code availability."""

    uri: Field[WorkspaceResourceUri] = proto_field(
        description="The URI this payload answers for.",
        number=1,
    )
    state: Field[core.ProjectionState] = proto_field(
        description="Current session projection state.",
        number=2,
    )
    contract: Field[Contract] = proto_field(
        description=(
            "Protocol version and schema identifier for every other field in this payload."
        ),
        number=3,
    )
    limits: Field[Limits] = proto_field(
        description="The ceilings a request has to stay inside here.", number=4
    )
    languages: Field[list[LanguageSupport]] = proto_field(
        description="Configured languages and their capabilities, sorted by name and dialect with null first.",
        number=5,
        json_schema_extra={"uniqueItems": True},
    )
    coverage: Field[core.SemanticCoverage] = proto_field(
        description=(
            "How complete each fact family is across the workspace. A family reported "
            "`unsupported` here remains unsupported in subsequent answers."
        ),
        number=6,
    )
    resources: Field[list[ResourceFamily]] = proto_field(
        description="The MCP resource families this workspace serves.",
        number=8,
        json_schema_extra={"uniqueItems": True},
    )
    next: Field[WorkspaceResourceUri | None] = proto_field(
        description="The same resource carrying the cursor for the next page, or null on the last one.",
        number=9,
    )
    matching: Field[MatchSyntax] = proto_field(
        description="The pattern grammars the `match` tool accepts here.", number=11
    )
    validators: Field[list[CommandValidator]] = proto_field(
        description="Publication checks declared by the workspace-root `rift.toml`.",
        number=13,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionResourcePayload(ClosedModel):
    """Where this session's projection lives on the filesystem. Adapters receive the tree they
    analyze over the adapter protocol; a caller that has to reach the projection through an
    ordinary filesystem tool reads it here."""

    uri: Field[ProjectionResourceUri] = proto_field(
        description="The URI this payload answers for.", number=1
    )
    path: Field[WorkspacePath] = proto_field(
        description=(
            "Absolute path of the projection directory. Rift places it at "
            "`.rift/projections/<session>` below the workspace and keeps it there for the "
            "session's life. Reading this resource materializes a projection that has not "
            "diverged yet, because a tool pointed here may write and those writes belong to "
            "the changeset. Session removal deletes the directory without waiting for a "
            "process that is still working inside it."
        ),
        number=2,
    )
    workspace: Field[WorkspacePath] = proto_field(
        description="Absolute path of the workspace this projection was taken from.",
        number=3,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SearchResult(ClosedModel):
    "One page of search hits, and what the page is worth. `coverage` is what makes an empty page readable: nothing matched, or Rift could not see far enough to know."

    coverage: Field[core.Coverage] = proto_field(
        description=(
            "How much Rift could see while answering. An empty page means nothing matched "
            "only where this is complete."
        ),
        number=2,
    )
    results: Field[list[SearchHit]] = proto_field(
        description="The hits on this page, in the order the request asked for.",
        number=3,
    )
    next_cursor: Field[Cursor | None] = proto_field(
        description="Pass this back to get the next page. Null on the last one.",
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ActionOffer(ClosedModel):
    "One adapter action discovered at an address: the URI that resolves it, and the portable description the adapter returned. A listing page leaves `descriptor.arguments_schema` null, because a page of offers carries as many schemas as it has entries and a caller reads one. The single-offer resource always carries it."

    action: Field[core.ActionOfferId] = proto_field(
        description=(
            "Identity of this offer. Hand it to `act`, or read it for the argument schema the "
            "action takes."
        ),
        number=1,
    )
    descriptor: Field[core.ActionDescriptor] = proto_field(
        description="What the action does and what it applies to.", number=2
    )


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://actions/(?:symbol|node|match|file)/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-F]{2}){1,8192}(?:\?only=[a-z][a-z0-9_.-]*(?:&cursor=[A-Za-z0-9_-]{1,4096})?|\?cursor=[A-Za-z0-9_-]{1,4096})?$",
    min_length=22,
    max_length=32768,
    examples=[
        "rift://actions/file/src/api.rs?only=quickfix",
        "rift://actions/symbol/python/pkg.util.load_config",
    ],
)
class ActionsResourceUri(ProtocolRoot):
    """URI for adapter actions at one address in the session projection. The address is
    `symbol/{language}/{name}`, `node/{language}/{path}@{start}-{end}`, `match/{token}`,
    or `file/{path}`."""

    @model_validator(mode="after")
    def address_is_well_formed(self) -> ActionsResourceUri:
        address = self.root.removeprefix("rift://actions/").partition("?")[0]
        kind, separator, body = address.partition("/")
        if not separator:
            raise ValueError("actions URI requires an address kind")
        if kind == "node":
            core.NodeId.model_validate(f"rift://node/{body}")
        elif kind == "file":
            core.FileId.model_validate(f"rift://file/{body}")
        elif kind == "symbol":
            core.SymbolId.model_validate(f"rift://symbol/{body}")
        elif kind == "match":
            core.MatchId.model_validate(f"rift://match/{body}")
        else:
            raise ValueError("unknown actions address kind")
        query = _raw_resource_query(self.root)
        cursor = query.get("cursor")
        if cursor is not None:
            Cursor.model_validate(cursor)
        only = query.get("only")
        if only is not None:
            core.ActionKind.model_validate(only)
        return self


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern="^rift://action/[A-Za-z0-9_-]{16,8192}$",
    min_length=31,
    max_length=8207,
    examples=["rift://action/eyJsYW5ndWFnZSI6eyJuYW1lIjoicnVzdCJ9fQ"],
)
class ActionResourceUri(ProtocolRoot):
    """URI for one discovered action. It is the `ActionOfferId` a listing returned, and reading it returns the same offer with the JSON Schema of the arguments the action takes."""

    @model_validator(mode="after")
    def identity_is_canonical(self) -> ActionResourceUri:
        core.ActionOfferId.model_validate(self.root)
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ActionsResourcePayload(ClosedModel):
    "One page of actions available at one address in the session projection."

    uri: Field[ActionsResourceUri] = proto_field(
        description="The address this page answers for, echoed back as it resolved.",
        number=1,
    )
    offers: Field[list[ActionOffer]] = proto_field(
        description="The actions on this page, each without its argument schema.",
        number=3,
    )
    coverage: Field[core.Coverage] = proto_field(
        description=(
            "Whether the adapter could answer here. A language with no action support returns "
            "an empty list with `unsupported` coverage and its reason."
        ),
        number=4,
    )
    next: Field[ActionsResourceUri | None] = proto_field(
        description="The same address carrying the cursor for the next page, or null on the last one.",
        number=5,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ActionResourcePayload(ClosedModel):
    "One discovered action and the arguments it accepts. This is the read that supplies `arguments_schema`, so a caller fetches one schema for the action it chose rather than a schema per offer on a page."

    uri: Field[ActionResourceUri] = proto_field(
        description="The offer this payload answers for, echoed back as it resolved.",
        number=1,
    )
    language: Field[core.Language] = proto_field(
        description="Language whose adapter minted the offer and resolves it.", number=3
    )
    offer: Field[ActionOffer] = proto_field(
        description="The offer, with `descriptor.arguments_schema` present.", number=4
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ExecuteParams(ClosedModel):
    """Evaluate code with one configured language adapter in the session projection."""

    language: Field[core.Language] = proto_field(
        description="Exact language and optional dialect selecting the adapter.",
        number=1,
    )
    block: Field[core.CodeBlock] = proto_field(
        description="Source to evaluate and its project-relative working directory.",
        number=2,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ExecuteResult(ClosedModel):
    """Bounded result of one execution. Writes made by evaluated code are absent because its
    execution workspace is discarded."""

    language: Field[core.Language] = proto_field(
        description="Adapter that evaluated the block.", number=2
    )
    result: Field[core.ExecutionResult] = proto_field(
        description="Runtime status, bounded output, and structured diagnostics.",
        number=3,
    )
    budget: Field[core.ExecutionBudget] = proto_field(
        description="Exact bounds sent to the adapter for this evaluation.", number=4
    )


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern="^[A-Za-z0-9_-]{16,128}$",
)
class DebugSessionId(ProtocolRoot):
    """Opaque connection-bound identity of one debugging session. It is valid only on the MCP connection that started it and until debug_stop or connection cleanup."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DebugSession(ClosedModel):
    """Public summary of an inspect-only debugging evaluation. Start runs the block to normal
    completion or an unhandled failure; failed runs retain their stack for frame reads."""

    id: Field[DebugSessionId] = proto_field(
        description="Server-minted public handle. The adapter-local DebugSessionKey never crosses MCP.",
        number=1,
    )
    language: Field[core.Language] = proto_field(
        description="Adapter that owns the retained debug state.", number=3
    )
    frame_count: Field[int] = proto_field(
        description="Retained stack frames, innermost first. Zero after normal completion.",
        ge=0,
        le=4294967295,
        number=4,
    )
    result: Field[core.ExecutionResult] = proto_field(
        description="How the debugging evaluation ended and its bounded output.",
        number=5,
    )
    budget: Field[core.DebugBudget] = proto_field(
        description="Exact execution and retained-frame bounds applied to this session.",
        number=6,
    )

    @model_validator(mode="after")
    def completed_session_has_no_frames(self) -> DebugSession:
        if (
            self.result.status is core.ExecutionStatus.COMPLETED
            and self.frame_count != 0
        ):
            raise ValueError("completed debugging session must have frame_count zero")
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DebugStartParams(ClosedModel):
    """Start an inspect-only debugging evaluation in a disposable execution workspace."""

    language: Field[core.Language] = proto_field(
        description="Exact language and optional dialect selecting the adapter.",
        number=1,
    )
    block: Field[core.CodeBlock] = proto_field(number=2)


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DebugGetFrameParams(ClosedModel):
    """Read one retained stack frame without resuming or mutating the debugging session."""

    session: Field[DebugSessionId] = proto_field(number=1)
    depth: Field[int] = proto_field(
        description="Zero-based stack depth, with the innermost retained frame first.",
        ge=0,
        le=4294967295,
        number=2,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DebugGetFrameResult(ClosedModel):
    """One retained frame from a connection-bound debugging session."""

    session: Field[DebugSessionId] = proto_field(number=1)
    frame: Field[core.DebugFrame] = proto_field(number=2)


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DebugStopParams(ClosedModel):
    """Stop a debugging session and release its adapter state and execution workspace."""

    session: Field[DebugSessionId] = proto_field(number=1)


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DebugStopResult(ClosedModel):
    """Acknowledgement that a debugging session no longer retains runtime state."""

    session: Field[DebugSessionId] = proto_field(number=1)
    stopped: Field[Literal[True]] = proto_field(default=True, number=2)


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class MatchHitStructural(ClosedModel):
    """A structural match with grammar-derived replacement ranges."""

    kind: Field[Literal["structural"]] = proto_field(
        description="Tags this as a structural match."
    )
    captures: Field[list[core.Capture]] = proto_field(
        description="The named captures this match bound, in the order the pattern declares them.",
        number=1,
    )
    explanation: Field[list[str]] = proto_field(
        description="Why this is a match, one step per line.", number=2
    )
    replacement_ranges: Field[core.StructuralMatchRanges] = proto_field(
        description=(
            "Ranges the adapter says can be replaced whole: the matched node alone, or with "
            "the whitespace and punctuation on either side. Rewriting `foo(a, b)` out of a "
            "list needs one of the wider ones to avoid leaving a comma behind."
        ),
        number=3,
    )
    extensions: Field[core.Extensions] = proto_field(
        description="Facts the adapter carries that this model has no field for, under a reverse-domain key.",
        number=4,
    )
    key: Field[core.MatchId] = proto_field(
        description=(
            "Identity of this match and the state it was found in. An edit addressed at it is "
            "checked against this before it lands."
        ),
        number=5,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class MatchHitText(ClosedModel):
    """A text match. Bytes matched bytes, with no tree behind them and nothing to say about what is safe to replace."""

    kind: Field[Literal["text"]] = proto_field(description="Tags this as a text match.")
    captures: Field[list[core.Capture]] = proto_field(
        description="The named captures this match bound, in the order the pattern declares them.",
        number=1,
    )
    explanation: Field[list[str]] = proto_field(
        description="Why this is a match, one step per line.", number=2
    )
    key: Field[core.MatchId] = proto_field(
        description=(
            "Identity of this match and the state it was found in. An edit addressed at it is "
            "checked against this before it lands."
        ),
        number=3,
    )


@union(
    owner=MCP,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("structural", "structural", 1, MatchHitStructural),
        Variant("text", "text", 2, MatchHitText),
    ),
)
class MatchHit(ProtocolRoot):
    "One match, tagged by the engine that produced it. The tag equals `key.query.kind`. A structural match carries grammar-derived replacement ranges. A text match carries source captures only."


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class MatchParams(ClosedModel):
    "A match query and page size for the session projection."

    query: Field[core.MatchQuery] = proto_field(
        description="What to look for, and which engine answers it.", number=1
    )
    limit: Field[int] = proto_field(
        default=50,
        description="Most matches to return in one page, capped by `max_page_items`.",
        ge=1,
        le=10000,
        number=2,
    )
    cursor: Field[Cursor | None] = proto_field(
        default=None,
        description="Continues a previous match where its last page ended. Omit it for the first page.",
        number=3,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class MatchResult(ClosedModel):
    "One page of matches. Matches sort by file bytes, range, and canonical key. A match key names the file it was found in, so Rift rechecks that file before applying an edit addressed at it."

    matches: Field[list[MatchHit]] = proto_field(
        description="The matches on this page.", number=2
    )
    coverage: Field[core.Coverage] = proto_field(
        description=(
            "How much of the selected path set was searched. A file too large to read makes "
            "this partial with a reason."
        ),
        number=3,
    )
    next_cursor: Field[Cursor | None] = proto_field(
        description="Pass this back to get the next page. Null on the last one.",
        number=4,
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Source",
        (
            EnumValue("adapter", "ADAPTER", 1),
            EnumValue("validator", "VALIDATOR", 2),
            EnumValue("apply", "APPLY", 3),
        ),
        placement=Placement("source", 1),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "adapter": "The language's own analysis.",
            "validator": "A check Rift ran over a proposed change.",
            "apply": "Reported while applying edits to the workspace.",
        }
    },
)
class DiagnosticContextSource(str, Enum):
    """Component that produced the diagnostic. Rift sets this after collecting adapter, validator, or apply output."""

    ADAPTER = "adapter"
    VALIDATOR = "validator"
    APPLY = "apply"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DiagnosticContext(ClosedModel):
    "One `Diagnostic` as an MCP answer carries it: the fact the adapter minted, plus what Rift can add without the adapter — where it lands in a line and column, and the source around it."

    source: Field[DiagnosticContextSource] = proto_field(
        description=(
            "Component that produced the diagnostic. Rift sets this after collecting adapter, "
            "validator, or apply output."
        ),
        number=1,
    )
    diagnostic: Field[core.Diagnostic] = proto_field(
        description="The finding itself, exactly as the adapter minted it.", number=3
    )
    line: Field[int | None] = proto_field(
        description=(
            "One-based line the finding starts on. Null where the diagnostic has no span — a "
            "whole-project error has nowhere to point."
        ),
        number=4,
    )
    column: Field[int | None] = proto_field(
        description="One-based column within that line, counted in UTF-8 bytes. Null for the same reason as `line`.",
        number=5,
    )
    excerpt: Field[SourceExcerpt | None] = proto_field(
        description="The source the finding points at. Null where there is no span to copy from.",
        number=6,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "ErrorCode",
        (
            EnumValue("invalid_request", "ERROR_CODE_INVALID_REQUEST", 1),
            EnumValue("unsupported_protocol", "ERROR_CODE_UNSUPPORTED_PROTOCOL", 2),
            EnumValue("permission_denied", "ERROR_CODE_PERMISSION_DENIED", 3),
            EnumValue("resource_not_found", "ERROR_CODE_RESOURCE_NOT_FOUND", 4),
            EnumValue("content_unavailable", "ERROR_CODE_CONTENT_UNAVAILABLE", 5),
            EnumValue("cursor_invalid", "ERROR_CODE_CURSOR_INVALID", 6),
            EnumValue("cancelled", "ERROR_CODE_CANCELLED", 7),
            EnumValue("deadline_exceeded", "ERROR_CODE_DEADLINE_EXCEEDED", 8),
            EnumValue("limit_exceeded", "ERROR_CODE_LIMIT_EXCEEDED", 9),
            EnumValue("projection_busy", "ERROR_CODE_PROJECTION_BUSY", 10),
            EnumValue("adapter_unavailable", "ERROR_CODE_ADAPTER_UNAVAILABLE", 11),
            EnumValue(
                "adapter_protocol_error", "ERROR_CODE_ADAPTER_PROTOCOL_ERROR", 12
            ),
            EnumValue("adapter_timeout", "ERROR_CODE_ADAPTER_TIMEOUT", 13),
            EnumValue("storage_failure", "ERROR_CODE_STORAGE_FAILURE", 14),
            EnumValue(
                "validator_execution_failure",
                "ERROR_CODE_VALIDATOR_EXECUTION_FAILURE",
                15,
            ),
            EnumValue("internal_error", "ERROR_CODE_INTERNAL_ERROR", 16),
            EnumValue("unsupported_path", "ERROR_CODE_UNSUPPORTED_PATH", 17),
            EnumValue("cursor_expired", "ERROR_CODE_CURSOR_EXPIRED", 18),
            EnumValue(
                "temporarily_unavailable",
                "ERROR_CODE_TEMPORARILY_UNAVAILABLE",
                19,
            ),
            EnumValue(
                "configuration_invalid",
                "ERROR_CODE_CONFIGURATION_INVALID",
                20,
            ),
            EnumValue(
                "capability_unavailable",
                "ERROR_CODE_CAPABILITY_UNAVAILABLE",
                22,
            ),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "invalid_request": (
                "The request does not satisfy the schema, or names something the schema forbids: "
                "a `limit` above the advertised maximum, a filter field that does not exist. The "
                "same bytes fail identically."
            ),
            "unsupported_protocol": (
                "The client and server use different protocol versions. Compare `Contract` with "
                "the client's supported contract."
            ),
            "permission_denied": (
                "The connection cannot perform this operation: a path leaves the workspace, or a "
                "debugging session belongs to another connection. The same request fails the "
                "same way."
            ),
            "resource_not_found": (
                "The identity is well-formed and resolves to nothing — no such symbol, no such "
                "filesystem entry, or a debug session already stopped or expired. Retrying "
                "does not help."
            ),
            "content_unavailable": (
                "The entry is known but its bytes cannot be read. Retrying does not help."
            ),
            "cursor_invalid": (
                "The cursor is malformed, or it was minted for a different request, order or page "
                "size. Start the query again from its first page."
            ),
            "cancelled": (
                "The caller cancelled, or the connection went away before the request finished. "
                "Sending it again is fine."
            ),
            "deadline_exceeded": (
                "The request ran past its time budget. A smaller one may succeed — a lower "
                "`limit`, a narrower path selector — and so may the same one once a cold adapter "
                "has warmed up."
            ),
            "limit_exceeded": (
                "A request or a response crossed an advertised limit. `limit` says which one and "
                "by how much, so the request can be resized and sent again."
            ),
            "projection_busy": (
                "The session has an in-flight Rift FS mutation that prevents this operation. "
                "Retry after that mutation completes."
            ),
            "adapter_unavailable": (
                "A configured adapter could serve this request, but its process failed to start or "
                "died. Retrying can succeed after the process restarts."
            ),
            "adapter_protocol_error": (
                "An adapter contract is unusable. Causes include a malformed message, a field "
                "outside its range, ambiguous source or virtual claims, overlapping write claims, "
                "a virtual path collision, and a cycle in virtual-source routing. Correct the "
                "adapter configuration before retrying."
            ),
            "adapter_timeout": (
                "The adapter took the call and did not answer inside its budget. Retrying can "
                "work: a cold adapter on a large workspace is slow once and fast afterwards."
            ),
            "storage_failure": (
                "Rift could not read or write workspace or projection files. Worth retrying only "
                "if the cause was transient, such as a disk that has since been cleared."
            ),
            "validator_execution_failure": (
                "Rift could not launch the command validator, enforce its timeout, or capture "
                "its output. One retry is reasonable when the host failure was transient."
            ),
            "internal_error": (
                "A bug in Rift. `causes` says what it was doing at the time, and a retry is not "
                "expected to answer differently."
            ),
            "unsupported_path": (
                "The workspace contains a path the protocol cannot represent safely."
            ),
            "cursor_expired": (
                "The cursor is valid, but its captured result page set left the process-local "
                "cache. Start again from the first page."
            ),
            "temporarily_unavailable": (
                "The resource exists but is in use. Another connection owns the session, a "
                "retained debugger blocks continuation, or a disposable workspace is still "
                "releasing. Retry after that owner or operation finishes."
            ),
            "configuration_invalid": (
                "The workspace-root `rift.toml` does not satisfy its schema. New requests remain "
                "blocked until the file is valid."
            ),
            "capability_unavailable": (
                "The tool exists, but the workspace configuration and configured adapters cannot "
                "serve this operation for the requested language. Read the workspace resource "
                "again after `rift.toml` or adapter availability changes."
            ),
        }
    },
)
class ErrorCode(str, Enum):
    "Why a request failed, as a stable code a caller branches on. The code is the complete classification. Domain results such as unsupported coverage and edit refusal use their typed result values."

    INVALID_REQUEST = "invalid_request"
    UNSUPPORTED_PROTOCOL = "unsupported_protocol"
    PERMISSION_DENIED = "permission_denied"
    RESOURCE_NOT_FOUND = "resource_not_found"
    CONTENT_UNAVAILABLE = "content_unavailable"
    CURSOR_INVALID = "cursor_invalid"
    CANCELLED = "cancelled"
    DEADLINE_EXCEEDED = "deadline_exceeded"
    LIMIT_EXCEEDED = "limit_exceeded"
    PROJECTION_BUSY = "projection_busy"
    ADAPTER_UNAVAILABLE = "adapter_unavailable"
    ADAPTER_PROTOCOL_ERROR = "adapter_protocol_error"
    ADAPTER_TIMEOUT = "adapter_timeout"
    STORAGE_FAILURE = "storage_failure"
    VALIDATOR_EXECUTION_FAILURE = "validator_execution_failure"
    INTERNAL_ERROR = "internal_error"
    UNSUPPORTED_PATH = "unsupported_path"
    CURSOR_EXPIRED = "cursor_expired"
    TEMPORARILY_UNAVAILABLE = "temporarily_unavailable"
    CONFIGURATION_INVALID = "configuration_invalid"
    CAPABILITY_UNAVAILABLE = "capability_unavailable"


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "RetryDirective",
        (
            EnumValue("never", "RETRY_DIRECTIVE_NEVER", 1),
            EnumValue("same_request", "RETRY_DIRECTIVE_SAME_REQUEST", 2),
            EnumValue("operator_action", "RETRY_DIRECTIVE_OPERATOR_ACTION", 4),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "never": "The request fails the same way every time. Change it or give up.",
            "same_request": (
                "Send the same bytes again. The cause was transient — a busy projection, an "
                "adapter still starting."
            ),
            "operator_action": (
                "A local state command or configuration change must resolve the condition before another "
                "request can succeed."
            ),
        }
    },
)
class RetryDirective(str, Enum):
    "Stable retry instruction for one failed request. `deadline_exceeded` can permit the same request; `invalid_request` requires changed input."

    NEVER = "never"
    SAME_REQUEST = "same_request"
    OPERATOR_ACTION = "operator_action"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ErrorCause(ClosedModel):
    """One cause in a failure chain. Entries appear from the outer operation to the concrete failure."""

    code: Field[ErrorCode] = proto_field(
        description="How this link classifies on its own.", number=1
    )
    message: Field[str] = proto_field(
        description="What happened, for a human reading a log.",
        max_length=4096,
        number=2,
    )
    retry: Field[RetryDirective] = proto_field(
        description="What could be done about this cause. The request's outer directive governs the call.",
        number=3,
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Phase",
        (
            EnumValue("discovery", "DISCOVERY", 1),
            EnumValue("read", "READ", 2),
            EnumValue("resolve", "RESOLVE", 3),
            EnumValue("validate", "VALIDATE", 4),
            EnumValue("change", "CHANGE", 5),
            EnumValue("publish", "PUBLISH", 6),
            EnumValue("execute", "EXECUTE", 7),
            EnumValue("debug", "DEBUG", 8),
        ),
        placement=Placement("phase", 4),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "discovery": "Working out what the workspace can do: capabilities, limits, which languages have adapters.",
            "read": "Reading workspace or adapter data.",
            "resolve": "Turning an address, a cursor or an action key into the concrete thing it names at a state.",
            "validate": (
                "Checking a proposed change against the schema, the state it was pinned to, and "
                "the checks required by the workspace-root `rift.toml`."
            ),
            "change": "Resolving the operation, writing the result into the projection, and validating the resulting tree.",
            "publish": "Publishing projection changes to the workspace.",
            "execute": "Preparing an execution workspace and evaluating caller-provided code.",
            "debug": "Starting, inspecting, or stopping a connection-bound debugging session.",
        }
    },
)
class ErrorDataPhase(str, Enum):
    "How far the request got before it failed. The same code means different things at different phases: `limit_exceeded` while reading is a response too big, and while validating it is a change set too large."

    DISCOVERY = "discovery"
    READ = "read"
    RESOLVE = "resolve"
    VALIDATE = "validate"
    CHANGE = "change"
    PUBLISH = "publish"
    EXECUTE = "execute"
    DEBUG = "debug"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ErrorData(ClosedModel):
    "The `data` object on every Rift MCP failure. `code` and `retry` are what a caller branches on, `message` is for a human, and the remaining fields carry the evidence available for that failure."

    model_config = closed_config(
        {
            "allOf": [
                {
                    "if": {
                        "required": ["code"],
                        "properties": {"code": {"const": "limit_exceeded"}},
                    },
                    "then": {"required": ["limit"]},
                    "else": {"not": {"required": ["limit"]}},
                },
            ]
        }
    )
    code: Field[ErrorCode] = proto_field(
        description="Why the request failed.", number=1
    )
    message: Field[str] = proto_field(
        description=(
            "Human-readable account of the failure. Machine-readable classification remains "
            "in the surrounding fields."
        ),
        max_length=4096,
        number=2,
    )
    retry: Field[RetryDirective] = proto_field(
        description="What the caller may do next.", number=3
    )
    phase: Field[ErrorDataPhase] = proto_field(
        description=(
            "How far the request got before it failed. The same code means different things "
            "at different phases: `limit_exceeded` while reading is a response too big, and "
            "while validating it is a change set too large."
        ),
        number=4,
    )
    diagnostics: Field[list[DiagnosticContext]] = proto_field(
        description=(
            "What an adapter or configured command check reported while the request was "
            "failing. Empty where neither ran."
        ),
        number=7,
    )
    limit: Field[LimitEvidence | None] = proto_field(
        default=None,
        description=(
            "Which advertised limit was hit, and by how much. Present exactly when `code` is "
            "`limit_exceeded`, and forbidden otherwise."
        ),
        number=8,
    )
    causes: Field[list[ErrorCause]] = proto_field(
        description=(
            "What led to this failure, outermost first. A code alone rarely says whether the "
            "cause is worth waiting out."
        ),
        number=9,
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Scope",
        (EnumValue("driver", "DRIVER", 1), EnumValue("adapter", "ADAPTER", 2)),
        placement=Placement("scope", 1),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "driver": "A field of `Limits`, advertised by the workspace resource.",
            "adapter": "A field of the adapter's `AdapterLimits`, advertised in `Describe`.",
        }
    },
)
class LimitEvidenceScope(str, Enum):
    "Which side of the server the limit belongs to. The two fail at different seams: a `driver` limit is enforced by Rift around the request, while an `adapter` limit is enforced inside one adapter process."

    DRIVER = "driver"
    ADAPTER = "adapter"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class LimitEvidence(ClosedModel):
    "Which advertised limit a `limit_exceeded` failure hit, and by how much. It is present exactly when the code is `limit_exceeded`; without it, choosing between retrying smaller, falling back to another resource and giving up means reparsing the human message."

    scope: Field[LimitEvidenceScope] = proto_field(
        description=(
            "Which side of the server the limit belongs to. The two fail at different seams: "
            "a `driver` limit is enforced by Rift around the request, while an `adapter` limit "
            "is enforced inside one adapter process."
        ),
        number=1,
    )
    field: Field[str] = proto_field(
        description=(
            "The limit's field path below whichever message `scope` names, such as "
            "`max_page_items`, `execution.max_concurrent`, or `max_in_flight_per_state`."
        ),
        min_length=1,
        max_length=128,
        number=2,
    )
    limit: Field[int] = proto_field(
        description="The value in force when the request was rejected.",
        ge=0,
        le=9007199254740991,
        number=3,
    )
    required: Field[int] = proto_field(
        description=(
            "What the request would have needed. Larger than `limit`, and the difference is "
            "what the caller has to close."
        ),
        ge=0,
        le=9007199254740991,
        number=4,
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.message(placement=Placement("text", 1)),
    schema_extra={},
)
class MatchSyntaxText(ClosedModel):
    """The grammar a `TextQuery` pattern is read in."""

    name: Field[Literal["rift-regex"]] = proto_field(number=1)
    version: Field[Literal[1]] = proto_field(number=2)


@definition(
    owner=MCP,
    public=False,
    proto=Proto.message(placement=Placement("path", 2)),
    schema_extra={},
)
class MatchSyntaxPath(ClosedModel):
    """The grammar used by path selectors."""

    name: Field[Literal["path-glob"]] = proto_field(number=1)
    version: Field[Literal[1]] = proto_field(number=2)


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class MatchSyntax(ClosedModel):
    "The two pattern grammars this workspace accepts: one for text patterns and one for path globs. Names remain stable; their version fields select syntax and matching semantics."

    text: Field[MatchSyntaxText] = proto_field(
        description="The grammar a `TextQuery` pattern is read in.", number=1
    )
    path: Field[MatchSyntaxPath] = proto_field(
        description="The grammar used by path selectors.",
        number=2,
    )


RESOURCE_FORMS: dict[ResourceFamily, tuple[str, str, type[Any]]] = {
    ResourceFamily.WORKSPACE: (
        "rift://workspace{?cursor}",
        "application/vnd.rift.workspace+json",
        WorkspaceResourceUri,
    ),
    ResourceFamily.SYMBOL: (
        "rift://symbol/{language}/{name}{?cursor}",
        "application/vnd.rift.symbol+json",
        SymbolResourceUri,
    ),
    ResourceFamily.FS: (
        "rift://fs{/path}{?start,length,cursor}",
        "application/vnd.rift.fs+json",
        FsResourceUri,
    ),
    ResourceFamily.ACTIONS: (
        "rift://actions/{address}{?only,cursor}",
        "application/vnd.rift.actions+json",
        ActionsResourceUri,
    ),
    ResourceFamily.ACTION: (
        "rift://action/{token}",
        "application/vnd.rift.action+json",
        ActionResourceUri,
    ),
    ResourceFamily.PROJECTION: (
        "rift://projection",
        "application/vnd.rift.projection+json",
        ProjectionResourceUri,
    ),
    ResourceFamily.CHANGES: (
        "rift://changes{?cursor}",
        "application/vnd.rift.changes+json",
        ChangesResourceUri,
    ),
}

RESOURCE_MEDIA_TYPE_PATTERN = r"^application/vnd\.rift\.[a-z]+\+json$"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ResourceLink(ClosedModel):
    "A link to one Rift resource, as MCP carries it inside tool output. `name` selects the family, which fixes the URI shape a read of `uri` accepts and the media type it returns."

    type: Field[Literal["resource_link"]] = proto_field(
        description="MCP's tag for a link to a resource inside tool output."
    )
    uri: Field[str] = proto_field(
        description="The resource to read. Hand it to `resources/read` unchanged.",
        min_length=1,
        max_length=32768,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
    )
    name: Field[ResourceFamily] = proto_field(
        description="The resource family this link belongs to.", number=2
    )
    mimeType: Field[str] = proto_field(
        description="What a read of `uri` returns, as `application/vnd.rift.<family>+json`.",
        pattern=RESOURCE_MEDIA_TYPE_PATTERN,
        max_length=64,
        number=3,
        proto_name="mime_type",
    )

    @model_validator(mode="after")
    def link_is_correlated(self) -> ResourceLink:
        _template, media_type, uri_model = RESOURCE_FORMS[self.name]
        if self.mimeType != media_type:
            raise ValueError("resource link media type must match its family")
        uri_model.model_validate(self.uri)
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ResourceTemplate(ClosedModel):
    "One advertised MCP resource template, as `resources/templates/list` returns it. The family fixes the template and the media type."

    uriTemplate: Field[str] = proto_field(
        description="The template, in RFC 6570 form. What follows `?` is optional.",
        min_length=1,
        max_length=256,
    )
    name: Field[ResourceFamily] = proto_field(
        description="The resource family being advertised.", number=1
    )
    mimeType: Field[str] = proto_field(
        description="What a read of a URI from this template returns, as `application/vnd.rift.<family>+json`.",
        pattern=RESOURCE_MEDIA_TYPE_PATTERN,
        max_length=64,
        number=2,
        proto_name="mime_type",
    )

    @model_validator(mode="after")
    def template_is_correlated(self) -> ResourceTemplate:
        template, media_type, _uri_model = RESOURCE_FORMS[self.name]
        if self.uriTemplate != template or self.mimeType != media_type:
            raise ValueError("resource template must match its family")
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ResourceReadParams(ClosedModel):
    """The URI passed to MCP `resources/read`. Each branch is one advertised Rift resource family."""

    uri: Field[
        WorkspaceResourceUri
        | SymbolResourceUri
        | FsResourceUri
        | ActionsResourceUri
        | ActionResourceUri
        | ProjectionResourceUri
        | ChangesResourceUri
    ] = proto_field(
        description="A URI matching one advertised resource family.",
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={
        "rift:contentTypes": {
            "application/vnd.rift.workspace+json": "WorkspaceResourcePayload",
            "application/vnd.rift.symbol+json": "SymbolResourcePayload",
            "application/vnd.rift.fs+json": "FsResourcePayload",
            "application/vnd.rift.actions+json": "ActionsResourcePayload",
            "application/vnd.rift.action+json": "ActionResourcePayload",
            "application/vnd.rift.projection+json": "ProjectionResourcePayload",
            "application/vnd.rift.changes+json": "ChangesResourcePayload",
        }
    },
)
class ResourceContent(ClosedModel):
    "One content block of an MCP resource read. `text` holds the family's payload as JSON — `WorkspaceResourcePayload` for `rift://workspace`, `SymbolResourcePayload` for a symbol URI, and so on — and `mimeType` names which. The `rift:contentTypes` map in the schema carries the complete pairing."

    uri: Field[str] = proto_field(
        description="The URI that was read, as it resolved.",
        min_length=1,
        max_length=32768,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
    )
    mimeType: Field[str] = proto_field(
        description="Which payload `text` holds, as `application/vnd.rift.<family>+json`.",
        pattern=RESOURCE_MEDIA_TYPE_PATTERN,
        max_length=64,
        number=3,
        proto_name="mime_type",
    )
    text: Field[str] = proto_field(
        description="The payload, serialized as JSON.",
        number=2,
    )

    @model_validator(mode="after")
    def content_is_correlated(self) -> ResourceContent:
        for _template, media_type, uri_model in RESOURCE_FORMS.values():
            if media_type == self.mimeType:
                uri_model.model_validate(self.uri)
                return self
        raise ValueError("resource content names an unknown media type")


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ResourceReadResult(ClosedModel):
    "The result of one MCP resource read. Each Rift resource returns one JSON payload in `text`, identified by `mimeType`. File bytes use UTF-8 or base64."

    contents: Field[list[ResourceContent]] = proto_field(
        description="The blocks this read produced. MCP allows several per read; each Rift resource returns one.",
        min_length=1,
        number=1,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class SearchHitTargetSymbol(ClosedModel):
    """A symbol hit: the declaration the adapter resolved, and a link to read the rest of what Rift knows about it."""

    target: Field[Literal["symbol"]] = proto_field(
        description="Tags this as a symbol hit."
    )
    symbol: Field[core.Symbol] = proto_field(
        description="The declaration that matched.", number=1
    )
    resource: Field[ResourceLink] = proto_field(
        description="Link to the symbol resource that carries this symbol's nodes and relationships.",
        number=2,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class SearchHitTargetNode(ClosedModel):
    """A node hit: one place in a syntax tree, without its enclosing symbol record."""

    target: Field[Literal["node"]] = proto_field(description="Tags this as a node hit.")
    node: Field[core.Node] = proto_field(
        description="The syntax-tree node that matched, and the symbol written at it where there is one.",
        number=1,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class SearchHitTargetFile(ClosedModel):
    """A file hit: one entry of the workspace tree. The only hit that needs no adapter."""

    target: Field[Literal["file"]] = proto_field(description="Tags this as a file hit.")
    file: Field[core.File] = proto_field(
        description="The tree entry that matched: what it holds, and which languages read it.",
        number=1,
    )


@union(
    owner=MCP,
    oneof="variant",
    discriminator="target",
    variants=(
        Variant("symbol", "symbol", 1, SearchHitTargetSymbol),
        Variant("node", "node", 2, SearchHitTargetNode),
        Variant("file", "file", 3, SearchHitTargetFile),
    ),
)
class SearchHitTarget(ProtocolRoot):
    """What a search hit is. Tagged, so the payload correlation survives code generation."""


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class EditParams(ClosedModel):
    "Concrete filesystem edits supplied by the caller. Their ranges address the state this operation resolves against, and replacements in one set may not overlap."

    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after this operation's edits resolve.", number=3
    )
    edits: Field[list[core.Edit]] = proto_field(
        description=(
            "An atomic effect set in canonical file-and-range order. Every text replacement "
            "addresses the state before this operation."
        ),
        min_length=1,
        number=4,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class PatchParams(ClosedModel):
    "A UTF-8 unified diff guarded by its context lines. Rift refuses absolute paths, path traversal, binary patches, malformed headers, and any hunk whose context differs from the state it resolves against."

    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after this operation's edits resolve.", number=3
    )
    patch: Field[str] = proto_field(
        description="Unified text diff with project-relative `a/` and `b/` paths.",
        min_length=1,
        number=4,
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Range",
        (
            EnumValue("exact", "EXACT", 1),
            EnumValue("leading", "LEADING", 2),
            EnumValue("trailing", "TRAILING", 3),
            EnumValue("both", "BOTH", 4),
        ),
        placement=Placement("range", 6),
    ),
    schema_extra={},
)
class RewriteRange(str, Enum):
    "Which safe structural range is replaced. Text queries accept `exact` only because they have no grammar-owned trivia boundaries."

    EXACT = "exact"
    LEADING = "leading"
    TRAILING = "trailing"
    BOTH = "both"


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class RewriteParams(ClosedModel):
    "An atomic query-and-rewrite. Rift finds every match, checks the cardinality, expands the replacement, and either applies all resulting edits or refuses the operation."

    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after this operation's edits resolve.", number=3
    )
    query: Field[core.MatchQuery] = proto_field(
        description="The text or structural pattern evaluated against the state this operation resolves against.",
        number=4,
    )
    replacement: Field[str] = proto_field(
        description=(
            "UTF-8 replacement template. `${NAME}` inserts the source bound by a named or "
            "numeric capture, `${0}` inserts the whole match, and `$$` inserts one dollar "
            "sign. An absent capture refuses the rewrite."
        ),
        number=5,
    )
    range: Field[RewriteRange] = proto_field(
        description=(
            "Which safe structural range is replaced. Text queries accept `exact` only "
            "because they have no grammar-owned trivia boundaries."
        ),
        number=6,
    )
    cardinality: Field[core.MatchCardinality] = proto_field(
        description="The accepted number of matches before expansion.", number=7
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class RenameParams(ClosedModel):
    "Changes what a declaration is called and rewrites the references that name it. The adapter checks language spelling, collisions, and binding changes; a reference outside `scope` refuses the operation rather than leaving it half done."

    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after this operation's edits resolve.", number=3
    )
    target: Field[core.Address] = proto_field(
        description="The declaration to rename: a symbol, a node, a byte range, or a match.",
        number=4,
    )
    arguments: Field[core.RenameArguments] = proto_field(
        description="The new name, and the source eligible for propagation.", number=5
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class MoveParams(ClosedModel):
    "Moves a declaration or file to another container or path and updates the imports and references that reach it."

    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after this operation's edits resolve.", number=3
    )
    target: Field[core.Address] = proto_field(
        description="The declaration or file to move.", number=4
    )
    arguments: Field[core.MoveArguments] = proto_field(
        description="The destination, and the source eligible for import and reference updates.",
        number=5,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class DeleteParams(ClosedModel):
    "Removes a declaration. Without a policy this is a mechanical removal that analyses no references and claims no reference guarantee. With one, the adapter classifies every remaining use, applies the stated disposition, and refuses the operation when reference coverage is incomplete."

    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after this operation's edits resolve.", number=3
    )
    target: Field[core.Address] = proto_field(
        description="The declaration or file to remove.", number=4
    )
    arguments: Field[core.SafeDeleteArguments] = proto_field(
        description="The disposition for each classified use, and the source it may be applied in.",
        number=5,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class ChangeSignatureParams(ClosedModel):
    "Changes the shape of a callable and propagates it. Unlike a rename, this rewrites argument lists: a new required parameter has to be supplied at every call site, which is why the operation commonly raises a `behavior_unknown` confirmation."

    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after this operation's edits resolve.", number=3
    )
    target: Field[core.Address] = proto_field(
        description="The callable whose signature changes.", number=4
    )
    arguments: Field[core.ChangeSignatureArguments] = proto_field(
        description="The desired callable shape, its propagation, and the source it may reach.",
        number=5,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class ActParams(ClosedModel):
    "Resolves one discovered adapter action — a quick fix, an extraction, an inline, anything an adapter offers that has no portable argument contract. Rift validates `arguments` against the offer's advertised schema. An offer carrying a portable argument contract is refused here, because `rename`, `move`, `delete`, and `change_signature` are its typed entry points."

    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after this operation's edits resolve.", number=3
    )
    action: Field[core.ActionOfferId] = proto_field(
        description="The offer to resolve, as the actions resource returned it.",
        number=4,
    )
    arguments: Field[dict[str, Any]] = proto_field(
        description=(
            "Arguments accepted by the offer's `ActionDescriptor.arguments_schema`. An action "
            "with no parameters receives an empty object."
        ),
        number=5,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionRestoreParams(ClosedModel):
    """Restores changed paths from the current workspace."""

    paths: Field[list[core.ProjectPath] | None] = proto_field(
        default=None,
        description="Changed paths to restore. Null restores every changed path.",
        min_length=1,
        number=2,
        json_schema_extra={"uniqueItems": True},
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Kind",
        (
            EnumValue("test", "KIND_TEST", 1),
            EnumValue("lint", "KIND_LINT", 2),
            EnumValue("build", "KIND_BUILD", 3),
            EnumValue("other", "KIND_OTHER", 4),
        ),
        placement=Placement("kind", 2),
    ),
    schema_extra={},
)
class CommandValidatorKind(str, Enum):
    """How workspace configuration presents this check."""

    TEST = "test"
    LINT = "lint"
    BUILD = "build"
    OTHER = "other"


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "ChangedPaths",
        (
            EnumValue("none", "CHANGED_PATHS_NONE", 1),
            EnumValue("append", "CHANGED_PATHS_APPEND", 2),
        ),
        placement=Placement("changed_paths", 4),
    ),
    schema_extra={},
)
class CommandValidatorChangedPaths(str, Enum):
    """Whether Rift appends changed `ProjectPath` values to `argv` in byte order."""

    NONE = "none"
    APPEND = "append"


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class CommandValidatorGuarantees(ClosedModel):
    "What a passing run of one configured command establishes."

    kind: Field[core.GuaranteeKind] = proto_field(
        description="Guarantee established when the validator passes.", number=1
    )
    scope: Field[core.CoverageScope] = proto_field(
        description="Source over which the command checks the property.", number=2
    )
    detail: Field[str] = proto_field(
        description="Exact property the command checks and limits on interpreting a pass.",
        min_length=1,
        max_length=4096,
        number=3,
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Determinism",
        (
            EnumValue("deterministic", "DETERMINISM_DETERMINISTIC", 1),
            EnumValue("best_effort", "DETERMINISM_BEST_EFFORT", 2),
        ),
        placement=Placement("determinism", 10),
    ),
    schema_extra={},
)
class CommandValidatorDeterminism(str, Enum):
    """Whether an identical tree and environment are expected to produce the same result."""

    DETERMINISTIC = "deterministic"
    BEST_EFFORT = "best_effort"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class CommandValidator(ClosedModel):
    "A command from the workspace-root `rift.toml`, run without a shell inside the session's projection directory. Whatever it writes there joins the changeset."

    id: Field[str] = proto_field(
        description=(
            "Label shown with this validator. It is unique within `validators.commands`."
        ),
        pattern="^[A-Za-z][A-Za-z0-9_.-]{0,127}$",
        number=1,
    )
    kind: Field[CommandValidatorKind] = proto_field(
        description="How workspace configuration presents this check.", number=2
    )
    argv: Field[list[str]] = proto_field(
        description=(
            "Executable followed by literal arguments. An absolute executable path is "
            "refused; a bare name resolves through the validator PATH and a relative path "
            "resolves below `working_directory`. Rift performs no shell expansion."
        ),
        min_length=1,
        number=3,
    )
    changed_paths: Field[CommandValidatorChangedPaths] = proto_field(
        description="Whether Rift appends changed `ProjectPath` values to `argv` in byte order.",
        number=4,
    )
    working_directory: Field[core.ProjectPath] = proto_field(
        description="Directory below the project root in which the process starts. The empty path selects the root.",
        number=5,
    )
    environment: Field[dict[str, str]] = proto_field(
        description=(
            "Environment additions declared for the command. Rift starts it directly without a "
            "shell and supplies its working directory separately."
        ),
        number=6,
        json_schema_extra={"propertyNames": {"pattern": "^[A-Za-z_][A-Za-z0-9_]*$"}},
    )
    timeout_ms: Field[int] = proto_field(
        description="Wall-clock limit before Rift terminates the process.",
        ge=1,
        le=3600000,
        number=7,
    )
    output_limit_bytes: Field[int] = proto_field(
        description=(
            "Captured prefix limit for each output stream. `CapturedText.total_bytes` "
            "reports the omitted size. The upper bound keeps one escaped validator result "
            "inside one bounded result."
        ),
        ge=256,
        le=4096,
        number=8,
    )
    guarantees: Field[list[CommandValidatorGuarantees]] = proto_field(
        description=(
            "Behavior or other properties this command is intended to check. A passing result "
            "turns each declaration into `GuaranteeEvidence`; a failed result rejects publication."
        ),
        number=9,
    )
    determinism: Field[CommandValidatorDeterminism] = proto_field(
        description="Whether an identical tree and environment are expected to produce the same result.",
        number=10,
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Strength",
        (
            EnumValue("mechanical", "STRENGTH_MECHANICAL", 1),
            EnumValue("available", "STRENGTH_AVAILABLE", 2),
            EnumValue("required", "STRENGTH_REQUIRED", 3),
        ),
        placement=Placement("strength", 3),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "mechanical": "No affected adapter supplied a semantic validation report.",
            "available": "Every affected adapter that advertises validation supplied a complete report.",
            "required": "Every language named by `validation.require` supplied a complete report.",
        }
    },
)
class ValidationStrength(str, Enum):
    """How much semantic validation the result contains."""

    MECHANICAL = "mechanical"
    AVAILABLE = "available"
    REQUIRED = "required"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ChangeValidation(ClosedModel):
    """Bounded adapter-validation evidence for one applied session change."""

    strength: Field[ValidationStrength] = proto_field(
        description="Semantic validation strength established for the resulting tree.",
        number=3,
    )
    adapter_reports: Field[list[core.ValidationReport]] = proto_field(
        description="Adapter reports in language order.", number=4
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ChangeSummary(ClosedModel):
    """One change in the projection's changeset, with everything Rift learned while resolving it.
    A change carrying confirmations is recorded the same way as one carrying none; the
    confirmations are what publication checks."""

    id: Field[ChangeId] = proto_field(
        description="Identity of this change in the changeset.", number=1
    )
    validation: Field[ChangeValidation] = proto_field(
        description="Adapter validation of the resulting projection.", number=3
    )
    edits: Field[list[core.Edit]] = proto_field(
        description="Concrete edits in canonical file-and-range order.", number=4
    )
    effects: Field[list[core.OperationEffect]] = proto_field(
        description="Semantic effects in adapter emission order.", number=6
    )
    guarantees: Field[list[core.GuaranteeEvidence]] = proto_field(
        description="Scoped guarantee evidence in guarantee-kind order.", number=7
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        description="Resolution findings in source order.", number=8
    )
    confirmations: Field[list[core.ConfirmationRequirement]] = proto_field(
        description=(
            "Effects the caller has to accept before this change can be published, sorted by "
            "kind, source location, title, and detail. Empty where every affected adapter and "
            "validator vouched for the result."
        ),
        number=10,
    )
    current: Field[core.ProjectionState] = proto_field(
        description="Projection state after this change landed.",
        number=9,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ChangesResourcePayload(ClosedModel):
    """One page of the session's changeset, oldest change first. This is the read an agent makes
    before publishing: every change it applied, what each one did, and which of them nobody
    vouched for."""

    uri: Field[ChangesResourceUri] = proto_field(
        description="The URI this payload answers for.", number=1
    )
    state: Field[core.ProjectionState] = proto_field(
        description="Projection state the page was read from.", number=2
    )
    changes: Field[list[ChangeSummary]] = proto_field(
        description="Changes on this page, in the order they were applied.", number=3
    )
    next: Field[ChangesResourceUri | None] = proto_field(
        default=None,
        description="The same resource carrying the cursor for the next page, or null on the last one.",
        number=4,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ChangeApplied(ClosedModel):
    """The operation resolved to edits and Rift wrote them into the projection."""

    status: Field[Literal["applied"]] = proto_field(
        description="Identifies an applied store change.", default="applied"
    )
    summary: Field[ChangeSummary] = proto_field(
        description="The applied change and its evidence.", number=1
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "RefusalReason",
        (
            EnumValue("unsupported", "REFUSAL_REASON_UNSUPPORTED", 1),
            EnumValue("unmet_precondition", "REFUSAL_REASON_UNMET_PRECONDITION", 2),
            EnumValue("ambiguous_target", "REFUSAL_REASON_AMBIGUOUS_TARGET", 3),
            EnumValue("stale_action", "REFUSAL_REASON_STALE_ACTION", 4),
            EnumValue("stale_match", "REFUSAL_REASON_STALE_MATCH", 5),
            EnumValue("cardinality_mismatch", "REFUSAL_REASON_CARDINALITY_MISMATCH", 6),
            EnumValue("unsafe_effect", "REFUSAL_REASON_UNSAFE_EFFECT", 8),
            EnumValue(
                "formatter_unsupported", "REFUSAL_REASON_FORMATTER_UNSUPPORTED", 9
            ),
            EnumValue("portable_family", "REFUSAL_REASON_PORTABLE_FAMILY", 11),
            EnumValue("language_refusal", "REFUSAL_REASON_LANGUAGE_REFUSAL", 12),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "unsupported": "No adapter implements this operation for the language it reaches.",
            "unmet_precondition": "A condition checked before resolution failed. The failed entry is in `preconditions`.",
            "ambiguous_target": "The address resolves to several targets. Narrow it and ask again.",
            "stale_action": "The file the offer was discovered in has been rewritten since. Read the actions resource again.",
            "stale_match": "The file the match was found in has been rewritten since. Search again.",
            "cardinality_mismatch": "A rewrite matched fewer or more times than its cardinality accepts.",
            "unsafe_effect": "The complete effect reaches outside what the caller can have meant — outside the project, or into generated source.",
            "formatter_unsupported": "The requested formatting policy has no formatter behind it for an affected language.",
            "portable_family": "The offer carries a portable argument contract, so it resolves through `rename`, `move`, `delete`, or `change_signature` rather than through `act`.",
            "language_refusal": "The language itself forbids it — a rename to a reserved word, a visibility change its rules do not allow.",
        }
    },
)
class RefusalReason(str, Enum):
    "Why Rift produced no edits at all. Every reason here means resolution had nothing to write: a change Rift can express but nobody will vouch for still lands, carrying its confirmations. `ErrorData` carries transport and infrastructure failures."

    UNSUPPORTED = "unsupported"
    UNMET_PRECONDITION = "unmet_precondition"
    AMBIGUOUS_TARGET = "ambiguous_target"
    STALE_ACTION = "stale_action"
    STALE_MATCH = "stale_match"
    CARDINALITY_MISMATCH = "cardinality_mismatch"
    UNSAFE_EFFECT = "unsafe_effect"
    FORMATTER_UNSUPPORTED = "formatter_unsupported"
    PORTABLE_FAMILY = "portable_family"
    LANGUAGE_REFUSAL = "language_refusal"


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RefusedResult(ClosedModel):
    "Resolution produced no edits, so the projection is untouched."

    status: Field[Literal["refused"]] = proto_field(
        description="Identifies a domain refusal.", default="refused"
    )
    reason: Field[RefusalReason] = proto_field(
        description="The condition the caller can act on.", number=2
    )
    preconditions: Field[list[core.OperationPrecondition]] = proto_field(
        description=(
            "Conditions checked before refusal, including at least one failed entry for "
            "`unmet_precondition`."
        ),
        number=3,
    )
    blockers: Field[list[core.OperationBlocker]] = proto_field(
        description="Existing code, paths, or relationships that prevented a deterministic resolution.",
        number=4,
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        description="Evidence that explains the refusal.", number=5
    )


@union(
    owner=MCP,
    oneof="variant",
    discriminator="status",
    variants=(
        Variant("applied", "applied", 1, ChangeApplied),
        Variant("refused", "refused", 2, RefusedResult),
    ),
)
class ChangeResult(ProtocolRoot):
    """An applied projection change or semantic refusal."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PublishParams(ClosedModel):
    """Publishes the session projection into the workspace. Publication is the one step that
    touches the user's own files, so it is where confirmations are settled: a change carrying
    one is published only when this call names it."""

    accept: Field[list[ChangeId]] = proto_field(
        default_factory=list,
        description=(
            "Changes whose confirmations the caller accepts. A change carrying a confirmation "
            "and absent here is returned in `unaccepted` and nothing is written."
        ),
        number=1,
        json_schema_extra={"uniqueItems": True},
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PublishResult(ClosedModel):
    """The result of publishing projection changes into the workspace."""

    state: Field[core.ProjectionState] = proto_field(
        description="Current projection state after the attempt.", number=2
    )
    conflicts: Field[list[core.ProjectPath]] = proto_field(
        description="Workspace paths changed outside this projection.", number=3
    )
    unaccepted: Field[list[ChangeId]] = proto_field(
        description=(
            "Changes carrying a confirmation that `accept` did not name, sorted by change "
            "identity. Non-empty means nothing was written."
        ),
        number=4,
        json_schema_extra={"uniqueItems": True},
    )

    @model_validator(mode="after")
    def result_is_correlated(self) -> PublishResult:
        if (self.conflicts or self.unaccepted) and not self.state.dirty:
            raise ValueError("a refused publication leaves the projection dirty")
        if not (self.conflicts or self.unaccepted) and self.state.dirty:
            raise ValueError("a successful publication leaves the projection clean")
        return self


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "ResultOrder",
        (
            EnumValue("relevance", "RESULT_ORDER_RELEVANCE", 1),
            EnumValue("path", "RESULT_ORDER_PATH", 2),
            EnumValue("identity", "RESULT_ORDER_IDENTITY", 3),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "relevance": (
                "Highest `score` first, then identity. Scores are comparable across every page of "
                "one request and nowhere else."
            ),
            "path": (
                "By the file a result is written in, then its byte range, then identity. Two hits "
                "in one file come back in source order."
            ),
            "identity": (
                "By the result's canonical identity: a symbol URI, a file path, or an "
                "`ActionOfferId`. Adapter actions use this order because they carry no relevance "
                "score or common source path."
            ),
        }
    },
)
class ResultOrder(str, Enum):
    "The total order a paginated answer comes back in, named in the request so a cursor can be bound to it. Every order ends in the result's own identity, so two results that tie never swap places between pages."

    RELEVANCE = "relevance"
    PATH = "path"
    IDENTITY = "identity"


def _fs_resource_path(uri: FsResourceUri | core.FileId) -> str:
    if isinstance(uri, core.FileId):
        return uri.root.removeprefix("rift://file/")
    return uri.root.partition("?")[0].removeprefix("rift://fs").removeprefix("/")


def _fs_resource_query(uri: FsResourceUri) -> dict[str, list[str]]:
    query = uri.root.partition("?")[2]
    if not query:
        return {}
    return parse_qs(query, keep_blank_values=True, strict_parsing=True)


def _validate_file_identity(uri: FsResourceUri, file: core.File) -> None:
    if _fs_resource_path(uri) != _fs_resource_path(file.id):
        raise ValueError("filesystem payload URI and entry must name the same path")


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Encoding",
        (
            EnumValue("directory", "ENCODING_DIRECTORY", 1),
            EnumValue("utf8", "ENCODING_UTF8", 2),
            EnumValue("base64", "ENCODING_BASE64", 3),
            EnumValue("none", "ENCODING_NONE", 4),
        ),
        placement=Placement("encoding", 4),
    ),
    schema_extra={},
)
class FsResourceEncoding(str, Enum):
    """How a filesystem resource entry is represented."""

    DIRECTORY = "directory"
    UTF8 = "utf8"
    BASE64 = "base64"
    NONE = "none"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class FsResourcePayload(ClosedModel):
    """One filesystem entry from the session projection."""

    uri: Field[FsResourceUri] = proto_field(
        description="Exact resource request this payload answers.", number=1
    )
    entry: Field[core.ProjectEntry] = proto_field(
        description="Directory, regular file, or symbolic link named by the URI.",
        number=3,
    )
    encoding: Field[FsResourceEncoding] = proto_field(
        description="How this entry's payload is represented.", number=4
    )
    entries: Field[list[core.ProjectEntry] | None] = proto_field(
        default=None,
        description=(
            "Directory children on this page — subdirectories and files alike, each one a "
            "complete `ProjectEntry`."
        ),
        number=5,
    )
    start: Field[int | None] = proto_field(
        default=None, ge=0, le=9007199254740991, number=6
    )
    end: Field[int | None] = proto_field(
        default=None, ge=0, le=9007199254740991, number=7
    )
    total_bytes: Field[int | None] = proto_field(
        default=None, ge=0, le=9007199254740991, number=8
    )
    content: Field[str | None] = proto_field(default=None, number=9)
    next: Field[FsResourceUri | None] = proto_field(
        default=None, description="Continuation for the same entry.", number=10
    )

    @model_validator(mode="after")
    def payload_is_correlated(self) -> FsResourcePayload:
        entry = self.entry.root
        query = _fs_resource_query(self.uri)
        if entry.kind == "directory":
            encoded = quote(entry.path.root, safe="/!$&'()*+,;=:@-._~")
            if _fs_resource_path(self.uri) != encoded:
                raise ValueError(
                    "filesystem payload URI and directory must name the same path"
                )
            if self.encoding != "directory" or self.entries is None:
                raise ValueError(
                    "directory payload requires directory encoding and entries"
                )
            if any(
                value is not None
                for value in (self.start, self.end, self.total_bytes, self.content)
            ):
                raise ValueError("directory payload cannot contain file content")
            if "start" in query:
                raise ValueError("directory reads cannot use a byte range")
            if self.next is not None and "cursor" not in _fs_resource_query(self.next):
                raise ValueError("directory continuation requires a cursor")
        else:
            file = entry.file
            _validate_file_identity(self.uri, file)
            if self.entries is not None or "cursor" in query:
                raise ValueError("file payload cannot contain directory entries")
            content = file.content.root
            if content.kind == "symlink":
                if query:
                    raise ValueError("symbolic-link reads cannot use a range or cursor")
                if self.encoding != "none" or any(
                    value is not None
                    for value in (
                        self.start,
                        self.end,
                        self.total_bytes,
                        self.content,
                        self.next,
                    )
                ):
                    raise ValueError(
                        "symbolic-link payload contains entry metadata only"
                    )
            else:
                self._validate_regular(content.size, query)
        if self.next is not None and _fs_resource_path(self.next) != _fs_resource_path(
            self.uri
        ):
            raise ValueError("continuation must keep the same path")
        return self

    def _validate_regular(self, size: int, query: dict[str, list[str]]) -> None:
        if self.encoding not in {"utf8", "base64"}:
            raise ValueError("regular-file payload requires utf8 or base64 encoding")
        if None in (self.start, self.end, self.total_bytes, self.content):
            raise ValueError("regular-file payload requires a complete byte range")
        assert self.start is not None and self.end is not None
        assert self.total_bytes is not None and self.content is not None
        if self.total_bytes != size or not self.start <= self.end <= size:
            raise ValueError("file range must fit the regular-file size")
        raw = self.content.encode("utf-8")
        if self.encoding == "base64":
            raw = base64.b64decode(self.content, validate=True)
            if base64.b64encode(raw).decode("ascii") != self.content:
                raise ValueError("file content must use canonical padded base64")
            try:
                raw.decode("utf-8")
            except UnicodeDecodeError:
                pass
            else:
                raise ValueError("valid UTF-8 ranges use utf8 encoding")
        if len(raw) != self.end - self.start:
            raise ValueError("content byte length must equal end minus start")
        if self.start != int(query.get("start", ["0"])[0]):
            raise ValueError("payload start must equal the requested start")
        if "length" in query and self.end - self.start > int(query["length"][0]):
            raise ValueError("payload exceeds the requested range length")
        if self.end == size and self.next is not None:
            raise ValueError("complete file payload cannot contain a continuation")
        if self.end < size:
            if self.next is None:
                raise ValueError("partial file payload requires a continuation")
            continuation = _fs_resource_query(self.next)
            if (
                continuation.get("start") != [str(self.end)]
                or "length" not in continuation
            ):
                raise ValueError(
                    "file continuation must begin at end with an explicit length"
                )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ExecutionAvailability(ClosedModel):
    """Effective caller-code routing after Rift intersects `rift.toml` with one adapter's
    advertised operations."""

    execute: Field[bool] = proto_field(
        description=(
            "Whether execute may route to this language after workspace configuration and adapter capability are applied."
        ),
        number=1,
    )
    debug: Field[bool] = proto_field(
        description=(
            "Whether all three debug tools may route to this language. True requires execute "
            "enablement and the complete adapter debug operation triplet."
        ),
        number=2,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class LanguageSupport(ClosedModel):
    """Capabilities reported by one configured language adapter. `execution` applies the
    workspace configuration to its caller-code operations."""

    language: Field[core.Language] = proto_field(
        description="Language name and optional dialect handled by the adapter.",
        number=1,
    )
    adapter: Field[str] = proto_field(
        description="Implementation name and version reported by the adapter.",
        max_length=4096,
        examples=["rift-adapter-typescript 0.4.1"],
        number=2,
    )
    operations: Field[list[core.AdapterOperation]] = proto_field(
        description="Optional operations implemented by the adapter, sorted by protocol order.",
        number=3,
        json_schema_extra={"uniqueItems": True},
    )
    fact_families: Field[list[core.FactFamily]] = proto_field(
        description="Fact families the adapter may emit from Analyze, sorted by protocol order.",
        number=4,
        json_schema_extra={"uniqueItems": True},
    )
    action_kinds: Field[list[core.ActionSupport]] = proto_field(
        description=(
            "Supported action families in kind-prefix order. Empty when `actions` is not "
            "advertised or the adapter has no action families."
        ),
        number=5,
    )
    execution: Field[ExecutionAvailability] = proto_field(
        description="Effective workspace configuration and adapter capability for caller code.",
        number=6,
    )


MODELS = (
    Cursor,
    SourceExcerpt,
    OutlineParams,
    OutlineItem,
    OutlineResult,
    SearchParams,
    SearchHit,
    ResourceFamily,
    ResourceLink,
    SymbolResourcePayload,
    Contract,
    WorkspacePath,
    ChangeId,
    SessionId,
    ConnectionId,
    FeatureId,
    ConnectRole,
    ConnectRequest,
    Connected,
    SessionSummary,
    SessionListParams,
    SessionListResult,
    SessionContinueParams,
    SessionContinueResult,
    SessionRemoveParams,
    SessionRemoveResult,
    DebugLimits,
    ExecutionLimits,
    Limits,
    WorkspaceResourceUri,
    ProjectionResourceUri,
    ChangesResourceUri,
    SymbolResourceUri,
    FsResourceUri,
    WorkspaceResourcePayload,
    ProjectionResourcePayload,
    SearchResult,
    ActionOffer,
    ActionsResourceUri,
    ActionResourceUri,
    ActionsResourcePayload,
    ActionResourcePayload,
    ExecuteParams,
    ExecuteResult,
    DebugSessionId,
    DebugSession,
    DebugStartParams,
    DebugGetFrameParams,
    DebugGetFrameResult,
    DebugStopParams,
    DebugStopResult,
    MatchHit,
    MatchParams,
    MatchResult,
    DiagnosticContext,
    ErrorCode,
    RetryDirective,
    ErrorCause,
    ErrorData,
    LimitEvidence,
    MatchSyntax,
    ResourceTemplate,
    ResourceContent,
    ResourceReadParams,
    ResourceReadResult,
    SearchHitTarget,
    EditParams,
    PatchParams,
    RewriteParams,
    RenameParams,
    MoveParams,
    DeleteParams,
    ChangeSignatureParams,
    ActParams,
    ProjectionRestoreParams,
    CommandValidator,
    ChangeValidation,
    ChangeSummary,
    ChangesResourcePayload,
    RefusalReason,
    ChangeResult,
    PublishParams,
    PublishResult,
    ResultOrder,
    FsResourcePayload,
    ExecutionAvailability,
    LanguageSupport,
)
