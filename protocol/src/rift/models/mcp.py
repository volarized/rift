from __future__ import annotations

from pydantic import field_validator

from . import core
from .base import *


@definition(
    owner=MCP,
    public=True,
    proto=Proto.scalar(ProtoFieldDescriptor.TYPE_STRING),
    schema_extra={},
)
class Cursor(
    ProtocolRoot[
        (
            "Annotated[str, schema_field(description='An opaque string that continues a "
            "paginated answer from where the last page ended. It binds the request, state, "
            "order, and page size. A mismatch returns `cursor_invalid`.', min_length=1, "
            "max_length=4096)]"
        )
    ]
):
    "An opaque string that continues a paginated answer from where the last page ended. It binds the request, state, order, and page size. A mismatch returns `cursor_invalid`."


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
        default=None, description="Revision whose tree is listed.", number=6
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class TreeResult(ClosedModel):
    "One page of a project tree, ordered by project-path UTF-8 bytes. A directory precedes a file at the same path, though a valid snapshot normally contains only one."

    at: Field[core.Snapshot] = proto_field(
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
        description="Revision whose file and semantic facts are read.",
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

    at: Field[core.Snapshot] = proto_field(
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
    "Which entity kinds may be returned. Type data is attached to the Symbol and Node views that bind it, and filters can search those attachments."

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
            "views that bind it, and filters can search those attachments."
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
        description="Which revision to answer against. Absent, the default branch at its latest commit.",
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


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PreviewResourceLink(ClosedModel):
    "MCP link to a retained preview plan. The fixed name and media type let `CandidateSummary.resource` admit only the resource that carries its complete contract."

    type: Field[Literal["resource_link"]] = proto_field(
        description="MCP's tag for a link to a resource inside tool output.", number=1
    )
    uri: Field[PreviewResourceUri] = proto_field(
        description="The retained preview to read, optionally continued by a cursor.",
        number=2,
    )
    name: Field[Literal["preview"]] = proto_field(
        description="The resource family this link belongs to.", number=3
    )
    mimeType: Field[Literal["application/vnd.rift.preview+json"]] = proto_field(
        description="What a read of this URI returns: `PreviewResourcePayload` as JSON.",
        number=4,
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


@definition(
    owner=MCP,
    public=True,
    proto=Proto.union(
        Oneof("variant"),
        (
            Variant("symbol", "symbol", 1, SymbolResourceLink),
            Variant("repository", "repository", 2, RepositoryResourceLink),
            Variant("diff", "diff", 3, DiffResourceLink),
            Variant("file", "file", 4, FileResourceLink),
            Variant("preview", "preview", 5, PreviewResourceLink),
        ),
    ),
    schema_extra={},
)
class ResourceLink(
    ProtocolRoot[
        (
            "Annotated[SymbolResourceLink | RepositoryResourceLink | DiffResourceLink | "
            "FileResourceLink | PreviewResourceLink, schema_field(discriminator='mimeType')]"
        )
    ]
):
    "A link to one Rift resource, as MCP carries it. The resource's name and media type are fixed per resource, and the URI is the one that resource accepts."


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SymbolResourcePayload(ClosedModel):
    """JSON payload for one symbol at one snapshot."""

    uri: Field[core.SymbolId] = proto_field(
        description="The symbol this payload answers for, echoed back so a link and its content carry the same address.",
        number=1,
    )
    at: Field[core.Snapshot] = proto_field(
        description=(
            "The snapshot this answer was resolved against. Pass it back as `rev` when a "
            "later call has to agree with this one."
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
    "One page of a comparison. `from` and `to` record the resolved revisions. Git answers without an adapter, so the comparison works for every file."

    uri: Field[core.DiffId] = proto_field(
        description="The comparison this payload answers for, echoed back with the cursor that produced this page.",
        number=1,
    )
    from_: Field[core.Snapshot] = proto_field(
        alias="from", description="The old side, as it resolved.", number=2
    )
    to: Field[core.Snapshot] = proto_field(
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


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class Contract(ClosedModel):
    "Which protocol contract this server speaks. The version selects compatibility behavior. The schema identifier names the JSON contract exposed through MCP."

    protocol: Field[core.ProtocolVersion] = proto_field(
        description="The protocol version both sides have to agree on before any other field is meaningful.",
        number=1,
    )
    schema_: Field[Literal["https://volar.sh/rift/protocol/mcp.json"]] = proto_field(
        alias="schema",
        description=(
            "Canonical identifier of the MCP JSON Schema. The protocol version selects its "
            "matching generated Protobuf packages."
        ),
        number=2,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class Limits(ClosedModel):
    "The ceilings this server enforces on MCP requests and responses. They come from host policy at launch, so two workspaces running the same Rift can differ. A request over one of them, or a response that would be, fails with `limit_exceeded` carrying `LimitEvidence`."

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
            "`limit_exceeded` before retaining a preview when one item exceeds this value. "
            "Host policy sets it at or below 49152 bytes, leaving page space for identity and "
            "cursors."
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
        description="Most top-level `Change` values one preview request accepts.",
        ge=1,
        le=4294967295,
        number=7,
    )
    max_edits: Field[int] = proto_field(
        description="Most concrete `Edit` values one resolved candidate may contain across every change.",
        ge=1,
        le=1000000,
        number=8,
    )
    max_validators: Field[int] = proto_field(
        description=(
            "How many caller-supplied checks may run over one proposed change. Zero where "
            "this workspace runs none, which is every profile below `full`."
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
    record_retention_seconds: Field[int] = proto_field(
        description=(
            "Minimum time Rift retains previews and idempotent publish results after their "
            "last access. A later read returns `record_reclaimed` once collection removes the "
            "record."
        ),
        ge=60,
        le=31536000,
        number=11,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.scalar(ProtoFieldDescriptor.TYPE_STRING),
    schema_extra={},
)
class RepositoryResourceUri(
    ProtocolRoot[
        (
            "Annotated[str, schema_field(description='Paginated workspace metadata and "
            "capabilities at one revision.', pattern=\"^rift://repository(\\\\?(rev=[A-Za-z0-9._"
            "~%!$&'()*+,;:@/-]{1,256}(&cursor=[^&#]+)?|cursor=[^&#]+))?$\")]"
        )
    ]
):
    """Paginated workspace metadata and capabilities at one revision."""


@definition(
    owner=MCP,
    public=True,
    proto=Proto.scalar(ProtoFieldDescriptor.TYPE_STRING),
    schema_extra={},
)
class FileResourceUri(
    ProtocolRoot[
        (
            'Annotated[str, schema_field(description="URI for one file content range. `start` '
            "and `length` are byte coordinates and appear together. Their absence starts at "
            "byte zero with the server's advertised chunk bound.\", "
            "pattern=\"^rift://file/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-F]{2}){1,1000}(?:\\"
            "\\?rev=[A-Za-z0-9._~%!$&'()*+,;:@/-]{1,256}(?:&start=[0-9]+&length=[1-9][0-9]*)?|"
            '\\\\?start=[0-9]+&length=[1-9][0-9]*)?$", min_length=13, max_length=1200)]'
        )
    ]
):
    "URI for one file content range. `start` and `length` are byte coordinates and appear together. Their absence starts at byte zero with the server's advertised chunk bound."


@definition(
    owner=MCP,
    public=True,
    proto=Proto.scalar(ProtoFieldDescriptor.TYPE_STRING),
    schema_extra={},
)
class PreviewResourceUri(
    ProtocolRoot[
        (
            "Annotated[str, schema_field(description='URI for one page of a retained preview. "
            "The path carries its opaque `PreviewId`; an optional cursor continues the same "
            "immutable plan.', pattern='^rift://preview/[A-Za-z0-9_-]{16,128}(\\\\?cursor=[^&#]"
            "+)?$')]"
        )
    ]
):
    "URI for one page of a retained preview. The path carries its opaque `PreviewId`; an optional cursor continues the same immutable plan."


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Tools",
        (
            EnumValue("tree", "TOOLS_TREE", 1),
            EnumValue("outline", "TOOLS_OUTLINE", 2),
            EnumValue("search", "TOOLS_SEARCH", 3),
            EnumValue("match", "TOOLS_MATCH", 4),
            EnumValue("actions", "TOOLS_ACTIONS", 5),
            EnumValue("apply", "TOOLS_APPLY", 6),
            EnumValue("integrate", "TOOLS_INTEGRATE", 7),
            EnumValue("persist", "TOOLS_PERSIST", 8),
            EnumValue("execute", "TOOLS_EXECUTE", 9),
            EnumValue("debug_start", "TOOLS_DEBUG_START", 10),
            EnumValue("debug_get_frame", "TOOLS_DEBUG_GET_FRAME", 11),
            EnumValue("debug_stop", "TOOLS_DEBUG_STOP", 12),
        ),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "tree": "Snapshot-pinned project tree listing over every file, without an adapter.",
            "outline": "Adapter-owned declaration structure and diagnostics for one file.",
            "search": "Ranked lookup of symbols, nodes and files.",
            "match": "Literal, regular-expression and structural matching, with a key per hit.",
            "actions": "The fixes and refactors an adapter offers at one address.",
            "apply": (
                "Previews, refreshes, and publishes deterministic candidates through the retained "
                "validation contract."
            ),
            "integrate": "Previews, refreshes, and publishes a validated merge into a local branch.",
            "persist": "Materializes selected paths from an accepted commit into the connection worktree.",
            "execute": "Evaluates a code block in one adapter's revision-specific project runtime.",
            "debug_start": "Starts an inspect-only debugging evaluation.",
            "debug_get_frame": "Reads one retained stack frame from a debugging session.",
            "debug_stop": "Releases a debugging session and its execution workspace.",
        }
    },
)
class RepositoryResourcePayloadToolsItemTools(str, Enum):
    TREE = "tree"
    OUTLINE = "outline"
    SEARCH = "search"
    MATCH = "match"
    ACTIONS = "actions"
    APPLY = "apply"
    INTEGRATE = "integrate"
    PERSIST = "persist"
    EXECUTE = "execute"
    DEBUG_START = "debug_start"
    DEBUG_GET_FRAME = "debug_get_frame"
    DEBUG_STOP = "debug_stop"


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
            EnumValue("preview", "RESOURCES_PREVIEW", 5),
        ),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "repository": "Workspace capabilities, resolved state, request limits, and retention policy.",
            "symbol": "One symbol, its nodes, its edges and its diagnostics.",
            "diff": "What changed between two revisions.",
            "file": "One file's tree entry and its bytes.",
            "preview": "One retained candidate's requests, resolved plans, validation evidence, and confirmations.",
        }
    },
)
class RepositoryResourcePayloadResourcesItemResources(str, Enum):
    REPOSITORY = "repository"
    SYMBOL = "symbol"
    DIFF = "diff"
    FILE = "file"
    PREVIEW = "preview"


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
    "Git object hash used by this repository. It determines whether a `Commit` contains 40 or 64 hexadecimal characters."

    SHA1 = "sha1"
    SHA256 = "sha256"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RepositoryResourcePayload(ClosedModel):
    "Capability manifest for one repository workspace, including configured languages, adapter support, resolved state, and request limits."

    uri: Field[RepositoryResourceUri] = proto_field(
        description="The URI this payload answers for, echoed back with the revision and cursor it resolved.",
        number=1,
    )
    at: Field[core.Snapshot] = proto_field(
        description=(
            "The snapshot this answer was resolved against. Pass it back as `rev` when a "
            "later call has to agree with this one."
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
    tools: Field[list[RepositoryResourcePayloadToolsItemTools]] = proto_field(
        description=(
            "The MCP tools this workspace serves. A tool the profile or host policy does not "
            "reach is absent from this list and from `tools/list`."
        ),
        number=7,
        json_schema_extra={"uniqueItems": True},
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
            "`Commit` is, so a client that validates one has to read this first."
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
    profile: Field[ConformanceProfile] = proto_field(
        description=(
            "The tier this workspace passes on this host. `edit` gates `apply` and "
            "`integrate`; `full` also gates `persist` and command validators."
        ),
        number=12,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class SearchResult(ClosedModel):
    "One page of search hits, and what the page is worth. `coverage` is what makes an empty page readable: nothing matched, or Rift could not see far enough to know."

    at: Field[core.Snapshot] = proto_field(
        description=(
            "The snapshot this answer was resolved against. Pass it back as `rev` when a "
            "later call has to agree with this one."
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
    "One adapter action discovered at an address. It combines a portable descriptor with a state-bound resolution key."

    descriptor: Field[core.ActionDescriptor] = proto_field(
        description="What the action does, what it applies to, and the schema of the arguments it takes.",
        number=1,
    )
    key: Field[core.ActionKey] = proto_field(
        description="Language, snapshot, and adapter token used to resolve this offer.",
        number=2,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ActionsParams(ClosedModel):
    "What to ask an adapter for, and where. `target` picks the place, `only` narrows the kinds, and `rev` selects the source state."

    target: Field[core.Address] = proto_field(
        description="Where in the code to ask. A symbol, a node, a byte range, or a match you already have.",
        number=1,
    )
    only: Field[list[core.ActionKind]] = proto_field(
        description=(
            "Kind prefixes to keep. `refactor` returns everything under it; an empty list "
            "returns every action the adapter offers."
        ),
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
    limit: Field[int | None] = proto_field(
        default=None,
        description="Most actions to return in one page.",
        ge=1,
        le=4294967295,
        number=3,
    )
    cursor: Field[Cursor | None] = proto_field(
        default=None,
        description="Continues a previous call. Omit for the first page.",
        number=4,
    )
    rev: Field[core.Revision | None] = proto_field(
        default=None,
        description="Which revision to answer against. Absent, the default branch at its latest commit.",
        number=5,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ActionsResult(ClosedModel):
    "Discovered actions for one pinned address. Offers sort by language name, dialect with null first, target, kind, argument contract, argument schema, guarantees, full descriptor, and token. Object fields use RFC 8785 ordering. The cursor binds this order, the snapshot, and the adapter build."

    at: Field[core.Snapshot] = proto_field(
        description=(
            "The snapshot this answer was resolved against. Pass it back as `rev` when a "
            "later call has to agree with this one."
        ),
        number=1,
    )
    actions: Field[list[ActionOffer]] = proto_field(
        description="The actions on this page.", number=2
    )
    coverage: Field[core.Coverage] = proto_field(
        description=(
            "Whether the adapter could answer here. A language with no action support returns "
            "an empty list with `unsupported` coverage and its reason."
        ),
        number=3,
    )
    next_cursor: Field[Cursor | None] = proto_field(
        description="Pass this back to get the next page. Null on the last one.",
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ExecuteParams(ClosedModel):
    """Evaluate a caller-provided code block with one configured language adapter against an
    exact revision. The server materializes a disposable execution workspace before crossing
    the adapter seam."""

    language: Field[core.Language] = proto_field(
        description="Exact language and optional dialect selecting the adapter.", number=1
    )
    block: Field[core.CodeBlock] = proto_field(
        description="Source to evaluate and its project-relative working directory.", number=2
    )
    rev: Field[core.Revision | None] = proto_field(
        default=None,
        description="Revision whose project source and runtime configuration provide context.",
        number=3,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ExecuteResult(ClosedModel):
    """Bounded result of one execution. Writes made by evaluated code are absent because its
    execution workspace is discarded."""

    at: Field[core.Snapshot] = proto_field(
        description="Exact snapshot used to prepare the execution workspace.", number=1
    )
    language: Field[core.Language] = proto_field(
        description="Adapter that evaluated the block.", number=2
    )
    result: Field[core.ExecutionResult] = proto_field(
        description="Runtime status, bounded output, and structured diagnostics.", number=3
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.scalar(ProtoFieldDescriptor.TYPE_STRING),
    schema_extra={},
)
class DebugSessionId(
    ProtocolRoot[
        "Annotated[str, schema_field(description='Opaque connection-bound identity of one debugging session. It is valid only on the MCP connection that started it and until debug_stop or connection cleanup.', pattern='^[A-Za-z0-9_-]{16,128}$')]"
    ]
):
    """Opaque connection-bound identity of one debugging session."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DebugSession(ClosedModel):
    """Public summary of an inspect-only debugging evaluation. Start runs the block to normal
    completion or an unhandled failure; failed runs retain their stack for frame reads."""

    id: Field[DebugSessionId] = proto_field(
        description="Server-minted public handle. The adapter-local token never crosses MCP.",
        number=1,
    )
    at: Field[core.Snapshot] = proto_field(
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
        description="How the debugging evaluation ended and its bounded output.", number=5
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DebugStartParams(ClosedModel):
    """Start an inspect-only debugging evaluation in a disposable execution workspace."""

    language: Field[core.Language] = proto_field(
        description="Exact language and optional dialect selecting the adapter.", number=1
    )
    block: Field[core.CodeBlock] = proto_field(number=2)
    rev: Field[core.Revision | None] = proto_field(
        default=None,
        description="Revision whose project source and runtime configuration provide context.",
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
    key: Field[core.MatchKey] = proto_field(
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
    key: Field[core.MatchKey] = proto_field(
        description=(
            "Identity of this match and the state it was found in. An edit addressed at it is "
            "checked against this before it lands."
        ),
        number=3,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.union(
        Oneof("variant"),
        (
            Variant("structural", "structural", 1, MatchHitStructural),
            Variant("text", "text", 2, MatchHitText),
        ),
    ),
    schema_extra={},
)
class MatchHit(
    ProtocolRoot[
        "Annotated[MatchHitStructural | MatchHitText, schema_field(discriminator='kind')]"
    ]
):
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
        description="Which revision to answer against. Absent, the default branch at its latest commit.",
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class MatchResult(ClosedModel):
    "One page of matches and the state they were found in. Matches sort by file bytes, range, and canonical key. Rift checks the key against `at` before applying an addressed edit."

    at: Field[core.Snapshot] = proto_field(
        description=(
            "The snapshot this answer was resolved against. Pass it back as `rev` when a "
            "later call has to agree with this one."
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
    at: Field[core.Snapshot] = proto_field(
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
            EnumValue("revision_not_accepted", "ERROR_CODE_REVISION_NOT_ACCEPTED", 5),
            EnumValue(
                "semantic_snapshot_mismatch", "ERROR_CODE_SEMANTIC_SNAPSHOT_MISMATCH", 6
            ),
            EnumValue(
                "semantic_snapshot_unavailable",
                "ERROR_CODE_SEMANTIC_SNAPSHOT_UNAVAILABLE",
                7,
            ),
            EnumValue("resource_not_found", "ERROR_CODE_RESOURCE_NOT_FOUND", 8),
            EnumValue("record_reclaimed", "ERROR_CODE_RECORD_RECLAIMED", 9),
            EnumValue("content_unavailable", "ERROR_CODE_CONTENT_UNAVAILABLE", 10),
            EnumValue("cursor_invalid", "ERROR_CODE_CURSOR_INVALID", 11),
            EnumValue(
                "cursor_snapshot_mismatch", "ERROR_CODE_CURSOR_SNAPSHOT_MISMATCH", 12
            ),
            EnumValue("cancelled", "ERROR_CODE_CANCELLED", 13),
            EnumValue("deadline_exceeded", "ERROR_CODE_DEADLINE_EXCEEDED", 14),
            EnumValue("limit_exceeded", "ERROR_CODE_LIMIT_EXCEEDED", 15),
            EnumValue("worktree_busy", "ERROR_CODE_WORKTREE_BUSY", 16),
            EnumValue("adapter_unavailable", "ERROR_CODE_ADAPTER_UNAVAILABLE", 17),
            EnumValue(
                "adapter_protocol_error", "ERROR_CODE_ADAPTER_PROTOCOL_ERROR", 18
            ),
            EnumValue("adapter_timeout", "ERROR_CODE_ADAPTER_TIMEOUT", 19),
            EnumValue("storage_failure", "ERROR_CODE_STORAGE_FAILURE", 20),
            EnumValue(
                "validator_execution_failure",
                "ERROR_CODE_VALIDATOR_EXECUTION_FAILURE",
                21,
            ),
            EnumValue("internal_error", "ERROR_CODE_INTERNAL_ERROR", 22),
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
                "Host policy refused — a path outside the workspace, a revision this deployment "
                "does not expose. The same request fails the same way."
            ),
            "snapshot_not_found": (
                "The revision does not resolve here: a deleted branch, a commit never fetched, a "
                "working tree that has been removed. Retrying helps only once the revision "
                "exists."
            ),
            "revision_not_accepted": (
                "The commit exists, but this connection has no accepted publication for it. "
                "Publish its preview before asking `persist` to materialize it."
            ),
            "semantic_snapshot_mismatch": (
                "The snapshot passed back is no longer one the server can answer from — the "
                "working tree moved under it, or the adapter that held its facts was restarted. "
                "Re-read the current snapshot and rebuild whatever was pinned to the old one."
            ),
            "semantic_snapshot_unavailable": (
                "The revision resolves, but Rift has no index for it yet: the adapters are still "
                "analysing, or that state's facts were dropped. Retrying once indexing catches up "
                "succeeds."
            ),
            "resource_not_found": (
                "The URI is well-formed and resolves to nothing — no such symbol, no such file at "
                "that revision. Retrying does not help."
            ),
            "record_reclaimed": (
                "The record existed and has been vacuumed away after its retention window. It "
                "stays distinct from `resource_not_found` because a retry that read a silent miss "
                "would re-run work that already happened. Nothing brings it back."
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
            "worktree_busy": (
                "Another Rift process holds the lease on the working tree this request has to "
                "touch. Contention is transient, so the same request retries."
            ),
            "adapter_unavailable": (
                "No adapter is running for the language the request names: none is installed, or "
                "the one that was has died. A structural query against a language with no adapter "
                "lands here."
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
                "Rift could not read or write its own state — the semantic database, the git "
                "object store, the socket directory. Worth retrying only if the cause was "
                "transient, such as a disk that has since been cleared."
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
        }
    },
)
class ErrorCode(str, Enum):
    "Why a request failed, as a stable code a caller branches on. The code is the complete classification. Domain results such as unsupported coverage and edit refusal use their typed result values."

    INVALID_REQUEST = "invalid_request"
    UNSUPPORTED_PROTOCOL = "unsupported_protocol"
    PERMISSION_DENIED = "permission_denied"
    SNAPSHOT_NOT_FOUND = "snapshot_not_found"
    REVISION_NOT_ACCEPTED = "revision_not_accepted"
    SEMANTIC_SNAPSHOT_MISMATCH = "semantic_snapshot_mismatch"
    SEMANTIC_SNAPSHOT_UNAVAILABLE = "semantic_snapshot_unavailable"
    RESOURCE_NOT_FOUND = "resource_not_found"
    RECORD_RECLAIMED = "record_reclaimed"
    CONTENT_UNAVAILABLE = "content_unavailable"
    CURSOR_INVALID = "cursor_invalid"
    CURSOR_SNAPSHOT_MISMATCH = "cursor_snapshot_mismatch"
    CANCELLED = "cancelled"
    DEADLINE_EXCEEDED = "deadline_exceeded"
    LIMIT_EXCEEDED = "limit_exceeded"
    WORKTREE_BUSY = "worktree_busy"
    ADAPTER_UNAVAILABLE = "adapter_unavailable"
    ADAPTER_PROTOCOL_ERROR = "adapter_protocol_error"
    ADAPTER_TIMEOUT = "adapter_timeout"
    STORAGE_FAILURE = "storage_failure"
    VALIDATOR_EXECUTION_FAILURE = "validator_execution_failure"
    INTERNAL_ERROR = "internal_error"


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "RetryDirective",
        (
            EnumValue("never", "RETRY_DIRECTIVE_NEVER", 1),
            EnumValue("same_request", "RETRY_DIRECTIVE_SAME_REQUEST", 2),
            EnumValue(
                "same_idempotency_key", "RETRY_DIRECTIVE_SAME_IDEMPOTENCY_KEY", 3
            ),
            EnumValue("refresh_snapshot", "RETRY_DIRECTIVE_REFRESH_SNAPSHOT", 4),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "never": "The request fails the same way every time. Change it or give up.",
            "same_request": (
                "Send the same bytes again. The cause was transient — a busy working tree, an "
                "adapter still starting."
            ),
            "same_idempotency_key": (
                "Retry under the operation's existing key. Rift returns the retained outcome when "
                "the first call completed before its response was lost."
            ),
            "refresh_snapshot": (
                "The state moved under the request. Re-read the current state, rebuild whatever "
                "was pinned to the old one, then ask again."
            ),
        }
    },
)
class RetryDirective(str, Enum):
    "Stable retry instruction for one failed request. `deadline_exceeded` can permit the same request; `invalid_request` requires changed input."

    NEVER = "never"
    SAME_REQUEST = "same_request"
    SAME_IDEMPOTENCY_KEY = "same_idempotency_key"
    REFRESH_SNAPSHOT = "refresh_snapshot"


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
            EnumValue("preview", "PREVIEW", 5),
            EnumValue("publish", "PUBLISH", 6),
            EnumValue("persist", "PERSIST", 7),
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
                "any checks the caller supplied."
            ),
            "preview": "Building a change into something readable without publishing it.",
            "publish": "Rerunning a retained preview and advancing the accepted ref through compare-and-swap.",
            "persist": "Materializing selected paths from an accepted commit into the connection worktree.",
        }
    },
)
class ErrorDataPhase(str, Enum):
    "How far the request got before it failed. The same code means different things at different phases: `limit_exceeded` while reading is a response too big, and while validating it is a change set too large."

    DISCOVERY = "discovery"
    READ = "read"
    RESOLVE = "resolve"
    VALIDATE = "validate"
    PREVIEW = "preview"
    PUBLISH = "publish"
    PERSIST = "persist"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ErrorData(ClosedModel):
    "The `data` object on every Rift MCP failure. `code` and `retry` are what a caller branches on, `message` is for a human, and `phase`, `diagnostics`, `limit` and `causes` are the evidence behind the code."

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
                }
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
                    "any checks the caller supplied."
                ),
                "preview": "Building a change into something readable without publishing it.",
                "publish": "Rerunning a retained preview and advancing the accepted ref through compare-and-swap.",
                "persist": "Materializing selected paths from an accepted commit into the connection worktree.",
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
            "What an adapter or a caller-supplied check reported while the request was "
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
            "driver": "A field of `Limits`, advertised by the repository resource.",
            "adapter": "A field of the adapter's `AdapterLimits`, advertised in `Describe`.",
        }
    },
)
class LimitEvidenceScope(str, Enum):
    "Which side of the server the limit belongs to. The two fail at different seams: a `driver` limit is host policy the caller can work within, an `adapter` limit is one adapter process running out of room."

    DRIVER = "driver"
    ADAPTER = "adapter"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class LimitEvidence(ClosedModel):
    "Which advertised limit a `limit_exceeded` failure hit, and by how much. It is present exactly when the code is `limit_exceeded`; without it, choosing between retrying smaller, falling back to another resource and giving up means reparsing the human message."

    scope: Field[LimitEvidenceScope] = proto_field(
        description=(
            "Which side of the server the limit belongs to. The two fail at different seams: "
            "a `driver` limit is host policy the caller can work within, an `adapter` limit "
            "is one adapter process running out of room."
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
            "The limit's field name in whichever message `scope` names, such as "
            "`max_page_items` or `max_in_flight_per_state`."
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
    """The repository resource. It takes no path, only an optional revision and cursor."""

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
    """The symbol resource, addressed by language and the name that language gives the declaration."""

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
    """The diff resource, addressed by two revisions in git's own range spelling."""

    uriTemplate: Field[Literal["rift://diff/{from}..{to}{?cursor}"]] = proto_field(
        description="The template, in RFC 6570 form. What follows `?` is optional."
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
    """The file resource, addressed by a path relative to the project root."""

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
class PreviewResourceTemplate(ClosedModel):
    """The retained preview resource, addressed by its opaque id and continued by an optional cursor."""

    uriTemplate: Field[Literal["rift://preview/{id}{?cursor}"]] = proto_field(
        description="The template, in RFC 6570 form. The cursor is optional."
    )
    name: Field[Literal["preview"]] = proto_field(
        description="The resource family, as `resources/templates/list` advertises it.",
        number=1,
    )
    mimeType: Field[Literal["application/vnd.rift.preview+json"]] = proto_field(
        description="What a read of a URI from this template returns: `PreviewResourcePayload` as JSON.",
        number=2,
        proto_name="mime_type",
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.union(
        Oneof("variant"),
        (
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
            Variant(
                "rift://diff/{from}..{to}{?cursor}", "diff", 3, DiffResourceTemplate
            ),
            Variant(
                "rift://file/{path}{?rev,start,length}", "file", 4, FileResourceTemplate
            ),
            Variant(
                "rift://preview/{id}{?cursor}", "preview", 5, PreviewResourceTemplate
            ),
        ),
    ),
    schema_extra={},
)
class ResourceTemplate(
    ProtocolRoot[
        (
            "Annotated[RepositoryResourceTemplate | SymbolResourceTemplate | "
            "DiffResourceTemplate | FileResourceTemplate | PreviewResourceTemplate, "
            "schema_field(discriminator='mimeType')]"
        )
    ]
):
    """One advertised MCP resource template. uriTemplate, name, and mimeType are correlated per family."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ResourceReadParams(ClosedModel):
    """The URI passed to MCP `resources/read`. Each branch is one advertised Rift resource family."""

    uri: Field[
        RepositoryResourceUri
        | core.SymbolId
        | core.DiffId
        | FileResourceUri
        | PreviewResourceUri
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
class PreviewResourceContent(ClosedModel):
    """What a read of a `rift://preview/…` URI returns."""

    uri: Field[PreviewResourceUri] = proto_field(
        description="The preview page that was read.", number=1
    )
    mimeType: Field[Literal["application/vnd.rift.preview+json"]] = proto_field(
        description="Which payload `text` holds."
    )
    text: Field[str] = proto_field(
        description="A `PreviewResourcePayload`, serialized as JSON.",
        number=2,
        json_schema_extra={
            "contentMediaType": "application/vnd.rift.preview+json",
            "rift:contentType": "PreviewResourcePayload",
        },
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.union(
        Oneof("variant"),
        (
            Variant(
                "application/vnd.rift.repository+json",
                "repository",
                1,
                RepositoryResourceContent,
            ),
            Variant(
                "application/vnd.rift.symbol+json", "symbol", 2, SymbolResourceContent
            ),
            Variant("application/vnd.rift.diff+json", "diff", 3, DiffResourceContent),
            Variant("application/vnd.rift.file+json", "file", 4, FileResourceContent),
            Variant(
                "application/vnd.rift.preview+json",
                "preview",
                5,
                PreviewResourceContent,
            ),
        ),
    ),
    schema_extra={},
)
class ResourceContent(
    ProtocolRoot[
        (
            "Annotated[RepositoryResourceContent | SymbolResourceContent | "
            "DiffResourceContent | FileResourceContent | PreviewResourceContent, "
            "schema_field(discriminator='mimeType')]"
        )
    ]
):
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
    """A node hit: one place in a syntax tree, without the symbol view around it."""

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


@definition(
    owner=MCP,
    public=True,
    proto=Proto.union(
        Oneof("variant"),
        (
            Variant("symbol", "symbol", 1, SearchHitTargetSymbol),
            Variant("node", "node", 2, SearchHitTargetNode),
            Variant("file", "file", 3, SearchHitTargetFile),
        ),
    ),
    schema_extra={},
)
class SearchHitTarget(
    ProtocolRoot[
        (
            "Annotated[SearchHitTargetSymbol | SearchHitTargetNode | SearchHitTargetFile, "
            "schema_field(discriminator='target')]"
        )
    ]
):
    """What a search hit is. Tagged, so the payload correlation survives code generation."""


@definition(
    owner=MCP,
    public=True,
    proto=Proto.union(
        Oneof("variant"),
        (
            Variant(None, "direct_change", 1, lambda: DirectChange),
            Variant(None, "patch_change", 2, lambda: PatchChange),
            Variant(None, "action_change", 3, lambda: ActionChange),
            Variant(None, "rewrite_change", 4, lambda: RewriteChange),
            Variant(None, "revert_change", 5, lambda: RevertChange),
            Variant(None, "merge_change", 6, lambda: MergeChange),
        ),
    ),
    schema_extra={},
)
class Change(
    ProtocolRoot[
        (
            "Annotated[DirectChange | PatchChange | ActionChange | RewriteChange | "
            "RevertChange | MergeChange, schema_field(discriminator='kind')]"
        )
    ]
):
    "One deterministic contribution to a candidate. Rift resolves the ordered array against one pinned state. After each change, it applies formatting and makes every adapter sharing the worktree acknowledge the new snapshot before resolving the next; any refusal discards the candidate. Product model components convert prompts and provider completions into these values before calling Rift."


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class DirectChange(ClosedModel):
    "Concrete filesystem edits supplied by the caller. Their ranges address the candidate state at this point in the ordered change list."

    kind: Field[Literal["edits"]] = proto_field(
        description="Tags this as a concrete edit list.", number=1
    )
    edits: Field[list[core.Edit]] = proto_field(
        description=(
            "An atomic effect set in canonical file-and-range order. Every text replacement "
            "addresses the state before this change, and replacements may not overlap."
        ),
        min_length=1,
        number=2,
    )
    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after these edits resolve.", number=3
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PatchChange(ClosedModel):
    "A UTF-8 unified diff guarded by its context lines. Rift refuses absolute paths, path traversal, binary patches, malformed headers, and any hunk whose context differs from the candidate state."

    kind: Field[Literal["patch"]] = proto_field(
        description="Tags this as a unified diff.", number=1
    )
    patch: Field[str] = proto_field(
        description="Unified diff in Git's text patch syntax, with project-relative `a/` and `b/` paths.",
        min_length=1,
        number=2,
    )
    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after every hunk resolves.", number=3
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ActionChange(ClosedModel):
    "One adapter action selected from `actions`. Rift validates `arguments` against the advertised schema and resolves the token. Refresh and publish rediscover at the descriptor's target. Kind, target, argument contract, argument schema, and guarantees select the same action. Zero or several matches cause a refusal."

    kind: Field[Literal["action"]] = proto_field(
        description="Tags this as an adapter action.", number=1
    )
    action: Field[ActionOffer] = proto_field(
        description="The descriptor used for replay and the key used for initial resolution.",
        number=2,
    )
    arguments: Field[dict[str, Any]] = proto_field(
        description=(
            "Arguments accepted by the offer's `ActionDescriptor.arguments_schema`. An action "
            "with no parameters receives an empty object."
        ),
        number=3,
    )
    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after the action's edits resolve.", number=4
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
        placement=Placement("range", 4),
    ),
    schema_extra={},
)
class RewriteChangeRange(str, Enum):
    "Which safe structural range is replaced. Text queries accept `exact` only because they have no grammar-owned trivia boundaries."

    EXACT = "exact"
    LEADING = "leading"
    TRAILING = "trailing"
    BOTH = "both"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RewriteChange(ClosedModel):
    "An atomic query-and-rewrite over the candidate state. Rift finds every match, checks the cardinality, expands the replacement, and either applies all resulting edits or refuses the candidate."

    kind: Field[Literal["rewrite"]] = proto_field(
        description="Tags this as an atomic match rewrite.", number=1
    )
    query: Field[core.MatchQuery] = proto_field(
        description="The text or structural pattern evaluated at this point in the change list.",
        number=2,
    )
    replacement: Field[str] = proto_field(
        description=(
            "UTF-8 replacement template. `${NAME}` inserts the source bound by a named or "
            "numeric capture, `${0}` inserts the whole match, and `$$` inserts one dollar "
            "sign. An absent capture refuses the rewrite."
        ),
        number=3,
    )
    range: Field[RewriteChangeRange] = proto_field(
        description=(
            "Which safe structural range is replaced. Text queries accept `exact` only "
            "because they have no grammar-owned trivia boundaries."
        ),
        number=4,
    )
    cardinality: Field[core.MatchCardinality] = proto_field(
        description="The accepted number of matches before expansion.", number=5
    )
    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after every replacement resolves.", number=6
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RevertChange(ClosedModel):
    "A validated three-way inverse of one commit. Rift computes the difference from `parent` to `revision`, applies its inverse to the candidate state, and refuses overlapping changes it cannot merge without guessing."

    kind: Field[Literal["revert"]] = proto_field(
        description="Tags this as a revision revert.", number=1
    )
    revision: Field[core.Commit] = proto_field(
        description="Exact commit whose changes are inverted.", number=2
    )
    parent: Field[core.Commit | None] = proto_field(
        description=(
            "Parent against which the commit's change is defined. Required for ordinary and "
            "merge commits; null selects the empty tree for a root commit. A commit that does "
            "not have this parent is refused."
        ),
        number=3,
    )
    paths: Field[core.PathSelector] = proto_field(
        description=(
            "Paths from the original commit eligible for inversion. Excluded paths remain "
            "untouched; the commit diff exposes them when the caller needs to inspect the "
            "omission."
        ),
        number=4,
    )
    formatting: Field[core.FormattingPolicy] = proto_field(
        description="Formatting applied after the inverse edits resolve.", number=5
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class MergeChange(ClosedModel):
    """A three-way merge of one exact commit into the candidate state."""

    kind: Field[Literal["merge"]] = proto_field(
        description="Tags this as a Git merge.", number=1
    )
    revision: Field[core.Commit] = proto_field(
        description="Commit merged into the candidate state.", number=2
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ResolvedChange(ClosedModel):
    "Bounded summary of one requested change after resolution. The preview resource pages its exact edits and evidence as records carrying this change index."

    index: Field[int] = proto_field(
        description="Zero-based position in the ordered request.",
        ge=0,
        le=4294967295,
        number=1,
    )
    owners: Field[list[core.Language]] = proto_field(
        description=(
            "Language adapters that contributed to resolution, sorted by name and dialect "
            "with null first. Empty for a change resolved entirely by Rift."
        ),
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
    edit_count: Field[int] = proto_field(
        description="Number of concrete Edit records retained for this change.",
        ge=0,
        le=4294967295,
        number=3,
    )
    precondition_count: Field[int] = proto_field(
        description="Number of satisfied preconditions retained for this change.",
        ge=0,
        le=4294967295,
        number=4,
    )
    effect_count: Field[int] = proto_field(
        description="Number of semantic effects retained for this change.",
        ge=0,
        le=4294967295,
        number=5,
    )
    guarantee_count: Field[int] = proto_field(
        description="Number of guarantee evidence records retained for this change.",
        ge=0,
        le=4294967295,
        number=6,
    )
    coverage: Field[core.Coverage] = proto_field(
        description="How completely Rift and its adapters resolved the request. Publication requires complete coverage.",
        number=7,
    )
    diagnostic_count: Field[int] = proto_field(
        description="Number of resolution diagnostics retained for this change.",
        ge=0,
        le=4294967295,
        number=8,
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
    """How the caller presents this check."""

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
    """Whether Rift appends the candidate's changed `ProjectPath` values to `argv` in byte order."""

    NONE = "none"
    APPEND = "append"


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class CommandValidatorGuarantees(ClosedModel):
    changes: Field[list[int]] = proto_field(
        description="Requested change indexes to which this evidence applies.",
        min_length=1,
        number=1,
        json_schema_extra={"uniqueItems": True},
    )
    kind: Field[core.GuaranteeKind] = proto_field(
        description="Guarantee established when the validator passes.", number=2
    )
    scope: Field[core.CoverageScope] = proto_field(
        description="Source over which the command checks the property.", number=3
    )
    detail: Field[str] = proto_field(
        description="Exact property the command checks and limits on interpreting a pass.",
        min_length=1,
        max_length=4096,
        number=4,
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
    """Whether an identical candidate and environment are expected to produce the same result."""

    DETERMINISTIC = "deterministic"
    BEST_EFFORT = "best_effort"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class CommandValidator(ClosedModel):
    "A caller-authorized acceptance check executed directly, without a shell, in a disposable validation workspace materialized from the complete candidate tree. The workspace is only the process working directory; it does not isolate the command from the host. Rift removes it when the check ends."

    id: Field[str] = proto_field(
        description=(
            "Caller label shown with this validator. The result links to the complete "
            "declaration by digest, so labels may repeat."
        ),
        pattern="^[A-Za-z][A-Za-z0-9_.-]{0,127}$",
        number=1,
    )
    kind: Field[CommandValidatorKind] = proto_field(
        description="How the caller presents this check.", number=2
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
        description="Whether Rift appends the candidate's changed `ProjectPath` values to `argv` in byte order.",
        number=4,
    )
    working_directory: Field[core.ProjectPath] = proto_field(
        description="Directory below the project root in which the process starts. The empty path selects the root.",
        number=5,
    )
    environment: Field[dict[str, str]] = proto_field(
        description=(
            "Caller-supplied environment additions. Rift supplies a policy-controlled PATH, "
            "private HOME and temporary directories, and a UTF-8 locale; it removes host "
            "secrets and every other inherited variable."
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
            "Captured prefix limit for each output stream. `ValidatorOutput.total_bytes` "
            "reports the omitted size. The upper bound keeps one escaped validator result "
            "inside a 65536-byte preview page."
        ),
        ge=256,
        le=4096,
        number=8,
    )
    guarantees: Field[list[CommandValidatorGuarantees]] = proto_field(
        description=(
            "Behavior or other properties this command is intended to check. A passing result "
            "turns each declaration into `GuaranteeEvidence`; a failed result rejects "
            "publication."
        ),
        number=9,
    )
    determinism: Field[CommandValidatorDeterminism] = proto_field(
        description="Whether an identical candidate and environment are expected to produce the same result.",
        number=10,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ValidatorOutput(ClosedModel):
    "A bounded UTF-8 rendering of one process stream. Invalid byte sequences become U+FFFD. The digest covers the complete raw stream, including bytes beyond the captured prefix."

    text: Field[str] = proto_field(description="Decoded captured prefix.", number=1)
    captured_bytes: Field[int] = proto_field(
        description="Raw bytes represented by `text` before replacement decoding.",
        ge=0,
        number=2,
    )
    total_bytes: Field[int] = proto_field(
        description="Raw bytes emitted by the complete stream.", ge=0, number=3
    )
    truncated: Field[bool] = proto_field(
        description="Whether bytes after the captured prefix were omitted from `text`.",
        number=4,
    )
    digest: Field[core.Digest] = proto_field(
        description=(
            "SHA-256 of the complete raw stream. This retains the identity of omitted bytes "
            "when `truncated` is true."
        ),
        number=5,
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
    stdout: Field[ValidatorOutput] = proto_field(
        description="Bounded standard output from the process.", number=5
    )
    stderr: Field[ValidatorOutput] = proto_field(
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
    stdout: Field[ValidatorOutput] = proto_field(
        description="Bounded standard output from the process.", number=5
    )
    stderr: Field[ValidatorOutput] = proto_field(
        description="Bounded standard error from the process.", number=6
    )

    @field_validator("exit_code")
    @classmethod
    def nonzero_exit_code(cls, value: int) -> int:
        if value == 0:
            raise ValueError("failed validator result requires a nonzero exit code")
        return value


@definition(
    owner=MCP,
    public=True,
    proto=Proto.union(
        Oneof("variant"),
        (
            Variant("passed", "passed", 1, ValidatorResultPassed),
            Variant("failed", "failed", 2, ValidatorResultFailed),
        ),
    ),
    schema_extra={},
)
class ValidatorResult(
    ProtocolRoot[
        "Annotated[ValidatorResultPassed | ValidatorResultFailed, schema_field(discriminator='status')]"
    ]
):
    "The completed outcome of one declared validator. Exit status zero passes. Every other exit status fails. A workspace, launch, timeout, or capture failure raises `validator_execution_failure` before Rift produces candidate evidence."


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class CandidateValidation(ClosedModel):
    "Bounded verdict over a candidate's adapter reports and command validators. The preview resource carries the complete paginated evidence."

    complete: Field[bool] = proto_field(
        description=(
            "Whether every adapter required by workspace validation policy returned complete "
            "coverage and every declared validator produced a result."
        ),
        number=1,
    )
    valid: Field[bool] = proto_field(
        description=(
            "True when `complete` is true, every adapter report is valid, and every validator "
            "result passed. Publication requires true."
        ),
        number=2,
    )
    adapter_reports: Field[int] = proto_field(
        description="Number of adapter reports retained by the preview.",
        ge=0,
        le=4294967295,
        number=3,
    )
    validator_results: Field[int] = proto_field(
        description="Number of command-validator results retained by the preview.",
        ge=0,
        le=4294967295,
        number=4,
    )
    validators_passed: Field[int] = proto_field(
        description="Number of retained validator results with `passed` status.",
        ge=0,
        le=4294967295,
        number=5,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class CandidateSummary(ClosedModel):
    "Bounded identity and validation evidence for a retained apply or integration candidate. The linked preview resource carries the complete plan."

    preview: Field[core.PreviewId] = proto_field(
        description="The retained plan's stable identity.", number=1
    )
    base: Field[core.Snapshot] = proto_field(
        description="The state against which the first change was resolved.", number=2
    )
    expected_head: Field[core.Commit] = proto_field(
        description=(
            "Ref head that publication will compare-and-swap. Apply reports the accepted "
            "head; integration reports the target head."
        ),
        number=3,
    )
    candidate: Field[core.Commit] = proto_field(
        description=(
            "Immutable Git commit containing the resolved change. It remains outside the "
            "destination ref until publication succeeds."
        ),
        number=4,
    )
    resource: Field[PreviewResourceLink] = proto_field(
        description="Link to the complete retained plan.", number=5
    )
    validation: Field[CandidateValidation] = proto_field(
        description="Bounded verdict and evidence counts for this candidate.", number=6
    )
    confirmation_count: Field[int] = proto_field(
        description="Number of acknowledgement requirements retained in the preview resource.",
        ge=0,
        le=4294967295,
        number=7,
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class PreviewResourcePayloadEdits(ClosedModel):
    change: Field[int] = proto_field(
        description="Requested change index that produced this edit.",
        ge=0,
        le=4294967295,
        number=1,
    )
    edit: Field[core.Edit] = proto_field(
        description="Concrete filesystem effect.", number=2
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class PreviewResourcePayloadPreconditions(ClosedModel):
    change: Field[int] = proto_field(
        description="Requested change index checked by this condition.",
        ge=0,
        le=4294967295,
        number=1,
    )
    precondition: Field[core.OperationPrecondition] = proto_field(number=2)


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class PreviewResourcePayloadEffects(ClosedModel):
    change: Field[int] = proto_field(
        description="Requested change index that produced this effect.",
        ge=0,
        le=4294967295,
        number=1,
    )
    effect: Field[core.OperationEffect] = proto_field(number=2)


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class PreviewResourcePayloadGuarantees(ClosedModel):
    change: Field[int] = proto_field(
        description="Requested change index supported by this evidence.",
        ge=0,
        le=4294967295,
        number=1,
    )
    evidence: Field[core.GuaranteeEvidence] = proto_field(number=2)


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class PreviewResourcePayloadResolutionDiagnostics(ClosedModel):
    change: Field[int] = proto_field(
        description="Requested change index that produced this finding.",
        ge=0,
        le=4294967295,
        number=1,
    )
    diagnostic: Field[core.Diagnostic] = proto_field(number=2)


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PreviewResourcePayload(ClosedModel):
    "One page of a retained candidate contract. Concatenating every array from successive pages reconstructs the complete plan and validation evidence. Every page repeats the URI, base, candidate, and bounded validation verdict."

    uri: Field[PreviewResourceUri] = proto_field(
        description="The preview resource URI for this page.", number=1
    )
    base: Field[core.Snapshot] = proto_field(
        description="The state from which resolution began.", number=2
    )
    candidate: Field[core.Commit] = proto_field(
        description="The immutable candidate commit produced by the complete plan.",
        number=3,
    )
    validators: Field[list[CommandValidator]] = proto_field(
        description="Caller-supplied checks on this page, preserving declaration order.",
        number=4,
    )
    requested: Field[list[Change]] = proto_field(
        description="Requested changes on this page, preserving their transaction order.",
        number=5,
    )
    resolved: Field[list[ResolvedChange]] = proto_field(
        description="Bounded resolution summaries on this page, ordered by request index.",
        number=6,
    )
    edits: Field[list[PreviewResourcePayloadEdits]] = proto_field(
        description="Concrete edits on this page, ordered by change index and canonical edit order.",
        number=7,
    )
    preconditions: Field[list[PreviewResourcePayloadPreconditions]] = proto_field(
        description="Satisfied preconditions on this page, ordered by change index and check order.",
        number=8,
    )
    effects: Field[list[PreviewResourcePayloadEffects]] = proto_field(
        description="Semantic effects on this page, ordered by change index and adapter emission order.",
        number=9,
    )
    guarantees: Field[list[PreviewResourcePayloadGuarantees]] = proto_field(
        description="Guarantee evidence on this page, ordered by change index and guarantee kind.",
        number=10,
    )
    resolution_diagnostics: Field[list[PreviewResourcePayloadResolutionDiagnostics]] = (
        proto_field(
            description="Resolution findings on this page, ordered by change index and source location.",
            number=11,
        )
    )
    files: Field[list[core.FileChange]] = proto_field(
        description=(
            "File-level diff entries on this page, ordered by the path present after the "
            "change and then by the path present before it."
        ),
        number=12,
    )
    validation: Field[CandidateValidation] = proto_field(
        description="Bounded verdict for the complete candidate.", number=13
    )
    adapter_reports: Field[list[core.ValidationReport]] = proto_field(
        description=(
            "Adapter reports on this page, sorted by language name and dialect with null "
            "first and unique across the complete resource."
        ),
        number=14,
    )
    validator_results: Field[list[ValidatorResult]] = proto_field(
        description=(
            "Command-validator results on this page, preserving declaration order and "
            "appearing once across the complete resource."
        ),
        number=15,
    )
    confirmations: Field[list[core.ConfirmationRequirement]] = proto_field(
        description="Acknowledgements on this page, sorted by id across the complete resource.",
        number=16,
        json_schema_extra={"uniqueItems": True},
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        description="Rift findings about the complete plan, including path, ownership, and formatting decisions.",
        number=17,
    )
    next: Field[PreviewResourceUri | None] = proto_field(
        description="The URI for the next plan page, or null after the final page.",
        number=18,
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.union(
        Oneof("variant"),
        (
            Variant(None, "preview_apply_params", 1, lambda: PreviewApplyParams),
            Variant(None, "refresh_apply_params", 2, lambda: RefreshApplyParams),
            Variant(None, "publish_apply_params", 3, lambda: PublishApplyParams),
        ),
    ),
    schema_extra={},
)
class ApplyParams(
    ProtocolRoot[
        "Annotated[PreviewApplyParams | RefreshApplyParams | PublishApplyParams, schema_field(discriminator='mode')]"
    ]
):
    "One phase of the candidate lifecycle. Preview creates a retained plan, refresh re-resolves it on a selected base, and publish attempts to advance the accepted ref with that contract."


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PreviewApplyParams(ClosedModel):
    """Builds and validates an immutable candidate while leaving the accepted ref and connection worktree unchanged."""

    mode: Field[Literal["preview"]] = proto_field(
        description="Selects candidate creation.", number=1
    )
    rev: Field[core.Revision | None] = proto_field(
        default=None,
        description=(
            "State against which resolution begins. Omission selects the connection's current "
            "accepted revision."
        ),
        number=2,
    )
    expected_accepted: Field[core.Commit | None] = proto_field(
        default=None,
        description=(
            "Accepted-ref head expected at publication. Omission selects the current accepted "
            "head. The selected commit must be an ancestor of `rev`."
        ),
        number=3,
    )
    changes: Field[list[Change]] = proto_field(
        description="Ordered deterministic changes. Each observes the result of its predecessors.",
        min_length=1,
        number=4,
    )
    validators: Field[list[CommandValidator]] = proto_field(
        description=(
            "Caller-supplied acceptance checks. The `edit` profile accepts an empty array. "
            "The `full` profile may accept up to `Limits.max_validators` declarations."
        ),
        number=5,
        json_schema_extra={"uniqueItems": True},
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RefreshApplyParams(ClosedModel):
    "Re-resolves a retained preview's exact changes and validators on a selected base. It creates a new preview and leaves the accepted ref unchanged."

    mode: Field[Literal["refresh"]] = proto_field(
        description="Selects preview refresh.", number=1
    )
    previous: Field[core.PreviewId] = proto_field(
        description="The retained contract to run again.", number=2
    )
    rev: Field[core.Revision | None] = proto_field(
        default=None,
        description="New base. Omission selects the current accepted revision.",
        number=3,
    )
    expected_accepted: Field[core.Commit | None] = proto_field(
        default=None,
        description=(
            "Accepted-ref head expected at publication. Omission selects the current accepted "
            "head. The selected commit must be an ancestor of `rev`."
        ),
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PublishApplyParams(ClosedModel):
    "Attempts to advance the accepted ref with a retained preview. Rift replays resolution and validation. Every deterministic preview record and the candidate tree must match. It then compares the accepted ref with the retained base and advances it to the candidate. Fresh validator verdicts govern publication; captured output may differ."

    mode: Field[Literal["publish"]] = proto_field(
        description="Selects publication.", number=1
    )
    preview: Field[core.PreviewId] = proto_field(
        description="The retained contract to publish.", number=2
    )
    idempotency_key: Field[str] = proto_field(
        description=(
            "Caller-chosen retry key. Rift retains the key with the exact publish request and "
            "result, returning that result on an exact retry and refusing reuse with "
            "different input."
        ),
        pattern="^[\\x21-\\x7E]{1,256}$",
        number=3,
    )
    confirmations: Field[list[int]] = proto_field(
        description=(
            "Every currently required confirmation id, sorted bytewise. Missing or extra ids "
            "refuse publication."
        ),
        number=4,
        json_schema_extra={"uniqueItems": True},
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.union(
        Oneof("variant"),
        (
            Variant(None, "preview_apply_result", 1, lambda: PreviewApplyResult),
            Variant(None, "refresh_apply_result", 2, lambda: RefreshApplyResult),
            Variant(None, "accepted_apply_result", 3, lambda: AcceptedApplyResult),
            Variant(None, "rejected_apply_result", 4, lambda: RejectedApplyResult),
            Variant(None, "refused_apply_result", 5, lambda: RefusedApplyResult),
            Variant(None, "conflict_apply_result", 6, lambda: ConflictApplyResult),
        ),
    ),
    schema_extra={},
)
class ApplyResult(
    ProtocolRoot[
        (
            "Annotated[PreviewApplyResult | RefreshApplyResult | AcceptedApplyResult | "
            "RejectedApplyResult | RefusedApplyResult | ConflictApplyResult, "
            "schema_field(discriminator='status')]"
        )
    ]
):
    "A completed candidate lifecycle decision. Malformed requests, unavailable infrastructure, storage faults, and validator execution failures use `ErrorData`."


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PreviewApplyResult(ClosedModel):
    """A retained preview created without advancing the accepted ref or materializing its files."""

    status: Field[Literal["preview"]] = proto_field(
        description="Identifies preview creation.", number=1
    )
    summary: Field[CandidateSummary] = proto_field(
        description="Candidate identity and acceptance evidence.", number=2
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RefreshApplyResult(ClosedModel):
    """A new retained preview produced from an earlier contract and a selected base."""

    status: Field[Literal["refresh"]] = proto_field(
        description="Identifies preview refresh.", number=1
    )
    previous: Field[core.PreviewId] = proto_field(
        description="The preview whose contract was rerun.", number=2
    )
    summary: Field[CandidateSummary] = proto_field(
        description="Identity and evidence for the refreshed candidate.", number=3
    )
    changed_request_count: Field[int] = proto_field(
        description=(
            "Number of request indexes whose resolved owner, edits, preconditions, effects, "
            "guarantees, coverage, or diagnostics differ from the previous preview. Comparing "
            "the two preview resources yields the exact indexes."
        ),
        ge=0,
        le=4294967295,
        number=4,
    )
    changed_file_count: Field[int] = proto_field(
        description=(
            "Number of candidate paths that differ from the previous preview. Comparing the "
            "paginated file records yields the exact paths."
        ),
        ge=0,
        le=4294967295,
        number=5,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class AcceptedApplyResult(ClosedModel):
    "Publication advanced the accepted ref to the retained candidate commit. `accepted` therefore equals `summary.candidate`. The connection worktree remains unchanged until `persist` materializes it."

    status: Field[Literal["accepted"]] = proto_field(
        description="Identifies successful publication.", number=1
    )
    summary: Field[CandidateSummary] = proto_field(
        description="The published candidate and its acceptance evidence.", number=2
    )
    accepted: Field[core.Commit] = proto_field(
        description="The commit now held by the accepted ref.", number=3
    )
    replayed: Field[bool] = proto_field(
        description="Whether Rift returned a previously stored result for this exact idempotency key and request.",
        number=4,
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RejectedApplyResult(ClosedModel):
    "Publication reran the retained contract and validation did not pass. The candidate remains readable as a preview and the accepted ref is unchanged."

    status: Field[Literal["rejected"]] = proto_field(
        description="Identifies validation rejection.", number=1
    )
    summary: Field[CandidateSummary] = proto_field(
        description="The rejected candidate and the evidence that prevented acceptance.",
        number=2,
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Reason",
        (
            EnumValue("unsupported", "UNSUPPORTED", 1),
            EnumValue("unmet_precondition", "UNMET_PRECONDITION", 2),
            EnumValue("ambiguous_target", "AMBIGUOUS_TARGET", 3),
            EnumValue("stale_preview", "STALE_PREVIEW", 4),
            EnumValue("stale_action", "STALE_ACTION", 5),
            EnumValue("stale_match", "STALE_MATCH", 6),
            EnumValue("cardinality_mismatch", "CARDINALITY_MISMATCH", 7),
            EnumValue("confirmation_required", "CONFIRMATION_REQUIRED", 8),
            EnumValue("unsafe_effect", "UNSAFE_EFFECT", 9),
            EnumValue("formatter_unsupported", "FORMATTER_UNSUPPORTED", 10),
            EnumValue("validation_incomplete", "VALIDATION_INCOMPLETE", 11),
        ),
        placement=Placement("reason", 4),
    ),
    schema_extra={},
)
class RefusedApplyResultReason(str, Enum):
    """Condition the caller can act on."""

    UNSUPPORTED = "unsupported"
    UNMET_PRECONDITION = "unmet_precondition"
    AMBIGUOUS_TARGET = "ambiguous_target"
    STALE_PREVIEW = "stale_preview"
    STALE_ACTION = "stale_action"
    STALE_MATCH = "stale_match"
    CARDINALITY_MISMATCH = "cardinality_mismatch"
    CONFIRMATION_REQUIRED = "confirmation_required"
    UNSAFE_EFFECT = "unsafe_effect"
    FORMATTER_UNSUPPORTED = "formatter_unsupported"
    VALIDATION_INCOMPLETE = "validation_incomplete"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RefusedApplyResult(ClosedModel):
    "Resolution or publication stopped before a valid candidate outcome existed. No accepted ref or connection worktree changes."

    status: Field[Literal["refused"]] = proto_field(
        description="Identifies a domain refusal.", number=1
    )
    base: Field[core.Snapshot] = proto_field(
        description="State against which the refused work was attempted.", number=2
    )
    change: Field[int | None] = proto_field(
        description="Zero-based requested change that caused the refusal, or null for a publication-wide condition.",
        number=3,
    )
    reason: Field[RefusedApplyResultReason] = proto_field(
        description="Condition the caller can act on.", number=4
    )
    preconditions: Field[list[core.OperationPrecondition]] = proto_field(
        description="Conditions checked before refusal, including at least one failed entry for `unmet_precondition`.",
        number=5,
    )
    blockers: Field[list[core.OperationBlocker]] = proto_field(
        description="Existing code, paths, or relationships that prevented a deterministic resolution.",
        number=6,
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        description="Evidence that explains the refusal.", number=7
    )
    suggestions: Field[list[Change]] = proto_field(
        description=(
            "Deterministic replacement changes that satisfy the condition Rift could "
            "identify. Empty when no safe repair is known."
        ),
        number=8,
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Reason",
        (
            EnumValue("stale_base", "STALE_BASE", 1),
            EnumValue("idempotency_key_reused", "IDEMPOTENCY_KEY_REUSED", 2),
        ),
        placement=Placement("reason", 2),
    ),
    schema_extra={},
)
class ConflictApplyResultReason(str, Enum):
    """Which identity comparison failed."""

    STALE_BASE = "stale_base"
    IDEMPOTENCY_KEY_REUSED = "idempotency_key_reused"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ConflictApplyResult(ClosedModel):
    "Publication reached compare-and-swap after another accepted change had moved the ref, or reused an idempotency key for different input. The accepted ref is unchanged by this request."

    status: Field[Literal["conflict"]] = proto_field(
        description="Identifies an optimistic concurrency conflict.", number=1
    )
    reason: Field[ConflictApplyResultReason] = proto_field(
        description="Which identity comparison failed.", number=2
    )
    base: Field[core.Snapshot] = proto_field(
        description="Accepted snapshot the retained preview expected.", number=3
    )
    current: Field[core.Snapshot] = proto_field(
        description="Accepted snapshot observed at publication time.", number=4
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.scalar(ProtoFieldDescriptor.TYPE_STRING),
    schema_extra={},
)
class IntegrationTarget(
    ProtocolRoot[
        (
            "Annotated[str, schema_field(description='A local branch ref that integration may "
            "compare-and-swap.', pattern='^refs/heads/(?!/)(?!.*//)(?!.*\\\\.\\\\.)(?!.*[~^:?*\\\\"
            "\\\\])[A-Za-z0-9._/-]+$', examples=['refs/heads/main', 'refs/heads/release/1.x'])]"
        )
    ]
):
    """A local branch ref that integration may compare-and-swap."""


@definition(
    owner=MCP,
    public=True,
    proto=Proto.union(
        Oneof("variant"),
        (
            Variant(
                None, "preview_integrate_params", 1, lambda: PreviewIntegrateParams
            ),
            Variant(
                None, "refresh_integrate_params", 2, lambda: RefreshIntegrateParams
            ),
            Variant(
                None, "publish_integrate_params", 3, lambda: PublishIntegrateParams
            ),
        ),
    ),
    schema_extra={},
)
class IntegrateParams(
    ProtocolRoot[
        (
            "Annotated[PreviewIntegrateParams | RefreshIntegrateParams | "
            "PublishIntegrateParams, schema_field(discriminator='mode')]"
        )
    ]
):
    """One retained preview, refresh, or publish phase for integration in the current repository."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PreviewIntegrateParams(ClosedModel):
    """Builds and validates a merge candidate while leaving the target ref unchanged."""

    mode: Field[Literal["preview"]] = proto_field(
        description="Selects integration preview.", number=1
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Branch that receives the accepted commit.", number=2
    )
    source: Field[core.Commit | None] = proto_field(
        default=None,
        description="Accepted commit to integrate. Omission selects the connection's current accepted ref.",
        number=3,
    )
    validators: Field[list[CommandValidator]] = proto_field(
        description="Caller-supplied checks run on the merge candidate.",
        number=4,
        json_schema_extra={"uniqueItems": True},
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RefreshIntegrateParams(ClosedModel):
    """Rebuilds an integration preview against the target's current head."""

    mode: Field[Literal["refresh"]] = proto_field(
        description="Selects integration refresh.", number=1
    )
    previous: Field[core.PreviewId] = proto_field(
        description="Retained integration contract to run again.", number=2
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PublishIntegrateParams(ClosedModel):
    """Replays a retained integration preview and attempts to advance its target ref."""

    mode: Field[Literal["publish"]] = proto_field(
        description="Selects integration publication.", number=1
    )
    preview: Field[core.PreviewId] = proto_field(
        description="Retained integration contract to publish.", number=2
    )
    idempotency_key: Field[str] = proto_field(
        description="Caller-chosen retry key bound to this exact publish request.",
        pattern="^[\\x21-\\x7E]{1,256}$",
        number=3,
    )
    confirmations: Field[list[int]] = proto_field(
        description="Every confirmation required by the retained integration preview.",
        number=4,
        json_schema_extra={"uniqueItems": True},
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class MergeConflict(ClosedModel):
    """Three Git entries that could not be merged automatically at one path."""

    path: Field[core.ProjectPath] = proto_field(
        description="Conflicting project path.", number=1
    )
    base: Field[core.File | None] = proto_field(
        description="Entry at the merge base, or null when absent.", number=2
    )
    source: Field[core.File | None] = proto_field(
        description="Entry in the accepted source commit, or null when absent.",
        number=3,
    )
    target: Field[core.File | None] = proto_field(
        description="Entry at the target head, or null when absent.", number=4
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PreparedIntegrateResult(ClosedModel):
    """A retained, conflict-free integration preview."""

    status: Field[Literal["integration_preview"]] = proto_field(
        description="Identifies a conflict-free integration preview.", number=1
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch the candidate would advance.", number=2
    )
    source: Field[core.Commit] = proto_field(
        description="Accepted commit merged into the target.", number=3
    )
    summary: Field[CandidateSummary] = proto_field(
        description="Retained merge candidate and validation evidence.", number=4
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RefreshedIntegrateResult(ClosedModel):
    """A new integration preview built from an earlier contract and the current target head."""

    status: Field[Literal["integration_refresh"]] = proto_field(
        description="Identifies a refreshed integration preview.", number=1
    )
    previous: Field[core.PreviewId] = proto_field(
        description="Integration preview whose contract was rerun.", number=2
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch the candidate would advance.", number=3
    )
    source: Field[core.Commit] = proto_field(
        description="Accepted commit merged into the target.", number=4
    )
    summary: Field[CandidateSummary] = proto_field(
        description="Refreshed merge candidate and validation evidence.", number=5
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class MergeConflictIntegrateResult(ClosedModel):
    """A retained provisional merge whose conflicting paths still use the target entries."""

    status: Field[Literal["merge_conflict"]] = proto_field(
        description="Identifies a retained provisional merge with conflicts.", number=1
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch used as the merge target.", number=2
    )
    target_head: Field[core.Commit] = proto_field(
        description="Target head used as the provisional merge parent.", number=3
    )
    source: Field[core.Commit] = proto_field(
        description="Accepted source used as the other provisional merge parent.",
        number=4,
    )
    candidate: Field[core.Commit] = proto_field(
        description=(
            "Two-parent provisional merge commit. Non-conflicting paths are merged; "
            "conflicting paths retain the target entries so the tree stays parseable."
        ),
        number=5,
    )
    preview: Field[core.PreviewId] = proto_field(
        description="Retained conflict record.", number=6
    )
    resource: Field[PreviewResourceLink] = proto_field(
        description="Link to the retained merge and conflict evidence.", number=7
    )
    conflicts: Field[list[MergeConflict]] = proto_field(
        description="Conflicts in project-path order.", min_length=1, number=8
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class IntegratedResult(ClosedModel):
    """Integration advanced the target ref to the retained candidate."""

    status: Field[Literal["integrated"]] = proto_field(
        description="Identifies successful integration publication.", number=1
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch advanced by publication.", number=2
    )
    previous: Field[core.Commit] = proto_field(
        description="Target head replaced by publication.", number=3
    )
    integrated: Field[core.Commit] = proto_field(
        description="Commit now held by the target ref.", number=4
    )
    summary: Field[CandidateSummary] = proto_field(
        description="Published merge candidate and fresh validation evidence.", number=5
    )
    replayed: Field[bool] = proto_field(
        description="Whether this is a retained idempotent result.", number=6
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RejectedIntegrateResult(ClosedModel):
    """Fresh validation rejected a replayed integration candidate."""

    status: Field[Literal["integration_rejected"]] = proto_field(
        description="Identifies fresh validation rejection.", number=1
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch left unchanged.", number=2
    )
    summary: Field[CandidateSummary] = proto_field(
        description="Rejected merge candidate and fresh validation evidence.", number=3
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Reason",
        (
            EnumValue("checked_out_target", "CHECKED_OUT_TARGET", 1),
            EnumValue("dirty_target", "DIRTY_TARGET", 2),
            EnumValue("stale_preview", "STALE_PREVIEW", 3),
            EnumValue("confirmation_required", "CONFIRMATION_REQUIRED", 4),
            EnumValue("validation_incomplete", "VALIDATION_INCOMPLETE", 5),
            EnumValue("unsupported", "UNSUPPORTED", 6),
        ),
        placement=Placement("reason", 3),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "checked_out_target": "Target branch is checked out and automatic integration is disabled.",
            "dirty_target": "Checked-out target worktree has local changes.",
            "stale_preview": "Retained preview no longer matches its target head or source.",
            "confirmation_required": "Caller omitted a confirmation retained by the preview.",
            "validation_incomplete": "Required adapter or command validation did not complete.",
            "unsupported": "Repository state cannot satisfy this integration contract.",
        }
    },
)
class RefusedIntegrateResultReason(str, Enum):
    """Repository or policy condition that prevented integration publication."""

    CHECKED_OUT_TARGET = "checked_out_target"
    DIRTY_TARGET = "dirty_target"
    STALE_PREVIEW = "stale_preview"
    CONFIRMATION_REQUIRED = "confirmation_required"
    VALIDATION_INCOMPLETE = "validation_incomplete"
    UNSUPPORTED = "unsupported"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class RefusedIntegrateResult(ClosedModel):
    """Integration stopped on a repository or policy condition the caller can change."""

    status: Field[Literal["integration_refused"]] = proto_field(
        description="Identifies an actionable integration refusal.", number=1
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch left unchanged.", number=2
    )
    reason: Field[RefusedIntegrateResultReason] = proto_field(
        description="Repository or policy condition that stopped publication.", number=3
    )
    candidate: Field[core.Commit | None] = proto_field(
        default=None,
        description="Validated merge commit available for manual handling.",
        number=4,
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        description="Findings associated with the refusal.", number=5
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Reason",
        (
            EnumValue("target_moved", "TARGET_MOVED", 1),
            EnumValue("idempotency_key_reused", "IDEMPOTENCY_KEY_REUSED", 2),
        ),
        placement=Placement("reason", 3),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "target_moved": "Target head changed after the retained preview was built.",
            "idempotency_key_reused": "The same idempotency key was already bound to different input.",
        }
    },
)
class ConflictIntegrateResultReason(str, Enum):
    """Concurrency or idempotency conflict during integration publication."""

    TARGET_MOVED = "target_moved"
    IDEMPOTENCY_KEY_REUSED = "idempotency_key_reused"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class ConflictIntegrateResult(ClosedModel):
    """Integration lost compare-and-swap or reused an idempotency key for different input."""

    status: Field[Literal["integration_conflict"]] = proto_field(
        description="Identifies an integration publication conflict.", number=1
    )
    target: Field[IntegrationTarget] = proto_field(
        description="Local branch left unchanged.", number=2
    )
    reason: Field[ConflictIntegrateResultReason] = proto_field(
        description="Concurrency or idempotency condition that conflicted.", number=3
    )
    expected: Field[core.Commit] = proto_field(
        description="Target head retained by the preview.", number=4
    )
    current: Field[core.Commit] = proto_field(
        description="Target head observed at publication.", number=5
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.union(
        Oneof("variant"),
        (
            Variant(None, "prepared_integrate_result", 1, PreparedIntegrateResult),
            Variant(None, "refreshed_integrate_result", 2, RefreshedIntegrateResult),
            Variant(
                None, "merge_conflict_integrate_result", 3, MergeConflictIntegrateResult
            ),
            Variant(None, "integrated_result", 4, IntegratedResult),
            Variant(None, "rejected_integrate_result", 5, RejectedIntegrateResult),
            Variant(None, "refused_integrate_result", 6, RefusedIntegrateResult),
            Variant(None, "conflict_integrate_result", 7, ConflictIntegrateResult),
        ),
    ),
    schema_extra={},
)
class IntegrateResult(
    ProtocolRoot[
        (
            "Annotated[PreparedIntegrateResult | RefreshedIntegrateResult | "
            "MergeConflictIntegrateResult | IntegratedResult | RejectedIntegrateResult | "
            "RefusedIntegrateResult | ConflictIntegrateResult, "
            "schema_field(discriminator='status')]"
        )
    ]
):
    """A completed integration preview, publication, conflict, rejection, or refusal."""


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PersistParams(ClosedModel):
    "Selects files from one accepted commit for materialization into the connection worktree. Omitted `paths` selects every changed path."

    revision: Field[core.Commit] = proto_field(
        description="Accepted commit whose tree supplies the desired entries.", number=1
    )
    paths: Field[list[core.ProjectPath] | None] = proto_field(
        default=None,
        description="Changed project paths to materialize. Omission selects all paths changed by the accepted commit.",
        number=2,
        json_schema_extra={"uniqueItems": True},
    )
    include_deletions: Field[bool] = proto_field(
        default=False,
        description="Whether entries absent from the accepted tree may be removed from the worktree.",
        number=3,
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Outcome",
        (
            EnumValue("written", "WRITTEN", 1),
            EnumValue("unchanged", "UNCHANGED", 2),
            EnumValue("skipped_drift", "SKIPPED_DRIFT", 3),
            EnumValue("skipped_deletion", "SKIPPED_DELETION", 4),
            EnumValue("skipped_unsupported_kind", "SKIPPED_UNSUPPORTED_KIND", 5),
            EnumValue("skipped_external_content", "SKIPPED_EXTERNAL_CONTENT", 6),
            EnumValue("skipped_sparse", "SKIPPED_SPARSE", 7),
            EnumValue("skipped_nested_repository", "SKIPPED_NESTED_REPOSITORY", 8),
            EnumValue("not_found", "NOT_FOUND", 9),
        ),
        placement=Placement("outcome", 2),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "written": "The worktree entry matched the candidate base and now matches the accepted commit.",
            "unchanged": "The worktree entry already matched the accepted commit.",
            "skipped_drift": (
                "The worktree entry differed from the candidate base and accepted commit, so "
                "materialization preserved the local change."
            ),
            "skipped_deletion": "The accepted commit removes the entry and `include_deletions` is false.",
            "skipped_unsupported_kind": "The selected Git entry kind has no safe worktree materialization rule.",
            "skipped_external_content": (
                "The selected entry refers to content Rift does not hydrate, such as Git LFS "
                "content."
            ),
            "skipped_sparse": "The selected path is outside the worktree's sparse-checkout definition.",
            "skipped_nested_repository": (
                "The selected path is a gitlink or lies inside a child repository, whose worktree "
                "is managed by its own Rift connection."
            ),
            "not_found": "The selected path does not differ between the candidate base and accepted commit.",
        }
    },
)
class PersistOutcomeOutcome(str, Enum):
    """What happened to this path."""

    WRITTEN = "written"
    UNCHANGED = "unchanged"
    SKIPPED_DRIFT = "skipped_drift"
    SKIPPED_DELETION = "skipped_deletion"
    SKIPPED_UNSUPPORTED_KIND = "skipped_unsupported_kind"
    SKIPPED_EXTERNAL_CONTENT = "skipped_external_content"
    SKIPPED_SPARSE = "skipped_sparse"
    SKIPPED_NESTED_REPOSITORY = "skipped_nested_repository"
    NOT_FOUND = "not_found"


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PersistOutcome(ClosedModel):
    "The materialization decision for one selected path. Safety conditions are reported as skips. Materialization keeps sparse-checkout boundaries, leaves child repositories untouched, and does not hydrate external content."

    path: Field[core.ProjectPath] = proto_field(
        description="Selected project path.", number=1
    )
    outcome: Field[PersistOutcomeOutcome] = proto_field(
        description="What happened to this path.",
        number=2,
        json_schema_extra={
            "rift:enumDescriptions": {
                "written": "The worktree entry matched the candidate base and now matches the accepted commit.",
                "unchanged": "The worktree entry already matched the accepted commit.",
                "skipped_drift": (
                    "The worktree entry differed from the candidate base and accepted commit, so "
                    "materialization preserved the local change."
                ),
                "skipped_deletion": "The accepted commit removes the entry and `include_deletions` is false.",
                "skipped_unsupported_kind": "The selected Git entry kind has no safe worktree materialization rule.",
                "skipped_external_content": (
                    "The selected entry refers to content Rift does not hydrate, such as Git LFS "
                    "content."
                ),
                "skipped_sparse": "The selected path is outside the worktree's sparse-checkout definition.",
                "skipped_nested_repository": (
                    "The selected path is a gitlink or lies inside a child repository, whose worktree "
                    "is managed by its own Rift connection."
                ),
                "not_found": "The selected path does not differ between the candidate base and accepted commit.",
            }
        },
    )


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class PersistResult(ClosedModel):
    "Materialization outcomes for one accepted commit. Results follow project-path byte order. Rift enumerates the selected paths and verifies that every outcome fits `max_response_bytes` before writing any path."

    revision: Field[core.Commit] = proto_field(
        description="Accepted commit used as the source tree.", number=1
    )
    all_written: Field[bool] = proto_field(
        description="Whether every selected path is `written` or `unchanged`.", number=2
    )
    outcomes: Field[list[PersistOutcome]] = proto_field(
        description="One result per selected path.", number=3
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.enum(
        "ConformanceProfile",
        (
            EnumValue("read", "CONFORMANCE_PROFILE_READ", 1),
            EnumValue("edit", "CONFORMANCE_PROFILE_EDIT", 2),
            EnumValue("full", "CONFORMANCE_PROFILE_FULL", 3),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "read": (
                "Serves the read tools plus repository, symbol, diff, and file resources. "
                "Fixtures reconstruct file bytes, verify language summaries and match tags, "
                "replay cursors, reject stale state, preserve coverage, and stop at gitlinks. "
                "Fixtures cover shared-worktree languages and retained virtual files. Mechanical "
                "reads and warm cached semantic reads run with adapters stopped. Fixed "
                "repositories record latency, bytes, tokens, and adapter memory."
            ),
            "edit": (
                "Adds `apply` and `integrate` preview, refresh, and publish, the preview "
                "resource, adapter formatting, and adapter validation. Fixtures exercise every "
                "Change variant and advertised action family, complete validation, preview "
                "pagination, refresh comparison, rejected-candidate repair, idempotent retry, "
                "compare-and-swap races, integration conflicts and target races, cancellation, "
                "and crash recovery. Caller-supplied validators remain unavailable and "
                "`max_validators` is zero."
            ),
            "full": (
                "Adds command validators and `persist` materialization. Fixtures verify response "
                "preflight before worktree writes, per-path drift outcomes, direct execution "
                "without shell expansion, validation-workspace cleanup, environment construction, "
                "timeouts, and bounded output capture. A release claim also runs the suite with "
                "two different language adapters and one embedded-language repository."
            ),
        }
    },
)
class ConformanceProfile(str, Enum):
    "Runtime conformance level verified for this workspace. Profiles accumulate. Shared checks validate schemas and examples, reference reachability, axis ownership, generated Protobuf output, adapter service compilation, and the handshake. Runtime fixtures cover ordering, limits, typed failures, the shared-worktree barrier, and topological virtual-source sync. `Contract` identifies the schema under test."

    READ = "read"
    EDIT = "edit"
    FULL = "full"


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


@definition(
    owner=MCP,
    public=False,
    proto=Proto.message(placement=Placement("content", 1)),
    schema_extra={},
)
class FileResourcePayloadUtf8FileContent(ClosedModel):
    kind: Field[Literal["regular"]] = proto_field(default="regular", number=1)


@definition(
    owner=MCP,
    public=False,
    proto=Proto.message(placement=Placement("file", 2)),
    schema_extra={},
)
class FileResourcePayloadUtf8File(ClosedModel):
    content: Field[FileResourcePayloadUtf8FileContent | None] = proto_field(
        default=None, number=1
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FileResourcePayloadUtf8(ClosedModel):
    """A range whose bytes decode as UTF-8. Start and end fall on UTF-8 code-point boundaries."""

    encoding: Field[Literal["utf8"]] = proto_field(default="utf8")
    content: Field[str | None] = proto_field(default=None, number=1)
    file: Field[FileResourcePayloadUtf8File | None] = proto_field(
        default=None, number=2
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.message(placement=Placement("content", 1)),
    schema_extra={},
)
class FileResourcePayloadBase64FileContent(ClosedModel):
    kind: Field[Literal["regular"]] = proto_field(default="regular", number=1)


@definition(
    owner=MCP,
    public=False,
    proto=Proto.message(placement=Placement("file", 2)),
    schema_extra={},
)
class FileResourcePayloadBase64File(ClosedModel):
    content: Field[FileResourcePayloadBase64FileContent | None] = proto_field(
        default=None, number=1
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FileResourcePayloadBase64(ClosedModel):
    """A range carried as canonical base64 because its bytes do not form valid UTF-8."""

    encoding: Field[Literal["base64"]] = proto_field(default="base64")
    content: Field[str | None] = proto_field(
        default=None,
        pattern="^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$",
        number=1,
    )
    file: Field[FileResourcePayloadBase64File | None] = proto_field(
        default=None, number=2
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.enum(
        "Kind",
        (
            EnumValue("lfs_pointer", "LFS_POINTER", 1),
            EnumValue("symlink", "SYMLINK", 2),
            EnumValue("gitlink", "GITLINK", 3),
        ),
        placement=Placement("kind", 1),
    ),
    schema_extra={},
)
class FileResourcePayloadNoneFileContentKind(str, Enum):
    LFS_POINTER = "lfs_pointer"
    SYMLINK = "symlink"
    GITLINK = "gitlink"


@definition(
    owner=MCP,
    public=False,
    proto=Proto.message(placement=Placement("content", 1)),
    schema_extra={},
)
class FileResourcePayloadNoneFileContent(ClosedModel):
    kind: Field[FileResourcePayloadNoneFileContentKind | None] = proto_field(
        default=None, number=1
    )


@definition(
    owner=MCP,
    public=False,
    proto=Proto.message(placement=Placement("file", 6)),
    schema_extra={},
)
class FileResourcePayloadNoneFile(ClosedModel):
    content: Field[FileResourcePayloadNoneFileContent | None] = proto_field(
        default=None, number=1
    )


@definition(owner=MCP, public=False, proto=Proto.message(), schema_extra={})
class FileResourcePayloadNone(ClosedModel):
    """A non-regular tree entry. Its empty interval carries no bytes and has no continuation."""

    encoding: Field[Literal["none"]] = proto_field(default="none")
    start: Field[Literal[0]] = proto_field(default=0, number=1)
    end: Field[Literal[0]] = proto_field(default=0, number=2)
    total_bytes: Field[Literal[0]] = proto_field(default=0, number=3)
    content: Field[None] = proto_field(default=None, number=4)
    next: Field[None] = proto_field(default=None, number=5)
    file: Field[FileResourcePayloadNoneFile | None] = proto_field(
        default=None, number=6
    )


@definition(
    owner=MCP,
    public=True,
    proto=Proto.union(
        Oneof("variant"),
        (
            Variant("utf8", "utf8", 1, FileResourcePayloadUtf8),
            Variant("base64", "base64", 2, FileResourcePayloadBase64),
            Variant("none", "none", 3, FileResourcePayloadNone),
        ),
    ),
    schema_extra={},
)
class FileResourcePayload(
    ProtocolRoot[
        (
            "Annotated[FileResourcePayloadUtf8 | FileResourcePayloadBase64 | "
            "FileResourcePayloadNone, schema_field(discriminator='encoding')]"
        )
    ]
):
    "One bounded byte range from a file at one state. Regular files carry UTF-8 text where the selected bytes form valid UTF-8 and base64 otherwise. `next` continues at `end` until the complete file has been read."


@definition(owner=MCP, public=True, proto=Proto.message(), schema_extra={})
class LanguageSupport(ClosedModel):
    """Adapter identity and advertised capability sets for one language in this workspace."""

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
    PreviewResourceLink,
    ResourceLink,
    SymbolResourcePayload,
    DiffResourcePayload,
    Contract,
    Limits,
    RepositoryResourceUri,
    FileResourceUri,
    PreviewResourceUri,
    RepositoryResourcePayload,
    SearchResult,
    ActionOffer,
    ActionsParams,
    ActionsResult,
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
    Change,
    DirectChange,
    PatchChange,
    ActionChange,
    RewriteChange,
    RevertChange,
    MergeChange,
    ResolvedChange,
    CommandValidator,
    ValidatorOutput,
    ValidatorResult,
    CandidateValidation,
    CandidateSummary,
    PreviewResourcePayload,
    ApplyParams,
    PreviewApplyParams,
    RefreshApplyParams,
    PublishApplyParams,
    ApplyResult,
    PreviewApplyResult,
    RefreshApplyResult,
    AcceptedApplyResult,
    RejectedApplyResult,
    RefusedApplyResult,
    ConflictApplyResult,
    IntegrationTarget,
    IntegrateParams,
    PreviewIntegrateParams,
    RefreshIntegrateParams,
    PublishIntegrateParams,
    MergeConflict,
    IntegrateResult,
    PreparedIntegrateResult,
    RefreshedIntegrateResult,
    MergeConflictIntegrateResult,
    IntegratedResult,
    RejectedIntegrateResult,
    RefusedIntegrateResult,
    ConflictIntegrateResult,
    PersistParams,
    PersistOutcome,
    PersistResult,
    ConformanceProfile,
    ResultOrder,
    FileResourcePayload,
    LanguageSupport,
)
