from __future__ import annotations

from pydantic import model_validator

from . import core
from .base import *


@proto_message(
    DirectMessage(
        ADAPTER,
        description="One generated adapter wire contract supported by the process.",
        section="Capabilities",
    )
)
class AdapterContract(ProtoModel):
    major: Field[core.ProtocolVersion] = proto_field(
        default=..., number=1, description="Breaking adapter protocol generation."
    )
    minor: Field[int] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
        description="Additive revision within the selected major.",
    )
    schema_digest: Field[core.Digest] = proto_field(
        default=...,
        number=3,
        description="SHA-256 of the generated adapter descriptor.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Adapter capabilities read once during startup.",
        reserved_numbers=(1,),
        section="Capabilities",
    )
)
class Capabilities(ProtoModel):
    """Adapter capabilities read once during startup."""

    language: Field[core.Language] = proto_field(
        default=...,
        number=2,
        description=(
            "Language name and optional dialect handled by this configured adapter process. "
            "A workspace configures at most one process for the exact pair; that process can "
            "hold many adapter states."
        ),
    )
    implementation: Field[str] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="The adapter build, as it names itself.",
    )
    source_claims: Field[list[SourceClaim]] = proto_field(
        default=...,
        number=4,
        description="Physical source files this adapter owns and parses as `language`.",
    )
    auxiliary_claims: Field[list[AuxiliaryClaim]] = proto_field(
        default=...,
        number=5,
        description=(
            "Files this adapter reads as configuration or dependency metadata —\n "
            "`Cargo.toml`, `tsconfig.json`. Several adapters may claim the same one."
        ),
    )
    write_claims: Field[list[WriteClaim]] = proto_field(
        default=...,
        number=6,
        description="Paths inside the workspace this adapter writes while operating.",
    )
    kinds: Field[list[KindDescriptor]] = proto_field(
        default=...,
        number=7,
        description=(
            "The exact kinds this adapter emits, and the portable facets each carries. This "
            "is the vocabulary Rift filters against: a facet nobody advertises is one no "
            "answer can be negated on."
        ),
    )
    operations: Field[list[core.AdapterOperation]] = proto_field(
        default=...,
        number=8,
        description="Optional RPC operations this adapter implements, sorted by enum number.",
    )
    fact_families: Field[list[core.FactFamily]] = proto_field(
        default=...,
        number=9,
        description="Fact families Analyze may emit, sorted by enum number. Non-empty requires the `analyze` operation.",
    )
    action_kinds: Field[list[core.ActionSupport]] = proto_field(
        default=...,
        number=10,
        description=(
            "Action families available for planning before a target-specific Actions call. "
            "Non-empty requires `actions` and `resolve`; entries sort by kind prefix."
        ),
    )
    extension_values: Field[list[ExtensionDescriptor]] = proto_field(
        default=...,
        number=11,
        description="Versioned extension values this adapter emits, and where each may appear.",
    )
    limits: Field[AdapterLimits] = proto_field(
        default=...,
        number=12,
        description="Bounds Rift must stay inside when driving this adapter.",
    )
    runtime: Field[RuntimeRequirements] = proto_field(
        default=...,
        number=13,
        description="Execution behavior the host should account for while this adapter runs.",
    )
    contracts: Field[list[AdapterContract]] = proto_field(
        default=...,
        number=14,
        description="Supported generated contracts in adapter preference order.",
    )

    @model_validator(mode="after")
    def operation_sets_are_consistent(self) -> Capabilities:
        operations = set(self.operations)
        if len(operations) != len(self.operations):
            raise ValueError("Capabilities.operations must not contain duplicates")
        if self.fact_families and core.AdapterOperation.ANALYZE not in operations:
            raise ValueError("Capabilities.fact_families requires analyze")
        if self.action_kinds and not {
            core.AdapterOperation.ACTIONS,
            core.AdapterOperation.RESOLVE,
        }.issubset(operations):
            raise ValueError("Capabilities.action_kinds requires actions and resolve")
        debug = {
            core.AdapterOperation.DEBUG_START,
            core.AdapterOperation.DEBUG_GET_FRAME,
            core.AdapterOperation.DEBUG_STOP,
        }
        if operations.intersection(debug) and not debug.issubset(operations):
            raise ValueError("Capabilities debugging operations are all-or-none")
        if not self.contracts:
            raise ValueError("Capabilities.contracts must not be empty")
        return self


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "A deterministic claim on physical files, re-evaluated after every path or\n "
            "content change. Selectors are ORed and compare UTF-8 bytes: suffixes against\n "
            "the full path, names against the final component, shebang prefixes against\n the "
            "first line. Highest priority wins. Equal priority from two adapters is an\n "
            "ownership error."
        ),
        section="Capabilities",
    )
)
class SourceClaim(ProtoModel):
    """A deterministic claim on physical files, re-evaluated after every path or
    content change. Selectors are ORed and compare UTF-8 bytes: suffixes against
    the full path, names against the final component, shebang prefixes against
    the first line. Highest priority wins. Equal priority from two adapters is an
    ownership error."""

    priority: Field[int] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
        description="Which adapter wins where two claim the same path. Higher takes it.",
    )
    path_suffixes: Field[list[str]] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Suffixes matched against the whole path, so `.d.ts` can be claimed apart\n from `.ts`.",
    )
    path_names: Field[list[str]] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Whole final path components, for files whose name carries the meaning —\n "
            "`Makefile`, `Dockerfile`."
        ),
    )
    shebang_prefixes: Field[list[str]] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="First-line prefixes for extensionless scripts: `#!/usr/bin/env python`.",
    )
    container: Field[bool] = proto_field(
        default=...,
        number=5,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description=(
            "Whether a claimed file may contain other languages. A Svelte owner sets\n this "
            "for an embedded TypeScript region, then publishes that region as a\n virtual "
            "source when the TypeScript adapter has to analyze it."
        ),
    )


@proto_enum(
    DirectEnum(
        ADAPTER,
        name="Purpose",
        parent=lambda: AuxiliaryClaim,
        description=(
            "What an auxiliary file is to the adapter that reads it. This is what the\n file "
            "means, and it is what Rift acts on: a workspace manifest changing can\n add or "
            "remove whole source trees, which Rift has to rescan for. What the\n adapter has "
            "to redo is `environment_defining`."
        ),
        value_descriptions=(
            (
                "The adapter did not say. Treated as the widest purpose, so a change to\n the file "
                "invalidates everything derived from it."
            ),
            "Settings the adapter reads directly — `tsconfig.json`, `.rustfmt.toml`.",
            "Declared dependencies, before resolution — `package.json`, `Cargo.toml`.",
            (
                "Resolved dependency versions — `Cargo.lock`, `bun.lock`. Editing one can\n change "
                "every type the adapter resolves."
            ),
            (
                "Which packages make up the project — a Cargo workspace, an npm workspace.\n A "
                "change here can add or remove whole source trees."
            ),
            "Metadata a build step wrote for the adapter to read back.",
            "Something the adapter needs that none of the above describes. The claim\n states its purpose.",
        ),
    )
)
class AuxiliaryClaimPurpose(IntEnum):
    """What an auxiliary file is to the adapter that reads it. This is what the
    file means, and it is what Rift acts on: a workspace manifest changing can
    add or remove whole source trees, which Rift has to rescan for. What the
    adapter has to redo is `environment_defining`."""

    UNSPECIFIED = 0
    ADAPTER_CONFIGURATION = 1
    DEPENDENCY_MANIFEST = 2
    DEPENDENCY_LOCK = 3
    WORKSPACE_MANIFEST = 4
    GENERATED_METADATA = 5
    OTHER = 6


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "A non-owning claim on project files needed to discover or configure adapter\n "
            "semantics. Same bytewise rules as SourceClaim, may overlap across adapters,\n and "
            "never makes a file a source unit."
        ),
        section="Capabilities",
    )
)
class AuxiliaryClaim(ProtoModel):
    """A non-owning claim on project files needed to discover or configure adapter
    semantics. Same bytewise rules as SourceClaim, may overlap across adapters,
    and never makes a file a source unit."""

    purposes: Field[list[AuxiliaryClaimPurpose]] = proto_field(
        default=...,
        number=1,
        description="What the file is to the adapter, and so what Rift itself has to redo when\n it changes.",
    )
    path_suffixes: Field[list[str]] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Suffixes matched against the whole path.",
    )
    path_names: Field[list[str]] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Whole final path components — `Cargo.toml`, `package.json`.",
    )
    environment_defining: Field[bool] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description=(
            "Whether a Refresh touching this file forces the adapter to rebuild its\n resolved "
            "state, including reselecting its project runtime when necessary. Declared and\n "
            "never inferred from purpose or path, because only the adapter knows whether it\n "
            "rereads a given file."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Paths inside the workspace that this adapter writes to. Rift\n classifies each "
            "claimed subtree as adapter output and excludes it from\n source analysis and agent results.\n "
            "Write claims are exclusive across adapters "
            "sharing a connection projection. Rift\n refuses workspace admission when two "
            "adapters can write the same path; one\n of them must redirect that output to its "
            "private state_root.\n\n CARGO_TARGET_DIR, GOCACHE, PYTHONPYCACHEPREFIX, and "
            "tsBuildInfoFile can point\n at `state_root`. Declare generated output that must remain "
            "beside source, such as npm's `node_modules`; an already tracked source path remains "
            "source even when a claim matches it.\n\n Selectors follow "
            "SourceClaim's bytewise rules. A directory name matches the\n directory and "
            "everything beneath it."
        ),
        section="Capabilities",
    )
)
class WriteClaim(ProtoModel):
    """Paths inside the workspace that this adapter writes to. Rift
    classifies each claimed subtree as adapter output and excludes it from
    source analysis and agent results.
    Write claims are exclusive across adapters sharing a connection projection. Rift
    refuses workspace admission when two adapters can write the same path; one
    of them must redirect that output to its private state_root.

    CARGO_TARGET_DIR, GOCACHE, PYTHONPYCACHEPREFIX, and tsBuildInfoFile can point
    at `state_root`. Declare generated output that must remain beside source, such as npm's
    `node_modules`; an already tracked source path remains source even when a claim matches it.

    Selectors follow SourceClaim's bytewise rules. A directory name matches the
    directory and everything beneath it."""

    path_names: Field[list[str]] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Whole final path components. A directory named here takes its whole tree\n with "
            "it — `node_modules`, `target`."
        ),
    )
    path_suffixes: Field[list[str]] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Suffixes matched against the whole path, covering generated files in\n several directories.",
    )
    reason: Field[str] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Why the adapter needs it, in one line. Rift logs this when a claim shadows\n a "
            "workspace file, which is nearly always a mistake."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "One kind this adapter emits, and the portable facets it carries. Which facet\n "
            "vocabulary applies depends on what the kind describes — a symbol, a node or\n a "
            "relationship — so the branch that is set is what says which."
        ),
        section="Capabilities",
    )
)
class KindDescriptor(ProtoModel):
    """One kind this adapter emits, and the portable facets it carries. Which facet
    vocabulary applies depends on what the kind describes — a symbol, a node or
    a relationship — so the branch that is set is what says which.

    Some facets separate two symbols the language spells with one kind: a `final`
    class and an open one are both `class`, and a `#[test]` function is a
    `fn_item` like any other. Listing `extensible` or `test` here says the adapter
    decides it per symbol, so this is the set a kind may carry rather than the set
    every symbol of that kind does."""

    kind: Field[core.ExactKind] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="The adapter-local kind name, such as `trait`, `mapping.key`, or `selector.class`.",
    )
    symbol: Field[SymbolFacets | None] = proto_field(
        default=None,
        number=2,
        oneof=Oneof("facets"),
        description="Facets carried by symbols of this kind.",
    )
    node: Field[NodeFacets | None] = proto_field(
        default=None,
        number=3,
        oneof=Oneof("facets"),
        description="Facets carried by nodes of this kind.",
    )
    relationship: Field[RelationshipFacets | None] = proto_field(
        default=None,
        number=4,
        oneof=Oneof("facets"),
        description="Facets carried by relationships of this kind.",
    )


@proto_message(DirectMessage(ADAPTER, section="Capabilities"))
class SymbolFacets(ProtoModel):
    values: Field[list[core.SymbolFacet]] = proto_field(default=..., number=1)


@proto_message(DirectMessage(ADAPTER, section="Capabilities"))
class NodeFacets(ProtoModel):
    values: Field[list[core.NodeFacet]] = proto_field(default=..., number=1)


@proto_message(DirectMessage(ADAPTER, section="Capabilities"))
class RelationshipFacets(ProtoModel):
    values: Field[list[core.RelationshipFacet]] = proto_field(default=..., number=1)


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "One versioned extension value, and every place it may appear. Rift validates\n "
            "each occurrence against the schema."
        ),
        section="Capabilities",
    )
)
class ExtensionDescriptor(ProtoModel):
    """One versioned extension value, and every place it may appear. Rift validates
    each occurrence against the schema."""

    key: Field[str] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Reverse-domain namespaced key.",
    )
    version: Field[int] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
        description="Bumped whenever the schema changes shape. A consumer skips versions it does\n not implement.",
    )
    targets: Field[list[str]] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Which model types this value may be attached to — `Symbol`, `Node`.",
    )
    schema_: Field[str] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "A self-contained JSON Schema draft 2020-12 document. Remote references are\n "
            "forbidden, so validation fetches nothing."
        ),
    )
    indexed: Field[bool] = proto_field(
        default=...,
        number=5,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description=(
            "Whether Rift builds a search index over this value. Each indexed fact costs\n "
            "storage, so the adapter declares this explicitly."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Bounds this adapter advertises. One process holds state for several "
            "server-owned\n projections, so each state receives its own in-flight limit."
        ),
        section="Capabilities",
    )
)
class AdapterLimits(ProtoModel):
    """Bounds this adapter advertises. One process holds state for several server-owned
    projections, so each state receives its own in-flight limit."""

    max_message_bytes: Field[int] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_UINT64,
        description="Largest single message this adapter will accept or send. A fact batch is\n split to stay under it.",
    )
    max_in_flight: Field[int] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
        description="Concurrent calls accepted across the whole connection.",
    )
    max_in_flight_per_state: Field[int] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
        description="Concurrent calls accepted for one adapter state, so one caller cannot consume the process.",
    )
    max_states: Field[int] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
        description=(
            "Server-owned projection states this adapter will hold at once. Each can retain a "
            "parsed tree, index, and runtime worker, so the bound controls resource use."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Execution behavior reported by an adapter process for server admission and diagnostics.\n "
            "Rift does not isolate the process or its workers from the host. These requirements\n "
            "do not select the project runtime used for one adapter state."
        ),
        section="Capabilities",
    )
)
class RuntimeRequirements(ProtoModel):
    """Execution behavior reported by an adapter process for server admission and diagnostics.
    Rift does not isolate the process or its workers from the host. These requirements
    do not select the project runtime used for one adapter state."""

    executes_workspace_code: Field[bool] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description="Whether the adapter executes workspace code, such as a build script, plugin, or macro.",
    )
    spawns_subprocesses: Field[bool] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description=(
            "Whether the adapter spawns subprocesses, including runtime workers selected "
            "for individual adapter states."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "One adapter's state for a Rift FS projection. Every call that reads code\n"
            " carries it. The adapter may back different states with different project\n"
            " runtimes. A call naming an earlier state receives `StaleState`."
        ),
        section="Adapter state",
    )
)
class AdapterState(ProtoModel):
    """One adapter's state for a Rift FS projection. Every call that reads code
    carries it. The adapter may back different states with different project runtimes.
    A call naming an earlier state receives `StaleState`."""

    generation: Field[int] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_UINT64,
        description=(
            "Adapter-local state generation, and the whole of an adapter state's identity.\n "
            "Open mints the first value; Refresh and SyncVirtual advance it. A call carrying\n "
            "an earlier generation receives StaleState."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Open one Rift FS projection in the adapter. The adapter\n"
            " may inspect its files and select or start the project runtime for this state."
        ),
        section="Adapter state",
    )
)
class OpenRequest(ProtoModel):
    """Open one Rift FS projection in the adapter."""

    state_root: Field[str] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Absolute path to a directory the adapter owns and Rift never reads. Build\n "
            "output belongs here — point CARGO_TARGET_DIR and friends at it. Every open\n "
            "adapter state receives a distinct path, and Rift never reuses that path after\n "
            "Close. An adapter may maintain its own shared read-only cache\n outside this "
            "directory when its language toolchain supports safe sharing."
        ),
    )
    contract: Field[AdapterContract] = proto_field(
        default=...,
        number=4,
        description="Exact contract selected from the adapter's `Capabilities.contracts`.",
    )
    root: Field[str] = proto_field(
        default=...,
        number=5,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Absolute Rift FS path for `projection`. Other adapters can receive the same path. "
            "Adapter output goes to `state_root` or a declared WriteClaim scratch subtree."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Rift changed files in a projection. It sends this call to every language\n"
            " adapter holding the shared path. Paths are\n "
            "project-relative and cover every file Rift touched; each adapter decides\n which "
            "changes reach its own analysis. Content is on disk before this call. The adapter\n "
            "reselects its project runtime when runtime-defining inputs have changed."
        ),
        section="Adapter state",
    )
)
class RefreshRequest(ProtoModel):
    """Updates an adapter after files in its projection change."""

    from_: Field[AdapterState] = proto_field(
        default=...,
        number=1,
        description=(
            "The state the adapter last acknowledged. A mismatch means Rift and the\n adapter "
            "disagree about what is on disk, and comes back as StaleState."
        ),
    )
    written: Field[list[str]] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Files created or overwritten since `from`, as `rift://file/<path>`.",
    )
    removed: Field[list[str]] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Files deleted since `from`. A rename arrives as one of each.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "One atomic replacement of the virtual sources consumed by an adapter. Rift\n "
            "sends exactly one `start` event first, then each `unit` followed by ordered\n "
            "`text` chunks ending in `final: true`. The complete stream replaces the\n "
            "previous overlay; an RPC failure leaves the previous generation intact. The\n "
            "units are the complete set selected for this consumer at `start`,\n "
            "including an empty set when every previous unit has been retracted."
        ),
        section="Adapter state",
    )
)
class VirtualSyncEvent(ProtoModel):
    """One atomic replacement of the virtual sources consumed by an adapter. Rift
    sends exactly one `start` event first, then each `unit` followed by ordered
    `text` chunks ending in `final: true`. The complete stream replaces the
    previous overlay; an RPC failure leaves the previous generation intact. The
    units are the complete set selected for this consumer at `start`,
    including an empty set when every previous unit has been retracted."""

    start: Field[AdapterState | None] = proto_field(
        default=None,
        number=1,
        oneof=Oneof("event"),
        description=(
            "Current adapter-local state. The returned AdapterState advances its\n "
            "generation."
        ),
    )
    unit: Field[VirtualUnit | None] = proto_field(
        default=None,
        number=2,
        oneof=Oneof("event"),
        description="One virtual file whose exact Language matches this adapter.",
    )
    text: Field[VirtualText | None] = proto_field(
        default=None,
        number=3,
        oneof=Oneof("event"),
        description="Ordered bytes for the preceding unit.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Release one open adapter state.",
        section="Adapter state",
    )
)
class CloseRequest(ProtoModel):
    """Release one open adapter state."""

    state: Field[AdapterState] = proto_field(default=..., number=1)


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Read selected claimed physical or synced virtual sources at one state."
        ),
        section="Analysis",
    )
)
class AnalyzeRequest(ProtoModel):
    """Read selected physical or virtual sources at one adapter state."""

    state: Field[AdapterState] = proto_field(default=..., number=1)
    units: Field[list[str]] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Source units requested as `rift://file/<path>`, sorted by URI. Empty selects "
            "every source unit this adapter owns."
        ),
    )
    fact_families: Field[list[core.FactFamily]] = proto_field(
        default=...,
        number=3,
        description="Advertised fact families requested in enum order. Empty selects every advertised family.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description="One event of an analyze stream, followed eventually by exactly one summary.",
        section="Analysis",
    )
)
class AnalyzeEvent(ProtoModel):
    """One event of an analyze stream, followed eventually by exactly one summary."""

    source_unit: Field[SourceUnit | None] = proto_field(
        default=None,
        number=1,
        oneof=Oneof("event"),
        description="A claimed physical or synced virtual unit is about to have facts.",
    )
    virtual_unit: Field[VirtualUnit | None] = proto_field(
        default=None,
        number=2,
        oneof=Oneof("event"),
        description="A file this adapter produced is about to have text and facts.",
    )
    virtual_text: Field[VirtualText | None] = proto_field(
        default=None,
        number=3,
        oneof=Oneof("event"),
        description="A chunk of a produced file's bytes.",
    )
    facts: Field[Facts | None] = proto_field(
        default=None,
        number=4,
        oneof=Oneof("event"),
        description="A batch of facts for one file.",
    )
    coverage: Field[UnitCoverage | None] = proto_field(
        default=None,
        number=5,
        oneof=Oneof("event"),
        description="No more facts for one file.",
    )
    summary: Field[AnalyzeSummary | None] = proto_field(
        default=None,
        number=6,
        oneof=Oneof("event"),
        description="No more events at all.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Metadata for one physical or synced virtual source unit this adapter owns.\n Rift "
            "resolves the URI against the immutable source-store tree or the virtual overlay at\n this "
            "AdapterState generation."
        ),
        section="Analysis",
    )
)
class SourceUnit(ProtoModel):
    """Metadata for one physical or synced virtual source unit this adapter owns.
    Rift resolves the URI against the immutable source-store tree or the virtual overlay at
    this AdapterState generation."""

    unit: Field[str] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "The unit, as `rift://file/<path>`. The same spelling `SourceSpan.unit`\n uses, so "
            "a fact and the unit it was read from compare without translation."
        ),
    )
    regions: Field[list[core.LanguageRegion]] = proto_field(
        default=...,
        number=2,
        description=(
            "Which bytes each language owns. One entry for a file in a single language,\n "
            "several where one embeds another."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Metadata for a file this adapter produced from another one. Rift validates\n the "
            "complete publication and forwards it to the selected consumer through\n "
            "SyncVirtual. The consumer stores it in a private overlay under state_root.\n Rift "
            "exposes the retained bytes as a regular, non-executable File resource."
        ),
        section="Analysis",
    )
)
class VirtualUnit(ProtoModel):
    """Metadata for a file this adapter produced from another one. Rift validates
    the complete publication and forwards it to the selected consumer through
    SyncVirtual. The consumer stores it in a private overlay under state_root.
    Rift exposes the retained bytes as a regular, non-executable File resource."""

    unit: Field[str] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "The produced file, as `rift://file/<path>`. The adapter mints the path;\n "
            "downstream facts use the same identity even though the bytes live in the\n "
            "consumer's private overlay."
        ),
    )
    language: Field[core.Language] = proto_field(
        default=...,
        number=2,
        description=(
            "The exact language that consumes these bytes. Rift routes it to the configured "
            "adapter for this pair."
        ),
    )
    regions: Field[list[core.LanguageRegion]] = proto_field(
        default=...,
        number=3,
        description=(
            "Which bytes each language owns inside the produced file. A consumer echoes\n "
            "these regions when it later emits SourceUnit for the synced path."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "One chunk of a produced file's bytes. Sent in pieces because a generated file\n "
            "can be far larger than the source it came from, and a message has a bound."
        ),
        section="Analysis",
    )
)
class VirtualText(ProtoModel):
    """One chunk of a produced file's bytes. Sent in pieces because a generated file
    can be far larger than the source it came from, and a message has a bound."""

    unit: Field[str] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="The produced file these bytes belong to, spelled as its VirtualUnit did.",
    )
    offset: Field[int] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_UINT64,
        description="Byte offset this chunk starts at. Chunks arrive in order and leave no gaps.",
    )
    text: Field[str] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="The bytes themselves.",
    )
    final: Field[bool] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description="Whether this is the last chunk. Rift holds the file until it arrives.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "One file's worth of one fact family. Rift buffers by file until the coverage\n "
            "event for that file, then routes complete virtual units to their consumer."
        ),
        section="Analysis",
    )
)
class Facts(ProtoModel):
    """One file's worth of one fact family. Rift buffers by file until the coverage
    event for that file, then routes complete virtual units to their consumer."""

    unit: Field[str] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="The file these facts were read from, as `rift://file/<path>`.",
    )
    origin_mappings: Field[OriginMappings | None] = proto_field(
        default=None, number=2, oneof=Oneof("family")
    )
    symbols: Field[Symbols | None] = proto_field(
        default=None, number=3, oneof=Oneof("family")
    )
    nodes: Field[Nodes | None] = proto_field(
        default=None, number=4, oneof=Oneof("family")
    )
    relationships: Field[Relationships | None] = proto_field(
        default=None, number=5, oneof=Oneof("family")
    )
    diagnostics: Field[Diagnostics | None] = proto_field(
        default=None, number=6, oneof=Oneof("family")
    )


@proto_message(DirectMessage(ADAPTER, section="Analysis"))
class OriginMappings(ProtoModel):
    items: Field[list[core.OriginMapping]] = proto_field(default=..., number=1)


@proto_message(DirectMessage(ADAPTER, section="Analysis"))
class Symbols(ProtoModel):
    items: Field[list[core.Symbol]] = proto_field(default=..., number=1)


@proto_message(DirectMessage(ADAPTER, section="Analysis"))
class Nodes(ProtoModel):
    items: Field[list[core.Node]] = proto_field(default=..., number=1)


@proto_message(DirectMessage(ADAPTER, section="Analysis"))
class Relationships(ProtoModel):
    items: Field[list[core.Relationship]] = proto_field(default=..., number=1)


@proto_message(DirectMessage(ADAPTER, section="Analysis"))
class Diagnostics(ProtoModel):
    items: Field[list[core.Diagnostic]] = proto_field(default=..., number=1)


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Closes one file's facts. `present=false` clears everything buffered for it.",
        section="Analysis",
    )
)
class UnitCoverage(ProtoModel):
    """Closes one file's facts. `present=false` clears everything buffered for it."""

    unit: Field[str] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="The file being closed, as `rift://file/<path>`.",
    )
    present: Field[bool] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description="Whether the file exists and was read at all. False retracts it.",
    )
    coverage: Field[core.SemanticCoverage] = proto_field(
        default=...,
        number=3,
        description=(
            "How complete each fact family is for this file. Every family is stated: a\n "
            "family the adapter cannot produce is `unsupported` and one the language has\n no "
            "concept of is `not_applicable`, so there is no silence to interpret."
        ),
    )


@proto_enum(
    DirectEnum(
        ADAPTER,
        description="How long Rift may retain one Analyze result.",
        value_descriptions=(
            "No cache claim. Rift treats the result as request-volatile.",
            "The projection contents, adapter implementation, language, and virtual inputs determine the result.",
            (
                "Adapter-observed external state also affects the result. Retention ends with the "
                "process or adapter-state generation."
            ),
            "The result is request-volatile and is not retained.",
        ),
    )
)
class CacheScope(IntEnum):
    """How long Rift may retain one Analyze result."""

    CACHE_SCOPE_UNSPECIFIED = 0
    PROJECTION = 1
    PROCESS = 2
    NONE = 3


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Completes one ordered Analyze stream and states how long Rift may retain its facts.",
        section="Analysis",
    )
)
class AnalyzeSummary(ProtoModel):
    """Completes one ordered Analyze stream and states how long Rift may retain its facts."""

    state: Field[AdapterState] = proto_field(
        default=...,
        number=1,
        description="The state every fact in this stream was read against.",
    )
    invalidated: Field[list[str]] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Files whose previously published facts no longer hold, as\n `rift://file/<path>`. "
            "This reaches files the stream said nothing about,\n which is what a per-file "
            "`UnitCoverage` cannot do."
        ),
    )
    cache_scope: Field[CacheScope] = proto_field(
        default=...,
        number=3,
        description="Retention scope for every fact in this Analyze stream.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Ask the adapter to validate the closure affected by a candidate change.\n Rift "
            "has applied the change only in the private candidate and acknowledged that state "
            "through Refresh, so the adapter reads the exact files Rift may publish."
        ),
        section="Validation",
    )
)
class ValidateRequest(ProtoModel):
    """Ask the adapter to validate the closure affected by a candidate change.
    Rift has applied the change only in the private candidate and acknowledged that state through
    Refresh, so the adapter reads the exact files Rift may publish."""

    state: Field[AdapterState] = proto_field(
        default=..., number=1, description="Private candidate state being checked."
    )
    changed: Field[list[str]] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Files changed in the candidate projection, as project-relative paths. The adapter\n expands "
            "these to the dependent files its own validity rules require."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Validation diagnostics followed by exactly one summary.",
        section="Validation",
    )
)
class ValidateEvent(ProtoModel):
    """Validation diagnostics followed by exactly one summary."""

    diagnostics: Field[Diagnostics | None] = proto_field(
        default=None,
        number=1,
        oneof=Oneof("event"),
        description="A bounded batch of findings from the adapter.",
    )
    summary: Field[ValidationSummary | None] = proto_field(
        default=None,
        number=2,
        oneof=Oneof("event"),
        description="The final verdict and the scope actually checked.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Closes one validation stream. Rift combines this with the preceding\n diagnostics "
            "and the adapter's language to construct ValidationReport."
        ),
        section="Validation",
    )
)
class ValidationSummary(ProtoModel):
    """Closes one validation stream. Rift combines this with the preceding
    diagnostics and the adapter's language to construct ValidationReport."""

    state: Field[AdapterState] = proto_field(
        default=...,
        number=1,
        description="State every finding and the verdict were computed from.",
    )
    valid: Field[bool] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_BOOL,
        description="Whether the adapter accepted the checked scope.",
    )
    coverage: Field[core.Coverage] = proto_field(
        default=...,
        number=3,
        description="How completely the adapter checked the affected source closure.",
    )
    files: Field[list[str]] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Files included in that closure, as project-relative paths in byte order.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Ask the language formatter for concrete edits over the current session state.\n Rift "
            "skips this RPC for FormattingPolicy.PRESERVE."
        ),
        section="Formatting",
    )
)
class FormatRequest(ProtoModel):
    """Ask the language formatter for concrete edits over the current session state.
    Rift skips this RPC for FormattingPolicy.PRESERVE."""

    state: Field[AdapterState] = proto_field(
        default=...,
        number=1,
        description="Session state whose offsets the response must use.",
    )
    policy: Field[core.FormattingPolicy] = proto_field(
        default=...,
        number=2,
        description="CHANGED_REGIONS or AFFECTED_FILES. PRESERVE is refused because it requires\n no formatter work.",
    )
    spans: Field[list[core.SourceSpan]] = proto_field(
        default=...,
        number=3,
        description=(
            "Text ranges changed by the resolved operation. Required for\n CHANGED_REGIONS and "
            "ignored for AFFECTED_FILES."
        ),
    )
    files: Field[list[str]] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Changed files owned by this adapter. Required for AFFECTED_FILES and used\n to "
            "group spans for CHANGED_REGIONS."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Formatter output over one acknowledged session state.",
        section="Formatting",
    )
)
class FormatResponse(ProtoModel):
    """Formatter output over one acknowledged session state."""

    state: Field[AdapterState] = proto_field(
        default=..., number=1, description="State the formatter read."
    )
    edits: Field[list[core.TextEdit]] = proto_field(
        default=...,
        number=2,
        description="Atomic non-overlapping replacements in canonical file-and-range order.",
    )
    coverage: Field[core.Coverage] = proto_field(
        default=...,
        number=3,
        description="Whether every requested region or file was formatted.",
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        default=...,
        number=4,
        description="Formatter findings that explain partial coverage or a refusal.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Find every place matching one pattern parsed by the language adapter.",
        section="Matching",
    )
)
class MatchRequest(ProtoModel):
    """Find every place matching one pattern parsed by the language adapter."""

    state: Field[AdapterState] = proto_field(
        default=...,
        number=1,
        description="The state to search, and what the answer is pinned to.",
    )
    query: Field[core.StructuralQuery] = proto_field(
        default=...,
        number=2,
        description="The pattern, and the constraints on what its captures may be.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description="One event of a match stream, followed eventually by exactly one summary.",
        section="Matching",
    )
)
class MatchEvent(ProtoModel):
    """One event of a match stream, followed eventually by exactly one summary."""

    matches: Field[Matches | None] = proto_field(
        default=None, number=1, oneof=Oneof("event"), description="A batch of matches."
    )
    summary: Field[MatchSummary | None] = proto_field(
        default=None, number=2, oneof=Oneof("event"), description="No more matches."
    )


@proto_message(DirectMessage(ADAPTER, section="Matching"))
class Matches(ProtoModel):
    items: Field[list[StructuralMatch]] = proto_field(default=..., number=1)


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "One match, as the adapter found it. It carries no identity of its own: a\n "
            "`MatchKey` is the query and the span it matched, and Rift already holds\n the "
            "first, so the span below completes it."
        ),
        section="Matching",
    )
)
class StructuralMatch(ProtoModel):
    """One match, as the adapter found it. It carries no identity of its own: a
    `MatchKey` is the query and the span it matched, and Rift already holds
    the first, so the span below completes it."""

    span: Field[core.SourceSpan] = proto_field(
        default=...,
        number=1,
        description="The file and byte range the pattern matched.",
    )
    replacement_ranges: Field[core.StructuralMatchRanges] = proto_field(
        default=...,
        number=2,
        description="The ranges around the matched node that are safe to replace.",
    )
    captures: Field[list[core.Capture]] = proto_field(
        default=...,
        number=3,
        description="What each named part of the pattern bound to.",
    )
    explanation: Field[list[str]] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Why this is a match, one step per line.",
    )


@proto_message(
    DirectMessage(ADAPTER, description="Closes a match stream.", section="Matching")
)
class MatchSummary(ProtoModel):
    """Closes a match stream."""

    state: Field[AdapterState] = proto_field(
        default=..., number=1, description="The state every match was found against."
    )
    coverage: Field[core.Coverage] = proto_field(
        default=...,
        number=2,
        description=(
            "Matches sort by unit UTF-8 bytes, range start, range end, then RFC 8785\n bytes "
            "of their replacement ranges and captures.\n How much of the workspace the pattern "
            "actually reached."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Ask which action descriptors and tokens apply at one address. Resolve computes\n "
            "edits after the caller selects one token."
        ),
        section="Actions",
    )
)
class ActionsRequest(ProtoModel):
    """Ask which action descriptors and tokens apply at one address. Resolve computes
    edits after the caller selects one token."""

    state: Field[AdapterState] = proto_field(
        default=...,
        number=1,
        description="The state to ask against. The answer is only valid against it.",
    )
    target: Field[core.Address] = proto_field(
        default=...,
        number=2,
        description="Where in the code to ask: a symbol, a node, a byte range, or a match.",
    )
    only: Field[list[str]] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Hierarchical kind prefixes. Empty requests every action.",
    )


@proto_message(
    DirectMessage(
        ADAPTER, description="The actions on offer at one address.", section="Actions"
    )
)
class ActionsResponse(ProtoModel):
    """The actions on offer at one address."""

    state: Field[AdapterState] = proto_field(
        default=..., number=1, description="The state they were discovered against."
    )
    actions: Field[list[ActionOffer]] = proto_field(
        default=...,
        number=2,
        description="What the adapter can do here, each with the token that resolves it.",
    )
    coverage: Field[core.Coverage] = proto_field(
        default=...,
        number=3,
        description="Coverage of action discovery at this address.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "An adapter action token and its portable descriptor. Rift combines the token,\n "
            "Capabilities.language, and ActionsResponse.state to build an ActionKey. The\n "
            "descriptor crosses into MCP unchanged."
        ),
        section="Actions",
    )
)
class ActionOffer(ProtoModel):
    """An adapter action token and its portable descriptor. Rift combines the token,
    Capabilities.language, and ActionsResponse.state to build an ActionKey. The
    descriptor crosses into MCP unchanged."""

    token: Field[str] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "The adapter's own handle on this action. Tokens are unique within one\n "
            "AdapterState. Rift passes the selected token back unread."
        ),
    )
    descriptor: Field[core.ActionDescriptor] = proto_field(
        default=...,
        number=2,
        description="What the action is, in terms the model shares with MCP.",
    )


@proto_message(
    DirectMessage(
        ADAPTER, description="Compute what one action would change.", section="Actions"
    )
)
class ResolveRequest(ProtoModel):
    """Compute what one action would change."""

    state: Field[AdapterState] = proto_field(
        default=...,
        number=1,
        description=(
            "The state the token was discovered in. A token resolved against anything\n else "
            "comes back as StaleState, because the offsets it computed no longer\n point where "
            "they did."
        ),
    )
    token: Field[str] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="The token from the `ActionOffer` being resolved.",
    )
    arguments: Field[str] = proto_field(
        default=...,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Arguments, as RFC 8785 canonical JSON, validated against the descriptor's\n "
            "arguments_schema before the call."
        ),
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "What the action changes. Rift applies these to the projection; the adapter's\n own "
            "tree is untouched and will next hear about the change through Refresh."
        ),
        section="Actions",
    )
)
class ResolveResponse(ProtoModel):
    """What the action changes. Rift applies these to the projection; the adapter's
    own tree is untouched and will next hear about the change through Refresh."""

    state: Field[AdapterState] = proto_field(
        default=...,
        number=1,
        description="The state whose bytes the edit offsets address.",
    )
    edits: Field[list[core.Edit]] = proto_field(
        default=...,
        number=2,
        description="The change itself, as byte ranges and their replacements.",
    )
    coverage: Field[core.Coverage] = proto_field(
        default=...,
        number=3,
        description=(
            "Whether the edits are the whole change. Partial means the adapter could\n only "
            "rewrite part of what the action names — a rename it cannot follow into\n a macro, "
            "say — and the diagnostics say where it stopped."
        ),
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        default=...,
        number=4,
        description="What the adapter wants the caller to know before applying this.",
    )
    preconditions: Field[list[core.OperationPrecondition]] = proto_field(
        default=...,
        number=5,
        description=(
            "Conditions the adapter checked while resolving. Every entry is satisfied;\n a "
            "failed condition is returned in Refusal instead."
        ),
    )
    effects: Field[list[core.OperationEffect]] = proto_field(
        default=...,
        number=6,
        description=(
            "Semantic consequences of the edits. Rift refreshes the adapter and\n reconstructs "
            "resulting-state addresses before publishing the change."
        ),
    )
    guarantees: Field[list[core.GuaranteeEvidence]] = proto_field(
        default=...,
        number=7,
        description="Scoped evidence for every guarantee advertised by the action descriptor.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Evaluate one caller-provided block in the project runtime selected for a pinned "
            "adapter state. Rift gives this call a disposable execution workspace, so evaluated "
            "code never runs in the session projection."
        ),
        section="Execution",
    )
)
class ExecuteRequest(ProtoModel):
    """Evaluate one caller-provided block in the project runtime selected for a pinned adapter
    state."""

    state: Field[AdapterState] = proto_field(
        default=...,
        number=1,
        description="Adapter state to evaluate against.",
    )
    block: Field[core.CodeBlock] = proto_field(
        default=...,
        number=2,
        description="Source and project-relative working directory.",
    )
    budget: Field[core.ExecutionBudget] = proto_field(
        default=...,
        number=3,
        description="Exact workspace-configured bounds the adapter must enforce for this evaluation.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Bounded result of evaluating a block against one unchanged adapter state.",
        section="Execution",
    )
)
class ExecuteResponse(ProtoModel):
    """Bounded result of evaluating a block against one unchanged adapter state."""

    state: Field[AdapterState] = proto_field(
        default=...,
        number=1,
        description="State whose project runtime and source context were used.",
    )
    result: Field[core.ExecutionResult] = proto_field(default=..., number=2)


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Adapter-local handle for an inspect-only debugging evaluation. Start runs until "
            "normal completion or an unhandled failure. A failed evaluation retains its stack "
            "frames until DebugStop or adapter-state cleanup."
        ),
        section="Debugging",
    )
)
class DebugSessionKey(ProtoModel):
    """Minimal adapter-local identity of one debugging session."""

    state: Field[AdapterState] = proto_field(
        default=...,
        number=1,
        description="Pinned adapter state used by the debugging evaluation.",
    )
    token: Field[str] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Opaque adapter-local handle. Rift never exposes it to the MCP caller.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Adapter-local key and terminal evaluation summary for one debugging session.",
        section="Debugging",
    )
)
class DebugSession(ProtoModel):
    """Adapter-local key and terminal evaluation summary for one debugging session."""

    key: Field[DebugSessionKey] = proto_field(
        default=...,
        number=1,
        description="Stable identity used by frame reads and cleanup.",
    )
    frame_count: Field[int] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
        description="Retained stack frames, innermost first. Zero after normal completion.",
    )
    result: Field[core.ExecutionResult] = proto_field(
        default=...,
        number=3,
        description="How evaluation ended and its bounded output.",
    )

    @model_validator(mode="after")
    def completed_session_has_no_frames(self) -> DebugSession:
        if (
            self.result.status is core.ExecutionStatus.COMPLETED
            and self.frame_count != 0
        ):
            raise ValueError("completed debugging session must have frame_count zero")
        return self


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Start an inspect-only debugging evaluation against one pinned adapter state.",
        section="Debugging",
    )
)
class DebugStartRequest(ProtoModel):
    """Start an inspect-only debugging evaluation against one pinned adapter state."""

    state: Field[AdapterState] = proto_field(default=..., number=1)
    block: Field[core.CodeBlock] = proto_field(default=..., number=2)
    budget: Field[core.DebugBudget] = proto_field(
        default=...,
        number=3,
        description="Exact workspace-configured bounds for evaluation, retained frames, and bindings.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Select one retained stack frame from a debugging session.",
        section="Debugging",
    )
)
class DebugGetFrameRequest(ProtoModel):
    """Select one retained stack frame from a debugging session."""

    session: Field[DebugSessionKey] = proto_field(default=..., number=1)
    depth: Field[int] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_UINT32,
        description="Zero-based stack depth, with the innermost retained frame first.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description="One retained frame and the session identity it came from.",
        section="Debugging",
    )
)
class DebugGetFrameResponse(ProtoModel):
    """One retained frame and the session identity it came from."""

    session: Field[DebugSessionKey] = proto_field(default=..., number=1)
    frame: Field[core.DebugFrame] = proto_field(default=..., number=2)


@proto_message(
    DirectMessage(
        ADAPTER,
        description="Release one debugging session and all retained runtime state behind it.",
        section="Debugging",
    )
)
class DebugStopRequest(ProtoModel):
    """Release one debugging session and all retained runtime state behind it."""

    session: Field[DebugSessionKey] = proto_field(default=..., number=1)


@proto_enum(
    DirectEnum(
        ADAPTER,
        name="Reason",
        parent=lambda: Refusal,
        description=(
            "Why the adapter declined. The value is what a caller acts on: some of these\n are "
            "worth retrying after a Refresh, and the rest never are."
        ),
        value_descriptions=(
            "The adapter did not classify it. Treated as permanent.",
            "The operation has no implementation for this language. Retrying is futile.",
            "The code has to change first — a file must compile before this can run.",
            "The address resolves to several targets. The caller narrows it and asks\n again.",
            (
                "The action was discovered against an adapter state that has advanced.\n "
                "Rediscover it and resolve the new token."
            ),
            "The match was found against an adapter state that has advanced. Search again.",
            (
                "Doing it would reach further than the caller can have meant — outside the\n "
                "workspace, or into generated code nothing regenerates."
            ),
            "The language itself forbids it: a rename to a reserved word, a visibility\n change its rules do not allow.",
        ),
    )
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
    DirectMessage(
        ADAPTER,
        description=(
            "Carried in `google.rpc.Status.details` when an adapter declines work it\n "
            "understood: the target is ambiguous, an action went stale, the language\n forbids "
            "it. Everything else is an ordinary status code."
        ),
        section="Failure",
    )
)
class Refusal(ProtoModel):
    """Carried in `google.rpc.Status.details` when an adapter declines work it
    understood: the target is ambiguous, an action went stale, the language
    forbids it. Everything else is an ordinary status code."""

    reason: Field[RefusalReason] = proto_field(
        default=...,
        number=1,
        description="Which class of refusal this is, so a caller can decide without parsing\n `message`.",
    )
    message: Field[str] = proto_field(
        default=...,
        number=2,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="One line for a human, saying what specifically was wrong.",
    )
    blockers: Field[list[core.OperationBlocker]] = proto_field(
        default=..., number=3, description="What in the code stopped it."
    )
    diagnostics: Field[list[core.Diagnostic]] = proto_field(
        default=..., number=4, description="What the adapter reported while trying."
    )
    preconditions: Field[list[core.OperationPrecondition]] = proto_field(
        default=...,
        number=5,
        description="Executable conditions checked before refusal, including failed values.",
    )


@proto_message(
    DirectMessage(
        ADAPTER,
        description=(
            "Carried in `google.rpc.Status.details` when a pinned call names a state the\n "
            "adapter has moved past. The caller resyncs to `actual` before retrying."
        ),
        section="Failure",
    )
)
class StaleState(ProtoModel):
    """Carried in `google.rpc.Status.details` when a pinned call names a state the
    adapter has moved past. The caller resyncs to `actual` before retrying."""

    actual: Field[AdapterState] = proto_field(default=..., number=1)


ADAPTER_PACKAGE = ProtoPackage(
    file=ProtoFile(
        "rift/adapter.proto",
        ADAPTER,
        imports=(
            "rift/core.proto",
            "google/protobuf/descriptor.proto",
            "google/protobuf/empty.proto",
        ),
        section_option=True,
    ),
    models=(
        AdapterContract,
        Capabilities,
        SourceClaim,
        AuxiliaryClaim,
        WriteClaim,
        KindDescriptor,
        SymbolFacets,
        NodeFacets,
        RelationshipFacets,
        ExtensionDescriptor,
        AdapterLimits,
        RuntimeRequirements,
        AdapterState,
        OpenRequest,
        RefreshRequest,
        VirtualSyncEvent,
        CloseRequest,
        AnalyzeRequest,
        AnalyzeEvent,
        SourceUnit,
        VirtualUnit,
        VirtualText,
        Facts,
        OriginMappings,
        Symbols,
        Nodes,
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
        ExecuteRequest,
        ExecuteResponse,
        DebugSessionKey,
        DebugSession,
        DebugStartRequest,
        DebugGetFrameRequest,
        DebugGetFrameResponse,
        DebugStopRequest,
        Refusal,
        StaleState,
    ),
    enums=(CacheScope,),
    services=(
        Service(
            "Adapter",
            (
                "Everything Rift can ask one language for. The adapter answers `Describe`\n once, "
                "then holds state for projections and answers questions about their code. Optional\n "
                "calls are advertised in `Capabilities.operations`; an unimplemented call returns\n"
                " gRPC `UNIMPLEMENTED`. Rift issues only advertised optional calls. Returning\n "
                "`UNIMPLEMENTED` from an advertised call is an adapter protocol error."
            ),
            (
                Rpc(
                    "Describe",
                    ProtoEmpty,
                    Capabilities,
                    description=(
                        "What this adapter can do, asked once before anything else. Rift holds the\n "
                        "adapter to exactly what it claims here and never probes or feature-tests."
                    ),
                ),
                Rpc(
                    "Open",
                    OpenRequest,
                    AdapterState,
                    description=(
                        "Here is the Rift FS projection. The adapter can inspect its "
                        "source, select a runtime worker, and initialize state."
                    ),
                ),
                Rpc(
                    "Refresh",
                    RefreshRequest,
                    AdapterState,
                    description=(
                        "The files on disk moved to a new state, and these are the paths that "
                        "changed. The adapter re-evaluates runtime-defining inputs before "
                        "acknowledging the new state. Without this it would have to stat the tree "
                        "to find out, and rebuilding adapter state per edit would discard "
                        "incremental work."
                    ),
                ),
                Rpc(
                    "SyncVirtual",
                    VirtualSyncEvent,
                    AdapterState,
                    description=(
                        "Replace this adapter state's private virtual-source overlay. The first\n event "
                        "pins the current adapter state; the remaining events carry a\n complete manifest "
                        "and its bytes. Rift drains calls for this adapter\n state before the stream and "
                        "blocks new calls until completion. Stream\n completion commits the overlay "
                        "atomically."
                    ),
                    request_stream=True,
                ),
                Rpc(
                    "Close",
                    CloseRequest,
                    ProtoEmpty,
                    description=(
                        "Release this tree's adapter state so Rift can delete the tree. Refused while\n "
                        "calls are in flight so their adapter state and source path remain valid\n through "
                        "the final stream event."
                    ),
                ),
                Rpc(
                    "Analyze",
                    AnalyzeRequest,
                    AnalyzeEvent,
                    description=(
                        "Read the code and say what is in it: symbols, where they appear, how they\n "
                        "connect, and what the adapter complains about. Facts stream per file and\n the "
                        "summary closes it."
                    ),
                    response_stream=True,
                ),
                Rpc(
                    "Validate",
                    ValidateRequest,
                    ValidateEvent,
                    description=(
                        "Check a session change after Rift has written it and refreshed adapter state.\n The "
                        "stream carries diagnostics followed by one verdict over the affected\n source "
                        "closure."
                    ),
                    response_stream=True,
                ),
                Rpc(
                    "Format",
                    FormatRequest,
                    FormatResponse,
                    description=(
                        "Format the files or syntactic regions touched by a session change. Rift applies\n the "
                        "returned text edits and refreshes adapter state before the next change."
                    ),
                ),
                Rpc(
                    "Match",
                    MatchRequest,
                    MatchEvent,
                    description=(
                        "Find code by structural shape: a call with two arguments, or a class\n "
                        "implementing an interface."
                    ),
                    response_stream=True,
                ),
                Rpc(
                    "Actions",
                    ActionsRequest,
                    ActionsResponse,
                    description="List the fixes and refactors the adapter can offer at one address.",
                ),
                Rpc(
                    "Resolve",
                    ResolveRequest,
                    ResolveResponse,
                    description=(
                        "Work out what one of those actions would actually change. Separate from\n "
                        "`Actions` because computing an action's edits costs about as much as\n performing "
                        "it, and only one of the actions offered gets picked."
                    ),
                ),
                Rpc(
                    "Execute",
                    ExecuteRequest,
                    ExecuteResponse,
                    description=(
                        "Evaluate a caller-provided code block in the project runtime selected for "
                        "this exact adapter state. The surrounding workspace is disposable."
                    ),
                ),
                Rpc(
                    "DebugStart",
                    DebugStartRequest,
                    DebugSession,
                    description=(
                        "Evaluate a code block for post-mortem inspection, retaining stack frames "
                        "after an unhandled failure until DebugStop."
                    ),
                ),
                Rpc(
                    "DebugGetFrame",
                    DebugGetFrameRequest,
                    DebugGetFrameResponse,
                    description="Read one retained stack frame without resuming evaluation.",
                ),
                Rpc(
                    "DebugStop",
                    DebugStopRequest,
                    ProtoEmpty,
                    description="Release a debugging session and its retained execution workspace.",
                ),
            ),
        ),
    ),
)
