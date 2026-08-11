from __future__ import annotations

import base64
from urllib.parse import parse_qs, quote, unquote_to_bytes

from pydantic import field_validator, model_validator

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
        description="The source itself, as it stood at the answer's snapshot.", number=2
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class TreeParams(ClosedModel):
    "Selects a portion of the project tree. The root controls hierarchy and `paths` filters the full project-relative path of each descendant."

    root: Field[core.ProjectPath] = proto_field(
        description=(
            "Directory whose descendants are listed. The empty path selects the project root; "
            "the root entry itself is not returned. A gitlink is a file entry and cannot be "
            "used as a directory root."
        ),
        number=1,
    )
    depth: Field[int | None] = proto_field(
        description=(
            "Most directory edges below `root` to include. One lists immediate children; null "
            "walks every descendant and relies on pagination and response limits."
        ),
        number=2,
    )
    paths: Field[core.PathSelector] = proto_field(
        description="Git-style include and exclude globs matched against each full project-relative descendant path.",
        number=3,
    )
    limit: Field[int | None] = proto_field(
        default=None,
        description=(
            "Most entries to return in one page. The server may stop earlier to keep the "
            "serialized result within `max_response_bytes`."
        ),
        ge=1,
        le=10000,
        number=4,
    )
    cursor: Field[Cursor | None] = proto_field(
        default=None,
        description="Continues the same tree walk. Root, depth, paths, revision, and page size remain fixed.",
        number=5,
    )
    rev: Field[core.Revision | None] = proto_field(
        default=None,
        description="Revision whose tree is listed. Omission selects the current session state.",
        number=6,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class TreeResult(ClosedModel):
    "One page of a project tree, ordered by project-path UTF-8 bytes. A directory precedes a file at the same path, though a valid snapshot normally contains only one."

    at: Field[core.ResolvedSnapshot] = proto_field(
        description="Snapshot whose paths were listed.", number=1
    )
    entries: Field[list[core.ProjectEntry]] = proto_field(
        description="Derived directories and Git entries on this page.", number=2
    )
    next_cursor: Field[Cursor | None] = proto_field(
        description="Cursor for the next path page, or null after the final entry.",
        number=3,
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
class OutlineParamsIncludeItemInclude(str, Enum):
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
    include: Field[list[OutlineParamsIncludeItemInclude]] = proto_field(
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
    rev: Field[core.Revision | None] = proto_field(
        default=None,
        description="Revision whose file and semantic facts are read. Omission selects the current session state.",
        number=6,
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

    at: Field[core.ResolvedSnapshot] = proto_field(
        description="Snapshot from which the file and semantic facts were read.",
        number=1,
    )
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
class SearchParamsIncludeItemInclude(str, Enum):
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
        json_schema_extra={
            "rift:enumDescriptions": {
                "symbol": "Declarations the adapter resolved — a function, a class, a trait.",
                "node": "Places in a syntax tree where a symbol is written. One symbol has many.",
                "file": "Entries of the workspace tree. The only target that answers with no adapter installed.",
                "all": "Every kind above, in one ranked list.",
            }
        },
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
            "Files eligible for the search, selected by project-relative Git globs. Omitted "
            "selects every visible file."
        ),
        number=5,
    )
    include: Field[list[SearchParamsIncludeItemInclude] | None] = proto_field(
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
            "Most hits to return in one page. `max_page_items` from the repository resource "
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
    rev: Field[core.Revision | None] = proto_field(
        default=None,
        description="Which revision to answer against. Omission selects the current session state.",
        number=9,
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


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ActionsResourceLink(ClosedModel):
    """A link to the actions available at one address."""

    uri: Field[ActionsResourceUri] = proto_field(
        description="The address to read, optionally filtered by kind and continued by a cursor.",
        number=1,
    )
    name: Field[Literal["actions"]] = proto_field(
        description="The resource family this link belongs to.", number=2
    )
    mimeType: Field[Literal["application/vnd.rift.actions+json"]] = proto_field(
        description="What a read of this URI returns: `ActionsResourcePayload` as JSON.",
        number=3,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ActionResourceLink(ClosedModel):
    """A link to one discovered action and the arguments it takes."""

    uri: Field[ActionResourceUri] = proto_field(
        description="The offer to read.", number=1
    )
    name: Field[Literal["action"]] = proto_field(
        description="The resource family this link belongs to.", number=2
    )
    mimeType: Field[Literal["application/vnd.rift.action+json"]] = proto_field(
        description="What a read of this URI returns: `ActionResourcePayload` as JSON.",
        number=3,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class SymbolResourceLink(ClosedModel):
    """A link to the symbol resource, carrying the symbol's own URI."""

    type: Field[Literal["resource_link"]] = proto_field(
        description="MCP's tag for a link to a resource inside tool output."
    )
    uri: Field[core.SymbolId] = proto_field(
        description="The symbol to read. Hand it to `resources/read` unchanged.",
        number=1,
    )
    name: Field[Literal["symbol"]] = proto_field(
        description="The resource family this link belongs to.", number=2
    )
    mimeType: Field[Literal["application/vnd.rift.symbol+json"]] = proto_field(
        description="What a read of this URI returns: `SymbolResourcePayload` as JSON.",
        number=3,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RepositoryResourceLink(ClosedModel):
    """A link to the repository resource."""

    type: Field[Literal["resource_link"]] = proto_field(
        description="MCP's tag for a link to a resource inside tool output."
    )
    uri: Field[RepositoryResourceUri] = proto_field(
        description="The repository resource, optionally pinned to a revision and continued by a cursor.",
        number=1,
    )
    name: Field[Literal["repository"]] = proto_field(
        description="The resource family this link belongs to.", number=2
    )
    mimeType: Field[Literal["application/vnd.rift.repository+json"]] = proto_field(
        description="What a read of this URI returns: `RepositoryResourcePayload` as JSON.",
        number=3,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class DiffResourceLink(ClosedModel):
    """A link to the comparison between two revisions."""

    type: Field[Literal["resource_link"]] = proto_field(
        description="MCP's tag for a link to a resource inside tool output."
    )
    uri: Field[core.DiffId] = proto_field(
        description="The comparison to read, optionally continued by a cursor.",
        number=1,
    )
    name: Field[Literal["diff"]] = proto_field(
        description="The resource family this link belongs to.", number=2
    )
    mimeType: Field[Literal["application/vnd.rift.diff+json"]] = proto_field(
        description="What a read of this URI returns: `DiffResourcePayload` as JSON.",
        number=3,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FileResourceLink(ClosedModel):
    """A link to one file at one revision."""

    type: Field[Literal["resource_link"]] = proto_field(
        description="MCP's tag for a link to a resource inside tool output."
    )
    uri: Field[FileResourceUri] = proto_field(
        description="The file range to read, optionally pinned to a revision.", number=1
    )
    name: Field[Literal["file"]] = proto_field(
        description="The resource family this link belongs to.", number=2
    )
    mimeType: Field[Literal["application/vnd.rift.file+json"]] = proto_field(
        description="What a read of this URI returns: `FileResourcePayload` as JSON.",
        number=3,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FsResourceLink(ClosedModel):
    """A link to the live filesystem-projection inventory."""

    type: Field[Literal["resource_link"]] = proto_field(
        description="MCP's tag for a link to a resource inside tool output."
    )
    uri: Field[FsResourceUri] = proto_field(
        description="Filesystem inventory page to read.", number=1
    )
    name: Field[Literal["fs"]] = proto_field(
        description="The resource family this link belongs to.", number=2
    )
    mimeType: Field[Literal["application/vnd.rift.fs+json"]] = proto_field(
        description="What a read returns: `FsResourcePayload` as JSON.",
        number=3,
        proto_name="mime_type",
    )


@union(
    owner=MCP,
    oneof="variant",
    discriminator="mimeType",
    variants=(
        Variant("symbol", "symbol", 1, SymbolResourceLink),
        Variant("repository", "repository", 2, RepositoryResourceLink),
        Variant("diff", "diff", 3, DiffResourceLink),
        Variant("file", "file", 4, FileResourceLink),
        Variant("actions", "actions", 5, ActionsResourceLink),
        Variant("action", "action", 6, ActionResourceLink),
        Variant("fs", "fs", 7, FsResourceLink),
    ),
)
class ResourceLink(ProtocolRoot):
    "A link to one Rift resource, as MCP carries it. The resource's name and media type are fixed per resource, and the URI is the one that resource accepts."


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SymbolResourcePayload(ClosedModel):
    """JSON payload for one symbol at one snapshot."""

    uri: Field[core.SymbolId] = proto_field(
        description="The symbol this payload answers for, echoed back so a link and its content carry the same address.",
        number=1,
    )
    at: Field[core.ResolvedSnapshot] = proto_field(
        description=(
            "The snapshot this answer resolved against. Select its id with a tagged snapshot "
            "revision while that tree remains pinned."
        ),
        number=2,
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
    next: Field[core.SymbolId | None] = proto_field(
        description=(
            "The same symbol URI carrying the cursor for the next page, or null on the last "
            "one. Nodes, edges and diagnostics are what page."
        ),
        number=10,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DiffResourcePayload(ClosedModel):
    "One page of a store-tree comparison. `from` and `to` record the resolved revisions. No adapter is needed, so the comparison works for every file."

    uri: Field[core.DiffId] = proto_field(
        description="The comparison this payload answers for, echoed back with the cursor that produced this page.",
        number=1,
    )
    from_: Field[core.ResolvedSnapshot] = proto_field(
        alias="from", description="The old side, as it resolved.", number=2
    )
    to: Field[core.ResolvedSnapshot] = proto_field(
        description="The new side, as it resolved.", number=3
    )
    files: Field[list[core.FileChange]] = proto_field(
        description="The files this page covers.", number=4
    )
    truncated: Field[bool] = proto_field(
        description="Whether files were dropped to stay inside the size limit. Paging past the limit uses `next`.",
        number=5,
    )
    next: Field[core.DiffId | None] = proto_field(
        description="The same comparison carrying the cursor for the next page, or null on the last one.",
        number=6,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "ProjectionKind",
        (
            EnumValue("session", "PROJECTION_KIND_SESSION", 1),
            EnumValue("read", "PROJECTION_KIND_READ", 2),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "session": "Persistent writable source state owned by one session.",
            "read": "Explicitly opened immutable snapshot for external read-only tools.",
        }
    },
)
class ProjectionKind(str, Enum):
    """Lifecycle and write policy of one filesystem projection."""

    SESSION = "session"
    READ = "read"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class FilesystemProjectionSummary(ClosedModel):
    """One externally addressable session or explicitly opened read projection."""

    location: Field[ProjectionLocation] = proto_field(
        description="Mounted identity and absolute path.", number=1
    )
    kind: Field[ProjectionKind] = proto_field(
        description="Lifecycle and write policy.", number=2
    )
    snapshot: Field[core.Snapshot] = proto_field(
        description="Exact source tree currently rendered.", number=3
    )
    state: Field[core.ProjectionState | None] = proto_field(
        default=None,
        description="Mutable session state, or null for immutable and disposable projections.",
        number=4,
    )
    writable: Field[bool] = proto_field(
        description="Whether ordinary filesystem writes are accepted.", number=5
    )
    scratch_bytes: Field[int] = proto_field(
        description="Bytes in its non-source scratch layer, excluding shared adapter state.",
        ge=0,
        le=9007199254740991,
        number=6,
    )
    open_handles: Field[int] = proto_field(
        description="Open file and directory handles currently pinning this projection.",
        ge=0,
        le=4294967295,
        number=7,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
    )
    available: Field[bool] = proto_field(
        description="Whether the frontend currently accepts new opens and coherent reads.",
        number=8,
    )
    degradation: Field[list[core.Diagnostic]] = proto_field(
        description="Frontend invalidation or notification failures while unavailable.",
        number=9,
    )

    @model_validator(mode="after")
    def state_matches_location(self) -> FilesystemProjectionSummary:
        if self.kind is ProjectionKind.SESSION:
            if self.state is None or not self.writable:
                raise ValueError("session projections require writable current state")
            if self.state.projection != self.location.projection:
                raise ValueError(
                    "projection location and state must name the same projection"
                )
            if self.state.snapshot != self.snapshot.id:
                raise ValueError(
                    "projection state and snapshot must name the same tree"
                )
        elif self.state is not None or self.writable:
            raise ValueError(
                "read projections are immutable and have no ProjectionState"
            )
        if self.available == bool(self.degradation):
            raise ValueError("degradation must be non-empty exactly while unavailable")
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class FsResourcePayload(ClosedModel):
    """One page of a connection-scoped captured Rift filesystem inventory. The first page
    captures externally addressable projections in identity order; its cursor reads that same
    capture and grants no close authority."""

    uri: Field[FsResourceUri] = proto_field(
        description="Resource page that produced this payload.", number=1
    )
    projections: Field[list[FilesystemProjectionSummary]] = proto_field(
        description="Live projections in identity order.", number=2
    )
    next: Field[FsResourceUri | None] = proto_field(
        description="Next captured page, or null after the final projection.", number=3
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class Contract(ClosedModel):
    "One generated wire contract. A peer accepts it only when all three fields match one of its own generated descriptors."

    major: Field[core.ProtocolVersion] = proto_field(
        description="Breaking protocol generation selected before the socket is opened.",
        number=1,
    )
    minor: Field[int] = proto_field(
        description="Additive revision within the selected major.",
        ge=0,
        le=4294967295,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
    )
    schema_digest: Field[core.Digest] = proto_field(
        description=(
            "SHA-256 of the generated descriptor and its MCP conversion metadata. Equal major "
            "and minor values with different digests are incompatible."
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
    public=False,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    min_length=1,
    max_length=32768,
    examples=[
        "/home/alice/projects/rift/.rift/projections/prj_aaaaaaaaaaaaaaaaaaaaaaaaaa",
        "C:\\Users\\alice\\projects\\rift\\.rift\\projections\\prj_aaaaaaaaaaaaaaaaaaaaaaaaaa",
    ],
)
class ProjectionPath(ProtocolRoot):
    """Canonical absolute path of one mounted Rift filesystem projection on this host."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionLocation(ClosedModel):
    """One mounted Rift filesystem projection, with its store identity and host path."""

    projection: Field[core.Projection] = proto_field(
        description="Stable projection identity accepted by a namespaced `Revision` selector.",
        number=1,
    )
    path: Field[ProjectionPath] = proto_field(
        description="Canonical absolute path through which ordinary filesystem tools reach it.",
        number=2,
    )


@scalar(
    owner=MCP,
    public=False,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    min_length=1,
    max_length=32768,
    examples=[
        "/home/alice/projects/rift/.rift/recovery/rec_aaaaaaaaaaaaaaaaaaaaaaaaaa"
    ],
)
class GitWorktreePath(ProtocolRoot):
    """Canonical absolute path of an exceptional conventional Git recovery worktree."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class GitWorktreeLocation(ClosedModel):
    """Conventional Git worktree created only for unresolved integration recovery."""

    path: Field[GitWorktreePath] = proto_field(
        description="Canonical absolute path accepted by `git -C`.", number=1
    )
    ref: Field[str] = proto_field(
        description="Private `refs/rift/recovery/<RecoveryId>` ref associated with this path.",
        min_length=1,
        max_length=1024,
        number=2,
    )


@scalar(
    owner=MCP,
    public=False,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^ses_[a-z2-7]{26}$",
)
class SessionId(ProtocolRoot):
    """Random 128-bit identity generated and retained in memory by one `rift mcp` process. It
    names one persistent filesystem projection. The server admits at most one live MCP
    connection for the identity."""


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
        description=(
            "Canonical absolute UTF-8 path through which the client reached `.rift`. It must "
            "match the physical workspace served by this endpoint."
        ),
        number=5,
    )
    client_build: Field[str] = proto_field(
        description="Client build as it names itself in diagnostics.",
        min_length=1,
        max_length=256,
        number=6,
    )
    initial_revision: Field[core.GitRevision | None] = proto_field(
        default=None,
        description=(
            "Git revision imported as the base of a newly created MCP session. Omission uses "
            "`session.base` configuration. Reconnection and SCIP roles require null."
        ),
        number=7,
    )

    @model_validator(mode="after")
    def role_has_valid_session_fields(self) -> ConnectRequest:
        if self.role == ConnectRole.MCP and self.session is None:
            raise ValueError("MCP connections require a process-generated session")
        if self.role == ConnectRole.SCIP and self.session is not None:
            raise ValueError("SCIP connections cannot carry a session")
        if self.role == ConnectRole.SCIP and self.initial_revision is not None:
            raise ValueError("SCIP connections cannot create a session base")
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
                "then": {
                    "properties": {
                        "projection": {"type": "null"},
                        "state": {"type": "null"},
                    }
                },
                "else": {
                    "properties": {
                        "projection": {"not": {"type": "null"}},
                        "state": {"not": {"type": "null"}},
                    },
                    "required": ["projection", "state"],
                },
            }
        ]
    },
)
class Connected(ClosedModel):
    """The first response on an accepted control stream, including persistent projection state
    for an MCP role."""

    contract: Field[Contract] = proto_field(
        description="Exact generated contract selected for this connection.", number=1
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
    projection: Field[ProjectionLocation | None] = proto_field(
        default=None,
        description="Mounted persistent session projection, or null for a SCIP role.",
        number=6,
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
            if self.projection is not None or self.state is not None:
                raise ValueError("SCIP connections cannot carry session state")
        elif self.projection is None or self.state is None:
            raise ValueError("MCP connections require the session projection and state")
        elif self.state.projection != self.projection.projection:
            raise ValueError(
                "projection location and state must name the same projection"
            )
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SessionSummary(ClosedModel):
    """One retained store-backed session that an MCP connection may continue or remove."""

    session: Field[SessionId] = proto_field(
        description="Persistent session identity.",
        number=1,
    )
    projection: Field[ProjectionLocation] = proto_field(
        description="Mounted persistent filesystem projection owned by this session.",
        number=3,
    )
    state: Field[core.ProjectionState] = proto_field(
        description="Exact current source state and concurrency token of the projection.",
        number=5,
    )
    active: Field[bool] = proto_field(
        description="Whether one live MCP connection currently owns this session.",
        number=4,
    )

    @model_validator(mode="after")
    def projection_is_correlated(self) -> SessionSummary:
        if self.state.projection != self.projection.projection:
            raise ValueError(
                "projection location and state must name the same projection"
            )
        return self


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
    """Previews or confirms removal of one retained session at an exact projection state. A
    session with a live connection returns `temporarily_unavailable`. A changed projection fails
    with `projection_head_moved`, so confirmation cannot discard writes the preview did not observe."""

    session: Field[SessionId] = proto_field(
        description="Retained session to inspect or remove.", number=1
    )
    expected: Field[core.ProjectionState] = proto_field(
        description="Projection state observed by the caller. Its head token prevents ABA races.",
        number=2,
    )
    confirm: Field[bool] = proto_field(
        default=False,
        description="False previews the removal. True applies the same plan when the head still matches.",
        number=3,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SessionRemoveResult(ClosedModel):
    """The projection affected by a session-removal preview or confirmation."""

    session: Field[SessionId] = proto_field(
        description="Session the result describes.", number=1
    )
    state: Field[core.ProjectionState] = proto_field(
        description="Exact projection state checked by this preview or confirmation.",
        number=2,
    )
    projection: Field[ProjectionLocation] = proto_field(
        description="Projection unmounted and removed by confirmation, or selected by preview.",
        number=3,
    )
    scratch_bytes: Field[int] = proto_field(
        description=(
            "Estimated disposable scratch bytes for preview, or actual bytes deleted after "
            "confirmation verified no handles and withdrew the namespace."
        ),
        ge=0,
        le=9007199254740991,
        number=4,
    )
    removed: Field[bool] = proto_field(
        description="True after confirmation removed the projection; false for a preview.",
        number=5,
    )
    unintegrated: Field[bool] = proto_field(
        description="Whether current source differs from the projection's retained Git base.",
        number=6,
    )
    reclaimable_bytes: Field[int] = proto_field(
        description="Estimated store bytes made collectible after removal and final pin release.",
        ge=0,
        le=9007199254740991,
        number=7,
    )

    @model_validator(mode="after")
    def projection_is_correlated(self) -> SessionRemoveResult:
        if self.state.projection != self.projection.projection:
            raise ValueError(
                "projection location and state must name the same projection"
            )
        if self.unintegrated != self.state.dirty:
            raise ValueError("unintegrated must equal the projection dirty state")
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionOpenParams(ClosedModel):
    """Opens an immutable snapshot as a mounted read projection for external tools. The
    projection is owned by this connection and remains pinned until `projection_close` or
    connection cleanup."""

    at: Field[core.Revision] = proto_field(
        description="Git commit, retained snapshot, or current projection state to resolve once.",
        number=1,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionOpenResult(ClosedModel):
    """New read-only filesystem projection and the exact snapshot it pins."""

    location: Field[ProjectionLocation] = proto_field(
        description="Mounted path available to ordinary filesystem tools.", number=1
    )
    snapshot: Field[core.Snapshot] = proto_field(
        description="Exact immutable source tree rendered at that path.", number=2
    )
    imported_from: Field[core.GitCommit | None] = proto_field(
        default=None,
        description="Exact peeled commit when `at` used a Git revision, otherwise null.",
        number=3,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionCloseParams(ClosedModel):
    """Releases one explicit read projection. Session and internal projections cannot be closed
    through this operation, and another connection cannot close the caller's projection."""

    projection: Field[core.Projection] = proto_field(
        description="Read projection returned by `projection_open`.", number=1
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ProjectionCloseResult(ClosedModel):
    """Confirmation that the read namespace no longer accepts opens. Existing handles retain
    only their inode version or captured directory enumeration; the final projection pin is
    released after they close."""

    projection: Field[core.Projection] = proto_field(
        description="Projection that was closed.", number=1
    )
    closed: Field[Literal[True]] = proto_field(
        description="The projection no longer accepts new opens.", number=2
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
    max_changes: Field[int] = proto_field(
        description="Most top-level `Change` values one mutation request accepts.",
        ge=1,
        le=4294967295,
        number=7,
    )
    max_edits: Field[int] = proto_field(
        description="Most concrete `Edit` values one resolved operation may contain across every change.",
        ge=1,
        le=1000000,
        number=8,
    )
    max_validators: Field[int] = proto_field(
        description=(
            "How many repository-declared command checks may run before one integration. "
            "Zero when `rift.toml` declares none."
        ),
        ge=0,
        le=4294967295,
        number=9,
    )
    max_rewrite_expansions: Field[int] = proto_field(
        description="Most concrete edits one atomic `RewriteChange` may produce after matching.",
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
    max_store_bytes: Field[int] = proto_field(
        description="Workspace ceiling for authoritative source, scratch, pins, and integration intents.",
        ge=1048576,
        le=9007199254740991,
        number=12,
    )
    max_store_entries: Field[int] = proto_field(
        description="Workspace ceiling for source, scratch, pin, and intent records.",
        ge=1024,
        le=9007199254740991,
        number=13,
    )


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://repository(\?(rev=(?:git:(?:[A-Za-z0-9._~-]|%[0-9A-F]{2}){1,256}|snapshot:snap_[0-9a-f]{64}|projection:prj_[a-z2-7]{26})(&cursor=[A-Za-z0-9_-]{1,4096})?|cursor=[A-Za-z0-9_-]{1,4096}))?$",
    max_length=8192,
)
class RepositoryResourceUri(ProtocolRoot):
    """Paginated workspace metadata and capabilities alongside one resolved source state. An
    omitted `rev` selects the current session state."""

    @model_validator(mode="after")
    def query_is_canonical(self) -> RepositoryResourceUri:
        query = _raw_resource_query(self.root)
        revision = query.get("rev")
        if revision is not None:
            core.validate_resource_revision(revision)
        cursor = query.get("cursor")
        if cursor is not None:
            Cursor.model_validate(cursor)
        return self


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://fs(?:\?cursor=[A-Za-z0-9_-]{1,4096})?$",
    max_length=4113,
)
class FsResourceUri(ProtocolRoot):
    """URI for one page of filesystem projections currently exposed below
    `.rift/projections`. It reports live namespaces, not past projection states."""

    @model_validator(mode="after")
    def cursor_is_canonical(self) -> FsResourceUri:
        cursor = _raw_resource_query(self.root).get("cursor")
        if cursor is not None:
            Cursor.model_validate(cursor)
        return self


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://file/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-F]{2}){1,1000}(?:\?rev=(?:git:(?:[A-Za-z0-9._~-]|%[0-9A-F]{2}){1,256}|snapshot:snap_[0-9a-f]{64}|projection:prj_[a-z2-7]{26})(?:&start=[0-9]+&length=[1-9][0-9]*)?|\?start=[0-9]+&length=[1-9][0-9]*)?$",
    min_length=13,
    max_length=8192,
)
class FileResourceUri(ProtocolRoot):
    """URI for one file content range. An omitted `rev` selects the current session state.
    `start` and `length` are byte coordinates and appear together. Their absence starts at byte
    zero with the server's advertised chunk bound."""

    @model_validator(mode="after")
    def range_is_bounded(self) -> FileResourceUri:
        encoded_path = self.root.removeprefix("rift://file/").partition("?")[0]
        decoded_path = unquote_to_bytes(encoded_path).decode("utf-8")
        core.ProjectPath.model_validate(decoded_path)
        canonical = quote(decoded_path, safe="/!$&'()*+,;=:@-._~")
        if canonical != encoded_path:
            raise ValueError("file path must use canonical URI encoding")
        raw_query = _raw_resource_query(self.root)
        revision = raw_query.get("rev")
        if revision is not None:
            core.validate_resource_revision(revision)
        query = self.root.partition("?")[2]
        if not query:
            return self
        values = parse_qs(query, keep_blank_values=True, strict_parsing=True)
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
    public=False,
    proto=Proto.enum(
        "Resources",
        (
            EnumValue("repository", "RESOURCES_REPOSITORY", 1),
            EnumValue("symbol", "RESOURCES_SYMBOL", 2),
            EnumValue("diff", "RESOURCES_DIFF", 3),
            EnumValue("file", "RESOURCES_FILE", 4),
            EnumValue("actions", "RESOURCES_ACTIONS", 5),
            EnumValue("action", "RESOURCES_ACTION", 6),
            EnumValue("fs", "RESOURCES_FS", 7),
        ),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "repository": "Workspace capabilities, resolved state, and request limits.",
            "symbol": "One symbol, its nodes, its edges and its diagnostics.",
            "diff": "What changed between two revisions.",
            "file": "One file's tree entry and its bytes.",
            "actions": "The fixes and refactors an adapter offers at one address, or across one file.",
            "action": "One discovered action, with the schema of the arguments it takes.",
            "fs": "Mounted filesystem projections and their current source state.",
        }
    },
)
class RepositoryResourcePayloadResourcesItemResources(str, Enum):
    REPOSITORY = "repository"
    SYMBOL = "symbol"
    DIFF = "diff"
    FILE = "file"
    ACTIONS = "actions"
    ACTION = "action"
    FS = "fs"


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "ObjectFormat",
        (
            EnumValue("sha1", "OBJECT_FORMAT_SHA1", 1),
            EnumValue("sha256", "OBJECT_FORMAT_SHA256", 2),
        ),
        placement=Placement("object_format", 10),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "sha1": "Object IDs are 40 hex characters. Still git's default.",
            "sha256": "Object IDs are 64 hex characters. A repository created with `--object-format=sha256`.",
        }
    },
)
class RepositoryResourcePayloadObjectFormat(str, Enum):
    "Git object hash used by this repository. It determines whether a `GitCommit` contains 40 or 64 hexadecimal characters."

    SHA1 = "sha1"
    SHA256 = "sha256"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RepositoryResourcePayload(ClosedModel):
    """Current repository state and request limits. Each configured language reports its
    adapter support and effective caller-code availability."""

    uri: Field[RepositoryResourceUri] = proto_field(
        description="The URI this payload answers for, echoed back with the revision and cursor it resolved.",
        number=1,
    )
    at: Field[core.ResolvedSnapshot] = proto_field(
        description=(
            "The snapshot this answer was resolved against. Use a `Revision` with kind "
            "`snapshot` and this id when a later call must select the same retained tree."
        ),
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
    resources: Field[list[RepositoryResourcePayloadResourcesItemResources]] = (
        proto_field(
            description="The MCP resource families this workspace serves.",
            number=8,
            json_schema_extra={"uniqueItems": True},
        )
    )
    next: Field[RepositoryResourceUri | None] = proto_field(
        description="The same resource carrying the cursor for the next page, or null on the last one.",
        number=9,
    )
    object_format: Field[RepositoryResourcePayloadObjectFormat] = proto_field(
        description=(
            "Which hash git uses for object IDs in this repository. It decides how long a "
            "`GitCommit` is, so a client that validates one has to read this first."
        ),
        number=10,
        json_schema_extra={
            "rift:enumDescriptions": {
                "sha1": "Object IDs are 40 hex characters. Still git's default.",
                "sha256": "Object IDs are 64 hex characters. A repository created with `--object-format=sha256`.",
            }
        },
    )
    matching: Field[MatchSyntax] = proto_field(
        description="The pattern grammars the `match` tool accepts here.", number=11
    )
    validators: Field[list[CommandValidator]] = proto_field(
        description="Integration checks declared by the workspace-root `rift.toml`, in execution order.",
        number=13,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SearchResult(ClosedModel):
    "One page of search hits, and what the page is worth. `coverage` is what makes an empty page readable: nothing matched, or Rift could not see far enough to know."

    at: Field[core.ResolvedSnapshot] = proto_field(
        description=(
            "The snapshot this answer resolved against. Select its id with a tagged snapshot "
            "revision while that tree remains pinned."
        ),
        number=1,
    )
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
    pattern=r"^rift://actions/(?:symbol|node|match|file)/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-F]{2}){1,8192}(?:\?only=[a-z][a-z0-9_.-]*(?:&rev=(?:git:(?:[A-Za-z0-9._~-]|%[0-9A-F]{2}){1,256}|snapshot:snap_[0-9a-f]{64}|projection:prj_[a-z2-7]{26}))?(?:&cursor=[A-Za-z0-9_-]{1,4096})?|\?rev=(?:git:(?:[A-Za-z0-9._~-]|%[0-9A-F]{2}){1,256}|snapshot:snap_[0-9a-f]{64}|projection:prj_[a-z2-7]{26})(?:&cursor=[A-Za-z0-9_-]{1,4096})?|\?cursor=[A-Za-z0-9_-]{1,4096})?$",
    min_length=22,
    max_length=32768,
    examples=[
        "rift://actions/file/src/api.rs?only=quickfix",
        "rift://actions/symbol/python/pkg.util.load_config",
    ],
)
class ActionsResourceUri(ProtocolRoot):
    """URI for the actions an adapter offers at one place. The path after `rift://actions/` is the address: `symbol/<language>/<name>`, `node/<language>/<path>@<start>-<end>`, `match/<token>`, or `file/<path>` for every offer in one file. A file address is what asks for the fixes across a file whose diagnostics an agent is working through. `?only=` keeps one kind prefix, `?rev=` selects the revision, and `?cursor=` continues the page. An omitted `rev` selects the current session state."""

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
        revision = query.get("rev")
        if revision is not None:
            core.validate_resource_revision(revision)
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
    "One page of the actions available at one address. Offers sort by language name, dialect with null first, target, kind, and offer identity. The cursor binds that order, the snapshot, and the adapter build."

    uri: Field[ActionsResourceUri] = proto_field(
        description="The address this page answers for, echoed back as it resolved.",
        number=1,
    )
    at: Field[core.ResolvedSnapshot] = proto_field(
        description=(
            "The snapshot this answer resolved against. Select its id with a tagged snapshot "
            "revision while that tree remains pinned."
        ),
        number=2,
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
    at: Field[core.ResolvedSnapshot] = proto_field(
        description="The snapshot this offer was discovered in.", number=2
    )
    language: Field[core.Language] = proto_field(
        description="Language whose adapter minted the offer and resolves it.", number=3
    )
    offer: Field[ActionOffer] = proto_field(
        description="The offer, with `descriptor.arguments_schema` present.", number=4
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ExecuteParams(ClosedModel):
    """Evaluate a caller-provided code block with one configured language adapter against an
    exact revision. The server materializes a disposable execution workspace before crossing
    the adapter seam."""

    language: Field[core.Language] = proto_field(
        description="Exact language and optional dialect selecting the adapter.",
        number=1,
    )
    block: Field[core.CodeBlock] = proto_field(
        description="Source to evaluate and its project-relative working directory.",
        number=2,
    )
    rev: Field[core.Revision | None] = proto_field(
        default=None,
        description="Revision whose project source and runtime files provide context. Omission selects the current session state. Rift configuration remains the physical workspace root's `rift.toml`.",
        number=3,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ExecuteResult(ClosedModel):
    """Bounded result of one execution. Writes made by evaluated code are absent because its
    execution workspace is discarded."""

    at: Field[core.ResolvedSnapshot] = proto_field(
        description="Exact snapshot used to prepare the execution workspace.", number=1
    )
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
    at: Field[core.ResolvedSnapshot] = proto_field(
        description="Snapshot retained by the session's execution workspace.", number=2
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
    rev: Field[core.Revision | None] = proto_field(
        default=None,
        description="Revision whose project source and runtime files provide context. Omission selects the current session state. Rift configuration remains the physical workspace root's `rift.toml`.",
        number=3,
    )


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
    "A match request: the query, a page size, and the revision to run it against. The complete query travels in every match key, so a key can be inspected and replayed without hidden lookup state."

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
    rev: Field[core.Revision | None] = proto_field(
        default=None,
        description="Which revision to answer against. Omission selects the current session state.",
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class MatchResult(ClosedModel):
    "One page of matches and the state they were found in. Matches sort by file bytes, range, and canonical key. Rift checks the key against `at` before applying an addressed edit."

    at: Field[core.ResolvedSnapshot] = proto_field(
        description=(
            "The snapshot this answer resolved against. Select its id with a tagged snapshot "
            "revision while that tree remains pinned."
        ),
        number=1,
    )
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
        json_schema_extra={
            "rift:enumDescriptions": {
                "adapter": "The language's own analysis.",
                "validator": "A check Rift ran over a proposed change.",
                "apply": "Reported while applying edits to the workspace.",
            }
        },
    )
    at: Field[core.ResolvedSnapshot] = proto_field(
        description="The state this answer was resolved against.", number=2
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
            EnumValue("snapshot_not_found", "ERROR_CODE_SNAPSHOT_NOT_FOUND", 4),
            EnumValue(
                "semantic_snapshot_mismatch", "ERROR_CODE_SEMANTIC_SNAPSHOT_MISMATCH", 5
            ),
            EnumValue(
                "semantic_snapshot_unavailable",
                "ERROR_CODE_SEMANTIC_SNAPSHOT_UNAVAILABLE",
                6,
            ),
            EnumValue("resource_not_found", "ERROR_CODE_RESOURCE_NOT_FOUND", 7),
            EnumValue("content_unavailable", "ERROR_CODE_CONTENT_UNAVAILABLE", 8),
            EnumValue("cursor_invalid", "ERROR_CODE_CURSOR_INVALID", 9),
            EnumValue(
                "cursor_snapshot_mismatch", "ERROR_CODE_CURSOR_SNAPSHOT_MISMATCH", 10
            ),
            EnumValue("cancelled", "ERROR_CODE_CANCELLED", 11),
            EnumValue("deadline_exceeded", "ERROR_CODE_DEADLINE_EXCEEDED", 12),
            EnumValue("limit_exceeded", "ERROR_CODE_LIMIT_EXCEEDED", 13),
            EnumValue("projection_busy", "ERROR_CODE_PROJECTION_BUSY", 14),
            EnumValue("adapter_unavailable", "ERROR_CODE_ADAPTER_UNAVAILABLE", 15),
            EnumValue(
                "adapter_protocol_error", "ERROR_CODE_ADAPTER_PROTOCOL_ERROR", 16
            ),
            EnumValue("adapter_timeout", "ERROR_CODE_ADAPTER_TIMEOUT", 17),
            EnumValue("storage_failure", "ERROR_CODE_STORAGE_FAILURE", 18),
            EnumValue(
                "validator_execution_failure",
                "ERROR_CODE_VALIDATOR_EXECUTION_FAILURE",
                19,
            ),
            EnumValue("internal_error", "ERROR_CODE_INTERNAL_ERROR", 20),
            EnumValue("unsupported_path", "ERROR_CODE_UNSUPPORTED_PATH", 21),
            EnumValue("cursor_expired", "ERROR_CODE_CURSOR_EXPIRED", 22),
            EnumValue("state_corrupt", "ERROR_CODE_STATE_CORRUPT", 23),
            EnumValue(
                "temporarily_unavailable",
                "ERROR_CODE_TEMPORARILY_UNAVAILABLE",
                24,
            ),
            EnumValue(
                "configuration_invalid",
                "ERROR_CODE_CONFIGURATION_INVALID",
                25,
            ),
            EnumValue("projection_head_moved", "ERROR_CODE_PROJECTION_HEAD_MOVED", 26),
            EnumValue(
                "capability_unavailable",
                "ERROR_CODE_CAPABILITY_UNAVAILABLE",
                27,
            ),
            EnumValue(
                "git_revision_not_found",
                "ERROR_CODE_GIT_REVISION_NOT_FOUND",
                28,
            ),
            EnumValue("projection_not_found", "ERROR_CODE_PROJECTION_NOT_FOUND", 29),
            EnumValue(
                "projection_unavailable",
                "ERROR_CODE_PROJECTION_UNAVAILABLE",
                30,
            ),
            EnumValue("recovery_moved", "ERROR_CODE_RECOVERY_MOVED", 31),
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
            "snapshot_not_found": (
                "The exact Rift snapshot is no longer retained. Current heads and live resources "
                "pin their snapshots; a copied identifier alone does not. Re-read or re-import it."
            ),
            "semantic_snapshot_mismatch": (
                "The exact source snapshot still exists, but the adapter state supplied with a "
                "semantic identity belongs to another snapshot. Re-read semantic facts against "
                "the selected snapshot."
            ),
            "semantic_snapshot_unavailable": (
                "The revision resolves, but Rift has no index for it yet: the adapters are still "
                "analysing, or that state's facts were dropped. Retrying once indexing catches up "
                "succeeds."
            ),
            "resource_not_found": (
                "The identity is well-formed and resolves to nothing — no such symbol, no such "
                "file at that revision, or a debug session already stopped or expired. Retrying "
                "does not help."
            ),
            "content_unavailable": (
                "The entry is known but its bytes cannot be produced: an LFS object Rift does not "
                "fetch, or a blob the object store cannot read. Retrying does not help."
            ),
            "cursor_invalid": (
                "The cursor is malformed, or it was minted for a different request, order or page "
                "size. Start the query again from its first page."
            ),
            "cursor_snapshot_mismatch": (
                "The cursor is well-formed but the state it was minted against has moved, so "
                "continuing it would splice two different answers together. Start again against "
                "the current state."
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
                "The required projection has an open writable handle, in-flight mutation, or "
                "operation barrier that prevents removal or recovery. The error identifies the "
                "projection. Retry after the owner completes or closes it."
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
                "Rift could not read or write the persistent store, mounted projection, Git "
                "objects or refs, or the repository-local control directory. Worth retrying only "
                "if the cause was transient, such as a disk that has since been cleared."
            ),
            "validator_execution_failure": (
                "Rift could not prepare the validation workspace, launch the command validator, "
                "enforce its timeout, or capture its output. One retry is reasonable when the "
                "host failure was transient."
            ),
            "internal_error": (
                "A bug in Rift. `causes` says what it was doing at the time, and a retry is not "
                "expected to answer differently."
            ),
            "unsupported_path": (
                "Git contains a path the protocol or host filesystem cannot represent safely. "
                "The same snapshot remains unusable until that path changes."
            ),
            "cursor_expired": (
                "The cursor is valid, but its captured result page set left the process-local "
                "cache. Start again from the first page."
            ),
            "state_corrupt": (
                "A projection head names a missing tree or blob, a digest is wrong, or a store "
                "invariant failed. Rift stops mutations until a local state check repairs or "
                "exports the readable state."
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
            "projection_head_moved": (
                "A cleanup or recovery request names a projection token that is no longer "
                "current. Read the projection again before retrying."
            ),
            "capability_unavailable": (
                "The tool exists, but the workspace configuration and configured adapters cannot "
                "serve this operation for the requested language. Read the repository resource "
                "again after `rift.toml` or adapter availability changes."
            ),
            "git_revision_not_found": (
                "A Git selector does not resolve locally to a commit, or a required object is "
                "missing from a shallow or partial repository. Rift never fetches implicitly."
            ),
            "projection_not_found": "The named filesystem projection no longer exists.",
            "projection_unavailable": (
                "The projection exists, but its filesystem frontend or mount is not serving. "
                "Retry after the server reconnects or remounts it."
            ),
            "recovery_moved": (
                "A recovery cleanup request names an index/worktree manifest that is no longer "
                "current. List recoveries again and review the changed bytes before retrying."
            ),
        }
    },
)
class ErrorCode(str, Enum):
    "Why a request failed, as a stable code a caller branches on. The code is the complete classification. Domain results such as unsupported coverage and edit refusal use their typed result values."

    INVALID_REQUEST = "invalid_request"
    UNSUPPORTED_PROTOCOL = "unsupported_protocol"
    PERMISSION_DENIED = "permission_denied"
    SNAPSHOT_NOT_FOUND = "snapshot_not_found"
    SEMANTIC_SNAPSHOT_MISMATCH = "semantic_snapshot_mismatch"
    SEMANTIC_SNAPSHOT_UNAVAILABLE = "semantic_snapshot_unavailable"
    RESOURCE_NOT_FOUND = "resource_not_found"
    CONTENT_UNAVAILABLE = "content_unavailable"
    CURSOR_INVALID = "cursor_invalid"
    CURSOR_SNAPSHOT_MISMATCH = "cursor_snapshot_mismatch"
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
    STATE_CORRUPT = "state_corrupt"
    TEMPORARILY_UNAVAILABLE = "temporarily_unavailable"
    CONFIGURATION_INVALID = "configuration_invalid"
    PROJECTION_HEAD_MOVED = "projection_head_moved"
    CAPABILITY_UNAVAILABLE = "capability_unavailable"
    GIT_REVISION_NOT_FOUND = "git_revision_not_found"
    PROJECTION_NOT_FOUND = "projection_not_found"
    PROJECTION_UNAVAILABLE = "projection_unavailable"
    RECOVERY_MOVED = "recovery_moved"


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "RetryDirective",
        (
            EnumValue("never", "RETRY_DIRECTIVE_NEVER", 1),
            EnumValue("same_request", "RETRY_DIRECTIVE_SAME_REQUEST", 2),
            EnumValue("refresh_snapshot", "RETRY_DIRECTIVE_REFRESH_SNAPSHOT", 3),
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
            "refresh_snapshot": (
                "The state moved under the request. Re-read the current state, rebuild whatever "
                "was pinned to the old one, then ask again."
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
    REFRESH_SNAPSHOT = "refresh_snapshot"
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
            EnumValue("integrate", "INTEGRATE", 6),
            EnumValue("execute", "EXECUTE", 7),
            EnumValue("debug", "DEBUG", 8),
        ),
        placement=Placement("phase", 4),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "discovery": "Working out what the workspace can do: capabilities, limits, which languages have adapters.",
            "read": "Fetching what was asked for, from the index, the object store or an adapter.",
            "resolve": "Turning an address, a cursor or an action key into the concrete thing it names at a state.",
            "validate": (
                "Checking a proposed change against the schema, the state it was pinned to, and "
                "the checks required by the workspace-root `rift.toml`."
            ),
            "change": "Checking the projection token, resolving in a private candidate, validating, and conditionally advancing the projection.",
            "integrate": "Merging the session head, validating the merged tree, and conditionally advancing a target branch.",
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
    INTEGRATE = "integrate"
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
                {
                    "if": {
                        "required": ["code"],
                        "properties": {"code": {"const": "projection_busy"}},
                    },
                    "then": {"required": ["projection"]},
                    "else": {"not": {"required": ["projection"]}},
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
        json_schema_extra={
            "rift:enumDescriptions": {
                "discovery": (
                    "Working out what the workspace can do: capabilities, limits, which languages "
                    "have adapters."
                ),
                "read": "Fetching what was asked for, from the index, the object store or an adapter.",
                "resolve": "Turning an address, a cursor or an action key into the concrete thing it names at a state.",
                "validate": (
                    "Checking a proposed change against the schema, the state it was pinned to, and "
                    "the checks required by the workspace-root `rift.toml`."
                ),
                "change": "Checking the projection token, resolving in a private candidate, validating, and conditionally advancing the projection.",
                "integrate": "Merging the session head, validating the merged tree, and conditionally advancing a target branch.",
                "execute": "Preparing an execution workspace and evaluating caller-provided code.",
                "debug": "Starting, inspecting, or stopping a connection-bound debugging session.",
            }
        },
    )
    at: Field[core.Snapshot | None] = proto_field(
        description=(
            "The state this answer was resolved against. Null where the failure happened "
            "before one was resolved."
        ),
        number=5,
    )
    operation: Field[int | None] = proto_field(
        description=(
            "Which operation of a multi-operation request failed, as its zero-based index. "
            "Null where the request carried one, or failed before any of them ran."
        ),
        number=6,
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
    projection: Field[ProjectionLocation | None] = proto_field(
        default=None,
        description=(
            "Projection with open or in-flight filesystem mutations. Present exactly when `code` is `projection_busy` "
            "and forbidden otherwise."
        ),
        number=10,
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
            "driver": "A field of `Limits`, advertised by the repository resource.",
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
        json_schema_extra={
            "rift:enumDescriptions": {
                "driver": "A field of `Limits`, advertised by the repository resource.",
                "adapter": "A field of the adapter's `AdapterLimits`, advertised in `Describe`.",
            }
        },
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
    """The grammar a path selector is read in: Git globs, so `src/**/*.ts` means here what it means in `.gitignore`."""

    name: Field[Literal["git-glob"]] = proto_field(number=1)
    version: Field[Literal[1]] = proto_field(number=2)


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class MatchSyntax(ClosedModel):
    "The two pattern grammars this workspace accepts: one for text patterns and one for path globs. Names remain stable; their version fields select syntax and matching semantics."

    text: Field[MatchSyntaxText] = proto_field(
        description="The grammar a `TextQuery` pattern is read in.", number=1
    )
    path: Field[MatchSyntaxPath] = proto_field(
        description=(
            "The grammar a path selector is read in: Git globs, so `src/**/*.ts` means here "
            "what it means in `.gitignore`."
        ),
        number=2,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RepositoryResourceTemplate(ClosedModel):
    """The repository resource. It takes no path, only an optional revision and cursor. An
    omitted revision selects the current session state."""

    uriTemplate: Field[Literal["rift://repository{?rev,cursor}"]] = proto_field(
        description="The template, in RFC 6570 form. What follows `?` is optional."
    )
    name: Field[Literal["repository"]] = proto_field(
        description="The resource family, as `resources/templates/list` advertises it.",
        number=1,
    )
    mimeType: Field[Literal["application/vnd.rift.repository+json"]] = proto_field(
        description="What a read of a URI from this template returns: `RepositoryResourcePayload` as JSON.",
        number=2,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class SymbolResourceTemplate(ClosedModel):
    """The symbol resource, addressed by language and the name that language gives the
    declaration. An omitted revision selects the current session state."""

    uriTemplate: Field[Literal["rift://symbol/{language}/{name}{?rev,cursor}"]] = (
        proto_field(
            description="The template, in RFC 6570 form. What follows `?` is optional."
        )
    )
    name: Field[Literal["symbol"]] = proto_field(
        description="The resource family, as `resources/templates/list` advertises it.",
        number=1,
    )
    mimeType: Field[Literal["application/vnd.rift.symbol+json"]] = proto_field(
        description="What a read of a URI from this template returns: `SymbolResourcePayload` as JSON.",
        number=2,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class DiffResourceTemplate(ClosedModel):
    """The diff resource, addressed by two independently namespaced revisions."""

    uriTemplate: Field[Literal["rift://diff{?from,to,cursor}"]] = proto_field(
        description="The template, in RFC 6570 form. `from` and `to` are required when read."
    )
    name: Field[Literal["diff"]] = proto_field(
        description="The resource family, as `resources/templates/list` advertises it.",
        number=1,
    )
    mimeType: Field[Literal["application/vnd.rift.diff+json"]] = proto_field(
        description="What a read of a URI from this template returns: `DiffResourcePayload` as JSON.",
        number=2,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FileResourceTemplate(ClosedModel):
    """The file resource, addressed by a path relative to the project root. An omitted revision
    selects the current session state."""

    uriTemplate: Field[Literal["rift://file/{path}{?rev,start,length}"]] = proto_field(
        description="The template, in RFC 6570 form. What follows `?` is optional."
    )
    name: Field[Literal["file"]] = proto_field(
        description="The resource family, as `resources/templates/list` advertises it.",
        number=1,
    )
    mimeType: Field[Literal["application/vnd.rift.file+json"]] = proto_field(
        description="What a read of a URI from this template returns: `FileResourcePayload` as JSON.",
        number=2,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FsResourceTemplate(ClosedModel):
    """The live filesystem-projection inventory, paged by an optional cursor."""

    uriTemplate: Field[Literal["rift://fs{?cursor}"]] = proto_field(
        description="The template in RFC 6570 form."
    )
    name: Field[Literal["fs"]] = proto_field(
        description="The resource family advertised by `resources/templates/list`.",
        number=1,
    )
    mimeType: Field[Literal["application/vnd.rift.fs+json"]] = proto_field(
        description="A read returns `FsResourcePayload` as JSON.",
        number=2,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ActionsResourceTemplate(ClosedModel):
    """The actions resource, addressed by the place to ask about. An omitted revision selects
    the current session state."""

    uriTemplate: Field[Literal["rift://actions/{address}{?only,rev,cursor}"]] = (
        proto_field(
            description=(
                "The template, in RFC 6570 form. The address is `symbol/{language}/{name}`, "
                "`node/{language}/{path}@{start}-{end}`, `match/{token}`, or `file/{path}`."
            )
        )
    )
    name: Field[Literal["actions"]] = proto_field(
        description="The resource family, as `resources/templates/list` advertises it.",
        number=1,
    )
    mimeType: Field[Literal["application/vnd.rift.actions+json"]] = proto_field(
        description="What a read of a URI from this template returns: `ActionsResourcePayload` as JSON.",
        number=2,
        proto_name="mime_type",
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ActionResourceTemplate(ClosedModel):
    """One discovered action, addressed by the offer identity a listing returned."""

    uriTemplate: Field[Literal["rift://action/{token}"]] = proto_field(
        description="The template, in RFC 6570 form."
    )
    name: Field[Literal["action"]] = proto_field(
        description="The resource family, as `resources/templates/list` advertises it.",
        number=1,
    )
    mimeType: Field[Literal["application/vnd.rift.action+json"]] = proto_field(
        description="What a read of a URI from this template returns: `ActionResourcePayload` as JSON.",
        number=2,
        proto_name="mime_type",
    )


@union(
    owner=MCP,
    oneof="variant",
    discriminator="mimeType",
    variants=(
        Variant(
            "rift://repository{?rev,cursor}",
            "repository",
            1,
            RepositoryResourceTemplate,
        ),
        Variant(
            "rift://symbol/{language}/{name}{?rev,cursor}",
            "symbol",
            2,
            SymbolResourceTemplate,
        ),
        Variant("rift://diff/{from}..{to}{?cursor}", "diff", 3, DiffResourceTemplate),
        Variant(
            "rift://file/{path}{?rev,start,length}", "file", 4, FileResourceTemplate
        ),
        Variant(
            "rift://actions/{address}{?only,rev,cursor}",
            "actions",
            5,
            ActionsResourceTemplate,
        ),
        Variant("rift://action/{token}", "action", 6, ActionResourceTemplate),
        Variant("rift://fs{?cursor}", "fs", 7, FsResourceTemplate),
    ),
)
class ResourceTemplate(ProtocolRoot):
    """One advertised MCP resource template. uriTemplate, name, and mimeType are correlated per family."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ResourceReadParams(ClosedModel):
    """The URI passed to MCP `resources/read`. Each branch is one advertised Rift resource family."""

    uri: Field[
        RepositoryResourceUri
        | core.SymbolId
        | core.DiffId
        | FileResourceUri
        | ActionsResourceUri
        | ActionResourceUri
        | FsResourceUri
    ] = proto_field(
        description="A URI matching one branch of `ResourceTemplate`.",
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RepositoryResourceContent(ClosedModel):
    """What a read of `rift://repository` returns."""

    uri: Field[RepositoryResourceUri] = proto_field(
        description="The URI that was read, as it resolved.", number=1
    )
    mimeType: Field[Literal["application/vnd.rift.repository+json"]] = proto_field(
        description="Which payload `text` holds."
    )
    text: Field[str] = proto_field(
        description="A `RepositoryResourcePayload`, serialized as JSON.",
        number=2,
        json_schema_extra={
            "contentMediaType": "application/vnd.rift.repository+json",
            "rift:contentType": "RepositoryResourcePayload",
        },
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class SymbolResourceContent(ClosedModel):
    """What a read of a `rift://symbol/…` URI returns."""

    uri: Field[core.SymbolId] = proto_field(
        description="The URI that was read, as it resolved.", number=1
    )
    mimeType: Field[Literal["application/vnd.rift.symbol+json"]] = proto_field(
        description="Which payload `text` holds."
    )
    text: Field[str] = proto_field(
        description="A `SymbolResourcePayload`, serialized as JSON.",
        number=2,
        json_schema_extra={
            "contentMediaType": "application/vnd.rift.symbol+json",
            "rift:contentType": "SymbolResourcePayload",
        },
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class DiffResourceContent(ClosedModel):
    """What a read of a `rift://diff/…` URI returns."""

    uri: Field[core.DiffId] = proto_field(
        description="The URI that was read, as it resolved.", number=1
    )
    mimeType: Field[Literal["application/vnd.rift.diff+json"]] = proto_field(
        description="Which payload `text` holds."
    )
    text: Field[str] = proto_field(
        description="A `DiffResourcePayload`, serialized as JSON.",
        number=2,
        json_schema_extra={
            "contentMediaType": "application/vnd.rift.diff+json",
            "rift:contentType": "DiffResourcePayload",
        },
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FileResourceContent(ClosedModel):
    """What a read of a `rift://file/…` URI returns."""

    uri: Field[FileResourceUri] = proto_field(
        description="The URI that was read, as it resolved.", number=1
    )
    mimeType: Field[Literal["application/vnd.rift.file+json"]] = proto_field(
        description="Which payload `text` holds."
    )
    text: Field[str] = proto_field(
        description="A `FileResourcePayload`, serialized as JSON.",
        number=2,
        json_schema_extra={
            "contentMediaType": "application/vnd.rift.file+json",
            "rift:contentType": "FileResourcePayload",
        },
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FsResourceContent(ClosedModel):
    """What a read of `rift://fs` returns."""

    uri: Field[FsResourceUri] = proto_field(
        description="Inventory page that was read.", number=1
    )
    mimeType: Field[Literal["application/vnd.rift.fs+json"]] = proto_field(
        description="Which payload `text` holds."
    )
    text: Field[str] = proto_field(
        description="An `FsResourcePayload`, serialized as JSON.",
        number=2,
        json_schema_extra={
            "contentMediaType": "application/vnd.rift.fs+json",
            "rift:contentType": "FsResourcePayload",
        },
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ActionsResourceContent(ClosedModel):
    """What a read of a `rift://actions/…` URI returns."""

    uri: Field[ActionsResourceUri] = proto_field(
        description="The address that was read, as it resolved.", number=1
    )
    mimeType: Field[Literal["application/vnd.rift.actions+json"]] = proto_field(
        description="Which payload `text` holds."
    )
    text: Field[str] = proto_field(
        description="An `ActionsResourcePayload`, serialized as JSON.",
        number=2,
        json_schema_extra={
            "contentMediaType": "application/vnd.rift.actions+json",
            "rift:contentType": "ActionsResourcePayload",
        },
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ActionResourceContent(ClosedModel):
    """What a read of a `rift://action/…` URI returns."""

    uri: Field[ActionResourceUri] = proto_field(
        description="The offer that was read, as it resolved.", number=1
    )
    mimeType: Field[Literal["application/vnd.rift.action+json"]] = proto_field(
        description="Which payload `text` holds."
    )
    text: Field[str] = proto_field(
        description="An `ActionResourcePayload`, serialized as JSON.",
        number=2,
        json_schema_extra={
            "contentMediaType": "application/vnd.rift.action+json",
            "rift:contentType": "ActionResourcePayload",
        },
    )


@union(
    owner=MCP,
    oneof="variant",
    discriminator="mimeType",
    variants=(
        Variant(
            "application/vnd.rift.repository+json",
            "repository",
            1,
            RepositoryResourceContent,
        ),
        Variant("application/vnd.rift.symbol+json", "symbol", 2, SymbolResourceContent),
        Variant("application/vnd.rift.diff+json", "diff", 3, DiffResourceContent),
        Variant("application/vnd.rift.file+json", "file", 4, FileResourceContent),
        Variant(
            "application/vnd.rift.actions+json", "actions", 5, ActionsResourceContent
        ),
        Variant("application/vnd.rift.action+json", "action", 6, ActionResourceContent),
        Variant("application/vnd.rift.fs+json", "fs", 7, FsResourceContent),
    ),
    public=False,
)
class ResourceContent(ProtocolRoot):
    pass


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

    expected: Field[core.ProjectionState] = proto_field(
        description=(
            "Exact projection state this operation must replace. Rift refuses a stale token before "
            "resolving edits, invoking an adapter, or publishing source state."
        ),
        number=1,
    )
    confirmations: Field[list[int]] = proto_field(
        default_factory=list,
        description=(
            "Confirmation ids returned by an earlier refusal for this same expected projection state and "
            "operation. Missing or extra ids refuse the retry."
        ),
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
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

    expected: Field[core.ProjectionState] = proto_field(
        description=(
            "Exact projection state this operation must replace. Rift refuses a stale token before "
            "resolving edits, invoking an adapter, or publishing source state."
        ),
        number=1,
    )
    confirmations: Field[list[int]] = proto_field(
        default_factory=list,
        description=(
            "Confirmation ids returned by an earlier refusal for this same expected projection state and "
            "operation. Missing or extra ids refuse the retry."
        ),
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after this operation's edits resolve.", number=3
    )
    patch: Field[str] = proto_field(
        description="Unified diff in Git's text patch syntax, with project-relative `a/` and `b/` paths.",
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

    expected: Field[core.ProjectionState] = proto_field(
        description=(
            "Exact projection state this operation must replace. Rift refuses a stale token before "
            "resolving edits, invoking an adapter, or publishing source state."
        ),
        number=1,
    )
    confirmations: Field[list[int]] = proto_field(
        default_factory=list,
        description=(
            "Confirmation ids returned by an earlier refusal for this same expected projection state and "
            "operation. Missing or extra ids refuse the retry."
        ),
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
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
class RevertParams(ClosedModel):
    "A validated three-way inverse of one commit. Rift computes the difference from `parent` to `revision`, applies its inverse, and refuses overlapping changes it cannot merge without guessing."

    expected: Field[core.ProjectionState] = proto_field(
        description=(
            "Exact projection state this operation must replace. Rift refuses a stale token before "
            "resolving edits, invoking an adapter, or publishing source state."
        ),
        number=1,
    )
    confirmations: Field[list[int]] = proto_field(
        default_factory=list,
        description=(
            "Confirmation ids returned by an earlier refusal for this same expected projection state and "
            "operation. Missing or extra ids refuse the retry."
        ),
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after this operation's edits resolve.", number=3
    )
    revision: Field[core.GitCommit] = proto_field(
        description="Exact commit whose changes are inverted.", number=4
    )
    parent: Field[core.GitCommit | None] = proto_field(
        default=None,
        description=(
            "Parent against which the commit's change is defined. Required for ordinary and "
            "merge commits; null selects the empty tree for a root commit. A commit that does "
            "not have this parent is refused."
        ),
        number=5,
    )
    paths: Field[core.PathSelector] = proto_field(
        description=(
            "Paths from the original commit eligible for inversion. Excluded paths remain "
            "untouched; the commit diff exposes them when the caller needs to inspect the "
            "omission."
        ),
        number=6,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.message(),
    schema_extra={},
)
class RenameParams(ClosedModel):
    "Changes what a declaration is called and rewrites the references that name it. The adapter checks language spelling, collisions, and binding changes; a reference outside `scope` refuses the operation rather than leaving it half done."

    expected: Field[core.ProjectionState] = proto_field(
        description=(
            "Exact projection state this operation must replace. Rift refuses a stale token before "
            "resolving edits, invoking an adapter, or publishing source state."
        ),
        number=1,
    )
    confirmations: Field[list[int]] = proto_field(
        default_factory=list,
        description=(
            "Confirmation ids returned by an earlier refusal for this same expected projection state and "
            "operation. Missing or extra ids refuse the retry."
        ),
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
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

    expected: Field[core.ProjectionState] = proto_field(
        description=(
            "Exact projection state this operation must replace. Rift refuses a stale token before "
            "resolving edits, invoking an adapter, or publishing source state."
        ),
        number=1,
    )
    confirmations: Field[list[int]] = proto_field(
        default_factory=list,
        description=(
            "Confirmation ids returned by an earlier refusal for this same expected projection state and "
            "operation. Missing or extra ids refuse the retry."
        ),
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
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

    expected: Field[core.ProjectionState] = proto_field(
        description=(
            "Exact projection state this operation must replace. Rift refuses a stale token before "
            "resolving edits, invoking an adapter, or publishing source state."
        ),
        number=1,
    )
    confirmations: Field[list[int]] = proto_field(
        default_factory=list,
        description=(
            "Confirmation ids returned by an earlier refusal for this same expected projection state and "
            "operation. Missing or extra ids refuse the retry."
        ),
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
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

    expected: Field[core.ProjectionState] = proto_field(
        description=(
            "Exact projection state this operation must replace. Rift refuses a stale token before "
            "resolving edits, invoking an adapter, or publishing source state."
        ),
        number=1,
    )
    confirmations: Field[list[int]] = proto_field(
        default_factory=list,
        description=(
            "Confirmation ids returned by an earlier refusal for this same expected projection state and "
            "operation. Missing or extra ids refuse the retry."
        ),
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
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
    "Resolves one discovered adapter action — a quick fix, an extraction, an inline, anything an adapter offers that has no portable contract. Rift validates `arguments` against the offer's advertised schema. An offer whose kind belongs to a portable family is refused here, because `rename`, `move`, `delete`, and `change_signature` are its typed entry points."

    expected: Field[core.ProjectionState] = proto_field(
        description=(
            "Exact projection state this operation must replace. Rift refuses a stale token before "
            "resolving edits, invoking an adapter, or publishing source state."
        ),
        number=1,
    )
    confirmations: Field[list[int]] = proto_field(
        default_factory=list,
        description=(
            "Confirmation ids returned by an earlier refusal for this same expected projection state and "
            "operation. Missing or extra ids refuse the retry."
        ),
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
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
    """Discards all source changes since the current session base and restores its pinned base
    snapshot. Scratch remains intact."""

    expected: Field[core.ProjectionState] = proto_field(
        description="Exact dirty state the caller reviewed and intends to discard.",
        number=1,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ResolvedOperation(ClosedModel):
    "Bounded summary of one operation after it resolves against the current session projection."

    owners: Field[list[core.Language]] = proto_field(
        description=(
            "Language adapters that contributed to resolution, sorted by name and dialect "
            "with null first. Empty for an operation resolved entirely by Rift."
        ),
        number=1,
        json_schema_extra={"uniqueItems": True},
    )
    edit_count: Field[int] = proto_field(
        description="Number of concrete Edit records produced by this operation.",
        ge=0,
        le=4294967295,
        number=2,
    )
    precondition_count: Field[int] = proto_field(
        description="Number of satisfied preconditions checked for this operation.",
        ge=0,
        le=4294967295,
        number=3,
    )
    effect_count: Field[int] = proto_field(
        description="Number of semantic effects reported for this operation.",
        ge=0,
        le=4294967295,
        number=4,
    )
    guarantee_count: Field[int] = proto_field(
        description="Number of guarantee evidence records produced for this operation.",
        ge=0,
        le=4294967295,
        number=5,
    )
    coverage: Field[core.Coverage] = proto_field(
        description="How completely Rift and its adapters resolved the request.",
        number=6,
    )
    diagnostic_count: Field[int] = proto_field(
        description="Number of resolution diagnostics produced for this operation.",
        ge=0,
        le=4294967295,
        number=7,
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
    """How repository configuration presents this check."""

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
    """Whether Rift appends the merged tree's changed `ProjectPath` values to `argv` in byte order."""

    NONE = "none"
    APPEND = "append"


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class CommandValidatorGuarantees(ClosedModel):
    "What a passing run of one configured command establishes over the merged tree."

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
    "An integration check from the workspace-root `rift.toml`, executed directly without a shell in a disposable validation workspace materialized from the complete merged tree. The workspace is the process working directory and does not isolate the command from the host."

    id: Field[str] = proto_field(
        description=(
            "Repository label shown with this validator. It is unique within `validators.commands`."
        ),
        pattern="^[A-Za-z][A-Za-z0-9_.-]{0,127}$",
        number=1,
    )
    kind: Field[CommandValidatorKind] = proto_field(
        description="How repository configuration presents this check.", number=2
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
        description="Whether Rift appends the merged tree's changed `ProjectPath` values to `argv` in byte order.",
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
            "inside one bounded integration result."
        ),
        ge=256,
        le=4096,
        number=8,
    )
    guarantees: Field[list[CommandValidatorGuarantees]] = proto_field(
        description=(
            "Behavior or other properties this command is intended to check. A passing result "
            "turns each declaration into `GuaranteeEvidence`; a failed result rejects integration."
        ),
        number=9,
    )
    determinism: Field[CommandValidatorDeterminism] = proto_field(
        description="Whether an identical tree and environment are expected to produce the same result.",
        number=10,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ValidatorResultPassed(ClosedModel):
    """A declared validator process that exited with status zero."""

    status: Field[Literal["passed"]] = proto_field(default="passed")
    exit_code: Field[Literal[0]] = proto_field(default=0, number=1)
    declaration_digest: Field[core.Digest] = proto_field(
        description=(
            "SHA-256 of the validator's RFC 8785 canonical JSON declaration. Results and "
            "declarations form a bijection on this value, because labels may repeat while "
            "commands differ."
        ),
        number=2,
    )
    files: Field[list[core.ProjectPath]] = proto_field(
        description="Paths evaluated by the validator, sorted by UTF-8 bytes and without duplicates.",
        number=3,
        json_schema_extra={"uniqueItems": True},
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        description=(
            "Structured findings produced by the validator. The list is empty when its output "
            "has no configured diagnostic decoder."
        ),
        number=4,
    )
    stdout: Field[core.CapturedText] = proto_field(
        description="Bounded standard output from the process.", number=5
    )
    stderr: Field[core.CapturedText] = proto_field(
        description="Bounded standard error from the process.", number=6
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ValidatorResultFailed(ClosedModel):
    """A declared validator process that exited with a nonzero status."""

    status: Field[Literal["failed"]] = proto_field(default="failed")
    exit_code: Field[int] = proto_field(
        number=1, json_schema_extra={"not": {"const": 0}}
    )
    declaration_digest: Field[core.Digest] = proto_field(
        description=(
            "SHA-256 of the validator's RFC 8785 canonical JSON declaration. Results and "
            "declarations form a bijection on this value, because labels may repeat while "
            "commands differ."
        ),
        number=2,
    )
    files: Field[list[core.ProjectPath]] = proto_field(
        description="Paths evaluated by the validator, sorted by UTF-8 bytes and without duplicates.",
        number=3,
        json_schema_extra={"uniqueItems": True},
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        description=(
            "Structured findings produced by the validator. The list is empty when its output "
            "has no configured diagnostic decoder."
        ),
        number=4,
    )
    stdout: Field[core.CapturedText] = proto_field(
        description="Bounded standard output from the process.", number=5
    )
    stderr: Field[core.CapturedText] = proto_field(
        description="Bounded standard error from the process.", number=6
    )

    @field_validator("exit_code")
    @classmethod
    def nonzero_exit_code(cls, value: int) -> int:
        if value == 0:
            raise ValueError("failed validator result requires a nonzero exit code")
        return value


@union(
    owner=MCP,
    oneof="variant",
    discriminator="status",
    variants=(
        Variant("passed", "passed", 1, ValidatorResultPassed),
        Variant("failed", "failed", 2, ValidatorResultFailed),
    ),
)
class ValidatorResult(ProtocolRoot):
    "The completed outcome of one configured validator. Exit status zero passes. Every other exit status fails. A workspace, launch, timeout, or capture failure raises `validator_execution_failure` before Rift produces integration evidence."


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

    valid: Field[bool] = proto_field(
        description="Whether every completed adapter report accepted the resulting tree.",
        number=1,
    )
    complete: Field[bool] = proto_field(
        description="Whether every adapter required by `validation.require` returned complete coverage.",
        number=2,
    )
    strength: Field[ValidationStrength] = proto_field(
        description="Semantic validation strength established for the resulting tree.",
        number=3,
    )
    adapter_reports: Field[list[core.ValidationReport]] = proto_field(
        description="Adapter reports in language order.", number=4
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ChangeSummary(ClosedModel):
    """Projection transition, immutable diff, resolution evidence, and validation for one
    applied store change."""

    previous: Field[core.ProjectionState] = proto_field(
        description="Exact projection state against which this operation resolved.",
        number=1,
    )
    diff: Field[DiffResourceLink] = proto_field(
        description=(
            "Store-tree diff using immutable `snapshot:` selectors for `previous.snapshot` "
            "and `current.snapshot`."
        ),
        number=2,
    )
    resolved: Field[ResolvedOperation] = proto_field(
        description="Bounded evidence from resolving the requested operation.", number=3
    )
    validation: Field[ChangeValidation] = proto_field(
        description="Adapter validation of the resulting snapshot.", number=4
    )
    edits: Field[list[core.Edit]] = proto_field(
        description="Concrete edits in canonical file-and-range order.", number=5
    )
    preconditions: Field[list[core.OperationPrecondition]] = proto_field(
        description="Satisfied preconditions in check order.", number=6
    )
    effects: Field[list[core.OperationEffect]] = proto_field(
        description="Semantic effects in adapter emission order.", number=7
    )
    guarantees: Field[list[core.GuaranteeEvidence]] = proto_field(
        description="Scoped guarantee evidence in guarantee-kind order.", number=8
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        description="Resolution findings in source order.", number=9
    )
    current: Field[core.ProjectionState] = proto_field(
        description="Published current projection state with a fresh head token.",
        number=10,
    )
    degradation: Field[list[core.Diagnostic]] = proto_field(
        description=(
            "Post-publication invalidation or notification failures. Non-empty means source "
            "was durably applied but the projection is unavailable until remount."
        ),
        number=11,
    )

    @model_validator(mode="after")
    def projection_transition_is_correlated(self) -> ChangeSummary:
        if not self.validation.valid:
            raise ValueError("an applied change requires valid adapter evidence")
        if self.previous.projection != self.current.projection:
            raise ValueError("change states must belong to the same projection")
        if self.previous.base != self.current.base:
            raise ValueError("ordinary changes cannot replace the projection base")
        if self.previous.head == self.current.head:
            raise ValueError("applied changes require a fresh projection head")
        if self.previous.snapshot == self.current.snapshot:
            raise ValueError("applied changes require a changed source snapshot")
        expected_diff = (
            f"rift://diff?from=snapshot:{self.previous.snapshot.root}"
            f"&to=snapshot:{self.current.snapshot.root}"
        )
        if self.diff.uri.root != expected_diff:
            raise ValueError("change diff must compare its two immutable snapshots")
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ChangeApplied(ClosedModel):
    """The operation atomically replaced the current projection state."""

    status: Field[Literal["applied"]] = proto_field(
        description="Identifies an applied store change.", default="applied"
    )
    summary: Field[ChangeSummary] = proto_field(
        description="The applied change and its evidence.", number=1
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class StaleProjectionResult(ClosedModel):
    """The current projection did not have the exact state supplied as `expected`."""

    status: Field[Literal["stale_projection"]] = proto_field(
        description="Identifies a projection compare-and-swap failure.",
        default="stale_projection",
    )
    expected: Field[core.ProjectionState] = proto_field(
        description="Exact projection state supplied by the caller.", number=1
    )
    current: Field[core.ProjectionState] = proto_field(
        description="Exact current projection state.", number=2
    )

    @model_validator(mode="after")
    def states_name_the_same_projection(self) -> StaleProjectionResult:
        if self.expected.projection != self.current.projection:
            raise ValueError("stale states must belong to the same projection")
        if self.expected.head == self.current.head:
            raise ValueError("stale result requires a different projection head")
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ProjectionUnchanged(ClosedModel):
    """The projection already matched its retained base, so restore changed nothing."""

    status: Field[Literal["noop"]] = proto_field(default="noop")
    state: Field[core.ProjectionState] = proto_field(
        description="Unchanged projection state.", number=1
    )

    @model_validator(mode="after")
    def state_is_clean(self) -> ProjectionUnchanged:
        if self.state.dirty or self.state.snapshot != self.state.base.snapshot:
            raise ValueError("unchanged restore result requires a clean base state")
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class ProjectionRestored(ClosedModel):
    """Unintegrated source changes were discarded in one projection transaction."""

    status: Field[Literal["restored"]] = proto_field(default="restored")
    previous: Field[core.ProjectionState] = proto_field(
        description="Dirty state discarded by this operation.", number=1
    )
    current: Field[core.ProjectionState] = proto_field(
        description="Clean current state at the same retained base with a fresh token.",
        number=2,
    )
    degradation: Field[list[core.Diagnostic]] = proto_field(
        description="Post-publication projection invalidation or notification failures.",
        number=3,
    )

    @model_validator(mode="after")
    def transition_is_correlated(self) -> ProjectionRestored:
        if self.previous.projection != self.current.projection:
            raise ValueError("restore states must belong to the same projection")
        if not self.previous.dirty:
            raise ValueError("restore must discard a dirty previous state")
        if self.previous.base != self.current.base or self.current.dirty:
            raise ValueError("restore must return a clean state at the same base")
        if self.current.snapshot != self.current.base.snapshot:
            raise ValueError("restored state must render its base snapshot")
        if self.previous.head == self.current.head:
            raise ValueError("restore must mint a fresh projection head")
        return self


@union(
    owner=MCP,
    oneof="variant",
    discriminator="status",
    variants=(
        Variant("restored", "restored", 1, ProjectionRestored),
        Variant("noop", "noop", 2, ProjectionUnchanged),
        Variant("stale_projection", "stale_projection", 3, StaleProjectionResult),
    ),
)
class ProjectionRestoreResult(ProtocolRoot):
    """A restored projection, clean no-op, or stale projection comparison."""


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
            EnumValue(
                "confirmation_required", "REFUSAL_REASON_CONFIRMATION_REQUIRED", 7
            ),
            EnumValue("unsafe_effect", "REFUSAL_REASON_UNSAFE_EFFECT", 8),
            EnumValue(
                "formatter_unsupported", "REFUSAL_REASON_FORMATTER_UNSUPPORTED", 9
            ),
            EnumValue(
                "validation_incomplete", "REFUSAL_REASON_VALIDATION_INCOMPLETE", 10
            ),
            EnumValue("portable_family", "REFUSAL_REASON_PORTABLE_FAMILY", 11),
            EnumValue("checked_out_target", "REFUSAL_REASON_CHECKED_OUT_TARGET", 12),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "unsupported": "No adapter implements this operation for the language it reaches, or repository state cannot satisfy the contract.",
            "unmet_precondition": "A condition checked before resolution failed. The failed entry is in `preconditions`.",
            "ambiguous_target": "The address resolves to several targets. Narrow it and ask again.",
            "stale_action": "The offer was discovered against a snapshot that has moved. Read the actions resource again.",
            "stale_match": "The match was found against a snapshot that has moved. Search again.",
            "cardinality_mismatch": "A rewrite matched fewer or more times than its cardinality accepts.",
            "confirmation_required": "The operation needs acknowledgements the caller did not supply.",
            "unsafe_effect": "The complete effect reaches outside what the caller can have meant — outside the project, or into generated source.",
            "formatter_unsupported": "The requested formatting policy has no formatter behind it for an affected language.",
            "validation_incomplete": "Required adapter or command validation did not complete.",
            "portable_family": "The offer belongs to a portable family, which resolves through `rename`, `move`, `delete`, or `change_signature` rather than through `act`.",
            "checked_out_target": "The integration target is checked out in a Git worktree. Rift never changes a checked-out ref, index, or working tree.",
        }
    },
)
class RefusalReason(str, Enum):
    "Why Rift declined a change or integration. A refusal is a completed decision with evidence, not a transport failure; `ErrorData` carries failures that never reached a decision."

    UNSUPPORTED = "unsupported"
    UNMET_PRECONDITION = "unmet_precondition"
    AMBIGUOUS_TARGET = "ambiguous_target"
    STALE_ACTION = "stale_action"
    STALE_MATCH = "stale_match"
    CARDINALITY_MISMATCH = "cardinality_mismatch"
    CONFIRMATION_REQUIRED = "confirmation_required"
    UNSAFE_EFFECT = "unsafe_effect"
    FORMATTER_UNSUPPORTED = "formatter_unsupported"
    VALIDATION_INCOMPLETE = "validation_incomplete"
    PORTABLE_FAMILY = "portable_family"
    CHECKED_OUT_TARGET = "checked_out_target"


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RefusedResult(ClosedModel):
    "Resolution stopped before Rift could commit a change or begin integration. No ref moved."

    status: Field[Literal["refused"]] = proto_field(
        description="Identifies a domain refusal.", default="refused"
    )
    base: Field[core.Snapshot] = proto_field(
        description="State against which the refused work was attempted.", number=1
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
    confirmations: Field[list[core.ConfirmationRequirement]] = proto_field(
        description=(
            "Acknowledgements required for a retry of the same operation and expected projection state. "
            "Empty unless `reason` is `confirmation_required`."
        ),
        number=6,
    )


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^refs/heads/(?!/)(?!.*//)(?!.*\.\.)(?!.*[~^:?*\\])[A-Za-z0-9._/-]+$",
    examples=["refs/heads/main", "refs/heads/release/1.x"],
)
class IntegrationTarget(ProtocolRoot):
    """A local branch ref that integration may compare-and-swap. The schema is a safe prefilter;
    the server also requires Git's authoritative ref-format validator to accept the complete name."""


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "GitConflictStatus",
        (
            EnumValue("both_modified", "GIT_CONFLICT_STATUS_BOTH_MODIFIED", 1),
            EnumValue("added_by_us", "GIT_CONFLICT_STATUS_ADDED_BY_US", 2),
            EnumValue("added_by_them", "GIT_CONFLICT_STATUS_ADDED_BY_THEM", 3),
            EnumValue("deleted_by_us", "GIT_CONFLICT_STATUS_DELETED_BY_US", 4),
            EnumValue("deleted_by_them", "GIT_CONFLICT_STATUS_DELETED_BY_THEM", 5),
            EnumValue("both_added", "GIT_CONFLICT_STATUS_BOTH_ADDED", 6),
            EnumValue("both_deleted", "GIT_CONFLICT_STATUS_BOTH_DELETED", 7),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "both_modified": "Both sides modified the path.",
            "added_by_us": "The current side added the path and the other side did not.",
            "added_by_them": "The other side added the path and the current side did not.",
            "deleted_by_us": "The current side deleted a path modified by the other side.",
            "deleted_by_them": "The other side deleted a path modified by the current side.",
            "both_added": "Both sides added different entries at the path.",
            "both_deleted": "Both sides deleted the path with incompatible index state.",
        }
    },
)
class GitConflictStatus(str, Enum):
    """Git's unmerged-index classification for one path."""

    BOTH_MODIFIED = "both_modified"
    ADDED_BY_US = "added_by_us"
    ADDED_BY_THEM = "added_by_them"
    DELETED_BY_US = "deleted_by_us"
    DELETED_BY_THEM = "deleted_by_them"
    BOTH_ADDED = "both_added"
    BOTH_DELETED = "both_deleted"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class GitConflict(ClosedModel):
    """One path left in Git's unmerged index."""

    path: Field[core.ProjectPath] = proto_field(
        description="Conflicting project path.", number=1
    )
    status: Field[GitConflictStatus] = proto_field(
        description="Conflict status derived from Git's index stages.", number=2
    )


@scalar(
    owner=MCP,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rec_[a-z2-7]{26}$",
)
class RecoveryId(ProtocolRoot):
    """Identity of one retained conventional Git merge recovery."""


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "RecoveryReason",
        (
            EnumValue("conflicts", "RECOVERY_REASON_CONFLICTS", 1),
            EnumValue("custom_driver", "RECOVERY_REASON_CUSTOM_DRIVER", 2),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "conflicts": "Built-in three-way merging produced unmerged index entries.",
            "custom_driver": "Repository attributes selected a custom merge driver Rift will not execute.",
        }
    },
)
class RecoveryReason(str, Enum):
    """Why integration requires a conventional Git worktree."""

    CONFLICTS = "conflicts"
    CUSTOM_DRIVER = "custom_driver"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RecoveryManifest(ClosedModel):
    """Content guard for one conventional recovery. Rift rescans the index and worktree under
    Git locks before continue or abort; equal digests mean the reviewed bytes are unchanged."""

    recovery: Field[RecoveryId] = proto_field(number=1)
    index: Field[core.Digest] = proto_field(
        description="SHA-256 of canonical index entries and stages.", number=2
    )
    worktree: Field[core.Digest] = proto_field(
        description="SHA-256 of canonical tracked and untracked worktree entries.",
        number=3,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class GitRecovery(ClosedModel):
    """Exceptional conventional Git worktree retained for a merge Rift could not complete.
    Normal filesystem projections do not expose Git administrative state."""

    recovery: Field[RecoveryId] = proto_field(
        description="Identity supplied to recovery continue or abort.", number=1
    )
    worktree: Field[GitWorktreeLocation] = proto_field(
        description="Durable worktree in `.rift/recovery` for ordinary Git conflict resolution.",
        number=2,
    )
    operation: Field[core.Digest] = proto_field(
        description=(
            "Deterministic identity of the exact source state, target guard, and request. An "
            "exact retry returns this active recovery instead of allocating another."
        ),
        number=3,
    )
    expected_source: Field[core.ProjectionState] = proto_field(
        description="Projection state that continuation must still observe.", number=4
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch guarded by this recovery.", number=5
    )
    expected_target: Field[core.GitCommit] = proto_field(
        description="Target head continuation must still observe.", number=6
    )
    reason: Field[RecoveryReason] = proto_field(number=7)
    conflicts: Field[list[GitConflict]] = proto_field(
        description="Unmerged paths in project-path order; empty for custom-driver handoff.",
        number=8,
    )
    driver_paths: Field[list[core.ProjectPath]] = proto_field(
        description="Paths selecting a custom driver; empty for a built-in merge conflict.",
        number=9,
    )
    manifest: Field[RecoveryManifest] = proto_field(
        description="Exact recovery bytes most recently scanned by Rift.", number=10
    )

    @model_validator(mode="after")
    def reason_has_evidence(self) -> GitRecovery:
        if self.worktree.ref != f"refs/rift/recovery/{self.recovery.root}":
            raise ValueError("recovery worktree must use its private recovery ref")
        if self.manifest.recovery != self.recovery:
            raise ValueError("recovery manifest must name this recovery")
        if not self.expected_source.dirty:
            raise ValueError("merge recovery requires unintegrated source")
        if self.reason is RecoveryReason.CONFLICTS and not self.conflicts:
            raise ValueError("conflict recovery requires conflict paths")
        if self.reason is RecoveryReason.CONFLICTS and self.driver_paths:
            raise ValueError("conflict recovery cannot carry custom-driver paths")
        if self.reason is RecoveryReason.CUSTOM_DRIVER and not self.driver_paths:
            raise ValueError("custom-driver recovery requires driver paths")
        if self.reason is RecoveryReason.CUSTOM_DRIVER and self.conflicts:
            raise ValueError("custom-driver recovery cannot carry conflict paths")
        return self


@union(
    owner=MCP,
    oneof="variant",
    discriminator="status",
    variants=(
        Variant("applied", "applied", 1, ChangeApplied),
        Variant("refused", "refused", 2, RefusedResult),
        Variant("stale_projection", "stale_projection", 3, StaleProjectionResult),
    ),
)
class ChangeResult(ProtocolRoot):
    """An applied store change, domain refusal, or stale-projection result."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class IntegrateParams(ClosedModel):
    """Squash-integrates one exact current projection into a local branch."""

    target: Field[IntegrationTarget] = proto_field(
        description="Local branch that receives the merged source tree.", number=1
    )
    expected: Field[core.ProjectionState] = proto_field(
        description="Exact current source and retained base to integrate.",
        number=2,
    )
    expected_target: Field[core.GitCommit] = proto_field(
        description="Target head that the final conditional ref update must replace.",
        number=3,
    )
    message: Field[str] = proto_field(
        description="Subject for the single squash commit. Newlines and NUL are refused.",
        min_length=1,
        max_length=256,
        pattern=r"^[^\u0000\r\n]+$",
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class IntegrationValidation(ClosedModel):
    """Adapter and command validation run over one complete merged tree."""

    adapters: Field[ChangeValidation] = proto_field(
        description="Adapter validation of the merged tree.", number=1
    )
    commands: Field[list[ValidatorResult]] = proto_field(
        description="Configured command-validator results in declaration order.",
        number=2,
    )
    valid: Field[bool] = proto_field(
        description="Whether configured adapter validation and every command validator passed.",
        number=3,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class IntegrationSucceeded(ClosedModel):
    """The merged tree passed validation, one squash commit advanced the target, and the
    projection was rebased to that published tree."""

    status: Field[Literal["integrated"]] = proto_field(
        description="Identifies successful integration.", default="integrated"
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch advanced by integration.", number=1
    )
    previous_target: Field[core.GitCommit] = proto_field(
        description="Target head replaced by the conditional update.", number=2
    )
    previous_state: Field[core.ProjectionState] = proto_field(
        description="Exact projection state supplied to integration.", number=3
    )
    integrated: Field[core.GitCommit] = proto_field(
        description="Single-parent squash commit now held by the target ref.", number=4
    )
    validation: Field[IntegrationValidation] = proto_field(
        description="Validation evidence for the merged tree.", number=5
    )
    current_state: Field[core.ProjectionState] = proto_field(
        description="Clean projection rebased to the integrated tree with a fresh token.",
        number=6,
    )
    diff: Field[DiffResourceLink] = proto_field(
        description="Pinned snapshot comparison from the supplied source to the merged tree.",
        number=7,
    )
    degradation: Field[list[core.Diagnostic]] = proto_field(
        description="Post-publication projection invalidation or notification failures.",
        number=8,
    )

    @model_validator(mode="after")
    def publication_is_correlated(self) -> IntegrationSucceeded:
        if not self.validation.valid:
            raise ValueError("successful integration requires valid evidence")
        if self.previous_state.projection != self.current_state.projection:
            raise ValueError("integration states must belong to the same projection")
        if not self.previous_state.dirty:
            raise ValueError("commit-producing integration requires dirty source")
        if self.previous_state.head == self.current_state.head:
            raise ValueError("integration rebase must mint a fresh projection head")
        if self.current_state.dirty:
            raise ValueError("successful integration must return a clean projection")
        if self.current_state.base.commit != self.integrated:
            raise ValueError("integrated commit must become the projection base")
        if self.current_state.base.snapshot != self.current_state.snapshot:
            raise ValueError("clean integration state must render its base snapshot")
        expected_diff = (
            f"rift://diff?from=snapshot:{self.previous_state.snapshot.root}"
            f"&to=snapshot:{self.current_state.snapshot.root}"
        )
        if self.diff.uri.root != expected_diff:
            raise ValueError(
                "integration diff must compare its two immutable snapshots"
            )
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class IntegrationSynchronized(ClosedModel):
    """The merged tree already matched the target tree. No commit was created; Rift only
    rebased the exact projection to the existing target commit and tree."""

    status: Field[Literal["synchronized"]] = proto_field(default="synchronized")
    target: Field[IntegrationTarget] = proto_field(number=1)
    target_head: Field[core.GitCommit] = proto_field(number=2)
    previous_state: Field[core.ProjectionState] = proto_field(number=3)
    current_state: Field[core.ProjectionState] = proto_field(number=4)
    validation: Field[IntegrationValidation] = proto_field(
        description="Validation evidence for the already-target-equivalent merged tree.",
        number=5,
    )
    degradation: Field[list[core.Diagnostic]] = proto_field(
        description="Post-rebase projection invalidation or notification failures.",
        number=6,
    )

    @model_validator(mode="after")
    def synchronization_is_correlated(self) -> IntegrationSynchronized:
        if not self.validation.valid:
            raise ValueError("synchronization requires valid evidence")
        if self.previous_state.projection != self.current_state.projection:
            raise ValueError("integration states must belong to the same projection")
        if self.previous_state.head == self.current_state.head:
            raise ValueError("integration rebase must mint a fresh projection head")
        if self.current_state.dirty:
            raise ValueError("synchronization must return a clean projection")
        if self.current_state.base.commit != self.target_head:
            raise ValueError("target commit must become the projection base")
        if self.current_state.base.snapshot != self.current_state.snapshot:
            raise ValueError("clean integration state must render its base snapshot")
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class IntegrationMergeConflict(ClosedModel):
    """Git reported an unresolved merge. Rift retained a conventional recovery worktree and
    left the session projection and target ref unchanged."""

    status: Field[Literal["merge_conflict"]] = proto_field(
        description="Identifies an unresolved Git merge.", default="merge_conflict"
    )
    recovery: Field[GitRecovery] = proto_field(
        description="Retained, discoverable recovery to resolve and continue or abort.",
        number=1,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class IntegrationRejected(ClosedModel):
    """Validation rejected a clean merge in a disposable projection. Rift discarded the
    candidate and left the target ref unchanged."""

    status: Field[Literal["rejected"]] = proto_field(
        description="Identifies integration rejected by validation.", default="rejected"
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch that remained unchanged.", number=1
    )
    target_head: Field[core.GitCommit] = proto_field(
        description="Target commit used by Git.", number=2
    )
    source: Field[core.ProjectionState] = proto_field(
        description="Exact session state merged for validation.", number=3
    )
    validation: Field[IntegrationValidation] = proto_field(
        description="Evidence that rejected integration.", number=4
    )

    @model_validator(mode="after")
    def validation_is_rejected(self) -> IntegrationRejected:
        if self.validation.valid:
            raise ValueError("rejected integration requires invalid evidence")
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class IntegrationRefused(ClosedModel):
    """Workspace configuration or repository state prevented Git integration from starting."""

    status: Field[Literal["refused"]] = proto_field(
        description="Identifies a refused integration.", default="refused"
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch Rift was asked to integrate.", number=1
    )
    source: Field[core.ProjectionState] = proto_field(
        description="Exact session state selected for integration.", number=2
    )
    expected_target: Field[core.GitCommit] = proto_field(
        description="Target commit supplied by the caller.", number=3
    )
    reason: Field[RefusalReason] = proto_field(
        description="Configuration or repository condition that prevented integration.",
        number=4,
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        description="Evidence that explains the refusal, including a checked-out path when relevant.",
        number=5,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class IntegrationTargetMoved(ClosedModel):
    """The target ref moved before Rift could conditionally advance it. Rift discarded the
    integration candidate."""

    status: Field[Literal["target_moved"]] = proto_field(
        description="Identifies a failed conditional target update.",
        default="target_moved",
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch that changed concurrently.", number=1
    )
    expected: Field[core.GitCommit] = proto_field(
        description="Target commit supplied by the caller.", number=2
    )
    current: Field[core.GitCommit | None] = proto_field(
        default=None,
        description="Target commit observed during the conditional update, or null if deleted.",
        number=3,
    )
    source: Field[core.ProjectionState] = proto_field(
        description="Exact session state used for the discarded integration attempt.",
        number=4,
    )


@union(
    owner=MCP,
    oneof="variant",
    discriminator="status",
    variants=(
        Variant("integrated", "integrated", 1, IntegrationSucceeded),
        Variant("synchronized", "synchronized", 2, IntegrationSynchronized),
        Variant("merge_conflict", "merge_conflict", 3, IntegrationMergeConflict),
        Variant("rejected", "rejected", 4, IntegrationRejected),
        Variant("refused", "refused", 5, IntegrationRefused),
        Variant("target_moved", "target_moved", 6, IntegrationTargetMoved),
        Variant("stale_projection", "stale_projection", 7, StaleProjectionResult),
    ),
)
class IntegrateResult(ProtocolRoot):
    """The complete decision from one Git integration attempt."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RecoveryContinueParams(ClosedModel):
    """Imports a conflict resolution from one retained conventional Git worktree. The index
    must have no unmerged entries and the worktree must have no unstaged or untracked paths.
    Rift imports only the reviewed index tree."""

    expected: Field[RecoveryManifest] = proto_field(
        description="Exact staged index and worktree bytes reviewed by the caller.",
        number=1,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ConsumedRecovery(ClosedModel):
    """Exact recovery guards consumed by successful continuation. It is response evidence and
    does not assert that the conventional worktree or private ref still exists."""

    recovery: Field[RecoveryId] = proto_field(number=1)
    expected_source: Field[core.ProjectionState] = proto_field(number=2)
    target: Field[IntegrationTarget] = proto_field(number=3)
    expected_target: Field[core.GitCommit] = proto_field(number=4)
    manifest: Field[RecoveryManifest] = proto_field(number=5)

    @model_validator(mode="after")
    def manifest_names_recovery(self) -> ConsumedRecovery:
        if self.manifest.recovery != self.recovery:
            raise ValueError("consumed manifest must name this recovery")
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RecoveryIntegrationContinued(ClosedModel):
    """A resolved integration merge was imported, validated, and published to its target ref."""

    status: Field[Literal["integration_continued"]] = proto_field(
        default="integration_continued"
    )
    recovery: Field[ConsumedRecovery] = proto_field(number=1)
    integration: Field[IntegrationSucceeded] = proto_field(
        description="Completed target update and validation evidence.", number=2
    )

    @model_validator(mode="after")
    def result_matches_recovery(self) -> RecoveryIntegrationContinued:
        if self.integration.previous_state != self.recovery.expected_source:
            raise ValueError("integration source must match the consumed recovery")
        if self.integration.target != self.recovery.target:
            raise ValueError("integration target must match the consumed recovery")
        if self.integration.previous_target != self.recovery.expected_target:
            raise ValueError("integration target head must match the consumed recovery")
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RecoveryIntegrationSynchronized(ClosedModel):
    """The reviewed resolution already matched the target tree. Rift rebased the source and
    removed the recovery without creating a commit."""

    status: Field[Literal["integration_synchronized"]] = proto_field(
        default="integration_synchronized"
    )
    recovery: Field[ConsumedRecovery] = proto_field(number=1)
    integration: Field[IntegrationSynchronized] = proto_field(number=2)

    @model_validator(mode="after")
    def result_matches_recovery(self) -> RecoveryIntegrationSynchronized:
        if self.integration.previous_state != self.recovery.expected_source:
            raise ValueError("synchronization source must match the consumed recovery")
        if self.integration.target != self.recovery.target:
            raise ValueError("synchronization target must match the consumed recovery")
        if self.integration.target_head != self.recovery.expected_target:
            raise ValueError(
                "synchronization target head must match the consumed recovery"
            )
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RecoveryIntegrationRejected(ClosedModel):
    """Validation rejected the reviewed resolution. The recovery remains available for more
    edits or explicit abort."""

    status: Field[Literal["integration_rejected"]] = proto_field(
        default="integration_rejected"
    )
    recovery: Field[GitRecovery] = proto_field(
        description="Retained recovery with its rescanned current manifest.", number=1
    )
    integration: Field[IntegrationRejected] = proto_field(number=2)

    @model_validator(mode="after")
    def result_matches_recovery(self) -> RecoveryIntegrationRejected:
        if self.integration.source != self.recovery.expected_source:
            raise ValueError("rejection source must match the retained recovery")
        if self.integration.target != self.recovery.target:
            raise ValueError("rejection target must match the retained recovery")
        if self.integration.target_head != self.recovery.expected_target:
            raise ValueError("rejection target head must match the retained recovery")
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RecoveryIntegrationRefused(ClosedModel):
    """Repository state prevented publication of the reviewed resolution. The recovery remains
    available for retry or explicit abort."""

    status: Field[Literal["integration_refused"]] = proto_field(
        default="integration_refused"
    )
    recovery: Field[GitRecovery] = proto_field(
        description="Retained recovery with its rescanned current manifest.", number=1
    )
    integration: Field[IntegrationRefused] = proto_field(number=2)

    @model_validator(mode="after")
    def result_matches_recovery(self) -> RecoveryIntegrationRefused:
        if self.integration.source != self.recovery.expected_source:
            raise ValueError("refusal source must match the retained recovery")
        if self.integration.target != self.recovery.target:
            raise ValueError("refusal target must match the retained recovery")
        if self.integration.expected_target != self.recovery.expected_target:
            raise ValueError("refusal target head must match the retained recovery")
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RecoveryObsolete(ClosedModel):
    """The source projection, target ref, or recovery bytes moved before continuation. Nothing
    was published; the exact current values let the caller choose whether to abort or restart."""

    status: Field[Literal["obsolete"]] = proto_field(default="obsolete")
    recovery: Field[RecoveryId] = proto_field(number=1)
    expected_source: Field[core.ProjectionState] = proto_field(number=2)
    current_source: Field[core.ProjectionState] = proto_field(number=3)
    target: Field[IntegrationTarget] = proto_field(number=4)
    expected_target: Field[core.GitCommit] = proto_field(number=5)
    current_target: Field[core.GitCommit | None] = proto_field(
        default=None,
        description="Current target commit, or null if the guarded branch was deleted.",
        number=6,
    )
    expected_manifest: Field[RecoveryManifest] = proto_field(number=7)
    current_manifest: Field[RecoveryManifest] = proto_field(number=8)

    @model_validator(mode="after")
    def values_are_correlated(self) -> RecoveryObsolete:
        if self.expected_source.projection != self.current_source.projection:
            raise ValueError("recovery source states must name the same projection")
        if self.expected_manifest.recovery != self.recovery:
            raise ValueError("expected manifest must name this recovery")
        if self.current_manifest.recovery != self.recovery:
            raise ValueError("current manifest must name this recovery")
        if (
            self.expected_source == self.current_source
            and self.expected_target == self.current_target
            and self.expected_manifest == self.current_manifest
        ):
            raise ValueError("obsolete recovery requires at least one changed guard")
        return self


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RecoveryListParams(ClosedModel):
    """Selects one captured page of active recoveries in recovery-id order."""

    limit: Field[int] = proto_field(default=100, ge=1, le=1000, number=1)
    cursor: Field[Cursor | None] = proto_field(default=None, number=2)


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RecoveryListResult(ClosedModel):
    """One page of durable merge recoveries. Listing rescans each conventional worktree and
    returns the manifest a caller can review and pass to continue or abort."""

    recoveries: Field[list[GitRecovery]] = proto_field(number=1)
    next_cursor: Field[Cursor | None] = proto_field(default=None, number=2)


@union(
    owner=MCP,
    oneof="variant",
    discriminator="status",
    variants=(
        Variant(
            "integration_continued",
            "integration_continued",
            1,
            RecoveryIntegrationContinued,
        ),
        Variant("obsolete", "obsolete", 2, RecoveryObsolete),
        Variant(
            "integration_synchronized",
            "integration_synchronized",
            3,
            RecoveryIntegrationSynchronized,
        ),
        Variant(
            "integration_rejected",
            "integration_rejected",
            4,
            RecoveryIntegrationRejected,
        ),
        Variant(
            "integration_refused",
            "integration_refused",
            5,
            RecoveryIntegrationRefused,
        ),
    ),
)
class RecoveryContinueResult(ProtocolRoot):
    """A completed integration or failed saved-state comparison."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RecoveryAbortParams(ClosedModel):
    """Previews or removes one retained recovery without changing source or a public Git ref.
    The manifest prevents deleting recovery edits the caller did not review. A mismatch fails
    with `recovery_moved`; an unrepresentable special file fails with `unsupported_path`."""

    expected: Field[RecoveryManifest] = proto_field(number=1)
    confirm: Field[bool] = proto_field(default=False, number=2)


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RecoveryAbortPreview(ClosedModel):
    """The recovery still exists and no cleanup was performed."""

    status: Field[Literal["preview"]] = proto_field(default="preview")
    recovery: Field[GitRecovery] = proto_field(number=1)


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class RecoveryAborted(ClosedModel):
    """The reviewed worktree, private ref, and recovery pins were removed."""

    status: Field[Literal["aborted"]] = proto_field(default="aborted")
    recovery: Field[RecoveryId] = proto_field(number=1)
    manifest: Field[RecoveryManifest] = proto_field(
        description="Exact final manifest accepted before cleanup.", number=2
    )

    @model_validator(mode="after")
    def manifest_names_recovery(self) -> RecoveryAborted:
        if self.manifest.recovery != self.recovery:
            raise ValueError("aborted manifest must name this recovery")
        return self


@union(
    owner=MCP,
    oneof="variant",
    discriminator="status",
    variants=(
        Variant("preview", "preview", 1, RecoveryAbortPreview),
        Variant("aborted", "aborted", 2, RecoveryAborted),
    ),
)
class RecoveryAbortResult(ProtocolRoot):
    """A recovery cleanup preview or completed cleanup."""


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
                "`ActionKey`. Adapter actions use this order because they carry no relevance "
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


def _file_resource_path(uri: FileResourceUri | core.FileId) -> str:
    prefix = "rift://file/"
    return uri.root.removeprefix(prefix).partition("?")[0]


def _file_resource_query(uri: FileResourceUri) -> dict[str, list[str]]:
    query = uri.root.partition("?")[2]
    if not query:
        return {}
    return parse_qs(query, keep_blank_values=True, strict_parsing=True)


def _validate_file_identity(
    uri: FileResourceUri, at: core.ResolvedSnapshot, file: core.File
) -> None:
    if _file_resource_path(uri) != _file_resource_path(file.id):
        raise ValueError("file payload URI and entry must name the same path")
    revision = _file_resource_query(uri).get("rev", [None])[0]
    if (
        revision is not None
        and revision.startswith("snapshot:")
        and revision != f"snapshot:{at.snapshot.id.root}"
    ):
        raise ValueError("file payload must resolve its snapshot selector exactly")


def _validate_regular_file_payload(
    *,
    uri: FileResourceUri,
    at: core.ResolvedSnapshot,
    file: core.File,
    start: int,
    end: int,
    total_bytes: int,
    content_bytes: int,
    next_uri: FileResourceUri | None,
) -> None:
    _validate_file_identity(uri, at, file)
    entry = file.content.root
    if entry.kind != "regular":
        raise ValueError("text or base64 encoding requires a regular entry")
    if total_bytes != entry.size:
        raise ValueError("total_bytes must equal the regular entry size")
    if not start <= end <= total_bytes:
        raise ValueError("file range must satisfy start <= end <= total_bytes")
    if content_bytes != end - start:
        raise ValueError("content byte length must equal end - start")

    request = _file_resource_query(uri)
    requested_start = int(request.get("start", ["0"])[0])
    if start != requested_start:
        raise ValueError("payload start must equal the requested start")
    if "length" in request and end - start > int(request["length"][0]):
        raise ValueError("payload exceeds the requested range length")

    if end == total_bytes:
        if next_uri is not None:
            raise ValueError("EOF payload cannot carry a continuation")
        return
    if next_uri is None:
        raise ValueError("a partial file payload requires a continuation")
    if _file_resource_path(next_uri) != _file_resource_path(uri):
        raise ValueError("file continuation must keep the same path")
    continuation = _file_resource_query(next_uri)
    expected_revision = f"snapshot:{at.snapshot.id.root}"
    if continuation.get("rev") != [expected_revision]:
        raise ValueError("file continuation must pin the resolved snapshot")
    if continuation.get("start") != [str(end)]:
        raise ValueError("file continuation must begin at end")
    if "length" not in continuation:
        raise ValueError("file continuation must carry an explicit length")


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FileResourcePayloadUtf8(ClosedModel):
    """A regular-file range whose bytes decode as UTF-8. Start and end fall on UTF-8
    code-point boundaries."""

    encoding: Field[Literal["utf8"]] = proto_field(default="utf8")
    uri: Field[FileResourceUri] = proto_field(
        description="Exact resource request this payload answers.", number=1
    )
    at: Field[core.ResolvedSnapshot] = proto_field(
        description="Exact source snapshot from which the entry and bytes were read.",
        number=2,
    )
    file: Field[core.File] = proto_field(
        description="Complete regular source-store entry.", number=3
    )
    start: Field[int] = proto_field(ge=0, le=9007199254740991, number=4)
    end: Field[int] = proto_field(ge=0, le=9007199254740991, number=5)
    total_bytes: Field[int] = proto_field(ge=0, le=9007199254740991, number=6)
    content: Field[str] = proto_field(number=7)
    next: Field[FileResourceUri | None] = proto_field(
        default=None,
        description="Next exact-snapshot range beginning at `end`, or null at EOF.",
        number=8,
    )

    @model_validator(mode="after")
    def range_is_correlated(self) -> FileResourcePayloadUtf8:
        _validate_regular_file_payload(
            uri=self.uri,
            at=self.at,
            file=self.file,
            start=self.start,
            end=self.end,
            total_bytes=self.total_bytes,
            content_bytes=len(self.content.encode("utf-8")),
            next_uri=self.next,
        )
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FileResourcePayloadBase64(ClosedModel):
    """A range carried as canonical base64 because its bytes do not form valid UTF-8."""

    encoding: Field[Literal["base64"]] = proto_field(default="base64")
    uri: Field[FileResourceUri] = proto_field(
        description="Exact resource request this payload answers.", number=1
    )
    at: Field[core.ResolvedSnapshot] = proto_field(
        description="Exact source snapshot from which the entry and bytes were read.",
        number=2,
    )
    file: Field[core.File] = proto_field(
        description="Complete regular source-store entry.", number=3
    )
    start: Field[int] = proto_field(ge=0, le=9007199254740991, number=4)
    end: Field[int] = proto_field(ge=0, le=9007199254740991, number=5)
    total_bytes: Field[int] = proto_field(ge=0, le=9007199254740991, number=6)
    content: Field[str] = proto_field(
        pattern="^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$",
        number=7,
    )
    next: Field[FileResourceUri | None] = proto_field(
        default=None,
        description="Next exact-snapshot range beginning at `end`, or null at EOF.",
        number=8,
    )

    @model_validator(mode="after")
    def range_is_correlated(self) -> FileResourcePayloadBase64:
        decoded = base64.b64decode(self.content, validate=True)
        if base64.b64encode(decoded).decode("ascii") != self.content:
            raise ValueError("file content must use canonical padded base64")
        try:
            decoded.decode("utf-8")
        except UnicodeDecodeError:
            pass
        else:
            raise ValueError("valid UTF-8 file ranges must use utf8 encoding")
        _validate_regular_file_payload(
            uri=self.uri,
            at=self.at,
            file=self.file,
            start=self.start,
            end=self.end,
            total_bytes=self.total_bytes,
            content_bytes=len(decoded),
            next_uri=self.next,
        )
        return self


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FileResourcePayloadNone(ClosedModel):
    """A complete non-regular source-store entry. Its empty interval carries no bytes and has
    no continuation."""

    encoding: Field[Literal["none"]] = proto_field(default="none")
    uri: Field[FileResourceUri] = proto_field(
        description="Exact resource request this payload answers.", number=1
    )
    at: Field[core.ResolvedSnapshot] = proto_field(
        description="Exact source snapshot from which the entry was read.", number=2
    )
    file: Field[core.File] = proto_field(
        description="Complete LFS-pointer, symlink, or gitlink entry.", number=3
    )
    start: Field[Literal[0]] = proto_field(default=0, number=4)
    end: Field[Literal[0]] = proto_field(default=0, number=5)
    total_bytes: Field[Literal[0]] = proto_field(default=0, number=6)
    content: Field[None] = proto_field(default=None, number=7)
    next: Field[None] = proto_field(default=None, number=8)

    @model_validator(mode="after")
    def entry_is_non_regular(self) -> FileResourcePayloadNone:
        if self.file.content.root.kind == "regular":
            raise ValueError("none encoding requires a non-regular entry")
        _validate_file_identity(self.uri, self.at, self.file)
        values = parse_qs(
            self.uri.root.partition("?")[2],
            keep_blank_values=True,
            strict_parsing=True,
        )
        if int(values.get("start", ["0"])[0]) != 0:
            raise ValueError("non-regular file reads must start at zero")
        return self


@union(
    owner=MCP,
    oneof="variant",
    discriminator="encoding",
    variants=(
        Variant("utf8", "utf8", 1, FileResourcePayloadUtf8),
        Variant("base64", "base64", 2, FileResourcePayloadBase64),
        Variant("none", "none", 3, FileResourcePayloadNone),
    ),
)
class FileResourcePayload(ProtocolRoot):
    "One complete source-store entry and a bounded byte range at an exact resolved snapshot. Regular files carry UTF-8 text where the selected bytes form valid UTF-8 and canonical base64 otherwise. `next` uses a `snapshot:` selector and continues at `end`; non-regular entries carry their complete metadata and no bytes."


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
    TreeParams,
    TreeResult,
    OutlineParams,
    OutlineItem,
    OutlineResult,
    SearchParams,
    SearchHit,
    ActionsResourceLink,
    ActionResourceLink,
    FsResourceLink,
    ResourceLink,
    SymbolResourcePayload,
    DiffResourcePayload,
    ProjectionKind,
    FilesystemProjectionSummary,
    FsResourcePayload,
    Contract,
    WorkspacePath,
    ProjectionPath,
    ProjectionLocation,
    GitWorktreePath,
    GitWorktreeLocation,
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
    ProjectionOpenParams,
    ProjectionOpenResult,
    ProjectionCloseParams,
    ProjectionCloseResult,
    DebugLimits,
    ExecutionLimits,
    Limits,
    RepositoryResourceUri,
    FsResourceUri,
    FileResourceUri,
    RepositoryResourcePayload,
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
    ResourceReadParams,
    ResourceReadResult,
    SearchHitTarget,
    EditParams,
    PatchParams,
    RewriteParams,
    RevertParams,
    RenameParams,
    MoveParams,
    DeleteParams,
    ChangeSignatureParams,
    ActParams,
    ProjectionRestoreParams,
    ResolvedOperation,
    CommandValidator,
    ValidatorResult,
    ChangeValidation,
    ChangeSummary,
    ProjectionRestoreResult,
    RefusalReason,
    ChangeResult,
    IntegrationTarget,
    IntegrateParams,
    GitConflict,
    RecoveryId,
    GitRecovery,
    IntegrateResult,
    RecoveryListParams,
    RecoveryListResult,
    RecoveryContinueParams,
    RecoveryContinueResult,
    RecoveryAbortParams,
    RecoveryAbortResult,
    ResultOrder,
    FileResourcePayload,
    ExecutionAvailability,
    LanguageSupport,
)
