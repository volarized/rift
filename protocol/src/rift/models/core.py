from __future__ import annotations

import base64
import re
from urllib.parse import quote, unquote_to_bytes

from pydantic import model_validator

from .base import *


def validate_base64url(value: str) -> None:
    """Require the canonical unpadded spelling used inside Rift resource identities."""

    if len(value) % 4 == 1:
        raise ValueError("value is not decodable base64url")
    padded = value + "=" * ((4 - len(value) % 4) % 4)
    decoded = base64.b64decode(padded, altchars=b"-_", validate=True)
    canonical = base64.urlsafe_b64encode(decoded).decode("ascii").rstrip("=")
    if canonical != value:
        raise ValueError("value must use canonical unpadded base64url")


def _raw_query(uri: str) -> dict[str, str]:
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
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_INT64,
    root=Literal[1],
)
class ProtocolVersion(ProtocolRoot):
    """The protocol major. It changes when a peer cannot decode the next message safely. The
    workspace lease exposes it before a client opens the version-independent server socket."""


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern="^[0-9a-f]{64}$",
)
class Digest(ProtocolRoot):
    """SHA-256 of the value being identified, lowercase hex, 64 characters. The contract fixes the algorithm so Rift and its adapters produce the same identity for the same bytes."""


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^ph_[a-z2-7]{26}$",
    examples=["ph_bbbbbbbbbbbbbbbbbbbbbbbbbb"],
)
class ProjectionHead(ProtocolRoot):
    """Opaque compare-and-swap token for the current source state of a writable projection.
    Rift generates a fresh token whenever the visible state advances."""


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ProjectionState(ClosedModel):
    """Current state of the session projection."""

    head: Field[ProjectionHead] = proto_field(
        description="Fresh ABA-safe token for this current state.", number=2
    )
    dirty: Field[bool] = proto_field(
        description="Whether the projection contains unpublished changes.", number=3
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class LanguageRegion(ClosedModel):
    "A byte range of one file and the language used to parse it. The owner of `App.svelte` can mark its script block as TypeScript. A produced virtual file records ranges in its own byte coordinates."

    language: Field[Language] = proto_field(
        description="The language grammar used for these bytes.", number=1
    )
    range: Field[TextRange] = proto_field(
        description="Offsets of the language region inside the file.", number=2
    )


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://file/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-F]{2}){1,1000}$",
    min_length=13,
    max_length=8192,
    examples=[
        "rift://file/pkg/util.py",
        "rift://file/src/%E2%98%83.ts",
        "rift://file/src/config.ts",
    ],
)
class FileId(ProtocolRoot):
    """Identity of one file in the session projection. The path after `rift://file/` is a
    canonical project-relative path."""

    @model_validator(mode="after")
    def path_is_canonical(self) -> FileId:
        encoded = self.root.removeprefix("rift://file/").partition("?")[0]
        decoded = unquote_to_bytes(encoded).decode("utf-8")
        ProjectPath.model_validate(decoded)
        canonical = quote(decoded, safe="/!$&'()*+,;=:@-._~")
        if canonical != encoded:
            raise ValueError("file path must use canonical URI encoding")
        return self


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class FileContentRegular(ClosedModel):
    """A physical or virtual file with bytes in it. Every node and symbol with readable source comes from this kind."""

    kind: Field[Literal["regular"]] = proto_field()
    size: Field[int] = proto_field(
        description="Size in bytes.", ge=0, le=9007199254740991, number=1
    )
    executable: Field[bool] = proto_field(
        description="Whether the file is executable.", number=2
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class FileContentSymlink(ClosedModel):
    "A symbolic link whose target is carried as canonical base64."

    kind: Field[Literal["symlink"]] = proto_field()
    target: Field[str] = proto_field(
        description=(
            "Canonical padded base64 of the raw target bytes. Rift does not follow the target."
        ),
        pattern="^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$",
        max_length=5464,
        number=1,
    )

    @model_validator(mode="after")
    def target_is_canonical_base64(self) -> FileContentSymlink:
        decoded = base64.b64decode(self.target, validate=True)
        if len(decoded) > 4096:
            raise ValueError("symlink target exceeds 4096 bytes")
        if base64.b64encode(decoded).decode("ascii") != self.target:
            raise ValueError("symlink target must use canonical padded base64")
        return self


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("regular", "regular", 1, FileContentRegular),
        Variant("symlink", "symlink", 2, FileContentSymlink),
    ),
    placement=Placement("content", 2),
    public=False,
)
class FileContent(ProtocolRoot):
    "The bytes of a regular file or the target of a symbolic link."


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class File(ClosedModel):
    "One file in the session projection and the languages that read it."

    model_config = closed_config(
        {
            "allOf": [
                {
                    "if": {
                        "properties": {
                            "content": {"properties": {"kind": {"const": "symlink"}}}
                        }
                    },
                    "then": {
                        "properties": {
                            "languages": {"maxItems": 0},
                            "regions": {"maxItems": 0},
                            "semantic": {"const": False},
                        }
                    },
                }
            ]
        }
    )
    id: Field[FileId] = proto_field(
        description="Project-relative source identity and the URI from which this record and its bytes are read.",
        number=1,
    )
    content: Field[FileContent] = proto_field(
        description="Regular-file metadata or a symbolic-link target.",
        number=2,
    )
    languages: Field[list[Language]] = proto_field(
        description=(
            "Distinct `Language` values in `regions`, sorted by name and dialect. This "
            "summary avoids scanning every region. The physical owner reports embedded "
            "languages even when another adapter consumes virtual source."
        ),
        examples=[["svelte", "typescript", "css"]],
        number=3,
        json_schema_extra={"uniqueItems": True},
    )
    regions: Field[list[LanguageRegion]] = proto_field(
        description=(
            "Byte ranges parsed with each language grammar. Entries sort by start, end, "
            "language name, and dialect with null first. Regions may overlap when two "
            "grammars parse the same bytes."
        ),
        number=4,
    )
    semantic: Field[bool] = proto_field(
        description=(
            "Whether Rift produced facts from this file. False where there is nothing to "
            "read, and where no adapter claims the path."
        ),
        number=5,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class ProjectEntryDirectory(ClosedModel):
    """A visible directory, including an empty one."""

    kind: Field[Literal["directory"]] = proto_field()
    path: Field[ProjectPath] = proto_field(
        description="Project-relative directory path. The empty path names the project root.",
        number=1,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class ProjectEntryFile(ClosedModel):
    "A file with its content and language ownership metadata."

    kind: Field[Literal["file"]] = proto_field()
    file: Field[File] = proto_field(
        description="The file represented by this tree node.", number=1
    )


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("directory", "directory", 1, ProjectEntryDirectory),
        Variant("file", "file", 2, ProjectEntryFile),
    ),
)
class ProjectEntry(ProtocolRoot):
    "One visible directory or file in the session projection."


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://node/[A-Za-z][A-Za-z0-9._-]*(?::[A-Za-z][A-Za-z0-9._-]*)?/(?:[A-Za-z0-9._~!$&'()*+,;=:/-]|%[0-9A-F]{2}){1,1000}@\d+-\d+$",
    min_length=18,
    max_length=8192,
    examples=[
        "rift://node/python/pkg/util.py@1204-1266",
        "rift://node/sql:postgresql/db/schema.sql@40-88",
    ],
)
class NodeId(ProtocolRoot):
    """Identity of one syntax-tree node in the session projection."""

    @model_validator(mode="after")
    def span_is_canonical(self) -> NodeId:
        address = self.root.removeprefix("rift://node/").partition("?")[0]
        _language, separator, encoded_span = address.partition("/")
        if not separator:
            raise ValueError("node identity requires a language and path")
        match = re.fullmatch(r"(.+)@([0-9]+)-([0-9]+)", encoded_span)
        if match is None:
            raise ValueError("node identity requires a byte range")
        encoded_path, start_text, end_text = match.groups()
        decoded_path = unquote_to_bytes(encoded_path).decode("utf-8")
        ProjectPath.model_validate(decoded_path)
        canonical = quote(decoded_path, safe="/!$&'()*+,;=:@-._~")
        if canonical != encoded_path:
            raise ValueError("node path must use canonical URI encoding")
        start, end = int(start_text), int(end_text)
        if str(start) != start_text or str(end) != end_text:
            raise ValueError("node range coordinates must use canonical decimal")
        if start > end:
            raise ValueError("node range start must not exceed end")
        if end > 9007199254740991:
            raise ValueError("node range exceeds exact protocol integers")
        return self


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://symbol/[A-Za-z][A-Za-z0-9._-]*(?::[A-Za-z][A-Za-z0-9._-]*)?/(?:[A-Za-z0-9._~!$&'()*+,;=:/@-]|%[0-9A-F]{2}){1,1000}(\?cursor=[A-Za-z0-9_-]{1,4096})?$",
    min_length=17,
    max_length=12288,
    examples=[
        "rift://symbol/python/pkg.util.load_config~1",
        "rift://symbol/sql:postgresql/public.users",
        "rift://symbol/css:scss/theme.$accent",
    ],
)
class SymbolId(ProtocolRoot):
    """Identity of one symbol in the session projection."""

    @model_validator(mode="after")
    def address_is_canonical(self) -> SymbolId:
        address = self.root.removeprefix("rift://symbol/").partition("?")[0]
        _language, separator, encoded_name = address.partition("/")
        if not separator:
            raise ValueError("symbol identity requires a language and name")
        decoded_name = unquote_to_bytes(encoded_name).decode("utf-8")
        canonical = quote(decoded_name, safe="/!$&'()*+,;=:@-._~")
        if canonical != encoded_name:
            raise ValueError("symbol name must use canonical URI encoding")
        query = _raw_query(self.root)
        cursor = query.get("cursor")
        if cursor is not None:
            validate_base64url(cursor)
        return self


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Language(ClosedModel):
    """A language name and its optional dialect. The pair selects at most one configured
    adapter process in a workspace; that process can serve many adapter states."""

    name: Field[str] = proto_field(
        description="The language name, such as `sql`, `json`, or `css`.",
        pattern="^[A-Za-z][A-Za-z0-9._-]*$",
        max_length=64,
        examples=["sql", "json", "css"],
        number=1,
    )
    dialect: Field[str | None] = proto_field(
        default=None,
        description=(
            "A dialect whose syntax or semantics differ within the language, such as "
            "`postgresql`, `jsonc`, or `scss`."
        ),
        pattern="^[A-Za-z][A-Za-z0-9._-]*$",
        max_length=64,
        examples=["postgresql", "jsonc", "scss"],
        number=2,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ExtensionValue(ClosedModel):
    """Versioned extension value. data is validated against the schema advertised for its key and version."""

    version: Field[int] = proto_field(
        description=(
            "Which version of the key's advertised schema shaped `data`. A consumer skips a "
            "value whose version it does not implement."
        ),
        ge=1,
        number=1,
    )
    data: Field[Any] = proto_field(
        description=(
            "The value itself, shaped by whatever that key and version advertise. Rift "
            "carries it and never interprets it."
        ),
        number=2,
    )


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^[a-z0-9]+(?:[.-][a-z0-9]+)+\.[A-Za-z][A-Za-z0-9_-]*$",
)
class ExtensionKey(ProtocolRoot):
    """A reverse-domain namespaced extension or extension-operation identifier."""


@mapping(
    owner=CORE,
    root=dict[ExtensionKey, ExtensionValue],
    placement=Placement("entries", 1),
)
class Extensions(ProtocolRoot):
    """Facts an adapter carries that the model has no field for, under a reverse-domain key. Keys and values use RFC 8785 canonical JSON. `Capabilities.extension_values` advertises every key and version, and consumers skip entries they do not implement."""


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^(?!/)(?!\.\.?(/|$))(?!.*(/\.\.?)(/|$))[^\\\u0000-\u001F\u007F]+$",
)
class PathPattern(ProtocolRoot):
    """Project-relative glob using *, ?, **, and character classes."""


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^(?:$|(?!\.rift(?:/|$))(?!/)(?!.*(?:^|/)\.{1,2}(?:/|$))(?!.*//)[^\\\u0000-\u001F\u007F/]+(?:/[^\\\u0000-\u001F\u007F/]+)*)$",
    max_length=1000,
    examples=["", "src/config.ts", "packages/api/Cargo.toml"],
)
class ProjectPath(ProtocolRoot):
    """One path below the project root, using forward slashes and UTF-8. The empty path names
    the root itself. Absolute paths, backslashes, control characters, empty segments, and `.` or
    `..` segments are refused before the filesystem is touched. The limit is 1000 UTF-8 bytes,
    not characters."""

    @model_validator(mode="after")
    def utf8_size_is_bounded(self) -> ProjectPath:
        if len(self.root.encode("utf-8")) > 1000:
            raise ValueError("project path exceeds 1000 UTF-8 bytes")
        if self.root == ".rift" or self.root.startswith(".rift/"):
            raise ValueError("project path is inside Rift workspace state")
        return self


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class PathSelector(ClosedModel):
    'Which files a query runs over, as two lists of globs matched against the project-relative path. `include: ["src/**"]` selects the source tree; `exclude: ["src/generated/**"]` then removes generated output.'

    include: Field[list[PathPattern]] = proto_field(
        description="Globs a path has to match to be searched at all.",
        number=1,
        json_schema_extra={"uniqueItems": True},
    )
    exclude: Field[list[PathPattern]] = proto_field(
        description="Globs that drop a path `include` already matched.",
        number=2,
        json_schema_extra={"uniqueItems": True},
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class TextRange(ClosedModel):
    "Half-open UTF-8 byte offsets over authoritative UTF-8 source. Every adapter converts from its adapter's native offsets at the seam. No JSON Schema keyword can tie one field to another, so that `end` is never below `start` is asserted by the conformance tests instead."

    start: Field[int] = proto_field(
        description="First byte of the range, counted from the start of the file.",
        ge=0,
        le=4294967295,
        number=1,
    )
    end: Field[int] = proto_field(
        description=(
            "One past the last byte. Equal to `start` for an empty range, which is how a "
            "position between two bytes is spelled."
        ),
        ge=0,
        le=4294967295,
        number=2,
    )


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "Severity",
        (
            EnumValue("error", "SEVERITY_ERROR", 1),
            EnumValue("warning", "SEVERITY_WARNING", 2),
            EnumValue("info", "SEVERITY_INFO", 3),
            EnumValue("hint", "SEVERITY_HINT", 4),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "error": "The adapter would not accept the code. Facts read from around it may be missing.",
            "warning": "The code compiles and the adapter thinks it is wrong anyway.",
            "info": "Something worth knowing that is not a defect.",
            "hint": "A suggestion, usually with a code action behind it.",
        }
    },
)
class Severity(str, Enum):
    "How much a `Diagnostic` matters, in the adapter's own judgement. Adapters map their adapter's levels onto these four, so a caller can drop everything below `warning` without knowing which language produced it."

    ERROR = "error"
    WARNING = "warning"
    INFO = "info"
    HINT = "hint"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class SourceSpan(ClosedModel):
    """A byte range of one file."""

    unit: Field[FileId] = proto_field(
        description="Which file the offsets are into.", number=1
    )
    range: Field[TextRange] = proto_field(
        description="The bytes, as offsets into that file.", number=2
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class TextEdit(ClosedModel):
    "One byte range of one file and what replaces it. Edits in a set share one input state."

    kind: Field[Literal["replace"]] = proto_field(number=1)
    span: Field[SourceSpan] = proto_field(
        description="The file, and the byte range being replaced.", number=2
    )
    text: Field[str] = proto_field(
        description="What the range becomes. Empty deletes it.", number=3
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class EditCreate(ClosedModel):
    """A regular UTF-8 file that does not exist yet, with its complete content and executable bit."""

    kind: Field[Literal["create"]] = proto_field()
    file: Field[FileId] = proto_field(
        description="A path that does not exist in the projection.",
        number=1,
    )
    text: Field[str] = proto_field(
        description="All UTF-8 source in the new file.", number=2
    )
    executable: Field[bool] = proto_field(
        description="Whether the new file is executable.", number=3
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class EditDelete(ClosedModel):
    """Removing a file by path."""

    kind: Field[Literal["delete"]] = proto_field()
    file: Field[FileId] = proto_field(description="The file to remove.", number=1)


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class EditRename(ClosedModel):
    """A move from one path to another."""

    kind: Field[Literal["rename"]] = proto_field()
    file: Field[FileId] = proto_field(
        description="The path the file has now.", number=1
    )
    destination: Field[FileId] = proto_field(
        description="The path it moves to.", number=2
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class EditCopy(ClosedModel):
    "Copying an existing file preserves its bytes and executable bit. A caller that needs different destination content uses `create` with that content."

    kind: Field[Literal["copy"]] = proto_field(description="Tags this as a file copy.")
    file: Field[FileId] = proto_field(
        description="The existing file whose entry is copied.", number=1
    )
    destination: Field[FileId] = proto_field(
        description="A path that does not exist before this edit runs.", number=2
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class EditSetExecutable(ClosedModel):
    """Changing the executable bit without rewriting file content."""

    kind: Field[Literal["set_executable"]] = proto_field(
        description="Tags this as an executable-bit change."
    )
    file: Field[FileId] = proto_field(
        description="The regular file whose executable bit changes.", number=1
    )
    executable: Field[bool] = proto_field(
        description="The executable bit the file has after the edit.", number=2
    )


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant(None, "text_edit", 1, TextEdit),
        Variant("create", "create", 2, EditCreate),
        Variant("delete", "delete", 3, EditDelete),
        Variant("rename", "rename", 4, EditRename),
        Variant("copy", "copy", 5, EditCopy),
        Variant("set_executable", "set_executable", 6, EditSetExecutable),
    ),
)
class Edit(ProtocolRoot):
    "A filesystem effect described before Rift performs it. Edit sets are atomic and sorted bytewise by each edit's RFC 8785 canonical JSON. Text replacements in one set address the same input state and cannot overlap."


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class OriginMappingExact(ClosedModel):
    "One origin, byte for byte. An offset in the generated file maps back to an offset in the source arithmetically."

    precision: Field[Literal["exact"]] = proto_field(default="exact")
    origins: Field[list[Any] | None] = proto_field(
        default=None, min_length=1, max_length=1, number=1
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class OriginMappingApproximate(ClosedModel):
    "One or more origins, without a position inside them. A macro expansion knows which source it came from and cannot say which byte of it."

    precision: Field[Literal["approximate"]] = proto_field(default="approximate")
    origins: Field[list[Any] | None] = proto_field(default=None, min_length=1, number=1)


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class OriginMappingSynthetic(ClosedModel):
    """No origin. The generator invented these bytes, so there is nothing in the workspace to jump to."""

    precision: Field[Literal["synthetic"]] = proto_field(default="synthetic")
    origins: Field[list[Any] | None] = proto_field(default=None, max_length=0, number=1)


@union(
    owner=CORE,
    oneof="variant",
    discriminator="precision",
    variants=(
        Variant("exact", "exact", 1, OriginMappingExact),
        Variant("approximate", "approximate", 2, OriginMappingApproximate),
        Variant("synthetic", "synthetic", 3, OriginMappingSynthetic),
    ),
)
class OriginMapping(ProtocolRoot):
    "A relation from one produced source range to the source ranges that contributed its bytes. It lets Rift project a finding on adapter input back to source a person can edit."


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Kind",
        (
            EnumValue("request", "REQUEST", 1),
            EnumValue("project", "PROJECT", 2),
            EnumValue("dependencies", "DEPENDENCIES", 3),
            EnumValue("all", "ALL", 4),
        ),
        placement=Placement("kind", 1),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "request": (
                "Only what this request asked for. The adapter answered the question put to it "
                "and claims nothing past it."
            ),
            "project": "Every file in the workspace, dependencies left out.",
            "dependencies": "Installed packages outside the workspace source.",
            "all": "Everything the adapter could see: the workspace, its dependencies, and the standard library.",
        }
    },
)
class CoverageScopeKindKind(str, Enum):
    """How far the claim reaches."""

    REQUEST = "request"
    PROJECT = "project"
    DEPENDENCIES = "dependencies"
    ALL = "all"


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class CoverageScopeKind(ClosedModel):
    """A standing scope identified by its name."""

    kind: Field[CoverageScopeKindKind] = proto_field(
        description="How far the claim reaches.",
        number=1,
        json_schema_extra={
            "rift:enumDescriptions": {
                "request": (
                    "Only what this request asked for. The adapter answered the question put to it "
                    "and claims nothing past it."
                ),
                "project": "Every file in the workspace, dependencies left out.",
                "dependencies": "Installed packages outside the workspace source.",
                "all": "Everything the adapter could see: the workspace, its dependencies, and the standard library.",
            }
        },
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class CoverageScopeUnit(ClosedModel):
    """One file. The claim holds for that path and says nothing about any other."""

    kind: Field[Literal["unit"]] = proto_field()
    unit: Field[FileId] = proto_field(
        description="The file the claim is about.", number=1
    )


@union(
    owner=CORE,
    oneof="variant",
    variants=(
        Variant(None, "kind", 1, CoverageScopeKind),
        Variant("unit", "unit", 2, CoverageScopeUnit),
    ),
)
class CoverageScope(ProtocolRoot):
    "What a completeness statement covers — everything the request asked for, one file, or a standing scope the answer holds over."


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class CoverageComplete(ClosedModel):
    """Everything in scope is here, so a fact that is missing is a fact that does not exist."""

    state: Field[Literal["complete"]] = proto_field()
    scope: Field[CoverageScope] = proto_field(
        description="What the claim covers.", number=1
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class CoveragePartial(ClosedModel):
    "Some of what is in scope is missing. `reason` is required here because a caller that reads absence as proof would be wrong."

    state: Field[Literal["partial"]] = proto_field()
    scope: Field[CoverageScope] = proto_field(
        description="What the claim covers.", number=1
    )
    reason: Field[str] = proto_field(
        description=(
            "Why the answer stops short — a limit hit, a file that would not parse, a page "
            "boundary. Prose for a reader; nothing keys on it."
        ),
        max_length=4096,
        number=2,
    )
    continuation: Field[str | None] = proto_field(
        default=None,
        description="How to ask for the rest, where there is a way to.",
        max_length=4096,
        number=3,
    )


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "State",
        (
            EnumValue("unsupported", "UNSUPPORTED", 1),
            EnumValue("not_applicable", "NOT_APPLICABLE", 2),
        ),
        placement=Placement("state", 1),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "unsupported": (
                "The adapter does not produce this family, though the language has the concept. A "
                "later build of it might."
            ),
            "not_applicable": "The language has no such concept because its adapter consumes only physical source.",
        }
    },
)
class CoverageStateState(str, Enum):
    "`unsupported` — the adapter cannot produce this family at all. `not_applicable` — the family has no meaning for this language."

    UNSUPPORTED = "unsupported"
    NOT_APPLICABLE = "not_applicable"


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class CoverageState(ClosedModel):
    """The family was never produced at all, so there is nothing here to be complete about."""

    state: Field[CoverageStateState] = proto_field(
        description=(
            "`unsupported` — the adapter cannot produce this family at all. `not_applicable` "
            "— the family has no meaning for this language."
        ),
        number=1,
        json_schema_extra={
            "rift:enumDescriptions": {
                "unsupported": (
                    "The adapter does not produce this family, though the language has the concept. A "
                    "later build of it might."
                ),
                "not_applicable": "The language has no such concept because its adapter consumes only physical source.",
            }
        },
    )
    scope: Field[CoverageScope] = proto_field(
        description="What the claim covers.", number=2
    )
    reason: Field[str] = proto_field(
        description=(
            "Which of the two this is, in words: the feature the adapter lacks, or the "
            "concept the language lacks."
        ),
        max_length=4096,
        number=3,
    )


@union(
    owner=CORE,
    oneof="variant",
    variants=(
        Variant("complete", "complete", 1, CoverageComplete),
        Variant("partial", "partial", 2, CoveragePartial),
        Variant(None, "state", 3, CoverageState),
    ),
)
class Coverage(ProtocolRoot):
    "How much of one `FactFamily` an answer actually covers. Absence of a fact means the fact does not exist only where the state is `complete`; anywhere else it means Rift did not get that far."


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class TypeExpression(ClosedModel):
    "How a type is written in the source, plus the symbol that declares it when one does. A type with a declaration resolves to that symbol; a structural type — `string | null`, `{ a: string }` — has the spelling and nothing to resolve to."

    language: Field[Language] = proto_field(
        description="The language the spelling is in, and so the adapter that produced it.",
        number=1,
    )
    source: Field[str] = proto_field(
        description="The type as it is written: `Optional[Config]`, `&mut [u8]`, `string | null`.",
        number=2,
    )
    resolved: Field[SymbolId | None] = proto_field(
        description=(
            "The symbol that declares this type, where one does. Null for a structural type, "
            "which has a spelling and nothing to open."
        ),
        number=3,
    )
    extensions: Field[Extensions] = proto_field(
        description="Type facts the model has no field for, namespaced by the adapter that emitted them.",
        number=4,
    )


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Role",
        (
            EnumValue("receiver", "RECEIVER", 1),
            EnumValue("parameter", "PARAMETER", 2),
            EnumValue("return", "RETURN", 3),
            EnumValue("field", "FIELD", 4),
            EnumValue("bound", "BOUND", 5),
            EnumValue("element", "ELEMENT", 6),
            EnumValue("key", "KEY", 7),
            EnumValue("error", "ERROR", 8),
            EnumValue("underlying", "UNDERLYING", 9),
            EnumValue("yielded", "YIELDED", 10),
            EnumValue("awaited", "AWAITED", 11),
            EnumValue("discriminant", "DISCRIMINANT", 12),
        ),
        placement=Placement("role", 1),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "receiver": "The type of the implicit first argument — `self`, `this`.",
            "parameter": "The type of an argument the callable takes.",
            "return": "The type the call yields.",
            "field": "The type of a data member.",
            "bound": "A constraint the type has to satisfy — the `Serialize` in `T: Serialize`.",
            "element": "What a container holds: the `u8` in `Vec<u8>`, the value type of a map.",
            "key": "The key type of a map.",
            "error": "The failure type — the `E` in `Result<T, E>`, a Go `error` return.",
            "underlying": "What an alias or a newtype wraps.",
            "yielded": "What a generator produces per step.",
            "awaited": "What completes inside an asynchronous value — the `T` in `Promise<T>`.",
            "discriminant": "The representation an enumeration is stored as, where the language pins one.",
        }
    },
)
class TypeBindingRole(str, Enum):
    """What this type is to the symbol that carries it."""

    RECEIVER = "receiver"
    PARAMETER = "parameter"
    RETURN_ = "return"
    FIELD = "field"
    BOUND = "bound"
    ELEMENT = "element"
    KEY = "key"
    ERROR = "error"
    UNDERLYING = "underlying"
    YIELDED = "yielded"
    AWAITED = "awaited"
    DISCRIMINANT = "discriminant"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Provenance",
        (
            EnumValue("declared", "DECLARED", 1),
            EnumValue("inferred", "INFERRED", 2),
            EnumValue("expected", "EXPECTED", 3),
        ),
        placement=Placement("provenance", 2),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "declared": "Written in the source by the author.",
            "inferred": "The adapter worked it out; nothing in the source says it.",
            "expected": "What the surrounding context demands here. A mismatch is reported against this one.",
        }
    },
)
class TypeBindingProvenance(str, Enum):
    "Where the type fact came from. A declared type and an inferred one can both be present and disagree, which is the interesting case."

    DECLARED = "declared"
    INFERRED = "inferred"
    EXPECTED = "expected"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class TypeBinding(ClosedModel):
    """One type a symbol carries, together with the role it plays for that symbol."""

    role: Field[TypeBindingRole] = proto_field(
        description="What this type is to the symbol that carries it.",
        number=1,
        json_schema_extra={
            "rift:enumDescriptions": {
                "receiver": "The type of the implicit first argument — `self`, `this`.",
                "parameter": "The type of an argument the callable takes.",
                "return": "The type the call yields.",
                "field": "The type of a data member.",
                "bound": "A constraint the type has to satisfy — the `Serialize` in `T: Serialize`.",
                "element": "What a container holds: the `u8` in `Vec<u8>`, the value type of a map.",
                "key": "The key type of a map.",
                "error": "The failure type — the `E` in `Result<T, E>`, a Go `error` return.",
                "underlying": "What an alias or a newtype wraps.",
                "yielded": "What a generator produces per step.",
                "awaited": "What completes inside an asynchronous value — the `T` in `Promise<T>`.",
                "discriminant": "The representation an enumeration is stored as, where the language pins one.",
            }
        },
    )
    provenance: Field[TypeBindingProvenance] = proto_field(
        description=(
            "Where the type fact came from. A declared type and an inferred one can both be "
            "present and disagree, which is the interesting case."
        ),
        number=2,
        json_schema_extra={
            "rift:enumDescriptions": {
                "declared": "Written in the source by the author.",
                "inferred": "The adapter worked it out; nothing in the source says it.",
                "expected": "What the surrounding context demands here. A mismatch is reported against this one.",
            }
        },
    )
    type: Field[TypeExpression] = proto_field(description="The type itself.", number=3)


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Format",
        (EnumValue("plain", "PLAIN", 1), EnumValue("markdown", "MARKDOWN", 2)),
        placement=Placement("format", 1),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "plain": "No markup. Show the text as it is.",
            "markdown": "Markdown, as the language's own doc tooling writes it.",
        }
    },
)
class DocumentationFormat(str, Enum):
    """Which markup the text is written in, since whoever displays a doc comment is the one that renders it."""

    PLAIN = "plain"
    MARKDOWN = "markdown"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Documentation(ClosedModel):
    """One block of documentation attached to a declaration, in the markup it was written in."""

    format: Field[DocumentationFormat] = proto_field(
        description=(
            "Which markup the text is written in, since whoever displays a doc comment is the "
            "one that renders it."
        ),
        number=1,
        json_schema_extra={
            "rift:enumDescriptions": {
                "plain": "No markup. Show the text as it is.",
                "markdown": "Markdown, as the language's own doc tooling writes it.",
            }
        },
    )
    text: Field[str] = proto_field(
        description="The body of the comment, with the comment syntax stripped.",
        number=2,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Parameter(ClosedModel):
    "One parameter of a `Signature`: what it is called, the types bound to it, and how a call may pass it. A receiver is one of these too, held in its own field because it has no position in the parameter list."

    name: Field[str | None] = proto_field(
        description=(
            "What the parameter is called. Null where the language allows an unnamed one, as "
            "a positional parameter in a function type."
        ),
        number=1,
    )
    node: Field[NodeId | None] = proto_field(
        default=None,
        description="Where this parameter is written in the source.",
        number=2,
    )
    types: Field[list[TypeBinding]] = proto_field(
        description="What it accepts. An array because a declared type and an inferred one are separate bindings.",
        number=3,
    )
    optional: Field[bool] = proto_field(
        description="Whether a call may leave it out.", number=4
    )
    variadic: Field[bool] = proto_field(
        description="Whether it absorbs the arguments that follow — `*args`, `...rest`.",
        number=5,
    )
    default: Field[str | None] = proto_field(
        description="The default value as written in the source. Null where there is none.",
        number=6,
    )
    extensions: Field[Extensions] = proto_field(
        description="Parameter facts the model has no field for, namespaced by the adapter that emitted them.",
        number=7,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Signature(ClosedModel):
    "One callable form of a symbol: the text it renders as, the symbols that text points at, and its structure. Overloads are separate entries."

    display: Field[str] = proto_field(
        description="The signature as a reader sees it, in the language's own syntax.",
        examples=["def load_config(path: str, *, strict: bool = False) -> Config"],
        number=1,
    )
    links: Field[list[SignatureLink]] = proto_field(
        description=(
            "Symbols named inside `display`, each with the byte range of `display` that names "
            "it, so a renderer can turn the rendered text into links."
        ),
        number=2,
    )
    language: Field[Language] = proto_field(
        description="The language whose syntax `display` is written in.", number=3
    )
    receiver: Field[Parameter | None] = proto_field(
        description=(
            "The implicit first parameter — `self`, `this`. Null for a free function, and for "
            "languages that have no such thing."
        ),
        number=4,
    )
    parameters: Field[list[Parameter]] = proto_field(
        description="Declared parameters, in source order.", number=5
    )
    returns: Field[list[TypeBinding]] = proto_field(
        description=(
            "What the call yields. An array because a language may return several values, and "
            "because a declared and an inferred return are separate bindings."
        ),
        number=6,
    )
    type_parameters: Field[list[SymbolId]] = proto_field(
        description="The generic parameters this form declares, each as the symbol that declares it.",
        number=7,
    )
    throws: Field[list[TypeExpression]] = proto_field(
        description="Types this form declares it can raise.", number=8
    )
    effects: Field[list[str]] = proto_field(
        description=(
            "Effect keywords the declaration carries, in the language's own words: `async`, "
            "`unsafe`, `pure`. The spelling is preserved and never mapped onto a portable "
            "meaning."
        ),
        examples=[["async"]],
        number=9,
    )
    extensions: Field[Extensions] = proto_field(
        description="Signature facts the model has no field for, namespaced by the adapter that emitted them.",
        number=10,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class SignatureLink(ClosedModel):
    """One symbol named inside a rendered signature, with the byte range of that rendering which names it."""

    range: Field[TextRange] = proto_field(
        description="Offsets into the rendered string in `Signature.display`.", number=1
    )
    symbol: Field[SymbolId] = proto_field(
        description="The symbol that stretch of text names.", number=2
    )


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "SymbolFacet",
        (
            EnumValue("namespace", "SYMBOL_FACET_NAMESPACE", 1),
            EnumValue("module", "SYMBOL_FACET_MODULE", 2),
            EnumValue("type", "SYMBOL_FACET_TYPE", 3),
            EnumValue("value", "SYMBOL_FACET_VALUE", 4),
            EnumValue("callable", "SYMBOL_FACET_CALLABLE", 5),
            EnumValue("member", "SYMBOL_FACET_MEMBER", 6),
            EnumValue("member_container", "SYMBOL_FACET_MEMBER_CONTAINER", 7),
            EnumValue("parameter", "SYMBOL_FACET_PARAMETER", 8),
            EnumValue("type_parameter", "SYMBOL_FACET_TYPE_PARAMETER", 9),
            EnumValue("constructible", "SYMBOL_FACET_CONSTRUCTIBLE", 10),
            EnumValue("extensible", "SYMBOL_FACET_EXTENSIBLE", 11),
            EnumValue("implementable", "SYMBOL_FACET_IMPLEMENTABLE", 12),
            EnumValue("macro", "SYMBOL_FACET_MACRO", 13),
            EnumValue("test", "SYMBOL_FACET_TEST", 14),
            EnumValue("annotation", "SYMBOL_FACET_ANNOTATION", 15),
            EnumValue("extension", "SYMBOL_FACET_EXTENSION", 16),
            EnumValue("variant", "SYMBOL_FACET_VARIANT", 17),
            EnumValue("enumeration", "SYMBOL_FACET_ENUMERATION", 18),
            EnumValue("alias", "SYMBOL_FACET_ALIAS", 19),
            EnumValue("property", "SYMBOL_FACET_PROPERTY", 20),
            EnumValue("abstract", "SYMBOL_FACET_ABSTRACT", 21),
            EnumValue("constructor", "SYMBOL_FACET_CONSTRUCTOR", 22),
            EnumValue("static", "SYMBOL_FACET_STATIC", 23),
            EnumValue("mutable", "SYMBOL_FACET_MUTABLE", 24),
            EnumValue("public", "SYMBOL_FACET_PUBLIC", 25),
            EnumValue("deprecated", "SYMBOL_FACET_DEPRECATED", 26),
            EnumValue("entrypoint", "SYMBOL_FACET_ENTRYPOINT", 27),
            EnumValue("operator", "SYMBOL_FACET_OPERATOR", 28),
            EnumValue("async", "SYMBOL_FACET_ASYNC", 29),
            EnumValue("generator", "SYMBOL_FACET_GENERATOR", 30),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "namespace": (
                "Groups names without being a unit the language loads — a C++ `namespace`, a "
                "Java package, a SQL schema."
            ),
            "module": (
                "A unit the language imports whole. A TypeScript file carries this facet, and so "
                "do a Rust `mod` and a Go package, both of which can span several files."
            ),
            "type": (
                "Declares a type. `struct Config` and `interface Props` both qualify, which is "
                "what makes one search for types find both."
            ),
            "value": (
                "Names something that exists while the program runs: a variable, a constant, a "
                "function object. A CSS class selector and a SQL sequence carry it too."
            ),
            "callable": "Can be called, and so carries at least one `Signature`.",
            "member": (
                "Owned by another symbol — a method, a field, an enum variant. Ownership is not "
                "lexical: a Go method is written beside its type, and a Rust method inside an "
                "`impl` block."
            ),
            "member_container": "Can hold members: a class, a struct, an interface.",
            "parameter": (
                "A parameter of a callable, declared as a symbol of its own so it can be renamed "
                "and referred to."
            ),
            "type_parameter": (
                "A generic parameter — the `T` in `Vec<T>`. A Rust lifetime and a C++ "
                "`template<int N>` carry it too."
            ),
            "constructible": "Can be instantiated — `new Foo()`, `Foo { .. }`.",
            "extensible": (
                "Can be inherited from or overridden: a non-`final` class, a Kotlin `open fun`, "
                "a C++ `virtual` method."
            ),
            "implementable": "Names a contract another type can satisfy: an interface, a trait, a protocol.",
            "macro": "Expands source at compile time, as `macro_rules!` and preprocessor definitions do.",
            "test": "The language's test tooling collects it as a test: a `#[test]` function, a `describe` block.",
            "annotation": (
                "Declared to be attached to another declaration: a Java `@interface`, a Kotlin "
                "`annotation class`, a Python decorator, a Rust attribute macro."
            ),
            "extension": (
                "Attached to a type declared somewhere else — a Kotlin extension function, a "
                "method in a Rust `impl` block, a Go method, a Dart extension."
            ),
            "variant": "One case of a closed set: a Rust enum variant, a Scala `case`, a Java enum constant.",
            "enumeration": (
                "Declares the closed set those variants belong to. A `match` over it is "
                "exhaustive once every variant is covered."
            ),
            "alias": (
                "Another name for a symbol declared elsewhere — a `typealias`, a C++ `using`, a "
                "re-export. The `aliases` edge says which symbol."
            ),
            "property": (
                "Read and written as a value, and backed by code that runs — a Kotlin property, "
                "a Python `@property`, a JavaScript getter."
            ),
            "abstract": (
                "Declared without an implementation. Whatever carries `implements` or `extends` "
                "to it has to supply one."
            ),
            "constructor": "The callable that produces an instance. `constructible` marks the type it produces.",
            "static": "Bound to the declaration rather than to an instance, so a call needs no receiver.",
            "mutable": (
                "Can be reassigned after it is initialized. A Rust `let mut` carries it, a `let` "
                "does not."
            ),
            "public": (
                "Reachable from outside the unit that declares it. `visibility` carries the "
                "language's own word, which Go and Python do not have: there the adapter reads "
                "the naming convention."
            ),
            "deprecated": (
                "Marked as no longer to be used, however the language spells it — `@Deprecated`, "
                "`#[deprecated]`, a doc tag."
            ),
            "entrypoint": (
                "Where execution starts: `fn main`, `func main`, a `bin` script named by the "
                "package manifest."
            ),
            "operator": (
                "Invoked through operator syntax — C++ `operator+`, Python `__add__`, a Kotlin "
                "`operator fun`."
            ),
            "async": (
                "Produces a value that completes later. `Signature.effects` keeps the language's "
                "own keyword: `async`, `suspend`."
            ),
            "generator": (
                "Yields a sequence one value at a time instead of returning once — a Python "
                "generator, a Dart `sync*` function."
            ),
        }
    },
)
class SymbolFacet(str, Enum):
    "One portable category a symbol falls into. Kinds are language-specific; facets are shared, so a filter written once applies to every language an adapter is installed for."

    NAMESPACE = "namespace"
    MODULE = "module"
    TYPE = "type"
    VALUE = "value"
    CALLABLE = "callable"
    MEMBER = "member"
    MEMBER_CONTAINER = "member_container"
    PARAMETER = "parameter"
    TYPE_PARAMETER = "type_parameter"
    CONSTRUCTIBLE = "constructible"
    EXTENSIBLE = "extensible"
    IMPLEMENTABLE = "implementable"
    MACRO = "macro"
    TEST = "test"
    ANNOTATION = "annotation"
    EXTENSION = "extension"
    VARIANT = "variant"
    ENUMERATION = "enumeration"
    ALIAS = "alias"
    PROPERTY = "property"
    ABSTRACT = "abstract"
    CONSTRUCTOR = "constructor"
    STATIC = "static"
    MUTABLE = "mutable"
    PUBLIC = "public"
    DEPRECATED = "deprecated"
    ENTRYPOINT = "entrypoint"
    OPERATOR = "operator"
    ASYNC = "async"
    GENERATOR = "generator"


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern="^[A-Za-z][A-Za-z0-9._-]*$",
    examples=["create_table", "selector.class", "mapping.key"],
)
class ExactKind(ProtocolRoot):
    """An adapter-local kind preserving the construct name used by that language implementation."""


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "NodeFacet",
        (
            EnumValue("declaration", "NODE_FACET_DECLARATION", 1),
            EnumValue("definition", "NODE_FACET_DEFINITION", 2),
            EnumValue("body", "NODE_FACET_BODY", 3),
            EnumValue("block", "NODE_FACET_BLOCK", 4),
            EnumValue("statement", "NODE_FACET_STATEMENT", 5),
            EnumValue("expression", "NODE_FACET_EXPRESSION", 6),
            EnumValue("type_expression", "NODE_FACET_TYPE_EXPRESSION", 7),
            EnumValue("import", "NODE_FACET_IMPORT", 8),
            EnumValue("export", "NODE_FACET_EXPORT", 9),
            EnumValue("parameter", "NODE_FACET_PARAMETER", 10),
            EnumValue("argument", "NODE_FACET_ARGUMENT", 11),
            EnumValue("annotation", "NODE_FACET_ANNOTATION", 12),
            EnumValue("comment", "NODE_FACET_COMMENT", 13),
            EnumValue("identifier", "NODE_FACET_IDENTIFIER", 14),
            EnumValue("literal", "NODE_FACET_LITERAL", 15),
            EnumValue("pattern", "NODE_FACET_PATTERN", 16),
            EnumValue("generated", "NODE_FACET_GENERATED", 17),
            EnumValue("test", "NODE_FACET_TEST", 18),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "declaration": "Introduces a name without necessarily giving it a body — a prototype, an `extern` line.",
            "definition": "Gives a name its body. Where jump-to-definition lands.",
            "body": "The implementation part of a declaration, after its header.",
            "block": "A run of statements that opens a scope, however the language delimits one.",
            "statement": "One step of execution.",
            "expression": "Something that evaluates to a value.",
            "type_expression": "A type as written, in a position where the language expects one.",
            "import": "Brings a name in from another unit.",
            "export": "Makes a name visible outside this unit.",
            "parameter": "A parameter as written in a declaration.",
            "argument": "A value as written at a call site.",
            "annotation": "Metadata attached to a declaration: a decorator, an attribute, a Java annotation.",
            "comment": (
                "Text the adapter ignores. A doc comment carries this too; "
                "`RegionRole.documentation` is what ties one to the declaration it describes."
            ),
            "identifier": "A name as written, whether it declares or refers.",
            "literal": "A value written straight into the source — `42`, `true`.",
            "pattern": "A destructuring or matching form: a `match` arm, a binding that pulls fields apart.",
            "generated": (
                "Produced by a tool from something else in the workspace. Destructive policy "
                "gates on this, because the next build overwrites whatever you write here."
            ),
            "test": (
                "Part of the test suite. Refactor scope gates on this, so a change can be "
                "confined to production code or carried across both."
            ),
        }
    },
)
class NodeFacet(str, Enum):
    "Portable structural facets used by filters and policy. `generated` and `test` also constrain destructive operations, reference handling, and refactor scope."

    DECLARATION = "declaration"
    DEFINITION = "definition"
    BODY = "body"
    BLOCK = "block"
    STATEMENT = "statement"
    EXPRESSION = "expression"
    TYPE_EXPRESSION = "type_expression"
    IMPORT_ = "import"
    EXPORT = "export"
    PARAMETER = "parameter"
    ARGUMENT = "argument"
    ANNOTATION = "annotation"
    COMMENT = "comment"
    IDENTIFIER = "identifier"
    LITERAL = "literal"
    PATTERN = "pattern"
    GENERATED = "generated"
    TEST = "test"


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "RegionRole",
        (
            EnumValue("selection", "REGION_ROLE_SELECTION", 1),
            EnumValue("name", "REGION_ROLE_NAME", 2),
            EnumValue("header", "REGION_ROLE_HEADER", 3),
            EnumValue("body", "REGION_ROLE_BODY", 4),
            EnumValue("content", "REGION_ROLE_CONTENT", 5),
            EnumValue("documentation", "REGION_ROLE_DOCUMENTATION", 6),
            EnumValue("enclosing", "REGION_ROLE_ENCLOSING", 7),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "selection": (
                "What an editor should select on arriving here: the name for a declaration, the "
                "whole node otherwise."
            ),
            "name": "The identifier alone. What a rename rewrites.",
            "header": "Everything before the body — the keyword, the name, the parameters, the return type.",
            "body": "The implementation. Replacing it leaves the signature and the documentation standing.",
            "content": (
                "What the node holds where its interior is not code: the text of a comment, the "
                "characters inside a string."
            ),
            "documentation": (
                "The doc comment for this declaration. In most languages it sits outside the "
                "declaration, which is why the node has to point at it."
            ),
            "enclosing": (
                "The node with everything that belongs to it, documentation and annotations "
                "included. What a delete should take."
            ),
        }
    },
)
class RegionRole(str, Enum):
    "One named part of a node. A language marks these out inside a declaration, so an operation can address the body of a function without addressing its documentation."

    SELECTION = "selection"
    NAME = "name"
    HEADER = "header"
    BODY = "body"
    CONTENT = "content"
    DOCUMENTATION = "documentation"
    ENCLOSING = "enclosing"


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "RelationshipFacet",
        (
            EnumValue("contains", "RELATIONSHIP_FACET_CONTAINS", 1),
            EnumValue("declares", "RELATIONSHIP_FACET_DECLARES", 2),
            EnumValue("augments", "RELATIONSHIP_FACET_AUGMENTS", 3),
            EnumValue("references", "RELATIONSHIP_FACET_REFERENCES", 4),
            EnumValue("calls", "RELATIONSHIP_FACET_CALLS", 5),
            EnumValue("constructs", "RELATIONSHIP_FACET_CONSTRUCTS", 6),
            EnumValue("reads", "RELATIONSHIP_FACET_READS", 7),
            EnumValue("writes", "RELATIONSHIP_FACET_WRITES", 8),
            EnumValue("imports", "RELATIONSHIP_FACET_IMPORTS", 9),
            EnumValue("exports", "RELATIONSHIP_FACET_EXPORTS", 10),
            EnumValue("extends", "RELATIONSHIP_FACET_EXTENDS", 11),
            EnumValue("implements", "RELATIONSHIP_FACET_IMPLEMENTS", 12),
            EnumValue("has_type", "RELATIONSHIP_FACET_HAS_TYPE", 13),
            EnumValue("overrides", "RELATIONSHIP_FACET_OVERRIDES", 14),
            EnumValue("aliases", "RELATIONSHIP_FACET_ALIASES", 15),
            EnumValue("generates", "RELATIONSHIP_FACET_GENERATES", 16),
            EnumValue("depends_on", "RELATIONSHIP_FACET_DEPENDS_ON", 17),
            EnumValue("annotated_by", "RELATIONSHIP_FACET_ANNOTATED_BY", 18),
            EnumValue("throws", "RELATIONSHIP_FACET_THROWS", 19),
            EnumValue("catches", "RELATIONSHIP_FACET_CATCHES", 20),
            EnumValue("bounded_by", "RELATIONSHIP_FACET_BOUNDED_BY", 21),
            EnumValue("instantiates", "RELATIONSHIP_FACET_INSTANTIATES", 22),
            EnumValue("specializes", "RELATIONSHIP_FACET_SPECIALIZES", 23),
            EnumValue("overloads", "RELATIONSHIP_FACET_OVERLOADS", 24),
            EnumValue("mixes_in", "RELATIONSHIP_FACET_MIXES_IN", 25),
            EnumValue("embeds", "RELATIONSHIP_FACET_EMBEDS", 26),
            EnumValue("tests", "RELATIONSHIP_FACET_TESTS", 27),
            EnumValue("configures", "RELATIONSHIP_FACET_CONFIGURES", 28),
            EnumValue("binds", "RELATIONSHIP_FACET_BINDS", 29),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "contains": "`from` lexically holds `to` — a module and the functions written in it.",
            "declares": (
                "`from` owns `to`. A class carries this to its methods even where the language "
                "writes them outside its body."
            ),
            "augments": (
                "`from` adds members to `to`, which is declared elsewhere — a Rust `impl` block, "
                "a TypeScript `declare module`, an `ALTER TABLE`, a CSS rule for a selector "
                "another file already styles."
            ),
            "references": (
                "`from` mentions `to` somewhere in the source and nothing narrower fits. The "
                "fallback edge where there is a node to point at."
            ),
            "calls": "`from` invokes `to`.",
            "constructs": "`from` creates an instance of `to`.",
            "reads": "`from` takes the value of `to`. A SQL `SELECT` against a table is this edge.",
            "writes": "`from` assigns to `to`. A SQL `INSERT`, `UPDATE` or `DELETE` is this edge.",
            "imports": (
                "`from` brings `to` in from another unit. A header or a stylesheet that declares "
                "nothing of its own is a synthetic `module` symbol, so `#include` and `@import` "
                "are the same edge as a Python `import`."
            ),
            "exports": "`from` makes `to` visible outside its unit.",
            "extends": "`from` inherits from `to`.",
            "implements": "`from` satisfies the contract `to` declares.",
            "has_type": "`to` is the type of `from`. What jump-to-type-definition follows.",
            "overrides": "`from` replaces an inherited `to`.",
            "aliases": "`from` is another name for `to` — a type alias, a re-export, an `as` rename.",
            "generates": "`to` was produced from `from` by a build step, so editing `to` lasts until the next build.",
            "depends_on": (
                "`from` needs `to` and there is no place in the source to point at — a manifest "
                "entry, a build dependency. The coarsest edge there is."
            ),
            "annotated_by": (
                "`to` is attached to `from` as metadata: an annotation, a decorator, an "
                "attribute."
            ),
            "throws": "`from` can raise `to`.",
            "catches": "`from` handles `to` when something it calls raises it.",
            "bounded_by": (
                "`from` is constrained to satisfy `to` — the `Serialize` in `T: Serialize`, the "
                "`Comparable` in `<T extends Comparable<T>>`."
            ),
            "instantiates": "`from` applies `to` as a type argument — the `Config` in `Vec<Config>`.",
            "specializes": "`from` is a narrower form of `to`, chosen over it where both match.",
            "overloads": (
                "`from` and `to` share a name and differ in signature, and the language picks "
                "between them at the call site."
            ),
            "mixes_in": (
                "`from` takes members from `to` by linearization — a Dart `with` clause, a Scala "
                "mixin. The order they are written in decides which one wins."
            ),
            "embeds": (
                "`from` holds `to` and promotes its members, so they are reachable without "
                "naming the field. Go struct embedding."
            ),
            "tests": (
                "`from` exercises `to`. A test that reaches its target through reflection or "
                "HTTP has no call edge to carry it."
            ),
            "configures": (
                "`from` supplies configuration `to` reads — a `rift.toml` key and the field it "
                "fills, a `tsconfig` path alias and the module it resolves."
            ),
            "binds": (
                "`from` names `to` across a language boundary: a `className` and the CSS rule it "
                "selects, an ORM entity and its table."
            ),
        }
    },
)
class RelationshipFacet(str, Enum):
    "One portable category an edge falls into. The local kinds `import` and `use` can share the `imports` facet, which lets one query cross languages."

    CONTAINS = "contains"
    DECLARES = "declares"
    AUGMENTS = "augments"
    REFERENCES = "references"
    CALLS = "calls"
    CONSTRUCTS = "constructs"
    READS = "reads"
    WRITES = "writes"
    IMPORTS = "imports"
    EXPORTS = "exports"
    EXTENDS = "extends"
    IMPLEMENTS = "implements"
    HAS_TYPE = "has_type"
    OVERRIDES = "overrides"
    ALIASES = "aliases"
    GENERATES = "generates"
    DEPENDS_ON = "depends_on"
    ANNOTATED_BY = "annotated_by"
    THROWS = "throws"
    CATCHES = "catches"
    BOUNDED_BY = "bounded_by"
    INSTANTIATES = "instantiates"
    SPECIALIZES = "specializes"
    OVERLOADS = "overloads"
    MIXES_IN = "mixes_in"
    EMBEDS = "embeds"
    TESTS = "tests"
    CONFIGURES = "configures"
    BINDS = "binds"


@definition(owner=CORE, public=False, proto=Proto.empty(), schema_extra={})
class SymbolOriginWorkspace(ClosedModel):
    """Declared in this workspace's own source. The code you can open and change."""

    kind: Field[Literal["workspace"]] = proto_field()


@definition(owner=CORE, public=False, proto=Proto.empty(), schema_extra={})
class SymbolOriginStdlib(ClosedModel):
    """Declared in the language's standard library and installed with its toolchain."""

    kind: Field[Literal["stdlib"]] = proto_field()


@definition(owner=CORE, public=False, proto=Proto.empty(), schema_extra={})
class SymbolOriginExternal(ClosedModel):
    """Declared outside the workspace, its packages, and the language's standard library."""

    kind: Field[Literal["external"]] = proto_field()


@definition(owner=CORE, public=False, proto=Proto.empty(), schema_extra={})
class SymbolOriginGenerated(ClosedModel):
    """Produced by a build step. Editing it lasts only until it is generated again."""

    kind: Field[Literal["generated"]] = proto_field()


@definition(owner=CORE, public=False, proto=Proto.empty(), schema_extra={})
class SymbolOriginSynthetic(ClosedModel):
    """Invented by the adapter, with no source declaration to read or edit."""

    kind: Field[Literal["synthetic"]] = proto_field()


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class SymbolOriginPackage(ClosedModel):
    "Declared in a dependency. `manager` is the package manager that installed it, so `npm`, `cargo` or `pip`; `name` and `version` are that manager's own. Upgrading the dependency replaces these symbols."

    kind: Field[Literal["package"]] = proto_field()
    manager: Field[str] = proto_field(
        description="The package manager that installed it.",
        examples=["npm", "cargo", "pip"],
        number=1,
    )
    name: Field[str] = proto_field(
        description="The package name, as that manager spells it.",
        max_length=4096,
        examples=["zod", "serde"],
        number=2,
    )
    version: Field[str] = proto_field(
        description="The installed version.", examples=["3.22.4", "1.0.197"], number=3
    )


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("workspace", "workspace", 1, SymbolOriginWorkspace, ProtoEmpty),
        Variant("package", "package", 2, SymbolOriginPackage),
        Variant("stdlib", "stdlib", 3, SymbolOriginStdlib, ProtoEmpty),
        Variant("external", "external", 4, SymbolOriginExternal, ProtoEmpty),
        Variant("generated", "generated", 5, SymbolOriginGenerated, ProtoEmpty),
        Variant("synthetic", "synthetic", 6, SymbolOriginSynthetic, ProtoEmpty),
    ),
)
class SymbolOrigin(ProtocolRoot):
    "Source of a symbol definition. The variant determines whether its source can be read or edited and whether a package upgrade can replace it."


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Symbol(ClosedModel):
    """Adapter-resolved semantic identity. Source structure lives in Node and is connected through Relationship."""

    id: Field[SymbolId] = proto_field(
        description=(
            "Unique identifier of this symbol across the whole workspace, and the URI that "
            "resolves it. Hand it to the symbol resource as it stands."
        ),
        examples=["rift://symbol/python/pkg.util.load_config~1"],
        number=1,
    )
    language: Field[Language] = proto_field(
        description="The language this symbol belongs to.",
        examples=[{"name": "typescript"}, {"name": "sql", "dialect": "postgresql"}],
        number=2,
    )
    name: Field[str] = proto_field(
        description=(
            "The human-readable name, as written in the source: `parseConfig`. Rendered "
            "signatures live in `signatures`."
        ),
        max_length=4096,
        examples=["parseConfig"],
        number=3,
    )
    kind: Field[ExactKind] = proto_field(
        description="What this symbol is in the adapter's vocabulary, such as `trait`, `function`, or `table`.",
        examples=["function", "trait", "table"],
        number=4,
    )
    facets: Field[list[SymbolFacet]] = proto_field(
        description=(
            "Portable classification for cross-language queries. The local kinds `trait` and "
            "`interface` can both carry the `type` facet."
        ),
        examples=[["value", "callable"]],
        number=5,
        json_schema_extra={"uniqueItems": True},
    )
    origin: Field[SymbolOrigin] = proto_field(
        description="Where the symbol comes from: code in this workspace, or a package it depends on.",
        number=6,
    )
    container: Field[SymbolId | None] = proto_field(
        default=None,
        description=(
            "The symbol this one belongs to — the class that owns a method, the module that "
            "owns a function. Ownership is not lexical: a Go method is written beside its type "
            "and a Rust method inside an `impl` block, and both name the type here. Absent at "
            "the top level."
        ),
        examples=["rift://symbol/typescript/src/config.ts:ConfigLoader"],
        number=7,
    )
    modifiers: Field[list[str]] = proto_field(
        description="Language keywords qualifying the declaration: `export`, `async`, `const`.",
        examples=[["export", "async"]],
        number=8,
        json_schema_extra={"uniqueItems": True},
    )
    visibility: Field[str | None] = proto_field(
        description=(
            "How widely the symbol is visible, in the language's own terms — `public`, "
            "`private`, `pub(crate)`. Null where the language has no such concept."
        ),
        examples=["public"],
        number=9,
    )
    types: Field[list[TypeBinding]] = proto_field(
        description=(
            "The types this symbol carries, each tagged with the role it plays: a return "
            "type, a field type, a bound."
        ),
        number=10,
    )
    signatures: Field[list[Signature]] = proto_field(
        description=(
            "One entry per callable form. Where the language dispatches overloads separately "
            "they are separate symbols joined by the `overloads` edge; several entries here are "
            "alternative forms of one dispatch target, as `typing.overload` writes them."
        ),
        number=11,
    )
    documentation: Field[list[Documentation]] = proto_field(
        description="Doc comments attached to the declaration, with the markup format they were written in.",
        number=12,
    )
    extensions: Field[Extensions] = proto_field(
        description="Language-specific facts with no portable equivalent, namespaced by the adapter that emitted them.",
        number=13,
    )
    document_local: Field[bool] = proto_field(
        description=(
            "Whether language semantics confine this symbol to the document that declares it. "
            "The adapter classifies locality from its language model."
        ),
        number=14,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class NodeRegion(ClosedModel):
    """One named part of a node, and the bytes it spans."""

    role: Field[RegionRole] = proto_field(
        description="Which part of the node this is.", number=1
    )
    range: Field[TextRange] = proto_field(
        description="Offsets into the file, on the same scale as `Node.range`.",
        number=2,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Node(ClosedModel):
    "One node of a file's concrete syntax tree. It identifies a source range and adapter-local syntax kind. `symbol` connects the node to semantic identity when the language supplies one."

    id: Field[NodeId] = proto_field(
        description="Unique identifier of this source region, and the URI that resolves it.",
        number=1,
    )
    symbol: Field[SymbolId | None] = proto_field(
        default=None,
        description=(
            "The symbol written at this node. Absent where a node writes no symbol — "
            "punctuation, a keyword, a comment."
        ),
        number=2,
    )
    unit: Field[FileId] = proto_field(
        description="The file the node is written in.", number=3
    )
    language: Field[Language] = proto_field(
        description=(
            "The grammar that produced this node. It belongs to the identity because two "
            "adapters can produce different trees over the same file bytes."
        ),
        number=4,
    )
    kind: Field[ExactKind] = proto_field(
        description=(
            "What the node is in the adapter's vocabulary, such as `fn_item`, `mapping.key`, "
            "or `selector.class`."
        ),
        number=5,
    )
    facets: Field[list[NodeFacet]] = proto_field(
        description=(
            "Portable structural classification, so a query can ask for bodies or imports "
            "without knowing the grammar that produced them."
        ),
        number=6,
        json_schema_extra={"uniqueItems": True},
    )
    range: Field[TextRange] = proto_field(
        description="The bytes it spans, as offsets into the file.", number=7
    )
    regions: Field[list[NodeRegion]] = proto_field(
        description=(
            "The node's named parts, so an operation can rewrite a function body without "
            "touching the documentation above it."
        ),
        number=8,
    )
    parent: Field[NodeId | None] = proto_field(
        default=None,
        description="The region this one is nested inside. Absent at the top level of a unit.",
        number=9,
    )
    extensions: Field[Extensions] = proto_field(
        description="Syntax facts the model has no field for, namespaced by the adapter that emitted them.",
        number=10,
    )


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Derivation",
        (
            EnumValue("resolution", "RESOLUTION", 1),
            EnumValue("syntax", "SYNTAX", 2),
            EnumValue("heuristic", "HEURISTIC", 3),
        ),
        placement=Placement("derivation", 6),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "resolution": (
                "The adapter's own name resolution or type checker produced the edge. It is a "
                "fact about the program."
            ),
            "syntax": (
                "Read off the syntax tree because the adapter did not resolve it — a call on an "
                "untyped receiver matched by name. Repeatable, and still capable of being wrong."
            ),
            "heuristic": "A guess, with `confidence` saying how good a one. Required there and meaningless elsewhere.",
        }
    },
)
class RelationshipDerivation(str, Enum):
    "How this edge was established. Every edge reaches Rift from an adapter; this field records how much the adapter knew. A refactor may use `resolution` directly. Lower levels require another check before rewriting."

    RESOLUTION = "resolution"
    SYNTAX = "syntax"
    HEURISTIC = "heuristic"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Relationship(ClosedModel):
    "One directed edge between two symbols. Its evidence is the nodes it was read from, and its derivation is how much the adapter knew when it was read."

    from_: Field[SymbolId] = proto_field(
        alias="from", description="The symbol the edge starts at.", number=1
    )
    kind: Field[ExactKind] = proto_field(
        description="What the edge is in this adapter's vocabulary, such as `import`, `use`, or `implements`.",
        number=2,
    )
    facets: Field[list[RelationshipFacet]] = proto_field(
        description=(
            "Portable classification, so a query for `imports` finds local kinds such as "
            "`import` and `use` alike."
        ),
        min_length=1,
        number=3,
        json_schema_extra={"uniqueItems": True},
    )
    to: Field[SymbolId] = proto_field(
        description=(
            "The symbol the edge points at. One Rift cannot read carries the `external` "
            "origin; the edge is the same either way."
        ),
        number=4,
    )
    evidence: Field[list[NodeId]] = proto_field(
        description="The nodes this edge was read from.", number=5
    )
    derivation: Field[RelationshipDerivation] = proto_field(
        description=(
            "How this edge was established. Every edge reaches Rift from an adapter; this "
            "field records how much the adapter knew. A refactor may use `resolution` "
            "directly. Lower levels require another check before rewriting."
        ),
        number=6,
        json_schema_extra={
            "rift:enumDescriptions": {
                "resolution": (
                    "The adapter's own name resolution or type checker produced the edge. It is a "
                    "fact about the program."
                ),
                "syntax": (
                    "Read off the syntax tree because the adapter did not resolve it — a call on an "
                    "untyped receiver matched by name. Repeatable, and still capable of being wrong."
                ),
                "heuristic": (
                    "A guess, with `confidence` saying how good a one. Required there and meaningless "
                    "elsewhere."
                ),
            }
        },
    )
    confidence: Field[float | None] = proto_field(
        default=None,
        description="How likely a `heuristic` edge is to hold, from 0 to 1. Absent for any other derivation.",
        ge=0,
        le=1,
        number=7,
    )
    extensions: Field[Extensions] = proto_field(
        description="Edge facts the model has no field for, namespaced by the adapter that emitted them.",
        number=8,
    )


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Op",
        (
            EnumValue("eq", "EQ", 1),
            EnumValue("ne", "NE", 2),
            EnumValue("in", "IN", 3),
            EnumValue("contains", "CONTAINS", 4),
            EnumValue("prefix", "PREFIX", 5),
            EnumValue("regex", "REGEX", 6),
            EnumValue("gt", "GT", 7),
            EnumValue("gte", "GTE", 8),
            EnumValue("lt", "LT", 9),
            EnumValue("lte", "LTE", 10),
            EnumValue("exists", "EXISTS", 11),
        ),
        placement=Placement("op", 2),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "eq": "The field equals the operand.",
            "ne": "The field does not equal the operand.",
            "in": "The field equals one of `values`.",
            "contains": "The field holds the operand: a substring of a string, a member of an array such as `facets`.",
            "prefix": "The field is a string starting with the operand.",
            "regex": "The field matches the `rift-regex` expression in the operand. Rift evaluates it.",
            "gt": "The field is greater than the operand.",
            "gte": "The field is greater than or equal to the operand.",
            "lt": "The field is less than the operand.",
            "lte": "The field is less than or equal to the operand.",
            "exists": "The field is present at all. No operand is read.",
        }
    },
)
class FieldFilterOp(str, Enum):
    "How the operand is compared against the field. What a comparison means follows the field's type, so ordering ops apply only where the values are ordered."

    EQ = "eq"
    NE = "ne"
    IN_ = "in"
    CONTAINS = "contains"
    PREFIX = "prefix"
    REGEX = "regex"
    GT = "gt"
    GTE = "gte"
    LT = "lt"
    LTE = "lte"
    EXISTS = "exists"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class FieldFilter(ClosedModel):
    "A predicate over a standard, namespaced substrate, or diagnostic field. Rift evaluates the regex operation under `rift-regex`, including filters nested in `StructuralCaptureConstraint.semantic`. Structural patterns and path selectors carry separate grammars."

    field: Field[str] = proto_field(
        description=(
            "Which field to test, by its name in this model: `facets`, `origin.kind`, "
            "`severity`. Extension keys and diagnostic fields are addressed the same way."
        ),
        number=1,
    )
    op: Field[FieldFilterOp] = proto_field(
        description=(
            "How the operand is compared against the field. What a comparison means follows "
            "the field's type, so ordering ops apply only where the values are ordered. An "
            "array field such as `facets` takes `contains`, `in` and `exists`; the rest have "
            "no meaning against a list and Rift rejects them."
        ),
        number=2,
        json_schema_extra={
            "rift:enumDescriptions": {
                "eq": "The field equals the operand.",
                "ne": "The field does not equal the operand.",
                "in": (
                    "The field equals one of `values`. Against an array field it holds at least one "
                    "of them, which is how `facets` is asked for any of several at once."
                ),
                "contains": (
                    "The field holds the operand: a substring of a string, a member of an array such "
                    "as `facets`."
                ),
                "prefix": "The field is a string starting with the operand.",
                "regex": "The field matches the `rift-regex` expression in the operand. Rift evaluates it.",
                "gt": "The field is greater than the operand.",
                "gte": "The field is greater than or equal to the operand.",
                "lt": "The field is less than the operand.",
                "lte": "The field is less than or equal to the operand.",
                "exists": "The field is present at all. No operand is read.",
            }
        },
    )
    value: Field[Any | None] = proto_field(
        default=None,
        description="The operand, for every op except `in` and `exists`.",
        number=3,
    )
    values: Field[list[Any] | None] = proto_field(
        default=None, description="The operands for `in`.", number=4
    )


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Direction",
        (
            EnumValue("outgoing", "DIRECTION_OUTGOING", 1),
            EnumValue("incoming", "DIRECTION_INCOMING", 2),
            EnumValue("either", "DIRECTION_EITHER", 3),
        ),
        placement=Placement("direction", 3),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "outgoing": "Edges that start at the entity — what it calls, imports, extends.",
            "incoming": "Edges that point at it — its callers, its implementors.",
            "either": "Edges in both directions.",
        }
    },
)
class RelationFilterDirection(str, Enum):
    """Which way the edge runs, seen from the entity being filtered."""

    OUTGOING = "outgoing"
    INCOMING = "incoming"
    EITHER = "either"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Quantifier",
        (
            EnumValue("exists", "QUANTIFIER_EXISTS", 1),
            EnumValue("not_exists", "QUANTIFIER_NOT_EXISTS", 2),
        ),
        placement=Placement("quantifier", 7),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "exists": "At least one edge matches.",
            "not_exists": (
                'No edge matches. How "a symbol nothing calls" is written. It applies after the '
                "depth bound, so `max_depth: 3` asks about edges within three hops and says "
                "nothing about the fourth."
            ),
        }
    },
)
class RelationFilterQuantifier(str, Enum):
    """Whether a match needs such an edge, or needs there to be none."""

    EXISTS = "exists"
    NOT_EXISTS = "not_exists"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class RelationFilter(ClosedModel):
    """A predicate over an exact advertised relationship kind or portable facet."""

    model_config = closed_config(
        {
            "anyOf": [
                {
                    "description": "Matching by exact kind, in one language's vocabulary.",
                    "required": ["kind"],
                },
                {
                    "description": (
                        "Matching by portable facet, which reaches every language an adapter is installed "
                        "for."
                    ),
                    "required": ["facet"],
                },
            ]
        }
    )
    kind: Field[list[str] | None] = proto_field(
        default=None,
        description="Exact relationship kinds advertised by an adapter. Any listed kind matches.",
        number=1,
    )
    facet: Field[list[RelationshipFacet] | None] = proto_field(
        default=None,
        description="Portable relationship facets. Any listed facet matches.",
        number=2,
    )
    direction: Field[RelationFilterDirection] = proto_field(
        description="Which way the edge runs, seen from the entity being filtered.",
        number=3,
        json_schema_extra={
            "rift:enumDescriptions": {
                "outgoing": "Edges that start at the entity — what it calls, imports, extends.",
                "incoming": "Edges that point at it — its callers, its implementors.",
                "either": "Edges in both directions.",
            }
        },
    )
    target: Field[Filter | None] = proto_field(
        default=None,
        description=(
            "What has to be true of the entity at the other end. Nesting a filter here is how "
            '"callers that are tests" becomes one query.'
        ),
        number=4,
    )
    min_depth: Field[int | None] = proto_field(
        default=None,
        description=(
            "How many edges to walk before a hit counts. Above 1 this asks about indirect "
            "neighbours and skips the direct ones."
        ),
        ge=1,
        number=5,
    )
    max_depth: Field[int | None] = proto_field(
        default=None,
        description=(
            "How many edges a traversal may cross. Call graphs contain cycles and can span "
            "the workspace, so every traversal has this bound. Only edges that compose carry "
            "a depth — `contains`, `declares`, `augments`, `calls`, `imports`, `extends`, "
            "`implements`, `mixes_in`, `embeds`, `depends_on`. A bound above 1 on any other "
            "facet has nothing to walk, and Rift rejects it."
        ),
        ge=1,
        le=100,
        number=6,
    )
    quantifier: Field[RelationFilterQuantifier | None] = proto_field(
        default=None,
        description="Whether a match needs such an edge, or needs there to be none.",
        number=7,
        json_schema_extra={
            "rift:enumDescriptions": {
                "exists": "At least one edge matches.",
                "not_exists": (
                    'No edge matches. How "a symbol nothing calls" is written. It applies after the '
                    "depth bound, so `max_depth: 3` asks about edges within three hops and says "
                    "nothing about the fourth."
                ),
            }
        },
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ElementFilter(ClosedModel):
    "A predicate over one entry of a list-valued field. `Symbol.types` holds several entries, and a filter that tests `role` and the resolved symbol separately would accept a symbol whose return type and whose `Config` came from two different entries. Everything under `where` addresses one entry, so both have to hold of the same one."

    field: Field[str] = proto_field(
        description="The list-valued field to walk, by its name in this model: `types`, `signatures`.",
        examples=["types"],
        number=1,
    )
    where: Field[Filter] = proto_field(
        description="What one entry has to satisfy. Field names inside address the entry, not the entity that holds it.",
        number=2,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class FilterField(ClosedModel):
    """A test on one field of the entity."""

    kind: Field[Literal["field"]] = proto_field()
    field: Field[FieldFilter] = proto_field(
        description="The field and the comparison.", number=1
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class FilterElement(ClosedModel):
    """A test on one entry of a list the entity holds."""

    kind: Field[Literal["element"]] = proto_field()
    element: Field[ElementFilter] = proto_field(
        description="The list to walk, and what one of its entries has to satisfy.",
        number=1,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class FilterRelation(ClosedModel):
    """A test on the edges the entity has."""

    kind: Field[Literal["relation"]] = proto_field()
    relation: Field[RelationFilter] = proto_field(
        description="The edges to look for, and what they must reach.", number=1
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class FilterAll(ClosedModel):
    """Conjunction: every member has to hold."""

    kind: Field[Literal["all"]] = proto_field()
    all: Field[list[Filter]] = proto_field(
        description="The filters that must all hold.", number=1
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class FilterAny(ClosedModel):
    """Disjunction: at least one member has to hold."""

    kind: Field[Literal["any"]] = proto_field()
    any: Field[list[Filter]] = proto_field(
        description="The filters, of which one is enough.", number=1
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class FilterNot(ClosedModel):
    """Negation of what it holds."""

    kind: Field[Literal["not"]] = proto_field()
    not_: Field[Filter] = proto_field(
        alias="not", description="The filter being negated.", number=1
    )


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("field", "field", 1, FilterField),
        Variant("relation", "relation", 2, FilterRelation),
        Variant("all", "all", 3, FilterAll),
        Variant("any", "any", 4, FilterAny),
        Variant("not", "not", 5, FilterNot),
        Variant("element", "element", 6, FilterElement),
    ),
)
class Filter(ProtocolRoot):
    """A recursive typed predicate. Every branch is tagged, so a filter tree parses in one pass."""


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class AddressSymbol(ClosedModel):
    "A symbol, wherever it happens to be written. Addressed this way, a rename reaches every node the adapter knows about."

    kind: Field[Literal["symbol"]] = proto_field()
    symbol: Field[SymbolId] = proto_field(
        description="The symbol the operation applies to.", number=1
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class AddressNode(ClosedModel):
    """One node of one file's syntax tree, optionally narrowed to one of its named parts."""

    kind: Field[Literal["node"]] = proto_field()
    node: Field[NodeId] = proto_field(
        description="The node the operation applies to.", number=1
    )
    region: Field[RegionRole | None] = proto_field(
        default=None,
        description="Which part of the node. Absent, the whole of it.",
        number=2,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class AddressSource(ClosedModel):
    "A byte range, whether or not anything was parsed there. This is what addresses a `LICENSE` file, or a region of a file no adapter claims."

    kind: Field[Literal["source"]] = proto_field()
    span: Field[SourceSpan] = proto_field(
        description="The file, and the bytes in it.", number=1
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class AddressMatch(ClosedModel):
    "A result of a `StructuralQuery`, by the identity it came back with. That identity carries its source state; resolution returns `stale_match` when the session state has moved."

    kind: Field[Literal["match"]] = proto_field()
    match: Field[MatchId] = proto_field(
        description="The match, and the state it was found in.", number=1
    )
    capture: Field[str | None] = proto_field(
        description="Which named capture of the match. Null addresses the match itself.",
        number=2,
    )


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("symbol", "symbol", 1, AddressSymbol),
        Variant("node", "node", 2, AddressNode),
        Variant("source", "source", 3, AddressSource),
        Variant("match", "match", 4, AddressMatch),
    ),
)
class Address(ProtocolRoot):
    "Where an operation applies. Separate union branches distinguish a semantic symbol, a syntax node, a source range, and a retained match."


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Cardinality",
        (EnumValue("one", "ONE", 1), EnumValue("many", "MANY", 2)),
        placement=Placement("cardinality", 2),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "one": "Exactly one node, as `$NAME` binds.",
            "many": "A run of nodes, as `$$$NAME` binds.",
        }
    },
)
class StructuralCaptureConstraintCardinality(str, Enum):
    """How many nodes the capture binds, which follows the sigil used in the pattern."""

    ONE = "one"
    MANY = "many"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class StructuralCaptureConstraint(ClosedModel):
    "A condition one capture of a `StructuralQuery` has to meet. Constraints are where a pattern over syntax reaches the adapter's knowledge: match every call, then keep only the ones whose target carries the `test` facet."

    capture: Field[str] = proto_field(
        description="Which metavariable of the pattern this constrains, written without the `$`.",
        pattern="^[A-Za-z][A-Za-z0-9_]*$",
        number=1,
    )
    cardinality: Field[StructuralCaptureConstraintCardinality] = proto_field(
        description="How many nodes the capture binds, which follows the sigil used in the pattern.",
        number=2,
        json_schema_extra={
            "rift:enumDescriptions": {
                "one": "Exactly one node, as `$NAME` binds.",
                "many": "A run of nodes, as `$$$NAME` binds.",
            }
        },
    )
    exact_kind: Field[ExactKind | None] = proto_field(
        default=None,
        description="The syntax kind the captured node has to be, in its grammar's own words.",
        number=3,
    )
    facet: Field[NodeFacet | None] = proto_field(
        default=None,
        description="A portable structural facet the captured node has to carry.",
        number=4,
    )
    text_regex: Field[str | None] = proto_field(
        default=None,
        description=(
            "A `rift-regex` expression the capture's text must match. Rift evaluates it with "
            "Unicode character classes, linear-time matching, and no look-around or "
            "backreferences."
        ),
        number=5,
    )
    semantic: Field[Filter | None] = proto_field(
        default=None,
        description=(
            "What has to be true of the symbol written at the capture. The adapter matches "
            "syntax; Rift applies this, because only Rift holds the resolved facts."
        ),
        number=6,
    )


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant(None, "structural_query", 1, "StructuralQuery"),
        Variant(None, "text_query", 2, "TextQuery"),
    ),
)
class MatchQuery(ProtocolRoot):
    "A text or structural pattern, tagged by the engine that evaluates it. Carrying the complete query lets a match key explain its own identity and lets a caller rerun it without retrieving hidden state."


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Mode",
        (EnumValue("literal", "LITERAL", 1), EnumValue("regex", "REGEX", 2)),
        placement=Placement("mode", 4),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "literal": "Match the pattern bytewise, with no special characters or flags.",
            "regex": "Read the pattern using the advertised text-matching dialect and the initial flags below.",
        }
    },
)
class TextQueryMode(str, Enum):
    """How Rift interprets `pattern`."""

    LITERAL = "literal"
    REGEX = "regex"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class TextQuery(ClosedModel):
    "Literal or regular-expression matching over any UTF-8 file. Rift evaluates this query even when no adapter claims the path. Matches are leftmost-first and non-overlapping. After an empty regular-expression match, scanning advances to the next UTF-8 boundary. The query carries its dialect so a stored match key remains self-contained."

    kind: Field[Literal["text"]] = proto_field(
        description="Selects Rift's text matcher.", number=1
    )
    version: Field[Literal[1]] = proto_field(
        description="Version of the query shape and pattern semantics.", number=2
    )
    dialect: Field[Literal["rift-regex"]] = proto_field(
        description=(
            "Regular-expression grammar used in `regex` mode. The separate `version` field "
            "selects its syntax and match semantics. Version 1 has Unicode character classes, "
            "linear-time matching, and no look-around or backreferences. Protocol conformance "
            "fixtures define accepted syntax and match selection."
        ),
        number=3,
    )
    mode: Field[TextQueryMode] = proto_field(
        description="How Rift interprets `pattern`.",
        number=4,
        json_schema_extra={
            "rift:enumDescriptions": {
                "literal": "Match the pattern bytewise, with no special characters or flags.",
                "regex": "Read the pattern using the advertised text-matching dialect and the initial flags below.",
            }
        },
    )
    pattern: Field[str] = proto_field(
        description="Text to find in the selected grammar.", min_length=1, number=5
    )
    case_sensitive: Field[bool] = proto_field(
        description=(
            "Initial case-sensitivity for regular-expression mode. Inline flags may change it "
            "within the pattern."
        ),
        number=6,
    )
    multiline: Field[bool] = proto_field(
        description=(
            "Whether `^` and `$` initially match line boundaries in regular-expression mode. "
            "Inline flags may change it within the pattern."
        ),
        number=7,
    )
    paths: Field[PathSelector] = proto_field(
        description="Files searched by this query.", number=8
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class StructuralQueryWithinSymbol(ClosedModel):
    """Every node that writes one symbol."""

    kind: Field[Literal["symbol"]] = proto_field(description="Selects symbol scope.")
    symbol: Field[SymbolId] = proto_field(number=1)


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class StructuralQueryWithinNode(ClosedModel):
    """One syntax subtree."""

    kind: Field[Literal["node"]] = proto_field(description="Selects node scope.")
    node: Field[NodeId] = proto_field(number=1)


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class StructuralQueryWithinSource(ClosedModel):
    """One source byte range."""

    kind: Field[Literal["source"]] = proto_field(
        description="Selects source-range scope."
    )
    span: Field[SourceSpan] = proto_field(number=1)


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("symbol", "symbol", 1, StructuralQueryWithinSymbol),
        Variant("node", "node", 2, StructuralQueryWithinNode),
        Variant("source", "source", 3, StructuralQueryWithinSource),
    ),
    placement=Placement("within", 6),
    public=False,
)
class StructuralQueryWithin(ProtocolRoot):
    "Optional source boundary for the search. A symbol selects its nodes. A node selects its syntax subtree. A span selects its bytes."


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Overlap",
        (
            EnumValue("outermost", "OUTERMOST", 1),
            EnumValue("innermost", "INNERMOST", 2),
            EnumValue("all_non_overlapping", "ALL_NON_OVERLAPPING", 3),
        ),
        placement=Placement("overlap", 8),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "outermost": "Keep the enclosing match. A chain of nested calls reports once, at the top.",
            "innermost": "Keep the enclosed match, and report the chain at its deepest point.",
            "all_non_overlapping": "Keep every match that does not overlap one already kept.",
        }
    },
)
class StructuralQueryOverlap(str, Enum):
    """Which match to keep when one encloses another, as a nested call chain does."""

    OUTERMOST = "outermost"
    INNERMOST = "innermost"
    ALL_NON_OVERLAPPING = "all_non_overlapping"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class StructuralQuery(ClosedModel):
    "A syntax search parsed with the target language's grammar. `$NAME` stands for one node and `$$$NAME` for a run of nodes. Each adapter uses the shared capture, constraint, and result shapes."

    kind: Field[Literal["structural"]] = proto_field(number=1)
    version: Field[Literal[1]] = proto_field(
        description="Which revision of the metavariable grammar the pattern is written in.",
        number=2,
    )
    language: Field[Language] = proto_field(
        description="Which grammar parses `pattern`, and so which adapter runs the search.",
        number=3,
    )
    pattern: Field[str] = proto_field(
        description=(
            "The pattern, in the target language's own syntax, with metavariables where the "
            "shape matters and the text does not."
        ),
        min_length=1,
        number=4,
    )
    paths: Field[PathSelector] = proto_field(
        description="Which files to search.", number=5
    )
    within: Field[StructuralQueryWithin | None] = proto_field(
        default=None,
        description=(
            "Optional source boundary for the search. A symbol selects its nodes. A node "
            "selects its syntax subtree. A span selects its bytes."
        ),
        number=6,
    )
    constraints: Field[list[StructuralCaptureConstraint]] = proto_field(
        description=(
            "Conditions the captures have to meet. The pattern finds a shape; these narrow it "
            "to the occurrences you meant."
        ),
        number=7,
    )
    overlap: Field[StructuralQueryOverlap] = proto_field(
        description="Which match to keep when one encloses another, as a nested call chain does.",
        number=8,
        json_schema_extra={
            "rift:enumDescriptions": {
                "outermost": "Keep the enclosing match. A chain of nested calls reports once, at the top.",
                "innermost": "Keep the enclosed match, and report the chain at its deepest point.",
                "all_non_overlapping": "Keep every match that does not overlap one already kept.",
            }
        },
    )
    extensions: Field[Extensions] = proto_field(
        description="Query fields the model has no place for, namespaced by the adapter that reads them.",
        number=9,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Capture(ClosedModel):
    """One named capture of a match, and the bytes it bound."""

    name: Field[CaptureName] = proto_field(
        description="Which metavariable of the pattern bound these bytes.", number=1
    )
    spans: Field[list[SourceSpan]] = proto_field(
        description="Where it bound. A capture that matched a run of nodes has one span per node.",
        min_length=1,
        number=2,
    )
    text: Field[str] = proto_field(
        description="The source it bound, as written.", number=3
    )
    entity: Field[NodeId | None] = proto_field(
        description=(
            "The node this capture landed on, or null when the capture is text and nothing "
            "was parsed — as every text-regex capture is. What the node means is read through "
            "`Node.symbol`."
        ),
        number=4,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class StructuralMatchRanges(ClosedModel):
    "The ranges a replacement can be written over, computed by the adapter that parsed the match. Whether the trivia around a node belongs to the edit depends on what you are doing — deleting a list element has to take a separator with it — and only the grammar knows where that trivia ends."

    exact: Field[SourceSpan] = proto_field(
        description="The node itself, with nothing around it. What a replacement in place writes over.",
        number=1,
    )
    leading: Field[SourceSpan] = proto_field(
        description=(
            "The node with the trivia in front of it: the indentation on its line, a "
            "separator that precedes it."
        ),
        number=2,
    )
    trailing: Field[SourceSpan] = proto_field(
        description="The node with what follows it: a trailing comma, the newline that ends its line.",
        number=3,
    )
    both: Field[SourceSpan] = proto_field(
        description=(
            "The node with the trivia on either side. Deleting this leaves no blank line and "
            "no orphaned separator."
        ),
        number=4,
    )


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern="^(?:[A-Za-z][A-Za-z0-9_]*|[0-9]+)$",
)
class CaptureName(ProtocolRoot):
    """Structural captures use identifier names; text regex results may additionally use numeric capture indexes."""


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class DiagnosticRelated(ClosedModel):
    "A second place the adapter wants you to look — the earlier declaration a redefinition conflicts with, the bound that failed. It carries a message and a location, and never a severity of its own, because it is part of one finding."

    message: Field[str] = proto_field(
        description='What to notice there — "first defined here", "required by this bound".',
        max_length=4096,
        number=1,
    )
    span: Field[SourceSpan] = proto_field(description="Where to look.", number=2)


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Tags",
        (
            EnumValue("deprecated", "TAGS_DEPRECATED", 1),
            EnumValue("unnecessary", "TAGS_UNNECESSARY", 2),
        ),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "deprecated": "The code still works and is marked for removal.",
            "unnecessary": "The code has no effect — an unused import, an unreachable branch.",
        }
    },
)
class DiagnosticTagsItemTags(str, Enum):
    DEPRECATED = "deprecated"
    UNNECESSARY = "unnecessary"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Reliability",
        (
            EnumValue("reliable", "RELIABILITY_RELIABLE", 1),
            EnumValue("recovered", "RELIABILITY_RECOVERED", 2),
        ),
        placement=Placement("reliability", 7),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "reliable": "The adapter parsed the file. Facts around this finding stand.",
            "recovered": (
                "The parser repaired the source to keep going, so the tree here is a guess and so "
                "is anything read from it."
            ),
        }
    },
)
class DiagnosticReliability(str, Enum):
    """Whether the facts around this finding came off a clean parse."""

    RELIABLE = "reliable"
    RECOVERED = "recovered"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Continuation",
        (
            EnumValue("repairable", "CONTINUATION_REPAIRABLE", 1),
            EnumValue("unrepairable", "CONTINUATION_UNREPAIRABLE", 2),
            EnumValue("unknown", "CONTINUATION_UNKNOWN", 3),
        ),
        placement=Placement("continuation", 8),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "repairable": (
                "Appending source can make it go away: an unclosed brace, a statement cut off at "
                "the end of the file."
            ),
            "unrepairable": "It stands whatever follows.",
            "unknown": "The adapter does not say.",
        }
    },
)
class DiagnosticContinuation(str, Enum):
    "Whether the finding is an artefact of source that stops mid-way, which is the normal state of a file an agent is halfway through writing."

    REPAIRABLE = "repairable"
    UNREPAIRABLE = "unrepairable"
    UNKNOWN = "unknown"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Diagnostic(ClosedModel):
    """One finding an adapter produced from source. Its code and message retain the adapter's vocabulary."""

    severity: Field[Severity] = proto_field(
        description="How much it matters.", number=1
    )
    code: Field[str | None] = proto_field(
        description=(
            "The adapter's own identifier for this finding — `TS2345`, `E0308`. Null where "
            "the adapter issues none."
        ),
        number=2,
    )
    message: Field[str] = proto_field(
        description="What the adapter said, in its own words.",
        max_length=4096,
        number=3,
    )
    span: Field[SourceSpan | None] = proto_field(
        description="Where it applies. Null for a finding about the file as a whole, or about the build.",
        number=4,
    )
    related: Field[list[DiagnosticRelated]] = proto_field(
        description="Other places the adapter pointed at while explaining this one.",
        number=5,
    )
    tags: Field[list[DiagnosticTagsItemTags]] = proto_field(
        description="Presentation tags for the finding. An editor can render them as strikethrough or grey text.",
        number=6,
        json_schema_extra={"uniqueItems": True},
    )
    reliability: Field[DiagnosticReliability] = proto_field(
        description="Whether the facts around this finding came off a clean parse.",
        number=7,
        json_schema_extra={
            "rift:enumDescriptions": {
                "reliable": "The adapter parsed the file. Facts around this finding stand.",
                "recovered": (
                    "The parser repaired the source to keep going, so the tree here is a guess and so "
                    "is anything read from it."
                ),
            }
        },
    )
    continuation: Field[DiagnosticContinuation] = proto_field(
        description=(
            "Whether the finding is an artefact of source that stops mid-way, which is the "
            "normal state of a file an agent is halfway through writing."
        ),
        number=8,
        json_schema_extra={
            "rift:enumDescriptions": {
                "repairable": (
                    "Appending source can make it go away: an unclosed brace, a statement cut off at "
                    "the end of the file."
                ),
                "unrepairable": "It stands whatever follows.",
                "unknown": "The adapter does not say.",
            }
        },
    )
    extensions: Field[Extensions] = proto_field(
        description="Diagnostic fields the model has no place for, namespaced by the adapter that emitted them.",
        number=9,
    )
    language: Field[Language | None] = proto_field(
        description="Which adapter produced this. Null for a finding Rift itself raised.",
        number=10,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ValidationReport(ClosedModel):
    "One adapter's verdict over a resulting tree. `coverage` states how much the adapter checked, while `valid` records whether the checked scope contains an error. `validation.require` in the workspace-root `rift.toml` decides which reports are required."

    language: Field[Language] = proto_field(
        description="The adapter that produced this verdict.", number=1
    )
    valid: Field[bool] = proto_field(
        description="Whether this adapter accepted every file in `files` under the reported coverage.",
        number=2,
    )
    coverage: Field[Coverage] = proto_field(
        description=(
            "How much of the affected program the adapter checked. The result is incomplete "
            "when `validation.require` names this language and coverage is partial or unsupported."
        ),
        number=3,
    )
    files: Field[list[ProjectPath]] = proto_field(
        description="Affected files this verdict covers, sorted by project path.",
        number=4,
        json_schema_extra={"uniqueItems": True},
    )
    diagnostics: Field[list[Diagnostic]] = proto_field(
        description="Adapter findings produced while checking the resulting tree, sorted by file and source range.",
        number=5,
    )


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "FormattingPolicy",
        (
            EnumValue("preserve", "FORMATTING_POLICY_PRESERVE", 1),
            EnumValue("changed_regions", "FORMATTING_POLICY_CHANGED_REGIONS", 2),
            EnumValue("affected_files", "FORMATTING_POLICY_AFFECTED_FILES", 3),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "preserve": "Keep every byte outside the resolved edits unchanged.",
            "changed_regions": (
                "Run the language formatter over the smallest syntactic regions containing the "
                "resolved edits."
            ),
            "affected_files": "Run the language formatter over every text file touched by the change.",
        }
    },
)
class FormattingPolicy(str, Enum):
    "How formatting participates in one session change."

    PRESERVE = "preserve"
    CHANGED_REGIONS = "changed_regions"
    AFFECTED_FILES = "affected_files"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class MatchCardinality(ClosedModel):
    "How many matches an atomic rewrite accepts. Rift counts against the expected session state and refuses the operation when the count falls outside this interval."

    minimum: Field[int] = proto_field(
        description="Fewest matches required.", ge=0, le=100000, number=1
    )
    maximum: Field[int | None] = proto_field(
        description="Most matches accepted. Null uses the workspace's `max_rewrite_expansions` limit.",
        number=2,
    )


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Tests",
        (
            EnumValue("exclude", "TESTS_EXCLUDE", 1),
            EnumValue("include", "TESTS_INCLUDE", 2),
            EnumValue("only", "TESTS_ONLY", 3),
        ),
        placement=Placement("tests", 2),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "exclude": "Do not inspect or change test source.",
            "include": "Treat production and test source as one action scope.",
            "only": "Confine the action to test source.",
        }
    },
)
class OperationScopeTests(str, Enum):
    """How source carrying the `test` node facet participates."""

    EXCLUDE = "exclude"
    INCLUDE = "include"
    ONLY = "only"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Generated",
        (
            EnumValue("exclude", "GENERATED_EXCLUDE", 1),
            EnumValue("include", "GENERATED_INCLUDE", 2),
        ),
        placement=Placement("generated", 3),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "exclude": "Refuse an action whose complete effect requires generated source.",
            "include": "Permit generated source and require a `generated_code` confirmation.",
        }
    },
)
class OperationScopeGenerated(str, Enum):
    """How source carrying the `generated` node facet participates."""

    EXCLUDE = "exclude"
    INCLUDE = "include"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class OperationScope(ClosedModel):
    "The project source an action may inspect or change. Dependencies remain readable adapter input and are never editable through this scope."

    paths: Field[PathSelector] = proto_field(
        description="Project paths eligible for the action. Every resolved edit must remain inside this selector.",
        number=1,
    )
    tests: Field[OperationScopeTests] = proto_field(
        description="How source carrying the `test` node facet participates.",
        number=2,
        json_schema_extra={
            "rift:enumDescriptions": {
                "exclude": "Do not inspect or change test source.",
                "include": "Treat production and test source as one action scope.",
                "only": "Confine the action to test source.",
            }
        },
    )
    generated: Field[OperationScopeGenerated] = proto_field(
        description="How source carrying the `generated` node facet participates.",
        number=3,
        json_schema_extra={
            "rift:enumDescriptions": {
                "exclude": "Refuse an action whose complete effect requires generated source.",
                "include": "Permit generated source and require a `generated_code` confirmation.",
            }
        },
    )


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Reads",
        (
            EnumValue("refuse", "READS_REFUSE", 1),
            EnumValue("rewrite", "READS_REWRITE", 2),
            EnumValue("remove", "READS_REMOVE", 3),
        ),
        placement=Placement("reads", 1),
    ),
    schema_extra={},
)
class SafeDeletePolicyReads(str, Enum):
    """Disposition for reads, calls, and constructions."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Writes",
        (
            EnumValue("refuse", "WRITES_REFUSE", 1),
            EnumValue("rewrite", "WRITES_REWRITE", 2),
            EnumValue("remove", "WRITES_REMOVE", 3),
        ),
        placement=Placement("writes", 2),
    ),
    schema_extra={},
)
class SafeDeletePolicyWrites(str, Enum):
    """Disposition for assignments and other writes."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Imports",
        (
            EnumValue("refuse", "IMPORTS_REFUSE", 1),
            EnumValue("rewrite", "IMPORTS_REWRITE", 2),
            EnumValue("remove", "IMPORTS_REMOVE", 3),
        ),
        placement=Placement("imports", 3),
    ),
    schema_extra={},
)
class SafeDeletePolicyImports(str, Enum):
    """Disposition for imports, exports, aliases, and re-exports."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Overrides",
        (
            EnumValue("refuse", "OVERRIDES_REFUSE", 1),
            EnumValue("rewrite", "OVERRIDES_REWRITE", 2),
            EnumValue("remove", "OVERRIDES_REMOVE", 3),
        ),
        placement=Placement("overrides", 4),
    ),
    schema_extra={},
)
class SafeDeletePolicyOverrides(str, Enum):
    """Disposition for overrides, implementations, and inherited declarations."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Generated",
        (
            EnumValue("refuse", "GENERATED_REFUSE", 1),
            EnumValue("rewrite", "GENERATED_REWRITE", 2),
            EnumValue("remove", "GENERATED_REMOVE", 3),
        ),
        placement=Placement("generated", 5),
    ),
    schema_extra={},
)
class SafeDeletePolicyGenerated(str, Enum):
    """Disposition for adapter-resolved uses in generated source. Scope must also permit generated files."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Strings",
        (
            EnumValue("refuse", "STRINGS_REFUSE", 1),
            EnumValue("rewrite", "STRINGS_REWRITE", 2),
            EnumValue("remove", "STRINGS_REMOVE", 3),
        ),
        placement=Placement("strings", 6),
    ),
    schema_extra={},
)
class SafeDeletePolicyStrings(str, Enum):
    "Disposition for adapter-classified string references such as reflection names. Unclassified strings are outside the reference set and cannot be claimed by a guarantee."

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Other",
        (
            EnumValue("refuse", "OTHER_REFUSE", 1),
            EnumValue("rewrite", "OTHER_REWRITE", 2),
            EnumValue("remove", "OTHER_REMOVE", 3),
        ),
        placement=Placement("other", 7),
    ),
    schema_extra={},
)
class SafeDeletePolicyOther(str, Enum):
    """Disposition for complete adapter-resolved uses not covered above."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class SafeDeletePolicy(ClosedModel):
    "How an adapter may eliminate each remaining use while deleting a declaration. `refuse` stops when that use exists, `rewrite` replaces it with a valid form selected by the adapter, and `remove` deletes its containing construct at a safe boundary selected by the adapter. Complete reference coverage is mandatory."

    reads: Field[SafeDeletePolicyReads] = proto_field(
        description="Disposition for reads, calls, and constructions.", number=1
    )
    writes: Field[SafeDeletePolicyWrites] = proto_field(
        description="Disposition for assignments and other writes.", number=2
    )
    imports: Field[SafeDeletePolicyImports] = proto_field(
        description="Disposition for imports, exports, aliases, and re-exports.",
        number=3,
    )
    overrides: Field[SafeDeletePolicyOverrides] = proto_field(
        description="Disposition for overrides, implementations, and inherited declarations.",
        number=4,
    )
    generated: Field[SafeDeletePolicyGenerated] = proto_field(
        description="Disposition for adapter-resolved uses in generated source. Scope must also permit generated files.",
        number=5,
    )
    strings: Field[SafeDeletePolicyStrings] = proto_field(
        description=(
            "Disposition for adapter-classified string references such as reflection names. "
            "Unclassified strings are outside the reference set and cannot be claimed by a "
            "guarantee."
        ),
        number=6,
    )
    other: Field[SafeDeletePolicyOther] = proto_field(
        description="Disposition for complete adapter-resolved uses not covered above.",
        number=7,
    )


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Propagation",
        (
            EnumValue("declaration", "DECLARATION", 1),
            EnumValue("callers", "CALLERS", 2),
            EnumValue("overrides", "OVERRIDES", 3),
            EnumValue("all", "ALL", 4),
        ),
        placement=Placement("propagation", 7),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "declaration": "Change only the selected declaration and refuse when existing uses would become invalid.",
            "callers": "Update the declaration and every resolved call site.",
            "overrides": "Update the declaration and its complete override or implementation family.",
            "all": "Update the declaration, callers, overrides, and implementations.",
        }
    },
)
class SignatureChangePropagation(str, Enum):
    """Declarations and calls the adapter must update with the signature."""

    DECLARATION = "declaration"
    CALLERS = "callers"
    OVERRIDES = "overrides"
    ALL = "all"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class SignatureChange(ClosedModel):
    "The desired callable shape for a structured signature action. Existing parameters retain identity through `Parameter.node`; a parameter without a node is new. Language constructs outside this shape travel in `extensions` under an advertised versioned schema."

    receiver: Field[Parameter | None] = proto_field(
        description="Desired implicit receiver, or null for a free callable.", number=1
    )
    parameters: Field[list[Parameter]] = proto_field(
        description=(
            "Desired parameters in declaration order. Reordering existing nodes preserves "
            "their identity while changing their position."
        ),
        number=2,
    )
    returns: Field[list[TypeBinding]] = proto_field(
        description="Desired declared return bindings.", number=3
    )
    type_parameters: Field[list[SymbolId]] = proto_field(
        description=(
            "Desired existing generic parameters in order. Creating a language-specific "
            "generic parameter uses `extensions`."
        ),
        number=4,
    )
    throws: Field[list[TypeExpression]] = proto_field(
        description="Desired declared error or exception types.", number=5
    )
    effects: Field[list[str]] = proto_field(
        description="Desired language effect keywords, in the adapter's spelling.",
        number=6,
        json_schema_extra={"uniqueItems": True},
    )
    propagation: Field[SignatureChangePropagation] = proto_field(
        description="Declarations and calls the adapter must update with the signature.",
        number=7,
        json_schema_extra={
            "rift:enumDescriptions": {
                "declaration": (
                    "Change only the selected declaration and refuse when existing uses would become "
                    "invalid."
                ),
                "callers": "Update the declaration and every resolved call site.",
                "overrides": "Update the declaration and its complete override or implementation family.",
                "all": "Update the declaration, callers, overrides, and implementations.",
            }
        },
    )
    extensions: Field[Extensions] = proto_field(
        description="Versioned language-specific signature fields.", number=8
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class RenameArguments(ClosedModel):
    """Portable arguments for `refactor.rename` actions."""

    name: Field[str] = proto_field(
        description=(
            "New source name. The adapter checks language spelling, collisions, visibility, "
            "and binding changes."
        ),
        min_length=1,
        max_length=4096,
        number=1,
    )
    scope: Field[OperationScope] = proto_field(
        description="Source eligible for propagation. Complete references outside it cause refusal.",
        number=2,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class MoveArgumentsDestinationAddress(ClosedModel):
    kind: Field[Literal["address"]] = proto_field()
    address: Field[Address] = proto_field(
        description="Existing semantic container that receives the moved declaration.",
        number=1,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class MoveArgumentsDestinationPath(ClosedModel):
    kind: Field[Literal["path"]] = proto_field()
    path: Field[ProjectPath] = proto_field(
        description="Project path that receives a file, module, or declaration supported by the language.",
        number=1,
    )


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("address", "address", 1, MoveArgumentsDestinationAddress),
        Variant("path", "path", 2, MoveArgumentsDestinationPath),
    ),
    placement=Placement("destination", 1),
    public=False,
)
class MoveArgumentsDestination(ProtocolRoot):
    "Existing destination container or project path. Symbols use an Address; files and new modules use a ProjectPath."


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class MoveArguments(ClosedModel):
    """Portable arguments for `refactor.move` actions."""

    destination: Field[MoveArgumentsDestination] = proto_field(
        description=(
            "Existing destination container or project path. Symbols use an Address; files "
            "and new modules use a ProjectPath."
        ),
        number=1,
    )
    scope: Field[OperationScope] = proto_field(
        description="Source eligible for import and reference updates.", number=2
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class SafeDeleteArguments(ClosedModel):
    "Portable arguments for deleting a declaration. `policy` is what separates the two kinds of removal: without it the declaration goes and nothing analyses what referred to it, and with it the adapter must classify every remaining use and dispose of it as stated."

    policy: Field[SafeDeletePolicy | None] = proto_field(
        default=None,
        description=(
            "Required behavior for every classified remaining use. Null removes the declaration "
            "without reference analysis, which claims no reference guarantee and reports no "
            "remaining-usage precondition."
        ),
        number=1,
    )
    scope: Field[OperationScope] = proto_field(
        description="Source in which complete usage analysis and any requested rewrite may occur.",
        number=2,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ChangeSignatureArguments(ClosedModel):
    """Portable arguments for `refactor.change_signature` actions."""

    signature: Field[SignatureChange] = proto_field(
        description="Desired callable structure and propagation policy.", number=1
    )
    scope: Field[OperationScope] = proto_field(
        description="Source eligible for caller and override propagation.", number=2
    )


@definition(
    owner=CORE,
    public=False,
    proto=Proto.message(),
    schema_extra={"rift:arguments": "RenameArguments"},
)
class ArgumentContractRename(ClosedModel):
    """Arguments conform to RenameArguments at the stated contract version."""

    name: Field[Literal["rename"]] = proto_field()
    version: Field[Literal[1]] = proto_field(number=1)


@definition(
    owner=CORE,
    public=False,
    proto=Proto.message(),
    schema_extra={"rift:arguments": "MoveArguments"},
)
class ArgumentContractMove(ClosedModel):
    """Arguments conform to MoveArguments at the stated contract version."""

    name: Field[Literal["move"]] = proto_field()
    version: Field[Literal[1]] = proto_field(number=1)


@definition(
    owner=CORE,
    public=False,
    proto=Proto.message(),
    schema_extra={"rift:arguments": "SafeDeleteArguments"},
)
class ArgumentContractSafeDelete(ClosedModel):
    """Arguments conform to SafeDeleteArguments at the stated contract version."""

    name: Field[Literal["safe_delete"]] = proto_field()
    version: Field[Literal[1]] = proto_field(number=1)


@definition(
    owner=CORE,
    public=False,
    proto=Proto.message(),
    schema_extra={"rift:arguments": "ChangeSignatureArguments"},
)
class ArgumentContractChangeSignature(ClosedModel):
    """Arguments conform to ChangeSignatureArguments at the stated contract version."""

    name: Field[Literal["change_signature"]] = proto_field()
    version: Field[Literal[1]] = proto_field(number=1)


@union(
    owner=CORE,
    oneof="variant",
    discriminator="name",
    variants=(
        Variant("rename", "rename", 1, ArgumentContractRename),
        Variant("move", "move", 2, ArgumentContractMove),
        Variant("safe_delete", "safe_delete", 3, ArgumentContractSafeDelete),
        Variant(
            "change_signature", "change_signature", 4, ArgumentContractChangeSignature
        ),
    ),
)
class ArgumentContract(ProtocolRoot):
    "A portable argument contract attached to a discovered adapter action. Its stable name and numeric version are separate fields. The descriptor's self-contained schema may narrow the selected contract for one language without changing its field meanings."


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class OperationVerifierAdapter(ClosedModel):
    """A language adapter checked its own state."""

    kind: Field[Literal["adapter"]] = proto_field()
    language: Field[Language] = proto_field(
        description="Adapter whose adapter performed the check.", number=1
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class OperationVerifierValidator(ClosedModel):
    """One workspace command validator checked a changed tree."""

    kind: Field[Literal["validator"]] = proto_field()
    validator: Field[int] = proto_field(
        description="Zero-based position in workspace validator declarations.",
        ge=0,
        le=4294967295,
        number=1,
    )


@definition(owner=CORE, public=False, proto=Proto.empty(), schema_extra={})
class OperationVerifierRift(ClosedModel):
    """Rift itself checked projection state."""

    kind: Field[Literal["rift"]] = proto_field()


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("rift", "rift", 1, OperationVerifierRift, ProtoEmpty),
        Variant("adapter", "adapter", 2, OperationVerifierAdapter),
        Variant("validator", "validator", 3, OperationVerifierValidator),
    ),
)
class OperationVerifier(ProtocolRoot):
    """The component that checked a precondition or established a guarantee."""


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class PreconditionValueBoolean(ClosedModel):
    """Boolean property such as target existence or writability."""

    kind: Field[Literal["boolean"]] = proto_field()
    value: Field[bool] = proto_field(number=1)


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class PreconditionValueCount(ClosedModel):
    """Non-negative count such as remaining usages."""

    kind: Field[Literal["count"]] = proto_field()
    value: Field[int] = proto_field(ge=0, le=9007199254740991, number=1)


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class PreconditionValueCoverage(ClosedModel):
    """Coverage required or observed for an adapter fact family."""

    kind: Field[Literal["coverage"]] = proto_field()
    value: Field[Coverage] = proto_field(number=1)


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class PreconditionValueText(ClosedModel):
    """Language or policy value whose spelling is itself significant."""

    kind: Field[Literal["text"]] = proto_field()
    value: Field[str] = proto_field(max_length=4096, number=1)


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("boolean", "boolean", 1, PreconditionValueBoolean),
        Variant("count", "count", 2, PreconditionValueCount),
        Variant("coverage", "coverage", 3, PreconditionValueCoverage),
        Variant("text", "text", 4, PreconditionValueText),
    ),
)
class PreconditionValue(ProtocolRoot):
    "A typed value compared by an operation precondition."


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Kind",
        (
            EnumValue("target_exists", "KIND_TARGET_EXISTS", 1),
            EnumValue("state_matches", "KIND_STATE_MATCHES", 2),
            EnumValue("writable", "KIND_WRITABLE", 3),
            EnumValue("references_complete", "KIND_REFERENCES_COMPLETE", 4),
            EnumValue("no_remaining_usages", "KIND_NO_REMAINING_USAGES", 5),
            EnumValue("destination_legal", "KIND_DESTINATION_LEGAL", 6),
            EnumValue("name_available", "KIND_NAME_AVAILABLE", 7),
            EnumValue("language_condition", "KIND_LANGUAGE_CONDITION", 8),
        ),
        placement=Placement("kind", 1),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "target_exists": "Every addressed symbol, node, match, or source range resolves at the expected state.",
            "state_matches": "The state used for discovery still matches the state being resolved.",
            "writable": "Every affected path is inside the project and writable through Rift's workspace handle.",
            "references_complete": "The adapter completely classified the references on which the operation depends.",
            "no_remaining_usages": "No usage remains after the selected safe-delete policy is applied.",
            "destination_legal": "The requested path or semantic container can receive the moved or created entity.",
            "name_available": "The requested name creates no forbidden collision or binding change.",
            "language_condition": "An adapter-defined condition described by diagnostics and extension data.",
        }
    },
)
class OperationPreconditionKind(str, Enum):
    """Condition being checked."""

    TARGET_EXISTS = "target_exists"
    STATE_MATCHES = "state_matches"
    WRITABLE = "writable"
    REFERENCES_COMPLETE = "references_complete"
    NO_REMAINING_USAGES = "no_remaining_usages"
    DESTINATION_LEGAL = "destination_legal"
    NAME_AVAILABLE = "name_available"
    LANGUAGE_CONDITION = "language_condition"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Status",
        (
            EnumValue("satisfied", "STATUS_SATISFIED", 1),
            EnumValue("failed", "STATUS_FAILED", 2),
        ),
        placement=Placement("status", 2),
    ),
    schema_extra={},
)
class OperationPreconditionStatus(str, Enum):
    """Result of this check."""

    SATISFIED = "satisfied"
    FAILED = "failed"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class OperationPrecondition(ClosedModel):
    "One executable condition checked while resolving an operation. Expected and observed values carry explicit value tags."

    kind: Field[OperationPreconditionKind] = proto_field(
        description="Condition being checked.",
        number=1,
        json_schema_extra={
            "rift:enumDescriptions": {
                "target_exists": "Every addressed symbol, node, match, or source range resolves at the expected state.",
                "state_matches": "The state used for discovery still matches the state being resolved.",
                "writable": "Every affected path is inside the project and writable through Rift's workspace handle.",
                "references_complete": "The adapter completely classified the references on which the operation depends.",
                "no_remaining_usages": "No usage remains after the selected safe-delete policy is applied.",
                "destination_legal": "The requested path or semantic container can receive the moved or created entity.",
                "name_available": "The requested name creates no forbidden collision or binding change.",
                "language_condition": "An adapter-defined condition described by diagnostics and extension data.",
            }
        },
    )
    status: Field[OperationPreconditionStatus] = proto_field(
        description="Result of this check.", number=2
    )
    verifier: Field[OperationVerifier] = proto_field(
        description="Component that performed the check.", number=3
    )
    addresses: Field[list[Address]] = proto_field(
        description="Existing semantic or source subjects involved in the condition.",
        number=4,
    )
    paths: Field[list[ProjectPath]] = proto_field(
        description="Project paths involved in the condition, including destinations that do not yet exist.",
        number=5,
        json_schema_extra={"uniqueItems": True},
    )
    expected: Field[PreconditionValue] = proto_field(
        description=(
            "Required value. Examples include the discovered head, zero remaining usages, "
            "or complete coverage."
        ),
        number=6,
    )
    observed: Field[PreconditionValue] = proto_field(
        description="Value found while checking the condition.", number=7
    )
    diagnostics: Field[list[Diagnostic]] = proto_field(
        description="Adapter or Rift findings that explain the observed value.",
        number=8,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class OperationBlockerAddress(ClosedModel):
    """Existing semantic or source target."""

    kind: Field[Literal["address"]] = proto_field()
    address: Field[Address] = proto_field(number=1)


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class OperationBlockerPath(ClosedModel):
    """Project path involved in a collision, ownership refusal, or illegal destination."""

    kind: Field[Literal["path"]] = proto_field()
    path: Field[ProjectPath] = proto_field(number=1)


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class OperationBlockerRelationship(ClosedModel):
    """Adapter relationship the operation could not preserve or rewrite."""

    kind: Field[Literal["relationship"]] = proto_field()
    relationship: Field[Relationship] = proto_field(number=1)


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("address", "address", 1, OperationBlockerAddress),
        Variant("path", "path", 2, OperationBlockerPath),
        Variant("relationship", "relationship", 3, OperationBlockerRelationship),
    ),
)
class OperationBlocker(ProtocolRoot):
    "A concrete subject preventing resolution. The union admits existing code, a path that may not exist, or the adapter edge that cannot be changed safely."


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Kind",
        (
            EnumValue("declaration_created", "DECLARATION_CREATED", 1),
            EnumValue("declaration_removed", "DECLARATION_REMOVED", 2),
            EnumValue("declaration_moved", "DECLARATION_MOVED", 3),
            EnumValue("declaration_changed", "DECLARATION_CHANGED", 4),
            EnumValue("references_updated", "REFERENCES_UPDATED", 5),
            EnumValue("imports_updated", "IMPORTS_UPDATED", 6),
            EnumValue("source_rewritten", "SOURCE_REWRITTEN", 7),
            EnumValue("formatting_applied", "FORMATTING_APPLIED", 8),
        ),
        placement=Placement("kind", 1),
    ),
    schema_extra={},
)
class OperationEffectKind(str, Enum):
    """Portable consequence of the change."""

    DECLARATION_CREATED = "declaration_created"
    DECLARATION_REMOVED = "declaration_removed"
    DECLARATION_MOVED = "declaration_moved"
    DECLARATION_CHANGED = "declaration_changed"
    REFERENCES_UPDATED = "references_updated"
    IMPORTS_UPDATED = "imports_updated"
    SOURCE_REWRITTEN = "source_rewritten"
    FORMATTING_APPLIED = "formatting_applied"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class OperationEffect(ClosedModel):
    "One semantic consequence of a resolved change. Exact bytes remain in `Edit`; this record explains what those bytes did to declarations and resolved relationships."

    kind: Field[OperationEffectKind] = proto_field(
        description="Portable consequence of the change.", number=1
    )
    before: Field[list[Address]] = proto_field(
        description="Subjects in the state before this change. Empty for creation.",
        number=2,
    )
    after: Field[list[Address]] = proto_field(
        description=(
            "Subjects after Rift applies the change and every adapter sharing the projection "
            "acknowledges the new head. Empty for deletion."
        ),
        number=3,
    )
    spans: Field[list[SourceSpan]] = proto_field(
        description=(
            "Source locations demonstrating the effect, pinned to their respective before or "
            "after address state."
        ),
        number=4,
    )
    detail: Field[str] = proto_field(
        description="Concrete account of the semantic consequence.",
        min_length=1,
        max_length=4096,
        number=5,
    )


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "GuaranteeKind",
        (
            EnumValue("syntax_validated", "GUARANTEE_KIND_SYNTAX_VALIDATED", 1),
            EnumValue("bindings_preserved", "GUARANTEE_KIND_BINDINGS_PRESERVED", 2),
            EnumValue("references_updated", "GUARANTEE_KIND_REFERENCES_UPDATED", 3),
            EnumValue("behavior_checked", "GUARANTEE_KIND_BEHAVIOR_CHECKED", 4),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "syntax_validated": "The language parser accepts the affected source under the stated scope.",
            "bindings_preserved": (
                "Unchanged references in scope resolve to the same symbols before and after the "
                "action."
            ),
            "references_updated": (
                "Every resolved reference in scope that should follow the operation now reaches "
                "its intended target."
            ),
            "behavior_checked": (
                "A named static analysis or command validator checked a stated behavioral "
                "property. The evidence is limited to that named property and scope."
            ),
        }
    },
)
class GuaranteeKind(str, Enum):
    """A property an action claims and must establish with scoped evidence when resolved."""

    SYNTAX_VALIDATED = "syntax_validated"
    BINDINGS_PRESERVED = "bindings_preserved"
    REFERENCES_UPDATED = "references_updated"
    BEHAVIOR_CHECKED = "behavior_checked"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Method",
        (
            EnumValue("construction", "CONSTRUCTION", 1),
            EnumValue("adapter", "ADAPTER", 2),
            EnumValue("static_analysis", "STATIC_ANALYSIS", 3),
            EnumValue("validator", "VALIDATOR", 4),
        ),
        placement=Placement("method", 4),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "construction": "Rift established the property directly from its closed edit and transaction rules.",
            "adapter": "The language adapter or resolver checked the transformed program.",
            "static_analysis": "A named language analysis checked a property narrower than compilation.",
            "validator": "A workspace command validator checked the changed tree.",
        }
    },
)
class GuaranteeEvidenceMethod(str, Enum):
    """How the property was established."""

    CONSTRUCTION = "construction"
    ADAPTER = "adapter"
    STATIC_ANALYSIS = "static_analysis"
    VALIDATOR = "validator"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class GuaranteeEvidence(ClosedModel):
    "Evidence establishing an action-advertised or caller-declared guarantee for a resolved change. The record supplies its scope, verifier, method, and findings."

    kind: Field[GuaranteeKind] = proto_field(number=1)
    scope: Field[CoverageScope] = proto_field(
        description="Source over which the guarantee holds.", number=2
    )
    verifier: Field[OperationVerifier] = proto_field(
        description="Component that established the property.", number=3
    )
    method: Field[GuaranteeEvidenceMethod] = proto_field(
        description="How the property was established.",
        number=4,
        json_schema_extra={
            "rift:enumDescriptions": {
                "construction": "Rift established the property directly from its closed edit and transaction rules.",
                "adapter": "The language adapter or resolver checked the transformed program.",
                "static_analysis": "A named language analysis checked a property narrower than compilation.",
                "validator": "A workspace command validator checked the changed tree.",
            }
        },
    )
    detail: Field[str] = proto_field(
        description="Exact property checked and any limit on its interpretation.",
        min_length=1,
        max_length=4096,
        number=5,
    )
    diagnostics: Field[list[Diagnostic]] = proto_field(
        description="Findings produced while establishing the guarantee.", number=6
    )


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Kind",
        (
            EnumValue("destructive", "DESTRUCTIVE", 1),
            EnumValue("large_scope", "LARGE_SCOPE", 2),
            EnumValue("generated_code", "GENERATED_CODE", 3),
            EnumValue("unresolved_reference", "UNRESOLVED_REFERENCE", 4),
            EnumValue("behavior_unknown", "BEHAVIOR_UNKNOWN", 5),
            EnumValue("formatting_scope", "FORMATTING_SCOPE", 6),
            EnumValue("command_validator", "COMMAND_VALIDATOR", 7),
        ),
        placement=Placement("kind", 2),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "destructive": "The change deletes source or replaces an existing file.",
            "large_scope": "The change reaches enough files or symbols to require explicit acknowledgement.",
            "generated_code": "The change touches source marked as generated.",
            "unresolved_reference": "The adapter found a reference it could not classify or update.",
            "behavior_unknown": "Adapter checks establish validity but do not establish equivalent behavior.",
            "formatting_scope": "Formatting reaches outside the changed syntactic regions.",
            "command_validator": "A workspace command validator will check the changed tree.",
        }
    },
)
class ConfirmationRequirementKind(str, Enum):
    """The condition that makes acknowledgement necessary."""

    DESTRUCTIVE = "destructive"
    LARGE_SCOPE = "large_scope"
    GENERATED_CODE = "generated_code"
    UNRESOLVED_REFERENCE = "unresolved_reference"
    BEHAVIOR_UNKNOWN = "behavior_unknown"
    FORMATTING_SCOPE = "formatting_scope"
    COMMAND_VALIDATOR = "command_validator"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ConfirmationRequirement(ClosedModel):
    "One effect a caller must acknowledge before a change becomes visible. Requirements are sorted by kind, source location, title, and detail, then numbered from zero. An acknowledgement applies to a retry of the same operation and expected head."

    id: Field[int] = proto_field(
        description="Zero-based position in the refusal's ordered confirmation list.",
        ge=0,
        le=4294967295,
        number=1,
    )
    kind: Field[ConfirmationRequirementKind] = proto_field(
        description="The condition that makes acknowledgement necessary.",
        number=2,
        json_schema_extra={
            "rift:enumDescriptions": {
                "destructive": "The change deletes source or replaces an existing file.",
                "large_scope": (
                    "The change reaches enough files or symbols to require explicit acknowledgement."
                ),
                "generated_code": "The change touches source marked as generated.",
                "unresolved_reference": "The adapter found a reference it could not classify or update.",
                "behavior_unknown": "Adapter checks establish validity but do not establish equivalent behavior.",
                "formatting_scope": "Formatting reaches outside the changed syntactic regions.",
                "command_validator": "A workspace command validator will check the changed tree.",
            }
        },
    )
    title: Field[str] = proto_field(
        description="A short account of the effect being acknowledged.",
        min_length=1,
        max_length=256,
        number=3,
    )
    detail: Field[str] = proto_field(
        description="The concrete consequence, including the scope or unresolved item that triggered it.",
        min_length=1,
        max_length=4096,
        number=4,
    )
    spans: Field[list[SourceSpan]] = proto_field(
        description=(
            "Source locations that demonstrate the effect. Empty where the condition applies "
            "to the resulting tree as a whole."
        ),
        number=5,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ActionSupport(ClosedModel):
    "One adapter action family advertised for a language. The prefix supports planning. Discovery still decides whether the family applies at an address."

    kind: Field[ActionKind] = proto_field(
        description="Supported action kind prefix, such as `refactor.rename` or `quickfix`.",
        number=1,
    )
    coverage: Field[Coverage] = proto_field(
        description="Languages, files, or constructs for which this family can be discovered and resolved.",
        number=2,
    )
    guarantees: Field[list[GuaranteeKind]] = proto_field(
        description="Guarantee kinds this family may advertise. Each discovered action states its actual subset.",
        number=3,
        json_schema_extra={"uniqueItems": True},
    )


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Applicability",
        (
            EnumValue("always", "ALWAYS", 1),
            EnumValue("conditional", "CONDITIONAL", 2),
            EnumValue("speculative", "SPECULATIVE", 3),
        ),
        placement=Placement("applicability", 4),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "always": (
                "The adapter needs no caller-supplied fact before resolution. Result validation "
                "and advertised guarantees still apply."
            ),
            "conditional": (
                "Correct only under a condition the adapter cannot establish. The condition "
                "appears in the resolved preconditions or confirmations."
            ),
            "speculative": (
                "The adapter inferred intent from incomplete target evidence. The caller inspects "
                "the operation's effects before retrying with acknowledgements."
            ),
        }
    },
)
class ActionDescriptorApplicability(str, Enum):
    """How much has to be checked before applying it."""

    ALWAYS = "always"
    CONDITIONAL = "conditional"
    SPECULATIVE = "speculative"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ActionDescriptor(ClosedModel):
    "An adapter's portable description of one discovered operation. Listing carries no edits because resolution computes only the selected action."

    title: Field[str] = proto_field(
        description="What the action is called, in the words the adapter would show a person.",
        number=1,
    )
    kind: Field[ActionKind] = proto_field(
        description=(
            "What this action does. Filtering by prefix is what makes it useful: an agent "
            "that wants a fix asks for `quickfix` and never sees a refactor."
        ),
        number=2,
    )
    preferred: Field[bool] = proto_field(
        description=(
            "Whether the adapter ranks this as the preferred action at the target. At most "
            "one action in a list is preferred."
        ),
        number=3,
    )
    applicability: Field[ActionDescriptorApplicability] = proto_field(
        description="How much has to be checked before applying it.",
        number=4,
        json_schema_extra={
            "rift:enumDescriptions": {
                "always": (
                    "The adapter needs no caller-supplied fact before resolution. Result validation "
                    "and advertised guarantees still apply."
                ),
                "conditional": (
                    "Correct only under a condition the adapter cannot establish. The condition "
                    "appears in the resolved preconditions or confirmations."
                ),
                "speculative": (
                    "The adapter inferred intent from incomplete target evidence. The caller inspects "
                    "the operation's effects before retrying with acknowledgements."
                ),
            }
        },
    )
    rationale: Field[str] = proto_field(
        description="Why the adapter is offering it here.", number=5
    )
    target: Field[Address] = proto_field(
        description="What the action applies to.", number=6
    )
    argument_contract: Field[ArgumentContract | None] = proto_field(
        description=(
            "Portable argument shape used by a standard refactor, or null when the adapter's "
            "self-contained schema is the complete contract."
        ),
        number=7,
    )
    arguments_schema: Field[dict[str, Any] | None] = proto_field(
        default=None,
        description=(
            "JSON Schema draft 2020-12 for the arguments this action takes. Remote references "
            "are forbidden. When `argument_contract` is set, this schema may add language "
            "constraints while preserving the portable fields and meanings. An adapter always "
            "supplies it; Rift leaves it null in a listing page and carries it in the read of "
            "one offer, so a caller fetches one schema rather than a schema per offer."
        ),
        number=8,
    )
    guarantees: Field[list[GuaranteeKind]] = proto_field(
        description=(
            "Properties this action promises to establish when resolution succeeds. Resolve "
            "must return one complete `GuaranteeEvidence` entry for every kind listed here."
        ),
        number=9,
        json_schema_extra={"uniqueItems": True},
    )
    disabled_reason: Field[str | None] = proto_field(
        description=(
            "Why the action cannot run right now, where the adapter lists it but refuses it. "
            "Null when it can run."
        ),
        number=10,
    )
    diagnostics: Field[list[Diagnostic]] = proto_field(
        description="The findings this action would clear. Empty for an action nothing reported.",
        number=11,
    )
    extensions: Field[Extensions] = proto_field(
        description="Action fields the model has no place for, namespaced by the adapter that emitted them.",
        number=12,
    )


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "FactFamily",
        (
            EnumValue("origin_mappings", "FACT_FAMILY_ORIGIN_MAPPINGS", 1),
            EnumValue("symbols", "FACT_FAMILY_SYMBOLS", 2),
            EnumValue("nodes", "FACT_FAMILY_NODES", 3),
            EnumValue("relationships", "FACT_FAMILY_RELATIONSHIPS", 4),
            EnumValue("types", "FACT_FAMILY_TYPES", 5),
            EnumValue("diagnostics", "FACT_FAMILY_DIAGNOSTICS", 6),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "origin_mappings": (
                "Relations from produced source ranges to the physical or virtual source ranges "
                "that contributed them."
            ),
            "symbols": "The declarations the adapter resolved in the file.",
            "nodes": "The nodes of the file's syntax tree, and which symbol each writes.",
            "relationships": "The edges between symbols the adapter read out of this file.",
            "types": (
                "The types carried by those symbols. Type facts live in `Symbol.types` and "
                "`Signature`; this family records how completely the adapter resolved them."
            ),
            "diagnostics": "What the adapter complained about while reading the file.",
        }
    },
)
class FactFamily(str, Enum):
    "One kind of fact an adapter can emit for a file. Coverage, streaming, and invalidation use the family as their unit."

    ORIGIN_MAPPINGS = "origin_mappings"
    SYMBOLS = "symbols"
    NODES = "nodes"
    RELATIONSHIPS = "relationships"
    TYPES = "types"
    DIAGNOSTICS = "diagnostics"


@mapping(
    owner=CORE,
    root=dict[FactFamily, Coverage],
    placement=Placement("entries", 1),
    json_schema_extra={"minProperties": 6},
)
class SemanticCoverage(ProtocolRoot):
    """Coverage for every fact family. Absence is authoritative only where state is complete."""


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "AdapterOperation",
        (
            EnumValue("analyze", "ADAPTER_OPERATION_ANALYZE", 1),
            EnumValue("validate", "ADAPTER_OPERATION_VALIDATE", 2),
            EnumValue("format", "ADAPTER_OPERATION_FORMAT", 3),
            EnumValue("match", "ADAPTER_OPERATION_MATCH", 4),
            EnumValue("actions", "ADAPTER_OPERATION_ACTIONS", 5),
            EnumValue("resolve", "ADAPTER_OPERATION_RESOLVE", 6),
            EnumValue("sync_virtual", "ADAPTER_OPERATION_SYNC_VIRTUAL", 7),
            EnumValue("execute", "ADAPTER_OPERATION_EXECUTE", 8),
            EnumValue("debug_start", "ADAPTER_OPERATION_DEBUG_START", 9),
            EnumValue("debug_get_frame", "ADAPTER_OPERATION_DEBUG_GET_FRAME", 10),
            EnumValue("debug_stop", "ADAPTER_OPERATION_DEBUG_STOP", 11),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "analyze": "Emit advertised fact families from source.",
            "validate": "Check a changed source closure.",
            "format": "Format selected source.",
            "match": "Run structural matching.",
            "actions": "Discover actions at an address.",
            "resolve": "Resolve a discovered action to edits and evidence.",
            "sync_virtual": "Consume virtual source produced by another adapter.",
            "execute": "Evaluate a caller-provided code block against one adapter state.",
            "debug_start": "Start an inspect-only debugging session for a code block.",
            "debug_get_frame": "Read one retained stack frame from a debugging session.",
            "debug_stop": "Release a debugging session and its runtime state.",
        }
    },
)
class AdapterOperation(str, Enum):
    """Optional adapter operations advertised before Rift issues a call."""

    ANALYZE = "analyze"
    VALIDATE = "validate"
    FORMAT = "format"
    MATCH = "match"
    ACTIONS = "actions"
    RESOLVE = "resolve"
    SYNC_VIRTUAL = "sync_virtual"
    EXECUTE = "execute"
    DEBUG_START = "debug_start"
    DEBUG_GET_FRAME = "debug_get_frame"
    DEBUG_STOP = "debug_stop"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class CodeBlock(ClosedModel):
    """Caller-provided source evaluated against a session projection. It is never written back
    to that projection."""

    source: Field[str] = proto_field(
        description="UTF-8 source to evaluate as one language-specific block.",
        min_length=1,
        max_length=32768,
        number=1,
    )
    working_directory: Field[ProjectPath] = proto_field(
        default="",
        description=(
            "Project-relative directory used as the evaluation working directory. The empty "
            "path selects the project root. It must name a directory in the projection."
        ),
        number=2,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class CapturedText(ClosedModel):
    """A bounded UTF-8 rendering of bytes. Invalid byte sequences become U+FFFD. The digest
    covers the complete raw value, including bytes beyond the captured prefix."""

    text: Field[str] = proto_field(description="Decoded captured prefix.", number=1)
    captured_bytes: Field[int] = proto_field(
        description="Raw bytes represented by `text` before replacement decoding.",
        ge=0,
        number=2,
    )
    total_bytes: Field[int] = proto_field(
        description="Raw bytes in the complete value.", ge=0, number=3
    )
    truncated: Field[bool] = proto_field(
        description="Whether bytes after the captured prefix were omitted from `text`.",
        number=4,
    )
    digest: Field[Digest] = proto_field(
        description="SHA-256 of the complete raw value.", number=5
    )

    @model_validator(mode="after")
    def counts_and_truncation_agree(self) -> CapturedText:
        if self.captured_bytes > self.total_bytes:
            raise ValueError("captured_bytes cannot exceed total_bytes")
        if self.truncated != (self.captured_bytes < self.total_bytes):
            raise ValueError(
                "truncated must say whether captured_bytes is below total_bytes"
            )
        return self


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ExecutionBudget(ClosedModel):
    """Exact server-selected bounds an adapter must enforce for one code-block evaluation.
    The gRPC deadline may be shorter than `timeout_ms`; whichever expires first governs."""

    timeout_ms: Field[int] = proto_field(
        description="Maximum evaluation wall time in milliseconds.",
        ge=1,
        le=86400000,
        number=1,
    )
    output_bytes: Field[int] = proto_field(
        description="Captured prefix limit applied separately to stdout and stderr.",
        ge=0,
        le=16384,
        number=2,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class DebugBudget(ClosedModel):
    """Execution and retained-frame bounds for one debugging session."""

    execution: Field[ExecutionBudget] = proto_field(
        description="Bounds for running and capturing the debugging evaluation.",
        number=1,
    )
    frames: Field[int] = proto_field(
        description="Maximum stack frames the adapter may retain, innermost first.",
        ge=1,
        le=256,
        number=2,
    )
    bindings_per_frame: Field[int] = proto_field(
        description="Maximum arguments plus locals returned for one frame.",
        ge=1,
        le=128,
        number=3,
    )
    value_bytes: Field[int] = proto_field(
        description="Captured prefix limit for each rendered debug binding value.",
        ge=0,
        le=8192,
        number=4,
    )


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "ExecutionStatus",
        (
            EnumValue("completed", "EXECUTION_STATUS_COMPLETED", 1),
            EnumValue("failed", "EXECUTION_STATUS_FAILED", 2),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "completed": "The code block returned normally.",
            "failed": "The language runtime rejected the block or raised an unhandled failure.",
        }
    },
)
class ExecutionStatus(str, Enum):
    """How evaluation of a code block ended. Infrastructure, cancellation, and deadline failures
    remain protocol errors rather than execution statuses."""

    COMPLETED = "completed"
    FAILED = "failed"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ExecutionResult(ClosedModel):
    """The complete bounded observation of one code-block evaluation."""

    status: Field[ExecutionStatus] = proto_field(
        description="Whether the language runtime completed or failed the block.",
        number=1,
    )
    exit_code: Field[int | None] = proto_field(
        description=(
            "Process exit status where the adapter used a process, or null for an in-process "
            "evaluator. A nonzero value accompanies `failed`."
        ),
        number=2,
    )
    stdout: Field[CapturedText] = proto_field(
        description="Bounded standard output from the evaluation.", number=3
    )
    stderr: Field[CapturedText] = proto_field(
        description="Bounded standard error from the evaluation.", number=4
    )
    diagnostics: Field[list[Diagnostic]] = proto_field(
        description="Structured failures produced while compiling or evaluating the block.",
        number=5,
    )

    @model_validator(mode="after")
    def status_and_exit_code_agree(self) -> ExecutionResult:
        if self.status is ExecutionStatus.COMPLETED and self.exit_code not in {None, 0}:
            raise ValueError("completed execution cannot have a nonzero exit_code")
        if self.status is ExecutionStatus.FAILED and self.exit_code == 0:
            raise ValueError("failed execution cannot have exit_code zero")
        return self


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class DebugBinding(ClosedModel):
    """One visible name in a debug frame, rendered by the language adapter. Values are display
    text rather than a portable object graph; future expansion can add a separate child-value
    interface without changing frame identity."""

    name: Field[str] = proto_field(
        description="Name as the language presents it.",
        min_length=1,
        max_length=2048,
        number=1,
    )
    type: Field[str | None] = proto_field(
        description="Adapter-rendered runtime type, or null when unavailable.",
        max_length=1024,
        number=2,
    )
    value: Field[CapturedText] = proto_field(
        description=(
            "Bounded adapter-rendered value. Its digest identifies the complete rendering when "
            "the session budget truncates it."
        ),
        number=3,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class DebugFrame(ClosedModel):
    """One stack frame retained by a debugging session. Depth zero is the innermost frame at
    the inspection stop."""

    depth: Field[int] = proto_field(
        description="Zero-based depth, with the innermost frame first.",
        ge=0,
        le=4294967295,
        number=1,
    )
    name: Field[str] = proto_field(
        description="Function, method, or adapter-local frame name.",
        min_length=1,
        max_length=4096,
        number=2,
    )
    source: Field[SourceSpan | None] = proto_field(
        description=(
            "Source location in the projection, or null for runtime and synthetic code. The "
            "caller-provided block itself has no source span."
        ),
        number=3,
    )
    arguments: Field[list[DebugBinding]] = proto_field(
        description="Visible arguments in adapter order.", number=4
    )
    locals: Field[list[DebugBinding]] = proto_field(
        description="Visible local bindings in adapter order.", number=5
    )


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern="^[a-z][a-z0-9_.-]*$",
    max_length=128,
)
class ActionKind(ProtocolRoot):
    """What an adapter action does, as a dotted hierarchical name. Rift fixes the portable families `quickfix`, `source`, `refactor.rename`, `refactor.move`, `refactor.safe_delete`, `refactor.change_signature`, `refactor.extract`, `refactor.inline`, `refactor.introduce`, `refactor.convert`, and `generate`; adapters may refine them with suffixes. Other roots are reverse-domain namespaced. Prefix filtering selects a whole family."""


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern="^rift://match/[A-Za-z0-9_-]{16,8192}$",
    min_length=30,
    max_length=8206,
    examples=["rift://match/eyJzbmFwc2hvdCI6eyJraW5kIjoiY29tbWl0In19"],
)
class MatchId(ProtocolRoot):
    """Identity of one match at a projection head."""

    @model_validator(mode="after")
    def token_is_canonical(self) -> MatchId:
        validate_base64url(self.root.removeprefix("rift://match/"))
        return self


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern="^rift://action/[A-Za-z0-9_-]{16,8192}$",
    min_length=31,
    max_length=8207,
    examples=["rift://action/eyJsYW5ndWFnZSI6eyJuYW1lIjoicnVzdCJ9fQ"],
)
class ActionOfferId(ProtocolRoot):
    """Identity of one adapter action at a projection head."""

    @model_validator(mode="after")
    def token_is_canonical(self) -> ActionOfferId:
        validate_base64url(self.root.removeprefix("rift://action/"))
        return self


MODELS = (
    ProtocolVersion,
    Digest,
    ProjectionHead,
    ProjectionState,
    LanguageRegion,
    FileId,
    File,
    ProjectEntry,
    NodeId,
    SymbolId,
    Language,
    ExtensionValue,
    ExtensionKey,
    Extensions,
    PathPattern,
    ProjectPath,
    PathSelector,
    TextRange,
    Severity,
    SourceSpan,
    TextEdit,
    Edit,
    OriginMapping,
    CoverageScope,
    Coverage,
    SemanticCoverage,
    TypeExpression,
    TypeBinding,
    Documentation,
    Parameter,
    Signature,
    SignatureLink,
    SymbolFacet,
    ExactKind,
    NodeFacet,
    RegionRole,
    RelationshipFacet,
    SymbolOrigin,
    Symbol,
    NodeRegion,
    Node,
    Relationship,
    FieldFilter,
    RelationFilter,
    Filter,
    Address,
    StructuralCaptureConstraint,
    MatchQuery,
    TextQuery,
    StructuralQuery,
    Capture,
    StructuralMatchRanges,
    CaptureName,
    DiagnosticRelated,
    Diagnostic,
    ValidationReport,
    FormattingPolicy,
    MatchCardinality,
    OperationScope,
    SafeDeletePolicy,
    SignatureChange,
    RenameArguments,
    MoveArguments,
    SafeDeleteArguments,
    ChangeSignatureArguments,
    ArgumentContract,
    OperationVerifier,
    PreconditionValue,
    OperationPrecondition,
    OperationBlocker,
    OperationEffect,
    GuaranteeKind,
    GuaranteeEvidence,
    ConfirmationRequirement,
    ActionSupport,
    ActionDescriptor,
    FactFamily,
    AdapterOperation,
    CodeBlock,
    CapturedText,
    ExecutionBudget,
    DebugBudget,
    ExecutionStatus,
    ExecutionResult,
    DebugBinding,
    DebugFrame,
    ActionKind,
    MatchId,
    ActionOfferId,
)
