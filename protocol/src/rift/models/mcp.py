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
    ended. It binds the request, state, order, and page size that apply to that answer. Padding
    is omitted. A mismatch returns `cursor_invalid`. For a captured result set, process restart
    or eviction returns `cursor_expired`, and later writes do not change remaining pages."""

    @model_validator(mode="after")
    def value_is_canonical_base64url(self) -> Cursor:
        core.validate_base64url(self.root)
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SourceExcerpt(ClosedModel):
    "A copy of source from the catalog. The unit may belong to the project, a dependency, or the standard library; the excerpt preserves bytes as they were when the answer was produced."

    span: Field[core.SourceUnitSpan] = proto_field(
        description="The source unit and byte range the text was taken from.", number=1
    )
    text: Field[str] = proto_field(
        description="The source bytes returned by the request.", number=2
    )


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
                "By source path, then byte range, then identity. Source-less symbols follow "
                "sourced hits and sort by identity."
            ),
            "identity": (
                "By the result's canonical identity: a symbol URI or a file path. Ties are "
                "impossible, so the order is total."
            ),
        }
    },
)
class ResultOrder(str, Enum):
    "The total order a paginated answer comes back in, named in the request so a cursor can be bound to it. Every order ends in the result's own identity, so two results that tie never swap places between pages."

    RELEVANCE = "relevance"
    PATH = "path"
    IDENTITY = "identity"


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
            "symbol": "Declarations a provider resolved — a function, a class, a trait.",
            "node": "Places in a syntax tree where a symbol is written. One symbol has many.",
            "file": "Entries of the tree, whether or not any provider reads them.",
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
    public=True,
    proto=Proto.enum(
        "SearchScope",
        (
            EnumValue("project", "SEARCH_SCOPE_PROJECT", 1),
            EnumValue("dependencies", "SEARCH_SCOPE_DEPENDENCIES", 2),
            EnumValue("all", "SEARCH_SCOPE_ALL", 3),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "project": "Declarations and files owned by the current workspace.",
            "dependencies": "Declarations owned by resolved dependencies.",
            "all": "Project, dependencies, standard library, and external declarations.",
        }
    },
)
class SearchScope(str, Enum):
    """Which source locations a symbol lookup or search may return."""

    PROJECT = "project"
    DEPENDENCIES = "dependencies"
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
            "diagnostics": "What providers reported at the hit.",
        }
    },
)
class SearchInclude(str, Enum):
    SOURCE = "source"
    SIGNATURE = "signature"
    RELATIONSHIPS = "relationships"
    DIAGNOSTICS = "diagnostics"


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "SearchIntent",
        (
            EnumValue("trace", "SEARCH_INTENT_TRACE", 1),
            EnumValue("find_tests", "SEARCH_INTENT_FIND_TESTS", 2),
            EnumValue("edit_ripple", "SEARCH_INTENT_EDIT_RIPPLE", 3),
            EnumValue("review_context", "SEARCH_INTENT_REVIEW_CONTEXT", 4),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "trace": "Follow outgoing `calls` edges from one symbol.",
            "find_tests": "Follow incoming `tests` edges to test symbols.",
            "edit_ripple": "Follow incoming `calls` edges to symbols that depend on the seed.",
            "review_context": "Follow incoming and outgoing `calls` edges around one symbol.",
        }
    },
)
class SearchIntent(str, Enum):
    "The task that selects graph defaults and ranking. The server still returns each traversed edge, so the caller can audit the ranking."

    TRACE = "trace"
    FIND_TESTS = "find_tests"
    EDIT_RIPPLE = "edit_ripple"
    REVIEW_CONTEXT = "review_context"


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "TraversalDirection",
        (
            EnumValue("outgoing", "TRAVERSAL_DIRECTION_OUTGOING", 1),
            EnumValue("incoming", "TRAVERSAL_DIRECTION_INCOMING", 2),
            EnumValue("both", "TRAVERSAL_DIRECTION_BOTH", 3),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "outgoing": "Edges whose `from` is the current symbol.",
            "incoming": "Edges whose `to` is the current symbol.",
            "both": "Incoming and outgoing edges, merged by canonical identity.",
        }
    },
)
class TraversalDirection(str, Enum):
    "Which edge direction the server walks from each visited symbol."

    OUTGOING = "outgoing"
    INCOMING = "incoming"
    BOTH = "both"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SearchTraversal(ClosedModel):
    "A bounded relationship walk starting at one symbol. The server visits at most `max_nodes` symbols outside `seed` and expands no path beyond `max_hops`."

    seed: Field[core.SymbolId] = proto_field(
        description="The symbol where the walk starts. The seed is not returned as a hit.",
        number=1,
    )
    intent: Field[SearchIntent] = proto_field(
        description=(
            "The task whose defaults rank and narrow the walk. `find_tests` returns test "
            "symbols; other intents return every eligible symbol."
        ),
        number=2,
    )
    direction: Field[TraversalDirection | None] = proto_field(
        default=None,
        description=(
            "Direction to walk. Omitted selects incoming for `find_tests` and "
            "`edit_ripple`, outgoing for `trace`, and both for `review_context`."
        ),
        number=3,
    )
    facets: Field[list[core.RelationshipFacet] | None] = proto_field(
        default=None,
        description=(
            "Portable relationship facets eligible for expansion. Omitted selects `tests` "
            "for `find_tests` and `calls` for every other intent; an empty list is "
            "`invalid_request`."
        ),
        min_length=1,
        number=4,
        json_schema_extra={"uniqueItems": True},
    )
    max_hops: Field[int] = proto_field(
        default=1,
        description=(
            "Maximum path length from `seed`. The server accepts 1 or 2; one hop is the "
            "default because a second hop can multiply weak edges."
        ),
        ge=1,
        le=2,
        number=5,
    )
    max_nodes: Field[int] = proto_field(
        default=25,
        description=(
            "Most distinct graph symbols the server may visit, excluding `seed`. Filtering "
            "can make the answer shorter than this bound."
        ),
        ge=1,
        le=100,
        number=6,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SearchParams(ClosedModel):
    "Criteria for one search. The caller supplies at least one of lexical `query`, provider `filter`, or relationship `traversal`; `scope` selects source locations, and `paths` narrows project-only searches."

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
                {
                    "description": "Satisfied by a bounded relationship traversal.",
                    "required": ["traversal"],
                },
            ],
            "allOf": [
                {
                    "if": {"required": ["traversal"]},
                    "then": {"properties": {"target": {"enum": ["symbol", "all"]}}},
                },
                {
                    "if": {"required": ["paths"]},
                    "then": {"properties": {"scope": {"const": "project"}}},
                }
            ],
        }
    )
    target: Field[SearchParamsTarget] = proto_field(
        default=SearchParamsTarget.ALL,
        description=(
            "Which entity kinds may be returned — a kind selector, never the text to search "
            "for; that is `query`. Omitted, every kind may match. Type data is attached to "
            "the Symbol and Node records that bind it, and filters can search those "
            "attachments."
        ),
        number=1,
    )
    order: Field[ResultOrder] = proto_field(
        default=ResultOrder.RELEVANCE,
        description=(
            "Which total order the page comes back in. The cursor is bound to it, so it "
            "cannot change between pages of one query. Omitted, relevance."
        ),
        number=2,
    )
    query: Field[str | None] = proto_field(
        default=None,
        description=(
            "Text to match against file contents, symbol names, and rendered signatures. "
            "Matching is case-insensitive and identifier-aware — the query and the fields "
            "split on case and underscore boundaries, so `loadConfig` finds `load_config`. "
            "Scoring is server-defined and stable for one cursor's life."
        ),
        number=3,
    )
    filter: Field[core.Filter | None] = proto_field(
        default=None,
        description=(
            "A predicate over resolved fields and relationships. This is where provider "
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
            "Extra payload to attach to every hit. Each entry costs a lookup per hit, so "
            "the caller requests only what it will read."
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
    projection: Field[core.ProjectionId | None] = proto_field(
        default=None,
        description="The projection to search. Null searches the workspace tree.",
        number=9,
    )
    traversal: Field[SearchTraversal | None] = proto_field(
        default=None,
        description=(
            "A bounded relationship walk. It may stand alone or add graph hits to a lexical "
            "or filtered search; duplicate symbols keep their shortest path."
        ),
        number=10,
    )
    scope: Field[SearchScope] = proto_field(
        default=SearchScope.PROJECT,
        description=(
            "Source locations eligible for results. Project is the default; select "
            "dependencies or all when the answer may live outside the workspace."
        ),
        number=11,
    )

    @model_validator(mode="after")
    def has_query_and_valid_traversal_target(self) -> SearchParams:
        if self.query is None and self.filter is None and self.traversal is None:
            raise ValueError("search requires query, filter, or traversal")
        if self.traversal is not None and self.target not in {
            SearchParamsTarget.SYMBOL,
            SearchParamsTarget.ALL,
        }:
            raise ValueError("search traversal target must be symbol or all")
        if self.paths is not None and self.scope is not SearchScope.PROJECT:
            raise ValueError("search paths require project scope")
        return self


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "MatchedField",
        (
            EnumValue("name", "MATCHED_FIELD_NAME", 1),
            EnumValue("signature", "MATCHED_FIELD_SIGNATURE", 2),
            EnumValue("documentation", "MATCHED_FIELD_DOCUMENTATION", 3),
            EnumValue("content", "MATCHED_FIELD_CONTENT", 4),
            EnumValue("path", "MATCHED_FIELD_PATH", 5),
            EnumValue("relationship", "MATCHED_FIELD_RELATIONSHIP", 6),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "name": "The symbol or file name matched.",
            "signature": "The rendered signature matched.",
            "documentation": "The documentation text matched.",
            "content": "The text of the file matched.",
            "path": "The project path matched.",
            "relationship": "A bounded relationship traversal reached the symbol.",
        }
    },
)
class MatchedField(str, Enum):
    """Which indexed field produced a search hit."""

    NAME = "name"
    SIGNATURE = "signature"
    DOCUMENTATION = "documentation"
    CONTENT = "content"
    PATH = "path"
    RELATIONSHIP = "relationship"


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "HopDirection",
        (
            EnumValue("outgoing", "HOP_DIRECTION_OUTGOING", 1),
            EnumValue("incoming", "HOP_DIRECTION_INCOMING", 2),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "outgoing": "The walk followed the edge from `from` to `to`.",
            "incoming": "The walk followed the edge from `to` to `from`.",
        }
    },
)
class HopDirection(str, Enum):
    "How one path step followed its directed relationship."

    OUTGOING = "outgoing"
    INCOMING = "incoming"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class GraphHop(ClosedModel):
    "One auditable step in a search traversal. `relationship` retains source-node evidence and derivation; `direction` records how the walk followed it."

    relationship: Field[core.Relationship] = proto_field(
        description="The relationship followed for this step.", number=1
    )
    direction: Field[HopDirection] = proto_field(
        description="How the walk followed the directed relationship.", number=2
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SearchHit(ClosedModel):
    """One search hit. Its file, node, or symbol payload carries the canonical identity.
    Dependency and synthetic symbols can have no readable source; node and file hits cannot."""

    model_config = closed_config(
        {
            "allOf": [
                {
                    "oneOf": [
                        {"required": ["span", "line"]},
                        {
                            "not": {
                                "anyOf": [
                                    {"required": ["span"]},
                                    {"required": ["line"]},
                                ]
                            }
                        },
                    ]
                },
                {
                    "if": {
                        "properties": {
                            "hit": {
                                "properties": {
                                    "target": {"enum": ["node", "file"]}
                                }
                            }
                        }
                    },
                    "then": {"required": ["span", "line"]},
                },
            ]
        }
    )

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
    matched_by: Field[list[MatchedField]] = proto_field(
        description="Which indexed fields produced the match.",
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
            "span, so the caller can act on what it read without searching for it again."
        ),
        number=5,
    )
    diagnostics: Field[list[DiagnosticContext] | None] = proto_field(
        default=None,
        description='What providers reported here, requested with `include: ["diagnostics"]`.',
        number=6,
    )
    span: Field[core.SourceUnitSpan | None] = proto_field(
        default=None,
        description=(
            "Where the hit is written in the source catalog. Null for a symbol whose source "
            "is unavailable or synthetic."
        ),
        number=7,
    )
    line: Field[int | None] = proto_field(
        default=None,
        description="The 1-based source line where the hit begins, or null with `span`.",
        ge=1,
        number=8,
    )
    path: Field[list[GraphHop] | None] = proto_field(
        default=None,
        description=(
            "Shortest relationship path from `traversal.seed` to this hit. Present whenever "
            "the traversal reached the hit, including a hit also matched lexically."
        ),
        min_length=1,
        max_length=2,
        number=9,
    )
    distance: Field[int | None] = proto_field(
        default=None,
        description=(
            "Number of edges in `path`. It is present exactly when `path` is present and "
            "equals its length."
        ),
        ge=1,
        le=2,
        number=10,
    )

    @model_validator(mode="after")
    def traversal_path_is_correlated(self) -> SearchHit:
        if (self.span is None) != (self.line is None):
            raise ValueError("search hit span and line must be present together")
        if not isinstance(self.hit.root, SearchHitTargetSymbol) and self.span is None:
            raise ValueError("node or file search hit requires source location")
        if (self.path is None) != (self.distance is None):
            raise ValueError("search hit path and distance must be present together")
        if self.path is None:
            return self
        if self.distance != len(self.path):
            raise ValueError("search hit distance must equal path length")
        if not isinstance(self.hit.root, SearchHitTargetSymbol):
            raise ValueError("only a symbol hit can carry a traversal path")
        if MatchedField.RELATIONSHIP not in self.matched_by:
            raise ValueError("a traversal hit must include relationship in matched_by")
        return self


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


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ServerLock(ClosedModel):
    """The rendezvous record at `.rift/server.json`, written by the server that owns the
    workspace and read by every process that wants to reach it. Atomic creation of the file
    is the election: the process that creates it starts the server, and everyone else
    connects to the endpoint it names. The file is readable by its owner alone, so holding
    the token is what authorizes a connection — localhost TCP carries no peer identity the
    server could check instead."""

    port: Field[int] = proto_field(
        description="TCP port on `127.0.0.1` where the server accepts HTTP.",
        ge=1,
        le=65535,
        number=1,
    )
    pid: Field[int] = proto_field(
        description=(
            "Process id of the owning server, for staleness checks. A reader that cannot "
            "reach the endpoint verifies the process before removing the file and "
            "re-running the election."
        ),
        ge=1,
        le=9007199254740991,
        number=2,
    )
    token: Field[str] = proto_field(
        description=(
            "Bearer token every HTTP request carries in `Authorization`. The server refuses "
            "a request without it."
        ),
        pattern=r"^[A-Za-z0-9_-]{32,128}$",
        number=3,
    )
    workspace: Field[WorkspacePath] = proto_field(
        description="Canonical absolute path of the workspace the server serves.",
        number=5,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class Projection(ClosedModel):
    """One pinned workspace snapshot: its identity, directory, base revision, and current
    state."""

    id: Field[core.ProjectionId] = proto_field(
        description="Identity of the projection, and the URI that resolves it.",
        number=1,
    )
    path: Field[WorkspacePath] = proto_field(
        description=(
            "Absolute path of the projection directory, at `.rift/projections/<id>` for the "
            "projection's life, so a shell working inside it keeps working across a server "
            "restart."
        ),
        number=2,
    )
    state: Field[core.ProjectionState] = proto_field(
        description="Current projection state.", number=3
    )
    base_revision: Field[core.Digest] = proto_field(
        description=(
            "Workspace tree revision captured when the projection was created or last "
            "published. A restored path carries its own later baseline in the projection "
            "manifest."
        ),
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionCreateParams(ClosedModel):
    """Materializes a pinned snapshot of the workspace. The server uses filesystem reflinks
    where available and copies entries otherwise; later workspace writes do not change the
    projection."""

    reason: Field[str | None] = proto_field(
        default=None,
        description="Why the projection exists, shown by `projection_list` for a human choosing what to keep.",
        max_length=256,
        number=1,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionListParams(ClosedModel):
    """Selects one page from a captured list of projections in identity order."""

    limit: Field[int] = proto_field(
        default=100,
        description="Most projections to return on this page.",
        ge=1,
        le=1000,
        number=1,
    )
    cursor: Field[Cursor | None] = proto_field(
        default=None,
        description=(
            "Continues a captured projection list with the same page size. A mismatched "
            "cursor returns `cursor_invalid`; one whose captured list the server dropped "
            "returns `cursor_expired`."
        ),
        number=2,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionListResult(ClosedModel):
    """One page of projections."""

    projections: Field[list[Projection]] = proto_field(
        description="Projections on this page, sorted by identity.", number=1
    )
    next_cursor: Field[Cursor | None] = proto_field(
        description="Cursor for the next page, or null after the final projection.",
        number=2,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionRemoveParams(ClosedModel):
    """Removes one projection and deletes its directory. A dirty projection is removed the
    same way a clean one is — the caller names it, so the destruction is chosen — and the
    deletion does not wait for whatever still runs inside the directory, because a lock
    there would let one abandoned watcher wedge cleanup forever."""

    projection: Field[core.ProjectionId] = proto_field(
        description="The projection to remove.", number=1
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionRemoveResult(ClosedModel):
    """The removed projection and the state it held."""

    projection: Field[core.ProjectionId] = proto_field(
        description="The projection the result describes.", number=1
    )
    state: Field[core.ProjectionState] = proto_field(
        description="Exact projection state at removal.",
        number=2,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ExecutionLimits(ClosedModel):
    """Workspace ceilings for caller-provided code. Null at `Limits.execution` means
    `rift.toml` disables execution, regardless of runtime capability."""

    max_code_bytes: Field[int] = proto_field(
        description="UTF-8 bytes accepted in one CodeBlock.source.",
        ge=1,
        le=32768,
        number=1,
    )
    budget: Field[core.ExecutionBudget] = proto_field(
        description=(
            "The exact evaluation budget every `execute` call receives — the advertised "
            "ceiling and the applied bound are one value."
        ),
        number=2,
    )
    max_concurrent: Field[int] = proto_field(
        description=(
            "Evaluations running concurrently across all connections in the workspace. A call "
            "arriving with every concurrent evaluation in use returns `temporarily_unavailable`."
        ),
        ge=1,
        le=64,
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class Limits(ClosedModel):
    "The ceilings this server enforces on MCP requests, responses, captured pages, and projection read dependencies. A request or response that crosses one fails with `limit_exceeded` carrying `LimitEvidence`."

    max_request_bytes: Field[int] = proto_field(
        description=(
            "Largest request body the server accepts, in bytes. A deep filter tree or a "
            "long patch is the usual way to exceed it."
        ),
        ge=1024,
        le=49152,
        number=1,
    )
    max_response_bytes: Field[Literal[65536]] = proto_field(
        description=(
            "Largest serialized tool result or resource page Rift returns. Paginated answers "
            "stop before this boundary and provide a cursor; an indivisible result that "
            "cannot fit fails with `limit_exceeded`. The value stays below the truncation "
            "boundary of common MCP harnesses."
        ),
        number=2,
    )
    max_record_bytes: Field[int] = proto_field(
        description=(
            "Largest RFC 8785 JSON encoding of one indivisible item in a paginated answer, "
            "such as one Change, Edit, diagnostic, or hook result. Resolution fails with "
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
    max_hooks: Field[int] = proto_field(
        description=(
            "How many workspace hooks run when a change applies. "
            "Zero when `rift.toml` declares none."
        ),
        ge=0,
        le=4294967295,
        number=9,
    )
    execution: Field[ExecutionLimits | None] = proto_field(
        description=(
            "Caller-code execution ceilings. Null means `rift.toml` disables `execute`; "
            "runtime capability alone never enables it."
        ),
        number=11,
    )
    max_filter_depth: Field[int] = proto_field(
        description=(
            "Nesting levels accepted in one caller-supplied predicate or argument object, so "
            "a schema-valid request cannot be a stack overflow. Exceeded fails with "
            "`limit_exceeded` naming this limit."
        ),
        ge=1,
        le=64,
        number=12,
    )
    max_request_ms: Field[int] = proto_field(
        description=(
            "Wall-clock the server allows one call, evaluation excluded — `execute` adds its "
            "own `max_timeout`. A call that runs past it fails with `deadline_exceeded`."
        ),
        ge=1000,
        le=3600000,
        number=13,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
    )
    max_active_cursors: Field[int] = proto_field(
        description=(
            "Captured result sets retained across the workspace. The server evicts the oldest "
            "capture first; a page requested after eviction fails with `cursor_expired`."
        ),
        ge=1,
        le=1024,
        number=14,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
    )
    max_capture_items: Field[int] = proto_field(
        description=(
            "Most items one paginated `get_symbol` or `search` capture may retain. A result "
            "set above this bound fails with `limit_exceeded` before returning its first page."
        ),
        ge=1,
        le=1000000,
        number=15,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
    )
    max_capture_bytes: Field[int] = proto_field(
        description=(
            "Largest serialized `get_symbol` or `search` capture retained for one cursor, "
            "in bytes. A larger result fails with `limit_exceeded`."
        ),
        ge=1024,
        le=1073741824,
        number=16,
    )
    max_retained_capture_bytes: Field[int] = proto_field(
        description=(
            "Serialized bytes retained across all captured reads in the workspace. The server "
            "evicts oldest captures until the retained total fits this bound."
        ),
        ge=1024,
        le=4294967296,
        number=17,
    )
    max_projection_dependencies: Field[int] = proto_field(
        description=(
            "Most distinct file digests one projection read set may retain. A read that would "
            "cross the bound fails with `limit_exceeded` before returning data."
        ),
        ge=1,
        le=1000000,
        number=18,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
    )
    max_projection_dependency_bytes: Field[int] = proto_field(
        description=(
            "Largest encoded path-and-digest data retained in one projection read set, in "
            "bytes. A read that would cross the bound fails with `limit_exceeded`."
        ),
        ge=1024,
        le=1073741824,
        number=19,
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
    pattern=r"^rift://projection/prj_[a-z2-7]{26}$",
)
class ProjectionResourceUri(ProtocolRoot):
    """URI for one projection read. It is the projection's own `ProjectionId`."""

    @model_validator(mode="after")
    def identity_is_canonical(self) -> ProjectionResourceUri:
        core.ProjectionId.model_validate(self.root)
        return self


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://changes(?:\?(?:projection=rift%3A%2F%2Fprojection%2Fprj_[a-z2-7]{26}(?:&cursor=[A-Za-z0-9_-]{1,4096})?|cursor=[A-Za-z0-9_-]{1,4096}))?$",
    max_length=8192,
)
class ChangesResourceUri(ProtocolRoot):
    """Paginated changeset. Without a `projection` parameter it pages the workspace journal —
    the changes applied directly to the workspace tree; with one, that projection's
    changeset. The `projection` value is a percent-encoded `ProjectionId`."""

    @model_validator(mode="after")
    def query_is_canonical(self) -> ChangesResourceUri:
        query = _raw_resource_query(self.root)
        cursor = query.get("cursor")
        if cursor is not None:
            Cursor.model_validate(cursor)
        projection = query.get("projection")
        if projection is not None:
            core.ProjectionId.model_validate(
                unquote_to_bytes(projection).decode("utf-8")
            )
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
    `cursor`. A `start` equal to the file size returns an empty final page; a `start` past it
    is `invalid_request` with `retry: never`."""

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
            EnumValue("fs", "RESOURCE_FAMILY_FS", 3),
            EnumValue("projection", "RESOURCE_FAMILY_PROJECTION", 6),
            EnumValue("changes", "RESOURCE_FAMILY_CHANGES", 7),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "workspace": "Workspace capabilities and request limits.",
            "fs": "One directory page or file-content range.",
            "projection": "Where one projection lives on the filesystem, and its state.",
            "changes": "One changeset — the workspace journal, or a projection's — and what vouched for each change.",
        }
    },
)
class ResourceFamily(str, Enum):
    "One family of Rift resources. The family fixes the URI shape a read accepts, the media type it returns, and the payload model inside it."

    WORKSPACE = "workspace"
    FS = "fs"
    PROJECTION = "projection"
    CHANGES = "changes"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class WorkspaceResourcePayload(ClosedModel):
    """Current workspace capabilities and request limits. Source resolvers advertise what
    they discover; each language advertises the fact providers that analyze those sources."""

    uri: Field[WorkspaceResourceUri] = proto_field(
        description="The URI this payload answers for.",
        number=1,
    )
    snapshot: Field[core.ReadSnapshot] = proto_field(
        description="Workspace and index revisions captured for this page.", number=2
    )
    limits: Field[Limits] = proto_field(
        description="The ceilings a request has to stay inside here.", number=4
    )
    languages: Field[list[LanguageSupport]] = proto_field(
        description="Served languages and their capabilities, sorted by name and dialect with null first.",
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
    source_resolvers: Field[list[SourceResolverSummary]] = proto_field(
        description=(
            "Resolvers that populate the source catalog, sorted by identity. Their revisions "
            "are independent from fact-provider revisions."
        ),
        number=7,
        json_schema_extra={"uniqueItems": True},
    )
    source_units: Field[list[core.SourceUnit]] = proto_field(
        description=(
            "Source-catalog units on this page, sorted by source identity. Pagination keeps "
            "the captured catalog revision."
        ),
        number=10,
        json_schema_extra={"uniqueItems": True},
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
    hooks: Field[list[Hook]] = proto_field(
        description="Hooks declared by the workspace-root `rift.toml`, in execution order.",
        number=13,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionResourcePayload(ClosedModel):
    """Where one projection lives on the filesystem and what it holds. A caller that has
    to reach the projection through an ordinary filesystem tool reads its path here."""

    uri: Field[ProjectionResourceUri] = proto_field(
        description="The URI this payload answers for.", number=1
    )
    projection: Field[Projection] = proto_field(
        description="The projection: identity, directory path, and state.", number=2
    )
    workspace: Field[WorkspacePath] = proto_field(
        description="Absolute path of the workspace this projection was taken from.",
        number=3,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SearchResult(ClosedModel):
    "One page of search hits from one captured tree and index revision. `coverage` states whether an empty result proves that no indexed candidate matched."

    snapshot: Field[core.ReadSnapshot] = proto_field(
        description="Tree and search-index revisions used for this result page.",
        number=1,
    )
    coverage: Field[core.Coverage] = proto_field(
        description=(
            "Coverage of the indexed candidate set used for this search. An empty result "
            "proves no match only where this is complete."
        ),
        number=2,
    )
    results: Field[list[SearchHit]] = proto_field(
        description="The hits on this page, in the order the request asked for.",
        number=3,
    )
    next_cursor: Field[Cursor | None] = proto_field(
        description="Cursor for the next page, or null after the final result.",
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ExecuteParams(ClosedModel):
    """Evaluate code in the targeted tree's execution copy — the workspace's, or the
    named projection's. The copy persists between calls, so installed dependencies and
    build caches survive; before each evaluation the server refreshes its visible files
    to match the targeted tree."""

    language: Field[core.Language] = proto_field(
        description="Exact language and optional dialect selecting the runtime.",
        number=1,
    )
    block: Field[core.CodeBlock] = proto_field(
        description="Source to evaluate and its project-relative working directory.",
        number=2,
    )
    projection: Field[core.ProjectionId | None] = proto_field(
        default=None,
        description="The projection whose execution copy is used. Null uses the workspace's.",
        number=3,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ExecuteResult(ClosedModel):
    """Bounded result of one execution. The server does not synchronize writes inside the
    execution directory back to the targeted tree. The directory is not a sandbox, so the
    runtime retains the server's OS permissions outside it."""

    snapshot: Field[core.ReadSnapshot] = proto_field(
        description="Target-tree revision copied into the execution directory.",
        number=1,
    )
    language: Field[core.Language] = proto_field(
        description="Language whose runtime evaluated the block.", number=2
    )
    result: Field[core.ExecutionResult] = proto_field(
        description="Runtime status, bounded output, and structured diagnostics.",
        number=3,
    )
    budget: Field[core.ExecutionBudget] = proto_field(
        description="Exact bounds applied to this evaluation.", number=4
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Source",
        (
            EnumValue("provider", "PROVIDER", 1),
            EnumValue("hook", "HOOK", 2),
            EnumValue("apply", "APPLY", 3),
        ),
        placement=Placement("source", 1),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "provider": "The analysis of a provider serving the language.",
            "hook": "A workspace hook Rift ran over a proposed change.",
            "apply": "Output from applying edits to the workspace.",
        }
    },
)
class DiagnosticContextSource(str, Enum):
    """Component that produced the diagnostic. Rift sets this after collecting provider, hook, or apply output."""

    PROVIDER = "provider"
    HOOK = "hook"
    APPLY = "apply"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DiagnosticContext(ClosedModel):
    "One `Diagnostic` as an MCP answer carries it: the fact its emitter minted, plus what Rift can add on top — where it lands in a line and column, and the source around it."

    source: Field[DiagnosticContextSource] = proto_field(
        description=(
            "Component that produced the diagnostic. Rift sets this after collecting provider, "
            "hook, or apply output."
        ),
        number=1,
    )
    diagnostic: Field[core.Diagnostic] = proto_field(
        description="The finding itself, exactly as its emitter minted it.", number=3
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
        "SourceResolverState",
        (
            EnumValue("ready", "SOURCE_RESOLVER_STATE_READY", 1),
            EnumValue("updating", "SOURCE_RESOLVER_STATE_UPDATING", 2),
            EnumValue("unavailable", "SOURCE_RESOLVER_STATE_UNAVAILABLE", 3),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "ready": "Published catalog revision includes the current resolver inputs.",
            "updating": "Resolver serves its previous revision while rebuilding the catalog.",
            "unavailable": "Resolver has no catalog revision it can serve.",
        }
    },
)
class SourceResolverState(str, Enum):
    """Lifecycle state of one source resolver."""

    READY = "ready"
    UPDATING = "updating"
    UNAVAILABLE = "unavailable"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SourceResolverSummary(ClosedModel):
    """One resolver that discovers source units before providers analyze them."""

    implementation: Field[str] = proto_field(
        description="Implementation name and version the resolver reports.",
        max_length=4096,
        examples=["rift-project 0.4.1", "rift-cargo 0.4.1"],
        number=1,
    )
    id: Field[core.SourceResolverId] = proto_field(
        description="Stable source-resolver identity.", number=2
    )
    locations: Field[list[core.SourceLocationKind]] = proto_field(
        description="Source locations this resolver discovers, in protocol order.",
        min_length=1,
        number=3,
        json_schema_extra={"uniqueItems": True},
    )
    revision: Field[core.Digest | None] = proto_field(
        default=None,
        description="Latest immutable resolver revision, or null when unavailable.",
        number=4,
    )
    state: Field[SourceResolverState] = proto_field(
        description="Current resolver lifecycle state.", number=5
    )

    @model_validator(mode="after")
    def state_has_revision(self) -> SourceResolverSummary:
        if self.state is SourceResolverState.UNAVAILABLE and self.revision is not None:
            raise ValueError("unavailable source resolver cannot advertise a revision")
        if self.state is not SourceResolverState.UNAVAILABLE and self.revision is None:
            raise ValueError("serving source resolver requires a revision")
        return self


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "ErrorCode",
        (
            EnumValue("invalid_request", "ERROR_CODE_INVALID_REQUEST", 1),
            EnumValue("permission_denied", "ERROR_CODE_PERMISSION_DENIED", 3),
            EnumValue("resource_not_found", "ERROR_CODE_RESOURCE_NOT_FOUND", 4),
            EnumValue("content_unavailable", "ERROR_CODE_CONTENT_UNAVAILABLE", 5),
            EnumValue("cursor_invalid", "ERROR_CODE_CURSOR_INVALID", 6),
            EnumValue("cancelled", "ERROR_CODE_CANCELLED", 7),
            EnumValue("deadline_exceeded", "ERROR_CODE_DEADLINE_EXCEEDED", 8),
            EnumValue("limit_exceeded", "ERROR_CODE_LIMIT_EXCEEDED", 9),
            EnumValue("projection_busy", "ERROR_CODE_PROJECTION_BUSY", 10),
            EnumValue("storage_failure", "ERROR_CODE_STORAGE_FAILURE", 14),
            EnumValue(
                "hook_execution_failure",
                "ERROR_CODE_HOOK_EXECUTION_FAILURE",
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
                "a `limit` above the advertised maximum, a filter field that does not exist."
            ),
            "permission_denied": (
                "The caller cannot perform this operation: a path addresses through a "
                "symlink component, which following could take outside the workspace, or "
                "the request carries no valid bearer token."
            ),
            "resource_not_found": (
                "The identity is well-formed and resolves to nothing, such as a missing symbol "
                "or filesystem entry."
            ),
            "content_unavailable": ("The entry is known but its bytes cannot be read."),
            "cursor_invalid": (
                "The cursor is malformed, or it was minted for a different request, order or page "
                "size."
            ),
            "cancelled": (
                "The caller cancelled, or the connection closed before the request finished."
            ),
            "deadline_exceeded": (
                "The request ran past `max_request_ms`, or an `execute` evaluation ran past its "
                "own timeout."
            ),
            "limit_exceeded": (
                "A request, response, capture, or projection read set crossed an advertised "
                "limit. `limit` identifies the bound and required value."
            ),
            "projection_busy": (
                "The targeted tree has an in-flight filesystem mutation that prevents this operation."
            ),
            "storage_failure": (
                "Rift could not read or write workspace or projection files."
            ),
            "hook_execution_failure": (
                "Rift could not launch the hook, enforce its timeout, or capture its output."
            ),
            "internal_error": (
                "A violated Rift invariant. `causes` identifies the operation and concrete failure."
            ),
            "unsupported_path": (
                "The workspace contains a path the protocol cannot represent safely."
            ),
            "cursor_expired": (
                "The cursor is valid, but its captured result page set left the process-local "
                "cache through eviction or process restart."
            ),
            "temporarily_unavailable": (
                "The resource exists but cannot serve this request now: another publication "
                "holds the workspace, every execution slot is taken, or a fresh indexed read "
                "could not capture stable revisions. The instance's `retry` field determines "
                "whether the same request is valid."
            ),
            "configuration_invalid": (
                "The workspace-root `rift.toml` does not satisfy its schema. New requests remain "
                "blocked until the file is valid."
            ),
            "capability_unavailable": (
                "The tool exists, but workspace configuration and the served providers cannot "
                "answer this operation for the requested language."
            ),
        }
    },
)
class ErrorCode(str, Enum):
    "Stable failure class for one request. `ErrorData.retry` carries the instance-specific retry decision; unsupported coverage and edit refusal use typed domain results instead."

    INVALID_REQUEST = "invalid_request"
    PERMISSION_DENIED = "permission_denied"
    RESOURCE_NOT_FOUND = "resource_not_found"
    CONTENT_UNAVAILABLE = "content_unavailable"
    CURSOR_INVALID = "cursor_invalid"
    CANCELLED = "cancelled"
    DEADLINE_EXCEEDED = "deadline_exceeded"
    LIMIT_EXCEEDED = "limit_exceeded"
    PROJECTION_BUSY = "projection_busy"
    STORAGE_FAILURE = "storage_failure"
    HOOK_EXECUTION_FAILURE = "hook_execution_failure"
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
            "never": "The request fails the same way every time. Change it before sending it again.",
            "same_request": (
                "Send the same bytes again. The cause was transient — a busy projection, an "
                "index still filling."
            ),
            "operator_action": (
                "Resolve the condition with a local state command or configuration change, "
                "then send the request again."
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
            EnumValue("check", "CHECK", 4),
            EnumValue("change", "CHANGE", 5),
            EnumValue("publish", "PUBLISH", 6),
            EnumValue("execute", "EXECUTE", 7),
        ),
        placement=Placement("phase", 4),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "discovery": "Working out what the workspace can do: capabilities, limits, which languages have providers.",
            "read": "Reading workspace or provider data.",
            "resolve": "Turning an address or a cursor into the concrete thing it names at a state.",
            "check": (
                "Checking a proposed change against the schema and the state it was pinned "
                "to."
            ),
            "change": "Resolving the operation, writing the result into the targeted tree, and running its hooks.",
            "publish": "Publishing projection changes to the workspace.",
            "execute": "Preparing the execution copy and evaluating caller-provided code.",
        }
    },
)
class ErrorDataPhase(str, Enum):
    "How far the request got before it failed. The same code means different things at different phases: `limit_exceeded` while reading is a response too big, and while checking a change it is a change set too large."

    DISCOVERY = "discovery"
    READ = "read"
    RESOLVE = "resolve"
    CHECK = "check"
    CHANGE = "change"
    PUBLISH = "publish"
    EXECUTE = "execute"


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
            "while checking a change it is a change set too large."
        ),
        number=4,
    )
    diagnostics: Field[list[DiagnosticContext]] = proto_field(
        description=(
            "What a provider or hook reported while the request was failing. Empty where "
            "neither ran."
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
@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class LimitEvidence(ClosedModel):
    "Which advertised limit a `limit_exceeded` failure hit, and by how much. It is present exactly when the code is `limit_exceeded`; without it, choosing between retrying smaller, falling back to another resource and giving up means reparsing the human message."

    field: Field[str] = proto_field(
        description=(
            "The limit's field path below `Limits`, such as `max_page_items` or "
            "`execution.max_concurrent`."
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


RESOURCE_FORMS: dict[ResourceFamily, tuple[str, str, type[Any]]] = {
    ResourceFamily.WORKSPACE: (
        "rift://workspace{?cursor}",
        "application/vnd.rift.workspace+json",
        WorkspaceResourceUri,
    ),
    ResourceFamily.FS: (
        "rift://fs{/path}{?start,length,cursor}",
        "application/vnd.rift.fs+json",
        FsResourceUri,
    ),
    ResourceFamily.PROJECTION: (
        "rift://projection/{id}",
        "application/vnd.rift.projection+json",
        ProjectionResourceUri,
    ),
    ResourceFamily.CHANGES: (
        "rift://changes{?projection,cursor}",
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
        | FsResourceUri
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
            "application/vnd.rift.fs+json": "FsResourcePayload",
            "application/vnd.rift.projection+json": "ProjectionResourcePayload",
            "application/vnd.rift.changes+json": "ChangesResourcePayload",
        }
    },
)
class ResourceContent(ClosedModel):
    "One content block of an MCP resource read. `text` holds the family's payload as JSON — `WorkspaceResourcePayload` for `rift://workspace`, `FsResourcePayload` for an `rift://fs` URI, and so on — and `mimeType` names which. The `rift:contentTypes` map in the schema carries the complete pairing."

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
    """A symbol hit: the declaration a provider resolved."""

    target: Field[Literal["symbol"]] = proto_field(
        description="Tags this as a symbol hit."
    )
    symbol: Field[core.Symbol] = proto_field(
        description="The declaration that matched.", number=1
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
    """A file hit: one entry of the tree, whether or not any provider reads it."""

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


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class GetSymbolParams(ClosedModel):
    """Gets declarations by name and returns them with their bodies inline, so one call
    replaces a search followed by paging through the file."""

    name: Field[str] = proto_field(
        description=(
            "The declaration name to look up — a name, not a free-text query; `search` "
            "takes those. An exact symbol name ranks first, then prefix matches, then "
            "qualified-name substrings."
        ),
        min_length=1,
        max_length=4096,
        number=1,
    )
    language: Field[core.Language | None] = proto_field(
        default=None,
        description="Narrows the answer to one language. Null searches every served language.",
        number=2,
    )
    include_body: Field[bool] = proto_field(
        default=True,
        description="Whether each hit carries its declaration source.",
        number=3,
    )
    include_history: Field[bool] = proto_field(
        default=False,
        description=(
            "Whether each hit carries its version-control timeline and co-change coupling. "
            "Off by default: a timeline is read when the caller is deciding about the "
            "symbol, not on every lookup."
        ),
        number=7,
    )
    limit: Field[int] = proto_field(
        default=5,
        description="Most hits to return in one page, capped by `max_page_items`.",
        ge=1,
        le=10000,
        number=4,
    )
    cursor: Field[Cursor | None] = proto_field(
        default=None,
        description="Continues a previous lookup where its last page ended.",
        number=5,
    )
    projection: Field[core.ProjectionId | None] = proto_field(
        default=None,
        description="The projection to read. Null reads the workspace tree.",
        number=6,
    )
    scope: Field[SearchScope] = proto_field(
        default=SearchScope.ALL,
        description=(
            "Source locations eligible for matches. All is the default because a known name "
            "may identify a dependency or standard-library declaration."
        ),
        number=8,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class GetSymbolHit(ClosedModel):
    """One declaration a `get_symbol` lookup found."""

    symbol: Field[core.Symbol] = proto_field(
        description="The declaration that matched.", number=1
    )
    node: Field[core.Node | None] = proto_field(
        default=None,
        description=(
            "The declaration node, whose identity `replace_symbol` can act on. Null when "
            "source is unavailable or outside the project."
        ),
        number=2,
    )
    source: Field[SourceExcerpt | None] = proto_field(
        default=None,
        description=(
            "The declaration source when the request asked for bodies and the provider can "
            "read it. Null for source-less declarations."
        ),
        number=3,
    )
    history: Field[core.SymbolHistory | None] = proto_field(
        default=None,
        description=(
            "The symbol's timeline, present when the request asked for history. Its "
            "coverage says how far back the walk reached."
        ),
        number=4,
    )
    co_changes: Field[list[core.CoChange] | None] = proto_field(
        default=None,
        description=(
            "Symbols that historically change with this one, strongest coupling first, "
            "present when the request asked for history."
        ),
        number=5,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class GetSymbolResult(ClosedModel):
    """One page of declarations matching a name."""

    hits: Field[list[GetSymbolHit]] = proto_field(
        description="The declarations on this page, best match first.", number=1
    )
    coverage: Field[core.Coverage] = proto_field(
        description="Coverage of the symbol index used for this lookup. An empty result proves absence only where this is complete.",
        number=2,
    )
    next_cursor: Field[Cursor | None] = proto_field(
        description="Cursor for the next page, or null after the final hit.", number=3
    )
    snapshot: Field[core.ReadSnapshot] = proto_field(
        description="Tree and index revisions used for this result page.", number=4
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class NodesParams(ClosedModel):
    """Lists the syntax nodes covering one position, outermost first. It returns a witnessed
    address for an edit smaller than a declaration, such as one call expression."""

    path: Field[core.ProjectPath] = proto_field(
        description="Project-relative file to inspect.",
        json_schema_extra={"minLength": 1},
        number=1,
    )
    position: Field[int] = proto_field(
        description=(
            "UTF-8 byte offset the listed nodes must cover — one position, not a range; "
            "the nodes themselves carry the spans."
        ),
        ge=0,
        le=9007199254740991,
        number=2,
    )
    projection: Field[core.ProjectionId | None] = proto_field(
        default=None,
        description="The projection to read. Null reads the workspace tree.",
        number=3,
    )

    @model_validator(mode="after")
    def path_names_a_file(self) -> NodesParams:
        if not self.path.root:
            raise ValueError("nodes path must name a file")
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class NodesResult(ClosedModel):
    """The nodes covering one position. Each identity carries its witness, so an address
    taken from this listing refuses cleanly once the bytes drift."""

    nodes: Field[list[core.Node]] = proto_field(
        description="Nodes covering the position, outermost first.", number=1
    )
    source: Field[list[SourceExcerpt]] = proto_field(
        description="The source of each node, in the same order.", number=2
    )
    coverage: Field[core.SemanticCoverage] = proto_field(
        description="Completeness of the node facts used to build the listing.",
        number=3,
    )
    snapshot: Field[core.ReadSnapshot] = proto_field(
        description="Tree revision and provider state used for this listing.", number=4
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Position",
        (
            EnumValue("before", "POSITION_BEFORE", 1),
            EnumValue("after", "POSITION_AFTER", 2),
        ),
        placement=Placement("position", 2),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "before": "The new declaration lands immediately before the anchor.",
            "after": "The new declaration lands immediately after the anchor.",
        }
    },
)
class InsertPosition(str, Enum):
    """Which side of the anchor receives the new declaration."""

    BEFORE = "before"
    AFTER = "after"


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class ReplaceSymbolParams(ClosedModel):
    "Replaces one declaration whole, addressed by its symbol. The parser derives the span, so the caller supplies no offsets; a name that resolves to several declarations refuses with `ambiguous_target` listing the candidates."

    symbol: Field[core.SymbolId] = proto_field(
        description="The declaration to replace.", number=4
    )
    region: Field[core.RegionRole | None] = proto_field(
        default=None,
        description=(
            "Which part of the declaration to replace — `body` leaves the header and "
            "documentation standing. Null replaces the enclosing declaration whole."
        ),
        number=5,
    )
    body: Field[str] = proto_field(description="The replacement source.", number=6)
    projection: Field[core.ProjectionId | None] = proto_field(
        default=None,
        description="The projection the change applies to. Null changes the workspace tree.",
        number=7,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class InsertSymbolParams(ClosedModel):
    "Inserts a new declaration beside an existing one. Anchoring on a symbol keeps the operation placement-safe: the parser decides the exact bytes, so an insertion cannot land inside a neighboring declaration."

    anchor: Field[core.SymbolId] = proto_field(
        description="The existing declaration the new one lands beside.", number=4
    )
    position: Field[InsertPosition] = proto_field(
        description="Which side of the anchor receives the new declaration.", number=5
    )
    body: Field[str] = proto_field(
        description="The new declaration's source.", number=6
    )
    projection: Field[core.ProjectionId | None] = proto_field(
        default=None,
        description="The projection the change applies to. Null changes the workspace tree.",
        number=7,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class ReplaceNodeParams(ClosedModel):
    "Replaces one syntax node, addressed by an identity a `nodes` listing returned. The address carries its witness, so a listing that has gone stale refuses with a failed `source_unchanged` precondition instead of splicing into moved bytes."

    node: Field[core.NodeId] = proto_field(
        description="The node to replace, witness included.", number=4
    )
    region: Field[core.RegionRole | None] = proto_field(
        default=None,
        description="Which named part of the node to replace. Null replaces the node whole.",
        number=5,
    )
    body: Field[str] = proto_field(description="The replacement source.", number=6)
    projection: Field[core.ProjectionId | None] = proto_field(
        default=None,
        description="The projection the change applies to. Null changes the workspace tree.",
        number=7,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class PatchParams(ClosedModel):
    "A UTF-8 unified diff guarded by its context lines, for the change no symbol or node address fits — several scattered hunks, a new file plus an edit. A malformed patch — an absolute path, path traversal, a binary patch, a broken header — is `invalid_request`; a hunk whose context differs from the state it resolves against is a refusal carrying a failed `source_unchanged` precondition."

    patch: Field[str] = proto_field(
        description="Unified text diff with project-relative `a/` and `b/` paths.",
        min_length=1,
        number=4,
    )
    projection: Field[core.ProjectionId | None] = proto_field(
        default=None,
        description="The projection the change applies to. Null changes the workspace tree.",
        number=5,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionRestoreParams(ClosedModel):
    """Restores changed paths of one projection from the current workspace."""

    projection: Field[core.ProjectionId] = proto_field(
        description="The projection whose changed paths are restored.", number=1
    )
    paths: Field[list[core.ProjectPath] | None] = proto_field(
        default=None,
        description=(
            "Changed paths to restore. Null restores every changed path. Naming any path of "
            "a change restores every path that change touched — a change leaves the changeset "
            "whole."
        ),
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
class HookKind(str, Enum):
    """How workspace configuration presents this hook."""

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
class CommandHookChangedPaths(str, Enum):
    """Whether Rift appends changed `ProjectPath` values to `argv` in byte order."""

    NONE = "none"
    APPEND = "append"


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class HookGuarantee(ClosedModel):
    "What a passing run of one configured hook establishes."

    kind: Field[core.GuaranteeKind] = proto_field(
        description="Guarantee established when the hook passes.", number=1
    )
    scope: Field[core.CoverageScope] = proto_field(
        description="Source over which the hook checks the property.", number=2
    )
    detail: Field[str] = proto_field(
        description="Exact property the hook checks and limits on interpreting a pass.",
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
class HookDeterminism(str, Enum):
    """Whether an identical tree and environment are expected to produce the same result."""

    DETERMINISTIC = "deterministic"
    BEST_EFFORT = "best_effort"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class CommandHook(ClosedModel):
    "A hook that runs an executable, without a shell, inside the tree a change applied to — the workspace itself, or the projection the change named. The command executes that changed tree. The server imports visible filesystem writes from a projection; VCS-ignored output outside its manifest remains untracked."

    type: Field[Literal["command"]] = proto_field()
    id: Field[str] = proto_field(
        description=("Label shown with this hook. It is unique within `hooks`."),
        pattern="^[A-Za-z][A-Za-z0-9_.-]{0,127}$",
        number=1,
    )
    kind: Field[HookKind] = proto_field(
        description="How workspace configuration presents this hook.", number=2
    )
    argv: Field[list[str]] = proto_field(
        description=(
            "Executable followed by literal arguments. An absolute executable path is "
            "refused; a bare name resolves through the `PATH` of the merged environment the "
            "command runs with, and a relative path resolves below `working_directory` in "
            "the changed tree. Rift performs no shell expansion."
        ),
        min_length=1,
        number=3,
    )
    changed_paths: Field[CommandHookChangedPaths] = proto_field(
        description="Whether Rift appends changed `ProjectPath` values to `argv` in byte order.",
        number=4,
    )
    working_directory: Field[core.ProjectPath] = proto_field(
        description="Directory below the changed tree's root in which the process starts. The empty path selects the root.",
        number=5,
    )
    environment: Field[dict[str, str]] = proto_field(
        description=(
            "Environment values added on top of the environment the server inherited. Rift "
            "starts the command directly, without a shell, and supplies its working directory "
            "separately."
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
            "reports the omitted size. The upper bound keeps one escaped hook result "
            "inside one bounded result."
        ),
        ge=256,
        le=4096,
        number=8,
    )
    guarantees: Field[list[HookGuarantee]] = proto_field(
        description=(
            "Behavior or other properties this hook is intended to check. A passing result "
            "turns each declaration into `GuaranteeEvidence`; a failed result attaches a "
            "`ConfirmationRequirement` to the change."
        ),
        number=9,
    )
    determinism: Field[HookDeterminism] = proto_field(
        description="Whether an identical tree and environment are expected to produce the same result.",
        number=10,
    )


@union(
    owner=MCP,
    oneof="variant",
    discriminator="type",
    variants=(Variant("command", "command", 1, CommandHook),),
)
class Hook(ProtocolRoot):
    """One workspace hook from the workspace-root `rift.toml`, run inside the changed tree
    each time a change applies. `type` selects how the hook runs; `command` is the only
    type."""


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "ChangeOrigin",
        (
            EnumValue("rift", "CHANGE_ORIGIN_RIFT", 1),
            EnumValue("filesystem", "CHANGE_ORIGIN_FILESYSTEM", 2),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "rift": "A Rift change tool resolved and applied the change.",
            "filesystem": "A process wrote directly into a projection directory and Rift imported the observed delta.",
        }
    },
)
class ChangeOrigin(str, Enum):
    """How a changeset entry reached its target tree."""

    RIFT = "rift"
    FILESYSTEM = "filesystem"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ChangeSummary(ClosedModel):
    """One change in a changeset, with everything Rift learned while resolving it.
    A change carrying confirmations is recorded the same way as one carrying none; the
    confirmations are what publication checks."""

    id: Field[ChangeId] = proto_field(
        description="Identity of this change in the changeset.", number=1
    )
    origin: Field[ChangeOrigin] = proto_field(
        description="Whether Rift applied the change or imported it from the projection directory.",
        number=2,
    )
    paths: Field[list[core.ProjectPath]] = proto_field(
        description=(
            "Paths whose entries differ because of this change, sorted bytewise. For a "
            "filesystem import this is the authoritative scope even when no portable "
            "`Edit` represents the entry bytes."
        ),
        number=3,
        json_schema_extra={"uniqueItems": True},
    )
    edits: Field[list[core.Edit]] = proto_field(
        description=(
            "Concrete edits in canonical file-and-range order. Empty for a filesystem "
            "import whose binary or symbolic-link delta has no portable `Edit` form."
        ),
        number=4,
    )
    effects: Field[list[core.OperationEffect]] = proto_field(
        description="Semantic effects in emission order.", number=6
    )
    guarantees: Field[list[core.GuaranteeEvidence]] = proto_field(
        description="Scoped guarantee evidence in guarantee-kind order.", number=7
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        description="Resolution findings in source order.", number=8
    )
    advisories: Field[list[core.Advisory]] = proto_field(
        description=(
            "Concerns providers and hooks attached to this change, warnings first. An open "
            "warning instructs the caller; in a projection it also mints an `advisory` "
            "confirmation below."
        ),
        number=9,
    )
    confirmations: Field[list[core.ConfirmationRequirement]] = proto_field(
        description=(
            "Effects the caller has to accept before this change can be published, sorted by "
            "kind, source location, title, and detail. Empty for a change applied directly "
            "to the workspace, where there is no publication to gate, and where every hook "
            "vouched for the result."
        ),
        number=10,
    )

    @model_validator(mode="after")
    def origin_has_evidence(self) -> ChangeSummary:
        if not self.paths:
            raise ValueError("a change must name at least one changed path")
        imports = [
            item
            for item in self.confirmations
            if item.kind is core.ConfirmationRequirementKind.EXTERNAL
        ]
        if self.origin is ChangeOrigin.FILESYSTEM and not imports:
            raise ValueError("filesystem change requires an external confirmation")
        if self.origin is ChangeOrigin.RIFT and imports:
            raise ValueError("Rift change cannot carry an external confirmation")
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ChangesResourcePayload(ClosedModel):
    """One page of a changeset, oldest change first: every change applied, what each one did,
    and which of them nobody vouched for. For a projection this is the read the caller makes
    before publishing; for the workspace it is the journal of what already landed."""

    uri: Field[ChangesResourceUri] = proto_field(
        description="The URI this payload answers for.", number=1
    )
    state: Field[core.ProjectionState | None] = proto_field(
        description=(
            "Projection state the page was read from, or null for the workspace journal — "
            "the workspace tree has no publication to summarize."
        ),
        number=2,
    )
    changes: Field[list[ChangeSummary]] = proto_field(
        description="Changes on this page, in the order they were applied.", number=3
    )
    next: Field[ChangesResourceUri | None] = proto_field(
        default=None,
        description="The same resource carrying the cursor for the next page, or null on the last one.",
        number=4,
    )
    snapshot: Field[core.ReadSnapshot] = proto_field(
        description="Target-tree revision captured after filesystem changes were reconciled.",
        number=5,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ChangeApplied(ClosedModel):
    """The operation resolved to edits and the server wrote them into the targeted tree."""

    status: Field[Literal["applied"]] = proto_field(
        description="Identifies an applied store change.", default="applied"
    )
    summary: Field[ChangeSummary] = proto_field(
        description="The applied change and its evidence.", number=1
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RefusedResult(ClosedModel):
    "Resolution produced no edits, so the targeted tree is untouched."

    status: Field[Literal["refused"]] = proto_field(
        description="Identifies a domain refusal.", default="refused"
    )
    reason: Field[core.RefusalReason] = proto_field(
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
    """An applied change or semantic refusal."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DependencyConflict(ClosedModel):
    """One recorded projection read whose workspace file changed before publication.
    Projection write paths use `conflicts` and do not also appear here. The caller can pass
    this exact value to `accept_dependencies`; another workspace change invalidates it."""

    path: Field[core.ProjectPath] = proto_field(
        description="File whose returned bytes informed work in the projection.",
        number=1,
    )
    observed: Field[core.Digest] = proto_field(
        description="SHA-256 of the file bytes returned by the projection read.",
        number=2,
    )
    current: Field[core.Digest | None] = proto_field(
        description=(
            "Current workspace file digest, or null when the workspace file no longer exists."
        ),
        number=3,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PublishParams(ClosedModel):
    """Publishes one projection into the workspace. Publication settles change confirmations
    and read-dependency conflicts because it is the step that writes into the workspace. The
    caller accepts either condition only by naming its exact evidence."""

    projection: Field[core.ProjectionId] = proto_field(
        description="The projection being published.", number=2
    )
    accept: Field[list[ChangeId]] = proto_field(
        default_factory=list,
        description=(
            "Changes whose confirmations the caller accepts. A change carrying a confirmation "
            "and absent here is returned in `unaccepted` and nothing is written."
        ),
        number=1,
        json_schema_extra={"uniqueItems": True},
    )
    accept_dependencies: Field[list[DependencyConflict]] = proto_field(
        default_factory=list,
        description=(
            "Read-dependency conflicts whose exact `path`, `observed`, and `current` values "
            "the caller accepts. The server recomputes each current digest while holding the "
            "workspace publication lock; a changed value remains in `dependency_conflicts`."
        ),
        number=3,
        json_schema_extra={"uniqueItems": True},
    )

    @model_validator(mode="after")
    def dependency_acceptances_name_unique_paths(self) -> PublishParams:
        paths = [entry.path.root for entry in self.accept_dependencies]
        if len(paths) != len(set(paths)):
            raise ValueError("accept_dependencies must name each path at most once")
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PublishResult(ClosedModel):
    """The result of publishing one projection's changes into the workspace. A successful
    publication clears its changeset and recorded read dependencies; any listed conflict or
    unaccepted change leaves both trees unchanged."""

    state: Field[core.ProjectionState] = proto_field(
        description="Current projection state after the attempt.", number=2
    )
    conflicts: Field[list[core.ProjectPath]] = proto_field(
        description=(
            "Paths changed in both this projection and the workspace. Non-empty means "
            "nothing was written."
        ),
        number=3,
    )
    dependency_conflicts: Field[list[DependencyConflict]] = proto_field(
        description=(
            "Projection-read files whose workspace bytes changed, sorted by path. Each entry "
            "is reusable in `accept_dependencies`; non-empty means nothing was written."
        ),
        number=5,
        json_schema_extra={"uniqueItems": True},
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
        refused = self.conflicts or self.dependency_conflicts or self.unaccepted
        if refused and not self.state.dirty:
            raise ValueError("a refused publication leaves the projection dirty")
        if not refused and self.state.dirty:
            raise ValueError("a successful publication leaves the projection clean")
        return self


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
    """One filesystem entry from the workspace tree."""

    uri: Field[FsResourceUri] = proto_field(
        description="Exact resource request this payload answers.", number=1
    )
    snapshot: Field[core.ReadSnapshot] = proto_field(
        description="Tree revision from which this entry was read.", number=2
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
            "complete `ProjectEntry`, sorted bytewise by path."
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
    digest: Field[core.Digest | None] = proto_field(
        default=None,
        description=(
            "SHA-256 of the complete file for a regular-file read, null for directories and "
            "symlinks. Compare it across pages of one range read: a changed digest means the "
            "file moved underneath the read, and the range starts over at zero."
        ),
        number=11,
    )
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
    """Effective caller-code routing after Rift intersects `rift.toml` with the language's
    runtime capability."""

    execute: Field[bool] = proto_field(
        description=(
            "Whether execute may route to this language after workspace configuration and "
            "runtime capability are applied."
        ),
        number=1,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "ProviderState",
        (
            EnumValue("ready", "PROVIDER_STATE_READY", 1),
            EnumValue("updating", "PROVIDER_STATE_UPDATING", 2),
            EnumValue("unavailable", "PROVIDER_STATE_UNAVAILABLE", 3),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "ready": "The published provider revision covers the current workspace tree.",
            "updating": "The provider serves its previous immutable revision while deriving the current tree.",
            "unavailable": "The provider has no revision it can serve.",
        }
    },
)
class ProviderState(str, Enum):
    """Lifecycle state of one provider for one language."""

    READY = "ready"
    UPDATING = "updating"
    UNAVAILABLE = "unavailable"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProviderSummary(ClosedModel):
    """One provider serving a language, and the fact families it produces there. Facts from
    several providers merge in `LanguageSupport.providers` order, and per-family coverage
    records the revisions that contributed."""

    implementation: Field[str] = proto_field(
        description="Implementation name and version the provider reports.",
        max_length=4096,
        examples=["rift-syntax 0.4.1", "rift-python 0.4.1"],
        number=1,
    )
    fact_families: Field[list[core.FactFamily]] = proto_field(
        description="Fact families this provider produces for the language, sorted by protocol order.",
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
    id: Field[core.ProviderId] = proto_field(
        description="Stable provider identity used by per-answer provenance.", number=3
    )
    revision: Field[core.Digest | None] = proto_field(
        default=None,
        description=(
            "Latest immutable fact revision this provider can serve. Null when `state` is "
            "`unavailable`."
        ),
        number=4,
    )
    state: Field[ProviderState] = proto_field(
        description="Current lifecycle state for this language.", number=5
    )

    @model_validator(mode="after")
    def state_has_revision(self) -> ProviderSummary:
        if self.state is ProviderState.UNAVAILABLE and self.revision is not None:
            raise ValueError("unavailable provider cannot advertise a revision")
        if self.state is not ProviderState.UNAVAILABLE and self.revision is None:
            raise ValueError("serving provider requires a revision")
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class LanguageSupport(ClosedModel):
    """What serves one language: the providers producing facts, and the effective
    caller-code availability."""

    language: Field[core.Language] = proto_field(
        description="Language name and optional dialect.",
        number=1,
    )
    providers: Field[list[ProviderSummary]] = proto_field(
        description=(
            "Providers producing facts for this language, in merge precedence order. The "
            "first provider supplying a scalar field wins; multi-valued facts are unioned "
            "by canonical identity."
        ),
        number=2,
    )
    execution: Field[ExecutionAvailability] = proto_field(
        description="Effective workspace configuration and runtime capability for caller code.",
        number=6,
    )


MODELS = (
    Cursor,
    SourceExcerpt,
    SearchScope,
    SearchParams,
    SearchHit,
    ResourceFamily,
    ResourceLink,
    WorkspacePath,
    ChangeId,
    ServerLock,
    Projection,
    ProjectionCreateParams,
    ProjectionListParams,
    ProjectionListResult,
    ProjectionRemoveParams,
    ProjectionRemoveResult,
    GetSymbolParams,
    GetSymbolHit,
    GetSymbolResult,
    NodesParams,
    NodesResult,
    ExecutionLimits,
    Limits,
    WorkspaceResourceUri,
    ProjectionResourceUri,
    ChangesResourceUri,
    FsResourceUri,
    WorkspaceResourcePayload,
    ProjectionResourcePayload,
    SearchResult,
    ExecuteParams,
    ExecuteResult,
    DiagnosticContext,
    ErrorCode,
    RetryDirective,
    ErrorCause,
    ErrorData,
    LimitEvidence,
    ResourceTemplate,
    ResourceContent,
    ResourceReadParams,
    ResourceReadResult,
    SearchHitTarget,
    ReplaceSymbolParams,
    InsertSymbolParams,
    ReplaceNodeParams,
    PatchParams,
    ProjectionRestoreParams,
    Hook,
    CommandHook,
    ChangeOrigin,
    ChangeSummary,
    ChangesResourcePayload,
    ChangeResult,
    DependencyConflict,
    PublishParams,
    PublishResult,
    ResultOrder,
    FsResourcePayload,
    ExecutionAvailability,
    SourceResolverState,
    SourceResolverSummary,
    ProviderState,
    ProviderSummary,
    LanguageSupport,
)
