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


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^\d{4}-\d{2}-\d{2}$",
    examples=["2026-08-04"],
)
class ProtocolVersion(ProtocolRoot):
    """One released contract snapshot, named by its release date as `YYYY-MM-DD`."""


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern="^[0-9a-f]{64}$",
)
class Digest(ProtocolRoot):
    """SHA-256 of the value being identified, lowercase hex, 64 characters. The contract fixes the algorithm so every consumer produces the same identity for the same bytes."""


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^[a-z][a-z0-9_.-]{0,127}$",
    examples=["rift.syntax", "rift.history"],
)
class ProviderId(ProtocolRoot):
    """Stable identity of one provider implementation. The identity remains unchanged across
    provider restarts and fact revisions, so callers can compare provenance from separate
    answers."""


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://projection/prj_[a-z2-7]{26}$",
    examples=["rift://projection/prj_bbbbbbbbbbbbbbbbbbbbbbbbbb"],
)
class ProjectionId(ProtocolRoot):
    """Identity of one projection, and the URI that resolves it. The server mints it when
    `projection_create` materializes the projection and retires it when `projection_remove`
    deletes the directory. A change request that omits its `projection` field applies to the
    workspace tree itself."""


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ProjectionState(ClosedModel):
    """Summary of one projection. The changed paths and the changes that produced them
    are read from the changes resource, which pages; both fields here are booleans a caller can
    branch on without that read."""

    dirty: Field[bool] = proto_field(
        description="Whether the projection holds anything the workspace does not.",
        number=3,
    )
    unaccepted: Field[bool] = proto_field(
        description=(
            "Whether the changeset holds a change carrying a confirmation. Publication "
            "refuses unless the `publish` call names each such change in `accept`."
        ),
        number=4,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class LanguageRegion(ClosedModel):
    "A byte range of one file and the language used to parse it. The owner of `App.svelte` can mark its script block as TypeScript. A generated file records ranges in its own byte coordinates."

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
    """Identity of one file in the tree a request targets. The path after `rift://file/` is a
    `ProjectPath` in canonical percent-encoding. The server re-validates the decoded path
    wherever a `FileId` arrives, so the `ProjectPath` exclusions hold for every consumer,
    whatever schema its implementation generated from."""

    @model_validator(mode="after")
    def path_is_canonical(self) -> FileId:
        encoded = self.root.removeprefix("rift://file/")
        decoded = unquote_to_bytes(encoded).decode("utf-8")
        ProjectPath.model_validate(decoded)
        canonical = quote(decoded, safe="/!$&'()*+,;=:@-._~")
        if canonical != encoded:
            raise ValueError("file path must use canonical URI encoding")
        return self


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class FileContentRegular(ClosedModel):
    """A physical or generated file with bytes in it. Every node and symbol with readable source comes from this kind."""

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
    "One file and the languages that read it."

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
            "Distinct `Language` values in `regions`, sorted by name and dialect. A file "
            "holding embedded languages advertises each of them."
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
            "read, and where no provider claims the path."
        ),
        number=5,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class ProjectEntryDirectory(ClosedModel):
    """A visible directory, including an empty one."""

    kind: Field[Literal["directory"]] = proto_field()
    path: Field[ProjectPath] = proto_field(
        description="Project-relative directory path. The empty path names the workspace root.",
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
    "One visible directory or file. Visible means below the workspace root, outside `.rift`, and not excluded by the workspace's VCS ignore rules; a symlink is listed and never followed. Visibility governs reads and unjournaled filesystem reconciliation. Publication retains exact paths already recorded by a Rift change even if ignore rules later hide them."


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://node/[A-Za-z][A-Za-z0-9._-]*(?::[A-Za-z][A-Za-z0-9._-]*)?/(?:[A-Za-z0-9._~!$&'()*+,;=:/-]|%[0-9A-F]{2}){1,1000}@\d+-\d+#[0-9a-f]{8}$",
    min_length=27,
    max_length=8192,
    examples=[
        "rift://node/python/pkg/util.py@1204-1266#3f9a1c2e",
        "rift://node/sql:postgresql/db/schema.sql@40-88#a01b23cd",
    ],
)
class NodeId(ProtocolRoot):
    """Identity of one syntax-tree node. The byte range locates the node in the tree the
    request targets; the fragment after `#` is its witness — the first eight lowercase hex
    characters of the SHA-256 of the node's source bytes. Resolution recomputes the witness
    before acting on the address and refuses with a failed `source_unchanged` precondition
    when the bytes have drifted, so an address read from a stale listing cannot splice into
    the wrong code."""

    @model_validator(mode="after")
    def span_is_canonical(self) -> NodeId:
        address = self.root.removeprefix("rift://node/")
        _language, separator, encoded_span = address.partition("/")
        if not separator:
            raise ValueError("node identity requires a language and path")
        match = re.fullmatch(r"(.+)@([0-9]+)-([0-9]+)#[0-9a-f]{8}", encoded_span)
        if match is None:
            raise ValueError("node identity requires a byte range and witness")
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
    pattern=r"^rift://symbol/[A-Za-z][A-Za-z0-9._-]*(?::[A-Za-z][A-Za-z0-9._-]*)?/(?:[A-Za-z0-9._~!$&'()*+,;=:/@-]|%[0-9A-F]{2}){1,1000}$",
    min_length=17,
    max_length=8192,
    examples=[
        "rift://symbol/python/pkg.util.load_config~1",
        "rift://symbol/sql:postgresql/public.users",
        "rift://symbol/css:scss/theme.$accent",
    ],
)
class SymbolId(ProtocolRoot):
    """Identity of one symbol. The name after the language is the provider's stable qualified
    name for the declaration; where the language derives module identity from the file path,
    as TypeScript does, that path is part of the name. A `~N` suffix separates declarations
    the qualified name alone cannot, such as overloads that dispatch separately. A move can
    change the identity when the language includes module path in that qualified name; history
    correlation records the declaration across that change."""

    @model_validator(mode="after")
    def address_is_canonical(self) -> SymbolId:
        address = self.root.removeprefix("rift://symbol/")
        _language, separator, encoded_name = address.partition("/")
        if not separator:
            raise ValueError("symbol identity requires a language and name")
        decoded_name = unquote_to_bytes(encoded_name).decode("utf-8")
        canonical = quote(decoded_name, safe="/!$&'()*+,;=:@-._~")
        if canonical != encoded_name:
            raise ValueError("symbol name must use canonical URI encoding")
        return self


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Language(ClosedModel):
    """A language name and its optional dialect. The pair is the identity facts are filed
    under, so `sql` and `sql:postgresql` are two languages with two symbol spaces."""

    name: Field[str] = proto_field(
        description=(
            "The language name, such as `sql`, `json`, or `css`. Lowercase, so `TypeScript` "
            "and `typescript` cannot split one language into two identity spaces."
        ),
        pattern="^[a-z][a-z0-9._-]*$",
        max_length=64,
        examples=["sql", "json", "css"],
        number=1,
    )
    dialect: Field[str | None] = proto_field(
        default=None,
        description=(
            "A dialect whose syntax or semantics differ within the language, such as "
            "`postgresql`, `jsonc`, or `scss`. Lowercase, as `name` is."
        ),
        pattern="^[a-z][a-z0-9._-]*$",
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
    """Facts a provider carries that the model has no field for, under a reverse-domain key. Keys and values use RFC 8785 canonical JSON. Consumers skip entries they do not implement."""


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
    """One path below the workspace root, using forward slashes and UTF-8 in Unicode NFC — Rift
    normalizes what it emits and what it accepts, and compares byte-for-byte. The empty path
    names the root itself. Absolute paths, backslashes, control characters, empty segments, and
    `.` or `..` segments are refused before the filesystem is touched. The limit is 1000 UTF-8
    bytes, not characters. A workspace holding two entries whose NFC forms are equal fails the
    read that touches them with `content_unavailable`."""

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
    "Half-open UTF-8 byte offsets over authoritative UTF-8 source. Every provider converts from whatever its toolchain counts in at its own boundary, so two toolchains' column numbers arrive here on the same scale. No JSON Schema keyword can tie one field to another, so that `end` is never below `start` is asserted by the conformance tests instead."

    start: Field[int] = proto_field(
        description="First byte of the range, counted from the start of the file.",
        ge=0,
        le=9007199254740991,
        number=1,
    )
    end: Field[int] = proto_field(
        description=(
            "One past the last byte. Equal to `start` for an empty range, which is how a "
            "position between two bytes is spelled."
        ),
        ge=0,
        le=9007199254740991,
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
            "error": "Code the provider would not accept. Facts read from around it may be missing.",
            "warning": "Code that parses and the provider judges wrong anyway.",
            "info": "Something worth knowing that is not a defect.",
            "hint": "A suggestion.",
        }
    },
)
class Severity(str, Enum):
    "How much a `Diagnostic` matters, in the provider's own judgement. Providers map their toolchain's own levels onto these four, so a caller can drop everything below `warning` without knowing which language produced it."

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


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^rift://source/src_[a-z2-7]{26}$",
    examples=["rift://source/src_bbbbbbbbbbbbbbbbbbbbbbbbbb"],
)
class SourceUnitId(ProtocolRoot):
    """Stable identity of one source unit in the source catalog. Rift derives it from the
    resolver identity and that resolver's canonical unit key, so the same installed source
    keeps its identity across catalog revisions."""


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^[a-z][a-z0-9_.-]{0,127}$",
    examples=["rift.sources.project", "rift.sources.cargo"],
)
class SourceResolverId(ProtocolRoot):
    """Stable identity of one source resolver. It identifies catalog revisions independently
    from fact-provider revisions."""


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^(?!/)(?!\.\.?(/|$))(?!.*(/\.\.?)(/|$))[^\\\u0000-\u001F\u007F]+$",
    max_length=4096,
    examples=["src/lib.rs", "pydantic/main.py"],
)
class SourcePath(ProtocolRoot):
    """Path relative to the source location that owns the unit. It never addresses the host
    filesystem directly."""


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class PackageIdentity(ClosedModel):
    """One package as its package manager identifies it."""

    manager: Field[str] = proto_field(
        description="Package manager or ecosystem name.",
        examples=["cargo", "npm", "pypi"],
        max_length=128,
        number=1,
    )
    name: Field[str] = proto_field(
        description="Package name in that ecosystem.",
        examples=["serde", "zod", "pydantic"],
        max_length=4096,
        number=2,
    )
    version: Field[str] = proto_field(
        description="Resolved package version.",
        examples=["1.0.197", "3.22.4", "2.8.2"],
        max_length=4096,
        number=3,
    )


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "SourceLocationKind",
        (
            EnumValue("project", "SOURCE_LOCATION_PROJECT", 1),
            EnumValue("dependency", "SOURCE_LOCATION_DEPENDENCY", 2),
            EnumValue("stdlib", "SOURCE_LOCATION_STDLIB", 3),
            EnumValue("external", "SOURCE_LOCATION_EXTERNAL", 4),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "project": "Source owned by the current workspace, optionally within one local package.",
            "dependency": "Source owned by one resolved dependency.",
            "stdlib": "Source installed with the language toolchain.",
            "external": "Source outside the project, dependency graph, and standard library.",
        }
    },
)
class SourceLocationKind(str, Enum):
    """Which part of the source catalog owns a unit or declaration."""

    PROJECT = "project"
    DEPENDENCY = "dependency"
    STDLIB = "stdlib"
    EXTERNAL = "external"


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class SourceLocationProject(ClosedModel):
    """Source owned by the current workspace."""

    kind: Field[Literal["project"]] = proto_field()
    package: Field[PackageIdentity | None] = proto_field(
        default=None,
        description="Local package that owns the source, or null when no package manifest assigns one.",
        number=1,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class SourceLocationDependency(ClosedModel):
    """Source owned by one resolved dependency."""

    kind: Field[Literal["dependency"]] = proto_field()
    package: Field[PackageIdentity] = proto_field(
        description="Resolved dependency that owns the source.", number=1
    )


@definition(owner=CORE, public=False, proto=Proto.empty(), schema_extra={})
class SourceLocationStdlib(ClosedModel):
    """Source installed with the language toolchain."""

    kind: Field[Literal["stdlib"]] = proto_field()


@definition(owner=CORE, public=False, proto=Proto.empty(), schema_extra={})
class SourceLocationExternal(ClosedModel):
    """Source outside the project, dependency graph, and standard library."""

    kind: Field[Literal["external"]] = proto_field()


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("project", "project", 1, SourceLocationProject),
        Variant("dependency", "dependency", 2, SourceLocationDependency),
        Variant("stdlib", "stdlib", 3, SourceLocationStdlib, ProtoEmpty),
        Variant("external", "external", 4, SourceLocationExternal, ProtoEmpty),
    ),
)
class SourceLocation(ProtocolRoot):
    """Where source belongs. Package ownership is separate from whether source was authored
    or generated."""


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "SourceKind",
        (
            EnumValue("authored", "SOURCE_KIND_AUTHORED", 1),
            EnumValue("generated", "SOURCE_KIND_GENERATED", 2),
            EnumValue("synthetic", "SOURCE_KIND_SYNTHETIC", 3),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "authored": "Original source supplied to the language toolchain.",
            "generated": "Source produced by a generator from other inputs.",
            "synthetic": "Declaration invented by a provider, with no source unit.",
        }
    },
)
class SourceKind(str, Enum):
    """How source or a declaration came to exist."""

    AUTHORED = "authored"
    GENERATED = "generated"
    SYNTHETIC = "synthetic"


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "SourceMappingPrecision",
        (
            EnumValue("exact", "SOURCE_MAPPING_EXACT", 1),
            EnumValue("approximate", "SOURCE_MAPPING_APPROXIMATE", 2),
            EnumValue("synthetic", "SOURCE_MAPPING_SYNTHETIC", 3),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "exact": "One byte-for-byte origin range.",
            "approximate": "One or more contributing ranges without positional equivalence.",
            "synthetic": "No source range produced these bytes.",
        }
    },
)
class SourceMappingPrecision(str, Enum):
    """How precisely generated bytes map back to their inputs."""

    EXACT = "exact"
    APPROXIMATE = "approximate"
    SYNTHETIC = "synthetic"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class SourceUnitSpan(ClosedModel):
    """One byte range in a source-catalog unit."""

    unit: Field[SourceUnitId] = proto_field(
        description="Source unit containing the bytes.", number=1
    )
    range: Field[TextRange] = proto_field(
        description="Half-open UTF-8 byte range in that unit.", number=2
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class SourceMapping(ClosedModel):
    """Mapping from one generated range to the source ranges that produced it."""

    generated: Field[SourceUnitSpan] = proto_field(
        description="Generated bytes being explained.", number=1
    )
    precision: Field[SourceMappingPrecision] = proto_field(
        description="Relationship between generated bytes and their inputs.", number=2
    )
    originals: Field[list[SourceUnitSpan]] = proto_field(
        description="Input ranges, in generator order.", number=3
    )

    @model_validator(mode="after")
    def originals_match_precision(self) -> SourceMapping:
        count = len(self.originals)
        if self.precision is SourceMappingPrecision.EXACT and count != 1:
            raise ValueError("exact source mapping requires one original")
        if self.precision is SourceMappingPrecision.APPROXIMATE and count < 1:
            raise ValueError("approximate source mapping requires an original")
        if self.precision is SourceMappingPrecision.SYNTHETIC and count != 0:
            raise ValueError("synthetic source mapping cannot carry originals")
        return self


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class SourceUnit(ClosedModel):
    """One readable unit discovered before analysis. Source resolvers classify units; fact
    providers consume them without guessing package ownership or generated status."""

    id: Field[SourceUnitId] = proto_field(
        description="Stable source-catalog identity.", number=1
    )
    location: Field[SourceLocation] = proto_field(
        description="Project, dependency, standard-library, or external ownership.", number=2
    )
    path: Field[SourcePath] = proto_field(
        description="Path relative to that location.", number=3
    )
    source_kind: Field[SourceKind] = proto_field(
        description="Whether this unit is authored or generated. A unit cannot be synthetic.",
        number=4,
    )
    languages: Field[list[Language]] = proto_field(
        description="Languages that analyze the unit, sorted by name and dialect.",
        min_length=1,
        number=5,
        json_schema_extra={"uniqueItems": True},
    )
    digest: Field[Digest] = proto_field(
        description="SHA-256 of the unit bytes.", number=6
    )
    generator: Field[str | None] = proto_field(
        default=None,
        description="Generator implementation or build step. Present only for generated units.",
        max_length=4096,
        number=7,
    )
    mappings: Field[list[SourceMapping]] = proto_field(
        description="Mappings back to input units. Empty when the generator supplies none.",
        number=8,
    )

    @model_validator(mode="after")
    def generated_metadata_matches_kind(self) -> SourceUnit:
        if self.source_kind is SourceKind.SYNTHETIC:
            raise ValueError("synthetic declarations have no source unit")
        if self.source_kind is SourceKind.AUTHORED and (
            self.generator is not None or self.mappings
        ):
            raise ValueError("authored source cannot carry generated metadata")
        if self.source_kind is SourceKind.GENERATED and self.generator is None:
            raise ValueError("generated source requires its generator")
        return self


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
        description=(
            "The path it moves to. It must not exist before this edit runs and must differ "
            "from `file`; an intentional overwrite spells as `delete` plus `rename` in one "
            "atomic set."
        ),
        number=2,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class EditCopy(ClosedModel):
    "Copying an existing file preserves its bytes and executable bit. Copying a symlink reproduces the link and never its target. A caller that needs different destination content uses `create` with that content."

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
    """A filesystem effect described before Rift performs it. Edit sets are atomic and sorted
    bytewise by each edit's RFC 8785 canonical JSON.

    Text replacements in one set address the same input state and cannot overlap. An empty
    range at `a` overlaps `[s, e)` only when `s < a < e`, so boundary insertions are legal; two
    edits sharing an insertion point apply in the set's canonical order, and an insertion at
    the start of a replacement lands before the replacement text.

    Application runs in phases: text replacements against the pre-state, then deletes, copies,
    renames, creates, and executable-bit changes. Structural sources read the pre-state, and
    two structural edits contending for one path are `invalid_request` unless a delete freed
    it."""


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "Freshness",
        (
            EnumValue("current", "FRESHNESS_CURRENT", 1),
            EnumValue("stale", "FRESHNESS_STALE", 2),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "current": "Derived state covers the input revisions captured by the answer.",
            "stale": "Derived state covers an earlier input revision while its producer catches up.",
        }
    },
)
class Freshness(str, Enum):
    """Whether derived state covers the tree and source-catalog revisions carried by the answer."""

    CURRENT = "current"
    STALE = "stale"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ProviderProvenance(ClosedModel):
    """The immutable provider state used for one fact-family answer. A provider publishes a
    new `revision` only after it has finished deriving facts from one workspace tree and source
    catalog revision."""

    provider: Field[ProviderId] = proto_field(
        description="Stable identity of the provider that contributed facts.", number=1
    )
    revision: Field[Digest] = proto_field(
        description="SHA-256 identity of the immutable provider fact revision.",
        number=2,
    )
    tree_revision: Field[Digest] = proto_field(
        description="Tree revision from which the provider derived this fact revision.",
        number=3,
    )
    freshness: Field[Freshness] = proto_field(
        description="Whether both input revisions equal the answer's captured revisions.",
        number=4,
    )
    source_revision: Field[Digest] = proto_field(
        description="Source-catalog revision from which the provider derived these facts.",
        number=5,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class IndexSnapshot(ClosedModel):
    """The immutable search-index revision used for an answer."""

    revision: Field[Digest] = proto_field(
        description="SHA-256 identity of the index revision.", number=1
    )
    tree_revision: Field[Digest] = proto_field(
        description="Tree revision indexed by this revision.", number=2
    )
    freshness: Field[Freshness] = proto_field(
        description="Whether both indexed revisions equal the answer's captured revisions.",
        number=3,
    )
    source_revision: Field[Digest] = proto_field(
        description="Source-catalog revision indexed by this revision.", number=4
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ReadSnapshot(ClosedModel):
    """Immutable state captured for one read. Every page produced from one cursor carries the
    same snapshot."""

    tree_revision: Field[Digest] = proto_field(
        description=(
            "SHA-256 identity of the targeted tree when the read began. The digest covers "
            "visible paths, entry kinds, executable bits, and contents."
        ),
        number=1,
    )
    index: Field[IndexSnapshot | None] = proto_field(
        default=None,
        description="Search-index state used by the read, or null when the read did not use the index.",
        number=2,
    )
    source_revision: Field[Digest] = proto_field(
        description="Source-catalog revision captured for the read.", number=3
    )

    @model_validator(mode="after")
    def index_freshness_matches_tree(self) -> ReadSnapshot:
        if self.index is None:
            return self
        same_inputs = (
            self.index.tree_revision == self.tree_revision
            and self.index.source_revision == self.source_revision
        )
        if same_inputs != (self.index.freshness is Freshness.CURRENT):
            raise ValueError("index freshness must match its tree and source revisions")
        return self


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
                "Only what this request asked for. The provider answered the question put to "
                "it and claims nothing past it."
            ),
            "project": "Every file in the workspace, dependencies left out.",
            "dependencies": "Installed packages outside the workspace source.",
            "all": "Everything the provider could see: the workspace, its dependencies, and the standard library.",
        }
    },
)
class CoverageReach(str, Enum):
    """How far the claim reaches."""

    REQUEST = "request"
    PROJECT = "project"
    DEPENDENCIES = "dependencies"
    ALL = "all"


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class CoverageScopeKind(ClosedModel):
    """A standing scope identified by its name."""

    kind: Field[Literal["reach"]] = proto_field()
    reach: Field[CoverageReach] = proto_field(
        description="How far the claim reaches.",
        number=1,
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
    discriminator="kind",
    variants=(
        Variant("reach", "reach", 1, CoverageScopeKind),
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
    provenance: Field[list[ProviderProvenance]] = proto_field(
        description=(
            "Provider revisions that contributed this family, in merge precedence order. "
            "Empty only when Rift establishes coverage without a provider."
        ),
        number=4,
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
    provenance: Field[list[ProviderProvenance]] = proto_field(
        description="Provider revisions that contributed available facts, in merge precedence order.",
        number=4,
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
                "The provider does not produce this family, though the language has the concept."
            ),
            "not_applicable": "The language has no such concept.",
        }
    },
)
class CoverageStateState(str, Enum):
    "`unsupported` — no provider produces this family for the language. `not_applicable` — the family has no meaning for this language."

    UNSUPPORTED = "unsupported"
    NOT_APPLICABLE = "not_applicable"


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class CoverageState(ClosedModel):
    """The family was never produced at all, so there is nothing here to be complete about."""

    state: Field[CoverageStateState] = proto_field(
        description=(
            "`unsupported` — no provider produces this family for the language. "
            "`not_applicable` — the family has no meaning for this language."
        ),
        number=1,
    )
    scope: Field[CoverageScope] = proto_field(
        description="What the claim covers.", number=2
    )
    reason: Field[str] = proto_field(
        description=(
            "Which of the two this is, in words: the feature no provider has, or the "
            "concept the language lacks."
        ),
        max_length=4096,
        number=3,
    )
    provenance: Field[list[ProviderProvenance]] = proto_field(
        description=(
            "Provider revisions consulted before reaching this state. Empty for "
            "`unsupported` and `not_applicable`."
        ),
        number=4,
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
    "How much of one fact family or indexed result an answer covers. Absence proves that no matching fact exists only where the state is `complete`."


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class TypeExpression(ClosedModel):
    "How a type is written in the source, plus the symbol that declares it when one does. A type with a declaration resolves to that symbol; a structural type — `string | null`, `{ a: string }` — has the spelling and nothing to resolve to."

    language: Field[Language] = proto_field(
        description="The language the spelling is in, and so which provider produced it.",
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
        description="Type facts the model has no field for, namespaced by the provider that emitted them.",
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
            "inferred": "The provider worked it out; nothing in the source says it.",
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
    )
    provenance: Field[TypeBindingProvenance] = proto_field(
        description=(
            "Where the type fact came from. A declared type and an inferred one can both be "
            "present and disagree, which is the interesting case."
        ),
        number=2,
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
            "plain": "Unmarked text, rendered as it is.",
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
        description="Parameter facts the model has no field for, namespaced by the provider that emitted them.",
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
        description="Signature facts the model has no field for, namespaced by the provider that emitted them.",
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
                "language's own word, which Go and Python do not have: there the provider "
                "reads the naming convention."
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
    "One portable category a symbol falls into. Kinds are language-specific; facets are shared, so a filter written once applies to every served language."

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
    """A provider-local kind preserving the construct name used by that language implementation."""


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
                "Text the parser ignores. A doc comment carries this too; "
                "`RegionRole.documentation` is what ties one to the declaration it describes."
            ),
            "identifier": "A name as written, whether it declares or refers.",
            "literal": "A value written straight into the source — `42`, `true`.",
            "pattern": "A destructuring or matching form: a `match` arm, a binding that pulls fields apart.",
            "generated": (
                "Produced by a tool from something else in the workspace. A change that "
                "touches it carries a `generated_code` confirmation in a projection, because "
                "the next build overwrites whatever is written here."
            ),
            "test": "Part of the test suite, so a filter can keep test code in or out of an answer.",
        }
    },
)
class NodeFacet(str, Enum):
    "Portable structural facets, so a filter can ask for bodies or imports without knowing the grammar that produced them."

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
                "What a caller should select on arriving here: the name for a declaration, the "
                "whole node otherwise."
            ),
            "name": "The identifier alone.",
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
                "included. What a removal should take."
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
                "entry or build dependency."
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


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class SymbolOrigin(ClosedModel):
    """Where a symbol belongs and how its declaration came to exist. Source location and
    generation are separate: generated code can belong to the project or to a dependency."""

    location: Field[SourceLocation | None] = proto_field(
        default=None,
        description="Source ownership. Null exactly when `source_kind` is `synthetic`.",
        number=1,
    )
    source_kind: Field[SourceKind] = proto_field(
        description="Whether the declaration is authored, generated, or synthetic.",
        number=2,
    )
    unit: Field[SourceUnitId | None] = proto_field(
        default=None,
        description=(
            "Source-catalog unit containing the declaration. Null when source is unavailable "
            "or the declaration is synthetic."
        ),
        number=3,
    )

    @model_validator(mode="after")
    def synthetic_has_no_source(self) -> SymbolOrigin:
        if self.source_kind is SourceKind.SYNTHETIC:
            if self.location is not None or self.unit is not None:
                raise ValueError("synthetic symbol cannot carry source location or unit")
            return self
        if self.location is None:
            raise ValueError("authored or generated symbol requires source location")
        return self


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Symbol(ClosedModel):
    """Provider-resolved semantic identity. Source structure lives in Node and is connected through Relationship."""

    id: Field[SymbolId] = proto_field(
        description=(
            "Unique identifier of this symbol across the whole workspace, and the URI that "
            "change requests, filters, and relationship records accept unchanged."
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
        description="What this symbol is in the provider's vocabulary, such as `trait`, `function`, or `table`.",
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
        description="Where the declaration belongs, how it was produced, and its source unit when readable.",
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
        description="Language-specific facts with no portable equivalent, namespaced by the provider that emitted them.",
        number=13,
    )
    document_local: Field[bool] = proto_field(
        description=(
            "Whether language semantics confine this symbol to the document that declares it. "
            "The provider classifies locality from its language model."
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
    "One node of a file's concrete syntax tree. It identifies a source range and provider-local syntax kind. `symbol` connects the node to semantic identity when the language supplies one."

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
            "providers can produce different trees over the same file bytes."
        ),
        number=4,
    )
    kind: Field[ExactKind] = proto_field(
        description=(
            "What the node is in the provider's vocabulary, such as `fn_item`, `mapping.key`, "
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
        description="Syntax facts the model has no field for, namespaced by the provider that emitted them.",
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
                "The provider's own name resolution or type checker produced the edge. It is a "
                "fact about the program."
            ),
            "syntax": (
                "Read off the syntax tree because nothing resolved it — a call on an untyped "
                "receiver matched by name. Repeatable, and still capable of being wrong."
            ),
            "heuristic": "A guess, with `confidence` saying how good a one. Required there and meaningless elsewhere.",
        }
    },
)
class RelationshipDerivation(str, Enum):
    "How this edge was established. Every edge reaches Rift from a provider; this field records how much the provider knew. A consumer may act on `resolution` directly. Lower levels require another check before rewriting."

    RESOLUTION = "resolution"
    SYNTAX = "syntax"
    HEURISTIC = "heuristic"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Relationship(ClosedModel):
    "One directed edge between two symbols. Its evidence is the nodes it was read from, and its derivation is how much the provider knew when it was read."

    from_: Field[SymbolId] = proto_field(
        alias="from", description="The symbol the edge starts at.", number=1
    )
    kind: Field[ExactKind] = proto_field(
        description="What the edge is in this provider's vocabulary, such as `import`, `use`, or `implements`.",
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
            "How this edge was established. Every edge reaches Rift from a provider; this "
            "field records how much the provider knew. A consumer may act on `resolution` "
            "directly. Lower levels require another check before rewriting."
        ),
        number=6,
    )
    confidence: Field[float | None] = proto_field(
        default=None,
        description="How likely a `heuristic` edge is to hold, from 0 to 1. Absent for any other derivation.",
        ge=0,
        le=1,
        number=7,
    )
    extensions: Field[Extensions] = proto_field(
        description="Edge facts the model has no field for, namespaced by the provider that emitted them.",
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
            "in": (
                "The field equals one of `values`. Against an array field it holds at least one "
                "of them, which is how `facets` is asked for any of several at once."
            ),
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
    "A predicate over a standard, namespaced substrate, or diagnostic field. Rift evaluates the regex operation under `rift-regex`. Path selectors carry their own glob grammar."

    field: Field[str] = proto_field(
        description=(
            "Which field to test, by its name in this model: `facets`, "
            "`origin.location.kind`, `origin.source_kind`, `severity`. Extension keys and "
            "diagnostic fields are addressed the same way."
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
                        "Matching by portable facet, which reaches every served language."
                    ),
                    "required": ["facet"],
                },
            ]
        }
    )
    kind: Field[list[str] | None] = proto_field(
        default=None,
        description="Exact relationship kinds a provider emits. Any listed kind matches.",
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
            "How many edges a traversal may cross. Only edges that compose carry "
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
    "A symbol, wherever it happens to be written. Addressed this way, an operation reaches the declaration without naming a file or an offset."

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
    "A byte range, whether or not anything was parsed there. This is what addresses a `LICENSE` file, or a region of a file no provider claims."

    kind: Field[Literal["source"]] = proto_field()
    span: Field[SourceSpan] = proto_field(
        description="The file, and the bytes in it.", number=1
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class AddressPath(ClosedModel):
    """A whole file or directory, by project path. A directory covers everything beneath
    it, and a directory holding no files resolves to zero edits, which refuses."""

    kind: Field[Literal["path"]] = proto_field()
    path: Field[ProjectPath] = proto_field(
        description="The file or directory the operation applies to.", number=1
    )


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("symbol", "symbol", 1, AddressSymbol),
        Variant("node", "node", 2, AddressNode),
        Variant("source", "source", 3, AddressSource),
        Variant("path", "path", 5, AddressPath),
    ),
)
class Address(ProtocolRoot):
    "Where an operation applies. Separate union branches distinguish a semantic symbol, a syntax node, a source range, and a whole file or directory."


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class DiagnosticRelated(ClosedModel):
    "A second place the provider points at — the earlier declaration a redefinition conflicts with, the bound that failed. It carries a message and a location, and never a severity of its own, because it is part of one finding."

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
        "Tag",
        (
            EnumValue("deprecated", "TAG_DEPRECATED", 1),
            EnumValue("unnecessary", "TAG_UNNECESSARY", 2),
        ),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "deprecated": "The code still works and is marked for removal.",
            "unnecessary": "The code has no effect — an unused import, an unreachable branch.",
        }
    },
)
class DiagnosticTag(str, Enum):
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
            "reliable": "The provider parsed the file. Facts around this finding stand.",
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
            "unknown": "The provider does not say.",
        }
    },
)
class DiagnosticContinuation(str, Enum):
    "Whether the finding is an artefact of source that stops mid-way, which is the normal state of a file the caller is halfway through writing."

    REPAIRABLE = "repairable"
    UNREPAIRABLE = "unrepairable"
    UNKNOWN = "unknown"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class Diagnostic(ClosedModel):
    """One finding a provider produced from source. Its code and message retain the provider's vocabulary."""

    severity: Field[Severity] = proto_field(
        description="How much it matters.", number=1
    )
    code: Field[str | None] = proto_field(
        description=(
            "The provider's own identifier for this finding — `TS2345`, `E0308`. Null where "
            "the provider issues none."
        ),
        number=2,
    )
    message: Field[str] = proto_field(
        description="What the provider said, in its own words.",
        max_length=4096,
        number=3,
    )
    span: Field[SourceSpan | None] = proto_field(
        description="Where it applies. Null for a finding about the file as a whole, or about the build.",
        number=4,
    )
    related: Field[list[DiagnosticRelated]] = proto_field(
        description="Other places the provider pointed at while explaining this one.",
        number=5,
    )
    tags: Field[list[DiagnosticTag]] = proto_field(
        description="Presentation tags for the finding. A consumer can render them as strikethrough or grey text.",
        number=6,
        json_schema_extra={"uniqueItems": True},
    )
    reliability: Field[DiagnosticReliability] = proto_field(
        description="Whether the facts around this finding came off a clean parse.",
        number=7,
    )
    continuation: Field[DiagnosticContinuation] = proto_field(
        description=(
            "Whether the finding is an artefact of source that stops mid-way, which is the "
            "normal state of a file the caller is halfway through writing."
        ),
        number=8,
    )
    extensions: Field[Extensions] = proto_field(
        description="Diagnostic fields the model has no place for, namespaced by the provider that emitted them.",
        number=9,
    )
    language: Field[Language | None] = proto_field(
        description="The language whose provider produced this. Null for a finding Rift itself raised.",
        number=10,
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class OperationVerifierProvider(ClosedModel):
    """A provider ran the check against its own analysis of the language."""

    kind: Field[Literal["provider"]] = proto_field()
    language: Field[Language] = proto_field(
        description="The language whose provider performed the check.", number=1
    )


@definition(owner=CORE, public=False, proto=Proto.message(), schema_extra={})
class OperationVerifierHook(ClosedModel):
    """One workspace hook checked a changed tree."""

    kind: Field[Literal["hook"]] = proto_field()
    hook: Field[str] = proto_field(
        description="The `id` of the workspace hook that ran the check.",
        pattern="^[A-Za-z][A-Za-z0-9_.-]{0,127}$",
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
        Variant("provider", "provider", 2, OperationVerifierProvider),
        Variant("hook", "hook", 3, OperationVerifierHook),
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
    """Coverage required or observed for a fact family."""

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
            EnumValue("source_unchanged", "KIND_SOURCE_UNCHANGED", 2),
            EnumValue("writable", "KIND_WRITABLE", 3),
            EnumValue("destination_legal", "KIND_DESTINATION_LEGAL", 6),
        ),
        placement=Placement("kind", 1),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "target_exists": "Every addressed symbol, node, or source range resolves in the targeted tree as it stands. A node address resolves only when its range is exactly a current node of the addressed kind, so offsets that shifted since the id was read fail here.",
            "source_unchanged": "The bytes an address pins have not been rewritten since the address was read. This is the check a witness fails.",
            "writable": "Every affected path is inside the project and writable through Rift's workspace handle.",
            "destination_legal": "The requested path can receive the moved or created entry. On a case-insensitive filesystem a destination that case-folds onto a different existing path fails here.",
        }
    },
)
class OperationPreconditionKind(str, Enum):
    """Condition being checked."""

    TARGET_EXISTS = "target_exists"
    SOURCE_UNCHANGED = "source_unchanged"
    WRITABLE = "writable"
    DESTINATION_LEGAL = "destination_legal"


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


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "RefusalReason",
        (
            EnumValue("unsupported", "REFUSAL_REASON_UNSUPPORTED", 1),
            EnumValue("unmet_precondition", "REFUSAL_REASON_UNMET_PRECONDITION", 2),
            EnumValue("ambiguous_target", "REFUSAL_REASON_AMBIGUOUS_TARGET", 3),
            EnumValue("unsafe_effect", "REFUSAL_REASON_UNSAFE_EFFECT", 7),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "unsupported": "Nothing serves this operation for the language it reaches.",
            "unmet_precondition": "A condition checked before resolution failed. The failed entry is in `preconditions`.",
            "ambiguous_target": "The address resolves to several targets. Narrow it and ask again.",
            "unsafe_effect": "The complete effect reaches outside what the caller can have meant — outside the project, or into generated source.",
        }
    },
)
class RefusalReason(str, Enum):
    """Why resolution produced no edits at all. A change Rift can express but nobody
    will vouch for still lands, carrying its confirmations — `ErrorData` carries
    transport and infrastructure failures."""

    UNSUPPORTED = "unsupported"
    UNMET_PRECONDITION = "unmet_precondition"
    AMBIGUOUS_TARGET = "ambiguous_target"
    UNSAFE_EFFECT = "unsafe_effect"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class OperationPrecondition(ClosedModel):
    "One executable condition checked while resolving an operation. Expected and observed values carry explicit value tags."

    kind: Field[OperationPreconditionKind] = proto_field(
        description="Condition being checked.",
        number=1,
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
            "Required value. Examples include zero remaining usages "
            "or complete coverage."
        ),
        number=6,
    )
    observed: Field[PreconditionValue] = proto_field(
        description="Value found while checking the condition.", number=7
    )
    diagnostics: Field[list[Diagnostic]] = proto_field(
        description="Provider or Rift findings that explain the observed value.",
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


@union(
    owner=CORE,
    oneof="variant",
    discriminator="kind",
    variants=(
        Variant("address", "address", 1, OperationBlockerAddress),
        Variant("path", "path", 2, OperationBlockerPath),
    ),
)
class OperationBlocker(ProtocolRoot):
    "A concrete subject preventing resolution. The union admits existing code and a path that may not exist."


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
            EnumValue("source_rewritten", "SOURCE_REWRITTEN", 7),
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
    SOURCE_REWRITTEN = "source_rewritten"


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
        description=("Subjects after Rift applies the change. Empty for deletion."),
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
            EnumValue("syntax_checked", "GUARANTEE_KIND_SYNTAX_CHECKED", 1),
            EnumValue("behavior_checked", "GUARANTEE_KIND_BEHAVIOR_CHECKED", 4),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "syntax_checked": "The language parser accepts the affected source under the stated scope.",
            "behavior_checked": (
                "A named static analysis or workspace hook checked a stated behavioral "
                "property. The evidence is limited to that named property and scope."
            ),
        }
    },
)
class GuaranteeKind(str, Enum):
    """A property a change claims and must establish with scoped evidence when resolved."""

    SYNTAX_CHECKED = "syntax_checked"
    BEHAVIOR_CHECKED = "behavior_checked"


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Method",
        (
            EnumValue("construction", "CONSTRUCTION", 1),
            EnumValue("provider", "PROVIDER", 2),
            EnumValue("hook", "HOOK", 4),
        ),
        placement=Placement("method", 4),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "construction": "Rift established the property directly from its closed edit and transaction rules.",
            "provider": "The language's provider checked the changed source.",
            "hook": "A workspace hook checked the changed tree.",
        }
    },
)
class GuaranteeEvidenceMethod(str, Enum):
    """How the property was established."""

    CONSTRUCTION = "construction"
    PROVIDER = "provider"
    HOOK = "hook"


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
            EnumValue("hook", "HOOK", 7),
            EnumValue("configuration", "CONFIGURATION", 10),
            EnumValue("advisory", "ADVISORY", 11),
            EnumValue("external", "EXTERNAL", 12),
        ),
        placement=Placement("kind", 2),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "destructive": "The change deletes source or replaces an existing file.",
            "large_scope": "The change reaches enough files or symbols to require explicit acceptance.",
            "generated_code": "The change touches source marked as generated.",
            "hook": "A workspace hook failed over the changed tree, or did not finish inside its timeout.",
            "configuration": "The change touches `rift.toml`. Publishing it changes which hooks and limits the server runs with.",
            "advisory": "The change carries a warning-severity `Advisory` nothing has discharged. Its `instruction` is the check that settles it.",
            "external": "A process wrote directly into a projection, outside a Rift change request.",
        }
    },
)
class ConfirmationRequirementKind(str, Enum):
    """The condition that makes acceptance necessary."""

    DESTRUCTIVE = "destructive"
    LARGE_SCOPE = "large_scope"
    GENERATED_CODE = "generated_code"
    HOOK = "hook"
    CONFIGURATION = "configuration"
    ADVISORY = "advisory"
    EXTERNAL = "external"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class ConfirmationRequirement(ClosedModel):
    "One effect the caller has to accept before publication. The change that raised it lands in the projection either way, carrying this record, so the caller can read what happened before deciding. Requirements are sorted by kind, source location, title, and detail within their change."

    kind: Field[ConfirmationRequirementKind] = proto_field(
        description="The condition that makes acceptance necessary.",
        number=2,
    )
    title: Field[str] = proto_field(
        description="A short account of the effect being accepted.",
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
class Advisory(ClosedModel):
    """One concern a provider or hook attached to an applied change, injected on the change
    result rather than offered behind a separate read. A `checked` advisory is a verdict the
    emitter verified itself and needs no action; a warning is open, and its `instruction` is
    the concrete check or edit that settles it. In a projection, an open warning also mints an
    `advisory` confirmation, so publication cannot take the change until the caller accepts
    it."""

    code: Field[str] = proto_field(
        description=(
            "Stable dotted identifier of the concern, such as `syntax.parse` or "
            "`hooks.tests`."
        ),
        pattern="^[a-z][a-z0-9_.-]*$",
        max_length=128,
        examples=["syntax.parse", "hooks.tests"],
        number=1,
    )
    severity: Field[Severity] = proto_field(
        description=(
            "How much it matters. `warning` is open and blocks publication until accepted; "
            "`info` reports a checked verdict or a fact; `hint` suggests."
        ),
        number=2,
    )
    message: Field[str] = proto_field(
        description="The concern, in the emitter's words, naming the concrete places involved.",
        max_length=4096,
        number=3,
    )
    checked: Field[bool] = proto_field(
        description=(
            "Whether the emitter verified the concern itself and found it settled. A checked "
            "advisory never carries `warning` severity — a verdict must not absolve what it "
            "did not check."
        ),
        number=4,
    )
    instruction: Field[str | None] = proto_field(
        default=None,
        description=(
            "The concrete step that settles an open advisory: the check to run, or the edit "
            "to make. Null on a checked advisory."
        ),
        max_length=4096,
        number=5,
    )
    addresses: Field[list[Address]] = proto_field(
        description="Symbols or nodes the concern involves.", number=6
    )
    paths: Field[list[ProjectPath]] = proto_field(
        description="Files the concern involves.",
        number=7,
        json_schema_extra={"uniqueItems": True},
    )

    @model_validator(mode="after")
    def checked_verdict_is_not_a_warning(self) -> Advisory:
        if self.checked and self.severity is Severity.WARNING:
            raise ValueError("a checked advisory cannot carry warning severity")
        return self


@definition(
    owner=CORE,
    public=True,
    proto=Proto.enum(
        "FactFamily",
        (
            EnumValue("symbols", "FACT_FAMILY_SYMBOLS", 2),
            EnumValue("nodes", "FACT_FAMILY_NODES", 3),
            EnumValue("relationships", "FACT_FAMILY_RELATIONSHIPS", 4),
            EnumValue("types", "FACT_FAMILY_TYPES", 5),
            EnumValue("diagnostics", "FACT_FAMILY_DIAGNOSTICS", 6),
            EnumValue("history", "FACT_FAMILY_HISTORY", 7),
        ),
        named=True,
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "symbols": "The declarations the provider resolved in the file.",
            "nodes": "The nodes of the file's syntax tree, and which symbol each writes.",
            "relationships": "The edges between symbols the provider read out of this file.",
            "types": (
                "The types carried by those symbols. Type facts live in `Symbol.types` and "
                "`Signature`; this family records how completely the provider resolved them."
            ),
            "diagnostics": "What the provider complained about while reading the file.",
            "history": (
                "How symbols changed across the workspace's version-control revisions: "
                "timelines and co-change coupling. `not_applicable` for a workspace with no "
                "version control."
            ),
        }
    },
)
class FactFamily(str, Enum):
    "One kind of fact a provider can emit. Coverage, streaming, and invalidation use the family as their unit."

    SYMBOLS = "symbols"
    NODES = "nodes"
    RELATIONSHIPS = "relationships"
    TYPES = "types"
    DIAGNOSTICS = "diagnostics"
    HISTORY = "history"


@mapping(
    owner=CORE,
    root=dict[FactFamily, Coverage],
    placement=Placement("entries", 1),
    json_schema_extra={"minProperties": 6},
)
class SemanticCoverage(ProtocolRoot):
    """Coverage for every fact family. Absence is authoritative only where state is complete."""


@scalar(
    owner=CORE,
    proto=ProtoFieldDescriptor.TYPE_STRING,
    root=str,
    pattern=r"^[A-Za-z0-9._/-]{1,128}$",
    examples=["9fceb02d0ae598e95dc970b74767f19372d61af8"],
)
class RevisionId(ProtocolRoot):
    """Identity of one revision in the workspace's version-control history, spelled the way
    the version-control system spells it. Rift carries it opaquely and never orders two
    revisions by comparing their identifiers."""


@definition(
    owner=CORE,
    public=False,
    proto=Proto.enum(
        "Kind",
        (
            EnumValue("introduced", "KIND_INTRODUCED", 1),
            EnumValue("body_changed", "KIND_BODY_CHANGED", 2),
            EnumValue("signature_changed", "KIND_SIGNATURE_CHANGED", 3),
            EnumValue("moved", "KIND_MOVED", 4),
            EnumValue("removed", "KIND_REMOVED", 5),
            EnumValue("decorators_changed", "KIND_DECORATORS_CHANGED", 6),
        ),
        placement=Placement("kind", 3),
    ),
    schema_extra={
        "rift:enumDescriptions": {
            "introduced": "The revision that first declares the symbol.",
            "body_changed": "The implementation changed while the header stayed.",
            "signature_changed": "The header changed, so every caller written before this revision predates the current shape.",
            "moved": "The history provider matched the declaration across a file move; `path` records where it lives at this revision.",
            "removed": "The declaration is gone after this revision.",
            "decorators_changed": "The declaration's attached decorators changed while the header and body stayed. Behavior may change without either.",
        }
    },
)
class SymbolVersionKind(str, Enum):
    """What the revision did to the symbol."""

    INTRODUCED = "introduced"
    BODY_CHANGED = "body_changed"
    SIGNATURE_CHANGED = "signature_changed"
    MOVED = "moved"
    REMOVED = "removed"
    DECORATORS_CHANGED = "decorators_changed"


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class SymbolVersion(ClosedModel):
    "One revision that touched a symbol. The history provider derives it by parsing the declaration at each revision that changed its file, so a version exists only where the parse found the symbol."

    revision: Field[RevisionId] = proto_field(
        description="The revision that touched the symbol.", number=1
    )
    path: Field[ProjectPath] = proto_field(
        description="Where the declaration lived at that revision.", number=2
    )
    kind: Field[SymbolVersionKind] = proto_field(
        description="What the revision did to the symbol.", number=3
    )
    timestamp: Field[str] = proto_field(
        description="When the revision was recorded, as RFC 3339 date-time.",
        examples=["2026-08-04T14:12:09Z"],
        max_length=64,
        number=4,
        json_schema_extra={"format": "date-time"},
    )
    summary: Field[str | None] = proto_field(
        default=None,
        description="The revision's own first summary line, where the version control records one.",
        max_length=4096,
        number=5,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class SymbolHistory(ClosedModel):
    "One symbol's timeline across the workspace's version-control history, newest revision first. The walk is bounded by the configured history depth, so `coverage` says whether the timeline reaches the symbol's introduction."

    symbol: Field[SymbolId] = proto_field(
        description="The symbol the timeline is for.", number=1
    )
    versions: Field[list[SymbolVersion]] = proto_field(
        description="Revisions that touched the symbol, newest first.", number=2
    )
    coverage: Field[Coverage] = proto_field(
        description=(
            "How far back the walk reached. Partial means the configured depth ended before "
            "the symbol's introduction, so an absent `introduced` version proves nothing."
        ),
        number=3,
    )


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class CoChange(ClosedModel):
    "Empirical coupling between two symbols: how often revisions that touched one touched the other. The relation is observed from history rather than resolved from source, so it reaches coupling no reference graph carries — two implementations of one concept with no edge between them."

    subject: Field[SymbolId] = proto_field(
        description="The symbol the coupling is stated for.", number=1
    )
    partner: Field[SymbolId] = proto_field(
        description="The symbol that historically changes with it.", number=2
    )
    together: Field[int] = proto_field(
        description="Revisions inside the walked depth that touched both symbols.",
        ge=1,
        le=9007199254740991,
        number=3,
    )
    touches: Field[int] = proto_field(
        description="Revisions inside the walked depth that touched `subject` at all. Never below `together`.",
        ge=1,
        le=9007199254740991,
        number=4,
    )

    @model_validator(mode="after")
    def coupling_is_bounded(self) -> CoChange:
        if self.together > self.touches:
            raise ValueError("together cannot exceed touches")
        return self


@definition(owner=CORE, public=True, proto=Proto.message(), schema_extra={})
class CodeBlock(ClosedModel):
    """Caller-provided source evaluated in the targeted tree's execution copy. It is never
    written back to that tree."""

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
            "path selects the project root. It must name a directory in the evaluated tree."
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
    """Exact server-enforced bounds for one code-block evaluation."""

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
            "Process exit status where the runtime used a process, or null for an in-process "
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


MODELS = (
    Digest,
    ProviderId,
    ProjectionId,
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
    SourceUnitId,
    SourceResolverId,
    SourcePath,
    PackageIdentity,
    SourceLocationKind,
    SourceLocation,
    SourceKind,
    SourceMappingPrecision,
    SourceUnitSpan,
    SourceMapping,
    SourceUnit,
    TextEdit,
    Edit,
    Freshness,
    ProviderProvenance,
    IndexSnapshot,
    ReadSnapshot,
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
    DiagnosticRelated,
    Diagnostic,
    OperationVerifier,
    PreconditionValue,
    OperationPrecondition,
    OperationBlocker,
    OperationEffect,
    GuaranteeKind,
    GuaranteeEvidence,
    ConfirmationRequirement,
    Advisory,
    FactFamily,
    RevisionId,
    SymbolVersion,
    SymbolHistory,
    CoChange,
    CodeBlock,
    CapturedText,
    ExecutionBudget,
    ExecutionStatus,
    ExecutionResult,
)
