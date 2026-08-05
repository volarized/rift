from __future__ import annotations

from .base import *


@definition(
    owner="core",
    public=True,
    proto={"scalar": "int64", "package": "rift.core"},
    schema_extra={},
)
class ProtocolVersion(
    ProtocolRoot[
        "Annotated[Literal[1], Field(description='Which revision of this contract a message speaks. A single integer changes when a reader would break. The handshake compares it before either side reads request fields.', json_schema_extra={'rift:proto': {'scalar': 'int64', 'package': 'rift.core'}})]"
    ]
):
    """Which revision of this contract a message speaks. A single integer changes when a reader would break. The handshake compares it before either side reads request fields."""


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class Digest(
    ProtocolRoot[
        "Annotated[str, Field(description='SHA-256 of the value being identified, lowercase hex, 64 characters. The contract fixes the algorithm so Rift and its adapters produce the same identity for the same bytes.', pattern='^[0-9a-f]{64}$', json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """SHA-256 of the value being identified, lowercase hex, 64 characters. The contract fixes the algorithm so Rift and its adapters produce the same identity for the same bytes."""


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class Revision(
    ProtocolRoot[
        "Annotated[str, Field(default='HEAD', description='Which state to answer against. Absent, the default branch at its latest commit.\\n\\nA git revision parameter, as gitrevisions(7) defines one: `main`, `v1.2.0`, `dae86e1`, `HEAD~3`. Dropping the `/HEAD` from a worktree ref addresses what is on disk there, uncommitted edits included, which gitrevisions cannot spell and is what an agent editing code usually means.', pattern='^[^\\\\u0000-\\\\u001F\\\\u007F]+$', min_length=1, max_length=256, examples=['main', 'v1.2.0', 'HEAD~3', 'worktrees/feature-x'], json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """Which state to answer against. Absent, the default branch at its latest commit.

    A git revision parameter, as gitrevisions(7) defines one: `main`, `v1.2.0`, `dae86e1`, `HEAD~3`. Dropping the `/HEAD` from a worktree ref addresses what is on disk there, uncommitted edits included, which gitrevisions cannot spell and is what an agent editing code usually means."""


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class Commit(
    ProtocolRoot[
        "Annotated[str, Field(description='One committed state, as its full object ID. SHA-1 repositories write 40 hex characters and SHA-256 repositories 64.', pattern='^[0-9a-f]{40}([0-9a-f]{24})?$', examples=['dae86e1950b1277e545cee180551750029cfe735'], json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """One committed state, as its full object ID. SHA-1 repositories write 40 hex characters and SHA-256 repositories 64."""


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class Worktree(
    ProtocolRoot[
        "Annotated[str, Field(description=\"A working tree, spelled the way git reaches one from any other: `main-worktree` for the repository's own, and `worktrees/<name>` for a linked one.\", pattern='^(main-worktree|worktrees/[A-Za-z0-9._-]{1,100})$', examples=['main-worktree', 'worktrees/feature-x'], json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """A working tree, spelled the way git reaches one from any other: `main-worktree` for the repository's own, and `worktrees/<name>` for a linked one."""


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.SnapshotCommit"},
    schema_extra={},
)
class SnapshotCommit(ClosedModel):
    """A commit. Nothing about it can change, so the object ID is the whole of the state."""

    kind: Literal["commit"] = Field()
    commit: Commit = Field(
        description="The object ID the revision resolved to.",
        json_schema_extra={"rift:proto": {"field": "commit", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.SnapshotWorktree"},
    schema_extra={},
)
class SnapshotWorktree(ClosedModel):
    """A working tree, which changes under you while you read it. It is reported as the commit it sits on plus a digest of what differs, so a second call can tell whether the disk moved."""

    kind: Literal["worktree"] = Field()
    worktree: Worktree = Field(
        description="Which working tree on disk, in the spelling git uses to reach one from another.",
        json_schema_extra={"rift:proto": {"field": "worktree", "number": 1}},
    )
    base: Commit = Field(
        description="The commit this working tree sits on.",
        json_schema_extra={"rift:proto": {"field": "base", "number": 2}},
    )
    changes: Digest = Field(
        description="What makes this snapshot an identity: two reads of one dirty tree on the same `base` are otherwise indistinguishable. The changed set has no bound. This field is SHA-256 over each differing path with the digest of its content, in RFC 8785 canonical JSON. A clean tree digests the empty set.",
        json_schema_extra={"rift:proto": {"field": "changes", "number": 3}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.Snapshot",
        "oneof": "variant",
        "variants": [
            {"tag": "commit", "field": "commit", "number": 1, "type": "SnapshotCommit"},
            {
                "tag": "worktree",
                "field": "worktree",
                "number": 2,
                "type": "SnapshotWorktree",
            },
        ],
    },
    schema_extra={},
)
class Snapshot(
    ProtocolRoot[
        "Annotated[SnapshotCommit | SnapshotWorktree, Field(discriminator='kind')]"
    ]
):
    """What a `Revision` resolved to, reported beside every answer so a second call can ask the same question again and get the same state.

    A commit names its immutable tree. A working-tree snapshot names its base commit and digests the changed set, leaving every unedited file's identity in the base."""


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.LanguageRegion"},
    schema_extra={},
)
class LanguageRegion(ClosedModel):
    """A byte range of one file and the language used to parse it. The owner of `App.svelte` can mark its script block as TypeScript. A produced virtual file records ranges in its own byte coordinates."""

    language: LanguageId = Field(
        description="The language grammar used for these bytes.",
        json_schema_extra={"rift:proto": {"field": "language", "number": 1}},
    )
    range: TextRange = Field(
        description="Offsets of the language region inside the file.",
        json_schema_extra={"rift:proto": {"field": "range", "number": 2}},
    )


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class FileId(
    ProtocolRoot[
        "Annotated[str, Field(description='Identity of one file, and the URI that resolves it. The path after `rift://file/` is a `ProjectPath`: unreserved URI characters remain literal, `/` separates segments, and every other UTF-8 byte uses uppercase percent-encoding. Decoding to an absolute path or a `.` or `..` segment is refused. A revision can be attached as `?rev=`.', pattern=\"^rift://file/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-F]{2}){1,1000}(\\\\?rev=[A-Za-z0-9._~%!$&'()*+,;:@/-]{1,256})?$\", min_length=13, max_length=1024, examples=['rift://file/pkg/util.py', 'rift://file/src/%E2%98%83.ts', 'rift://file/src/config.ts?rev=main'], json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """Identity of one file, and the URI that resolves it. The path after `rift://file/` is a `ProjectPath`: unreserved URI characters remain literal, `/` separates segments, and every other UTF-8 byte uses uppercase percent-encoding. Decoding to an absolute path or a `.` or `..` segment is refused. A revision can be attached as `?rev=`."""


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.FileContentRegular"},
    schema_extra={},
)
class FileContentRegular(ClosedModel):
    """A physical or virtual file with bytes in it. Every leaf and symbol with readable source comes from this kind."""

    kind: Literal["regular"] = Field()
    digest: Digest = Field(
        description="SHA-256 of the bytes. The same value `Snapshot.changes` digests for a file that differs from its base.",
        json_schema_extra={"rift:proto": {"field": "digest", "number": 1}},
    )
    size: int = Field(
        description="Size in bytes.",
        ge=0,
        le=9007199254740991,
        json_schema_extra={"rift:proto": {"field": "size", "number": 2}},
    )
    executable: bool = Field(
        description="Whether Git records the executable bit.",
        json_schema_extra={"rift:proto": {"field": "executable", "number": 3}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.FileContentLfsPointer"},
    schema_extra={},
)
class FileContentLfsPointer(ClosedModel):
    """A Git LFS pointer: a stub committed in place of a large file. Rift reads the stub and never fetches what it names, so the size and hash below are the only facts available."""

    kind: Literal["lfs_pointer"] = Field()
    oid: str = Field(
        description="The content hash the pointer names. Rift checks the pointer's syntax and never fetches what it points at.",
        pattern="^sha256:[0-9a-f]{64}$",
        json_schema_extra={"rift:proto": {"field": "oid", "number": 1}},
    )
    size: int = Field(
        description="Size in bytes of the content the pointer names.",
        ge=0,
        le=9007199254740991,
        json_schema_extra={"rift:proto": {"field": "size", "number": 2}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.FileContentSymlink"},
    schema_extra={},
)
class FileContentSymlink(ClosedModel):
    """A symbolic link. The entry is the path it points at, reported as git recorded it, because the target may not exist in this checkout at all."""

    kind: Literal["symlink"] = Field()
    target: str = Field(
        description="The path the link points at, as recorded. Rift does not follow it.",
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "target", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.FileContentGitlink"},
    schema_extra={},
)
class FileContentGitlink(ClosedModel):
    """A submodule boundary: the parent repository records one child commit at this path. Tree, file, search, and adapter analysis treat the gitlink as one opaque entry and do not descend into the child checkout."""

    kind: Literal["gitlink"] = Field()
    commit: Commit = Field(
        description="The child repository commit stored in the parent tree entry. Its hexadecimal width matches the parent repository's object format. A separate Rift workspace can open the child repository at this commit.",
        json_schema_extra={"rift:proto": {"field": "commit", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "content",
        "number": 2,
        "type": "rift.core.FileContent",
        "oneof": "variant",
        "variants": [
            {
                "tag": "regular",
                "field": "regular",
                "number": 1,
                "type": "FileContentRegular",
            },
            {
                "tag": "lfs_pointer",
                "field": "lfs_pointer",
                "number": 2,
                "type": "FileContentLfsPointer",
            },
            {
                "tag": "symlink",
                "field": "symlink",
                "number": 3,
                "type": "FileContentSymlink",
            },
            {
                "tag": "gitlink",
                "field": "gitlink",
                "number": 4,
                "type": "FileContentGitlink",
            },
        ],
    },
    schema_extra={},
)
class FileContent(
    ProtocolRoot[
        "Annotated[FileContentRegular | FileContentLfsPointer | FileContentSymlink | FileContentGitlink, Field(discriminator='kind')]"
    ]
):
    """What the entry holds. Only a regular entry has source in it; the other three are paths that point elsewhere and carry no leaves."""


@definition(
    owner="core", public=True, proto={"type": "rift.core.File"}, schema_extra={}
)
class File(ClosedModel):
    """One source entry at a revision: what it is called, what it holds, and which languages read it. Physical entries come from the Git tree. A generated regular entry comes from a retained adapter virtual publication. Bytes are read from the same URI that identifies the entry."""

    model_config = closed_config(
        {
            "allOf": [
                {
                    "if": {
                        "properties": {
                            "content": {
                                "properties": {
                                    "kind": {
                                        "enum": ["lfs_pointer", "symlink", "gitlink"]
                                    }
                                }
                            }
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
    id: FileId = Field(
        description="Project-relative source identity and the URI from which this record and its bytes are read.",
        json_schema_extra={"rift:proto": {"field": "id", "number": 1}},
    )
    content: FileContent = Field(
        description="What the entry holds. Only a regular entry has source in it; the other three are paths that point elsewhere and carry no leaves.",
        json_schema_extra={
            "rift:proto": {
                "field": "content",
                "number": 2,
                "type": "rift.core.FileContent",
                "oneof": "variant",
                "variants": [
                    {
                        "tag": "regular",
                        "field": "regular",
                        "number": 1,
                        "type": "FileContentRegular",
                    },
                    {
                        "tag": "lfs_pointer",
                        "field": "lfs_pointer",
                        "number": 2,
                        "type": "FileContentLfsPointer",
                    },
                    {
                        "tag": "symlink",
                        "field": "symlink",
                        "number": 3,
                        "type": "FileContentSymlink",
                    },
                    {
                        "tag": "gitlink",
                        "field": "gitlink",
                        "number": 4,
                        "type": "FileContentGitlink",
                    },
                ],
            }
        },
    )
    languages: list[LanguageId] = Field(
        description="Distinct languages in `regions`, sorted by language ID. This summary avoids scanning every region. The physical owner reports embedded languages even when another adapter consumes virtual source.",
        examples=[["svelte", "typescript", "css"]],
        json_schema_extra={
            "rift:proto": {"field": "languages", "number": 3},
            "uniqueItems": True,
        },
    )
    regions: list[LanguageRegion] = Field(
        description="Byte ranges parsed with each language grammar. Entries sort by start, end, and language. Regions may overlap when two grammars parse the same bytes.",
        json_schema_extra={"rift:proto": {"field": "regions", "number": 4}},
    )
    semantic: bool = Field(
        description="Whether Rift produced facts from this file. False where there is nothing to read, and where no adapter claims the path.",
        json_schema_extra={"rift:proto": {"field": "semantic", "number": 5}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.ProjectEntryDirectory"},
    schema_extra={},
)
class ProjectEntryDirectory(ClosedModel):
    """A derived directory containing at least one visible descendant at this snapshot."""

    kind: Literal["directory"] = Field()
    path: ProjectPath = Field(
        description="Project-relative directory path. The empty path names the project root.",
        json_schema_extra={"rift:proto": {"field": "path", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.ProjectEntryFile"},
    schema_extra={},
)
class ProjectEntryFile(ClosedModel):
    """A physical Git tree entry with content and language ownership metadata. Generated virtual files remain outside the project tree listing."""

    kind: Literal["file"] = Field()
    file: File = Field(
        description="The file represented by this tree node.",
        json_schema_extra={"rift:proto": {"field": "file", "number": 1}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.ProjectEntry",
        "oneof": "variant",
        "variants": [
            {
                "tag": "directory",
                "field": "directory",
                "number": 1,
                "type": "ProjectEntryDirectory",
            },
            {"tag": "file", "field": "file", "number": 2, "type": "ProjectEntryFile"},
        ],
    },
    schema_extra={},
)
class ProjectEntry(
    ProtocolRoot[
        "Annotated[ProjectEntryDirectory | ProjectEntryFile, Field(discriminator='kind')]"
    ]
):
    """One visible node in a project tree. Rift walks Git tree objects at the requested snapshot, returning a directory path or the complete `File` for a content-bearing entry."""


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class LeafId(
    ProtocolRoot[
        "Annotated[str, Field(description=\"Identity of one leaf of a file's syntax tree, and the URI that resolves it. `rift://leaf/<language>/<path>@<start>-<end>` carries the canonically percent-encoded `ProjectPath` and the half-open UTF-8 byte range. A revision can be attached as `?rev=`.\", pattern=\"^rift://leaf/[A-Za-z][A-Za-z0-9._-]*/(?:[A-Za-z0-9._~!$&'()*+,;=:/-]|%[0-9A-F]{2}){1,1000}@\\\\d+-\\\\d+(\\\\?rev=[A-Za-z0-9._~%!$&'()*+,;:@/-]{1,256})?$\", min_length=18, max_length=1024, examples=['rift://leaf/python/pkg/util.py@1204-1266'], json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """Identity of one leaf of a file's syntax tree, and the URI that resolves it. `rift://leaf/<language>/<path>@<start>-<end>` carries the canonically percent-encoded `ProjectPath` and the half-open UTF-8 byte range. A revision can be attached as `?rev=`."""


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class SymbolId(
    ProtocolRoot[
        "Annotated[str, Field(description=\"Identity of one symbol, and the URI that resolves it: `rift://symbol/<language>/<qualified-name>`. The adapter uses the language's qualified name, percent-encoding UTF-8 bytes outside URI path characters with uppercase hex. Where a namespace declares two identical names, the adapter appends `~` and an index. A revision can be attached as `?rev=`.\", pattern=\"^rift://symbol/[A-Za-z][A-Za-z0-9._-]*/(?:[A-Za-z0-9._~!$&'()*+,;=:/@-]|%[0-9A-F]{2}){1,1000}(\\\\?(rev=[A-Za-z0-9._~%!$&'()*+,;:@/-]{1,256}(&cursor=[^&#]+)?|cursor=[^&#]+))?$\", min_length=17, max_length=1024, examples=['rift://symbol/python/pkg.util.load_config~1?rev=HEAD~3', 'rift://symbol/rust/serde::de::Deserialize', 'rift://symbol/typescript/src/config.ts:ConfigLoader.parse'], json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """Identity of one symbol, and the URI that resolves it: `rift://symbol/<language>/<qualified-name>`. The adapter uses the language's qualified name, percent-encoding UTF-8 bytes outside URI path characters with uppercase hex. Where a namespace declares two identical names, the adapter appends `~` and an index. A revision can be attached as `?rev=`."""


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class LanguageId(
    ProtocolRoot[
        "Annotated[str, Field(description='A language, as its adapter advertises it. One workspace runs one adapter per language, so this also identifies the adapter that owns a file or minted a fact. Which build of that adapter is answering is on the repository resource, beside the compiler it drives.', pattern='^[A-Za-z][A-Za-z0-9._-]*$', max_length=64, examples=['typescript', 'rust', 'python'], json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """A language, as its adapter advertises it. One workspace runs one adapter per language, so this also identifies the adapter that owns a file or minted a fact. Which build of that adapter is answering is on the repository resource, beside the compiler it drives."""


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.ExtensionValue"},
    schema_extra={},
)
class ExtensionValue(ClosedModel):
    """Versioned extension value. data is validated against the schema advertised for its key and version."""

    version: int = Field(
        description="Which version of the key's advertised schema shaped `data`. A consumer skips a value whose version it does not implement.",
        ge=1,
        json_schema_extra={"rift:proto": {"field": "version", "number": 1}},
    )
    data: Any = Field(
        description="The value itself, shaped by whatever that key and version advertise. Rift carries it and never interprets it.",
        json_schema_extra={"rift:proto": {"field": "data", "number": 2}},
    )


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class ExtensionKey(
    ProtocolRoot[
        "Annotated[str, Field(description='A reverse-domain namespaced extension or extension-operation identifier.', pattern='^[a-z0-9]+(?:[.-][a-z0-9]+)+\\\\.[A-Za-z][A-Za-z0-9_-]*$', json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """A reverse-domain namespaced extension or extension-operation identifier."""


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.Extensions", "field": "entries", "number": 1},
    schema_extra={},
)
class Extensions(
    ProtocolRoot[
        "Annotated[dict[ExtensionKey, ExtensionValue], Field(description='Facts an adapter carries that the model has no field for, under a reverse-domain key. Keys and values use RFC 8785 canonical JSON. `Capabilities.extension_values` advertises every key and version, and consumers skip entries they do not implement.', json_schema_extra={'rift:proto': {'type': 'rift.core.Extensions', 'field': 'entries', 'number': 1}})]"
    ]
):
    """Facts an adapter carries that the model has no field for, under a reverse-domain key. Keys and values use RFC 8785 canonical JSON. `Capabilities.extension_values` advertises every key and version, and consumers skip entries they do not implement."""


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class PathPattern(
    ProtocolRoot[
        "Annotated[str, Field(description=\"Project-relative Git-style glob using *, ?, **, and character classes; absolute, backslash, control, and '..' segments are forbidden.\", pattern='^(?!/)(?!\\\\.\\\\.?(/|$))(?!.*(/\\\\.\\\\.?)(/|$))[^\\\\\\\\\\\\u0000-\\\\u001F\\\\u007F]+$', json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """Project-relative Git-style glob using *, ?, **, and character classes; absolute, backslash, control, and '..' segments are forbidden."""


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class ProjectPath(
    ProtocolRoot[
        "Annotated[str, Field(description='One path below the project root, using forward slashes and UTF-8. The empty path names the root itself. Absolute paths, backslashes, control characters, empty segments, and `.` or `..` segments are refused before the filesystem is touched.', pattern='^(?:$|(?!/)(?!.*(?:^|/)\\\\.{1,2}(?:/|$))(?!.*//)[^\\\\\\\\\\\\u0000-\\\\u001F\\\\u007F/]+(?:/[^\\\\\\\\\\\\u0000-\\\\u001F\\\\u007F/]+)*)$', max_length=1000, examples=['', 'src/config.ts', 'packages/api/Cargo.toml'], json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """One path below the project root, using forward slashes and UTF-8. The empty path names the root itself. Absolute paths, backslashes, control characters, empty segments, and `.` or `..` segments are refused before the filesystem is touched."""


@definition(
    owner="core", public=True, proto={"type": "rift.core.PathSelector"}, schema_extra={}
)
class PathSelector(ClosedModel):
    """Which files a query runs over, as two lists of globs matched against the project-relative path. `include: ["src/**"]` selects the source tree; `exclude: ["src/generated/**"]` then removes generated output."""

    include: list[PathPattern] = Field(
        description="Globs a path has to match to be searched at all.",
        json_schema_extra={
            "rift:proto": {"field": "include", "number": 1},
            "uniqueItems": True,
        },
    )
    exclude: list[PathPattern] = Field(
        description="Globs that drop a path `include` already matched.",
        json_schema_extra={
            "rift:proto": {"field": "exclude", "number": 2},
            "uniqueItems": True,
        },
    )


@definition(
    owner="core", public=True, proto={"type": "rift.core.TextRange"}, schema_extra={}
)
class TextRange(ClosedModel):
    """Half-open UTF-8 byte offsets over authoritative UTF-8 source. Every adapter converts from its compiler's native offsets at the seam. No JSON Schema keyword can tie one field to another, so that `end` is never below `start` is asserted by the conformance tests instead."""

    start: int = Field(
        description="First byte of the range, counted from the start of the file.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "start", "number": 1}},
    )
    end: int = Field(
        description="One past the last byte. Equal to `start` for an empty range, which is how a position between two bytes is spelled.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "end", "number": 2}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.Severity",
        "enum": "Severity",
        "values": {
            "error": {"name": "SEVERITY_ERROR", "number": 1},
            "warning": {"name": "SEVERITY_WARNING", "number": 2},
            "info": {"name": "SEVERITY_INFO", "number": 3},
            "hint": {"name": "SEVERITY_HINT", "number": 4},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "error": "The compiler would not accept the code. Facts read from around it may be missing.",
            "warning": "The code compiles and the compiler thinks it is wrong anyway.",
            "info": "Something worth knowing that is not a defect.",
            "hint": "A suggestion, usually with a code action behind it.",
        }
    },
)
class Severity(str, Enum):
    """How much a `Diagnostic` matters, in the compiler's own judgement. Adapters map their compiler's levels onto these four, so a caller can drop everything below `warning` without knowing which language produced it."""

    ERROR = "error"
    WARNING = "warning"
    INFO = "info"
    HINT = "hint"


@definition(
    owner="core", public=True, proto={"type": "rift.core.SourceSpan"}, schema_extra={}
)
class SourceSpan(ClosedModel):
    """A byte range of one file."""

    unit: FileId = Field(
        description="Which file the offsets are into.",
        json_schema_extra={"rift:proto": {"field": "unit", "number": 1}},
    )
    range: TextRange = Field(
        description="The bytes, as offsets into that file.",
        json_schema_extra={"rift:proto": {"field": "range", "number": 2}},
    )


@definition(
    owner="core", public=True, proto={"type": "rift.core.TextEdit"}, schema_extra={}
)
class TextEdit(ClosedModel):
    """One byte range of one file, and what replaces it. Offsets are into the file as it stands at the pinned snapshot, so edits in a set never observe each other, and two of them may not overlap."""

    kind: Literal["replace"] = Field(
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}}
    )
    span: SourceSpan = Field(
        description="The file, and the byte range being replaced.",
        json_schema_extra={"rift:proto": {"field": "span", "number": 2}},
    )
    text: str = Field(
        description="What the range becomes. Empty deletes it.",
        json_schema_extra={"rift:proto": {"field": "text", "number": 3}},
    )


@definition(
    owner="core", public=False, proto={"type": "rift.core.EditCreate"}, schema_extra={}
)
class EditCreate(ClosedModel):
    """A regular UTF-8 file that does not exist yet, with its complete content and executable bit."""

    kind: Literal["create"] = Field()
    file: FileId = Field(
        description="A path that does not exist at the pinned snapshot. Extracting a module is the usual reason.",
        json_schema_extra={"rift:proto": {"field": "file", "number": 1}},
    )
    text: str = Field(
        description="All UTF-8 source in the new file.",
        json_schema_extra={"rift:proto": {"field": "text", "number": 2}},
    )
    executable: bool = Field(
        description="Whether Git records the new file as executable.",
        json_schema_extra={"rift:proto": {"field": "executable", "number": 3}},
    )


@definition(
    owner="core", public=False, proto={"type": "rift.core.EditDelete"}, schema_extra={}
)
class EditDelete(ClosedModel):
    """Removing a file by path. The pinned snapshot supplies its previous entry and content identity."""

    kind: Literal["delete"] = Field()
    file: FileId = Field(
        description="The file to remove, as it stands at the pinned snapshot.",
        json_schema_extra={"rift:proto": {"field": "file", "number": 1}},
    )


@definition(
    owner="core", public=False, proto={"type": "rift.core.EditRename"}, schema_extra={}
)
class EditRename(ClosedModel):
    """A move carries the source and destination paths. Its content stays in the existing Git object."""

    kind: Literal["rename"] = Field()
    file: FileId = Field(
        description="The path the file has now.",
        json_schema_extra={"rift:proto": {"field": "file", "number": 1}},
    )
    destination: FileId = Field(
        description="The path it moves to.",
        json_schema_extra={"rift:proto": {"field": "destination", "number": 2}},
    )


@definition(
    owner="core", public=False, proto={"type": "rift.core.EditCopy"}, schema_extra={}
)
class EditCopy(ClosedModel):
    """Copying an existing file preserves its bytes and executable bit. A caller that needs different destination content uses `create` with that content."""

    kind: Literal["copy"] = Field(description="Tags this as a file copy.")
    file: FileId = Field(
        description="The existing file whose entry is copied.",
        json_schema_extra={"rift:proto": {"field": "file", "number": 1}},
    )
    destination: FileId = Field(
        description="A path that does not exist before this edit runs.",
        json_schema_extra={"rift:proto": {"field": "destination", "number": 2}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.EditSetExecutable"},
    schema_extra={},
)
class EditSetExecutable(ClosedModel):
    """Changing the executable bit without rewriting file content."""

    kind: Literal["set_executable"] = Field(
        description="Tags this as an executable-bit change."
    )
    file: FileId = Field(
        description="The regular file whose Git mode changes.",
        json_schema_extra={"rift:proto": {"field": "file", "number": 1}},
    )
    executable: bool = Field(
        description="The executable bit the file has after the edit.",
        json_schema_extra={"rift:proto": {"field": "executable", "number": 2}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.EditCreateGitlink"},
    schema_extra={},
)
class EditCreateGitlink(ClosedModel):
    """Creating a submodule tree entry in the parent repository. A companion text edit updates `.gitmodules` when the mapping is new."""

    kind: Literal["create_gitlink"] = Field(
        description="Tags this as creation of a gitlink entry."
    )
    file: FileId = Field(
        description="A parent-repository path that does not exist at the pinned snapshot.",
        json_schema_extra={"rift:proto": {"field": "file", "number": 1}},
    )
    commit: Commit = Field(
        description="Child repository commit stored in the new gitlink. Its hexadecimal width matches the parent repository's object format.",
        json_schema_extra={"rift:proto": {"field": "commit", "number": 2}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.EditSetGitlink"},
    schema_extra={},
)
class EditSetGitlink(ClosedModel):
    """Updating an existing submodule entry to a different child commit. The operation changes the parent tree entry and does not read or mutate the child checkout."""

    kind: Literal["set_gitlink"] = Field(
        description="Tags this as a gitlink commit update."
    )
    file: FileId = Field(
        description="Existing gitlink in the parent repository at the pinned snapshot.",
        json_schema_extra={"rift:proto": {"field": "file", "number": 1}},
    )
    commit: Commit = Field(
        description="Child repository commit stored after the edit. Its hexadecimal width matches the parent repository's object format.",
        json_schema_extra={"rift:proto": {"field": "commit", "number": 2}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.Edit",
        "oneof": "variant",
        "variants": [
            {"tag": None, "field": "text_edit", "number": 1, "type": "TextEdit"},
            {"tag": "create", "field": "create", "number": 2, "type": "EditCreate"},
            {"tag": "delete", "field": "delete", "number": 3, "type": "EditDelete"},
            {"tag": "rename", "field": "rename", "number": 4, "type": "EditRename"},
            {"tag": "copy", "field": "copy", "number": 5, "type": "EditCopy"},
            {
                "tag": "set_executable",
                "field": "set_executable",
                "number": 6,
                "type": "EditSetExecutable",
            },
            {
                "tag": "create_gitlink",
                "field": "create_gitlink",
                "number": 7,
                "type": "EditCreateGitlink",
            },
            {
                "tag": "set_gitlink",
                "field": "set_gitlink",
                "number": 8,
                "type": "EditSetGitlink",
            },
        ],
    },
    schema_extra={},
)
class Edit(
    ProtocolRoot[
        "Annotated[TextEdit | EditCreate | EditDelete | EditRename | EditCopy | EditSetExecutable | EditCreateGitlink | EditSetGitlink, Field(discriminator='kind')]"
    ]
):
    """A filesystem effect described before Rift performs it. Edit sets are atomic and sorted bytewise by each edit's RFC 8785 canonical JSON. Text replacements in one set address the same input state and cannot overlap."""


@definition(
    owner="core", public=True, proto={"type": "rift.core.FileChange"}, schema_extra={}
)
class FileChange(ClosedModel):
    """One file between two states. A creation has no `before`. A deletion has no `after`. A rename has different paths on each side. The path comes from `after.id`, or `before.id` after deletion."""

    before: File | None = Field(
        description="The entry before the change. Null where the file did not exist.",
        json_schema_extra={"rift:proto": {"field": "before", "number": 1}},
    )
    after: File | None = Field(
        description="The entry after the change. Null where the file was deleted.",
        json_schema_extra={"rift:proto": {"field": "after", "number": 2}},
    )
    edits: list[TextEdit] = Field(
        description="Text replacements that transform the `before` content into `after`. The surrounding entries carry creation, deletion, and rename state.",
        json_schema_extra={"rift:proto": {"field": "edits", "number": 3}},
    )
    truncated: bool = Field(
        description="Whether edits were dropped to stay inside the size limit.",
        json_schema_extra={"rift:proto": {"field": "truncated", "number": 4}},
    )


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class DiffId(
    ProtocolRoot[
        "Annotated[str, Field(description='URI for one comparison: `rift://diff/<from>..<to>`. The two revisions use Git range spelling. Git forbids `..` in a ref name, so the separator is unambiguous.\\n\\nAttach `?cursor=` to continue a paged read.', pattern=\"^rift://diff/(?:[A-Za-z0-9_~%!$&'()*+,;:@/-]|\\\\.(?!\\\\.)){1,256}\\\\.\\\\.(?:[A-Za-z0-9_~%!$&'()*+,;:@/-]|\\\\.(?!\\\\.)){1,256}(\\\\?cursor=[^&#]+)?$\", min_length=15, max_length=1024, examples=['rift://diff/main..feature-x', 'rift://diff/HEAD~3..HEAD'], json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """URI for one comparison: `rift://diff/<from>..<to>`. The two revisions use Git range spelling. Git forbids `..` in a ref name, so the separator is unambiguous.

    Attach `?cursor=` to continue a paged read."""


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class PreviewId(
    ProtocolRoot[
        "Annotated[str, Field(description='Identity of one retained candidate. Rift mints an opaque base64url token when it retains the preview; reading the corresponding resource reveals the pinned base, ordered changes, resolved edits, validation evidence, and confirmations.', pattern='^[A-Za-z0-9_-]{16,128}$', examples=['AQIDBAUGBwgJCgsMDQ4PEA'], json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """Identity of one retained candidate. Rift mints an opaque base64url token when it retains the preview; reading the corresponding resource reveals the pinned base, ordered changes, resolved edits, validation evidence, and confirmations."""


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.OriginMappingExact"},
    schema_extra={},
)
class OriginMappingExact(ClosedModel):
    """One origin, byte for byte. An offset in the generated file maps back to an offset in the source arithmetically."""

    precision: Literal["exact"] = Field(default="exact")
    origins: list[Any] | None = Field(
        default=None,
        min_length=1,
        max_length=1,
        json_schema_extra={"rift:proto": {"field": "origins", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.OriginMappingApproximate"},
    schema_extra={},
)
class OriginMappingApproximate(ClosedModel):
    """One or more origins, without a position inside them. A macro expansion knows which source it came from and cannot say which byte of it."""

    precision: Literal["approximate"] = Field(default="approximate")
    origins: list[Any] | None = Field(
        default=None,
        min_length=1,
        json_schema_extra={"rift:proto": {"field": "origins", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.OriginMappingSynthetic"},
    schema_extra={},
)
class OriginMappingSynthetic(ClosedModel):
    """No origin. The generator invented these bytes, so there is nothing in the workspace to jump to."""

    precision: Literal["synthetic"] = Field(default="synthetic")
    origins: list[Any] | None = Field(
        default=None,
        max_length=0,
        json_schema_extra={"rift:proto": {"field": "origins", "number": 1}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.OriginMapping",
        "oneof": "variant",
        "variants": [
            {
                "tag": "exact",
                "field": "exact",
                "number": 1,
                "type": "OriginMappingExact",
            },
            {
                "tag": "approximate",
                "field": "approximate",
                "number": 2,
                "type": "OriginMappingApproximate",
            },
            {
                "tag": "synthetic",
                "field": "synthetic",
                "number": 3,
                "type": "OriginMappingSynthetic",
            },
        ],
    },
    schema_extra={},
)
class OriginMapping(
    ProtocolRoot[
        "Annotated[OriginMappingExact | OriginMappingApproximate | OriginMappingSynthetic, Field(discriminator='precision')]"
    ]
):
    """A relation from one produced source range to the source ranges that contributed its bytes. It lets Rift project a finding on compiler input back to source a person can edit."""


@definition(
    owner="core",
    public=False,
    proto={
        "field": "kind",
        "number": 1,
        "enum": "Kind",
        "values": {
            "request": {"name": "REQUEST", "number": 1},
            "project": {"name": "PROJECT", "number": 2},
            "dependencies": {"name": "DEPENDENCIES", "number": 3},
            "all": {"name": "ALL", "number": 4},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "request": "Only what this request asked for. The adapter answered the question put to it "
            "and claims nothing past it.",
            "project": "Every file in the workspace, dependencies left out.",
            "dependencies": "Installed packages outside the workspace source.",
            "all": "Everything the compiler could see: the workspace, its dependencies, and the "
            "standard library.",
        }
    },
)
class CoverageScopeKindKind(str, Enum):
    """How far the claim reaches."""

    REQUEST = "request"
    PROJECT = "project"
    DEPENDENCIES = "dependencies"
    ALL = "all"


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.CoverageScopeKind"},
    schema_extra={},
)
class CoverageScopeKind(ClosedModel):
    """A standing scope identified by its name."""

    kind: CoverageScopeKindKind = Field(
        description="How far the claim reaches.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "request": "Only what this request asked for. The adapter answered the question put to it "
                "and claims nothing past it.",
                "project": "Every file in the workspace, dependencies left out.",
                "dependencies": "Installed packages outside the workspace source.",
                "all": "Everything the compiler could see: the workspace, its dependencies, and the "
                "standard library.",
            },
            "rift:proto": {
                "field": "kind",
                "number": 1,
                "enum": "Kind",
                "values": {
                    "request": {"name": "REQUEST", "number": 1},
                    "project": {"name": "PROJECT", "number": 2},
                    "dependencies": {"name": "DEPENDENCIES", "number": 3},
                    "all": {"name": "ALL", "number": 4},
                },
            },
        },
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.CoverageScopeUnit"},
    schema_extra={},
)
class CoverageScopeUnit(ClosedModel):
    """One file. The claim holds for that path and says nothing about any other."""

    kind: Literal["unit"] = Field()
    unit: FileId = Field(
        description="The file the claim is about.",
        json_schema_extra={"rift:proto": {"field": "unit", "number": 1}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.CoverageScope",
        "oneof": "variant",
        "variants": [
            {"tag": None, "field": "kind", "number": 1, "type": "CoverageScopeKind"},
            {"tag": "unit", "field": "unit", "number": 2, "type": "CoverageScopeUnit"},
        ],
    },
    schema_extra={},
)
class CoverageScope(ProtocolRoot["CoverageScopeKind | CoverageScopeUnit"]):
    """What a completeness statement covers — everything the request asked for, one file, or a standing scope the answer holds over."""


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.CoverageComplete"},
    schema_extra={},
)
class CoverageComplete(ClosedModel):
    """Everything in scope is here, so a fact that is missing is a fact that does not exist."""

    state: Literal["complete"] = Field()
    scope: CoverageScope = Field(
        description="What the claim covers.",
        json_schema_extra={"rift:proto": {"field": "scope", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.CoveragePartial"},
    schema_extra={},
)
class CoveragePartial(ClosedModel):
    """Some of what is in scope is missing. `reason` is required here because a caller that reads absence as proof would be wrong."""

    state: Literal["partial"] = Field()
    scope: CoverageScope = Field(
        description="What the claim covers.",
        json_schema_extra={"rift:proto": {"field": "scope", "number": 1}},
    )
    reason: str = Field(
        description="Why the answer stops short — a limit hit, a file that would not parse, a page boundary. Prose for a reader; nothing keys on it.",
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "reason", "number": 2}},
    )
    continuation: str | None = Field(
        default=None,
        description="How to ask for the rest, where there is a way to.",
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "continuation", "number": 3}},
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "state",
        "number": 1,
        "enum": "State",
        "values": {
            "unsupported": {"name": "UNSUPPORTED", "number": 1},
            "not_applicable": {"name": "NOT_APPLICABLE", "number": 2},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "unsupported": "The adapter does not produce this family, though the language has the "
            "concept. A later build of it might.",
            "not_applicable": "The language has no such concept because its compiler consumes only "
            "physical source.",
        }
    },
)
class CoverageStateState(str, Enum):
    """`unsupported` — the adapter cannot produce this family at all. `not_applicable` — the family has no meaning for this language."""

    UNSUPPORTED = "unsupported"
    NOT_APPLICABLE = "not_applicable"


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.CoverageState"},
    schema_extra={},
)
class CoverageState(ClosedModel):
    """The family was never produced at all, so there is nothing here to be complete about."""

    state: CoverageStateState = Field(
        description="`unsupported` — the adapter cannot produce this family at all. `not_applicable` — the family has no meaning for this language.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "unsupported": "The adapter does not produce this family, though the language has the "
                "concept. A later build of it might.",
                "not_applicable": "The language has no such concept because its compiler consumes only "
                "physical source.",
            },
            "rift:proto": {
                "field": "state",
                "number": 1,
                "enum": "State",
                "values": {
                    "unsupported": {"name": "UNSUPPORTED", "number": 1},
                    "not_applicable": {"name": "NOT_APPLICABLE", "number": 2},
                },
            },
        },
    )
    scope: CoverageScope = Field(
        description="What the claim covers.",
        json_schema_extra={"rift:proto": {"field": "scope", "number": 2}},
    )
    reason: str = Field(
        description="Which of the two this is, in words: the feature the adapter lacks, or the concept the language lacks.",
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "reason", "number": 3}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.Coverage",
        "oneof": "variant",
        "variants": [
            {
                "tag": "complete",
                "field": "complete",
                "number": 1,
                "type": "CoverageComplete",
            },
            {
                "tag": "partial",
                "field": "partial",
                "number": 2,
                "type": "CoveragePartial",
            },
            {"tag": None, "field": "state", "number": 3, "type": "CoverageState"},
        ],
    },
    schema_extra={},
)
class Coverage(ProtocolRoot["CoverageComplete | CoveragePartial | CoverageState"]):
    """How much of one `FactFamily` an answer actually covers. Absence of a fact means the fact does not exist only where the state is `complete`; anywhere else it means Rift did not get that far."""


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.SemanticCoverage", "field": "entries", "number": 1},
    schema_extra={},
)
class SemanticCoverage(
    ProtocolRoot[
        "Annotated[dict[FactFamily, Coverage], Field(description='Coverage for every fact family. Absence is authoritative only where state is complete.', json_schema_extra={'rift:proto': {'type': 'rift.core.SemanticCoverage', 'field': 'entries', 'number': 1},\n 'minProperties': 6})]"
    ]
):
    """Coverage for every fact family. Absence is authoritative only where state is complete."""


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.TypeExpression"},
    schema_extra={},
)
class TypeExpression(ClosedModel):
    """How a type is written in the source, plus the symbol that declares it when one does. A type with a declaration resolves to that symbol; a structural type — `string | null`, `{ a: string }` — has the spelling and nothing to resolve to."""

    language: LanguageId = Field(
        description="The language the spelling is in, and so the adapter that produced it.",
        json_schema_extra={"rift:proto": {"field": "language", "number": 1}},
    )
    source: str = Field(
        description="The type as it is written: `Optional[Config]`, `&mut [u8]`, `string | null`.",
        json_schema_extra={"rift:proto": {"field": "source", "number": 2}},
    )
    resolved: SymbolId | None = Field(
        description="The symbol that declares this type, where one does. Null for a structural type, which has a spelling and nothing to open.",
        json_schema_extra={"rift:proto": {"field": "resolved", "number": 3}},
    )
    extensions: Extensions = Field(
        description="Type facts the model has no field for, namespaced by the adapter that emitted them.",
        json_schema_extra={"rift:proto": {"field": "extensions", "number": 4}},
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "role",
        "number": 1,
        "enum": "Role",
        "values": {
            "declared": {"name": "DECLARED", "number": 1},
            "inferred": {"name": "INFERRED", "number": 2},
            "expected": {"name": "EXPECTED", "number": 3},
            "receiver": {"name": "RECEIVER", "number": 4},
            "parameter": {"name": "PARAMETER", "number": 5},
            "return": {"name": "RETURN", "number": 6},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "declared": "Written in the source by the author.",
            "inferred": "The compiler worked it out; nothing in the source says it.",
            "expected": "What the surrounding context demands here. A mismatch is reported against this "
            "one.",
            "receiver": "The type of the implicit first argument — `self`, `this`.",
            "parameter": "The type of an argument the callable takes.",
            "return": "The type the call yields.",
        }
    },
)
class TypeBindingRole(str, Enum):
    """Why the symbol carries this type. A declared type and an inferred one can both be present and disagree, which is the interesting case."""

    DECLARED = "declared"
    INFERRED = "inferred"
    EXPECTED = "expected"
    RECEIVER = "receiver"
    PARAMETER = "parameter"
    RETURN_ = "return"


@definition(
    owner="core", public=True, proto={"type": "rift.core.TypeBinding"}, schema_extra={}
)
class TypeBinding(ClosedModel):
    """One type a symbol carries, together with the role it plays for that symbol."""

    role: TypeBindingRole = Field(
        description="Why the symbol carries this type. A declared type and an inferred one can both be present and disagree, which is the interesting case.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "declared": "Written in the source by the author.",
                "inferred": "The compiler worked it out; nothing in the source says it.",
                "expected": "What the surrounding context demands here. A mismatch is reported against this "
                "one.",
                "receiver": "The type of the implicit first argument — `self`, `this`.",
                "parameter": "The type of an argument the callable takes.",
                "return": "The type the call yields.",
            },
            "rift:proto": {
                "field": "role",
                "number": 1,
                "enum": "Role",
                "values": {
                    "declared": {"name": "DECLARED", "number": 1},
                    "inferred": {"name": "INFERRED", "number": 2},
                    "expected": {"name": "EXPECTED", "number": 3},
                    "receiver": {"name": "RECEIVER", "number": 4},
                    "parameter": {"name": "PARAMETER", "number": 5},
                    "return": {"name": "RETURN", "number": 6},
                },
            },
        },
    )
    type: TypeExpression = Field(
        description="The type itself.",
        json_schema_extra={"rift:proto": {"field": "type", "number": 2}},
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "format",
        "number": 1,
        "enum": "Format",
        "values": {
            "plain": {"name": "PLAIN", "number": 1},
            "markdown": {"name": "MARKDOWN", "number": 2},
        },
    },
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


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.Documentation"},
    schema_extra={},
)
class Documentation(ClosedModel):
    """One block of documentation attached to a declaration, in the markup it was written in."""

    format: DocumentationFormat = Field(
        description="Which markup the text is written in, since whoever displays a doc comment is the one that renders it.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "plain": "No markup. Show the text as it is.",
                "markdown": "Markdown, as the language's own doc tooling writes it.",
            },
            "rift:proto": {
                "field": "format",
                "number": 1,
                "enum": "Format",
                "values": {
                    "plain": {"name": "PLAIN", "number": 1},
                    "markdown": {"name": "MARKDOWN", "number": 2},
                },
            },
        },
    )
    text: str = Field(
        description="The body of the comment, with the comment syntax stripped.",
        json_schema_extra={"rift:proto": {"field": "text", "number": 2}},
    )


@definition(
    owner="core", public=True, proto={"type": "rift.core.Parameter"}, schema_extra={}
)
class Parameter(ClosedModel):
    """One parameter of a `Signature`: what it is called, the types bound to it, and how a call may pass it. A receiver is one of these too, held in its own field because it has no position in the parameter list."""

    name: str | None = Field(
        description="What the parameter is called. Null where the language allows an unnamed one, as a positional parameter in a function type.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 1}},
    )
    leaf: LeafId | None = Field(
        default=None,
        description="Where this parameter is written in the source.",
        json_schema_extra={"rift:proto": {"field": "leaf", "number": 2}},
    )
    types: list[TypeBinding] = Field(
        description="What it accepts. An array because a declared type and an inferred one are separate bindings.",
        json_schema_extra={"rift:proto": {"field": "types", "number": 3}},
    )
    optional: bool = Field(
        description="Whether a call may leave it out.",
        json_schema_extra={"rift:proto": {"field": "optional", "number": 4}},
    )
    variadic: bool = Field(
        description="Whether it absorbs the arguments that follow — `*args`, `...rest`.",
        json_schema_extra={"rift:proto": {"field": "variadic", "number": 5}},
    )
    default: str | None = Field(
        description="The default value as written in the source. Null where there is none.",
        json_schema_extra={"rift:proto": {"field": "default", "number": 6}},
    )
    extensions: Extensions = Field(
        description="Parameter facts the model has no field for, namespaced by the adapter that emitted them.",
        json_schema_extra={"rift:proto": {"field": "extensions", "number": 7}},
    )


@definition(
    owner="core", public=True, proto={"type": "rift.core.Signature"}, schema_extra={}
)
class Signature(ClosedModel):
    """One callable form of a symbol: the text it renders as, the symbols that text points at, and its structure. Overloads are separate entries."""

    display: str = Field(
        description="The signature as a reader sees it, in the language's own syntax.",
        examples=["def load_config(path: str, *, strict: bool = False) -> Config"],
        json_schema_extra={"rift:proto": {"field": "display", "number": 1}},
    )
    links: list[SignatureLink] = Field(
        description="Symbols named inside `display`, each with the byte range of `display` that names it, so a renderer can turn the rendered text into links.",
        json_schema_extra={"rift:proto": {"field": "links", "number": 2}},
    )
    language: LanguageId = Field(
        description="The language whose syntax `display` is written in.",
        json_schema_extra={"rift:proto": {"field": "language", "number": 3}},
    )
    receiver: Parameter | None = Field(
        description="The implicit first parameter — `self`, `this`. Null for a free function, and for languages that have no such thing.",
        json_schema_extra={"rift:proto": {"field": "receiver", "number": 4}},
    )
    parameters: list[Parameter] = Field(
        description="Declared parameters, in source order.",
        json_schema_extra={"rift:proto": {"field": "parameters", "number": 5}},
    )
    returns: list[TypeBinding] = Field(
        description="What the call yields. An array because a language may return several values, and because a declared and an inferred return are separate bindings.",
        json_schema_extra={"rift:proto": {"field": "returns", "number": 6}},
    )
    type_parameters: list[SymbolId] = Field(
        description="The generic parameters this form declares, each as the symbol that declares it.",
        json_schema_extra={"rift:proto": {"field": "type_parameters", "number": 7}},
    )
    throws: list[TypeExpression] = Field(
        description="Types this form declares it can raise.",
        json_schema_extra={"rift:proto": {"field": "throws", "number": 8}},
    )
    effects: list[str] = Field(
        description="Effect keywords the declaration carries, in the language's own words: `async`, `unsafe`, `pure`. The spelling is preserved and never mapped onto a portable meaning.",
        examples=[["async"]],
        json_schema_extra={"rift:proto": {"field": "effects", "number": 9}},
    )
    extensions: Extensions = Field(
        description="Signature facts the model has no field for, namespaced by the adapter that emitted them.",
        json_schema_extra={"rift:proto": {"field": "extensions", "number": 10}},
    )


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.SignatureLink"},
    schema_extra={},
)
class SignatureLink(ClosedModel):
    """One symbol named inside a rendered signature, with the byte range of that rendering which names it."""

    range: TextRange = Field(
        description="Offsets into the rendered string in `Signature.display`.",
        json_schema_extra={"rift:proto": {"field": "range", "number": 1}},
    )
    symbol: SymbolId = Field(
        description="The symbol that stretch of text names.",
        json_schema_extra={"rift:proto": {"field": "symbol", "number": 2}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.SymbolFacet",
        "enum": "SymbolFacet",
        "values": {
            "namespace": {"name": "SYMBOL_FACET_NAMESPACE", "number": 1},
            "module": {"name": "SYMBOL_FACET_MODULE", "number": 2},
            "type": {"name": "SYMBOL_FACET_TYPE", "number": 3},
            "value": {"name": "SYMBOL_FACET_VALUE", "number": 4},
            "callable": {"name": "SYMBOL_FACET_CALLABLE", "number": 5},
            "member": {"name": "SYMBOL_FACET_MEMBER", "number": 6},
            "member_container": {"name": "SYMBOL_FACET_MEMBER_CONTAINER", "number": 7},
            "parameter": {"name": "SYMBOL_FACET_PARAMETER", "number": 8},
            "type_parameter": {"name": "SYMBOL_FACET_TYPE_PARAMETER", "number": 9},
            "constructible": {"name": "SYMBOL_FACET_CONSTRUCTIBLE", "number": 10},
            "extensible": {"name": "SYMBOL_FACET_EXTENSIBLE", "number": 11},
            "implementable": {"name": "SYMBOL_FACET_IMPLEMENTABLE", "number": 12},
            "macro": {"name": "SYMBOL_FACET_MACRO", "number": 13},
            "test": {"name": "SYMBOL_FACET_TEST", "number": 14},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "namespace": "Groups names without being a unit the language loads — a C++ `namespace`, a "
            "Rust `mod`.",
            "module": "A unit the language imports whole. A TypeScript file carries this facet; a Rust "
            "module can span several files.",
            "type": "Declares a type. `struct Config` and `interface Props` both qualify, which is what "
            "makes one search for types find both.",
            "value": "Names something that exists while the program runs: a variable, a constant, a "
            "function object.",
            "callable": "Can be called, and so carries at least one `Signature`.",
            "member": "Declared inside another symbol — a method, a field, an enum variant.",
            "member_container": "Can hold members: a class, a struct, an interface.",
            "parameter": "A parameter of a callable, declared as a symbol of its own so it can be "
            "renamed and referred to.",
            "type_parameter": "A generic parameter — the `T` in `Vec<T>`.",
            "constructible": "Can be instantiated — `new Foo()`, `Foo { .. }`.",
            "extensible": "Can be inherited from. A `final` class omits this facet.",
            "implementable": "Names a contract another type can satisfy: an interface, a trait, a "
            "protocol.",
            "macro": "Expands source at compile time, as `macro_rules!` and preprocessor definitions "
            "do.",
            "test": "The language's test tooling collects it as a test: a `#[test]` function, a "
            "`describe` block.",
        }
    },
)
class SymbolFacet(str, Enum):
    """One portable category a symbol falls into. Kinds are language-specific; facets are shared, so a filter written once applies to every language an adapter is installed for."""

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


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class ExactKind(
    ProtocolRoot[
        "Annotated[str, Field(description='An adapter-owned namespaced kind preserving exact language meaning.', pattern='^[A-Za-z][A-Za-z0-9._-]*\\\\.[A-Za-z][A-Za-z0-9._-]*$', json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """An adapter-owned namespaced kind preserving exact language meaning."""


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.LeafFacet",
        "enum": "LeafFacet",
        "values": {
            "declaration": {"name": "LEAF_FACET_DECLARATION", "number": 1},
            "definition": {"name": "LEAF_FACET_DEFINITION", "number": 2},
            "body": {"name": "LEAF_FACET_BODY", "number": 3},
            "block": {"name": "LEAF_FACET_BLOCK", "number": 4},
            "statement": {"name": "LEAF_FACET_STATEMENT", "number": 5},
            "expression": {"name": "LEAF_FACET_EXPRESSION", "number": 6},
            "type_expression": {"name": "LEAF_FACET_TYPE_EXPRESSION", "number": 7},
            "import": {"name": "LEAF_FACET_IMPORT", "number": 8},
            "export": {"name": "LEAF_FACET_EXPORT", "number": 9},
            "parameter": {"name": "LEAF_FACET_PARAMETER", "number": 10},
            "argument": {"name": "LEAF_FACET_ARGUMENT", "number": 11},
            "annotation": {"name": "LEAF_FACET_ANNOTATION", "number": 12},
            "comment": {"name": "LEAF_FACET_COMMENT", "number": 13},
            "identifier": {"name": "LEAF_FACET_IDENTIFIER", "number": 14},
            "literal": {"name": "LEAF_FACET_LITERAL", "number": 15},
            "pattern": {"name": "LEAF_FACET_PATTERN", "number": 16},
            "generated": {"name": "LEAF_FACET_GENERATED", "number": 17},
            "test": {"name": "LEAF_FACET_TEST", "number": 18},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "declaration": "Introduces a name without necessarily giving it a body — a prototype, an "
            "`extern` line.",
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
            "annotation": "Metadata attached to a declaration: a decorator, an attribute, a Java "
            "annotation.",
            "comment": "Text the compiler ignores. A doc comment carries this too; "
            "`RegionRole.documentation` is what ties one to the declaration it describes.",
            "identifier": "A name as written, whether it declares or refers.",
            "literal": "A value written straight into the source — `42`, `true`.",
            "pattern": "A destructuring or matching form: a `match` arm, a binding that pulls fields "
            "apart.",
            "generated": "Produced by a tool from something else in the workspace. Destructive policy "
            "gates on this, because the next build overwrites whatever you write here.",
            "test": "Part of the test suite. Refactor scope gates on this, so a change can be confined "
            "to production code or carried across both.",
        }
    },
)
class LeafFacet(str, Enum):
    """Portable structural facets. generated and test are load-bearing: destructive policy, reference policy, and refactor scope all gate on them."""

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
    owner="core",
    public=True,
    proto={
        "type": "rift.core.RegionRole",
        "enum": "RegionRole",
        "values": {
            "selection": {"name": "REGION_ROLE_SELECTION", "number": 1},
            "name": {"name": "REGION_ROLE_NAME", "number": 2},
            "header": {"name": "REGION_ROLE_HEADER", "number": 3},
            "body": {"name": "REGION_ROLE_BODY", "number": 4},
            "content": {"name": "REGION_ROLE_CONTENT", "number": 5},
            "documentation": {"name": "REGION_ROLE_DOCUMENTATION", "number": 6},
            "enclosing": {"name": "REGION_ROLE_ENCLOSING", "number": 7},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "selection": "What an editor should select on arriving here: the name for a declaration, "
            "the whole node otherwise.",
            "name": "The identifier alone. What a rename rewrites.",
            "header": "Everything before the body — the keyword, the name, the parameters, the return "
            "type.",
            "body": "The implementation. Replacing it leaves the signature and the documentation "
            "standing.",
            "content": "What the node holds where its interior is not code: the text of a comment, the "
            "characters inside a string.",
            "documentation": "The doc comment for this declaration. In most languages it sits outside "
            "the declaration, which is why the leaf has to point at it.",
            "enclosing": "The node with everything that belongs to it, documentation and annotations "
            "included. What a delete should take.",
        }
    },
)
class RegionRole(str, Enum):
    """One named part of a leaf. A language marks these out inside a declaration, so an operation can address the body of a function without addressing its documentation."""

    SELECTION = "selection"
    NAME = "name"
    HEADER = "header"
    BODY = "body"
    CONTENT = "content"
    DOCUMENTATION = "documentation"
    ENCLOSING = "enclosing"


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.RelationshipFacet",
        "enum": "RelationshipFacet",
        "values": {
            "contains": {"name": "RELATIONSHIP_FACET_CONTAINS", "number": 1},
            "declares": {"name": "RELATIONSHIP_FACET_DECLARES", "number": 2},
            "defines": {"name": "RELATIONSHIP_FACET_DEFINES", "number": 3},
            "references": {"name": "RELATIONSHIP_FACET_REFERENCES", "number": 4},
            "calls": {"name": "RELATIONSHIP_FACET_CALLS", "number": 5},
            "constructs": {"name": "RELATIONSHIP_FACET_CONSTRUCTS", "number": 6},
            "reads": {"name": "RELATIONSHIP_FACET_READS", "number": 7},
            "writes": {"name": "RELATIONSHIP_FACET_WRITES", "number": 8},
            "imports": {"name": "RELATIONSHIP_FACET_IMPORTS", "number": 9},
            "exports": {"name": "RELATIONSHIP_FACET_EXPORTS", "number": 10},
            "extends": {"name": "RELATIONSHIP_FACET_EXTENDS", "number": 11},
            "implements": {"name": "RELATIONSHIP_FACET_IMPLEMENTS", "number": 12},
            "type_definition": {
                "name": "RELATIONSHIP_FACET_TYPE_DEFINITION",
                "number": 13,
            },
            "overrides": {"name": "RELATIONSHIP_FACET_OVERRIDES", "number": 14},
            "aliases": {"name": "RELATIONSHIP_FACET_ALIASES", "number": 15},
            "generates": {"name": "RELATIONSHIP_FACET_GENERATES", "number": 16},
            "depends_on": {"name": "RELATIONSHIP_FACET_DEPENDS_ON", "number": 17},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "contains": "`from` lexically holds `to` — a module and the functions written in it.",
            "declares": "`from` introduces `to` as a name.",
            "defines": "`from` gives `to` its body.",
            "references": "`from` mentions `to` and nothing narrower fits. The fallback edge.",
            "calls": "`from` invokes `to`.",
            "constructs": "`from` creates an instance of `to`.",
            "reads": "`from` takes the value of `to`.",
            "writes": "`from` assigns to `to`.",
            "imports": "`from` brings `to` in from another unit.",
            "exports": "`from` makes `to` visible outside its unit.",
            "extends": "`from` inherits from `to`.",
            "implements": "`from` satisfies the contract `to` declares.",
            "type_definition": "`to` is the type of `from`. What jump-to-type-definition follows.",
            "overrides": "`from` replaces an inherited `to`.",
            "aliases": "`from` is another name for `to` — a type alias, a re-export, an `as` rename.",
            "generates": "`to` was produced from `from` by a build step, so editing `to` lasts until "
            "the next build.",
            "depends_on": "`from` needs `to` and the compiler did not say how. The coarsest edge there "
            "is.",
        }
    },
)
class RelationshipFacet(str, Enum):
    """One portable category an edge falls into. Kinds are language-specific, so `typescript.import` and `rust.use` are different kinds that share the `imports` facet, which is what lets one query cross languages."""

    CONTAINS = "contains"
    DECLARES = "declares"
    DEFINES = "defines"
    REFERENCES = "references"
    CALLS = "calls"
    CONSTRUCTS = "constructs"
    READS = "reads"
    WRITES = "writes"
    IMPORTS = "imports"
    EXPORTS = "exports"
    EXTENDS = "extends"
    IMPLEMENTS = "implements"
    TYPE_DEFINITION = "type_definition"
    OVERRIDES = "overrides"
    ALIASES = "aliases"
    GENERATES = "generates"
    DEPENDS_ON = "depends_on"


@definition(owner="core", public=False, proto={}, schema_extra={})
class Empty(ClosedModel):
    """Declared in this workspace's own source. The code you can open and change."""

    kind: Literal["workspace"] = Field()


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.SymbolOriginPackage"},
    schema_extra={},
)
class SymbolOriginPackage(ClosedModel):
    """Declared in a dependency. `manager` is the package manager that installed it, so `npm`, `cargo` or `pip`; `name` and `version` are that manager's own. Upgrading the dependency replaces these symbols."""

    kind: Literal["package"] = Field()
    manager: str = Field(
        description="The package manager that installed it.",
        examples=["npm", "cargo", "pip"],
        json_schema_extra={"rift:proto": {"field": "manager", "number": 1}},
    )
    name: str = Field(
        description="The package name, as that manager spells it.",
        max_length=4096,
        examples=["zod", "serde"],
        json_schema_extra={"rift:proto": {"field": "name", "number": 2}},
    )
    version: str = Field(
        description="The installed version.",
        examples=["3.22.4", "1.0.197"],
        json_schema_extra={"rift:proto": {"field": "version", "number": 3}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.SymbolOrigin",
        "oneof": "variant",
        "variants": [
            {
                "tag": "workspace",
                "field": "workspace",
                "number": 1,
                "type": "google.protobuf.Empty",
            },
            {
                "tag": "package",
                "field": "package",
                "number": 2,
                "type": "SymbolOriginPackage",
            },
            {
                "tag": "stdlib",
                "field": "stdlib",
                "number": 3,
                "type": "google.protobuf.Empty",
            },
            {
                "tag": "external",
                "field": "external",
                "number": 4,
                "type": "google.protobuf.Empty",
            },
            {
                "tag": "generated",
                "field": "generated",
                "number": 5,
                "type": "google.protobuf.Empty",
            },
            {
                "tag": "synthetic",
                "field": "synthetic",
                "number": 6,
                "type": "google.protobuf.Empty",
            },
        ],
    },
    schema_extra={},
)
class SymbolOrigin(
    ProtocolRoot[
        "Annotated[Empty | SymbolOriginPackage | Empty | Empty | Empty | Empty, Field(discriminator='kind')]"
    ]
):
    """Where this symbol is defined. The origins answer different questions: whether you can edit it, whether it moves when you upgrade something, and whether anything can be read from it at all."""


@definition(
    owner="core", public=True, proto={"type": "rift.core.Symbol"}, schema_extra={}
)
class Symbol(ClosedModel):
    """Compiler-resolved semantic identity. Source structure lives in Leaf and is connected through Relationship."""

    id: SymbolId = Field(
        description="Unique identifier of this symbol across the whole workspace, and the URI that resolves it. Hand it to the symbol resource as it stands.",
        examples=["rift://symbol/python/pkg.util.load_config~1?rev=HEAD~3"],
        json_schema_extra={"rift:proto": {"field": "id", "number": 1}},
    )
    language: LanguageId = Field(
        description="The language this symbol belongs to.",
        examples=["typescript", "rust"],
        json_schema_extra={"rift:proto": {"field": "language", "number": 2}},
    )
    name: str = Field(
        description="The human-readable name, as written in the source: `parseConfig`. Rendered signatures live in `signatures`.",
        max_length=4096,
        examples=["parseConfig"],
        json_schema_extra={"rift:proto": {"field": "name", "number": 3}},
    )
    kind: ExactKind = Field(
        description="What this symbol is in its own language's words — `rust.trait`, `typescript.function`. The adapter owns this vocabulary, so each language keeps its own term.",
        examples=["typescript.function", "rust.trait"],
        json_schema_extra={"rift:proto": {"field": "kind", "number": 4}},
    )
    facets: list[SymbolFacet] = Field(
        description="Portable classification, so one query can cross languages. `rust.trait` and `typescript.interface` are different kinds that share the `type` facet, which is what lets a search for types find both.",
        examples=[["value", "callable"]],
        json_schema_extra={
            "rift:proto": {"field": "facets", "number": 5},
            "uniqueItems": True,
        },
    )
    origin: SymbolOrigin = Field(
        description="Where the symbol comes from: code in this workspace, or a package it depends on.",
        json_schema_extra={"rift:proto": {"field": "origin", "number": 6}},
    )
    container: SymbolId | None = Field(
        default=None,
        description="The symbol this one is declared inside — the class that owns a method, the module that owns a function. Absent at the top level.",
        examples=["rift://symbol/typescript/src/config.ts:ConfigLoader"],
        json_schema_extra={"rift:proto": {"field": "container", "number": 7}},
    )
    modifiers: list[str] = Field(
        description="Language keywords qualifying the declaration: `export`, `async`, `const`.",
        examples=[["export", "async"]],
        json_schema_extra={
            "rift:proto": {"field": "modifiers", "number": 8},
            "uniqueItems": True,
        },
    )
    visibility: str | None = Field(
        description="How widely the symbol is visible, in the language's own terms — `public`, `private`, `pub(crate)`. Null where the language has no such concept.",
        examples=["public"],
        json_schema_extra={"rift:proto": {"field": "visibility", "number": 9}},
    )
    types: list[TypeBinding] = Field(
        description="The types this symbol carries, each tagged with the role it plays: a return type, a field type, a bound.",
        json_schema_extra={"rift:proto": {"field": "types", "number": 10}},
    )
    signatures: list[Signature] = Field(
        description="One entry per callable form. Overloads are separate entries.",
        json_schema_extra={"rift:proto": {"field": "signatures", "number": 11}},
    )
    documentation: list[Documentation] = Field(
        description="Doc comments attached to the declaration, with the markup format they were written in.",
        json_schema_extra={"rift:proto": {"field": "documentation", "number": 12}},
    )
    extensions: Extensions = Field(
        description="Language-specific facts with no portable equivalent, namespaced by the adapter that emitted them.",
        json_schema_extra={"rift:proto": {"field": "extensions", "number": 13}},
    )
    document_local: bool = Field(
        description="Whether language semantics confine this symbol to the document that declares it. The compiler adapter classifies the symbol; Rift does not infer locality from observed references.",
        json_schema_extra={"rift:proto": {"field": "document_local", "number": 14}},
    )


@definition(
    owner="core", public=True, proto={"type": "rift.core.LeafRegion"}, schema_extra={}
)
class LeafRegion(ClosedModel):
    """One named part of a leaf, and the bytes it spans."""

    role: RegionRole = Field(
        description="Which part of the leaf this is.",
        json_schema_extra={"rift:proto": {"field": "role", "number": 1}},
    )
    range: TextRange = Field(
        description="Offsets into the file, on the same scale as `Leaf.range`.",
        json_schema_extra={"rift:proto": {"field": "range", "number": 2}},
    )


@definition(
    owner="core", public=True, proto={"type": "rift.core.Leaf"}, schema_extra={}
)
class Leaf(ClosedModel):
    """One leaf of a file's concrete syntax tree: the place where a symbol is physically written. It carries where it is — the unit, the byte range, the syntax kind — and the symbol it writes. Everything semantic about that symbol is read from the symbol."""

    id: LeafId = Field(
        description="Unique identifier of this source region, and the URI that resolves it.",
        json_schema_extra={"rift:proto": {"field": "id", "number": 1}},
    )
    symbol: SymbolId | None = Field(
        default=None,
        description="The symbol written at this leaf. Absent where a leaf writes no symbol — punctuation, a keyword, a comment.",
        json_schema_extra={"rift:proto": {"field": "symbol", "number": 2}},
    )
    unit: FileId = Field(
        description="The file the leaf is written in.",
        json_schema_extra={"rift:proto": {"field": "unit", "number": 3}},
    )
    language: LanguageId = Field(
        description="The grammar that produced this leaf. It belongs to the identity because two adapters can produce different trees over the same file bytes.",
        json_schema_extra={"rift:proto": {"field": "language", "number": 4}},
    )
    kind: ExactKind = Field(
        description="What the node is in its own grammar's words — `rust.fn_item`, `python.call`.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 5}},
    )
    facets: list[LeafFacet] = Field(
        description="Portable structural classification, so a query can ask for bodies or imports without knowing the grammar that produced them.",
        json_schema_extra={
            "rift:proto": {"field": "facets", "number": 6},
            "uniqueItems": True,
        },
    )
    range: TextRange = Field(
        description="The bytes it spans, as offsets into the file.",
        json_schema_extra={"rift:proto": {"field": "range", "number": 7}},
    )
    regions: list[LeafRegion] = Field(
        description="The leaf's named parts, so an operation can rewrite a function body without touching the documentation above it.",
        json_schema_extra={"rift:proto": {"field": "regions", "number": 8}},
    )
    parent: LeafId | None = Field(
        default=None,
        description="The region this one is nested inside. Absent at the top level of a unit.",
        json_schema_extra={"rift:proto": {"field": "parent", "number": 9}},
    )
    extensions: Extensions = Field(
        description="Syntax facts the model has no field for, namespaced by the adapter that emitted them.",
        json_schema_extra={"rift:proto": {"field": "extensions", "number": 10}},
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "derivation",
        "number": 6,
        "enum": "Derivation",
        "values": {
            "resolution": {"name": "RESOLUTION", "number": 1},
            "syntax": {"name": "SYNTAX", "number": 2},
            "heuristic": {"name": "HEURISTIC", "number": 3},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "resolution": "The compiler's own name resolution or type checker produced the edge. It is "
            "a fact about the program.",
            "syntax": "Read off the syntax tree because the compiler did not resolve it — a call on an "
            "untyped receiver matched by name. Repeatable, and still capable of being wrong.",
            "heuristic": "A guess, with `confidence` saying how good a one. Required there and "
            "meaningless elsewhere.",
        }
    },
)
class RelationshipDerivation(str, Enum):
    """How this edge was established. Every edge reaches Rift from an adapter; this field records how much the compiler knew. A refactor may use `resolution` directly. Lower levels require another check before rewriting."""

    RESOLUTION = "resolution"
    SYNTAX = "syntax"
    HEURISTIC = "heuristic"


@definition(
    owner="core", public=True, proto={"type": "rift.core.Relationship"}, schema_extra={}
)
class Relationship(ClosedModel):
    """One directed edge between two symbols. Its evidence is the leaves it was read from, and its derivation is how much the compiler knew when it was read."""

    from_: SymbolId = Field(
        alias="from",
        description="The symbol the edge starts at.",
        json_schema_extra={"rift:proto": {"field": "from", "number": 1}},
    )
    kind: ExactKind = Field(
        description="What the edge is in its own language's words — `typescript.import`, `rust.impl`.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 2}},
    )
    facets: list[RelationshipFacet] = Field(
        description="Portable classification, so a query for `imports` finds `typescript.import` and `rust.use` alike.",
        min_length=1,
        json_schema_extra={
            "rift:proto": {"field": "facets", "number": 3},
            "uniqueItems": True,
        },
    )
    to: SymbolId = Field(
        description="The symbol the edge points at. One Rift cannot read carries the `external` origin; the edge is the same either way.",
        json_schema_extra={"rift:proto": {"field": "to", "number": 4}},
    )
    evidence: list[LeafId] = Field(
        description="The leaves this edge was read from.",
        json_schema_extra={"rift:proto": {"field": "evidence", "number": 5}},
    )
    derivation: RelationshipDerivation = Field(
        description="How this edge was established. Every edge reaches Rift from an adapter; this field records how much the compiler knew. A refactor may use `resolution` directly. Lower levels require another check before rewriting.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "resolution": "The compiler's own name resolution or type checker produced the edge. It is "
                "a fact about the program.",
                "syntax": "Read off the syntax tree because the compiler did not resolve it — a call on an "
                "untyped receiver matched by name. Repeatable, and still capable of being wrong.",
                "heuristic": "A guess, with `confidence` saying how good a one. Required there and "
                "meaningless elsewhere.",
            },
            "rift:proto": {
                "field": "derivation",
                "number": 6,
                "enum": "Derivation",
                "values": {
                    "resolution": {"name": "RESOLUTION", "number": 1},
                    "syntax": {"name": "SYNTAX", "number": 2},
                    "heuristic": {"name": "HEURISTIC", "number": 3},
                },
            },
        },
    )
    confidence: float | None = Field(
        default=None,
        description="How likely a `heuristic` edge is to hold, from 0 to 1. Absent for any other derivation.",
        ge=0,
        le=1,
        json_schema_extra={"rift:proto": {"field": "confidence", "number": 7}},
    )
    extensions: Extensions = Field(
        description="Edge facts the model has no field for, namespaced by the adapter that emitted them.",
        json_schema_extra={"rift:proto": {"field": "extensions", "number": 8}},
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "op",
        "number": 2,
        "enum": "Op",
        "values": {
            "eq": {"name": "EQ", "number": 1},
            "ne": {"name": "NE", "number": 2},
            "in": {"name": "IN", "number": 3},
            "contains": {"name": "CONTAINS", "number": 4},
            "prefix": {"name": "PREFIX", "number": 5},
            "regex": {"name": "REGEX", "number": 6},
            "gt": {"name": "GT", "number": 7},
            "gte": {"name": "GTE", "number": 8},
            "lt": {"name": "LT", "number": 9},
            "lte": {"name": "LTE", "number": 10},
            "exists": {"name": "EXISTS", "number": 11},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "eq": "The field equals the operand.",
            "ne": "The field does not equal the operand.",
            "in": "The field equals one of `values`.",
            "contains": "The field holds the operand: a substring of a string, a member of an array "
            "such as `facets`.",
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
    """How the operand is compared against the field. What a comparison means follows the field's type, so ordering ops apply only where the values are ordered."""

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


@definition(
    owner="core", public=True, proto={"type": "rift.core.FieldFilter"}, schema_extra={}
)
class FieldFilter(ClosedModel):
    """A predicate over a standard, namespaced substrate, or diagnostic field. Rift evaluates the regex operation under `rift-regex`, including filters nested in `StructuralCaptureConstraint.semantic`. Structural patterns and path selectors carry separate grammars."""

    field: str = Field(
        description="Which field to test, by its name in this model: `facets`, `origin.kind`, `severity`. Extension keys and diagnostic fields are addressed the same way.",
        json_schema_extra={"rift:proto": {"field": "field", "number": 1}},
    )
    op: FieldFilterOp = Field(
        description="How the operand is compared against the field. What a comparison means follows the field's type, so ordering ops apply only where the values are ordered.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "eq": "The field equals the operand.",
                "ne": "The field does not equal the operand.",
                "in": "The field equals one of `values`.",
                "contains": "The field holds the operand: a substring of a string, a member of an array "
                "such as `facets`.",
                "prefix": "The field is a string starting with the operand.",
                "regex": "The field matches the `rift-regex` expression in the operand. Rift evaluates it.",
                "gt": "The field is greater than the operand.",
                "gte": "The field is greater than or equal to the operand.",
                "lt": "The field is less than the operand.",
                "lte": "The field is less than or equal to the operand.",
                "exists": "The field is present at all. No operand is read.",
            },
            "rift:proto": {
                "field": "op",
                "number": 2,
                "enum": "Op",
                "values": {
                    "eq": {"name": "EQ", "number": 1},
                    "ne": {"name": "NE", "number": 2},
                    "in": {"name": "IN", "number": 3},
                    "contains": {"name": "CONTAINS", "number": 4},
                    "prefix": {"name": "PREFIX", "number": 5},
                    "regex": {"name": "REGEX", "number": 6},
                    "gt": {"name": "GT", "number": 7},
                    "gte": {"name": "GTE", "number": 8},
                    "lt": {"name": "LT", "number": 9},
                    "lte": {"name": "LTE", "number": 10},
                    "exists": {"name": "EXISTS", "number": 11},
                },
            },
        },
    )
    value: Any | None = Field(
        default=None,
        description="The operand, for every op except `in` and `exists`.",
        json_schema_extra={"rift:proto": {"field": "value", "number": 3}},
    )
    values: list[Any] | None = Field(
        default=None,
        description="The operands for `in`.",
        json_schema_extra={"rift:proto": {"field": "values", "number": 4}},
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "direction",
        "number": 3,
        "enum": "Direction",
        "values": {
            "outgoing": {"name": "DIRECTION_OUTGOING", "number": 1},
            "incoming": {"name": "DIRECTION_INCOMING", "number": 2},
            "either": {"name": "DIRECTION_EITHER", "number": 3},
        },
    },
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
    owner="core",
    public=False,
    proto={
        "field": "quantifier",
        "number": 7,
        "enum": "Quantifier",
        "values": {
            "exists": {"name": "QUANTIFIER_EXISTS", "number": 1},
            "not_exists": {"name": "QUANTIFIER_NOT_EXISTS", "number": 2},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "exists": "At least one edge matches.",
            "not_exists": 'No edge matches. How "a symbol nothing calls" is written.',
        }
    },
)
class RelationFilterQuantifier(str, Enum):
    """Whether a match needs such an edge, or needs there to be none."""

    EXISTS = "exists"
    NOT_EXISTS = "not_exists"


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.RelationFilter"},
    schema_extra={},
)
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
                    "description": "Matching by portable facet, which reaches every language an adapter is installed for.",
                    "required": ["facet"],
                },
            ]
        }
    )
    kind: list[str] | None = Field(
        default=None,
        description="Exact relationship kinds advertised by an adapter. Any listed kind matches.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}},
    )
    facet: list[RelationshipFacet] | None = Field(
        default=None,
        description="Portable relationship facets. Any listed facet matches.",
        json_schema_extra={"rift:proto": {"field": "facet", "number": 2}},
    )
    direction: RelationFilterDirection = Field(
        description="Which way the edge runs, seen from the entity being filtered.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "outgoing": "Edges that start at the entity — what it calls, imports, extends.",
                "incoming": "Edges that point at it — its callers, its implementors.",
                "either": "Edges in both directions.",
            },
            "rift:proto": {
                "field": "direction",
                "number": 3,
                "enum": "Direction",
                "values": {
                    "outgoing": {"name": "DIRECTION_OUTGOING", "number": 1},
                    "incoming": {"name": "DIRECTION_INCOMING", "number": 2},
                    "either": {"name": "DIRECTION_EITHER", "number": 3},
                },
            },
        },
    )
    target: Filter | None = Field(
        default=None,
        description='What has to be true of the entity at the other end. Nesting a filter here is how "callers that are tests" becomes one query.',
        json_schema_extra={"rift:proto": {"field": "target", "number": 4}},
    )
    min_depth: int | None = Field(
        default=None,
        description="How many edges to walk before a hit counts. Above 1 this asks about indirect neighbours and skips the direct ones.",
        ge=1,
        json_schema_extra={"rift:proto": {"field": "min_depth", "number": 5}},
    )
    max_depth: int | None = Field(
        default=None,
        description="How many edges a traversal may cross. Call graphs contain cycles and can span the workspace, so every traversal has this bound.",
        ge=1,
        le=100,
        json_schema_extra={"rift:proto": {"field": "max_depth", "number": 6}},
    )
    quantifier: RelationFilterQuantifier | None = Field(
        default=None,
        description="Whether a match needs such an edge, or needs there to be none.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "exists": "At least one edge matches.",
                "not_exists": 'No edge matches. How "a symbol nothing calls" is written.',
            },
            "rift:proto": {
                "field": "quantifier",
                "number": 7,
                "enum": "Quantifier",
                "values": {
                    "exists": {"name": "QUANTIFIER_EXISTS", "number": 1},
                    "not_exists": {"name": "QUANTIFIER_NOT_EXISTS", "number": 2},
                },
            },
        },
    )


@definition(
    owner="core", public=False, proto={"type": "rift.core.FilterField"}, schema_extra={}
)
class FilterField(ClosedModel):
    """A test on one field of the entity."""

    kind: Literal["field"] = Field()
    field: FieldFilter = Field(
        description="The field and the comparison.",
        json_schema_extra={"rift:proto": {"field": "field", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.FilterRelation"},
    schema_extra={},
)
class FilterRelation(ClosedModel):
    """A test on the edges the entity has."""

    kind: Literal["relation"] = Field()
    relation: RelationFilter = Field(
        description="The edges to look for, and what they must reach.",
        json_schema_extra={"rift:proto": {"field": "relation", "number": 1}},
    )


@definition(
    owner="core", public=False, proto={"type": "rift.core.FilterAll"}, schema_extra={}
)
class FilterAll(ClosedModel):
    """Conjunction: every member has to hold."""

    kind: Literal["all"] = Field()
    all: list[Filter] = Field(
        description="The filters that must all hold.",
        json_schema_extra={"rift:proto": {"field": "all", "number": 1}},
    )


@definition(
    owner="core", public=False, proto={"type": "rift.core.FilterAny"}, schema_extra={}
)
class FilterAny(ClosedModel):
    """Disjunction: at least one member has to hold."""

    kind: Literal["any"] = Field()
    any: list[Filter] = Field(
        description="The filters, of which one is enough.",
        json_schema_extra={"rift:proto": {"field": "any", "number": 1}},
    )


@definition(
    owner="core", public=False, proto={"type": "rift.core.FilterNot"}, schema_extra={}
)
class FilterNot(ClosedModel):
    """Negation of what it holds."""

    kind: Literal["not"] = Field()
    not_: Filter = Field(
        alias="not",
        description="The filter being negated.",
        json_schema_extra={"rift:proto": {"field": "not", "number": 1}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.Filter",
        "oneof": "variant",
        "variants": [
            {"tag": "field", "field": "field", "number": 1, "type": "FilterField"},
            {
                "tag": "relation",
                "field": "relation",
                "number": 2,
                "type": "FilterRelation",
            },
            {"tag": "all", "field": "all", "number": 3, "type": "FilterAll"},
            {"tag": "any", "field": "any", "number": 4, "type": "FilterAny"},
            {"tag": "not", "field": "not", "number": 5, "type": "FilterNot"},
        ],
    },
    schema_extra={},
)
class Filter(
    ProtocolRoot[
        "Annotated[FilterField | FilterRelation | FilterAll | FilterAny | FilterNot, Field(discriminator='kind')]"
    ]
):
    """A recursive typed predicate. Every branch is tagged, so a filter tree parses in one pass."""


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.AddressSymbol"},
    schema_extra={},
)
class AddressSymbol(ClosedModel):
    """A symbol, wherever it happens to be written. Addressed this way, a rename reaches every leaf the compiler knows about."""

    kind: Literal["symbol"] = Field()
    symbol: SymbolId = Field(
        description="The symbol the operation applies to.",
        json_schema_extra={"rift:proto": {"field": "symbol", "number": 1}},
    )


@definition(
    owner="core", public=False, proto={"type": "rift.core.AddressLeaf"}, schema_extra={}
)
class AddressLeaf(ClosedModel):
    """One node of one file's syntax tree, optionally narrowed to one of its named parts."""

    kind: Literal["leaf"] = Field()
    leaf: LeafId = Field(
        description="The node the operation applies to.",
        json_schema_extra={"rift:proto": {"field": "leaf", "number": 1}},
    )
    region: RegionRole | None = Field(
        default=None,
        description="Which part of the leaf. Absent, the whole of it.",
        json_schema_extra={"rift:proto": {"field": "region", "number": 2}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.AddressSource"},
    schema_extra={},
)
class AddressSource(ClosedModel):
    """A byte range, whether or not anything was parsed there. This is what addresses a `LICENSE` file, or a region of a file no adapter claims."""

    kind: Literal["source"] = Field()
    span: SourceSpan = Field(
        description="The file, and the bytes in it.",
        json_schema_extra={"rift:proto": {"field": "span", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.AddressMatch"},
    schema_extra={},
)
class AddressMatch(ClosedModel):
    """A result of a `StructuralQuery`, by the identity it came back with. That identity carries its source state; resolution returns `stale_match` when the candidate has moved."""

    kind: Literal["match"] = Field()
    match: MatchKey = Field(
        description="The match, and the state it was found in.",
        json_schema_extra={"rift:proto": {"field": "match", "number": 1}},
    )
    capture: str | None = Field(
        description="Which named capture of the match. Null addresses the match itself.",
        json_schema_extra={"rift:proto": {"field": "capture", "number": 2}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.Address",
        "oneof": "variant",
        "variants": [
            {"tag": "symbol", "field": "symbol", "number": 1, "type": "AddressSymbol"},
            {"tag": "leaf", "field": "leaf", "number": 2, "type": "AddressLeaf"},
            {"tag": "source", "field": "source", "number": 3, "type": "AddressSource"},
            {"tag": "match", "field": "match", "number": 4, "type": "AddressMatch"},
        ],
    },
    schema_extra={},
)
class Address(
    ProtocolRoot[
        "Annotated[AddressSymbol | AddressLeaf | AddressSource | AddressMatch, Field(discriminator='kind')]"
    ]
):
    """Where an operation applies. Separate union branches distinguish a semantic symbol, a syntax leaf, a source range, and a retained match."""


@definition(
    owner="core",
    public=False,
    proto={
        "field": "cardinality",
        "number": 2,
        "enum": "Cardinality",
        "values": {
            "one": {"name": "ONE", "number": 1},
            "many": {"name": "MANY", "number": 2},
        },
    },
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


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.StructuralCaptureConstraint"},
    schema_extra={},
)
class StructuralCaptureConstraint(ClosedModel):
    """A condition one capture of a `StructuralQuery` has to meet. Constraints are where a pattern over syntax reaches the compiler's knowledge: match every call, then keep only the ones whose target carries the `test` facet."""

    capture: str = Field(
        description="Which metavariable of the pattern this constrains, written without the `$`.",
        pattern="^[A-Za-z][A-Za-z0-9_]*$",
        json_schema_extra={"rift:proto": {"field": "capture", "number": 1}},
    )
    cardinality: StructuralCaptureConstraintCardinality = Field(
        description="How many nodes the capture binds, which follows the sigil used in the pattern.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "one": "Exactly one node, as `$NAME` binds.",
                "many": "A run of nodes, as `$$$NAME` binds.",
            },
            "rift:proto": {
                "field": "cardinality",
                "number": 2,
                "enum": "Cardinality",
                "values": {
                    "one": {"name": "ONE", "number": 1},
                    "many": {"name": "MANY", "number": 2},
                },
            },
        },
    )
    exact_kind: ExactKind | None = Field(
        default=None,
        description="The syntax kind the captured node has to be, in its grammar's own words.",
        json_schema_extra={"rift:proto": {"field": "exact_kind", "number": 3}},
    )
    facet: LeafFacet | None = Field(
        default=None,
        description="A portable structural facet the captured node has to carry.",
        json_schema_extra={"rift:proto": {"field": "facet", "number": 4}},
    )
    text_regex: str | None = Field(
        default=None,
        description="A `rift-regex` expression the capture's text must match. Rift evaluates it with Unicode character classes, linear-time matching, and no look-around or backreferences.",
        json_schema_extra={"rift:proto": {"field": "text_regex", "number": 5}},
    )
    semantic: Filter | None = Field(
        default=None,
        description="What has to be true of the symbol written at the capture. The adapter matches syntax; Rift applies this, because only Rift holds the resolved facts.",
        json_schema_extra={"rift:proto": {"field": "semantic", "number": 6}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.MatchQuery",
        "oneof": "variant",
        "variants": [
            {
                "tag": None,
                "field": "structural_query",
                "number": 1,
                "type": "StructuralQuery",
            },
            {"tag": None, "field": "text_query", "number": 2, "type": "TextQuery"},
        ],
    },
    schema_extra={},
)
class MatchQuery(
    ProtocolRoot["Annotated[StructuralQuery | TextQuery, Field(discriminator='kind')]"]
):
    """A text or structural pattern, tagged by the engine that evaluates it. Carrying the complete query lets a match key explain its own identity and lets a caller rerun it without retrieving hidden state."""


@definition(
    owner="core",
    public=False,
    proto={
        "field": "mode",
        "number": 4,
        "enum": "Mode",
        "values": {
            "literal": {"name": "LITERAL", "number": 1},
            "regex": {"name": "REGEX", "number": 2},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "literal": "Match the pattern bytewise, with no special characters or flags.",
            "regex": "Read the pattern using the advertised text-matching dialect and the initial flags "
            "below.",
        }
    },
)
class TextQueryMode(str, Enum):
    """How Rift interprets `pattern`."""

    LITERAL = "literal"
    REGEX = "regex"


@definition(
    owner="core", public=True, proto={"type": "rift.core.TextQuery"}, schema_extra={}
)
class TextQuery(ClosedModel):
    """Literal or regular-expression matching over any UTF-8 file. Rift evaluates this query even when no adapter claims the path. Matches are leftmost-first and non-overlapping. After an empty regular-expression match, scanning advances to the next UTF-8 boundary. The query carries its dialect so a stored match key remains self-contained."""

    kind: Literal["text"] = Field(
        description="Selects Rift's text matcher.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}},
    )
    version: Literal[1] = Field(
        description="Version of the query shape and pattern semantics.",
        json_schema_extra={"rift:proto": {"field": "version", "number": 2}},
    )
    dialect: Literal["rift-regex"] = Field(
        description="Regular-expression grammar used in `regex` mode. The separate `version` field selects its syntax and match semantics. Version 1 has Unicode character classes, linear-time matching, and no look-around or backreferences. Protocol conformance fixtures define accepted syntax and match selection.",
        json_schema_extra={"rift:proto": {"field": "dialect", "number": 3}},
    )
    mode: TextQueryMode = Field(
        description="How Rift interprets `pattern`.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "literal": "Match the pattern bytewise, with no special characters or flags.",
                "regex": "Read the pattern using the advertised text-matching dialect and the initial flags "
                "below.",
            },
            "rift:proto": {
                "field": "mode",
                "number": 4,
                "enum": "Mode",
                "values": {
                    "literal": {"name": "LITERAL", "number": 1},
                    "regex": {"name": "REGEX", "number": 2},
                },
            },
        },
    )
    pattern: str = Field(
        description="Text to find in the selected grammar.",
        min_length=1,
        json_schema_extra={"rift:proto": {"field": "pattern", "number": 5}},
    )
    case_sensitive: bool = Field(
        description="Initial case-sensitivity for regular-expression mode. Inline flags may change it within the pattern.",
        json_schema_extra={"rift:proto": {"field": "case_sensitive", "number": 6}},
    )
    multiline: bool = Field(
        description="Whether `^` and `$` initially match line boundaries in regular-expression mode. Inline flags may change it within the pattern.",
        json_schema_extra={"rift:proto": {"field": "multiline", "number": 7}},
    )
    paths: PathSelector = Field(
        description="Files searched by this query.",
        json_schema_extra={"rift:proto": {"field": "paths", "number": 8}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.StructuralQueryWithinSymbol"},
    schema_extra={},
)
class StructuralQueryWithinSymbol(ClosedModel):
    """Every leaf that writes one symbol."""

    kind: Literal["symbol"] = Field(description="Selects symbol scope.")
    symbol: SymbolId = Field(
        json_schema_extra={"rift:proto": {"field": "symbol", "number": 1}}
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.StructuralQueryWithinLeaf"},
    schema_extra={},
)
class StructuralQueryWithinLeaf(ClosedModel):
    """One syntax subtree."""

    kind: Literal["leaf"] = Field(description="Selects leaf scope.")
    leaf: LeafId = Field(
        json_schema_extra={"rift:proto": {"field": "leaf", "number": 1}}
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.StructuralQueryWithinSource"},
    schema_extra={},
)
class StructuralQueryWithinSource(ClosedModel):
    """One source byte range."""

    kind: Literal["source"] = Field(description="Selects source-range scope.")
    span: SourceSpan = Field(
        json_schema_extra={"rift:proto": {"field": "span", "number": 1}}
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "within",
        "number": 6,
        "type": "rift.core.StructuralQueryWithin",
        "oneof": "variant",
        "variants": [
            {
                "tag": "symbol",
                "field": "symbol",
                "number": 1,
                "type": "StructuralQueryWithinSymbol",
            },
            {
                "tag": "leaf",
                "field": "leaf",
                "number": 2,
                "type": "StructuralQueryWithinLeaf",
            },
            {
                "tag": "source",
                "field": "source",
                "number": 3,
                "type": "StructuralQueryWithinSource",
            },
        ],
    },
    schema_extra={},
)
class StructuralQueryWithin(
    ProtocolRoot[
        "Annotated[StructuralQueryWithinSymbol | StructuralQueryWithinLeaf | StructuralQueryWithinSource, Field(discriminator='kind')]"
    ]
):
    """Optional source boundary for the search. A symbol selects its leaves. A leaf selects its syntax subtree. A span selects its bytes."""


@definition(
    owner="core",
    public=False,
    proto={
        "field": "overlap",
        "number": 8,
        "enum": "Overlap",
        "values": {
            "outermost": {"name": "OUTERMOST", "number": 1},
            "innermost": {"name": "INNERMOST", "number": 2},
            "all_non_overlapping": {"name": "ALL_NON_OVERLAPPING", "number": 3},
        },
    },
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


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.StructuralQuery"},
    schema_extra={},
)
class StructuralQuery(ClosedModel):
    """A syntax search parsed with the target language's grammar. `$NAME` stands for one node and `$$$NAME` for a run of nodes. Each adapter uses the shared capture, constraint, and result shapes."""

    kind: Literal["structural"] = Field(
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}}
    )
    version: Literal[1] = Field(
        description="Which revision of the metavariable grammar the pattern is written in.",
        json_schema_extra={"rift:proto": {"field": "version", "number": 2}},
    )
    language: LanguageId = Field(
        description="Which grammar parses `pattern`, and so which adapter runs the search.",
        json_schema_extra={"rift:proto": {"field": "language", "number": 3}},
    )
    pattern: str = Field(
        description="The pattern, in the target language's own syntax, with metavariables where the shape matters and the text does not.",
        min_length=1,
        json_schema_extra={"rift:proto": {"field": "pattern", "number": 4}},
    )
    paths: PathSelector = Field(
        description="Which files to search.",
        json_schema_extra={"rift:proto": {"field": "paths", "number": 5}},
    )
    within: StructuralQueryWithin | None = Field(
        default=None,
        description="Optional source boundary for the search. A symbol selects its leaves. A leaf selects its syntax subtree. A span selects its bytes.",
        json_schema_extra={
            "rift:proto": {
                "field": "within",
                "number": 6,
                "type": "rift.core.StructuralQueryWithin",
                "oneof": "variant",
                "variants": [
                    {
                        "tag": "symbol",
                        "field": "symbol",
                        "number": 1,
                        "type": "StructuralQueryWithinSymbol",
                    },
                    {
                        "tag": "leaf",
                        "field": "leaf",
                        "number": 2,
                        "type": "StructuralQueryWithinLeaf",
                    },
                    {
                        "tag": "source",
                        "field": "source",
                        "number": 3,
                        "type": "StructuralQueryWithinSource",
                    },
                ],
            }
        },
    )
    constraints: list[StructuralCaptureConstraint] = Field(
        description="Conditions the captures have to meet. The pattern finds a shape; these narrow it to the occurrences you meant.",
        json_schema_extra={"rift:proto": {"field": "constraints", "number": 7}},
    )
    overlap: StructuralQueryOverlap = Field(
        description="Which match to keep when one encloses another, as a nested call chain does.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "outermost": "Keep the enclosing match. A chain of nested calls reports once, at the top.",
                "innermost": "Keep the enclosed match, and report the chain at its deepest point.",
                "all_non_overlapping": "Keep every match that does not overlap one already kept.",
            },
            "rift:proto": {
                "field": "overlap",
                "number": 8,
                "enum": "Overlap",
                "values": {
                    "outermost": {"name": "OUTERMOST", "number": 1},
                    "innermost": {"name": "INNERMOST", "number": 2},
                    "all_non_overlapping": {"name": "ALL_NON_OVERLAPPING", "number": 3},
                },
            },
        },
    )
    extensions: Extensions = Field(
        description="Query fields the model has no place for, namespaced by the adapter that reads them.",
        json_schema_extra={"rift:proto": {"field": "extensions", "number": 9}},
    )


@definition(
    owner="core", public=True, proto={"type": "rift.core.Capture"}, schema_extra={}
)
class Capture(ClosedModel):
    """One named capture of a match, and the bytes it bound."""

    name: CaptureName = Field(
        description="Which metavariable of the pattern bound these bytes.",
        json_schema_extra={"rift:proto": {"field": "name", "number": 1}},
    )
    spans: list[SourceSpan] = Field(
        description="Where it bound. A capture that matched a run of nodes has one span per node.",
        min_length=1,
        json_schema_extra={"rift:proto": {"field": "spans", "number": 2}},
    )
    text: str = Field(
        description="The source it bound, as written.",
        json_schema_extra={"rift:proto": {"field": "text", "number": 3}},
    )
    entity: LeafId | None = Field(
        description="The leaf this capture landed on, or null when the capture is text and nothing was parsed — as every text-regex capture is. What the leaf means is read through `Leaf.symbol`.",
        json_schema_extra={"rift:proto": {"field": "entity", "number": 4}},
    )


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.StructuralMatchRanges"},
    schema_extra={},
)
class StructuralMatchRanges(ClosedModel):
    """The ranges a replacement can be written over, computed by the adapter that parsed the match. Whether the trivia around a node belongs to the edit depends on what you are doing — deleting a list element has to take a separator with it — and only the grammar knows where that trivia ends."""

    exact: SourceSpan = Field(
        description="The node itself, with nothing around it. What a replacement in place writes over.",
        json_schema_extra={"rift:proto": {"field": "exact", "number": 1}},
    )
    leading: SourceSpan = Field(
        description="The node with the trivia in front of it: the indentation on its line, a separator that precedes it.",
        json_schema_extra={"rift:proto": {"field": "leading", "number": 2}},
    )
    trailing: SourceSpan = Field(
        description="The node with what follows it: a trailing comma, the newline that ends its line.",
        json_schema_extra={"rift:proto": {"field": "trailing", "number": 3}},
    )
    both: SourceSpan = Field(
        description="The node with the trivia on either side. Deleting this leaves no blank line and no orphaned separator.",
        json_schema_extra={"rift:proto": {"field": "both", "number": 4}},
    )


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class CaptureName(
    ProtocolRoot[
        "Annotated[str, Field(description='Structural captures use identifier names; text regex results may additionally use numeric capture indexes.', pattern='^(?:[A-Za-z][A-Za-z0-9_]*|[0-9]+)$', json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """Structural captures use identifier names; text regex results may additionally use numeric capture indexes."""


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.DiagnosticRelated"},
    schema_extra={},
)
class DiagnosticRelated(ClosedModel):
    """A second place the compiler wants you to look — the earlier declaration a redefinition conflicts with, the bound that failed. It carries a message and a location, and never a severity of its own, because it is part of one finding."""

    message: str = Field(
        description='What to notice there — "first defined here", "required by this bound".',
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "message", "number": 1}},
    )
    span: SourceSpan = Field(
        description="Where to look.",
        json_schema_extra={"rift:proto": {"field": "span", "number": 2}},
    )


@definition(
    owner="core",
    public=False,
    proto={
        "enum": "Tags",
        "values": {
            "deprecated": {"name": "TAGS_DEPRECATED", "number": 1},
            "unnecessary": {"name": "TAGS_UNNECESSARY", "number": 2},
        },
    },
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
    owner="core",
    public=False,
    proto={
        "field": "reliability",
        "number": 7,
        "enum": "Reliability",
        "values": {
            "reliable": {"name": "RELIABILITY_RELIABLE", "number": 1},
            "recovered": {"name": "RELIABILITY_RECOVERED", "number": 2},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "reliable": "The compiler parsed the file. Facts around this finding stand.",
            "recovered": "The parser repaired the source to keep going, so the tree here is a guess and "
            "so is anything read from it.",
        }
    },
)
class DiagnosticReliability(str, Enum):
    """Whether the facts around this finding came off a clean parse."""

    RELIABLE = "reliable"
    RECOVERED = "recovered"


@definition(
    owner="core",
    public=False,
    proto={
        "field": "continuation",
        "number": 8,
        "enum": "Continuation",
        "values": {
            "repairable": {"name": "CONTINUATION_REPAIRABLE", "number": 1},
            "unrepairable": {"name": "CONTINUATION_UNREPAIRABLE", "number": 2},
            "unknown": {"name": "CONTINUATION_UNKNOWN", "number": 3},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "repairable": "Appending source can make it go away: an unclosed brace, a statement cut off "
            "at the end of the file.",
            "unrepairable": "It stands whatever follows.",
            "unknown": "The compiler does not say.",
        }
    },
)
class DiagnosticContinuation(str, Enum):
    """Whether the finding is an artefact of source that stops mid-way, which is the normal state of a file an agent is halfway through writing."""

    REPAIRABLE = "repairable"
    UNREPAIRABLE = "unrepairable"
    UNKNOWN = "unknown"


@definition(
    owner="core", public=True, proto={"type": "rift.core.Diagnostic"}, schema_extra={}
)
class Diagnostic(ClosedModel):
    """One thing a compiler said about the source. The message and the code are passed through in the compiler's own words, because a normalised message loses the detail that makes it actionable."""

    severity: Severity = Field(
        description="How much it matters.",
        json_schema_extra={"rift:proto": {"field": "severity", "number": 1}},
    )
    code: str | None = Field(
        description="The compiler's own identifier for this finding — `TS2345`, `E0308`. Null where the compiler issues none.",
        json_schema_extra={"rift:proto": {"field": "code", "number": 2}},
    )
    message: str = Field(
        description="What the compiler said, in its own words.",
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "message", "number": 3}},
    )
    span: SourceSpan | None = Field(
        description="Where it applies. Null for a finding about the file as a whole, or about the build.",
        json_schema_extra={"rift:proto": {"field": "span", "number": 4}},
    )
    related: list[DiagnosticRelated] = Field(
        description="Other places the compiler pointed at while explaining this one.",
        json_schema_extra={"rift:proto": {"field": "related", "number": 5}},
    )
    tags: list[DiagnosticTagsItemTags] = Field(
        description="Presentation tags for the finding. An editor can render them as strikethrough or grey text.",
        json_schema_extra={
            "rift:proto": {"field": "tags", "number": 6},
            "uniqueItems": True,
        },
    )
    reliability: DiagnosticReliability = Field(
        description="Whether the facts around this finding came off a clean parse.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "reliable": "The compiler parsed the file. Facts around this finding stand.",
                "recovered": "The parser repaired the source to keep going, so the tree here is a guess and "
                "so is anything read from it.",
            },
            "rift:proto": {
                "field": "reliability",
                "number": 7,
                "enum": "Reliability",
                "values": {
                    "reliable": {"name": "RELIABILITY_RELIABLE", "number": 1},
                    "recovered": {"name": "RELIABILITY_RECOVERED", "number": 2},
                },
            },
        },
    )
    continuation: DiagnosticContinuation = Field(
        description="Whether the finding is an artefact of source that stops mid-way, which is the normal state of a file an agent is halfway through writing.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "repairable": "Appending source can make it go away: an unclosed brace, a statement cut off "
                "at the end of the file.",
                "unrepairable": "It stands whatever follows.",
                "unknown": "The compiler does not say.",
            },
            "rift:proto": {
                "field": "continuation",
                "number": 8,
                "enum": "Continuation",
                "values": {
                    "repairable": {"name": "CONTINUATION_REPAIRABLE", "number": 1},
                    "unrepairable": {"name": "CONTINUATION_UNREPAIRABLE", "number": 2},
                    "unknown": {"name": "CONTINUATION_UNKNOWN", "number": 3},
                },
            },
        },
    )
    extensions: Extensions = Field(
        description="Diagnostic fields the model has no place for, namespaced by the adapter that emitted them.",
        json_schema_extra={"rift:proto": {"field": "extensions", "number": 9}},
    )
    language: LanguageId | None = Field(
        description="Which adapter produced this. Null for a finding Rift itself raised.",
        json_schema_extra={"rift:proto": {"field": "language", "number": 10}},
    )


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.ValidationReport"},
    schema_extra={},
)
class ValidationReport(ClosedModel):
    """One compiler's verdict over a candidate snapshot. `coverage` states how much the compiler checked, while `valid` records whether the checked scope contains an error. Publication requires complete coverage from every adapter that owns an affected file and a valid verdict from each of them."""

    language: LanguageId = Field(
        description="The adapter that produced this verdict.",
        json_schema_extra={"rift:proto": {"field": "language", "number": 1}},
    )
    valid: bool = Field(
        description="Whether this compiler accepted every file in `files` under the reported coverage.",
        json_schema_extra={"rift:proto": {"field": "valid", "number": 2}},
    )
    coverage: Coverage = Field(
        description="How much of the affected program the compiler checked. Publication refuses partial or unsupported coverage where the change depends on that language.",
        json_schema_extra={"rift:proto": {"field": "coverage", "number": 3}},
    )
    files: list[ProjectPath] = Field(
        description="Affected files this verdict covers, sorted by project path.",
        json_schema_extra={
            "rift:proto": {"field": "files", "number": 4},
            "uniqueItems": True,
        },
    )
    diagnostics: list[Diagnostic] = Field(
        description="Compiler findings produced while checking the candidate, sorted by file and source range.",
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 5}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.FormattingPolicy",
        "enum": "FormattingPolicy",
        "values": {
            "preserve": {"name": "FORMATTING_POLICY_PRESERVE", "number": 1},
            "changed_regions": {
                "name": "FORMATTING_POLICY_CHANGED_REGIONS",
                "number": 2,
            },
            "affected_files": {"name": "FORMATTING_POLICY_AFFECTED_FILES", "number": 3},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "preserve": "Keep every byte outside the resolved edits unchanged.",
            "changed_regions": "Run the language formatter over the smallest syntactic regions "
            "containing the resolved edits.",
            "affected_files": "Run the language formatter over every text file touched by the change.",
        }
    },
)
class FormattingPolicy(str, Enum):
    """How formatting participates in a change. The policy is part of the preview identity, so refresh and publish cannot widen its reach."""

    PRESERVE = "preserve"
    CHANGED_REGIONS = "changed_regions"
    AFFECTED_FILES = "affected_files"


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.MatchCardinality"},
    schema_extra={},
)
class MatchCardinality(ClosedModel):
    """How many matches an atomic rewrite accepts. Rift counts against the candidate state at that point in the ordered change list and refuses the whole preview when the count falls outside this interval."""

    minimum: int = Field(
        description="Fewest matches required.",
        ge=0,
        le=100000,
        json_schema_extra={"rift:proto": {"field": "minimum", "number": 1}},
    )
    maximum: int | None = Field(
        description="Most matches accepted. Null uses the workspace's `max_rewrite_expansions` limit.",
        json_schema_extra={"rift:proto": {"field": "maximum", "number": 2}},
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "tests",
        "number": 2,
        "enum": "Tests",
        "values": {
            "exclude": {"name": "TESTS_EXCLUDE", "number": 1},
            "include": {"name": "TESTS_INCLUDE", "number": 2},
            "only": {"name": "TESTS_ONLY", "number": 3},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "exclude": "Do not inspect or change test source.",
            "include": "Treat production and test source as one action scope.",
            "only": "Confine the action to test source.",
        }
    },
)
class OperationScopeTests(str, Enum):
    """How source carrying the `test` leaf facet participates."""

    EXCLUDE = "exclude"
    INCLUDE = "include"
    ONLY = "only"


@definition(
    owner="core",
    public=False,
    proto={
        "field": "generated",
        "number": 3,
        "enum": "Generated",
        "values": {
            "exclude": {"name": "GENERATED_EXCLUDE", "number": 1},
            "include": {"name": "GENERATED_INCLUDE", "number": 2},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "exclude": "Refuse an action whose complete effect requires generated source.",
            "include": "Permit generated source and require a `generated_code` confirmation.",
        }
    },
)
class OperationScopeGenerated(str, Enum):
    """How source carrying the `generated` leaf facet participates."""

    EXCLUDE = "exclude"
    INCLUDE = "include"


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.OperationScope"},
    schema_extra={},
)
class OperationScope(ClosedModel):
    """The project source an action may inspect or change. Dependencies remain readable compiler input and are never editable through this scope."""

    paths: PathSelector = Field(
        description="Project paths eligible for the action. Every resolved edit must remain inside this selector.",
        json_schema_extra={"rift:proto": {"field": "paths", "number": 1}},
    )
    tests: OperationScopeTests = Field(
        description="How source carrying the `test` leaf facet participates.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "exclude": "Do not inspect or change test source.",
                "include": "Treat production and test source as one action scope.",
                "only": "Confine the action to test source.",
            },
            "rift:proto": {
                "field": "tests",
                "number": 2,
                "enum": "Tests",
                "values": {
                    "exclude": {"name": "TESTS_EXCLUDE", "number": 1},
                    "include": {"name": "TESTS_INCLUDE", "number": 2},
                    "only": {"name": "TESTS_ONLY", "number": 3},
                },
            },
        },
    )
    generated: OperationScopeGenerated = Field(
        description="How source carrying the `generated` leaf facet participates.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "exclude": "Refuse an action whose complete effect requires generated source.",
                "include": "Permit generated source and require a `generated_code` confirmation.",
            },
            "rift:proto": {
                "field": "generated",
                "number": 3,
                "enum": "Generated",
                "values": {
                    "exclude": {"name": "GENERATED_EXCLUDE", "number": 1},
                    "include": {"name": "GENERATED_INCLUDE", "number": 2},
                },
            },
        },
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "reads",
        "number": 1,
        "enum": "Reads",
        "values": {
            "refuse": {"name": "READS_REFUSE", "number": 1},
            "rewrite": {"name": "READS_REWRITE", "number": 2},
            "remove": {"name": "READS_REMOVE", "number": 3},
        },
    },
    schema_extra={},
)
class SafeDeletePolicyReads(str, Enum):
    """Disposition for reads, calls, and constructions."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner="core",
    public=False,
    proto={
        "field": "writes",
        "number": 2,
        "enum": "Writes",
        "values": {
            "refuse": {"name": "WRITES_REFUSE", "number": 1},
            "rewrite": {"name": "WRITES_REWRITE", "number": 2},
            "remove": {"name": "WRITES_REMOVE", "number": 3},
        },
    },
    schema_extra={},
)
class SafeDeletePolicyWrites(str, Enum):
    """Disposition for assignments and other writes."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner="core",
    public=False,
    proto={
        "field": "imports",
        "number": 3,
        "enum": "Imports",
        "values": {
            "refuse": {"name": "IMPORTS_REFUSE", "number": 1},
            "rewrite": {"name": "IMPORTS_REWRITE", "number": 2},
            "remove": {"name": "IMPORTS_REMOVE", "number": 3},
        },
    },
    schema_extra={},
)
class SafeDeletePolicyImports(str, Enum):
    """Disposition for imports, exports, aliases, and re-exports."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner="core",
    public=False,
    proto={
        "field": "overrides",
        "number": 4,
        "enum": "Overrides",
        "values": {
            "refuse": {"name": "OVERRIDES_REFUSE", "number": 1},
            "rewrite": {"name": "OVERRIDES_REWRITE", "number": 2},
            "remove": {"name": "OVERRIDES_REMOVE", "number": 3},
        },
    },
    schema_extra={},
)
class SafeDeletePolicyOverrides(str, Enum):
    """Disposition for overrides, implementations, and inherited declarations."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner="core",
    public=False,
    proto={
        "field": "generated",
        "number": 5,
        "enum": "Generated",
        "values": {
            "refuse": {"name": "GENERATED_REFUSE", "number": 1},
            "rewrite": {"name": "GENERATED_REWRITE", "number": 2},
            "remove": {"name": "GENERATED_REMOVE", "number": 3},
        },
    },
    schema_extra={},
)
class SafeDeletePolicyGenerated(str, Enum):
    """Disposition for compiler-resolved uses in generated source. Scope must also permit generated files."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner="core",
    public=False,
    proto={
        "field": "strings",
        "number": 6,
        "enum": "Strings",
        "values": {
            "refuse": {"name": "STRINGS_REFUSE", "number": 1},
            "rewrite": {"name": "STRINGS_REWRITE", "number": 2},
            "remove": {"name": "STRINGS_REMOVE", "number": 3},
        },
    },
    schema_extra={},
)
class SafeDeletePolicyStrings(str, Enum):
    """Disposition for compiler-classified string references such as reflection names. Unclassified strings are outside the reference set and cannot be claimed by a guarantee."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner="core",
    public=False,
    proto={
        "field": "other",
        "number": 7,
        "enum": "Other",
        "values": {
            "refuse": {"name": "OTHER_REFUSE", "number": 1},
            "rewrite": {"name": "OTHER_REWRITE", "number": 2},
            "remove": {"name": "OTHER_REMOVE", "number": 3},
        },
    },
    schema_extra={},
)
class SafeDeletePolicyOther(str, Enum):
    """Disposition for complete compiler-resolved uses not covered above."""

    REFUSE = "refuse"
    REWRITE = "rewrite"
    REMOVE = "remove"


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.SafeDeletePolicy"},
    schema_extra={},
)
class SafeDeletePolicy(ClosedModel):
    """How a compiler may eliminate each remaining use while deleting a declaration. `refuse` stops when that use exists, `rewrite` replaces it with a compiler-selected valid form, and `remove` deletes its containing construct at a compiler-selected safe boundary. Complete reference coverage is mandatory; any unresolved use refuses `refactor.safe_delete` before a candidate is built."""

    reads: SafeDeletePolicyReads = Field(
        description="Disposition for reads, calls, and constructions.",
        json_schema_extra={
            "rift:proto": {
                "field": "reads",
                "number": 1,
                "enum": "Reads",
                "values": {
                    "refuse": {"name": "READS_REFUSE", "number": 1},
                    "rewrite": {"name": "READS_REWRITE", "number": 2},
                    "remove": {"name": "READS_REMOVE", "number": 3},
                },
            }
        },
    )
    writes: SafeDeletePolicyWrites = Field(
        description="Disposition for assignments and other writes.",
        json_schema_extra={
            "rift:proto": {
                "field": "writes",
                "number": 2,
                "enum": "Writes",
                "values": {
                    "refuse": {"name": "WRITES_REFUSE", "number": 1},
                    "rewrite": {"name": "WRITES_REWRITE", "number": 2},
                    "remove": {"name": "WRITES_REMOVE", "number": 3},
                },
            }
        },
    )
    imports: SafeDeletePolicyImports = Field(
        description="Disposition for imports, exports, aliases, and re-exports.",
        json_schema_extra={
            "rift:proto": {
                "field": "imports",
                "number": 3,
                "enum": "Imports",
                "values": {
                    "refuse": {"name": "IMPORTS_REFUSE", "number": 1},
                    "rewrite": {"name": "IMPORTS_REWRITE", "number": 2},
                    "remove": {"name": "IMPORTS_REMOVE", "number": 3},
                },
            }
        },
    )
    overrides: SafeDeletePolicyOverrides = Field(
        description="Disposition for overrides, implementations, and inherited declarations.",
        json_schema_extra={
            "rift:proto": {
                "field": "overrides",
                "number": 4,
                "enum": "Overrides",
                "values": {
                    "refuse": {"name": "OVERRIDES_REFUSE", "number": 1},
                    "rewrite": {"name": "OVERRIDES_REWRITE", "number": 2},
                    "remove": {"name": "OVERRIDES_REMOVE", "number": 3},
                },
            }
        },
    )
    generated: SafeDeletePolicyGenerated = Field(
        description="Disposition for compiler-resolved uses in generated source. Scope must also permit generated files.",
        json_schema_extra={
            "rift:proto": {
                "field": "generated",
                "number": 5,
                "enum": "Generated",
                "values": {
                    "refuse": {"name": "GENERATED_REFUSE", "number": 1},
                    "rewrite": {"name": "GENERATED_REWRITE", "number": 2},
                    "remove": {"name": "GENERATED_REMOVE", "number": 3},
                },
            }
        },
    )
    strings: SafeDeletePolicyStrings = Field(
        description="Disposition for compiler-classified string references such as reflection names. Unclassified strings are outside the reference set and cannot be claimed by a guarantee.",
        json_schema_extra={
            "rift:proto": {
                "field": "strings",
                "number": 6,
                "enum": "Strings",
                "values": {
                    "refuse": {"name": "STRINGS_REFUSE", "number": 1},
                    "rewrite": {"name": "STRINGS_REWRITE", "number": 2},
                    "remove": {"name": "STRINGS_REMOVE", "number": 3},
                },
            }
        },
    )
    other: SafeDeletePolicyOther = Field(
        description="Disposition for complete compiler-resolved uses not covered above.",
        json_schema_extra={
            "rift:proto": {
                "field": "other",
                "number": 7,
                "enum": "Other",
                "values": {
                    "refuse": {"name": "OTHER_REFUSE", "number": 1},
                    "rewrite": {"name": "OTHER_REWRITE", "number": 2},
                    "remove": {"name": "OTHER_REMOVE", "number": 3},
                },
            }
        },
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "propagation",
        "number": 7,
        "enum": "Propagation",
        "values": {
            "declaration": {"name": "DECLARATION", "number": 1},
            "callers": {"name": "CALLERS", "number": 2},
            "overrides": {"name": "OVERRIDES", "number": 3},
            "all": {"name": "ALL", "number": 4},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "declaration": "Change only the selected declaration and refuse when existing uses would "
            "become invalid.",
            "callers": "Update the declaration and every resolved call site.",
            "overrides": "Update the declaration and its complete override or implementation family.",
            "all": "Update the declaration, callers, overrides, and implementations.",
        }
    },
)
class SignatureChangePropagation(str, Enum):
    """Declarations and calls the compiler must update with the signature."""

    DECLARATION = "declaration"
    CALLERS = "callers"
    OVERRIDES = "overrides"
    ALL = "all"


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.SignatureChange"},
    schema_extra={},
)
class SignatureChange(ClosedModel):
    """The desired callable shape for a structured signature action. Existing parameters retain identity through `Parameter.leaf`; a parameter without a leaf is new. Language constructs outside this shape travel in `extensions` under an advertised versioned schema."""

    receiver: Parameter | None = Field(
        description="Desired implicit receiver, or null for a free callable.",
        json_schema_extra={"rift:proto": {"field": "receiver", "number": 1}},
    )
    parameters: list[Parameter] = Field(
        description="Desired parameters in declaration order. Reordering existing leaves preserves their identity while changing their position.",
        json_schema_extra={"rift:proto": {"field": "parameters", "number": 2}},
    )
    returns: list[TypeBinding] = Field(
        description="Desired declared return bindings.",
        json_schema_extra={"rift:proto": {"field": "returns", "number": 3}},
    )
    type_parameters: list[SymbolId] = Field(
        description="Desired existing generic parameters in order. Creating a language-specific generic parameter uses `extensions`.",
        json_schema_extra={"rift:proto": {"field": "type_parameters", "number": 4}},
    )
    throws: list[TypeExpression] = Field(
        description="Desired declared error or exception types.",
        json_schema_extra={"rift:proto": {"field": "throws", "number": 5}},
    )
    effects: list[str] = Field(
        description="Desired language effect keywords, in the adapter's spelling.",
        json_schema_extra={
            "rift:proto": {"field": "effects", "number": 6},
            "uniqueItems": True,
        },
    )
    propagation: SignatureChangePropagation = Field(
        description="Declarations and calls the compiler must update with the signature.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "declaration": "Change only the selected declaration and refuse when existing uses would "
                "become invalid.",
                "callers": "Update the declaration and every resolved call site.",
                "overrides": "Update the declaration and its complete override or implementation family.",
                "all": "Update the declaration, callers, overrides, and implementations.",
            },
            "rift:proto": {
                "field": "propagation",
                "number": 7,
                "enum": "Propagation",
                "values": {
                    "declaration": {"name": "DECLARATION", "number": 1},
                    "callers": {"name": "CALLERS", "number": 2},
                    "overrides": {"name": "OVERRIDES", "number": 3},
                    "all": {"name": "ALL", "number": 4},
                },
            },
        },
    )
    extensions: Extensions = Field(
        description="Versioned language-specific signature fields.",
        json_schema_extra={"rift:proto": {"field": "extensions", "number": 8}},
    )


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.RenameArguments"},
    schema_extra={},
)
class RenameArguments(ClosedModel):
    """Portable arguments for `refactor.rename` actions."""

    name: str = Field(
        description="New source name. The compiler checks language spelling, collisions, visibility, and binding changes.",
        min_length=1,
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "name", "number": 1}},
    )
    scope: OperationScope = Field(
        description="Source eligible for propagation. Complete references outside it cause refusal.",
        json_schema_extra={"rift:proto": {"field": "scope", "number": 2}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.MoveArgumentsDestinationAddress"},
    schema_extra={},
)
class MoveArgumentsDestinationAddress(ClosedModel):
    kind: Literal["address"] = Field()
    address: Address = Field(
        description="Existing semantic container that receives the moved declaration.",
        json_schema_extra={"rift:proto": {"field": "address", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.MoveArgumentsDestinationPath"},
    schema_extra={},
)
class MoveArgumentsDestinationPath(ClosedModel):
    kind: Literal["path"] = Field()
    path: ProjectPath = Field(
        description="Project path that receives a file, module, or declaration supported by the language.",
        json_schema_extra={"rift:proto": {"field": "path", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "destination",
        "number": 1,
        "type": "rift.core.MoveArgumentsDestination",
        "oneof": "variant",
        "variants": [
            {
                "tag": "address",
                "field": "address",
                "number": 1,
                "type": "MoveArgumentsDestinationAddress",
            },
            {
                "tag": "path",
                "field": "path",
                "number": 2,
                "type": "MoveArgumentsDestinationPath",
            },
        ],
    },
    schema_extra={},
)
class MoveArgumentsDestination(
    ProtocolRoot[
        "Annotated[MoveArgumentsDestinationAddress | MoveArgumentsDestinationPath, Field(discriminator='kind')]"
    ]
):
    """Existing destination container or project path. Symbols use an Address; files and new modules use a ProjectPath."""


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.MoveArguments"},
    schema_extra={},
)
class MoveArguments(ClosedModel):
    """Portable arguments for `refactor.move` actions."""

    destination: MoveArgumentsDestination = Field(
        description="Existing destination container or project path. Symbols use an Address; files and new modules use a ProjectPath.",
        json_schema_extra={
            "rift:proto": {
                "field": "destination",
                "number": 1,
                "type": "rift.core.MoveArgumentsDestination",
                "oneof": "variant",
                "variants": [
                    {
                        "tag": "address",
                        "field": "address",
                        "number": 1,
                        "type": "MoveArgumentsDestinationAddress",
                    },
                    {
                        "tag": "path",
                        "field": "path",
                        "number": 2,
                        "type": "MoveArgumentsDestinationPath",
                    },
                ],
            }
        },
    )
    scope: OperationScope = Field(
        description="Source eligible for import and reference updates.",
        json_schema_extra={"rift:proto": {"field": "scope", "number": 2}},
    )


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.SafeDeleteArguments"},
    schema_extra={},
)
class SafeDeleteArguments(ClosedModel):
    """Portable arguments for `refactor.safe_delete` actions."""

    policy: SafeDeletePolicy = Field(
        description="Required behavior for every classified remaining use.",
        json_schema_extra={"rift:proto": {"field": "policy", "number": 1}},
    )
    scope: OperationScope = Field(
        description="Source in which complete usage analysis and any requested rewrite may occur.",
        json_schema_extra={"rift:proto": {"field": "scope", "number": 2}},
    )


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.ChangeSignatureArguments"},
    schema_extra={},
)
class ChangeSignatureArguments(ClosedModel):
    """Portable arguments for `refactor.change_signature` actions."""

    signature: SignatureChange = Field(
        description="Desired callable structure and propagation policy.",
        json_schema_extra={"rift:proto": {"field": "signature", "number": 1}},
    )
    scope: OperationScope = Field(
        description="Source eligible for caller and override propagation.",
        json_schema_extra={"rift:proto": {"field": "scope", "number": 2}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.ArgumentContractRename"},
    schema_extra={"rift:arguments": "RenameArguments"},
)
class ArgumentContractRename(ClosedModel):
    """Arguments conform to RenameArguments at the stated contract version."""

    name: Literal["rename"] = Field()
    version: Literal[1] = Field(
        json_schema_extra={"rift:proto": {"field": "version", "number": 1}}
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.ArgumentContractMove"},
    schema_extra={"rift:arguments": "MoveArguments"},
)
class ArgumentContractMove(ClosedModel):
    """Arguments conform to MoveArguments at the stated contract version."""

    name: Literal["move"] = Field()
    version: Literal[1] = Field(
        json_schema_extra={"rift:proto": {"field": "version", "number": 1}}
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.ArgumentContractSafeDelete"},
    schema_extra={"rift:arguments": "SafeDeleteArguments"},
)
class ArgumentContractSafeDelete(ClosedModel):
    """Arguments conform to SafeDeleteArguments at the stated contract version."""

    name: Literal["safe_delete"] = Field()
    version: Literal[1] = Field(
        json_schema_extra={"rift:proto": {"field": "version", "number": 1}}
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.ArgumentContractChangeSignature"},
    schema_extra={"rift:arguments": "ChangeSignatureArguments"},
)
class ArgumentContractChangeSignature(ClosedModel):
    """Arguments conform to ChangeSignatureArguments at the stated contract version."""

    name: Literal["change_signature"] = Field()
    version: Literal[1] = Field(
        json_schema_extra={"rift:proto": {"field": "version", "number": 1}}
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.ArgumentContract",
        "oneof": "variant",
        "variants": [
            {
                "tag": "rename",
                "field": "rename",
                "number": 1,
                "type": "ArgumentContractRename",
            },
            {
                "tag": "move",
                "field": "move",
                "number": 2,
                "type": "ArgumentContractMove",
            },
            {
                "tag": "safe_delete",
                "field": "safe_delete",
                "number": 3,
                "type": "ArgumentContractSafeDelete",
            },
            {
                "tag": "change_signature",
                "field": "change_signature",
                "number": 4,
                "type": "ArgumentContractChangeSignature",
            },
        ],
    },
    schema_extra={},
)
class ArgumentContract(
    ProtocolRoot[
        "Annotated[ArgumentContractRename | ArgumentContractMove | ArgumentContractSafeDelete | ArgumentContractChangeSignature, Field(discriminator='name')]"
    ]
):
    """A portable argument contract attached to a discovered compiler action. Its stable name and numeric version are separate fields. The descriptor's self-contained schema may narrow the selected contract for one language without changing its field meanings."""


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.OperationVerifierAdapter"},
    schema_extra={},
)
class OperationVerifierAdapter(ClosedModel):
    """A language adapter checked compiler-owned state."""

    kind: Literal["adapter"] = Field()
    language: LanguageId = Field(
        description="Adapter whose compiler performed the check.",
        json_schema_extra={"rift:proto": {"field": "language", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.OperationVerifierValidator"},
    schema_extra={},
)
class OperationVerifierValidator(ClosedModel):
    """One caller-supplied sandbox validator checked the candidate."""

    kind: Literal["validator"] = Field()
    validator: int = Field(
        description="Zero-based position in the preview's validator declarations.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "validator", "number": 1}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.OperationVerifier",
        "oneof": "variant",
        "variants": [
            {
                "tag": "rift",
                "field": "rift",
                "number": 1,
                "type": "google.protobuf.Empty",
            },
            {
                "tag": "adapter",
                "field": "adapter",
                "number": 2,
                "type": "OperationVerifierAdapter",
            },
            {
                "tag": "validator",
                "field": "validator",
                "number": 3,
                "type": "OperationVerifierValidator",
            },
        ],
    },
    schema_extra={},
)
class OperationVerifier(
    ProtocolRoot[
        "Annotated[Empty | OperationVerifierAdapter | OperationVerifierValidator, Field(discriminator='kind')]"
    ]
):
    """The component that checked a precondition or established a guarantee."""


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.PreconditionValueBoolean"},
    schema_extra={},
)
class PreconditionValueBoolean(ClosedModel):
    """Boolean property such as target existence or writability."""

    kind: Literal["boolean"] = Field()
    value: bool = Field(
        json_schema_extra={"rift:proto": {"field": "value", "number": 1}}
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.PreconditionValueCount"},
    schema_extra={},
)
class PreconditionValueCount(ClosedModel):
    """Non-negative count such as remaining usages."""

    kind: Literal["count"] = Field()
    value: int = Field(
        ge=0,
        le=9007199254740991,
        json_schema_extra={"rift:proto": {"field": "value", "number": 1}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.PreconditionValueSnapshot"},
    schema_extra={},
)
class PreconditionValueSnapshot(ClosedModel):
    """Pinned project state used by stale-state checks."""

    kind: Literal["snapshot"] = Field()
    value: Snapshot = Field(
        json_schema_extra={"rift:proto": {"field": "value", "number": 1}}
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.PreconditionValueCoverage"},
    schema_extra={},
)
class PreconditionValueCoverage(ClosedModel):
    """Coverage required or observed for a compiler fact family."""

    kind: Literal["coverage"] = Field()
    value: Coverage = Field(
        json_schema_extra={"rift:proto": {"field": "value", "number": 1}}
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.PreconditionValueText"},
    schema_extra={},
)
class PreconditionValueText(ClosedModel):
    """Language or policy value whose spelling is itself significant."""

    kind: Literal["text"] = Field()
    value: str = Field(
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "value", "number": 1}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.PreconditionValue",
        "oneof": "variant",
        "variants": [
            {
                "tag": "boolean",
                "field": "boolean",
                "number": 1,
                "type": "PreconditionValueBoolean",
            },
            {
                "tag": "count",
                "field": "count",
                "number": 2,
                "type": "PreconditionValueCount",
            },
            {
                "tag": "snapshot",
                "field": "snapshot",
                "number": 3,
                "type": "PreconditionValueSnapshot",
            },
            {
                "tag": "coverage",
                "field": "coverage",
                "number": 4,
                "type": "PreconditionValueCoverage",
            },
            {
                "tag": "text",
                "field": "text",
                "number": 5,
                "type": "PreconditionValueText",
            },
        ],
    },
    schema_extra={},
)
class PreconditionValue(
    ProtocolRoot[
        "Annotated[PreconditionValueBoolean | PreconditionValueCount | PreconditionValueSnapshot | PreconditionValueCoverage | PreconditionValueText, Field(discriminator='kind')]"
    ]
):
    """A typed value compared by an operation precondition. The tag prevents a snapshot string, a name, and a count rendered as text from becoming indistinguishable."""


@definition(
    owner="core",
    public=False,
    proto={
        "field": "kind",
        "number": 1,
        "enum": "Kind",
        "values": {
            "target_exists": {"name": "KIND_TARGET_EXISTS", "number": 1},
            "state_matches": {"name": "KIND_STATE_MATCHES", "number": 2},
            "writable": {"name": "KIND_WRITABLE", "number": 3},
            "references_complete": {"name": "KIND_REFERENCES_COMPLETE", "number": 4},
            "no_remaining_usages": {"name": "KIND_NO_REMAINING_USAGES", "number": 5},
            "destination_legal": {"name": "KIND_DESTINATION_LEGAL", "number": 6},
            "name_available": {"name": "KIND_NAME_AVAILABLE", "number": 7},
            "language_condition": {"name": "KIND_LANGUAGE_CONDITION", "number": 8},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "target_exists": "Every addressed symbol, leaf, match, or source range resolves at the "
            "candidate state.",
            "state_matches": "The state used for discovery still matches the state being resolved or "
            "published.",
            "writable": "Every affected path is inside the project, owned by Rift, and permitted by "
            "host policy.",
            "references_complete": "The compiler completely classified the references on which the "
            "operation depends.",
            "no_remaining_usages": "No usage remains after the selected safe-delete policy is applied.",
            "destination_legal": "The requested path or semantic container can receive the moved or "
            "created entity.",
            "name_available": "The requested name creates no forbidden collision or binding change.",
            "language_condition": "A compiler-defined condition described by diagnostics and extension "
            "data.",
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
    owner="core",
    public=False,
    proto={
        "field": "status",
        "number": 2,
        "enum": "Status",
        "values": {
            "satisfied": {"name": "STATUS_SATISFIED", "number": 1},
            "failed": {"name": "STATUS_FAILED", "number": 2},
        },
    },
    schema_extra={},
)
class OperationPreconditionStatus(str, Enum):
    """Result of this check."""

    SATISFIED = "satisfied"
    FAILED = "failed"


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.OperationPrecondition"},
    schema_extra={},
)
class OperationPrecondition(ClosedModel):
    """One executable condition checked during resolution and rechecked during publication. Expected and observed values carry explicit value tags."""

    kind: OperationPreconditionKind = Field(
        description="Condition being checked.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "target_exists": "Every addressed symbol, leaf, match, or source range resolves at the "
                "candidate state.",
                "state_matches": "The state used for discovery still matches the state being resolved or "
                "published.",
                "writable": "Every affected path is inside the project, owned by Rift, and permitted by "
                "host policy.",
                "references_complete": "The compiler completely classified the references on which the "
                "operation depends.",
                "no_remaining_usages": "No usage remains after the selected safe-delete policy is applied.",
                "destination_legal": "The requested path or semantic container can receive the moved or "
                "created entity.",
                "name_available": "The requested name creates no forbidden collision or binding change.",
                "language_condition": "A compiler-defined condition described by diagnostics and extension "
                "data.",
            },
            "rift:proto": {
                "field": "kind",
                "number": 1,
                "enum": "Kind",
                "values": {
                    "target_exists": {"name": "KIND_TARGET_EXISTS", "number": 1},
                    "state_matches": {"name": "KIND_STATE_MATCHES", "number": 2},
                    "writable": {"name": "KIND_WRITABLE", "number": 3},
                    "references_complete": {
                        "name": "KIND_REFERENCES_COMPLETE",
                        "number": 4,
                    },
                    "no_remaining_usages": {
                        "name": "KIND_NO_REMAINING_USAGES",
                        "number": 5,
                    },
                    "destination_legal": {
                        "name": "KIND_DESTINATION_LEGAL",
                        "number": 6,
                    },
                    "name_available": {"name": "KIND_NAME_AVAILABLE", "number": 7},
                    "language_condition": {
                        "name": "KIND_LANGUAGE_CONDITION",
                        "number": 8,
                    },
                },
            },
        },
    )
    status: OperationPreconditionStatus = Field(
        description="Result of this check.",
        json_schema_extra={
            "rift:proto": {
                "field": "status",
                "number": 2,
                "enum": "Status",
                "values": {
                    "satisfied": {"name": "STATUS_SATISFIED", "number": 1},
                    "failed": {"name": "STATUS_FAILED", "number": 2},
                },
            }
        },
    )
    verifier: OperationVerifier = Field(
        description="Component that performed the check.",
        json_schema_extra={"rift:proto": {"field": "verifier", "number": 3}},
    )
    addresses: list[Address] = Field(
        description="Existing semantic or source subjects involved in the condition.",
        json_schema_extra={"rift:proto": {"field": "addresses", "number": 4}},
    )
    paths: list[ProjectPath] = Field(
        description="Project paths involved in the condition, including destinations that do not yet exist.",
        json_schema_extra={
            "rift:proto": {"field": "paths", "number": 5},
            "uniqueItems": True,
        },
    )
    expected: PreconditionValue = Field(
        description="Required value. Examples include the discovered snapshot, zero remaining usages, or complete coverage.",
        json_schema_extra={"rift:proto": {"field": "expected", "number": 6}},
    )
    observed: PreconditionValue = Field(
        description="Value found while checking the condition.",
        json_schema_extra={"rift:proto": {"field": "observed", "number": 7}},
    )
    diagnostics: list[Diagnostic] = Field(
        description="Compiler or Rift findings that explain the observed value.",
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 8}},
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.OperationBlockerAddress"},
    schema_extra={},
)
class OperationBlockerAddress(ClosedModel):
    """Existing semantic or source target."""

    kind: Literal["address"] = Field()
    address: Address = Field(
        json_schema_extra={"rift:proto": {"field": "address", "number": 1}}
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.OperationBlockerPath"},
    schema_extra={},
)
class OperationBlockerPath(ClosedModel):
    """Project path involved in a collision, ownership refusal, or illegal destination."""

    kind: Literal["path"] = Field()
    path: ProjectPath = Field(
        json_schema_extra={"rift:proto": {"field": "path", "number": 1}}
    )


@definition(
    owner="core",
    public=False,
    proto={"type": "rift.core.OperationBlockerRelationship"},
    schema_extra={},
)
class OperationBlockerRelationship(ClosedModel):
    """Compiler relationship the operation could not preserve or rewrite."""

    kind: Literal["relationship"] = Field()
    relationship: Relationship = Field(
        json_schema_extra={"rift:proto": {"field": "relationship", "number": 1}}
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.OperationBlocker",
        "oneof": "variant",
        "variants": [
            {
                "tag": "address",
                "field": "address",
                "number": 1,
                "type": "OperationBlockerAddress",
            },
            {
                "tag": "path",
                "field": "path",
                "number": 2,
                "type": "OperationBlockerPath",
            },
            {
                "tag": "relationship",
                "field": "relationship",
                "number": 3,
                "type": "OperationBlockerRelationship",
            },
        ],
    },
    schema_extra={},
)
class OperationBlocker(
    ProtocolRoot[
        "Annotated[OperationBlockerAddress | OperationBlockerPath | OperationBlockerRelationship, Field(discriminator='kind')]"
    ]
):
    """A concrete subject preventing resolution. The union admits existing code, a path that may not exist, or the compiler edge that cannot be changed safely."""


@definition(
    owner="core",
    public=False,
    proto={
        "field": "kind",
        "number": 1,
        "enum": "Kind",
        "values": {
            "declaration_created": {"name": "DECLARATION_CREATED", "number": 1},
            "declaration_removed": {"name": "DECLARATION_REMOVED", "number": 2},
            "declaration_moved": {"name": "DECLARATION_MOVED", "number": 3},
            "declaration_changed": {"name": "DECLARATION_CHANGED", "number": 4},
            "references_updated": {"name": "REFERENCES_UPDATED", "number": 5},
            "imports_updated": {"name": "IMPORTS_UPDATED", "number": 6},
            "source_rewritten": {"name": "SOURCE_REWRITTEN", "number": 7},
            "formatting_applied": {"name": "FORMATTING_APPLIED", "number": 8},
        },
    },
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


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.OperationEffect"},
    schema_extra={},
)
class OperationEffect(ClosedModel):
    """One semantic consequence of a resolved change. Exact bytes remain in `Edit`; this record explains what those bytes did to declarations and resolved relationships."""

    kind: OperationEffectKind = Field(
        description="Portable consequence of the change.",
        json_schema_extra={
            "rift:proto": {
                "field": "kind",
                "number": 1,
                "enum": "Kind",
                "values": {
                    "declaration_created": {"name": "DECLARATION_CREATED", "number": 1},
                    "declaration_removed": {"name": "DECLARATION_REMOVED", "number": 2},
                    "declaration_moved": {"name": "DECLARATION_MOVED", "number": 3},
                    "declaration_changed": {"name": "DECLARATION_CHANGED", "number": 4},
                    "references_updated": {"name": "REFERENCES_UPDATED", "number": 5},
                    "imports_updated": {"name": "IMPORTS_UPDATED", "number": 6},
                    "source_rewritten": {"name": "SOURCE_REWRITTEN", "number": 7},
                    "formatting_applied": {"name": "FORMATTING_APPLIED", "number": 8},
                },
            }
        },
    )
    before: list[Address] = Field(
        description="Subjects in the state before this change. Empty for creation.",
        json_schema_extra={"rift:proto": {"field": "before", "number": 2}},
    )
    after: list[Address] = Field(
        description="Subjects after Rift applies the change and every adapter sharing the worktree acknowledges the new snapshot. Empty for deletion.",
        json_schema_extra={"rift:proto": {"field": "after", "number": 3}},
    )
    spans: list[SourceSpan] = Field(
        description="Source locations demonstrating the effect, pinned to their respective before or after address state.",
        json_schema_extra={"rift:proto": {"field": "spans", "number": 4}},
    )
    detail: str = Field(
        description="Concrete account of the semantic consequence.",
        min_length=1,
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "detail", "number": 5}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.GuaranteeKind",
        "enum": "GuaranteeKind",
        "values": {
            "syntax_validated": {
                "name": "GUARANTEE_KIND_SYNTAX_VALIDATED",
                "number": 1,
            },
            "bindings_preserved": {
                "name": "GUARANTEE_KIND_BINDINGS_PRESERVED",
                "number": 2,
            },
            "references_updated": {
                "name": "GUARANTEE_KIND_REFERENCES_UPDATED",
                "number": 3,
            },
            "behavior_checked": {
                "name": "GUARANTEE_KIND_BEHAVIOR_CHECKED",
                "number": 4,
            },
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "syntax_validated": "The language parser accepts the affected source under the stated "
            "scope.",
            "bindings_preserved": "Unchanged references in scope resolve to the same symbols before and "
            "after the action.",
            "references_updated": "Every resolved reference in scope that should follow the operation "
            "now reaches its intended target.",
            "behavior_checked": "A named static analysis or sandbox validator checked a stated "
            "behavioral property. The evidence is limited to that named property "
            "and scope.",
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
    owner="core",
    public=False,
    proto={
        "field": "method",
        "number": 4,
        "enum": "Method",
        "values": {
            "construction": {"name": "CONSTRUCTION", "number": 1},
            "compiler": {"name": "COMPILER", "number": 2},
            "static_analysis": {"name": "STATIC_ANALYSIS", "number": 3},
            "sandbox": {"name": "SANDBOX", "number": 4},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "construction": "Rift established the property directly from its closed edit and "
            "transaction rules.",
            "compiler": "The language compiler or resolver checked the transformed program.",
            "static_analysis": "A named language analysis checked a property narrower than compilation.",
            "sandbox": "A caller-supplied validator checked the candidate in the execution sandbox.",
        }
    },
)
class GuaranteeEvidenceMethod(str, Enum):
    """How the property was established."""

    CONSTRUCTION = "construction"
    COMPILER = "compiler"
    STATIC_ANALYSIS = "static_analysis"
    SANDBOX = "sandbox"


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.GuaranteeEvidence"},
    schema_extra={},
)
class GuaranteeEvidence(ClosedModel):
    """Evidence establishing an action-advertised or caller-declared guarantee for a resolved change. The record supplies its scope, verifier, method, and findings."""

    kind: GuaranteeKind = Field(
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}}
    )
    scope: CoverageScope = Field(
        description="Source over which the guarantee holds.",
        json_schema_extra={"rift:proto": {"field": "scope", "number": 2}},
    )
    verifier: OperationVerifier = Field(
        description="Component that established the property.",
        json_schema_extra={"rift:proto": {"field": "verifier", "number": 3}},
    )
    method: GuaranteeEvidenceMethod = Field(
        description="How the property was established.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "construction": "Rift established the property directly from its closed edit and "
                "transaction rules.",
                "compiler": "The language compiler or resolver checked the transformed program.",
                "static_analysis": "A named language analysis checked a property narrower than compilation.",
                "sandbox": "A caller-supplied validator checked the candidate in the execution sandbox.",
            },
            "rift:proto": {
                "field": "method",
                "number": 4,
                "enum": "Method",
                "values": {
                    "construction": {"name": "CONSTRUCTION", "number": 1},
                    "compiler": {"name": "COMPILER", "number": 2},
                    "static_analysis": {"name": "STATIC_ANALYSIS", "number": 3},
                    "sandbox": {"name": "SANDBOX", "number": 4},
                },
            },
        },
    )
    detail: str = Field(
        description="Exact property checked and any limit on its interpretation.",
        min_length=1,
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "detail", "number": 5}},
    )
    diagnostics: list[Diagnostic] = Field(
        description="Findings produced while establishing the guarantee.",
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 6}},
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "kind",
        "number": 2,
        "enum": "Kind",
        "values": {
            "destructive": {"name": "DESTRUCTIVE", "number": 1},
            "large_scope": {"name": "LARGE_SCOPE", "number": 2},
            "generated_code": {"name": "GENERATED_CODE", "number": 3},
            "unresolved_reference": {"name": "UNRESOLVED_REFERENCE", "number": 4},
            "behavior_unknown": {"name": "BEHAVIOR_UNKNOWN", "number": 5},
            "formatting_scope": {"name": "FORMATTING_SCOPE", "number": 6},
            "external_validator": {"name": "EXTERNAL_VALIDATOR", "number": 7},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "destructive": "The change deletes source or replaces an existing file.",
            "large_scope": "The change reaches more files or symbols than host policy permits without "
            "acknowledgement.",
            "generated_code": "The change touches source marked as generated.",
            "unresolved_reference": "The compiler found a reference it could not classify or update.",
            "behavior_unknown": "Compiler checks establish validity but do not establish equivalent "
            "behavior.",
            "formatting_scope": "Formatting reaches outside the changed syntactic regions.",
            "external_validator": "A caller-supplied program will execute in the validation sandbox.",
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
    EXTERNAL_VALIDATOR = "external_validator"


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.ConfirmationRequirement"},
    schema_extra={},
)
class ConfirmationRequirement(ClosedModel):
    """One effect a caller must acknowledge before publication. Requirements are sorted by kind, source location, title, and detail, then numbered from zero. The acknowledgement is interpreted only with the preview named by the publish request."""

    id: int = Field(
        description="Zero-based position in the preview's ordered confirmation list.",
        ge=0,
        le=4294967295,
        json_schema_extra={"rift:proto": {"field": "id", "number": 1}},
    )
    kind: ConfirmationRequirementKind = Field(
        description="The condition that makes acknowledgement necessary.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "destructive": "The change deletes source or replaces an existing file.",
                "large_scope": "The change reaches more files or symbols than host policy permits without "
                "acknowledgement.",
                "generated_code": "The change touches source marked as generated.",
                "unresolved_reference": "The compiler found a reference it could not classify or update.",
                "behavior_unknown": "Compiler checks establish validity but do not establish equivalent "
                "behavior.",
                "formatting_scope": "Formatting reaches outside the changed syntactic regions.",
                "external_validator": "A caller-supplied program will execute in the validation sandbox.",
            },
            "rift:proto": {
                "field": "kind",
                "number": 2,
                "enum": "Kind",
                "values": {
                    "destructive": {"name": "DESTRUCTIVE", "number": 1},
                    "large_scope": {"name": "LARGE_SCOPE", "number": 2},
                    "generated_code": {"name": "GENERATED_CODE", "number": 3},
                    "unresolved_reference": {
                        "name": "UNRESOLVED_REFERENCE",
                        "number": 4,
                    },
                    "behavior_unknown": {"name": "BEHAVIOR_UNKNOWN", "number": 5},
                    "formatting_scope": {"name": "FORMATTING_SCOPE", "number": 6},
                    "external_validator": {"name": "EXTERNAL_VALIDATOR", "number": 7},
                },
            },
        },
    )
    title: str = Field(
        description="A short account of the effect being acknowledged.",
        min_length=1,
        max_length=256,
        json_schema_extra={"rift:proto": {"field": "title", "number": 3}},
    )
    detail: str = Field(
        description="The concrete consequence, including the scope or unresolved item that triggered it.",
        min_length=1,
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "detail", "number": 4}},
    )
    spans: list[SourceSpan] = Field(
        description="Source locations that demonstrate the effect. Empty where the condition applies to the candidate as a whole.",
        json_schema_extra={"rift:proto": {"field": "spans", "number": 5}},
    )


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.ActionSupport"},
    schema_extra={},
)
class ActionSupport(ClosedModel):
    """One compiler action family advertised for a language. The prefix supports planning. Discovery still decides whether the family applies at an address."""

    kind: ActionKind = Field(
        description="Supported action kind prefix, such as `refactor.rename` or `quickfix`.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 1}},
    )
    coverage: Coverage = Field(
        description="Languages, files, or constructs for which this family can be discovered and resolved.",
        json_schema_extra={"rift:proto": {"field": "coverage", "number": 2}},
    )
    guarantees: list[GuaranteeKind] = Field(
        description="Guarantee kinds this family may advertise. Each discovered action states its actual subset.",
        json_schema_extra={
            "rift:proto": {"field": "guarantees", "number": 3},
            "uniqueItems": True,
        },
    )


@definition(
    owner="core",
    public=False,
    proto={
        "field": "applicability",
        "number": 4,
        "enum": "Applicability",
        "values": {
            "always": {"name": "ALWAYS", "number": 1},
            "conditional": {"name": "CONDITIONAL", "number": 2},
            "speculative": {"name": "SPECULATIVE", "number": 3},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "always": "The compiler needs no caller-supplied fact before resolution. Preview validation "
            "and advertised guarantees still apply.",
            "conditional": "Correct only under a condition the compiler cannot establish. The condition "
            "appears in the resolved preconditions or confirmations.",
            "speculative": "The compiler inferred intent from incomplete target evidence. The caller "
            "inspects the resolved plan before publication.",
        }
    },
)
class ActionDescriptorApplicability(str, Enum):
    """How much has to be checked before applying it."""

    ALWAYS = "always"
    CONDITIONAL = "conditional"
    SPECULATIVE = "speculative"


@definition(
    owner="core",
    public=True,
    proto={"type": "rift.core.ActionDescriptor"},
    schema_extra={},
)
class ActionDescriptor(ClosedModel):
    """A compiler's portable description of one discovered operation. Listing carries no edits because resolution computes only the selected action."""

    title: str = Field(
        description="What the action is called, in the words the compiler would show a person.",
        json_schema_extra={"rift:proto": {"field": "title", "number": 1}},
    )
    kind: ActionKind = Field(
        description="What this action does. Filtering by prefix is what makes it useful: an agent that wants a fix asks for `quickfix` and never sees a refactor.",
        json_schema_extra={"rift:proto": {"field": "kind", "number": 2}},
    )
    preferred: bool = Field(
        description="Whether the compiler ranks this as the preferred action at the target. At most one action in a list is preferred.",
        json_schema_extra={"rift:proto": {"field": "preferred", "number": 3}},
    )
    applicability: ActionDescriptorApplicability = Field(
        description="How much has to be checked before applying it.",
        json_schema_extra={
            "rift:enumDescriptions": {
                "always": "The compiler needs no caller-supplied fact before resolution. Preview validation "
                "and advertised guarantees still apply.",
                "conditional": "Correct only under a condition the compiler cannot establish. The condition "
                "appears in the resolved preconditions or confirmations.",
                "speculative": "The compiler inferred intent from incomplete target evidence. The caller "
                "inspects the resolved plan before publication.",
            },
            "rift:proto": {
                "field": "applicability",
                "number": 4,
                "enum": "Applicability",
                "values": {
                    "always": {"name": "ALWAYS", "number": 1},
                    "conditional": {"name": "CONDITIONAL", "number": 2},
                    "speculative": {"name": "SPECULATIVE", "number": 3},
                },
            },
        },
    )
    rationale: str = Field(
        description="Why the compiler is offering it here.",
        json_schema_extra={"rift:proto": {"field": "rationale", "number": 5}},
    )
    target: Address = Field(
        description="What the action applies to.",
        json_schema_extra={"rift:proto": {"field": "target", "number": 6}},
    )
    argument_contract: ArgumentContract | None = Field(
        description="Portable argument shape used by a standard refactor, or null when the adapter's self-contained schema is the complete contract.",
        json_schema_extra={"rift:proto": {"field": "argument_contract", "number": 7}},
    )
    arguments_schema: dict[str, Any] = Field(
        description="JSON Schema draft 2020-12 for the arguments this action takes. Remote references are forbidden. When `argument_contract` is set, this schema may add language constraints while preserving the portable fields and meanings.",
        json_schema_extra={"rift:proto": {"field": "arguments_schema", "number": 8}},
    )
    guarantees: list[GuaranteeKind] = Field(
        description="Properties this action promises to establish when resolution succeeds. Resolve must return one complete `GuaranteeEvidence` entry for every kind listed here.",
        json_schema_extra={
            "rift:proto": {"field": "guarantees", "number": 9},
            "uniqueItems": True,
        },
    )
    disabled_reason: str | None = Field(
        description="Why the action cannot run right now, where the compiler lists it but refuses it. Null when it can run.",
        json_schema_extra={"rift:proto": {"field": "disabled_reason", "number": 10}},
    )
    diagnostics: list[Diagnostic] = Field(
        description="The findings this action would clear. Empty for an action nothing reported.",
        json_schema_extra={"rift:proto": {"field": "diagnostics", "number": 11}},
    )
    extensions: Extensions = Field(
        description="Action fields the model has no place for, namespaced by the adapter that emitted them.",
        json_schema_extra={"rift:proto": {"field": "extensions", "number": 12}},
    )


@definition(
    owner="core",
    public=True,
    proto={
        "type": "rift.core.FactFamily",
        "enum": "FactFamily",
        "values": {
            "origin_mappings": {"name": "FACT_FAMILY_ORIGIN_MAPPINGS", "number": 1},
            "symbols": {"name": "FACT_FAMILY_SYMBOLS", "number": 2},
            "leaves": {"name": "FACT_FAMILY_LEAVES", "number": 3},
            "relationships": {"name": "FACT_FAMILY_RELATIONSHIPS", "number": 4},
            "types": {"name": "FACT_FAMILY_TYPES", "number": 5},
            "diagnostics": {"name": "FACT_FAMILY_DIAGNOSTICS", "number": 6},
        },
    },
    schema_extra={
        "rift:enumDescriptions": {
            "origin_mappings": "Relations from produced source ranges to the physical or virtual source "
            "ranges that contributed them.",
            "symbols": "The declarations the compiler resolved in the file.",
            "leaves": "The nodes of the file's syntax tree, and which symbol each writes.",
            "relationships": "The edges between symbols the compiler read out of this file.",
            "types": "The types carried by those symbols. Type facts live in `Symbol.types` and "
            "`Signature`; this family records how completely the adapter resolved them.",
            "diagnostics": "What the compiler complained about while reading the file.",
        }
    },
)
class FactFamily(str, Enum):
    """One kind of thing a compiler can tell Rift about a file. Coverage, analysis streaming and invalidation all key on this, so a family is the unit in which an answer can be complete, partial or missing."""

    ORIGIN_MAPPINGS = "origin_mappings"
    SYMBOLS = "symbols"
    LEAVES = "leaves"
    RELATIONSHIPS = "relationships"
    TYPES = "types"
    DIAGNOSTICS = "diagnostics"


@definition(
    owner="core",
    public=True,
    proto={"scalar": "string", "package": "rift.core"},
    schema_extra={},
)
class ActionKind(
    ProtocolRoot[
        "Annotated[str, Field(description='What a compiler action does, as a dotted hierarchical name. Rift fixes the portable families `quickfix`, `source`, `refactor.rename`, `refactor.move`, `refactor.safe_delete`, `refactor.change_signature`, `refactor.extract`, `refactor.inline`, `refactor.introduce`, `refactor.convert`, and `generate`; adapters may refine them with suffixes. Other roots are reverse-domain namespaced. Prefix filtering selects a whole family.', pattern='^[a-z][a-z0-9_.-]*$', max_length=128, json_schema_extra={'rift:proto': {'scalar': 'string', 'package': 'rift.core'}})]"
    ]
):
    """What a compiler action does, as a dotted hierarchical name. Rift fixes the portable families `quickfix`, `source`, `refactor.rename`, `refactor.move`, `refactor.safe_delete`, `refactor.change_signature`, `refactor.extract`, `refactor.inline`, `refactor.introduce`, `refactor.convert`, and `generate`; adapters may refine them with suffixes. Other roots are reverse-domain namespaced. Prefix filtering selects a whole family."""


@definition(
    owner="core", public=True, proto={"type": "rift.core.MatchKey"}, schema_extra={}
)
class MatchKey(ClosedModel):
    """Identity of one match: the query that found it, the snapshot it searched, and the range where it landed. The complete query remains available for inspection and replay."""

    snapshot: Snapshot = Field(
        description="The snapshot it was found in. A match does not survive into another one, because the bytes its span points at have moved.",
        json_schema_extra={"rift:proto": {"field": "snapshot", "number": 1}},
    )
    span: SourceSpan = Field(
        description="The file and byte range that matched.",
        json_schema_extra={"rift:proto": {"field": "span", "number": 2}},
    )
    query: MatchQuery = Field(
        description="The query that produced this match. One span found by two queries denotes two match identities.",
        json_schema_extra={"rift:proto": {"field": "query", "number": 3}},
    )


@definition(
    owner="core", public=True, proto={"type": "rift.core.ActionKey"}, schema_extra={}
)
class ActionKey(ClosedModel):
    """State-bound identity of one discovered compiler action. The language selects the adapter. The opaque token binds its handle and adapter generation."""

    snapshot: Snapshot = Field(
        description="The snapshot it was discovered in. Rift rediscovers the adapter's action and checks this before applying anything, because an action is only meaningful against the code it was computed from.",
        json_schema_extra={"rift:proto": {"field": "snapshot", "number": 1}},
    )
    language: LanguageId = Field(
        description="Language whose adapter minted the token and resolves it.",
        json_schema_extra={"rift:proto": {"field": "language", "number": 2}},
    )
    token: str = Field(
        description="Opaque Rift handle containing the adapter token and generation. Rift unwraps it only when resolving the offer.",
        min_length=1,
        max_length=4096,
        json_schema_extra={"rift:proto": {"field": "token", "number": 3}},
    )


MODELS = (
    ProtocolVersion,
    Digest,
    Revision,
    Commit,
    Worktree,
    Snapshot,
    LanguageRegion,
    FileId,
    File,
    ProjectEntry,
    LeafId,
    SymbolId,
    LanguageId,
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
    FileChange,
    DiffId,
    PreviewId,
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
    LeafFacet,
    RegionRole,
    RelationshipFacet,
    SymbolOrigin,
    Symbol,
    LeafRegion,
    Leaf,
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
    ActionKind,
    MatchKey,
    ActionKey,
)
