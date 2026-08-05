from __future__ import annotations

from . import core
from .base import *


@proto_message(
    {
        "package": "rift.adapter",
        "name": "Capabilities",
        "parent": None,
        "description": "Adapter capabilities read once during startup.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class Capabilities(ProtoModel):
    """Adapter capabilities read once during startup."""

    protocol_version: int = proto_field(
        default=...,
        spec={
            "name": "protocol_version",
            "number": 1,
            "type": "uint32",
            "description": "The contract version this adapter implements. A mismatch stops startup.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    language: str = proto_field(
        default=...,
        spec={
            "name": "language",
            "number": 2,
            "type": "string",
            "description": "The language this adapter answers for, as every `LanguageId` in the model\n"
            " will spell it. One workspace runs one adapter per language, so this is also\n"
            " what makes two adapters distinct.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    implementation: str = proto_field(
        default=...,
        spec={
            "name": "implementation",
            "number": 3,
            "type": "string",
            "description": "The adapter build, as it names itself.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    compiler: str = proto_field(
        default=...,
        spec={
            "name": "compiler",
            "number": 4,
            "type": "string",
            "description": "The compiler behind it, as it names itself.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    claims: list[SourceClaim] = proto_field(
        default=...,
        spec={
            "name": "claims",
            "number": 5,
            "type": "rift.adapter.SourceClaim",
            "description": "Which files this adapter owns, and which it merely reads.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    auxiliary_claims: list[AuxiliaryClaim] = proto_field(
        default=...,
        spec={
            "name": "auxiliary_claims",
            "number": 6,
            "type": "rift.adapter.AuxiliaryClaim",
            "description": "Files that configure the compiler without being source themselves —\n"
            " `Cargo.toml`, `tsconfig.json`. Several adapters may claim the same one.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    virtual_claims: list[VirtualClaim] = proto_field(
        default=...,
        spec={
            "name": "virtual_claims",
            "number": 7,
            "type": "rift.adapter.VirtualClaim",
            "description": "Languages whose generated files this adapter consumes from other adapters.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    write_claims: list[WriteClaim] = proto_field(
        default=...,
        spec={
            "name": "write_claims",
            "number": 8,
            "type": "rift.adapter.WriteClaim",
            "description": "Which paths inside the workspace this adapter's compiler writes to.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    kinds: list[KindDescriptor] = proto_field(
        default=...,
        spec={
            "name": "kinds",
            "number": 9,
            "type": "rift.adapter.KindDescriptor",
            "description": "The exact kinds this adapter emits, and the portable facets each carries.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    structural_matching: core.Coverage = proto_field(
        default=...,
        spec={
            "name": "structural_matching",
            "number": 10,
            "type": "rift.core.Coverage",
            "description": "Coverage and failure reason for structural queries. `StructuralQuery.version`\n"
            " selects the pattern grammar revision the adapter must parse.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    action_discovery: core.Coverage = proto_field(
        default=...,
        spec={
            "name": "action_discovery",
            "number": 11,
            "type": "rift.core.Coverage",
            "description": "Coverage and failure reason for compiler action discovery.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    extension_values: list[ExtensionDescriptor] = proto_field(
        default=...,
        spec={
            "name": "extension_values",
            "number": 12,
            "type": "rift.adapter.ExtensionDescriptor",
            "description": "Versioned extension values this adapter emits, and where each may appear.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    limits: AdapterLimits = proto_field(
        default=...,
        spec={
            "name": "limits",
            "number": 13,
            "type": "rift.adapter.AdapterLimits",
            "description": "Bounds Rift must stay inside when driving this adapter.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    runtime: RuntimeRequirements = proto_field(
        default=...,
        spec={
            "name": "runtime",
            "number": 14,
            "type": "rift.adapter.RuntimeRequirements",
            "description": "What the host has to enforce around this adapter's process.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    validation: core.Coverage = proto_field(
        default=...,
        spec={
            "name": "validation",
            "number": 15,
            "type": "rift.core.Coverage",
            "description": "Whether this adapter can validate a changed program before publication,\n"
            " and which scope it can check completely.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    formatting: core.Coverage = proto_field(
        default=...,
        spec={
            "name": "formatting",
            "number": 16,
            "type": "rift.core.Coverage",
            "description": "Whether this adapter can format changed syntax regions or complete affected\n"
            " files under FormattingPolicy.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    action_kinds: list[core.ActionSupport] = proto_field(
        default=...,
        spec={
            "name": "action_kinds",
            "number": 17,
            "type": "rift.core.ActionSupport",
            "description": "Action families available for planning before a target-specific Actions\n"
            " call. Sorted by kind prefix.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "SourceClaim",
        "parent": None,
        "description": "A deterministic claim on physical files, re-evaluated after every path or\n"
        " content change. Selectors are ORed and compare UTF-8 bytes: suffixes against\n"
        " the full path, names against the final component, shebang prefixes against\n"
        " the first line. Highest priority wins. Equal priority from two adapters is an\n"
        " ownership error.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class SourceClaim(ProtoModel):
    """A deterministic claim on physical files, re-evaluated after every path or
    content change. Selectors are ORed and compare UTF-8 bytes: suffixes against
    the full path, names against the final component, shebang prefixes against
    the first line. Highest priority wins. Equal priority from two adapters is an
    ownership error."""

    language: str = proto_field(
        default=...,
        spec={
            "name": "language",
            "number": 1,
            "type": "string",
            "description": "The language these files are parsed as.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    priority: int = proto_field(
        default=...,
        spec={
            "name": "priority",
            "number": 2,
            "type": "uint32",
            "description": "Which adapter wins where two claim the same path. Higher takes it.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    path_suffixes: list[str] = proto_field(
        default=...,
        spec={
            "name": "path_suffixes",
            "number": 3,
            "type": "string",
            "description": "Suffixes matched against the whole path, so `.d.ts` can be claimed apart\n from `.ts`.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    path_names: list[str] = proto_field(
        default=...,
        spec={
            "name": "path_names",
            "number": 4,
            "type": "string",
            "description": "Whole final path components, for files whose name carries the meaning —\n `Makefile`, `Dockerfile`.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    shebang_prefixes: list[str] = proto_field(
        default=...,
        spec={
            "name": "shebang_prefixes",
            "number": 5,
            "type": "string",
            "description": "First-line prefixes for extensionless scripts: `#!/usr/bin/env python`.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    container: bool = proto_field(
        default=...,
        spec={
            "name": "container",
            "number": 6,
            "type": "bool",
            "description": "Whether a claimed file may contain other languages. A Svelte owner sets\n"
            " this for an embedded TypeScript region, then publishes that region as a\n"
            " virtual source when the TypeScript adapter has to analyze it.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_enum(
    {
        "package": "rift.adapter",
        "name": "Purpose",
        "parent": "rift.adapter.AuxiliaryClaim",
        "description": "What an auxiliary file is to the compiler that reads it. This is what the\n"
        " file means, and it is what Rift acts on: a workspace manifest changing can\n"
        " add or remove whole source trees, which Rift has to rescan for. What the\n"
        " adapter has to redo is `environment_defining`.",
        "allow_alias": False,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {
                "name": "UNSPECIFIED",
                "number": 0,
                "description": "The adapter did not say. Treated as the widest purpose, so a change to\n"
                " the file invalidates everything derived from it.",
                "deprecated": False,
            },
            {
                "name": "COMPILER_CONFIGURATION",
                "number": 1,
                "description": "Settings the compiler reads directly — `tsconfig.json`, `.rustfmt.toml`.",
                "deprecated": False,
            },
            {
                "name": "DEPENDENCY_MANIFEST",
                "number": 2,
                "description": "Declared dependencies, before resolution — `package.json`, `Cargo.toml`.",
                "deprecated": False,
            },
            {
                "name": "DEPENDENCY_LOCK",
                "number": 3,
                "description": "Resolved dependency versions — `Cargo.lock`, `bun.lock`. Editing one can\n"
                " change every type the compiler resolves.",
                "deprecated": False,
            },
            {
                "name": "WORKSPACE_MANIFEST",
                "number": 4,
                "description": "Which packages make up the project — a Cargo workspace, an npm workspace.\n"
                " A change here can add or remove whole source trees.",
                "deprecated": False,
            },
            {
                "name": "GENERATED_METADATA",
                "number": 5,
                "description": "Metadata a build step wrote for the compiler to read back.",
                "deprecated": False,
            },
            {
                "name": "OTHER",
                "number": 6,
                "description": "Something the compiler needs that none of the above describes. The claim\n"
                " states its purpose.",
                "deprecated": False,
            },
        ],
    }
)
class AuxiliaryClaimPurpose(IntEnum):
    """What an auxiliary file is to the compiler that reads it. This is what the
    file means, and it is what Rift acts on: a workspace manifest changing can
    add or remove whole source trees, which Rift has to rescan for. What the
    adapter has to redo is `environment_defining`."""

    UNSPECIFIED = 0
    COMPILER_CONFIGURATION = 1
    DEPENDENCY_MANIFEST = 2
    DEPENDENCY_LOCK = 3
    WORKSPACE_MANIFEST = 4
    GENERATED_METADATA = 5
    OTHER = 6


@proto_message(
    {
        "package": "rift.adapter",
        "name": "AuxiliaryClaim",
        "parent": None,
        "description": "A non-owning claim on project files needed to discover or configure compiler\n"
        " semantics. Same bytewise rules as SourceClaim, may overlap across adapters,\n"
        " and never makes a file a source unit.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class AuxiliaryClaim(ProtoModel):
    """A non-owning claim on project files needed to discover or configure compiler
    semantics. Same bytewise rules as SourceClaim, may overlap across adapters,
    and never makes a file a source unit."""

    purposes: list[AuxiliaryClaimPurpose] = proto_field(
        default=...,
        spec={
            "name": "purposes",
            "number": 1,
            "type": "rift.adapter.AuxiliaryClaim.Purpose",
            "description": "What the file is to the compiler, and so what Rift itself has to redo when\n it changes.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    path_suffixes: list[str] = proto_field(
        default=...,
        spec={
            "name": "path_suffixes",
            "number": 2,
            "type": "string",
            "description": "Suffixes matched against the whole path.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    path_names: list[str] = proto_field(
        default=...,
        spec={
            "name": "path_names",
            "number": 3,
            "type": "string",
            "description": "Whole final path components — `Cargo.toml`, `package.json`.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    environment_defining: bool = proto_field(
        default=...,
        spec={
            "name": "environment_defining",
            "number": 4,
            "type": "bool",
            "description": "Whether a Refresh touching this file forces the adapter to rebuild its\n"
            " resolved state. Declared and never inferred\n"
            " from purpose or path, because whether a compiler rereads a given file is a\n"
            " fact only the adapter driving it has.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "VirtualClaim",
        "parent": None,
        "description": "A claim to consume virtual sources of one language. Highest priority selects\n"
        " one consumer. Equal priorities, duplicate producer paths, collisions with a\n"
        " physical entry or write claim, paths below a gitlink, and cycles in the\n"
        " producer-to-consumer graph are adapter protocol errors. The producer keeps\n"
        " authority over the virtual bytes, regions, and origin mappings.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class VirtualClaim(ProtoModel):
    """A claim to consume virtual sources of one language. Highest priority selects
    one consumer. Equal priorities, duplicate producer paths, collisions with a
    physical entry or write claim, paths below a gitlink, and cycles in the
    producer-to-consumer graph are adapter protocol errors. The producer keeps
    authority over the virtual bytes, regions, and origin mappings."""

    language: str = proto_field(
        default=...,
        spec={
            "name": "language",
            "number": 1,
            "type": "string",
            "description": "The language of the produced files this adapter will consume.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    priority: int = proto_field(
        default=...,
        spec={
            "name": "priority",
            "number": 2,
            "type": "uint32",
            "description": "Which consumer wins where two claim the same produced language.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "WriteClaim",
        "parent": None,
        "description": "Paths inside the workspace that this adapter's compiler writes to. Rift\n"
        " classifies each claimed subtree as compiler output and excludes it from\n"
        " source analysis, snapshot diffs, and agent results.\n"
        " Write claims are exclusive across adapters sharing a session worktree. Rift\n"
        " refuses workspace admission when two adapters can write the same path; one\n"
        " of them must redirect that output to its private state_root.\n"
        "\n"
        " CARGO_TARGET_DIR, GOCACHE, PYTHONPYCACHEPREFIX, and tsBuildInfoFile can point\n"
        " at `state_root`. Declare output that remains in the tree: `cargo`\n"
        " rewrites Cargo.lock at the workspace root, npm insists on `node_modules` next\n"
        " to the package it belongs to.\n"
        "\n"
        " Selectors follow SourceClaim's bytewise rules. A directory name matches the\n"
        " directory and everything beneath it.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class WriteClaim(ProtoModel):
    """Paths inside the workspace that this adapter's compiler writes to. Rift
    classifies each claimed subtree as compiler output and excludes it from
    source analysis, snapshot diffs, and agent results.
    Write claims are exclusive across adapters sharing a session worktree. Rift
    refuses workspace admission when two adapters can write the same path; one
    of them must redirect that output to its private state_root.

    CARGO_TARGET_DIR, GOCACHE, PYTHONPYCACHEPREFIX, and tsBuildInfoFile can point
    at `state_root`. Declare output that remains in the tree: `cargo`
    rewrites Cargo.lock at the workspace root, npm insists on `node_modules` next
    to the package it belongs to.

    Selectors follow SourceClaim's bytewise rules. A directory name matches the
    directory and everything beneath it."""

    path_names: list[str] = proto_field(
        default=...,
        spec={
            "name": "path_names",
            "number": 1,
            "type": "string",
            "description": "Whole final path components. A directory named here takes its whole tree\n"
            " with it — `node_modules`, `target`.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    path_suffixes: list[str] = proto_field(
        default=...,
        spec={
            "name": "path_suffixes",
            "number": 2,
            "type": "string",
            "description": "Suffixes matched against the whole path, covering generated files in\n several directories.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    reason: str = proto_field(
        default=...,
        spec={
            "name": "reason",
            "number": 3,
            "type": "string",
            "description": "Why the compiler needs it, in one line. Rift logs this when a claim shadows\n"
            " a file the repository actually tracks, which is nearly always a mistake.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "KindDescriptor",
        "parent": None,
        "description": "One kind this adapter emits, and the portable facets it carries. Which facet\n"
        " vocabulary applies depends on what the kind describes — a symbol, a leaf or\n"
        " a relationship — so the branch that is set is what says which.",
        "oneofs": ["facets"],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class KindDescriptor(ProtoModel):
    """One kind this adapter emits, and the portable facets it carries. Which facet
    vocabulary applies depends on what the kind describes — a symbol, a leaf or
    a relationship — so the branch that is set is what says which."""

    kind: str = proto_field(
        default=...,
        spec={
            "name": "kind",
            "number": 1,
            "type": "string",
            "description": "The kind as the language names it: `rust.trait`, `typescript.interface`.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    symbol: SymbolFacets | None = proto_field(
        default=None,
        spec={
            "name": "symbol",
            "number": 2,
            "type": "rift.adapter.SymbolFacets",
            "description": "Facets carried by symbols of this kind.",
            "repeated": False,
            "optional": False,
            "oneof": "facets",
            "deprecated": False,
        },
    )
    leaf: LeafFacets | None = proto_field(
        default=None,
        spec={
            "name": "leaf",
            "number": 3,
            "type": "rift.adapter.LeafFacets",
            "description": "Facets carried by leaves of this kind.",
            "repeated": False,
            "optional": False,
            "oneof": "facets",
            "deprecated": False,
        },
    )
    relationship: RelationshipFacets | None = proto_field(
        default=None,
        spec={
            "name": "relationship",
            "number": 4,
            "type": "rift.adapter.RelationshipFacets",
            "description": "Facets carried by relationships of this kind.",
            "repeated": False,
            "optional": False,
            "oneof": "facets",
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "SymbolFacets",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class SymbolFacets(ProtoModel):
    values: list[core.SymbolFacet] = proto_field(
        default=...,
        spec={
            "name": "values",
            "number": 1,
            "type": "rift.core.SymbolFacet",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "LeafFacets",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class LeafFacets(ProtoModel):
    values: list[core.LeafFacet] = proto_field(
        default=...,
        spec={
            "name": "values",
            "number": 1,
            "type": "rift.core.LeafFacet",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "RelationshipFacets",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class RelationshipFacets(ProtoModel):
    values: list[core.RelationshipFacet] = proto_field(
        default=...,
        spec={
            "name": "values",
            "number": 1,
            "type": "rift.core.RelationshipFacet",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "ExtensionDescriptor",
        "parent": None,
        "description": "One versioned extension value, and every place it may appear. Rift validates\n"
        " each occurrence against the schema.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class ExtensionDescriptor(ProtoModel):
    """One versioned extension value, and every place it may appear. Rift validates
    each occurrence against the schema."""

    key: str = proto_field(
        default=...,
        spec={
            "name": "key",
            "number": 1,
            "type": "string",
            "description": "Reverse-domain namespaced key.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    version: int = proto_field(
        default=...,
        spec={
            "name": "version",
            "number": 2,
            "type": "uint32",
            "description": "Bumped whenever the schema changes shape. A consumer skips versions it does\n not implement.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    targets: list[str] = proto_field(
        default=...,
        spec={
            "name": "targets",
            "number": 3,
            "type": "string",
            "description": "Which model types this value may be attached to — `Symbol`, `Leaf`.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    schema_: str = proto_field(
        default=...,
        spec={
            "name": "schema",
            "number": 4,
            "type": "string",
            "description": "A self-contained JSON Schema draft 2020-12 document. Remote references are\n"
            " forbidden, so validation fetches nothing.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    indexed: bool = proto_field(
        default=...,
        spec={
            "name": "indexed",
            "number": 5,
            "type": "bool",
            "description": "Whether Rift builds a search index over this value. Each indexed fact costs\n"
            " storage, so the adapter declares this explicitly.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    semantic: bool = proto_field(
        default=...,
        spec={
            "name": "semantic",
            "number": 6,
            "type": "bool",
            "description": "Whether this value contributes to semantic cache identity.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "AdapterLimits",
        "parent": None,
        "description": "Bounds this adapter advertises. One connection holds several workspaces, so\n"
        " each workspace receives its own in-flight limit.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class AdapterLimits(ProtoModel):
    """Bounds this adapter advertises. One connection holds several workspaces, so
    each workspace receives its own in-flight limit."""

    max_message_bytes: int = proto_field(
        default=...,
        spec={
            "name": "max_message_bytes",
            "number": 1,
            "type": "uint64",
            "description": "Largest single message this adapter will accept or send. A fact batch is\n split to stay under it.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    max_in_flight: int = proto_field(
        default=...,
        spec={
            "name": "max_in_flight",
            "number": 2,
            "type": "uint32",
            "description": "Concurrent calls accepted across the whole connection.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    max_in_flight_per_workspace: int = proto_field(
        default=...,
        spec={
            "name": "max_in_flight_per_workspace",
            "number": 3,
            "type": "uint32",
            "description": "Concurrent calls accepted for any one workspace, so one agent cannot\n consume the process.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    max_workspaces: int = proto_field(
        default=...,
        spec={
            "name": "max_workspaces",
            "number": 4,
            "type": "uint32",
            "description": "Workspaces this adapter will hold open at once. Each one is a parsed tree\n"
            " and a compiler's state, so this is the number that decides its memory.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "RuntimeRequirements",
        "parent": None,
        "description": "What the host must enforce for this adapter to run on untrusted source.\n"
        " The host denies network access. It mounts the workspace and declared\n"
        " dependencies read-only. Only state_root and WriteClaim paths are writable.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Capabilities",
    }
)
class RuntimeRequirements(ProtoModel):
    """What the host must enforce for this adapter to run on untrusted source.
    The host denies network access. It mounts the workspace and declared
    dependencies read-only. Only state_root and WriteClaim paths are writable."""

    executes_repository_code: bool = proto_field(
        default=...,
        spec={
            "name": "executes_repository_code",
            "number": 1,
            "type": "bool",
            "description": "Whether the adapter executes code from the repository — a build script, a\n plugin, a macro.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    spawns_subprocesses: bool = proto_field(
        default=...,
        spec={
            "name": "spawns_subprocesses",
            "number": 2,
            "type": "bool",
            "description": "Whether the adapter spawns subprocesses.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "WorkspaceState",
        "parent": None,
        "description": "What a workspace holds right now. Every call that reads code carries one. A\n"
        " call naming an earlier state receives `StaleState`.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Workspaces",
    }
)
class WorkspaceState(ProtoModel):
    """What a workspace holds right now. Every call that reads code carries one. A
    call naming an earlier state receives `StaleState`."""

    workspace: str = proto_field(
        default=...,
        spec={
            "name": "workspace",
            "number": 1,
            "type": "string",
            "description": "Which workspace this describes, by the absolute path of its working tree.\n"
            " The path is unique within this adapter process. Another language adapter\n"
            " can hold the same path with its own compiler state and state_root.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    snapshot: core.Snapshot = proto_field(
        default=...,
        spec={
            "name": "snapshot",
            "number": 2,
            "type": "rift.core.Snapshot",
            "description": "The state the files in it hold.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    generation: int = proto_field(
        default=...,
        spec={
            "name": "generation",
            "number": 3,
            "type": "uint64",
            "description": "Adapter-local state generation. OpenWorkspace mints the first value;\n"
            " Refresh and SyncVirtual advance it. Calls carrying an earlier generation\n"
            " receive StaleState even when the Git snapshot is unchanged.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "OpenWorkspaceRequest",
        "parent": None,
        "description": "Take this working tree and start a compiler against it. Rift has already\n"
        " written every file, so nothing here has to be fetched or waited for.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Workspaces",
    }
)
class OpenWorkspaceRequest(ProtoModel):
    """Take this working tree and start a compiler against it. Rift has already
    written every file, so nothing here has to be fetched or waited for."""

    workspace: str = proto_field(
        default=...,
        spec={
            "name": "workspace",
            "number": 1,
            "type": "string",
            "description": "Absolute path to the shared session worktree, and the identity this adapter\n"
            " process uses for it. Other language adapters can receive the same path.\n"
            " Compiler writes go to `state_root` or a declared WriteClaim.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    state_root: str = proto_field(
        default=...,
        spec={
            "name": "state_root",
            "number": 2,
            "type": "string",
            "description": "Absolute path to a directory the adapter owns and Rift never reads. Build\n"
            " output belongs here — point CARGO_TARGET_DIR and friends at it. Every open\n"
            " workspace receives a distinct path, and Rift never reuses that path after\n"
            " CloseWorkspace. An adapter may maintain its own shared read-only cache\n"
            " outside this directory when its compiler supports safe sharing.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    snapshot: core.Snapshot = proto_field(
        default=...,
        spec={
            "name": "snapshot",
            "number": 3,
            "type": "rift.core.Snapshot",
            "description": "The state that tree holds.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "RefreshRequest",
        "parent": None,
        "description": "Rift rewrote the tree in place under the session-wide write barrier. It sends\n"
        " this call to every language adapter holding the shared path. Paths are\n"
        " project-relative and cover every file Rift touched; each adapter decides\n"
        " which changes affect its compiler. Content is on disk before this call.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Workspaces",
    }
)
class RefreshRequest(ProtoModel):
    """Rift rewrote the tree in place under the session-wide write barrier. It sends
    this call to every language adapter holding the shared path. Paths are
    project-relative and cover every file Rift touched; each adapter decides
    which changes affect its compiler. Content is on disk before this call."""

    from_: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "from",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "The state the adapter last acknowledged. A mismatch means Rift and the\n"
            " adapter disagree about what is on disk, and comes back as StaleState.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    to: core.Snapshot = proto_field(
        default=...,
        spec={
            "name": "to",
            "number": 2,
            "type": "rift.core.Snapshot",
            "description": "The state the tree holds now.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    written: list[str] = proto_field(
        default=...,
        spec={
            "name": "written",
            "number": 3,
            "type": "string",
            "description": "Files created or overwritten since `from`, as `rift://file/<path>`.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    removed: list[str] = proto_field(
        default=...,
        spec={
            "name": "removed",
            "number": 4,
            "type": "string",
            "description": "Files deleted since `from`. A rename arrives as one of each.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "VirtualSyncEvent",
        "parent": None,
        "description": "One atomic replacement of the virtual sources consumed by an adapter. Rift\n"
        " sends exactly one `start` event first, then each `unit` followed by ordered\n"
        " `text` chunks ending in `final: true`. The complete stream replaces the\n"
        " previous overlay; an RPC failure leaves the previous generation intact. The\n"
        " units are the complete set selected for this consumer at `start.snapshot`,\n"
        " including an empty set when every previous unit has been retracted.",
        "oneofs": ["event"],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Workspaces",
    }
)
class VirtualSyncEvent(ProtoModel):
    """One atomic replacement of the virtual sources consumed by an adapter. Rift
    sends exactly one `start` event first, then each `unit` followed by ordered
    `text` chunks ending in `final: true`. The complete stream replaces the
    previous overlay; an RPC failure leaves the previous generation intact. The
    units are the complete set selected for this consumer at `start.snapshot`,
    including an empty set when every previous unit has been retracted."""

    start: WorkspaceState | None = proto_field(
        default=None,
        spec={
            "name": "start",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "Current adapter-local state. The returned WorkspaceState keeps its\n"
            " snapshot and advances its generation.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )
    unit: VirtualUnit | None = proto_field(
        default=None,
        spec={
            "name": "unit",
            "number": 2,
            "type": "rift.adapter.VirtualUnit",
            "description": "One virtual file selected for this adapter by VirtualClaim.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )
    text: VirtualText | None = proto_field(
        default=None,
        spec={
            "name": "text",
            "number": 3,
            "type": "rift.adapter.VirtualText",
            "description": "Ordered bytes for the preceding unit.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "CloseWorkspaceRequest",
        "parent": None,
        "description": "Let go of this workspace. Rift deletes the tree once the adapter has.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Workspaces",
    }
)
class CloseWorkspaceRequest(ProtoModel):
    """Let go of this workspace. Rift deletes the tree once the adapter has."""

    workspace: str = proto_field(
        default=...,
        spec={
            "name": "workspace",
            "number": 1,
            "type": "string",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "AnalyzeRequest",
        "parent": None,
        "description": "Read every claimed physical or synced virtual source in this workspace. A\n"
        " physical unit below a gitlink is an adapter protocol error. The state is what\n"
        " the answer is pinned to.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class AnalyzeRequest(ProtoModel):
    """Read every claimed physical or synced virtual source in this workspace. A
    physical unit below a gitlink is an adapter protocol error. The state is what
    the answer is pinned to."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "AnalyzeEvent",
        "parent": None,
        "description": "One event of an analyze stream, followed eventually by exactly one summary.",
        "oneofs": ["event"],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class AnalyzeEvent(ProtoModel):
    """One event of an analyze stream, followed eventually by exactly one summary."""

    source_unit: SourceUnit | None = proto_field(
        default=None,
        spec={
            "name": "source_unit",
            "number": 1,
            "type": "rift.adapter.SourceUnit",
            "description": "A claimed physical or synced virtual unit is about to have facts.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )
    virtual_unit: VirtualUnit | None = proto_field(
        default=None,
        spec={
            "name": "virtual_unit",
            "number": 2,
            "type": "rift.adapter.VirtualUnit",
            "description": "A file this adapter produced is about to have text and facts.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )
    virtual_text: VirtualText | None = proto_field(
        default=None,
        spec={
            "name": "virtual_text",
            "number": 3,
            "type": "rift.adapter.VirtualText",
            "description": "A chunk of a produced file's bytes.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )
    facts: Facts | None = proto_field(
        default=None,
        spec={
            "name": "facts",
            "number": 4,
            "type": "rift.adapter.Facts",
            "description": "A batch of facts for one file.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )
    coverage: UnitCoverage | None = proto_field(
        default=None,
        spec={
            "name": "coverage",
            "number": 5,
            "type": "rift.adapter.UnitCoverage",
            "description": "No more facts for one file.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )
    summary: AnalyzeSummary | None = proto_field(
        default=None,
        spec={
            "name": "summary",
            "number": 6,
            "type": "rift.adapter.AnalyzeSummary",
            "description": "No more events at all.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "SourceUnit",
        "parent": None,
        "description": "Metadata for one physical or synced virtual source unit this adapter owns.\n"
        " Rift resolves the URI against the parent Git tree or the virtual overlay at\n"
        " this WorkspaceState generation.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class SourceUnit(ProtoModel):
    """Metadata for one physical or synced virtual source unit this adapter owns.
    Rift resolves the URI against the parent Git tree or the virtual overlay at
    this WorkspaceState generation."""

    unit: str = proto_field(
        default=...,
        spec={
            "name": "unit",
            "number": 1,
            "type": "string",
            "description": "The unit, as `rift://file/<path>`. The same spelling `SourceSpan.unit`\n"
            " uses, so a fact and the unit it was read from compare without translation.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    regions: list[core.LanguageRegion] = proto_field(
        default=...,
        spec={
            "name": "regions",
            "number": 2,
            "type": "rift.core.LanguageRegion",
            "description": "Which bytes each language owns. One entry for a file in a single language,\n"
            " several where one embeds another.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "VirtualUnit",
        "parent": None,
        "description": "Metadata for a file this adapter produced from another one. Rift validates\n"
        " the complete publication and forwards it to the selected consumer through\n"
        " SyncVirtual. The consumer stores it in a private overlay under state_root.\n"
        " Rift exposes the retained bytes as a regular, non-executable File resource.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class VirtualUnit(ProtoModel):
    """Metadata for a file this adapter produced from another one. Rift validates
    the complete publication and forwards it to the selected consumer through
    SyncVirtual. The consumer stores it in a private overlay under state_root.
    Rift exposes the retained bytes as a regular, non-executable File resource."""

    unit: str = proto_field(
        default=...,
        spec={
            "name": "unit",
            "number": 1,
            "type": "string",
            "description": "The produced file, as `rift://file/<path>`. The adapter mints the path;\n"
            " downstream facts use the same identity even though the bytes live in the\n"
            " consumer's private overlay.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    language: str = proto_field(
        default=...,
        spec={
            "name": "language",
            "number": 2,
            "type": "string",
            "description": "The language that will consume these bytes, selected by a VirtualClaim.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    regions: list[core.LanguageRegion] = proto_field(
        default=...,
        spec={
            "name": "regions",
            "number": 3,
            "type": "rift.core.LanguageRegion",
            "description": "Which bytes each language owns inside the produced file. A consumer echoes\n"
            " these regions when it later emits SourceUnit for the synced path.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "VirtualText",
        "parent": None,
        "description": "One chunk of a produced file's bytes. Sent in pieces because a generated file\n"
        " can be far larger than the source it came from, and a message has a bound.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class VirtualText(ProtoModel):
    """One chunk of a produced file's bytes. Sent in pieces because a generated file
    can be far larger than the source it came from, and a message has a bound."""

    unit: str = proto_field(
        default=...,
        spec={
            "name": "unit",
            "number": 1,
            "type": "string",
            "description": "The produced file these bytes belong to, spelled as its VirtualUnit did.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    offset: int = proto_field(
        default=...,
        spec={
            "name": "offset",
            "number": 2,
            "type": "uint64",
            "description": "Byte offset this chunk starts at. Chunks arrive in order and leave no gaps.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    text: str = proto_field(
        default=...,
        spec={
            "name": "text",
            "number": 3,
            "type": "string",
            "description": "The bytes themselves.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    final: bool = proto_field(
        default=...,
        spec={
            "name": "final",
            "number": 4,
            "type": "bool",
            "description": "Whether this is the last chunk. Rift holds the file until it arrives.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "Facts",
        "parent": None,
        "description": "One file's worth of one fact family. Rift buffers by file until the coverage\n"
        " event for that file, then routes complete virtual units to their consumer.",
        "oneofs": ["family"],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class Facts(ProtoModel):
    """One file's worth of one fact family. Rift buffers by file until the coverage
    event for that file, then routes complete virtual units to their consumer."""

    unit: str = proto_field(
        default=...,
        spec={
            "name": "unit",
            "number": 1,
            "type": "string",
            "description": "The file these facts were read from, as `rift://file/<path>`.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    origin_mappings: OriginMappings | None = proto_field(
        default=None,
        spec={
            "name": "origin_mappings",
            "number": 2,
            "type": "rift.adapter.OriginMappings",
            "repeated": False,
            "optional": False,
            "oneof": "family",
            "deprecated": False,
        },
    )
    symbols: Symbols | None = proto_field(
        default=None,
        spec={
            "name": "symbols",
            "number": 3,
            "type": "rift.adapter.Symbols",
            "repeated": False,
            "optional": False,
            "oneof": "family",
            "deprecated": False,
        },
    )
    leaves: Leaves | None = proto_field(
        default=None,
        spec={
            "name": "leaves",
            "number": 4,
            "type": "rift.adapter.Leaves",
            "repeated": False,
            "optional": False,
            "oneof": "family",
            "deprecated": False,
        },
    )
    relationships: Relationships | None = proto_field(
        default=None,
        spec={
            "name": "relationships",
            "number": 5,
            "type": "rift.adapter.Relationships",
            "repeated": False,
            "optional": False,
            "oneof": "family",
            "deprecated": False,
        },
    )
    diagnostics: Diagnostics | None = proto_field(
        default=None,
        spec={
            "name": "diagnostics",
            "number": 6,
            "type": "rift.adapter.Diagnostics",
            "repeated": False,
            "optional": False,
            "oneof": "family",
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "OriginMappings",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class OriginMappings(ProtoModel):
    items: list[core.OriginMapping] = proto_field(
        default=...,
        spec={
            "name": "items",
            "number": 1,
            "type": "rift.core.OriginMapping",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "Symbols",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class Symbols(ProtoModel):
    items: list[core.Symbol] = proto_field(
        default=...,
        spec={
            "name": "items",
            "number": 1,
            "type": "rift.core.Symbol",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "Leaves",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class Leaves(ProtoModel):
    items: list[core.Leaf] = proto_field(
        default=...,
        spec={
            "name": "items",
            "number": 1,
            "type": "rift.core.Leaf",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "Relationships",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class Relationships(ProtoModel):
    items: list[core.Relationship] = proto_field(
        default=...,
        spec={
            "name": "items",
            "number": 1,
            "type": "rift.core.Relationship",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "Diagnostics",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class Diagnostics(ProtoModel):
    items: list[core.Diagnostic] = proto_field(
        default=...,
        spec={
            "name": "items",
            "number": 1,
            "type": "rift.core.Diagnostic",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "UnitCoverage",
        "parent": None,
        "description": "Closes one file's facts. `present=false` clears everything buffered for it.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class UnitCoverage(ProtoModel):
    """Closes one file's facts. `present=false` clears everything buffered for it."""

    unit: str = proto_field(
        default=...,
        spec={
            "name": "unit",
            "number": 1,
            "type": "string",
            "description": "The file being closed, as `rift://file/<path>`.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    present: bool = proto_field(
        default=...,
        spec={
            "name": "present",
            "number": 2,
            "type": "bool",
            "description": "Whether the file exists and was read at all. False retracts it.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    coverage: core.SemanticCoverage = proto_field(
        default=...,
        spec={
            "name": "coverage",
            "number": 3,
            "type": "rift.core.SemanticCoverage",
            "description": "How complete each fact family is for this file. All six are stated: a\n"
            " family the adapter cannot produce is `unsupported` and one the language has\n"
            " no concept of is `not_applicable`, so there is no silence to interpret.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "AnalyzeSummary",
        "parent": None,
        "description": "Analyze is rerun-equal: the same snapshot under the same environment yields\n"
        " the same ordered facts. Rift compares or stores those facts directly.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Analysis",
    }
)
class AnalyzeSummary(ProtoModel):
    """Analyze is rerun-equal: the same snapshot under the same environment yields
    the same ordered facts. Rift compares or stores those facts directly."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "The state every fact in this stream was read against.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    invalidated: list[str] = proto_field(
        default=...,
        spec={
            "name": "invalidated",
            "number": 2,
            "type": "string",
            "description": "Files whose previously published facts no longer hold, as\n"
            " `rift://file/<path>`. This reaches files the stream said nothing about,\n"
            " which is what a per-file `UnitCoverage` cannot do.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "ValidateRequest",
        "parent": None,
        "description": "Ask the compiler to validate the closure affected by a candidate change.\n"
        " Rift has already applied the candidate and acknowledged its state through\n"
        " Refresh, so the adapter reads the same files publication is considering.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Validation",
    }
)
class ValidateRequest(ProtoModel):
    """Ask the compiler to validate the closure affected by a candidate change.
    Rift has already applied the candidate and acknowledged its state through
    Refresh, so the adapter reads the same files publication is considering."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "Candidate state being checked.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    changed: list[str] = proto_field(
        default=...,
        spec={
            "name": "changed",
            "number": 2,
            "type": "string",
            "description": "Files changed by the candidate, as project-relative paths. The compiler\n"
            " expands these to the dependent files its own validity rules require.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "ValidateEvent",
        "parent": None,
        "description": "Validation diagnostics followed by exactly one summary.",
        "oneofs": ["event"],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Validation",
    }
)
class ValidateEvent(ProtoModel):
    """Validation diagnostics followed by exactly one summary."""

    diagnostics: Diagnostics | None = proto_field(
        default=None,
        spec={
            "name": "diagnostics",
            "number": 1,
            "type": "rift.adapter.Diagnostics",
            "description": "A bounded batch of findings from the compiler.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )
    summary: ValidationSummary | None = proto_field(
        default=None,
        spec={
            "name": "summary",
            "number": 2,
            "type": "rift.adapter.ValidationSummary",
            "description": "The final verdict and the scope actually checked.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "ValidationSummary",
        "parent": None,
        "description": "Closes one validation stream. Rift combines this with the preceding\n"
        " diagnostics and the adapter's language to construct ValidationReport.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Validation",
    }
)
class ValidationSummary(ProtoModel):
    """Closes one validation stream. Rift combines this with the preceding
    diagnostics and the adapter's language to construct ValidationReport."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "State every finding and the verdict were computed from.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    valid: bool = proto_field(
        default=...,
        spec={
            "name": "valid",
            "number": 2,
            "type": "bool",
            "description": "Whether the compiler accepted the checked scope.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    coverage: core.Coverage = proto_field(
        default=...,
        spec={
            "name": "coverage",
            "number": 3,
            "type": "rift.core.Coverage",
            "description": "How completely the adapter checked the affected compiler closure.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    files: list[str] = proto_field(
        default=...,
        spec={
            "name": "files",
            "number": 4,
            "type": "string",
            "description": "Files included in that closure, as project-relative paths in byte order.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "FormatRequest",
        "parent": None,
        "description": "Ask the language formatter for concrete edits over the current candidate.\n"
        " Rift skips this RPC for FormattingPolicy.PRESERVE.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Formatting",
    }
)
class FormatRequest(ProtoModel):
    """Ask the language formatter for concrete edits over the current candidate.
    Rift skips this RPC for FormattingPolicy.PRESERVE."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "Candidate state whose offsets the response must use.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    policy: core.FormattingPolicy = proto_field(
        default=...,
        spec={
            "name": "policy",
            "number": 2,
            "type": "rift.core.FormattingPolicy",
            "description": "CHANGED_REGIONS or AFFECTED_FILES. PRESERVE is refused because it requires\n no formatter work.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    spans: list[core.SourceSpan] = proto_field(
        default=...,
        spec={
            "name": "spans",
            "number": 3,
            "type": "rift.core.SourceSpan",
            "description": "Text ranges changed by the resolved operation. Required for\n"
            " CHANGED_REGIONS and ignored for AFFECTED_FILES.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    files: list[str] = proto_field(
        default=...,
        spec={
            "name": "files",
            "number": 4,
            "type": "string",
            "description": "Changed files owned by this adapter. Required for AFFECTED_FILES and used\n"
            " to group spans for CHANGED_REGIONS.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "FormatResponse",
        "parent": None,
        "description": "Formatter output over one acknowledged candidate state.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Formatting",
    }
)
class FormatResponse(ProtoModel):
    """Formatter output over one acknowledged candidate state."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "State the formatter read.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    edits: list[core.TextEdit] = proto_field(
        default=...,
        spec={
            "name": "edits",
            "number": 2,
            "type": "rift.core.TextEdit",
            "description": "Atomic non-overlapping replacements in canonical file-and-range order.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    coverage: core.Coverage = proto_field(
        default=...,
        spec={
            "name": "coverage",
            "number": 3,
            "type": "rift.core.Coverage",
            "description": "Whether every requested region or file was formatted.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    diagnostics: list[core.Diagnostic] = proto_field(
        default=...,
        spec={
            "name": "diagnostics",
            "number": 4,
            "type": "rift.core.Diagnostic",
            "description": "Formatter findings that explain partial coverage or a refusal.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "MatchRequest",
        "parent": None,
        "description": "Find every place matching one pattern parsed by the language compiler.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Matching",
    }
)
class MatchRequest(ProtoModel):
    """Find every place matching one pattern parsed by the language compiler."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "The state to search, and what the answer is pinned to.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    query: core.StructuralQuery = proto_field(
        default=...,
        spec={
            "name": "query",
            "number": 2,
            "type": "rift.core.StructuralQuery",
            "description": "The pattern, and the constraints on what its captures may be.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "MatchEvent",
        "parent": None,
        "description": "One event of a match stream, followed eventually by exactly one summary.",
        "oneofs": ["event"],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Matching",
    }
)
class MatchEvent(ProtoModel):
    """One event of a match stream, followed eventually by exactly one summary."""

    matches: Matches | None = proto_field(
        default=None,
        spec={
            "name": "matches",
            "number": 1,
            "type": "rift.adapter.Matches",
            "description": "A batch of matches.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )
    summary: MatchSummary | None = proto_field(
        default=None,
        spec={
            "name": "summary",
            "number": 2,
            "type": "rift.adapter.MatchSummary",
            "description": "No more matches.",
            "repeated": False,
            "optional": False,
            "oneof": "event",
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "Matches",
        "parent": None,
        "description": None,
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Matching",
    }
)
class Matches(ProtoModel):
    items: list[StructuralMatch] = proto_field(
        default=...,
        spec={
            "name": "items",
            "number": 1,
            "type": "rift.adapter.StructuralMatch",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "StructuralMatch",
        "parent": None,
        "description": "One match, as the adapter found it. It carries no identity of its own: a\n"
        " `MatchKey` is the snapshot, the query and the span, and Rift already holds\n"
        " the first two, so the span below completes it.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Matching",
    }
)
class StructuralMatch(ProtoModel):
    """One match, as the adapter found it. It carries no identity of its own: a
    `MatchKey` is the snapshot, the query and the span, and Rift already holds
    the first two, so the span below completes it."""

    span: core.SourceSpan = proto_field(
        default=...,
        spec={
            "name": "span",
            "number": 1,
            "type": "rift.core.SourceSpan",
            "description": "The file and byte range the pattern matched.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    replacement_ranges: core.StructuralMatchRanges = proto_field(
        default=...,
        spec={
            "name": "replacement_ranges",
            "number": 2,
            "type": "rift.core.StructuralMatchRanges",
            "description": "The ranges around the matched node that are safe to replace.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    captures: list[core.Capture] = proto_field(
        default=...,
        spec={
            "name": "captures",
            "number": 3,
            "type": "rift.core.Capture",
            "description": "What each named part of the pattern bound to.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    explanation: list[str] = proto_field(
        default=...,
        spec={
            "name": "explanation",
            "number": 4,
            "type": "string",
            "description": "Why this is a match, one step per line.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "MatchSummary",
        "parent": None,
        "description": "Closes a match stream.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Matching",
    }
)
class MatchSummary(ProtoModel):
    """Closes a match stream."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "The state every match was found against.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    coverage: core.Coverage = proto_field(
        default=...,
        spec={
            "name": "coverage",
            "number": 2,
            "type": "rift.core.Coverage",
            "description": "Matches sort by unit UTF-8 bytes, range start, range end, then RFC 8785\n"
            " bytes of their replacement ranges and captures.\n"
            " How much of the workspace the pattern actually reached.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "ActionsRequest",
        "parent": None,
        "description": "Ask which action descriptors and tokens apply at one address. Resolve computes\n"
        " edits after the caller selects one token.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Actions",
    }
)
class ActionsRequest(ProtoModel):
    """Ask which action descriptors and tokens apply at one address. Resolve computes
    edits after the caller selects one token."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "The state to ask against. The answer is only valid against it.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    target: core.Address = proto_field(
        default=...,
        spec={
            "name": "target",
            "number": 2,
            "type": "rift.core.Address",
            "description": "Where in the code to ask: a symbol, a leaf, a byte range, or a match.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    only: list[str] = proto_field(
        default=...,
        spec={
            "name": "only",
            "number": 3,
            "type": "string",
            "description": "Hierarchical kind prefixes. Empty requests every action.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "ActionsResponse",
        "parent": None,
        "description": "The actions on offer at one address.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Actions",
    }
)
class ActionsResponse(ProtoModel):
    """The actions on offer at one address."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "The state they were discovered against.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    actions: list[ActionOffer] = proto_field(
        default=...,
        spec={
            "name": "actions",
            "number": 2,
            "type": "rift.adapter.ActionOffer",
            "description": "What the compiler can do here, each with the token that resolves it.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    coverage: core.Coverage = proto_field(
        default=...,
        spec={
            "name": "coverage",
            "number": 3,
            "type": "rift.core.Coverage",
            "description": "Coverage of action discovery at this address.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "ActionOffer",
        "parent": None,
        "description": "An adapter action token and its portable descriptor. Rift combines the token,\n"
        " Capabilities.language, and ActionsResponse.state to build an ActionKey. The\n"
        " descriptor crosses into MCP unchanged.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Actions",
    }
)
class ActionOffer(ProtoModel):
    """An adapter action token and its portable descriptor. Rift combines the token,
    Capabilities.language, and ActionsResponse.state to build an ActionKey. The
    descriptor crosses into MCP unchanged."""

    token: str = proto_field(
        default=...,
        spec={
            "name": "token",
            "number": 1,
            "type": "string",
            "description": "The adapter's own handle on this action. Tokens are unique within one\n"
            " WorkspaceState. Rift passes the selected token back unread.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    descriptor: core.ActionDescriptor = proto_field(
        default=...,
        spec={
            "name": "descriptor",
            "number": 2,
            "type": "rift.core.ActionDescriptor",
            "description": "What the action is, in terms the model shares with MCP.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "ResolveRequest",
        "parent": None,
        "description": "Compute what one action would change.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Actions",
    }
)
class ResolveRequest(ProtoModel):
    """Compute what one action would change."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "The state the token was discovered in. A token resolved against anything\n"
            " else comes back as StaleState, because the offsets it computed no longer\n"
            " point where they did.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    token: str = proto_field(
        default=...,
        spec={
            "name": "token",
            "number": 2,
            "type": "string",
            "description": "The token from the `ActionOffer` being resolved.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    arguments: str = proto_field(
        default=...,
        spec={
            "name": "arguments",
            "number": 3,
            "type": "string",
            "description": "Arguments, as RFC 8785 canonical JSON, validated against the descriptor's\n"
            " arguments_schema before the call.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "ResolveResponse",
        "parent": None,
        "description": "What the action changes. Rift applies these to the worktree; the adapter's\n"
        " own tree is untouched and will next hear about the change through Refresh.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Actions",
    }
)
class ResolveResponse(ProtoModel):
    """What the action changes. Rift applies these to the worktree; the adapter's
    own tree is untouched and will next hear about the change through Refresh."""

    state: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "state",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "description": "The state whose bytes the edit offsets address.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    edits: list[core.Edit] = proto_field(
        default=...,
        spec={
            "name": "edits",
            "number": 2,
            "type": "rift.core.Edit",
            "description": "The change itself, as byte ranges and their replacements.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    coverage: core.Coverage = proto_field(
        default=...,
        spec={
            "name": "coverage",
            "number": 3,
            "type": "rift.core.Coverage",
            "description": "Whether the edits are the whole change. Partial means the compiler could\n"
            " only rewrite part of what the action names — a rename it cannot follow into\n"
            " a macro, say — and the diagnostics say where it stopped.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    diagnostics: list[core.Diagnostic] = proto_field(
        default=...,
        spec={
            "name": "diagnostics",
            "number": 4,
            "type": "rift.core.Diagnostic",
            "description": "What the compiler wants the caller to know before applying this.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    preconditions: list[core.OperationPrecondition] = proto_field(
        default=...,
        spec={
            "name": "preconditions",
            "number": 5,
            "type": "rift.core.OperationPrecondition",
            "description": "Conditions the compiler checked while resolving. Every entry is satisfied;\n"
            " a failed condition is returned in Refusal instead.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    effects: list[core.OperationEffect] = proto_field(
        default=...,
        spec={
            "name": "effects",
            "number": 6,
            "type": "rift.core.OperationEffect",
            "description": "Semantic consequences of the edits. Rift refreshes the adapter and\n"
            " reconstructs candidate-state addresses before retaining the preview.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    guarantees: list[core.GuaranteeEvidence] = proto_field(
        default=...,
        spec={
            "name": "guarantees",
            "number": 7,
            "type": "rift.core.GuaranteeEvidence",
            "description": "Scoped evidence for every guarantee advertised by the action descriptor.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_enum(
    {
        "package": "rift.adapter",
        "name": "Reason",
        "parent": "rift.adapter.Refusal",
        "description": "Why the adapter declined. The value is what a caller acts on: some of these\n"
        " are worth retrying after a Refresh, and the rest never are.",
        "allow_alias": False,
        "reserved_numbers": [],
        "reserved_names": [],
        "values": [
            {
                "name": "UNSPECIFIED",
                "number": 0,
                "description": "The adapter did not classify it. Treated as permanent.",
                "deprecated": False,
            },
            {
                "name": "UNSUPPORTED",
                "number": 1,
                "description": "The operation has no implementation for this language. Retrying is futile.",
                "deprecated": False,
            },
            {
                "name": "UNMET_PRECONDITION",
                "number": 2,
                "description": "The code has to change first — a file must compile before this can run.",
                "deprecated": False,
            },
            {
                "name": "AMBIGUOUS_TARGET",
                "number": 3,
                "description": "The address resolves to several targets. The caller narrows it and asks\n again.",
                "deprecated": False,
            },
            {
                "name": "STALE_ACTION",
                "number": 4,
                "description": "The action was discovered against a state the workspace has left.\n"
                " Rediscover it and resolve the new token.",
                "deprecated": False,
            },
            {
                "name": "STALE_MATCH",
                "number": 5,
                "description": "The match was found against a state the workspace has left. Search again.",
                "deprecated": False,
            },
            {
                "name": "UNSAFE_EFFECT",
                "number": 6,
                "description": "Doing it would reach further than the caller can have meant — outside the\n"
                " workspace, or into generated code nothing regenerates.",
                "deprecated": False,
            },
            {
                "name": "LANGUAGE_REFUSAL",
                "number": 7,
                "description": "The language itself forbids it: a rename to a reserved word, a visibility\n"
                " change its rules do not allow.",
                "deprecated": False,
            },
        ],
    }
)
class RefusalReason(IntEnum):
    """Why the adapter declined. The value is what a caller acts on: some of these
    are worth retrying after a Refresh, and the rest never are."""

    UNSPECIFIED = 0
    UNSUPPORTED = 1
    UNMET_PRECONDITION = 2
    AMBIGUOUS_TARGET = 3
    STALE_ACTION = 4
    STALE_MATCH = 5
    UNSAFE_EFFECT = 6
    LANGUAGE_REFUSAL = 7


@proto_message(
    {
        "package": "rift.adapter",
        "name": "Refusal",
        "parent": None,
        "description": "Carried in `google.rpc.Status.details` when an adapter declines work it\n"
        " understood: the target is ambiguous, an action went stale, the language\n"
        " forbids it. Everything else is an ordinary status code.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Failure",
    }
)
class Refusal(ProtoModel):
    """Carried in `google.rpc.Status.details` when an adapter declines work it
    understood: the target is ambiguous, an action went stale, the language
    forbids it. Everything else is an ordinary status code."""

    reason: RefusalReason = proto_field(
        default=...,
        spec={
            "name": "reason",
            "number": 1,
            "type": "rift.adapter.Refusal.Reason",
            "description": "Which class of refusal this is, so a caller can decide without parsing\n `message`.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    message: str = proto_field(
        default=...,
        spec={
            "name": "message",
            "number": 2,
            "type": "string",
            "description": "One line for a human, saying what specifically was wrong.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    blockers: list[core.OperationBlocker] = proto_field(
        default=...,
        spec={
            "name": "blockers",
            "number": 3,
            "type": "rift.core.OperationBlocker",
            "description": "What in the code stopped it.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    diagnostics: list[core.Diagnostic] = proto_field(
        default=...,
        spec={
            "name": "diagnostics",
            "number": 4,
            "type": "rift.core.Diagnostic",
            "description": "What the compiler reported while trying.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )
    preconditions: list[core.OperationPrecondition] = proto_field(
        default=...,
        spec={
            "name": "preconditions",
            "number": 5,
            "type": "rift.core.OperationPrecondition",
            "description": "Executable conditions checked before refusal, including failed values.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.adapter",
        "name": "StaleState",
        "parent": None,
        "description": "Carried in `google.rpc.Status.details` when a pinned call names a state the\n"
        " workspace has moved past. The caller resyncs to `actual` before retrying.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": "Failure",
    }
)
class StaleState(ProtoModel):
    """Carried in `google.rpc.Status.details` when a pinned call names a state the
    workspace has moved past. The caller resyncs to `actual` before retrying."""

    actual: WorkspaceState = proto_field(
        default=...,
        spec={
            "name": "actual",
            "number": 1,
            "type": "rift.adapter.WorkspaceState",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


ADAPTER_PACKAGE = ProtoPackage(
    spec={
        "path": "rift/adapter.proto",
        "package": "rift.adapter",
        "description": None,
        "imports": [
            "rift/core.proto",
            "google/protobuf/descriptor.proto",
            "google/protobuf/empty.proto",
        ],
        "options": {},
        "section_option": True,
    },
    models=(
        Capabilities,
        SourceClaim,
        AuxiliaryClaim,
        VirtualClaim,
        WriteClaim,
        KindDescriptor,
        SymbolFacets,
        LeafFacets,
        RelationshipFacets,
        ExtensionDescriptor,
        AdapterLimits,
        RuntimeRequirements,
        WorkspaceState,
        OpenWorkspaceRequest,
        RefreshRequest,
        VirtualSyncEvent,
        CloseWorkspaceRequest,
        AnalyzeRequest,
        AnalyzeEvent,
        SourceUnit,
        VirtualUnit,
        VirtualText,
        Facts,
        OriginMappings,
        Symbols,
        Leaves,
        Relationships,
        Diagnostics,
        UnitCoverage,
        AnalyzeSummary,
        ValidateRequest,
        ValidateEvent,
        ValidationSummary,
        FormatRequest,
        FormatResponse,
        MatchRequest,
        MatchEvent,
        Matches,
        StructuralMatch,
        MatchSummary,
        ActionsRequest,
        ActionsResponse,
        ActionOffer,
        ResolveRequest,
        ResolveResponse,
        Refusal,
        StaleState,
    ),
    enums=(),
    services=[
        {
            "name": "Adapter",
            "description": "Everything Rift can ask one language for. The adapter answers `Describe`\n"
            " once, then holds workspaces and answers questions about the code in them.",
            "rpcs": [
                {
                    "name": "Describe",
                    "request": "google.protobuf.Empty",
                    "response": "rift.adapter.Capabilities",
                    "description": "What this adapter can do, asked once before anything else. Rift holds the\n"
                    " adapter to exactly what it claims here and never probes or feature-tests.",
                    "request_stream": False,
                    "response_stream": False,
                },
                {
                    "name": "OpenWorkspace",
                    "request": "rift.adapter.OpenWorkspaceRequest",
                    "response": "rift.adapter.WorkspaceState",
                    "description": "Here is a worktree and here is the state it holds. The tree exists before\n"
                    " this call, so the adapter can start its compiler against something complete\n"
                    " the moment it is named.",
                    "request_stream": False,
                    "response_stream": False,
                },
                {
                    "name": "Refresh",
                    "request": "rift.adapter.RefreshRequest",
                    "response": "rift.adapter.WorkspaceState",
                    "description": "The files on disk moved to a new state, and these are the paths that\n"
                    " changed. Without this the adapter would have to stat the tree to find out,\n"
                    " and a fresh workspace per edit would make every keystroke a cold compile.",
                    "request_stream": False,
                    "response_stream": False,
                },
                {
                    "name": "SyncVirtual",
                    "request": "rift.adapter.VirtualSyncEvent",
                    "response": "rift.adapter.WorkspaceState",
                    "description": "Replace this adapter workspace's private virtual-source overlay. The first\n"
                    " event pins the current workspace state; the remaining events carry a\n"
                    " complete manifest and its bytes. Rift drains calls for this adapter\n"
                    " workspace before the stream and blocks new calls until completion. Stream\n"
                    " completion commits the overlay atomically.",
                    "request_stream": True,
                    "response_stream": False,
                },
                {
                    "name": "CloseWorkspace",
                    "request": "rift.adapter.CloseWorkspaceRequest",
                    "response": "google.protobuf.Empty",
                    "description": "Stop holding this workspace, and let Rift delete the tree. Refused while\n"
                    " calls are in flight so their compiler state and source path remain valid\n"
                    " through the final stream event.",
                    "request_stream": False,
                    "response_stream": False,
                },
                {
                    "name": "Analyze",
                    "request": "rift.adapter.AnalyzeRequest",
                    "response": "rift.adapter.AnalyzeEvent",
                    "description": "Read the code and say what is in it: symbols, where they appear, how they\n"
                    " connect, and what the compiler complains about. Facts stream per file and\n"
                    " the summary closes it.",
                    "request_stream": False,
                    "response_stream": True,
                },
                {
                    "name": "Validate",
                    "request": "rift.adapter.ValidateRequest",
                    "response": "rift.adapter.ValidateEvent",
                    "description": "Check a candidate after Rift has written it and refreshed the workspace.\n"
                    " The stream carries diagnostics followed by one verdict over the affected\n"
                    " compiler closure.",
                    "request_stream": False,
                    "response_stream": True,
                },
                {
                    "name": "Format",
                    "request": "rift.adapter.FormatRequest",
                    "response": "rift.adapter.FormatResponse",
                    "description": "Format the files or syntactic regions touched by a candidate. Rift applies\n"
                    " the returned text edits and refreshes the workspace before the next change.",
                    "request_stream": False,
                    "response_stream": False,
                },
                {
                    "name": "Match",
                    "request": "rift.adapter.MatchRequest",
                    "response": "rift.adapter.MatchEvent",
                    "description": "Find code by compiler-parsed shape: a call with two arguments, or a class\n"
                    " implementing an interface.",
                    "request_stream": False,
                    "response_stream": True,
                },
                {
                    "name": "Actions",
                    "request": "rift.adapter.ActionsRequest",
                    "response": "rift.adapter.ActionsResponse",
                    "description": "List the fixes and refactors the compiler can offer at one address.",
                    "request_stream": False,
                    "response_stream": False,
                },
                {
                    "name": "Resolve",
                    "request": "rift.adapter.ResolveRequest",
                    "response": "rift.adapter.ResolveResponse",
                    "description": "Work out what one of those actions would actually change. Separate from\n"
                    " `Actions` because computing an action's edits costs about as much as\n"
                    " performing it, and only one of the actions offered gets picked.",
                    "request_stream": False,
                    "response_stream": False,
                },
            ],
        }
    ],
)
