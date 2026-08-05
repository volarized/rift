from __future__ import annotations

from pydantic import field_validator

from . import core
from .base import *


@definition(
    owner="mcp",
    public=True,
    proto={"scalar": "string", "package": "rift.mcp"},
    schema_extra={},
)
class Cursor(
    ProtocolRoot[
        "Annotated[str, Field(description='An opaque string that continues a paginated answer from where the last page ended. It binds the request, state, order, and page size. A mismatch returns `cursor_invalid`.', min_length=1, max_length=4096, json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.mcp'}})]"
    ]
):
    """An opaque string that continues a paginated answer from where the last page ended. It binds the request, state, order, and page size. A mismatch returns `cursor_invalid`."""


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.SourceExcerpt"}, schema_extra={}
)
class SourceExcerpt(ClosedModel):
    """A copy of some source, and where it was copied from. A span points into a file that may change under you; an excerpt is the bytes as they were when the answer was produced."""

    span: core.SourceSpan = Field(
        description="The file and byte range the text was taken from.",
        json_schema_extra={"rift:proto": {"field": "span", "number": 1}},
    )
    text: str = Field(
        description="The source itself, as it stood at the answer's snapshot.",
        json_schema_extra={"rift:proto": {"field": "text", "number": 2}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.TreeParams"}, schema_extra={}
)
class TreeParams(ClosedModel):
    """Selects a portion of the project tree. The root controls hierarchy and `paths` filters the full project-relative path of each descendant."""

    root: core.ProjectPath = Field(
        description="Directory whose descendants are listed. The empty path selects the project root; the root entry itself is not returned. A gitlink is a file entry and cannot be used as a directory root.",
        json_schema_extra={"rift:proto": {"field": "root", "number": 1}},
    )
    depth: int | None = Field(
        description="Most directory edges below `root` to include. One lists immediate children; null walks every descendant and relies on pagination and response limits.",
        json_schema_extra={"rift:proto": {"field": "depth", "number": 2}},
    )
    paths: core.PathSelector = Field(
        description="Git-style include and exclude globs matched against each full project-relative descendant path.",
        json_schema_extra={"rift:proto": {"field": "paths", "number": 3}},
    )
    limit: int | None = Field(
        default=None,
        description="Most entries to return in one page. The server may stop earlier to keep the serialized result within `max_response_bytes`.",
        ge=1,
        le=10000,
        json_schema_extra={"rift:proto": {"field": "limit", "number": 4}},
    )
    cursor: Cursor | None = Field(
        default=None,
        description="Continues the same tree walk. Root, depth, paths, revision, and page size remain fixed.",
        json_schema_extra={"rift:proto": {"field": "cursor", "number": 5}},
    )
    rev: core.Revision | None = Field(
        default=None,
        description="Revision whose tree is listed.",
        json_schema_extra={"rift:proto": {"field": "rev", "number": 6}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.TreeResult"}, schema_extra={}
)
class TreeResult(ClosedModel):
    """One page of a project tree, ordered by project-path UTF-8 bytes. A directory precedes a file at the same path, though a valid snapshot normally contains only one."""

    at: core.Snapshot = Field(
        description="Snapshot whose paths were listed.",
        json_schema_extra={"rift:proto": {"field": "at", "number": 1}},
    )
    entries: list[core.ProjectEntry] = Field(
        description="Derived directories and Git entries on this page.",
        json_schema_extra={"rift:proto": {"field": "entries", "number": 2}},
    )
    next_cursor: Cursor | None = Field(
        description="Cursor for the next path page, or null after the final entry.",
        json_schema_extra={"rift:proto": {"field": "next_cursor", "number": 3}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "enum": "Include",
        "values": {
            "symbol": {"name": "SYMBOL", "number": 1},
            "source": {"name": "SOURCE", "number": 2},
            "diagnostics": {"name": "DIAGNOSTICS", "number": 3},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "symbol": "Attach the complete Symbol record, including signatures, types, documentation, "
            "and modifiers.",
            "source": "Attach the source covered by the outline leaf.",
            "diagnostics": "Attach compiler findings whose primary span falls inside the outline leaf.",
        }
    },
)
class OutlineParamsIncludeItemInclude(str, Enum):
    SYMBOL = "symbol"
    SOURCE = "source"
    DIAGNOSTICS = "diagnostics"


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.OutlineParams"}, schema_extra={}
)
class OutlineParams(ClosedModel):
    """Selects the compiler outline for one file. Outline nodes include declarations, definitions, imports, exports, and the parent leaves needed to preserve their nesting."""

    path: core.ProjectPath = Field(
        description="Project-relative file to outline.",
        min_length=1,
        json_schema_extra={"rift:proto": {"field": "path", "number": 1}},
    )
    depth: int | None = Field(
        description="Most outline nesting levels to return. Zero keeps top-level items; null includes every nested item.",
        json_schema_extra={"rift:proto": {"field": "depth", "number": 2}},
    )
    include: list[OutlineParamsIncludeItemInclude] = Field(
        description="Optional payload attached to each outline item. Symbol identity and source structure are always present.",
        json_schema_extra={
            "rift:proto": {"field": "include", "number": 3},
            "uniqueItems": True,
        },
    )
    limit: int | None = Field(
        default=None,
        description="Most outline items in one page. The server may stop earlier at the response-byte limit.",
        ge=1,
        le=10000,
        json_schema_extra={"rift:proto": {"field": "limit", "number": 4}},
    )
    cursor: Cursor | None = Field(
        default=None,
        description="Continues the same file outline with every other parameter unchanged.",
        json_schema_extra={"rift:proto": {"field": "cursor", "number": 5}},
    )
    rev: core.Revision | None = Field(
        default=None,
        description="Revision whose file and semantic facts are read.",
        json_schema_extra={"rift:proto": {"field": "rev", "number": 6}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.OutlineItem"}, schema_extra={}
)
class OutlineItem(ClosedModel):
    """One declaration-oriented source node. Items are ordered by range start, then widest range, then leaf identity, producing a stable source preorder."""

    leaf: core.Leaf = Field(
        description="Source structure and parent identity for this outline node.",
        json_schema_extra={"rift:proto": {"field": "leaf", "number": 1}},
    )
    symbol: core.Symbol | None = Field(
        default=None,
        description="Complete symbol data, present when `include` contains `symbol` and `leaf.symbol` exists.",
        json_schema_extra={"rift:proto": {"field": "symbol", "number": 2}},
    )
    source: SourceExcerpt | None = Field(
        default=None,
        description="Source covered by the leaf, present when `include` contains `source`.",
        json_schema_extra={"rift:proto": {"field": "source", "number": 3}},
    )
    diagnostics: list[DiagnosticContext] | None = Field(
        default=None,
        description="Findings inside the leaf, present when `include` contains `diagnostics`.",
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 4}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.OutlineResult"}, schema_extra={}
)
class OutlineResult(ClosedModel):
    """One page of a file outline with explicit semantic coverage."""

    at: core.Snapshot = Field(
        description="Snapshot from which the file and semantic facts were read.",
        json_schema_extra={"rift:proto": {"field": "at", "number": 1}},
    )
    file: core.File = Field(
        description="File entry being outlined.",
        json_schema_extra={"rift:proto": {"field": "file", "number": 2}},
    )
    items: list[OutlineItem] = Field(
        description="Outline items on this page in stable source preorder.",
        json_schema_extra={"rift:proto": {"field": "items", "number": 3}},
    )
    coverage: core.SemanticCoverage = Field(
        description="Completeness of leaves, symbols, types, relationships, and diagnostics used to build the outline.",
        json_schema_extra={"rift:proto": {"field": "coverage", "number": 4}},
    )
    next_cursor: Cursor | None = Field(
        description="Cursor for the next outline page, or null after the final item.",
        json_schema_extra={"rift:proto": {"field": "next_cursor", "number": 5}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "target",
        "number": 1,
        "enum": "Target",
        "values": {
            "symbol": {"name": "TARGET_SYMBOL", "number": 1},
            "leaf": {"name": "TARGET_LEAF", "number": 2},
            "file": {"name": "TARGET_FILE", "number": 3},
            "all": {"name": "TARGET_ALL", "number": 4},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "symbol": "Declarations the compiler resolved — a function, a class, a trait.",
            "leaf": "Places in a syntax tree where a symbol is written. One symbol has many.",
            "file": "Entries of the workspace tree. The only target that answers with no adapter "
            "installed.",
            "all": "Every kind above, in one ranked list.",
        }
    },
)
class SearchParamsTarget(str, Enum):
    """Which entity kinds may be returned. Type data is attached to the Symbol and Leaf views that bind it, and filters can search those attachments."""

    SYMBOL = "symbol"
    LEAF = "leaf"
    FILE = "file"
    ALL = "all"


@definition(
    owner="mcp",
    public=False,
    proto={
        "enum": "Include",
        "values": {
            "source": {"name": "INCLUDE_SOURCE", "number": 1},
            "signature": {"name": "INCLUDE_SIGNATURE", "number": 2},
            "relationships": {"name": "INCLUDE_RELATIONSHIPS", "number": 3},
            "diagnostics": {"name": "INCLUDE_DIAGNOSTICS", "number": 4},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "source": "The source around the hit, with the span it was copied from.",
            "signature": "The rendered signatures of a symbol hit, filled into `Symbol.signatures`. "
            "Nothing to add for a leaf or a file.",
            "relationships": "The edges leading out of the hit.",
            "diagnostics": "What the compiler reported at the hit.",
        }
    },
)
class SearchParamsIncludeItemInclude(str, Enum):
    SOURCE = "source"
    SIGNATURE = "signature"
    RELATIONSHIPS = "relationships"
    DIAGNOSTICS = "diagnostics"


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.SearchParams"}, schema_extra={}
)
class SearchParams(ClosedModel):
    """What to search for. At least one of `query` and `filter` is required: `query` is lexical, while `filter` is a predicate over compiler facts. `paths` narrows either form before matching."""

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
    target: SearchParamsTarget = Field(
        description="Which entity kinds may be returned. Type data is attached to the Symbol and Leaf views that bind it, and filters can search those attachments.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "symbol": "Declarations the compiler resolved — a function, a class, a trait.",
                "leaf": "Places in a syntax tree where a symbol is written. One symbol has many.",
                "file": "Entries of the workspace tree. The only target that answers with no adapter "
                "installed.",
                "all": "Every kind above, in one ranked list.",
            },
            "rift:proto": {
                "field": "target",
                "number": 1,
                "enum": "Target",
                "values": {
                    "symbol": {"name": "TARGET_SYMBOL", "number": 1},
                    "leaf": {"name": "TARGET_LEAF", "number": 2},
                    "file": {"name": "TARGET_FILE", "number": 3},
                    "all": {"name": "TARGET_ALL", "number": 4},
                },
            },
        },
    )
    order: ResultOrder = Field(
        description="Which total order the page comes back in. The cursor is bound to it, so it cannot change between pages of one query.",
        json_schema_extra={"rift:proto": {"field": "order", "number": 2}},
    )
    query: str | None = Field(
        default=None,
        description="Text to match against file contents, symbol names, and rendered signatures. `parse` matches that word in those fields. Caller lookup uses a relationship filter.",
        json_schema_extra={"rift:proto": {"field": "query", "number": 3}},
    )
    filter: core.Filter | None = Field(
        default=None,
        description="A predicate over resolved fields and relationships. This is where compiler knowledge enters a search — implements this trait, called by that function, declared under `src/api`.",
        json_schema_extra={"rift:proto": {"field": "filter", "number": 4}},
    )
    paths: core.PathSelector | None = Field(
        default=None,
        description="Files eligible for the search, selected by project-relative Git globs. Omitted selects every visible file.",
        json_schema_extra={"rift:proto": {"field": "paths", "number": 5}},
    )
    include: list[SearchParamsIncludeItemInclude] | None = Field(
        default=None,
        description="Extra payload to attach to every hit. Each entry costs a lookup per hit, so ask for what you will read.",
        json_schema_extra={"rift:proto": {"field": "include", "number": 6}},
    )
    limit: int | None = Field(
        default=None,
        description="Most hits to return in one page. `max_page_items` from the repository resource caps it, and fewer may come back.",
        ge=1,
        le=10000,
        json_schema_extra={"rift:proto": {"field": "limit", "number": 7}},
    )
    cursor: Cursor | None = Field(
        default=None,
        description="Continues a previous search where its last page ended. Omit it for the first page; everything else in the request has to match what the cursor was minted for.",
        json_schema_extra={"rift:proto": {"field": "cursor", "number": 8}},
    )
    rev: core.Revision | None = Field(
        default=None,
        description="Which revision to answer against. Absent, the default branch at its latest commit.",
        json_schema_extra={"rift:proto": {"field": "rev", "number": 9}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.SearchHit"}, schema_extra={}
)
class SearchHit(ClosedModel):
    """One search hit. Its file, leaf, or symbol payload carries the canonical identity."""

    hit: SearchHitTarget = Field(
        description="What was found. A symbol, a leaf, or a file — whichever `target` allowed.",
        json_schema_extra={"rift:proto": {"field": "hit", "number": 1}},
    )
    score: float = Field(
        description="How well this hit matched. Scores are comparable across every page of one request and nowhere else.",
        json_schema_extra={"rift:proto": {"field": "score", "number": 2}},
    )
    matched_by: list[str] = Field(
        description="Which fields produced the match — `name`, `signature`, the text of the file.",
        json_schema_extra={"rift:proto": {"field": "matched_by", "number": 3}},
    )
    relationships: list[core.Relationship] | None = Field(
        default=None,
        description='Edges from this hit, requested with `include: ["relationships"]`.',
        json_schema_extra={"rift:proto": {"field": "relationships", "number": 4}},
    )
    source: SourceExcerpt | None = Field(
        default=None,
        description='The source around the hit, requested with `include: ["source"]`. Carries its span, so an agent can act on what it read without searching for it again.',
        json_schema_extra={"rift:proto": {"field": "source", "number": 5}},
    )
    diagnostics: list[DiagnosticContext] | None = Field(
        default=None,
        description='What the compiler reported here, requested with `include: ["diagnostics"]`.',
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 6}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.PreviewResourceLink"},
    schema_extra={},
)
class PreviewResourceLink(ClosedModel):
    """MCP link to a retained preview plan. The fixed name and media type let `CandidateSummary.resource` admit only the resource that carries its complete contract."""

    type: Literal["resource_link"] = Field(
        description="MCP's tag for a link to a resource inside tool output.",
        json_schema_extra={"rift:proto": {"field": "type", "number": 1}},
    )
    uri: PreviewResourceUri = Field(
        description="The retained preview to read, optionally continued by a cursor.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 2}},
    )
    name: Literal["preview"] = Field(
        description="The resource family this link belongs to.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 3}},
    )
    mimeType: Literal["application/vnd.rift.preview+json"] = Field(
        description="What a read of this URI returns: `PreviewResourcePayload` as JSON.",
        json_schema_extra={"rift:proto": {"field": "mime_type", "number": 4}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.SymbolResourceLink"},
    schema_extra={},
)
class SymbolResourceLink(ClosedModel):
    """A link to the symbol resource, carrying the symbol's own URI."""

    type: Literal["resource_link"] = Field(
        description="MCP's tag for a link to a resource inside tool output."
    )
    uri: core.SymbolId = Field(
        description="The symbol to read. Hand it to `resources/read` unchanged.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    name: Literal["symbol"] = Field(
        description="The resource family this link belongs to.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 2}},
    )
    mimeType: Literal["application/vnd.rift.symbol+json"] = Field(
        description="What a read of this URI returns: `SymbolResourcePayload` as JSON.",
        json_schema_extra={"rift:proto": {"field": "mime_type", "number": 3}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.RepositoryResourceLink"},
    schema_extra={},
)
class RepositoryResourceLink(ClosedModel):
    """A link to the repository resource."""

    type: Literal["resource_link"] = Field(
        description="MCP's tag for a link to a resource inside tool output."
    )
    uri: RepositoryResourceUri = Field(
        description="The repository resource, optionally pinned to a revision and continued by a cursor.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    name: Literal["repository"] = Field(
        description="The resource family this link belongs to.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 2}},
    )
    mimeType: Literal["application/vnd.rift.repository+json"] = Field(
        description="What a read of this URI returns: `RepositoryResourcePayload` as JSON.",
        json_schema_extra={"rift:proto": {"field": "mime_type", "number": 3}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.DiffResourceLink"},
    schema_extra={},
)
class DiffResourceLink(ClosedModel):
    """A link to the comparison between two revisions."""

    type: Literal["resource_link"] = Field(
        description="MCP's tag for a link to a resource inside tool output."
    )
    uri: core.DiffId = Field(
        description="The comparison to read, optionally continued by a cursor.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    name: Literal["diff"] = Field(
        description="The resource family this link belongs to.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 2}},
    )
    mimeType: Literal["application/vnd.rift.diff+json"] = Field(
        description="What a read of this URI returns: `DiffResourcePayload` as JSON.",
        json_schema_extra={"rift:proto": {"field": "mime_type", "number": 3}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.FileResourceLink"},
    schema_extra={},
)
class FileResourceLink(ClosedModel):
    """A link to one file at one revision."""

    type: Literal["resource_link"] = Field(
        description="MCP's tag for a link to a resource inside tool output."
    )
    uri: FileResourceUri = Field(
        description="The file range to read, optionally pinned to a revision.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    name: Literal["file"] = Field(
        description="The resource family this link belongs to.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 2}},
    )
    mimeType: Literal["application/vnd.rift.file+json"] = Field(
        description="What a read of this URI returns: `FileResourcePayload` as JSON.",
        json_schema_extra={"rift:proto": {"field": "mime_type", "number": 3}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.ResourceLink",
        "oneof": "variant",
        "variants": [
            {
                "tag": "symbol",
                "field": "symbol",
                "number": 1,
                "type": "SymbolResourceLink",
            },
            {
                "tag": "repository",
                "field": "repository",
                "number": 2,
                "type": "RepositoryResourceLink",
            },
            {
                "tag": "diff",
                "field": "diff",
                "number": 3,
                "type": "DiffResourceLink",
            },
            {
                "tag": "file",
                "field": "file",
                "number": 4,
                "type": "FileResourceLink",
            },
            {
                "tag": "preview",
                "field": "preview",
                "number": 5,
                "type": "PreviewResourceLink",
            },
        ],
    },
    schema_extra={},
)
class ResourceLink(
    ProtocolRoot[
        "Annotated[SymbolResourceLink | RepositoryResourceLink | DiffResourceLink | FileResourceLink | PreviewResourceLink, Field(discriminator='mimeType')]"
    ]
):
    """A link to one Rift resource, as MCP carries it. The resource's name and media type are fixed per resource, and the URI is the one that resource accepts."""


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.SymbolResourcePayload"},
    schema_extra={},
)
class SymbolResourcePayload(ClosedModel):
    """JSON payload for one symbol at one snapshot."""

    uri: core.SymbolId = Field(
        description="The symbol this payload answers for, echoed back so a link and its content carry the same address.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    at: core.Snapshot = Field(
        description="The snapshot this answer was resolved against. Pass it back as `rev` when a later call has to agree with this one.",
        json_schema_extra={"rift:proto": {"field": "at", "number": 2}},
    )
    symbol: core.Symbol = Field(
        description="The declaration itself: its name, its kind in the language's own words, its origin, its types and its signatures.",
        json_schema_extra={"rift:proto": {"field": "symbol", "number": 3}},
    )
    origin_mappings: list[core.OriginMapping] = Field(
        description="Relations from produced declaration bytes to source ranges a caller can inspect or edit. Empty for a declaration read directly from physical source.",
        json_schema_extra={"rift:proto": {"field": "origin_mappings", "number": 4}},
    )
    leaves: list[core.Leaf] = Field(
        description="Every place this symbol is written — the declaration and each mention. This is the list a rename has to rewrite.",
        json_schema_extra={"rift:proto": {"field": "leaves", "number": 5}},
    )
    relationships: list[core.Relationship] = Field(
        description="Edges into and out of this symbol, each carrying the leaves the compiler read it from.",
        json_schema_extra={"rift:proto": {"field": "relationships", "number": 6}},
    )
    source: list[SourceExcerpt] = Field(
        description="The source at each leaf, so the declaration and its call sites can be read without a second round trip.",
        json_schema_extra={"rift:proto": {"field": "source", "number": 7}},
    )
    diagnostics: list[DiagnosticContext] = Field(
        description="What the compiler reported at this symbol's leaves.",
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 8}},
    )
    coverage: core.SemanticCoverage = Field(
        description="How complete each fact family is for this symbol. An empty `relationships` means the symbol has no edges only where that family is complete.",
        json_schema_extra={"rift:proto": {"field": "coverage", "number": 9}},
    )
    next: core.SymbolId | None = Field(
        description="The same symbol URI carrying the cursor for the next page, or null on the last one. Leaves, edges and diagnostics are what page.",
        json_schema_extra={"rift:proto": {"field": "next", "number": 10}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.DiffResourcePayload"},
    schema_extra={},
)
class DiffResourcePayload(ClosedModel):
    """One page of a comparison. `from` and `to` are what the URI's revisions resolved to, so a diff taken against a moving branch records which commits it actually compared. Git answers this without a compiler, so it works for a language Rift has no adapter for."""

    uri: core.DiffId = Field(
        description="The comparison this payload answers for, echoed back with the cursor that produced this page.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    from_: core.Snapshot = Field(
        alias="from",
        description="The old side, as it resolved.",
        json_schema_extra={"rift:proto": {"field": "from", "number": 2}},
    )
    to: core.Snapshot = Field(
        description="The new side, as it resolved.",
        json_schema_extra={"rift:proto": {"field": "to", "number": 3}},
    )
    files: list[core.FileChange] = Field(
        description="The files this page covers.",
        json_schema_extra={"rift:proto": {"field": "files", "number": 4}},
    )
    truncated: bool = Field(
        description="Whether files were dropped to stay inside the size limit. Paging past the limit uses `next`.",
        json_schema_extra={"rift:proto": {"field": "truncated", "number": 5}},
    )
    next: core.DiffId | None = Field(
        description="The same comparison carrying the cursor for the next page, or null on the last one.",
        json_schema_extra={"rift:proto": {"field": "next", "number": 6}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.Contract"}, schema_extra={}
)
class Contract(ClosedModel):
    """Which protocol contract this server speaks. The version selects compatibility behavior. The schema identifier names the JSON contract exposed through MCP."""

    protocol: core.ProtocolVersion = Field(
        description="The protocol version both sides have to agree on before any other field is meaningful.",
        json_schema_extra={"rift:proto": {"field": "protocol", "number": 1}},
    )
    schema_: Literal["https://volar.sh/rift/protocol/mcp.json"] = Field(
        alias="schema",
        description="Canonical identifier of the MCP JSON Schema. The protocol version selects its matching generated Protobuf packages.",
        json_schema_extra={"rift:proto": {"field": "schema", "number": 2}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.Limits"}, schema_extra={}
)
class Limits(ClosedModel):
    """The ceilings this server enforces on MCP requests and responses. They come from host policy at launch, so two workspaces running the same Rift can differ. A request over one of them, or a response that would be, fails with `limit_exceeded` carrying `LimitEvidence`."""

    max_request_bytes: int = Field(
        description="Largest request body the server accepts, in bytes. A deep filter tree or a long structural pattern is the usual way to exceed it.",
        ge=1024,
        le=49152,
        json_schema_extra={"rift:proto": {"field": "max_request_bytes", "number": 1}},
    )
    max_response_bytes: Literal[65536] = Field(
        description="Largest serialized tool result or resource page Rift returns. Paginated answers stop before this boundary and provide a cursor; an indivisible result that cannot fit fails with `limit_exceeded`. Conforming `read` implementations advertise at most 65536 bytes, below the truncation boundary of common MCP harnesses.",
        json_schema_extra={"rift:proto": {"field": "max_response_bytes", "number": 2}},
    )
    max_record_bytes: int = Field(
        description="Largest RFC 8785 JSON encoding of one indivisible item in a paginated answer, such as one Change, Edit, diagnostic, or validator result. Resolution fails with `limit_exceeded` before retaining a preview when one item exceeds this value. Host policy sets it at or below 49152 bytes, leaving page space for identity and cursors.",
        ge=1024,
        le=49152,
        json_schema_extra={"rift:proto": {"field": "max_record_bytes", "number": 3}},
    )
    max_file_chunk_bytes: int = Field(
        description="Most source bytes one file resource page carries before UTF-8 or base64 serialization. Rift may return fewer bytes to preserve a UTF-8 boundary and stay within `max_response_bytes`.",
        ge=1024,
        le=32768,
        json_schema_extra={
            "rift:proto": {"field": "max_file_chunk_bytes", "number": 4}
        },
    )
    max_page_items: int = Field(
        description="Ceiling on `limit` for every paginated tool. Asking for more is `invalid_request`; asking for less than this still permits a shorter page.",
        ge=1,
        le=10000,
        json_schema_extra={"rift:proto": {"field": "max_page_items", "number": 5}},
    )
    max_relation_depth: int = Field(
        description="How far a relationship filter may walk. Transitive callers of a widely used function fan out fast, and this is what stops one query from touching the whole graph.",
        ge=1,
        le=100,
        json_schema_extra={"rift:proto": {"field": "max_relation_depth", "number": 6}},
    )
    max_changes: int = Field(
        description="Most top-level `Change` values one preview request accepts.",
        ge=1,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "max_changes", "number": 7}},
    )
    max_edits: int = Field(
        description="Most concrete `Edit` values one resolved candidate may contain across every change.",
        ge=1,
        le=1000000,
        json_schema_extra={"rift:proto": {"field": "max_edits", "number": 8}},
    )
    max_validators: int = Field(
        description="How many caller-supplied checks may run over one proposed change. Zero where this workspace runs none, which is every profile below `full`.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "max_validators", "number": 9}},
    )
    max_rewrite_expansions: int = Field(
        description="Most concrete edits one atomic `RewriteChange` may produce after matching.",
        ge=1,
        le=100000,
        json_schema_extra={
            "rift:proto": {"field": "max_rewrite_expansions", "number": 10}
        },
    )
    record_retention_seconds: int = Field(
        description="Minimum time Rift retains previews and idempotent publish results after their last access. A later read returns `record_reclaimed` once collection removes the record.",
        ge=60,
        le=31536000,
        json_schema_extra={
            "rift:proto": {"field": "record_retention_seconds", "number": 11}
        },
    )


@definition(
    owner="mcp",
    public=True,
    proto={"scalar": "string", "package": "rift.mcp"},
    schema_extra={},
)
class RepositoryResourceUri(
    ProtocolRoot[
        "Annotated[str, Field(description='Paginated workspace metadata and capabilities at one revision.', pattern=\"^rift://repository(\\\\?(rev=[A-Za-z0-9._~%!$&'()*+,;:@/-]{1,256}(&cursor=[^&#]+)?|cursor=[^&#]+))?$\", json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.mcp'}})]"
    ]
):
    """Paginated workspace metadata and capabilities at one revision."""


@definition(
    owner="mcp",
    public=True,
    proto={"scalar": "string", "package": "rift.mcp"},
    schema_extra={},
)
class FileResourceUri(
    ProtocolRoot[
        "Annotated[str, Field(description=\"URI for one file content range. `start` and `length` are byte coordinates and appear together. Their absence starts at byte zero with the server's advertised chunk bound.\", pattern=\"^rift://file/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-F]{2}){1,1000}(?:\\\\?rev=[A-Za-z0-9._~%!$&'()*+,;:@/-]{1,256}(?:&start=[0-9]+&length=[1-9][0-9]*)?|\\\\?start=[0-9]+&length=[1-9][0-9]*)?$\", min_length=13, max_length=1200, json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.mcp'}})]"
    ]
):
    """URI for one file content range. `start` and `length` are byte coordinates and appear together. Their absence starts at byte zero with the server's advertised chunk bound."""


@definition(
    owner="mcp",
    public=True,
    proto={"scalar": "string", "package": "rift.mcp"},
    schema_extra={},
)
class PreviewResourceUri(
    ProtocolRoot[
        "Annotated[str, Field(description='URI for one page of a retained preview. The path carries its opaque `PreviewId`; an optional cursor continues the same immutable plan.', pattern='^rift://preview/[A-Za-z0-9_-]{16,128}(\\\\?cursor=[^&#]+)?$', json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.mcp'}})]"
    ]
):
    """URI for one page of a retained preview. The path carries its opaque `PreviewId`; an optional cursor continues the same immutable plan."""


@definition(
    owner="mcp",
    public=False,
    proto={
        "enum": "Tools",
        "values": {
            "tree": {"name": "TOOLS_TREE", "number": 1},
            "outline": {"name": "TOOLS_OUTLINE", "number": 2},
            "search": {"name": "TOOLS_SEARCH", "number": 3},
            "match": {"name": "TOOLS_MATCH", "number": 4},
            "actions": {"name": "TOOLS_ACTIONS", "number": 5},
            "apply": {"name": "TOOLS_APPLY", "number": 6},
            "persist": {"name": "TOOLS_PERSIST", "number": 7},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "tree": "Snapshot-pinned project tree listing over every file, without an adapter.",
            "outline": "Compiler-owned declaration structure and diagnostics for one file.",
            "search": "Ranked lookup of symbols, leaves and files.",
            "match": "Literal, regular-expression and structural matching, with a key per hit.",
            "actions": "The fixes and refactors a compiler offers at one address.",
            "apply": "Previews, refreshes, and publishes deterministic candidates through the retained "
            "validation contract.",
            "persist": "Materializes selected paths from an accepted commit into the session worktree.",
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
    PERSIST = "persist"


@definition(
    owner="mcp",
    public=False,
    proto={
        "enum": "Resources",
        "values": {
            "repository": {"name": "RESOURCES_REPOSITORY", "number": 1},
            "symbol": {"name": "RESOURCES_SYMBOL", "number": 2},
            "diff": {"name": "RESOURCES_DIFF", "number": 3},
            "file": {"name": "RESOURCES_FILE", "number": 4},
            "preview": {"name": "RESOURCES_PREVIEW", "number": 5},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "repository": "This resource: what the workspace can do, and the limits on asking.",
            "symbol": "One symbol, its leaves, its edges and its diagnostics.",
            "diff": "What changed between two revisions.",
            "file": "One file's tree entry and its bytes.",
            "preview": "One retained candidate's requests, resolved plans, validation evidence, and "
            "confirmations.",
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
    owner="mcp",
    public=False,
    proto={
        "field": "object_format",
        "number": 10,
        "enum": "ObjectFormat",
        "values": {
            "sha1": {"name": "OBJECT_FORMAT_SHA1", "number": 1},
            "sha256": {"name": "OBJECT_FORMAT_SHA256", "number": 2},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "sha1": "Object IDs are 40 hex characters. Still git's default.",
            "sha256": "Object IDs are 64 hex characters. A repository created with "
            "`--object-format=sha256`.",
        }
    },
)
class RepositoryResourcePayloadObjectFormat(str, Enum):
    """Which hash git uses for object IDs in this repository. It decides how long a `Commit` is, so a client that validates one has to read this first."""

    SHA1 = "sha1"
    SHA256 = "sha256"


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.RepositoryResourcePayload"},
    schema_extra={},
)
class RepositoryResourcePayload(ClosedModel):
    """What Rift can do for this workspace right now: the languages it understands, what their adapters support, the snapshot it answered from, and the limits a request has to stay inside. One workspace covers one repository. Future federation reads several repository resources and keeps each repository's snapshot and publish boundary independent."""

    uri: RepositoryResourceUri = Field(
        description="The URI this payload answers for, echoed back with the revision and cursor it resolved.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    at: core.Snapshot = Field(
        description="The snapshot this answer was resolved against. Pass it back as `rev` when a later call has to agree with this one.",
        json_schema_extra={"rift:proto": {"field": "at", "number": 2}},
    )
    contract: Contract = Field(
        description="Which protocol version and schema this server speaks. Check it before trusting the shape of anything else.",
        json_schema_extra={"rift:proto": {"field": "contract", "number": 3}},
    )
    limits: Limits = Field(
        description="The ceilings a request has to stay inside here.",
        json_schema_extra={"rift:proto": {"field": "limits", "number": 4}},
    )
    languages: list[LanguageSupport] = Field(
        description="The languages Rift understands here, and what it can do with each.",
        json_schema_extra={
            "rift:proto": {"field": "languages", "number": 5},
            "uniqueItems": True,
        },
    )
    coverage: core.SemanticCoverage = Field(
        description="How complete each fact family is across the workspace. A family reported `unsupported` here will be unsupported in every answer, so there is no point asking for it.",
        json_schema_extra={"rift:proto": {"field": "coverage", "number": 6}},
    )
    tools: list[RepositoryResourcePayloadToolsItemTools] = Field(
        description="The MCP tools this workspace serves. A tool the profile does not reach is absent from this list and from `tools/list`.",
        json_schema_extra={
            "rift:proto": {"field": "tools", "number": 7},
            "uniqueItems": True,
        },
    )
    resources: list[RepositoryResourcePayloadResourcesItemResources] = Field(
        description="The MCP resource families this workspace serves.",
        json_schema_extra={
            "rift:proto": {"field": "resources", "number": 8},
            "uniqueItems": True,
        },
    )
    next: RepositoryResourceUri | None = Field(
        description="The same resource carrying the cursor for the next page, or null on the last one.",
        json_schema_extra={"rift:proto": {"field": "next", "number": 9}},
    )
    object_format: RepositoryResourcePayloadObjectFormat = Field(
        description="Which hash git uses for object IDs in this repository. It decides how long a `Commit` is, so a client that validates one has to read this first.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "sha1": "Object IDs are 40 hex characters. Still git's default.",
                "sha256": "Object IDs are 64 hex characters. A repository created with "
                "`--object-format=sha256`.",
            },
            "rift:proto": {
                "field": "object_format",
                "number": 10,
                "enum": "ObjectFormat",
                "values": {
                    "sha1": {"name": "OBJECT_FORMAT_SHA1", "number": 1},
                    "sha256": {"name": "OBJECT_FORMAT_SHA256", "number": 2},
                },
            },
        },
    )
    matching: MatchSyntax = Field(
        description="The pattern grammars the `match` tool accepts here.",
        json_schema_extra={"rift:proto": {"field": "matching", "number": 11}},
    )
    profile: ConformanceProfile = Field(
        description="The tier this workspace passes the conformance suite for. It is what says whether `apply` and `persist` are real here.",
        json_schema_extra={"rift:proto": {"field": "profile", "number": 12}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.SearchResult"}, schema_extra={}
)
class SearchResult(ClosedModel):
    """One page of search hits, and what the page is worth. `coverage` is what makes an empty page readable: nothing matched, or Rift could not see far enough to know."""

    at: core.Snapshot = Field(
        description="The snapshot this answer was resolved against. Pass it back as `rev` when a later call has to agree with this one.",
        json_schema_extra={"rift:proto": {"field": "at", "number": 1}},
    )
    coverage: core.Coverage = Field(
        description="How much Rift could see while answering. An empty page means nothing matched only where this is complete.",
        json_schema_extra={"rift:proto": {"field": "coverage", "number": 2}},
    )
    results: list[SearchHit] = Field(
        description="The hits on this page, in the order the request asked for.",
        json_schema_extra={"rift:proto": {"field": "results", "number": 3}},
    )
    next_cursor: Cursor | None = Field(
        description="Pass this back to get the next page. Null on the last one.",
        json_schema_extra={"rift:proto": {"field": "next_cursor", "number": 4}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.ActionOffer"}, schema_extra={}
)
class ActionOffer(ClosedModel):
    """One compiler action discovered at an address. It combines a portable descriptor with a state-bound resolution key."""

    descriptor: core.ActionDescriptor = Field(
        description="What the action does, what it applies to, and the schema of the arguments it takes.",
        json_schema_extra={"rift:proto": {"field": "descriptor", "number": 1}},
    )
    key: core.ActionKey = Field(
        description="Language, snapshot, and adapter token used to resolve this offer.",
        json_schema_extra={"rift:proto": {"field": "key", "number": 2}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.ActionsParams"}, schema_extra={}
)
class ActionsParams(ClosedModel):
    """What to ask a compiler for, and where. `target` picks the place, `only` narrows the kinds, and `rev` decides which state the question is asked against."""

    target: core.Address = Field(
        description="Where in the code to ask. A symbol, a leaf, a byte range, or a match you already have.",
        json_schema_extra={"rift:proto": {"field": "target", "number": 1}},
    )
    only: list[core.ActionKind] = Field(
        description="Kind prefixes to keep. `refactor` returns everything under it; an empty list returns every action the compiler offers.",
        json_schema_extra={
            "rift:proto": {"field": "only", "number": 2},
            "uniqueItems": True,
        },
    )
    limit: int | None = Field(
        default=None,
        description="Most actions to return in one page.",
        ge=1,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "limit", "number": 3}},
    )
    cursor: Cursor | None = Field(
        default=None,
        description="Continues a previous call. Omit for the first page.",
        json_schema_extra={"rift:proto": {"field": "cursor", "number": 4}},
    )
    rev: core.Revision | None = Field(
        default=None,
        description="Which revision to answer against. Absent, the default branch at its latest commit.",
        json_schema_extra={"rift:proto": {"field": "rev", "number": 5}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.ActionsResult"}, schema_extra={}
)
class ActionsResult(ClosedModel):
    """Discovered actions for one pinned address. Offers sort by language, target, kind, argument contract, argument schema, guarantees, full descriptor, and token. Object fields use RFC 8785 ordering. The cursor binds this order, the snapshot, and the adapter build."""

    at: core.Snapshot = Field(
        description="The snapshot this answer was resolved against. Pass it back as `rev` when a later call has to agree with this one.",
        json_schema_extra={"rift:proto": {"field": "at", "number": 1}},
    )
    actions: list[ActionOffer] = Field(
        description="The actions on this page.",
        json_schema_extra={"rift:proto": {"field": "actions", "number": 2}},
    )
    coverage: core.Coverage = Field(
        description="Whether the compiler could answer here. A language with no action support returns an empty list with `unsupported` coverage and its reason.",
        json_schema_extra={"rift:proto": {"field": "coverage", "number": 3}},
    )
    next_cursor: Cursor | None = Field(
        description="Pass this back to get the next page. Null on the last one.",
        json_schema_extra={"rift:proto": {"field": "next_cursor", "number": 4}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.MatchHitStructural"},
    schema_extra={},
)
class MatchHitStructural(ClosedModel):
    """A structural match with grammar-derived replacement ranges."""

    kind: Literal["structural"] = Field(description="Tags this as a structural match.")
    captures: list[core.Capture] = Field(
        description="The named captures this match bound, in the order the pattern declares them.",
        json_schema_extra={"rift:proto": {"field": "captures", "number": 1}},
    )
    explanation: list[str] = Field(
        description="Why this is a match, one step per line.",
        json_schema_extra={"rift:proto": {"field": "explanation", "number": 2}},
    )
    replacement_ranges: core.StructuralMatchRanges = Field(
        description="Ranges the adapter says can be replaced whole: the matched node alone, or with the whitespace and punctuation on either side. Rewriting `foo(a, b)` out of a list needs one of the wider ones to avoid leaving a comma behind.",
        json_schema_extra={"rift:proto": {"field": "replacement_ranges", "number": 3}},
    )
    extensions: core.Extensions = Field(
        description="Facts the adapter carries that this model has no field for, under a reverse-domain key.",
        json_schema_extra={"rift:proto": {"field": "extensions", "number": 4}},
    )
    key: core.MatchKey = Field(
        description="Identity of this match and the state it was found in. An edit addressed at it is checked against this before it lands.",
        json_schema_extra={"rift:proto": {"field": "key", "number": 5}},
    )


@definition(
    owner="mcp", public=False, proto={"type": "rift.mcp.MatchHitText"}, schema_extra={}
)
class MatchHitText(ClosedModel):
    """A text match. Bytes matched bytes, with no tree behind them and nothing to say about what is safe to replace."""

    kind: Literal["text"] = Field(description="Tags this as a text match.")
    captures: list[core.Capture] = Field(
        description="The named captures this match bound, in the order the pattern declares them.",
        json_schema_extra={"rift:proto": {"field": "captures", "number": 1}},
    )
    explanation: list[str] = Field(
        description="Why this is a match, one step per line.",
        json_schema_extra={"rift:proto": {"field": "explanation", "number": 2}},
    )
    key: core.MatchKey = Field(
        description="Identity of this match and the state it was found in. An edit addressed at it is checked against this before it lands.",
        json_schema_extra={"rift:proto": {"field": "key", "number": 3}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.MatchHit",
        "oneof": "variant",
        "variants": [
            {
                "tag": "structural",
                "field": "structural",
                "number": 1,
                "type": "MatchHitStructural",
            },
            {"tag": "text", "field": "text", "number": 2, "type": "MatchHitText"},
        ],
    },
    schema_extra={},
)
class MatchHit(
    ProtocolRoot[
        "Annotated[MatchHitStructural | MatchHitText, Field(discriminator='kind')]"
    ]
):
    """One match, tagged by the engine that produced it. The tag equals `key.query.kind`. A structural match carries grammar-derived replacement ranges. A text match carries source captures only."""


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.MatchParams"}, schema_extra={}
)
class MatchParams(ClosedModel):
    """A match request: the query, a page size, and the revision to run it against. The complete query travels in every match key, so a key can be inspected and replayed without hidden lookup state."""

    query: core.MatchQuery = Field(
        description="What to look for, and which engine answers it.",
        json_schema_extra={"rift:proto": {"field": "query", "number": 1}},
    )
    limit: int = Field(
        default=50,
        description="Most matches to return in one page, capped by `max_page_items`.",
        ge=1,
        le=10000,
        json_schema_extra={"rift:proto": {"field": "limit", "number": 2}},
    )
    cursor: Cursor | None = Field(
        default=None,
        description="Continues a previous match where its last page ended. Omit it for the first page.",
        json_schema_extra={"rift:proto": {"field": "cursor", "number": 3}},
    )
    rev: core.Revision | None = Field(
        default=None,
        description="Which revision to answer against. Absent, the default branch at its latest commit.",
        json_schema_extra={"rift:proto": {"field": "rev", "number": 4}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.MatchResult"}, schema_extra={}
)
class MatchResult(ClosedModel):
    """One page of matches and the state they were found in. Matches sort by file bytes, range, and canonical key. Rift checks the key against `at` before applying an addressed edit."""

    at: core.Snapshot = Field(
        description="The snapshot this answer was resolved against. Pass it back as `rev` when a later call has to agree with this one.",
        json_schema_extra={"rift:proto": {"field": "at", "number": 1}},
    )
    matches: list[MatchHit] = Field(
        description="The matches on this page.",
        json_schema_extra={"rift:proto": {"field": "matches", "number": 2}},
    )
    coverage: core.Coverage = Field(
        description="How much of the selected path set was actually searched. A file too large to read leaves this partial with a reason.",
        json_schema_extra={"rift:proto": {"field": "coverage", "number": 3}},
    )
    next_cursor: Cursor | None = Field(
        description="Pass this back to get the next page. Null on the last one.",
        json_schema_extra={"rift:proto": {"field": "next_cursor", "number": 4}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "source",
        "number": 1,
        "enum": "Source",
        "values": {
            "compiler": {"name": "COMPILER", "number": 1},
            "validator": {"name": "VALIDATOR", "number": 2},
            "apply": {"name": "APPLY", "number": 3},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "compiler": "The language's own analysis.",
            "validator": "A check Rift ran over a proposed change.",
            "apply": "Reported while applying edits to the workspace.",
        }
    },
)
class DiagnosticContextSource(str, Enum):
    """Who produced it. The adapter's compiler never knows which of these ran, so the field is Rift's to set."""

    COMPILER = "compiler"
    VALIDATOR = "validator"
    APPLY = "apply"


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.DiagnosticContext"},
    schema_extra={},
)
class DiagnosticContext(ClosedModel):
    """One `Diagnostic` as an MCP answer carries it: the fact the compiler minted, plus what Rift can add without the compiler — where it lands in a line and column, and the source around it."""

    source: DiagnosticContextSource = Field(
        description="Who produced it. The adapter's compiler never knows which of these ran, so the field is Rift's to set.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "compiler": "The language's own analysis.",
                "validator": "A check Rift ran over a proposed change.",
                "apply": "Reported while applying edits to the workspace.",
            },
            "rift:proto": {
                "field": "source",
                "number": 1,
                "enum": "Source",
                "values": {
                    "compiler": {"name": "COMPILER", "number": 1},
                    "validator": {"name": "VALIDATOR", "number": 2},
                    "apply": {"name": "APPLY", "number": 3},
                },
            },
        },
    )
    at: core.Snapshot = Field(
        description="The state this answer was resolved against.",
        json_schema_extra={"rift:proto": {"field": "at", "number": 2}},
    )
    diagnostic: core.Diagnostic = Field(
        description="The finding itself, exactly as the adapter minted it.",
        json_schema_extra={"rift:proto": {"field": "diagnostic", "number": 3}},
    )
    line: int | None = Field(
        description="One-based line the finding starts on. Null where the diagnostic has no span — a whole-project error has nowhere to point.",
        json_schema_extra={"rift:proto": {"field": "line", "number": 4}},
    )
    column: int | None = Field(
        description="One-based column within that line, counted in UTF-8 bytes. Null for the same reason as `line`.",
        json_schema_extra={"rift:proto": {"field": "column", "number": 5}},
    )
    excerpt: SourceExcerpt | None = Field(
        description="The source the finding points at. Null where there is no span to copy from.",
        json_schema_extra={"rift:proto": {"field": "excerpt", "number": 6}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.ErrorCode",
        "enum": "ErrorCode",
        "values": {
            "invalid_request": {"name": "ERROR_CODE_INVALID_REQUEST", "number": 1},
            "unsupported_protocol": {
                "name": "ERROR_CODE_UNSUPPORTED_PROTOCOL",
                "number": 2,
            },
            "permission_denied": {"name": "ERROR_CODE_PERMISSION_DENIED", "number": 3},
            "snapshot_not_found": {
                "name": "ERROR_CODE_SNAPSHOT_NOT_FOUND",
                "number": 4,
            },
            "revision_not_accepted": {
                "name": "ERROR_CODE_REVISION_NOT_ACCEPTED",
                "number": 5,
            },
            "semantic_snapshot_mismatch": {
                "name": "ERROR_CODE_SEMANTIC_SNAPSHOT_MISMATCH",
                "number": 6,
            },
            "semantic_snapshot_unavailable": {
                "name": "ERROR_CODE_SEMANTIC_SNAPSHOT_UNAVAILABLE",
                "number": 7,
            },
            "resource_not_found": {
                "name": "ERROR_CODE_RESOURCE_NOT_FOUND",
                "number": 8,
            },
            "record_reclaimed": {"name": "ERROR_CODE_RECORD_RECLAIMED", "number": 9},
            "content_unavailable": {
                "name": "ERROR_CODE_CONTENT_UNAVAILABLE",
                "number": 10,
            },
            "cursor_invalid": {"name": "ERROR_CODE_CURSOR_INVALID", "number": 11},
            "cursor_snapshot_mismatch": {
                "name": "ERROR_CODE_CURSOR_SNAPSHOT_MISMATCH",
                "number": 12,
            },
            "cancelled": {"name": "ERROR_CODE_CANCELLED", "number": 13},
            "deadline_exceeded": {"name": "ERROR_CODE_DEADLINE_EXCEEDED", "number": 14},
            "limit_exceeded": {"name": "ERROR_CODE_LIMIT_EXCEEDED", "number": 15},
            "worktree_busy": {"name": "ERROR_CODE_WORKTREE_BUSY", "number": 16},
            "adapter_unavailable": {
                "name": "ERROR_CODE_ADAPTER_UNAVAILABLE",
                "number": 17,
            },
            "adapter_protocol_error": {
                "name": "ERROR_CODE_ADAPTER_PROTOCOL_ERROR",
                "number": 18,
            },
            "adapter_timeout": {"name": "ERROR_CODE_ADAPTER_TIMEOUT", "number": 19},
            "storage_failure": {"name": "ERROR_CODE_STORAGE_FAILURE", "number": 20},
            "sandbox_failure": {"name": "ERROR_CODE_SANDBOX_FAILURE", "number": 21},
            "internal_error": {"name": "ERROR_CODE_INTERNAL_ERROR", "number": 22},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "invalid_request": "The request does not satisfy the schema, or names something the schema "
            "forbids: a `limit` above the advertised maximum, a filter field that "
            "does not exist. The same bytes fail identically.",
            "unsupported_protocol": "The client and server use different protocol versions. Compare "
            "`Contract` with the client's supported contract.",
            "permission_denied": "Host policy refused — a path outside the workspace, a revision this "
            "deployment does not expose. The same request fails the same way.",
            "snapshot_not_found": "The revision does not resolve here: a deleted branch, a commit never "
            "fetched, a working tree that has been removed. Retrying helps only "
            "once the revision exists.",
            "revision_not_accepted": "The commit exists, but this session has no accepted publication for "
            "it. Publish its preview before asking `persist` to materialize "
            "it.",
            "semantic_snapshot_mismatch": "The snapshot passed back is no longer one the server can "
            "answer from — the working tree moved under it, or the "
            "adapter that held its facts was restarted. Re-read the "
            "current snapshot and rebuild whatever was pinned to the old "
            "one.",
            "semantic_snapshot_unavailable": "The revision resolves, but Rift has no index for it yet: "
            "the adapters are still analysing, or that state's facts "
            "were dropped. Retrying once indexing catches up succeeds.",
            "resource_not_found": "The URI is well-formed and resolves to nothing — no such symbol, no "
            "such file at that revision. Retrying does not help.",
            "record_reclaimed": "The record existed and has been vacuumed away after its retention "
            "window. It stays distinct from `resource_not_found` because a retry "
            "that read a silent miss would re-run work that already happened. "
            "Nothing brings it back.",
            "content_unavailable": "The entry is known but its bytes cannot be produced: an LFS object "
            "Rift does not fetch, or a blob the object store cannot read. "
            "Retrying does not help.",
            "cursor_invalid": "The cursor is malformed, or it was minted for a different request, order "
            "or page size. Start the query again from its first page.",
            "cursor_snapshot_mismatch": "The cursor is well-formed but the state it was minted against "
            "has moved, so continuing it would splice two different answers "
            "together. Start again against the current state.",
            "cancelled": "The caller cancelled, or the connection went away before the request "
            "finished. Sending it again is fine.",
            "deadline_exceeded": "The request ran past its time budget. A smaller one may succeed — a "
            "lower `limit`, a narrower path selector — and so may the same one "
            "once a cold compiler has warmed up.",
            "limit_exceeded": "A request or a response crossed an advertised limit. `limit` says which "
            "one and by how much, so the request can be resized and sent again.",
            "worktree_busy": "Another Rift process holds the lease on the working tree this request has "
            "to touch. Contention is transient, so the same request retries.",
            "adapter_unavailable": "No adapter is running for the language the request names: none is "
            "installed, or the one that was has died. A structural query against "
            "a language with no adapter lands here.",
            "adapter_protocol_error": "An adapter contract is unusable. Causes include a malformed "
            "message, a field outside its range, ambiguous source or virtual "
            "claims, overlapping write claims, a virtual path collision, and "
            "a cycle in virtual-source routing. Correct the adapter "
            "configuration before retrying.",
            "adapter_timeout": "The adapter took the call and did not answer inside its budget. "
            "Retrying can work: a cold compiler on a large workspace is slow once "
            "and fast afterwards.",
            "storage_failure": "Rift could not read or write its own state — the semantic database, the "
            "git object store, the socket directory. Worth retrying only if the "
            "cause was transient, such as a disk that has since been cleared.",
            "sandbox_failure": "The sandbox a caller-supplied check runs in could not be created, or "
            "died mid-run. One retry is reasonable; a repeat means the host cannot "
            "provide the sandbox this profile promised.",
            "internal_error": "A bug in Rift. `causes` says what it was doing at the time, and a retry "
            "is not expected to answer differently.",
        }
    },
)
class ErrorCode(str, Enum):
    """Why a request failed, as a stable code a caller branches on. The code is the complete classification. Domain results such as unsupported coverage and edit refusal use their typed result values."""

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
    SANDBOX_FAILURE = "sandbox_failure"
    INTERNAL_ERROR = "internal_error"


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.RetryDirective",
        "enum": "RetryDirective",
        "values": {
            "never": {"name": "RETRY_DIRECTIVE_NEVER", "number": 1},
            "same_request": {"name": "RETRY_DIRECTIVE_SAME_REQUEST", "number": 2},
            "same_idempotency_key": {
                "name": "RETRY_DIRECTIVE_SAME_IDEMPOTENCY_KEY",
                "number": 3,
            },
            "refresh_snapshot": {
                "name": "RETRY_DIRECTIVE_REFRESH_SNAPSHOT",
                "number": 4,
            },
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "never": "The request fails the same way every time. Change it or give up.",
            "same_request": "Send the same bytes again. The cause was transient — a busy working tree, "
            "an adapter still starting.",
            "same_idempotency_key": "Retry under the operation's existing key. Rift returns the "
            "retained outcome when the first call completed before its response "
            "was lost.",
            "refresh_snapshot": "The state moved under the request. Re-read the current state, rebuild "
            "whatever was pinned to the old one, then ask again.",
        }
    },
)
class RetryDirective(str, Enum):
    """Stable retry instruction for one failed request. `deadline_exceeded` can permit the same request; `invalid_request` requires changed input."""

    NEVER = "never"
    SAME_REQUEST = "same_request"
    SAME_IDEMPOTENCY_KEY = "same_idempotency_key"
    REFRESH_SNAPSHOT = "refresh_snapshot"


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.ErrorCause"}, schema_extra={}
)
class ErrorCause(ClosedModel):
    """One cause in a failure chain. Entries appear from the outer operation to the concrete failure."""

    code: ErrorCode = Field(
        description="How this link classifies on its own.",
        json_schema_extra={"rift:proto": {"field": "code", "number": 1}},
    )
    message: str = Field(
        description="What happened, for a human reading a log.",
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "message", "number": 2}},
    )
    retry: RetryDirective = Field(
        description="What could be done about this cause. The request's outer directive governs the call.",
        json_schema_extra={"rift:proto": {"field": "retry", "number": 3}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "phase",
        "number": 4,
        "enum": "Phase",
        "values": {
            "discovery": {"name": "DISCOVERY", "number": 1},
            "read": {"name": "READ", "number": 2},
            "resolve": {"name": "RESOLVE", "number": 3},
            "validate": {"name": "VALIDATE", "number": 4},
            "preview": {"name": "PREVIEW", "number": 5},
            "publish": {"name": "PUBLISH", "number": 6},
            "persist": {"name": "PERSIST", "number": 7},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "discovery": "Working out what the workspace can do: capabilities, limits, which languages "
            "have adapters.",
            "read": "Fetching what was asked for, from the index, the object store or an adapter.",
            "resolve": "Turning an address, a cursor or an action key into the concrete thing it names "
            "at a state.",
            "validate": "Checking a proposed change against the schema, the state it was pinned to, and "
            "any checks the caller supplied.",
            "preview": "Building a change into something readable without publishing it.",
            "publish": "Rerunning a retained preview and advancing the accepted ref through "
            "compare-and-swap.",
            "persist": "Materializing selected paths from an accepted commit into the session worktree.",
        }
    },
)
class ErrorDataPhase(str, Enum):
    """How far the request got before it failed. The same code means different things at different phases: `limit_exceeded` while reading is a response too big, and while validating it is a change set too large."""

    DISCOVERY = "discovery"
    READ = "read"
    RESOLVE = "resolve"
    VALIDATE = "validate"
    PREVIEW = "preview"
    PUBLISH = "publish"
    PERSIST = "persist"


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.ErrorData"}, schema_extra={}
)
class ErrorData(ClosedModel):
    """The `data` object on every Rift MCP failure. `code` and `retry` are what a caller branches on, `message` is for a human, and `phase`, `diagnostics`, `limit` and `causes` are the evidence behind the code."""

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
    code: ErrorCode = Field(
        description="Why the request failed.",
        json_schema_extra={"rift:proto": {"field": "code", "number": 1}},
    )
    message: str = Field(
        description="Human-readable account of the failure. Machine-readable classification remains in the surrounding fields.",
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "message", "number": 2}},
    )
    retry: RetryDirective = Field(
        description="What the caller may do next.",
        json_schema_extra={"rift:proto": {"field": "retry", "number": 3}},
    )
    phase: ErrorDataPhase = Field(
        description="How far the request got before it failed. The same code means different things at different phases: `limit_exceeded` while reading is a response too big, and while validating it is a change set too large.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "discovery": "Working out what the workspace can do: capabilities, limits, which languages "
                "have adapters.",
                "read": "Fetching what was asked for, from the index, the object store or an adapter.",
                "resolve": "Turning an address, a cursor or an action key into the concrete thing it names "
                "at a state.",
                "validate": "Checking a proposed change against the schema, the state it was pinned to, and "
                "any checks the caller supplied.",
                "preview": "Building a change into something readable without publishing it.",
                "publish": "Rerunning a retained preview and advancing the accepted ref through "
                "compare-and-swap.",
                "persist": "Materializing selected paths from an accepted commit into the session worktree.",
            },
            "rift:proto": {
                "field": "phase",
                "number": 4,
                "enum": "Phase",
                "values": {
                    "discovery": {"name": "DISCOVERY", "number": 1},
                    "read": {"name": "READ", "number": 2},
                    "resolve": {"name": "RESOLVE", "number": 3},
                    "validate": {"name": "VALIDATE", "number": 4},
                    "preview": {"name": "PREVIEW", "number": 5},
                    "publish": {"name": "PUBLISH", "number": 6},
                    "persist": {"name": "PERSIST", "number": 7},
                },
            },
        },
    )
    at: core.Snapshot | None = Field(
        description="The state this answer was resolved against. Null where the failure happened before one was resolved.",
        json_schema_extra={"rift:proto": {"field": "at", "number": 5}},
    )
    operation: int | None = Field(
        description="Which operation of a multi-operation request failed, as its zero-based index. Null where the request carried one, or failed before any of them ran.",
        json_schema_extra={"rift:proto": {"field": "operation", "number": 6}},
    )
    diagnostics: list[DiagnosticContext] = Field(
        description="What a compiler or a caller-supplied check reported while the request was failing. Empty where neither ran.",
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 7}},
    )
    limit: LimitEvidence | None = Field(
        default=None,
        description="Which advertised limit was hit, and by how much. Present exactly when `code` is `limit_exceeded`, and forbidden otherwise.",
        json_schema_extra={"rift:proto": {"field": "limit", "number": 8}},
    )
    causes: list[ErrorCause] = Field(
        description="What led to this failure, outermost first. A code alone rarely says whether the cause is worth waiting out.",
        json_schema_extra={"rift:proto": {"field": "causes", "number": 9}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "scope",
        "number": 1,
        "enum": "Scope",
        "values": {
            "driver": {"name": "DRIVER", "number": 1},
            "adapter": {"name": "ADAPTER", "number": 2},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "driver": "A field of `Limits`, advertised by the repository resource.",
            "adapter": "A field of the adapter's `AdapterLimits`, advertised in `Describe`.",
        }
    },
)
class LimitEvidenceScope(str, Enum):
    """Which side of the server the limit belongs to. The two fail at different seams: a `driver` limit is host policy the caller can work within, an `adapter` limit is one compiler process running out of room."""

    DRIVER = "driver"
    ADAPTER = "adapter"


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.LimitEvidence"}, schema_extra={}
)
class LimitEvidence(ClosedModel):
    """Which advertised limit a `limit_exceeded` failure hit, and by how much. It is present exactly when the code is `limit_exceeded`; without it, choosing between retrying smaller, falling back to another resource and giving up means reparsing the human message."""

    scope: LimitEvidenceScope = Field(
        description="Which side of the server the limit belongs to. The two fail at different seams: a `driver` limit is host policy the caller can work within, an `adapter` limit is one compiler process running out of room.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "driver": "A field of `Limits`, advertised by the repository resource.",
                "adapter": "A field of the adapter's `AdapterLimits`, advertised in `Describe`.",
            },
            "rift:proto": {
                "field": "scope",
                "number": 1,
                "enum": "Scope",
                "values": {
                    "driver": {"name": "DRIVER", "number": 1},
                    "adapter": {"name": "ADAPTER", "number": 2},
                },
            },
        },
    )
    field: str = Field(
        description="The limit's field name in whichever message `scope` names — `max_page_items`, `max_in_flight_per_workspace`.",
        min_length=1,
        max_length=128,
        json_schema_extra={"rift:proto": {"field": "field", "number": 2}},
    )
    limit: int = Field(
        description="The value in force when the request was rejected.",
        ge=0,
        le=9007199254740991,
        json_schema_extra={"rift:proto": {"field": "limit", "number": 3}},
    )
    required: int = Field(
        description="What the request would have needed. Larger than `limit`, and the difference is what the caller has to close.",
        ge=0,
        le=9007199254740991,
        json_schema_extra={"rift:proto": {"field": "required", "number": 4}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"field": "text", "number": 1, "type": "rift.mcp.MatchSyntaxText"},
    schema_extra={},
)
class MatchSyntaxText(ClosedModel):
    """The grammar a `TextQuery` pattern is read in."""

    name: Literal["rift-regex"] = Field(
        json_schema_extra={"rift:proto": {"field": "name", "number": 1}}
    )
    version: Literal[1] = Field(
        json_schema_extra={"rift:proto": {"field": "version", "number": 2}}
    )


@definition(
    owner="mcp",
    public=False,
    proto={"field": "path", "number": 2, "type": "rift.mcp.MatchSyntaxPath"},
    schema_extra={},
)
class MatchSyntaxPath(ClosedModel):
    """The grammar a path selector is read in: Git globs, so `src/**/*.ts` means here what it means in `.gitignore`."""

    name: Literal["git-glob"] = Field(
        json_schema_extra={"rift:proto": {"field": "name", "number": 1}}
    )
    version: Literal[1] = Field(
        json_schema_extra={"rift:proto": {"field": "version", "number": 2}}
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.MatchSyntax"}, schema_extra={}
)
class MatchSyntax(ClosedModel):
    """The two pattern grammars this workspace accepts: one for text patterns and one for path globs. Names remain stable; their version fields select syntax and matching semantics."""

    text: MatchSyntaxText = Field(
        description="The grammar a `TextQuery` pattern is read in.",
        json_schema_extra={
            "rift:proto": {
                "field": "text",
                "number": 1,
                "type": "rift.mcp.MatchSyntaxText",
            }
        },
    )
    path: MatchSyntaxPath = Field(
        description="The grammar a path selector is read in: Git globs, so `src/**/*.ts` means here what it means in `.gitignore`.",
        json_schema_extra={
            "rift:proto": {
                "field": "path",
                "number": 2,
                "type": "rift.mcp.MatchSyntaxPath",
            }
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.RepositoryResourceTemplate"},
    schema_extra={},
)
class RepositoryResourceTemplate(ClosedModel):
    """The repository resource. It takes no path, only an optional revision and cursor."""

    uriTemplate: Literal["rift://repository{?rev,cursor}"] = Field(
        description="The template, in RFC 6570 form. What follows `?` is optional."
    )
    name: Literal["repository"] = Field(
        description="The resource family, as `resources/templates/list` advertises it.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 1}},
    )
    mimeType: Literal["application/vnd.rift.repository+json"] = Field(
        description="What a read of a URI from this template returns: `RepositoryResourcePayload` as JSON.",
        json_schema_extra={"rift:proto": {"field": "mime_type", "number": 2}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.SymbolResourceTemplate"},
    schema_extra={},
)
class SymbolResourceTemplate(ClosedModel):
    """The symbol resource, addressed by language and the name that language gives the declaration."""

    uriTemplate: Literal["rift://symbol/{language}/{name}{?rev,cursor}"] = Field(
        description="The template, in RFC 6570 form. What follows `?` is optional."
    )
    name: Literal["symbol"] = Field(
        description="The resource family, as `resources/templates/list` advertises it.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 1}},
    )
    mimeType: Literal["application/vnd.rift.symbol+json"] = Field(
        description="What a read of a URI from this template returns: `SymbolResourcePayload` as JSON.",
        json_schema_extra={"rift:proto": {"field": "mime_type", "number": 2}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.DiffResourceTemplate"},
    schema_extra={},
)
class DiffResourceTemplate(ClosedModel):
    """The diff resource, addressed by two revisions in git's own range spelling."""

    uriTemplate: Literal["rift://diff/{from}..{to}{?cursor}"] = Field(
        description="The template, in RFC 6570 form. What follows `?` is optional."
    )
    name: Literal["diff"] = Field(
        description="The resource family, as `resources/templates/list` advertises it.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 1}},
    )
    mimeType: Literal["application/vnd.rift.diff+json"] = Field(
        description="What a read of a URI from this template returns: `DiffResourcePayload` as JSON.",
        json_schema_extra={"rift:proto": {"field": "mime_type", "number": 2}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.FileResourceTemplate"},
    schema_extra={},
)
class FileResourceTemplate(ClosedModel):
    """The file resource, addressed by a path relative to the project root."""

    uriTemplate: Literal["rift://file/{path}{?rev,start,length}"] = Field(
        description="The template, in RFC 6570 form. What follows `?` is optional."
    )
    name: Literal["file"] = Field(
        description="The resource family, as `resources/templates/list` advertises it.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 1}},
    )
    mimeType: Literal["application/vnd.rift.file+json"] = Field(
        description="What a read of a URI from this template returns: `FileResourcePayload` as JSON.",
        json_schema_extra={"rift:proto": {"field": "mime_type", "number": 2}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.PreviewResourceTemplate"},
    schema_extra={},
)
class PreviewResourceTemplate(ClosedModel):
    """The retained preview resource, addressed by its opaque id and continued by an optional cursor."""

    uriTemplate: Literal["rift://preview/{id}{?cursor}"] = Field(
        description="The template, in RFC 6570 form. The cursor is optional."
    )
    name: Literal["preview"] = Field(
        description="The resource family, as `resources/templates/list` advertises it.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 1}},
    )
    mimeType: Literal["application/vnd.rift.preview+json"] = Field(
        description="What a read of a URI from this template returns: `PreviewResourcePayload` as JSON.",
        json_schema_extra={"rift:proto": {"field": "mime_type", "number": 2}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.ResourceTemplate",
        "oneof": "variant",
        "variants": [
            {
                "tag": "rift://repository{?rev,cursor}",
                "field": "repository",
                "number": 1,
                "type": "RepositoryResourceTemplate",
            },
            {
                "tag": "rift://symbol/{language}/{name}{?rev,cursor}",
                "field": "symbol",
                "number": 2,
                "type": "SymbolResourceTemplate",
            },
            {
                "tag": "rift://diff/{from}..{to}{?cursor}",
                "field": "diff",
                "number": 3,
                "type": "DiffResourceTemplate",
            },
            {
                "tag": "rift://file/{path}{?rev,start,length}",
                "field": "file",
                "number": 4,
                "type": "FileResourceTemplate",
            },
            {
                "tag": "rift://preview/{id}{?cursor}",
                "field": "preview",
                "number": 5,
                "type": "PreviewResourceTemplate",
            },
        ],
    },
    schema_extra={},
)
class ResourceTemplate(
    ProtocolRoot[
        "Annotated[RepositoryResourceTemplate | SymbolResourceTemplate | DiffResourceTemplate | FileResourceTemplate | PreviewResourceTemplate, Field(discriminator='mimeType')]"
    ]
):
    """One advertised MCP resource template. uriTemplate, name, and mimeType are correlated per family."""


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.ResourceReadParams"},
    schema_extra={},
)
class ResourceReadParams(ClosedModel):
    """The URI passed to MCP `resources/read`. Each branch is one advertised Rift resource family."""

    uri: (
        RepositoryResourceUri
        | core.SymbolId
        | core.DiffId
        | FileResourceUri
        | PreviewResourceUri
    ) = Field(
        description="A URI matching one branch of `ResourceTemplate`.",
        json_schema_extra={
            "rift:proto": {"field": "uri", "number": 1, "scalar": "string"},
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.RepositoryResourceContent"},
    schema_extra={},
)
class RepositoryResourceContent(ClosedModel):
    """What a read of `rift://repository` returns."""

    uri: RepositoryResourceUri = Field(
        description="The URI that was read, as it resolved.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    mimeType: Literal["application/vnd.rift.repository+json"] = Field(
        description="Which payload `text` holds."
    )
    text: str = Field(
        description="A `RepositoryResourcePayload`, serialized as JSON.",
        json_schema_extra={
            "rift:proto": {"field": "text", "number": 2},
            "contentMediaType": "application/vnd.rift.repository+json",
            "rift:contentType": "RepositoryResourcePayload",
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.SymbolResourceContent"},
    schema_extra={},
)
class SymbolResourceContent(ClosedModel):
    """What a read of a `rift://symbol/…` URI returns."""

    uri: core.SymbolId = Field(
        description="The URI that was read, as it resolved.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    mimeType: Literal["application/vnd.rift.symbol+json"] = Field(
        description="Which payload `text` holds."
    )
    text: str = Field(
        description="A `SymbolResourcePayload`, serialized as JSON.",
        json_schema_extra={
            "rift:proto": {"field": "text", "number": 2},
            "contentMediaType": "application/vnd.rift.symbol+json",
            "rift:contentType": "SymbolResourcePayload",
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.DiffResourceContent"},
    schema_extra={},
)
class DiffResourceContent(ClosedModel):
    """What a read of a `rift://diff/…` URI returns."""

    uri: core.DiffId = Field(
        description="The URI that was read, as it resolved.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    mimeType: Literal["application/vnd.rift.diff+json"] = Field(
        description="Which payload `text` holds."
    )
    text: str = Field(
        description="A `DiffResourcePayload`, serialized as JSON.",
        json_schema_extra={
            "rift:proto": {"field": "text", "number": 2},
            "contentMediaType": "application/vnd.rift.diff+json",
            "rift:contentType": "DiffResourcePayload",
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.FileResourceContent"},
    schema_extra={},
)
class FileResourceContent(ClosedModel):
    """What a read of a `rift://file/…` URI returns."""

    uri: FileResourceUri = Field(
        description="The URI that was read, as it resolved.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    mimeType: Literal["application/vnd.rift.file+json"] = Field(
        description="Which payload `text` holds."
    )
    text: str = Field(
        description="A `FileResourcePayload`, serialized as JSON.",
        json_schema_extra={
            "rift:proto": {"field": "text", "number": 2},
            "contentMediaType": "application/vnd.rift.file+json",
            "rift:contentType": "FileResourcePayload",
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.PreviewResourceContent"},
    schema_extra={},
)
class PreviewResourceContent(ClosedModel):
    """What a read of a `rift://preview/…` URI returns."""

    uri: PreviewResourceUri = Field(
        description="The preview page that was read.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    mimeType: Literal["application/vnd.rift.preview+json"] = Field(
        description="Which payload `text` holds."
    )
    text: str = Field(
        description="A `PreviewResourcePayload`, serialized as JSON.",
        json_schema_extra={
            "rift:proto": {"field": "text", "number": 2},
            "contentMediaType": "application/vnd.rift.preview+json",
            "rift:contentType": "PreviewResourcePayload",
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "type": "rift.mcp.ResourceContent",
        "oneof": "variant",
        "variants": [
            {
                "tag": "application/vnd.rift.repository+json",
                "field": "repository",
                "number": 1,
                "type": "RepositoryResourceContent",
            },
            {
                "tag": "application/vnd.rift.symbol+json",
                "field": "symbol",
                "number": 2,
                "type": "SymbolResourceContent",
            },
            {
                "tag": "application/vnd.rift.diff+json",
                "field": "diff",
                "number": 3,
                "type": "DiffResourceContent",
            },
            {
                "tag": "application/vnd.rift.file+json",
                "field": "file",
                "number": 4,
                "type": "FileResourceContent",
            },
            {
                "tag": "application/vnd.rift.preview+json",
                "field": "preview",
                "number": 5,
                "type": "PreviewResourceContent",
            },
        ],
    },
    schema_extra={},
)
class ResourceContent(
    ProtocolRoot[
        "Annotated[RepositoryResourceContent | SymbolResourceContent | DiffResourceContent | FileResourceContent | PreviewResourceContent, Field(discriminator='mimeType')]"
    ]
):
    pass


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.ResourceReadResult"},
    schema_extra={},
)
class ResourceReadResult(ClosedModel):
    """The result of one MCP resource read. Each Rift resource returns one JSON payload in `text`, identified by `mimeType`. File bytes use UTF-8 or base64."""

    contents: list[ResourceContent] = Field(
        description="The blocks this read produced. MCP allows several per read; each Rift resource returns one.",
        min_length=1,
        json_schema_extra={"rift:proto": {"field": "contents", "number": 1}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.SearchHitTargetSymbol"},
    schema_extra={},
)
class SearchHitTargetSymbol(ClosedModel):
    """A symbol hit: the declaration the compiler resolved, and a link to read the rest of what Rift knows about it."""

    target: Literal["symbol"] = Field(description="Tags this as a symbol hit.")
    symbol: core.Symbol = Field(
        description="The declaration that matched.",
        json_schema_extra={"rift:proto": {"field": "symbol", "number": 1}},
    )
    resource: ResourceLink = Field(
        description="Link to the symbol resource that carries this symbol's leaves and relationships.",
        json_schema_extra={"rift:proto": {"field": "resource", "number": 2}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.SearchHitTargetLeaf"},
    schema_extra={},
)
class SearchHitTargetLeaf(ClosedModel):
    """A leaf hit: one place in a syntax tree, without the symbol view around it."""

    target: Literal["leaf"] = Field(description="Tags this as a leaf hit.")
    leaf: core.Leaf = Field(
        description="The syntax-tree node that matched, and the symbol written at it where there is one.",
        json_schema_extra={"rift:proto": {"field": "leaf", "number": 1}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.SearchHitTargetFile"},
    schema_extra={},
)
class SearchHitTargetFile(ClosedModel):
    """A file hit: one entry of the workspace tree. The only hit that needs no adapter."""

    target: Literal["file"] = Field(description="Tags this as a file hit.")
    file: core.File = Field(
        description="The tree entry that matched: what it holds, and which languages read it.",
        json_schema_extra={"rift:proto": {"field": "file", "number": 1}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.SearchHitTarget",
        "oneof": "variant",
        "variants": [
            {
                "tag": "symbol",
                "field": "symbol",
                "number": 1,
                "type": "SearchHitTargetSymbol",
            },
            {
                "tag": "leaf",
                "field": "leaf",
                "number": 2,
                "type": "SearchHitTargetLeaf",
            },
            {
                "tag": "file",
                "field": "file",
                "number": 3,
                "type": "SearchHitTargetFile",
            },
        ],
    },
    schema_extra={},
)
class SearchHitTarget(
    ProtocolRoot[
        "Annotated[SearchHitTargetSymbol | SearchHitTargetLeaf | SearchHitTargetFile, Field(discriminator='target')]"
    ]
):
    """What a search hit is. Tagged, so the payload correlation survives code generation."""


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.Change",
        "oneof": "variant",
        "variants": [
            {
                "tag": None,
                "field": "direct_change",
                "number": 1,
                "type": "DirectChange",
            },
            {"tag": None, "field": "patch_change", "number": 2, "type": "PatchChange"},
            {
                "tag": None,
                "field": "action_change",
                "number": 3,
                "type": "ActionChange",
            },
            {
                "tag": None,
                "field": "rewrite_change",
                "number": 4,
                "type": "RewriteChange",
            },
            {
                "tag": None,
                "field": "revert_change",
                "number": 5,
                "type": "RevertChange",
            },
        ],
    },
    schema_extra={},
)
class Change(
    ProtocolRoot[
        "Annotated[DirectChange | PatchChange | ActionChange | RewriteChange | RevertChange, Field(discriminator='kind')]"
    ]
):
    """One deterministic contribution to a candidate. Rift resolves the ordered array against one pinned state. After each change, it applies formatting and makes every adapter sharing the worktree acknowledge the new snapshot before resolving the next; any refusal discards the candidate. Product model components convert prompts and provider completions into these values before calling Rift."""


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.DirectChange"}, schema_extra={}
)
class DirectChange(ClosedModel):
    """Concrete filesystem edits supplied by the caller. Their ranges address the candidate state at this point in the ordered change list."""

    kind: Literal["edits"] = Field(
        description="Tags this as a concrete edit list.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}},
    )
    edits: list[core.Edit] = Field(
        description="An atomic effect set in canonical file-and-range order. Every text replacement addresses the state before this change, and replacements may not overlap.",
        min_length=1,
        json_schema_extra={"rift:proto": {"field": "edits", "number": 2}},
    )
    formatting: core.FormattingPolicy = Field(
        description="Formatting applied after these edits resolve.",
        json_schema_extra={"rift:proto": {"field": "formatting", "number": 3}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.PatchChange"}, schema_extra={}
)
class PatchChange(ClosedModel):
    """A UTF-8 unified diff guarded by its context lines. Rift refuses absolute paths, path traversal, binary patches, malformed headers, and any hunk whose context differs from the candidate state."""

    kind: Literal["patch"] = Field(
        description="Tags this as a unified diff.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}},
    )
    patch: str = Field(
        description="Unified diff in Git's text patch syntax, with project-relative `a/` and `b/` paths.",
        min_length=1,
        json_schema_extra={"rift:proto": {"field": "patch", "number": 2}},
    )
    formatting: core.FormattingPolicy = Field(
        description="Formatting applied after every hunk resolves.",
        json_schema_extra={"rift:proto": {"field": "formatting", "number": 3}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.ActionChange"}, schema_extra={}
)
class ActionChange(ClosedModel):
    """One compiler action selected from `actions`. Rift validates `arguments` against the advertised schema and resolves the token. Refresh and publish rediscover at the descriptor's target. Kind, target, argument contract, argument schema, and guarantees select the same action. Zero or several matches cause a refusal."""

    kind: Literal["action"] = Field(
        description="Tags this as a compiler action.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}},
    )
    action: ActionOffer = Field(
        description="The descriptor used for replay and the key used for initial resolution.",
        json_schema_extra={"rift:proto": {"field": "action", "number": 2}},
    )
    arguments: dict[str, Any] = Field(
        description="Arguments accepted by the offer's `ActionDescriptor.arguments_schema`. An action with no parameters receives an empty object.",
        json_schema_extra={"rift:proto": {"field": "arguments", "number": 3}},
    )
    formatting: core.FormattingPolicy = Field(
        description="Formatting applied after the action's edits resolve.",
        json_schema_extra={"rift:proto": {"field": "formatting", "number": 4}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "range",
        "number": 4,
        "enum": "Range",
        "values": {
            "exact": {"name": "EXACT", "number": 1},
            "leading": {"name": "LEADING", "number": 2},
            "trailing": {"name": "TRAILING", "number": 3},
            "both": {"name": "BOTH", "number": 4},
        },
    },
    schema_extra={},
)
class RewriteChangeRange(str, Enum):
    """Which safe structural range is replaced. Text queries accept `exact` only because they have no grammar-owned trivia boundaries."""

    EXACT = "exact"
    LEADING = "leading"
    TRAILING = "trailing"
    BOTH = "both"


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.RewriteChange"}, schema_extra={}
)
class RewriteChange(ClosedModel):
    """An atomic query-and-rewrite over the candidate state. Rift finds every match, checks the cardinality, expands the replacement, and either applies all resulting edits or refuses the candidate."""

    kind: Literal["rewrite"] = Field(
        description="Tags this as an atomic match rewrite.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}},
    )
    query: core.MatchQuery = Field(
        description="The text or structural pattern evaluated at this point in the change list.",
        json_schema_extra={"rift:proto": {"field": "query", "number": 2}},
    )
    replacement: str = Field(
        description="UTF-8 replacement template. `${NAME}` inserts the source bound by a named or numeric capture, `${0}` inserts the whole match, and `$$` inserts one dollar sign. An absent capture refuses the rewrite.",
        json_schema_extra={"rift:proto": {"field": "replacement", "number": 3}},
    )
    range: RewriteChangeRange = Field(
        description="Which safe structural range is replaced. Text queries accept `exact` only because they have no grammar-owned trivia boundaries.",
        json_schema_extra={
            "rift:proto": {
                "field": "range",
                "number": 4,
                "enum": "Range",
                "values": {
                    "exact": {"name": "EXACT", "number": 1},
                    "leading": {"name": "LEADING", "number": 2},
                    "trailing": {"name": "TRAILING", "number": 3},
                    "both": {"name": "BOTH", "number": 4},
                },
            }
        },
    )
    cardinality: core.MatchCardinality = Field(
        description="The accepted number of matches before expansion.",
        json_schema_extra={"rift:proto": {"field": "cardinality", "number": 5}},
    )
    formatting: core.FormattingPolicy = Field(
        description="Formatting applied after every replacement resolves.",
        json_schema_extra={"rift:proto": {"field": "formatting", "number": 6}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.RevertChange"}, schema_extra={}
)
class RevertChange(ClosedModel):
    """A validated three-way inverse of one commit. Rift computes the difference from `parent` to `revision`, applies its inverse to the candidate state, and refuses overlapping changes it cannot merge without guessing."""

    kind: Literal["revert"] = Field(
        description="Tags this as a revision revert.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}},
    )
    revision: core.Commit = Field(
        description="Exact commit whose changes are inverted.",
        json_schema_extra={"rift:proto": {"field": "revision", "number": 2}},
    )
    parent: core.Commit | None = Field(
        description="Parent against which the commit's change is defined. Required for ordinary and merge commits; null selects the empty tree for a root commit. A commit that does not have this parent is refused.",
        json_schema_extra={"rift:proto": {"field": "parent", "number": 3}},
    )
    paths: core.PathSelector = Field(
        description="Paths from the original commit eligible for inversion. Excluded paths remain untouched; the commit diff exposes them when the caller needs to inspect the omission.",
        json_schema_extra={"rift:proto": {"field": "paths", "number": 4}},
    )
    formatting: core.FormattingPolicy = Field(
        description="Formatting applied after the inverse edits resolve.",
        json_schema_extra={"rift:proto": {"field": "formatting", "number": 5}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.ResolvedChange"}, schema_extra={}
)
class ResolvedChange(ClosedModel):
    """Bounded summary of one requested change after resolution. The preview resource pages its exact edits and evidence as records carrying this change index."""

    index: int = Field(
        description="Zero-based position in the ordered request.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "index", "number": 1}},
    )
    owners: list[core.LanguageId] = Field(
        description="Language adapters that contributed to resolution, sorted by language. Empty for a change resolved entirely by Rift.",
        json_schema_extra={
            "rift:proto": {"field": "owners", "number": 2},
            "uniqueItems": True,
        },
    )
    edit_count: int = Field(
        description="Number of concrete Edit records retained for this change.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "edit_count", "number": 3}},
    )
    precondition_count: int = Field(
        description="Number of satisfied preconditions retained for this change.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "precondition_count", "number": 4}},
    )
    effect_count: int = Field(
        description="Number of semantic effects retained for this change.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "effect_count", "number": 5}},
    )
    guarantee_count: int = Field(
        description="Number of guarantee evidence records retained for this change.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "guarantee_count", "number": 6}},
    )
    coverage: core.Coverage = Field(
        description="How completely Rift and its adapters resolved the request. Publication requires complete coverage.",
        json_schema_extra={"rift:proto": {"field": "coverage", "number": 7}},
    )
    diagnostic_count: int = Field(
        description="Number of resolution diagnostics retained for this change.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "diagnostic_count", "number": 8}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "kind",
        "number": 2,
        "enum": "Kind",
        "values": {
            "test": {"name": "KIND_TEST", "number": 1},
            "lint": {"name": "KIND_LINT", "number": 2},
            "build": {"name": "KIND_BUILD", "number": 3},
            "other": {"name": "KIND_OTHER", "number": 4},
        },
    },
    schema_extra={},
)
class SandboxedValidatorKind(str, Enum):
    """How the caller presents this check."""

    TEST = "test"
    LINT = "lint"
    BUILD = "build"
    OTHER = "other"


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "changed_paths",
        "number": 4,
        "enum": "ChangedPaths",
        "values": {
            "none": {"name": "CHANGED_PATHS_NONE", "number": 1},
            "append": {"name": "CHANGED_PATHS_APPEND", "number": 2},
        },
    },
    schema_extra={},
)
class SandboxedValidatorChangedPaths(str, Enum):
    """Whether Rift appends the candidate's changed `ProjectPath` values to `argv` in byte order."""

    NONE = "none"
    APPEND = "append"


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.SandboxedValidatorGuarantees"},
    schema_extra={},
)
class SandboxedValidatorGuarantees(ClosedModel):
    changes: list[int] = Field(
        description="Requested change indexes to which this evidence applies.",
        min_length=1,
        json_schema_extra={
            "rift:proto": {"field": "changes", "number": 1},
            "uniqueItems": True,
        },
    )
    kind: core.GuaranteeKind = Field(
        description="Guarantee established when the validator passes.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 2}},
    )
    scope: core.CoverageScope = Field(
        description="Source over which the command checks the property.",
        json_schema_extra={"rift:proto": {"field": "scope", "number": 3}},
    )
    detail: str = Field(
        description="Exact property the command checks and limits on interpreting a pass.",
        min_length=1,
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "detail", "number": 4}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "determinism",
        "number": 10,
        "enum": "Determinism",
        "values": {
            "deterministic": {"name": "DETERMINISM_DETERMINISTIC", "number": 1},
            "best_effort": {"name": "DETERMINISM_BEST_EFFORT", "number": 2},
        },
    },
    schema_extra={},
)
class SandboxedValidatorDeterminism(str, Enum):
    """Whether an identical candidate and environment are expected to produce the same result."""

    DETERMINISTIC = "deterministic"
    BEST_EFFORT = "best_effort"


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.SandboxedValidator"},
    schema_extra={},
)
class SandboxedValidator(ClosedModel):
    """A caller-authorized acceptance check executed directly, without a shell, in a disposable copy of the complete candidate tree. The sandbox denies network access, mounts external dependencies read-only, and discards every write when the check ends."""

    id: str = Field(
        description="Caller label shown with this validator. The result links to the complete declaration by digest, so labels may repeat.",
        pattern="^[A-Za-z][A-Za-z0-9_.-]{0,127}$",
        json_schema_extra={"rift:proto": {"field": "id", "number": 1}},
    )
    kind: SandboxedValidatorKind = Field(
        description="How the caller presents this check.",
        json_schema_extra={
            "rift:proto": {
                "field": "kind",
                "number": 2,
                "enum": "Kind",
                "values": {
                    "test": {"name": "KIND_TEST", "number": 1},
                    "lint": {"name": "KIND_LINT", "number": 2},
                    "build": {"name": "KIND_BUILD", "number": 3},
                    "other": {"name": "KIND_OTHER", "number": 4},
                },
            }
        },
    )
    argv: list[str] = Field(
        description="Executable followed by literal arguments. An absolute executable path is refused; a bare name resolves through the sandbox PATH and a relative path resolves below `working_directory`. Rift performs no shell expansion.",
        min_length=1,
        json_schema_extra={"rift:proto": {"field": "argv", "number": 3}},
    )
    changed_paths: SandboxedValidatorChangedPaths = Field(
        description="Whether Rift appends the candidate's changed `ProjectPath` values to `argv` in byte order.",
        json_schema_extra={
            "rift:proto": {
                "field": "changed_paths",
                "number": 4,
                "enum": "ChangedPaths",
                "values": {
                    "none": {"name": "CHANGED_PATHS_NONE", "number": 1},
                    "append": {"name": "CHANGED_PATHS_APPEND", "number": 2},
                },
            }
        },
    )
    working_directory: core.ProjectPath = Field(
        description="Directory below the project root in which the process starts. The empty path selects the root.",
        json_schema_extra={"rift:proto": {"field": "working_directory", "number": 5}},
    )
    environment: dict[str, str] = Field(
        description="Caller-supplied environment additions. Rift supplies a policy-controlled PATH, private HOME and temporary directories, and a UTF-8 locale; it removes host secrets and every other inherited variable.",
        json_schema_extra={
            "rift:proto": {"field": "environment", "number": 6},
            "propertyNames": {"pattern": "^[A-Za-z_][A-Za-z0-9_]*$"},
        },
    )
    timeout_ms: int = Field(
        description="Wall-clock limit before Rift terminates the process.",
        ge=1,
        le=3600000,
        json_schema_extra={"rift:proto": {"field": "timeout_ms", "number": 7}},
    )
    output_limit_bytes: int = Field(
        description="Captured prefix limit for each output stream. `ValidatorOutput.total_bytes` reports the omitted size. The upper bound keeps one escaped validator result inside a 65536-byte preview page.",
        ge=256,
        le=4096,
        json_schema_extra={"rift:proto": {"field": "output_limit_bytes", "number": 8}},
    )
    guarantees: list[SandboxedValidatorGuarantees] = Field(
        description="Behavior or other properties this command is intended to check. A passing result turns each declaration into `GuaranteeEvidence`; a failed result rejects publication.",
        json_schema_extra={"rift:proto": {"field": "guarantees", "number": 9}},
    )
    determinism: SandboxedValidatorDeterminism = Field(
        description="Whether an identical candidate and environment are expected to produce the same result.",
        json_schema_extra={
            "rift:proto": {
                "field": "determinism",
                "number": 10,
                "enum": "Determinism",
                "values": {
                    "deterministic": {"name": "DETERMINISM_DETERMINISTIC", "number": 1},
                    "best_effort": {"name": "DETERMINISM_BEST_EFFORT", "number": 2},
                },
            }
        },
    )
    network: Literal["denied"] = Field(
        description="Network policy enforced by the sandbox.",
        json_schema_extra={"rift:proto": {"field": "network", "number": 11}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.ValidatorOutput"},
    schema_extra={},
)
class ValidatorOutput(ClosedModel):
    """A bounded UTF-8 rendering of one process stream. Invalid byte sequences become U+FFFD. The digest covers the complete raw stream, including bytes beyond the captured prefix."""

    text: str = Field(
        description="Decoded captured prefix.",
        json_schema_extra={"rift:proto": {"field": "text", "number": 1}},
    )
    captured_bytes: int = Field(
        description="Raw bytes represented by `text` before replacement decoding.",
        ge=0,
        json_schema_extra={"rift:proto": {"field": "captured_bytes", "number": 2}},
    )
    total_bytes: int = Field(
        description="Raw bytes emitted by the complete stream.",
        ge=0,
        json_schema_extra={"rift:proto": {"field": "total_bytes", "number": 3}},
    )
    truncated: bool = Field(
        description="Whether bytes after the captured prefix were omitted from `text`.",
        json_schema_extra={"rift:proto": {"field": "truncated", "number": 4}},
    )
    digest: core.Digest = Field(
        description="SHA-256 of the complete raw stream. This retains the identity of omitted bytes when `truncated` is true.",
        json_schema_extra={"rift:proto": {"field": "digest", "number": 5}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.ValidatorResultPassed"},
    schema_extra={},
)
class ValidatorResultPassed(ClosedModel):
    """A declared validator process that exited with status zero."""

    status: Literal["passed"] = Field(default="passed")
    exit_code: Literal[0] = Field(
        default=0,
        json_schema_extra={"rift:proto": {"field": "exit_code", "number": 1}},
    )
    declaration_digest: core.Digest = Field(
        description="SHA-256 of the validator's RFC 8785 canonical JSON declaration. Results and declarations form a bijection on this value, because labels may repeat while commands differ.",
        json_schema_extra={"rift:proto": {"field": "declaration_digest", "number": 2}},
    )
    files: list[core.ProjectPath] = Field(
        description="Paths evaluated by the validator, sorted by UTF-8 bytes and without duplicates.",
        json_schema_extra={
            "rift:proto": {"field": "files", "number": 3},
            "uniqueItems": True,
        },
    )
    diagnostics: list[core.Diagnostic] = Field(
        description="Structured findings produced by the validator. The list is empty when its output has no configured diagnostic decoder.",
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 4}},
    )
    stdout: ValidatorOutput = Field(
        description="Bounded standard output from the process.",
        json_schema_extra={"rift:proto": {"field": "stdout", "number": 5}},
    )
    stderr: ValidatorOutput = Field(
        description="Bounded standard error from the process.",
        json_schema_extra={"rift:proto": {"field": "stderr", "number": 6}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.ValidatorResultFailed"},
    schema_extra={},
)
class ValidatorResultFailed(ClosedModel):
    """A declared validator process that exited with a nonzero status."""

    status: Literal["failed"] = Field(default="failed")
    exit_code: int = Field(
        json_schema_extra={
            "rift:proto": {"field": "exit_code", "number": 1},
            "not": {"const": 0},
        },
    )
    declaration_digest: core.Digest = Field(
        description="SHA-256 of the validator's RFC 8785 canonical JSON declaration. Results and declarations form a bijection on this value, because labels may repeat while commands differ.",
        json_schema_extra={"rift:proto": {"field": "declaration_digest", "number": 2}},
    )
    files: list[core.ProjectPath] = Field(
        description="Paths evaluated by the validator, sorted by UTF-8 bytes and without duplicates.",
        json_schema_extra={
            "rift:proto": {"field": "files", "number": 3},
            "uniqueItems": True,
        },
    )
    diagnostics: list[core.Diagnostic] = Field(
        description="Structured findings produced by the validator. The list is empty when its output has no configured diagnostic decoder.",
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 4}},
    )
    stdout: ValidatorOutput = Field(
        description="Bounded standard output from the process.",
        json_schema_extra={"rift:proto": {"field": "stdout", "number": 5}},
    )
    stderr: ValidatorOutput = Field(
        description="Bounded standard error from the process.",
        json_schema_extra={"rift:proto": {"field": "stderr", "number": 6}},
    )

    @field_validator("exit_code")
    @classmethod
    def nonzero_exit_code(cls, value: int) -> int:
        if value == 0:
            raise ValueError("failed validator result requires a nonzero exit code")
        return value


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.ValidatorResult",
        "oneof": "variant",
        "variants": [
            {
                "tag": "passed",
                "field": "passed",
                "number": 1,
                "type": "ValidatorResultPassed",
            },
            {
                "tag": "failed",
                "field": "failed",
                "number": 2,
                "type": "ValidatorResultFailed",
            },
        ],
    },
    schema_extra={},
)
class ValidatorResult(
    ProtocolRoot[
        "Annotated[ValidatorResultPassed | ValidatorResultFailed, Field(discriminator='status')]"
    ]
):
    """The completed outcome of one declared validator. Exit status zero passes. Every other exit status fails. A launch, timeout, sandbox, or capture failure raises `sandbox_failure` before Rift produces candidate evidence."""


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.CandidateValidation"},
    schema_extra={},
)
class CandidateValidation(ClosedModel):
    """Bounded verdict over a candidate's compiler reports and sandbox validators. The preview resource carries the complete paginated evidence."""

    complete: bool = Field(
        description="Whether every affected language returned complete compiler coverage and every declared validator produced a result.",
        json_schema_extra={"rift:proto": {"field": "complete", "number": 1}},
    )
    valid: bool = Field(
        description="True when `complete` is true, every compiler report is valid, and every validator result passed. Publication requires true.",
        json_schema_extra={"rift:proto": {"field": "valid", "number": 2}},
    )
    adapter_reports: int = Field(
        description="Number of compiler reports retained by the preview.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "adapter_reports", "number": 3}},
    )
    validator_results: int = Field(
        description="Number of sandbox validator results retained by the preview.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "validator_results", "number": 4}},
    )
    validators_passed: int = Field(
        description="Number of retained validator results with `passed` status.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "validators_passed", "number": 5}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.CandidateSummary"},
    schema_extra={},
)
class CandidateSummary(ClosedModel):
    """The bounded identity and acceptance evidence returned by `apply`. The linked preview resource carries the complete requested and resolved plan."""

    preview: core.PreviewId = Field(
        description="The retained plan's stable identity.",
        json_schema_extra={"rift:proto": {"field": "preview", "number": 1}},
    )
    base: core.Snapshot = Field(
        description="The state against which the first change was resolved.",
        json_schema_extra={"rift:proto": {"field": "base", "number": 2}},
    )
    candidate: core.Commit = Field(
        description="The immutable Git commit containing every resolved edit. It remains outside the accepted ref until publication succeeds.",
        json_schema_extra={"rift:proto": {"field": "candidate", "number": 3}},
    )
    resource: PreviewResourceLink = Field(
        description="Link to the complete retained plan.",
        json_schema_extra={"rift:proto": {"field": "resource", "number": 4}},
    )
    validation: CandidateValidation = Field(
        description="Bounded verdict and evidence counts for this candidate.",
        json_schema_extra={"rift:proto": {"field": "validation", "number": 5}},
    )
    confirmation_count: int = Field(
        description="Number of acknowledgement requirements retained in the preview resource.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "confirmation_count", "number": 6}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.PreviewResourcePayloadEdits"},
    schema_extra={},
)
class PreviewResourcePayloadEdits(ClosedModel):
    change: int = Field(
        description="Requested change index that produced this edit.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "change", "number": 1}},
    )
    edit: core.Edit = Field(
        description="Concrete filesystem effect.",
        json_schema_extra={"rift:proto": {"field": "edit", "number": 2}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.PreviewResourcePayloadPreconditions"},
    schema_extra={},
)
class PreviewResourcePayloadPreconditions(ClosedModel):
    change: int = Field(
        description="Requested change index checked by this condition.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "change", "number": 1}},
    )
    precondition: core.OperationPrecondition = Field(
        json_schema_extra={"rift:proto": {"field": "precondition", "number": 2}}
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.PreviewResourcePayloadEffects"},
    schema_extra={},
)
class PreviewResourcePayloadEffects(ClosedModel):
    change: int = Field(
        description="Requested change index that produced this effect.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "change", "number": 1}},
    )
    effect: core.OperationEffect = Field(
        json_schema_extra={"rift:proto": {"field": "effect", "number": 2}}
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.PreviewResourcePayloadGuarantees"},
    schema_extra={},
)
class PreviewResourcePayloadGuarantees(ClosedModel):
    change: int = Field(
        description="Requested change index supported by this evidence.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "change", "number": 1}},
    )
    evidence: core.GuaranteeEvidence = Field(
        json_schema_extra={"rift:proto": {"field": "evidence", "number": 2}}
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.PreviewResourcePayloadResolutionDiagnostics"},
    schema_extra={},
)
class PreviewResourcePayloadResolutionDiagnostics(ClosedModel):
    change: int = Field(
        description="Requested change index that produced this finding.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "change", "number": 1}},
    )
    diagnostic: core.Diagnostic = Field(
        json_schema_extra={"rift:proto": {"field": "diagnostic", "number": 2}}
    )


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.PreviewResourcePayload"},
    schema_extra={},
)
class PreviewResourcePayload(ClosedModel):
    """One page of a retained candidate contract. Concatenating every array from successive pages reconstructs the complete plan and validation evidence. Every page repeats the URI, base, candidate, and bounded validation verdict."""

    uri: PreviewResourceUri = Field(
        description="The preview resource URI for this page.",
        json_schema_extra={"rift:proto": {"field": "uri", "number": 1}},
    )
    base: core.Snapshot = Field(
        description="The state from which resolution began.",
        json_schema_extra={"rift:proto": {"field": "base", "number": 2}},
    )
    candidate: core.Commit = Field(
        description="The immutable candidate commit produced by the complete plan.",
        json_schema_extra={"rift:proto": {"field": "candidate", "number": 3}},
    )
    validators: list[SandboxedValidator] = Field(
        description="Caller-supplied checks on this page, preserving declaration order.",
        json_schema_extra={"rift:proto": {"field": "validators", "number": 4}},
    )
    requested: list[Change] = Field(
        description="Requested changes on this page, preserving their transaction order.",
        json_schema_extra={"rift:proto": {"field": "requested", "number": 5}},
    )
    resolved: list[ResolvedChange] = Field(
        description="Bounded resolution summaries on this page, ordered by request index.",
        json_schema_extra={"rift:proto": {"field": "resolved", "number": 6}},
    )
    edits: list[PreviewResourcePayloadEdits] = Field(
        description="Concrete edits on this page, ordered by change index and canonical edit order.",
        json_schema_extra={"rift:proto": {"field": "edits", "number": 7}},
    )
    preconditions: list[PreviewResourcePayloadPreconditions] = Field(
        description="Satisfied preconditions on this page, ordered by change index and check order.",
        json_schema_extra={"rift:proto": {"field": "preconditions", "number": 8}},
    )
    effects: list[PreviewResourcePayloadEffects] = Field(
        description="Semantic effects on this page, ordered by change index and adapter emission order.",
        json_schema_extra={"rift:proto": {"field": "effects", "number": 9}},
    )
    guarantees: list[PreviewResourcePayloadGuarantees] = Field(
        description="Guarantee evidence on this page, ordered by change index and guarantee kind.",
        json_schema_extra={"rift:proto": {"field": "guarantees", "number": 10}},
    )
    resolution_diagnostics: list[PreviewResourcePayloadResolutionDiagnostics] = Field(
        description="Resolution findings on this page, ordered by change index and source location.",
        json_schema_extra={
            "rift:proto": {"field": "resolution_diagnostics", "number": 11}
        },
    )
    files: list[core.FileChange] = Field(
        description="File-level diff entries on this page, ordered by the path present after the change and then by the path present before it.",
        json_schema_extra={"rift:proto": {"field": "files", "number": 12}},
    )
    validation: CandidateValidation = Field(
        description="Bounded verdict for the complete candidate.",
        json_schema_extra={"rift:proto": {"field": "validation", "number": 13}},
    )
    adapter_reports: list[core.ValidationReport] = Field(
        description="Compiler reports on this page, sorted by language and unique across the complete resource.",
        json_schema_extra={"rift:proto": {"field": "adapter_reports", "number": 14}},
    )
    validator_results: list[ValidatorResult] = Field(
        description="Sandbox validator results on this page, preserving declaration order and appearing once across the complete resource.",
        json_schema_extra={"rift:proto": {"field": "validator_results", "number": 15}},
    )
    confirmations: list[core.ConfirmationRequirement] = Field(
        description="Acknowledgements on this page, sorted by id across the complete resource.",
        json_schema_extra={
            "rift:proto": {"field": "confirmations", "number": 16},
            "uniqueItems": True,
        },
    )
    diagnostics: list[core.Diagnostic] = Field(
        description="Rift findings about the complete plan, including path, ownership, and formatting decisions.",
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 17}},
    )
    next: PreviewResourceUri | None = Field(
        description="The URI for the next plan page, or null after the final page.",
        json_schema_extra={"rift:proto": {"field": "next", "number": 18}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.ApplyParams",
        "oneof": "variant",
        "variants": [
            {
                "tag": None,
                "field": "preview_apply_params",
                "number": 1,
                "type": "PreviewApplyParams",
            },
            {
                "tag": None,
                "field": "refresh_apply_params",
                "number": 2,
                "type": "RefreshApplyParams",
            },
            {
                "tag": None,
                "field": "publish_apply_params",
                "number": 3,
                "type": "PublishApplyParams",
            },
        ],
    },
    schema_extra={},
)
class ApplyParams(
    ProtocolRoot[
        "Annotated[PreviewApplyParams | RefreshApplyParams | PublishApplyParams, Field(discriminator='mode')]"
    ]
):
    """One phase of the candidate lifecycle for one repository. Preview creates a retained plan, refresh re-resolves that plan on a selected base, and publish attempts to advance the accepted ref with the retained contract. Future federation composes independent Apply calls and records partial outcomes and compensation across their separate compare-and-swap boundaries."""


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.PreviewApplyParams"},
    schema_extra={},
)
class PreviewApplyParams(ClosedModel):
    """Builds and validates an immutable candidate while leaving the accepted ref and session worktree unchanged."""

    mode: Literal["preview"] = Field(
        description="Selects candidate creation.",
        json_schema_extra={"rift:proto": {"field": "mode", "number": 1}},
    )
    rev: core.Revision | None = Field(
        default=None,
        description="State against which resolution begins. Omission selects the workspace's current accepted revision.",
        json_schema_extra={"rift:proto": {"field": "rev", "number": 2}},
    )
    changes: list[Change] = Field(
        description="Ordered deterministic changes. Each observes the result of its predecessors.",
        min_length=1,
        json_schema_extra={"rift:proto": {"field": "changes", "number": 3}},
    )
    validators: list[SandboxedValidator] = Field(
        description="Caller-supplied acceptance checks. The `edit` profile accepts an empty array. The `full` profile may accept up to `Limits.max_validators` declarations.",
        json_schema_extra={
            "rift:proto": {"field": "validators", "number": 4},
            "uniqueItems": True,
        },
    )


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.RefreshApplyParams"},
    schema_extra={},
)
class RefreshApplyParams(ClosedModel):
    """Re-resolves a retained preview's exact changes and validators on a selected base. It creates a new preview and leaves the accepted ref unchanged."""

    mode: Literal["refresh"] = Field(
        description="Selects preview refresh.",
        json_schema_extra={"rift:proto": {"field": "mode", "number": 1}},
    )
    previous: core.PreviewId = Field(
        description="The retained contract to run again.",
        json_schema_extra={"rift:proto": {"field": "previous", "number": 2}},
    )
    rev: core.Revision | None = Field(
        default=None,
        description="New base. Omission selects the current accepted revision.",
        json_schema_extra={"rift:proto": {"field": "rev", "number": 3}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.PublishApplyParams"},
    schema_extra={},
)
class PublishApplyParams(ClosedModel):
    """Attempts to advance the accepted ref with a retained preview. Rift replays resolution and validation. Every deterministic preview record and the candidate tree must match. It then compares the accepted ref with the retained base and advances it to the candidate. Fresh validator verdicts govern publication; captured output may differ."""

    mode: Literal["publish"] = Field(
        description="Selects publication.",
        json_schema_extra={"rift:proto": {"field": "mode", "number": 1}},
    )
    preview: core.PreviewId = Field(
        description="The retained contract to publish.",
        json_schema_extra={"rift:proto": {"field": "preview", "number": 2}},
    )
    idempotency_key: str = Field(
        description="Caller-chosen retry key. Rift retains the key with the exact publish request and result, returning that result on an exact retry and refusing reuse with different input.",
        pattern="^[\\x21-\\x7E]{1,256}$",
        json_schema_extra={"rift:proto": {"field": "idempotency_key", "number": 3}},
    )
    confirmations: list[int] = Field(
        description="Every currently required confirmation id, sorted bytewise. Missing or extra ids refuse publication.",
        json_schema_extra={
            "rift:proto": {"field": "confirmations", "number": 4},
            "uniqueItems": True,
        },
    )


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.ApplyResult",
        "oneof": "variant",
        "variants": [
            {
                "tag": None,
                "field": "preview_apply_result",
                "number": 1,
                "type": "PreviewApplyResult",
            },
            {
                "tag": None,
                "field": "refresh_apply_result",
                "number": 2,
                "type": "RefreshApplyResult",
            },
            {
                "tag": None,
                "field": "accepted_apply_result",
                "number": 3,
                "type": "AcceptedApplyResult",
            },
            {
                "tag": None,
                "field": "rejected_apply_result",
                "number": 4,
                "type": "RejectedApplyResult",
            },
            {
                "tag": None,
                "field": "refused_apply_result",
                "number": 5,
                "type": "RefusedApplyResult",
            },
            {
                "tag": None,
                "field": "conflict_apply_result",
                "number": 6,
                "type": "ConflictApplyResult",
            },
        ],
    },
    schema_extra={},
)
class ApplyResult(
    ProtocolRoot[
        "Annotated[PreviewApplyResult | RefreshApplyResult | AcceptedApplyResult | RejectedApplyResult | RefusedApplyResult | ConflictApplyResult, Field(discriminator='status')]"
    ]
):
    """A completed candidate lifecycle decision. Malformed requests, unavailable infrastructure, storage faults, and sandbox failures use `ErrorData`."""


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.PreviewApplyResult"},
    schema_extra={},
)
class PreviewApplyResult(ClosedModel):
    """A retained preview created without advancing the accepted ref or materializing its files."""

    status: Literal["preview"] = Field(
        description="Identifies preview creation.",
        json_schema_extra={"rift:proto": {"field": "status", "number": 1}},
    )
    summary: CandidateSummary = Field(
        description="Candidate identity and acceptance evidence.",
        json_schema_extra={"rift:proto": {"field": "summary", "number": 2}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.RefreshApplyResult"},
    schema_extra={},
)
class RefreshApplyResult(ClosedModel):
    """A new retained preview produced from an earlier contract and a selected base."""

    status: Literal["refresh"] = Field(
        description="Identifies preview refresh.",
        json_schema_extra={"rift:proto": {"field": "status", "number": 1}},
    )
    previous: core.PreviewId = Field(
        description="The preview whose contract was rerun.",
        json_schema_extra={"rift:proto": {"field": "previous", "number": 2}},
    )
    summary: CandidateSummary = Field(
        description="Identity and evidence for the refreshed candidate.",
        json_schema_extra={"rift:proto": {"field": "summary", "number": 3}},
    )
    changed_request_count: int = Field(
        description="Number of request indexes whose resolved owner, edits, preconditions, effects, guarantees, coverage, or diagnostics differ from the previous preview. Comparing the two preview resources yields the exact indexes.",
        ge=0,
        le=4294967295,
        json_schema_extra={
            "rift:proto": {"field": "changed_request_count", "number": 4}
        },
    )
    changed_file_count: int = Field(
        description="Number of candidate paths that differ from the previous preview. Comparing the paginated file records yields the exact paths.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "changed_file_count", "number": 5}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.AcceptedApplyResult"},
    schema_extra={},
)
class AcceptedApplyResult(ClosedModel):
    """Publication advanced the accepted ref to the retained candidate commit. `accepted` therefore equals `summary.candidate`. The session worktree remains unchanged until `persist` materializes it."""

    status: Literal["accepted"] = Field(
        description="Identifies successful publication.",
        json_schema_extra={"rift:proto": {"field": "status", "number": 1}},
    )
    summary: CandidateSummary = Field(
        description="The published candidate and its acceptance evidence.",
        json_schema_extra={"rift:proto": {"field": "summary", "number": 2}},
    )
    accepted: core.Commit = Field(
        description="The commit now held by the accepted ref.",
        json_schema_extra={"rift:proto": {"field": "accepted", "number": 3}},
    )
    replayed: bool = Field(
        description="Whether Rift returned a previously stored result for this exact idempotency key and request.",
        json_schema_extra={"rift:proto": {"field": "replayed", "number": 4}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.RejectedApplyResult"},
    schema_extra={},
)
class RejectedApplyResult(ClosedModel):
    """Publication reran the retained contract and validation did not pass. The candidate remains readable as a preview and the accepted ref is unchanged."""

    status: Literal["rejected"] = Field(
        description="Identifies validation rejection.",
        json_schema_extra={"rift:proto": {"field": "status", "number": 1}},
    )
    summary: CandidateSummary = Field(
        description="The rejected candidate and the evidence that prevented acceptance.",
        json_schema_extra={"rift:proto": {"field": "summary", "number": 2}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "reason",
        "number": 4,
        "enum": "Reason",
        "values": {
            "unsupported": {"name": "UNSUPPORTED", "number": 1},
            "unmet_precondition": {"name": "UNMET_PRECONDITION", "number": 2},
            "ambiguous_target": {"name": "AMBIGUOUS_TARGET", "number": 3},
            "stale_preview": {"name": "STALE_PREVIEW", "number": 4},
            "stale_action": {"name": "STALE_ACTION", "number": 5},
            "stale_match": {"name": "STALE_MATCH", "number": 6},
            "cardinality_mismatch": {"name": "CARDINALITY_MISMATCH", "number": 7},
            "confirmation_required": {"name": "CONFIRMATION_REQUIRED", "number": 8},
            "unsafe_effect": {"name": "UNSAFE_EFFECT", "number": 9},
            "formatter_unsupported": {"name": "FORMATTER_UNSUPPORTED", "number": 10},
            "validation_incomplete": {"name": "VALIDATION_INCOMPLETE", "number": 11},
        },
    },
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


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.RefusedApplyResult"},
    schema_extra={},
)
class RefusedApplyResult(ClosedModel):
    """Resolution or publication stopped before a valid candidate outcome existed. No accepted ref or session worktree changes."""

    status: Literal["refused"] = Field(
        description="Identifies a domain refusal.",
        json_schema_extra={"rift:proto": {"field": "status", "number": 1}},
    )
    base: core.Snapshot = Field(
        description="State against which the refused work was attempted.",
        json_schema_extra={"rift:proto": {"field": "base", "number": 2}},
    )
    change: int | None = Field(
        description="Zero-based requested change that caused the refusal, or null for a publication-wide condition.",
        json_schema_extra={"rift:proto": {"field": "change", "number": 3}},
    )
    reason: RefusedApplyResultReason = Field(
        description="Condition the caller can act on.",
        json_schema_extra={
            "rift:proto": {
                "field": "reason",
                "number": 4,
                "enum": "Reason",
                "values": {
                    "unsupported": {"name": "UNSUPPORTED", "number": 1},
                    "unmet_precondition": {"name": "UNMET_PRECONDITION", "number": 2},
                    "ambiguous_target": {"name": "AMBIGUOUS_TARGET", "number": 3},
                    "stale_preview": {"name": "STALE_PREVIEW", "number": 4},
                    "stale_action": {"name": "STALE_ACTION", "number": 5},
                    "stale_match": {"name": "STALE_MATCH", "number": 6},
                    "cardinality_mismatch": {
                        "name": "CARDINALITY_MISMATCH",
                        "number": 7,
                    },
                    "confirmation_required": {
                        "name": "CONFIRMATION_REQUIRED",
                        "number": 8,
                    },
                    "unsafe_effect": {"name": "UNSAFE_EFFECT", "number": 9},
                    "formatter_unsupported": {
                        "name": "FORMATTER_UNSUPPORTED",
                        "number": 10,
                    },
                    "validation_incomplete": {
                        "name": "VALIDATION_INCOMPLETE",
                        "number": 11,
                    },
                },
            }
        },
    )
    preconditions: list[core.OperationPrecondition] = Field(
        description="Conditions checked before refusal, including at least one failed entry for `unmet_precondition`.",
        json_schema_extra={"rift:proto": {"field": "preconditions", "number": 5}},
    )
    blockers: list[core.OperationBlocker] = Field(
        description="Existing code, paths, or relationships that prevented a deterministic resolution.",
        json_schema_extra={"rift:proto": {"field": "blockers", "number": 6}},
    )
    diagnostics: list[core.Diagnostic] = Field(
        description="Evidence that explains the refusal.",
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 7}},
    )
    suggestions: list[Change] = Field(
        description="Deterministic replacement changes that satisfy the condition Rift could identify. Empty when no safe repair is known.",
        json_schema_extra={"rift:proto": {"field": "suggestions", "number": 8}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "reason",
        "number": 2,
        "enum": "Reason",
        "values": {
            "stale_base": {"name": "STALE_BASE", "number": 1},
            "idempotency_key_reused": {"name": "IDEMPOTENCY_KEY_REUSED", "number": 2},
        },
    },
    schema_extra={},
)
class ConflictApplyResultReason(str, Enum):
    """Which identity comparison failed."""

    STALE_BASE = "stale_base"
    IDEMPOTENCY_KEY_REUSED = "idempotency_key_reused"


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.ConflictApplyResult"},
    schema_extra={},
)
class ConflictApplyResult(ClosedModel):
    """Publication reached compare-and-swap after another accepted change had moved the ref, or reused an idempotency key for different input. The accepted ref is unchanged by this request."""

    status: Literal["conflict"] = Field(
        description="Identifies an optimistic concurrency conflict.",
        json_schema_extra={"rift:proto": {"field": "status", "number": 1}},
    )
    reason: ConflictApplyResultReason = Field(
        description="Which identity comparison failed.",
        json_schema_extra={
            "rift:proto": {
                "field": "reason",
                "number": 2,
                "enum": "Reason",
                "values": {
                    "stale_base": {"name": "STALE_BASE", "number": 1},
                    "idempotency_key_reused": {
                        "name": "IDEMPOTENCY_KEY_REUSED",
                        "number": 2,
                    },
                },
            }
        },
    )
    base: core.Snapshot = Field(
        description="Accepted snapshot the retained preview expected.",
        json_schema_extra={"rift:proto": {"field": "base", "number": 3}},
    )
    current: core.Snapshot = Field(
        description="Accepted snapshot observed at publication time.",
        json_schema_extra={"rift:proto": {"field": "current", "number": 4}},
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.PersistParams"}, schema_extra={}
)
class PersistParams(ClosedModel):
    """Selects files from one accepted commit for materialization into the session worktree. Omitted `paths` selects every changed path."""

    revision: core.Commit = Field(
        description="Accepted commit whose tree supplies the desired entries.",
        json_schema_extra={"rift:proto": {"field": "revision", "number": 1}},
    )
    paths: list[core.ProjectPath] | None = Field(
        default=None,
        description="Changed project paths to materialize. Omission selects all paths changed by the accepted commit.",
        json_schema_extra={
            "rift:proto": {"field": "paths", "number": 2},
            "uniqueItems": True,
        },
    )
    include_deletions: bool = Field(
        default=False,
        description="Whether entries absent from the accepted tree may be removed from the worktree.",
        json_schema_extra={"rift:proto": {"field": "include_deletions", "number": 3}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "outcome",
        "number": 2,
        "enum": "Outcome",
        "values": {
            "written": {"name": "WRITTEN", "number": 1},
            "unchanged": {"name": "UNCHANGED", "number": 2},
            "skipped_drift": {"name": "SKIPPED_DRIFT", "number": 3},
            "skipped_deletion": {"name": "SKIPPED_DELETION", "number": 4},
            "skipped_unsupported_kind": {
                "name": "SKIPPED_UNSUPPORTED_KIND",
                "number": 5,
            },
            "skipped_external_content": {
                "name": "SKIPPED_EXTERNAL_CONTENT",
                "number": 6,
            },
            "skipped_sparse": {"name": "SKIPPED_SPARSE", "number": 7},
            "skipped_nested_repository": {
                "name": "SKIPPED_NESTED_REPOSITORY",
                "number": 8,
            },
            "not_found": {"name": "NOT_FOUND", "number": 9},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "written": "The worktree entry matched the candidate base and now matches the accepted "
            "commit.",
            "unchanged": "The worktree entry already matched the accepted commit.",
            "skipped_drift": "The worktree entry differed from the candidate base and accepted commit, "
            "so materialization preserved the local change.",
            "skipped_deletion": "The accepted commit removes the entry and `include_deletions` is "
            "false.",
            "skipped_unsupported_kind": "The selected Git entry kind has no safe worktree "
            "materialization rule.",
            "skipped_external_content": "The selected entry refers to content Rift does not hydrate, "
            "such as Git LFS content.",
            "skipped_sparse": "The selected path is outside the worktree's sparse-checkout definition.",
            "skipped_nested_repository": "The selected path is a gitlink or lies inside a child "
            "repository, whose worktree is managed by its own Rift "
            "connection.",
            "not_found": "The selected path does not differ between the candidate base and accepted "
            "commit.",
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


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.PersistOutcome"}, schema_extra={}
)
class PersistOutcome(ClosedModel):
    """The materialization decision for one selected path. Safety conditions are reported as skips. Materialization keeps sparse-checkout boundaries, leaves child repositories untouched, and does not hydrate external content."""

    path: core.ProjectPath = Field(
        description="Selected project path.",
        json_schema_extra={"rift:proto": {"field": "path", "number": 1}},
    )
    outcome: PersistOutcomeOutcome = Field(
        description="What happened to this path.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "written": "The worktree entry matched the candidate base and now matches the accepted "
                "commit.",
                "unchanged": "The worktree entry already matched the accepted commit.",
                "skipped_drift": "The worktree entry differed from the candidate base and accepted commit, "
                "so materialization preserved the local change.",
                "skipped_deletion": "The accepted commit removes the entry and `include_deletions` is "
                "false.",
                "skipped_unsupported_kind": "The selected Git entry kind has no safe worktree "
                "materialization rule.",
                "skipped_external_content": "The selected entry refers to content Rift does not hydrate, "
                "such as Git LFS content.",
                "skipped_sparse": "The selected path is outside the worktree's sparse-checkout definition.",
                "skipped_nested_repository": "The selected path is a gitlink or lies inside a child "
                "repository, whose worktree is managed by its own Rift "
                "connection.",
                "not_found": "The selected path does not differ between the candidate base and accepted "
                "commit.",
            },
            "rift:proto": {
                "field": "outcome",
                "number": 2,
                "enum": "Outcome",
                "values": {
                    "written": {"name": "WRITTEN", "number": 1},
                    "unchanged": {"name": "UNCHANGED", "number": 2},
                    "skipped_drift": {"name": "SKIPPED_DRIFT", "number": 3},
                    "skipped_deletion": {"name": "SKIPPED_DELETION", "number": 4},
                    "skipped_unsupported_kind": {
                        "name": "SKIPPED_UNSUPPORTED_KIND",
                        "number": 5,
                    },
                    "skipped_external_content": {
                        "name": "SKIPPED_EXTERNAL_CONTENT",
                        "number": 6,
                    },
                    "skipped_sparse": {"name": "SKIPPED_SPARSE", "number": 7},
                    "skipped_nested_repository": {
                        "name": "SKIPPED_NESTED_REPOSITORY",
                        "number": 8,
                    },
                    "not_found": {"name": "NOT_FOUND", "number": 9},
                },
            },
        },
    )


@definition(
    owner="mcp", public=True, proto={"type": "rift.mcp.PersistResult"}, schema_extra={}
)
class PersistResult(ClosedModel):
    """Materialization outcomes for one accepted commit. Results follow project-path byte order. Rift enumerates the selected paths and verifies that every outcome fits `max_response_bytes` before writing any path."""

    revision: core.Commit = Field(
        description="Accepted commit used as the source tree.",
        json_schema_extra={"rift:proto": {"field": "revision", "number": 1}},
    )
    all_written: bool = Field(
        description="Whether every selected path is `written` or `unchanged`.",
        json_schema_extra={"rift:proto": {"field": "all_written", "number": 2}},
    )
    outcomes: list[PersistOutcome] = Field(
        description="One result per selected path.",
        json_schema_extra={"rift:proto": {"field": "outcomes", "number": 3}},
    )


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.ConformanceProfile",
        "enum": "ConformanceProfile",
        "values": {
            "read": {"name": "CONFORMANCE_PROFILE_READ", "number": 1},
            "edit": {"name": "CONFORMANCE_PROFILE_EDIT", "number": 2},
            "full": {"name": "CONFORMANCE_PROFILE_FULL", "number": 3},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "read": "Serves the read tools plus repository, symbol, diff, and file resources. Fixtures "
            "reconstruct file bytes, verify language summaries and match tags, replay cursors, "
            "reject stale state, preserve coverage, and stop at gitlinks. Fixtures cover "
            "shared-worktree languages and retained virtual files. Mechanical reads and warm "
            "cached semantic reads run with adapters "
            "stopped. Fixed repositories record latency, bytes, tokens, and adapter memory.",
            "edit": "Adds `apply` preview, refresh, and publish, the preview resource, compiler "
            "formatting, and compiler validation. Fixtures exercise every Change variant and "
            "advertised action family, complete validation, preview pagination, refresh "
            "comparison, rejected-candidate repair, idempotent retry, compare-and-swap races, "
            "cancellation, and crash recovery. Caller-supplied validators remain unavailable "
            "and `max_validators` is zero.",
            "full": "Adds sandboxed validators and `persist` materialization. Fixtures verify response "
            "preflight before worktree writes, per-path drift outcomes, and shell, network, "
            "filesystem, timeout, and output-capture isolation for every sandbox backend. A "
            "release claim also runs the suite with two different language adapters and one "
            "embedded-language repository.",
        }
    },
)
class ConformanceProfile(str, Enum):
    """Executable runtime claim for this workspace. Profiles accumulate. Every claim validates schemas and examples, checks reference reachability and axis ownership, rejects generated Protobuf drift, compiles the adapter service, and exercises the handshake. Runtime fixtures check ordering, limits, typed failures, the shared-worktree barrier, and topological virtual-source sync. `Contract` identifies the target schema."""

    READ = "read"
    EDIT = "edit"
    FULL = "full"


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.ResultOrder",
        "enum": "ResultOrder",
        "values": {
            "relevance": {"name": "RESULT_ORDER_RELEVANCE", "number": 1},
            "path": {"name": "RESULT_ORDER_PATH", "number": 2},
            "identity": {"name": "RESULT_ORDER_IDENTITY", "number": 3},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "relevance": "Highest `score` first, then identity. Scores are comparable across every page "
            "of one request and nowhere else.",
            "path": "By the file a result is written in, then its byte range, then identity. Two hits "
            "in one file come back in source order.",
            "identity": "By the result's canonical identity: a symbol URI, a file path, or an "
            "`ActionKey`. Compiler actions use this order because they carry no relevance "
            "score or common source path.",
        }
    },
)
class ResultOrder(str, Enum):
    """The total order a paginated answer comes back in, named in the request so a cursor can be bound to it. Every order ends in the result's own identity, so two results that tie never swap places between pages."""

    RELEVANCE = "relevance"
    PATH = "path"
    IDENTITY = "identity"


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "content",
        "number": 1,
        "type": "rift.mcp.FileResourcePayloadUtf8FileContent",
    },
    schema_extra={},
)
class FileResourcePayloadUtf8FileContent(ClosedModel):
    kind: Literal["regular"] = Field(
        default="regular",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "file",
        "number": 2,
        "type": "rift.mcp.FileResourcePayloadUtf8File",
    },
    schema_extra={},
)
class FileResourcePayloadUtf8File(ClosedModel):
    content: FileResourcePayloadUtf8FileContent | None = Field(
        default=None,
        json_schema_extra={
            "rift:proto": {
                "field": "content",
                "number": 1,
                "type": "rift.mcp.FileResourcePayloadUtf8FileContent",
            }
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.FileResourcePayloadUtf8"},
    schema_extra={},
)
class FileResourcePayloadUtf8(ClosedModel):
    """A range whose bytes decode as UTF-8. Start and end fall on UTF-8 code-point boundaries."""

    encoding: Literal["utf8"] = Field(default="utf8")
    content: str | None = Field(
        default=None,
        json_schema_extra={"rift:proto": {"field": "content", "number": 1}},
    )
    file: FileResourcePayloadUtf8File | None = Field(
        default=None,
        json_schema_extra={
            "rift:proto": {
                "field": "file",
                "number": 2,
                "type": "rift.mcp.FileResourcePayloadUtf8File",
            }
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "content",
        "number": 1,
        "type": "rift.mcp.FileResourcePayloadBase64FileContent",
    },
    schema_extra={},
)
class FileResourcePayloadBase64FileContent(ClosedModel):
    kind: Literal["regular"] = Field(
        default="regular",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}},
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "file",
        "number": 2,
        "type": "rift.mcp.FileResourcePayloadBase64File",
    },
    schema_extra={},
)
class FileResourcePayloadBase64File(ClosedModel):
    content: FileResourcePayloadBase64FileContent | None = Field(
        default=None,
        json_schema_extra={
            "rift:proto": {
                "field": "content",
                "number": 1,
                "type": "rift.mcp.FileResourcePayloadBase64FileContent",
            }
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.FileResourcePayloadBase64"},
    schema_extra={},
)
class FileResourcePayloadBase64(ClosedModel):
    """A range carried as canonical base64 because its bytes do not form valid UTF-8."""

    encoding: Literal["base64"] = Field(default="base64")
    content: str | None = Field(
        default=None,
        pattern="^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$",
        json_schema_extra={"rift:proto": {"field": "content", "number": 1}},
    )
    file: FileResourcePayloadBase64File | None = Field(
        default=None,
        json_schema_extra={
            "rift:proto": {
                "field": "file",
                "number": 2,
                "type": "rift.mcp.FileResourcePayloadBase64File",
            }
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "kind",
        "number": 1,
        "enum": "Kind",
        "values": {
            "lfs_pointer": {"name": "LFS_POINTER", "number": 1},
            "symlink": {"name": "SYMLINK", "number": 2},
            "gitlink": {"name": "GITLINK", "number": 3},
        },
    },
    schema_extra={},
)
class FileResourcePayloadNoneFileContentKind(str, Enum):
    LFS_POINTER = "lfs_pointer"
    SYMLINK = "symlink"
    GITLINK = "gitlink"


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "content",
        "number": 1,
        "type": "rift.mcp.FileResourcePayloadNoneFileContent",
    },
    schema_extra={},
)
class FileResourcePayloadNoneFileContent(ClosedModel):
    kind: FileResourcePayloadNoneFileContentKind | None = Field(
        default=None,
        json_schema_extra={
            "rift:proto": {
                "field": "kind",
                "number": 1,
                "enum": "Kind",
                "values": {
                    "lfs_pointer": {"name": "LFS_POINTER", "number": 1},
                    "symlink": {"name": "SYMLINK", "number": 2},
                    "gitlink": {"name": "GITLINK", "number": 3},
                },
            }
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={
        "field": "file",
        "number": 6,
        "type": "rift.mcp.FileResourcePayloadNoneFile",
    },
    schema_extra={},
)
class FileResourcePayloadNoneFile(ClosedModel):
    content: FileResourcePayloadNoneFileContent | None = Field(
        default=None,
        json_schema_extra={
            "rift:proto": {
                "field": "content",
                "number": 1,
                "type": "rift.mcp.FileResourcePayloadNoneFileContent",
            }
        },
    )


@definition(
    owner="mcp",
    public=False,
    proto={"type": "rift.mcp.FileResourcePayloadNone"},
    schema_extra={},
)
class FileResourcePayloadNone(ClosedModel):
    """A non-regular tree entry. Its empty interval carries no bytes and has no continuation."""

    encoding: Literal["none"] = Field(default="none")
    start: Literal[0] = Field(
        default=0,
        json_schema_extra={"rift:proto": {"field": "start", "number": 1}},
    )
    end: Literal[0] = Field(
        default=0,
        json_schema_extra={"rift:proto": {"field": "end", "number": 2}},
    )
    total_bytes: Literal[0] = Field(
        default=0,
        json_schema_extra={"rift:proto": {"field": "total_bytes", "number": 3}},
    )
    content: None = Field(
        default=None,
        json_schema_extra={"rift:proto": {"field": "content", "number": 4}},
    )
    next: None = Field(
        default=None, json_schema_extra={"rift:proto": {"field": "next", "number": 5}}
    )
    file: FileResourcePayloadNoneFile | None = Field(
        default=None,
        json_schema_extra={
            "rift:proto": {
                "field": "file",
                "number": 6,
                "type": "rift.mcp.FileResourcePayloadNoneFile",
            }
        },
    )


@definition(
    owner="mcp",
    public=True,
    proto={
        "type": "rift.mcp.FileResourcePayload",
        "oneof": "variant",
        "variants": [
            {
                "tag": "utf8",
                "field": "utf8",
                "number": 1,
                "type": "FileResourcePayloadUtf8",
            },
            {
                "tag": "base64",
                "field": "base64",
                "number": 2,
                "type": "FileResourcePayloadBase64",
            },
            {
                "tag": "none",
                "field": "none",
                "number": 3,
                "type": "FileResourcePayloadNone",
            },
        ],
    },
    schema_extra={},
)
class FileResourcePayload(
    ProtocolRoot[
        "Annotated[FileResourcePayloadUtf8 | FileResourcePayloadBase64 | FileResourcePayloadNone, Field(discriminator='encoding')]"
    ]
):
    """One bounded byte range from a file at one state. Regular files carry UTF-8 text where the selected bytes form valid UTF-8 and base64 otherwise. `next` continues at `end` until the complete file has been read."""


@definition(
    owner="mcp",
    public=True,
    proto={"type": "rift.mcp.LanguageSupport"},
    schema_extra={},
)
class LanguageSupport(ClosedModel):
    """What Rift can do for one language in this workspace, and which compiler is doing it. An agent uses this to select queries and operations. File recognition, virtual inputs, and compiler write claims remain on the adapter seam."""

    language: core.LanguageId = Field(
        description="Which language this entry answers for. It is the adapter's identity too, since one workspace runs one adapter per language.",
        json_schema_extra={"rift:proto": {"field": "language", "number": 1}},
    )
    compiler: str = Field(
        description="The compiler name and version used by this adapter process. It remains fixed for the process lifetime and is reported once with the workspace capabilities.",
        max_length=4096,
        examples=["tsc 5.6.2"],
        json_schema_extra={"rift:proto": {"field": "compiler", "number": 2}},
    )
    adapter: str = Field(
        description="What the adapter build driving that compiler calls itself. Two adapter versions can read one compiler differently, so the pair is what a bug report needs.",
        max_length=4096,
        examples=["rift-adapter-typescript 0.4.1"],
        json_schema_extra={"rift:proto": {"field": "adapter", "number": 3}},
    )
    structural_matching: core.Coverage = Field(
        description="Coverage and failure reason for structural matching in this language. Unsupported coverage distinguishes a missing parser from a complete query with no matches.",
        json_schema_extra={"rift:proto": {"field": "structural_matching", "number": 4}},
    )
    actions: core.Coverage = Field(
        description="Coverage and failure reason for compiler action discovery.",
        json_schema_extra={"rift:proto": {"field": "actions", "number": 5}},
    )
    action_kinds: list[core.ActionSupport] = Field(
        description="Supported action families in kind-prefix order. An empty array with complete `actions` coverage means the compiler offers no actions; unsupported coverage means Rift cannot know.",
        json_schema_extra={"rift:proto": {"field": "action_kinds", "number": 6}},
    )
    validation: core.Coverage = Field(
        description="Whether the adapter can validate a changed compiler closure completely enough for publication.",
        json_schema_extra={"rift:proto": {"field": "validation", "number": 7}},
    )
    formatting: core.Coverage = Field(
        description="Whether the adapter can format changed syntax regions or complete affected files.",
        json_schema_extra={"rift:proto": {"field": "formatting", "number": 8}},
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
    ResolvedChange,
    SandboxedValidator,
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
    PersistParams,
    PersistOutcome,
    PersistResult,
    ConformanceProfile,
    ResultOrder,
    FileResourcePayload,
    LanguageSupport,
)
