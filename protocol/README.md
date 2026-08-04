# protocol

The Rift contract. Two seams, two artifacts, one model.

| File | Holds |
| --- | --- |
| `core.json` | The model: versioning, files and their syntax trees, symbols. JSON Schema. |
| `mcp.json` | The agent-facing MCP surface: tools, resources, cursors, errors. JSON Schema. |
| `core.proto` | **Generated** from `core.json`. The same model, in protobuf. |
| `adapter.proto` | The server-to-adapter gRPC service, and the messages only that seam has. |

The contract carries its own version: `ProtocolVersion` in `core.json`, and `protocol_version` in
`Capabilities`. Nothing here names a release in prose, because that would be a second and unenforced
source of truth.

These files are **normative**. Generated clients and the documentation are projections of them; where
a projection disagrees, the file wins.

## Why two artifacts

MCP is JSON-RPC, so its surface is JSON Schema. The adapter seam is gRPC over a Unix domain socket, so
its surface is a `.proto` with a service definition. Neither is a translation of the other, and neither
side knows the other exists:

- **MCP knows nothing about adapters.** No mirror, no sync stream, no environment digest and no path
  claim reaches it. Those are how the server keeps a compiler warm, which is the server's problem.
  MCP publishes `LanguageSupport` — what Rift can do for a language — and stops there.
- **The adapter knows nothing about agents.** It has no notion of a tool call, a cursor, or a resource
  URI. It is told which mirror to make and which state that mirror holds.

## One model, two spellings

```
core.json  ──generated──▶  core.proto
    │                          │
    ▼                          ▼
 mcp.json                 adapter.proto
```

Both seams carry the same things — `Symbol`, `Leaf`, `File`, `Snapshot`. Writing that model twice
would guarantee it drifts, so `core.proto` is generated:

```sh
bun protocol/scripts/gen-core-proto.mjs   # run from the repository root
```

A message and a definition of the same name describe the same thing by construction. Where protobuf
cannot express a constraint — a string pattern, a conditional requirement, `minItems` — the JSON
Schema stays normative and the proto carries the shape only.

Scalar identities inline as their scalar. `SymbolId` is a `string` in the proto, because protobuf has
no newtype and a one-field wrapper buys nothing but indirection; the URI pattern lives in `core.json`.

## What gRPC already does

`adapter.proto` carries no framing of its own — no envelope, no request id, no operation name, no
sequence number, no cancel operation. An RPC method is the operation, a stream is ordered, a
cancelled context or an expired deadline ends the work, and failures are `google.rpc.Status` with
`Refusal` or `StaleState` in `status.details`. Eight RPCs replace what was thirteen operations and
twenty-nine envelope types.

## The three axes

`core.json` declares its own grouping under `rift:axes`, so which definition belongs to which axis is
part of the contract rather than a list in the documentation code:

- **Versioning** — `Revision` is the question, `Snapshot` is what it resolved to.
- **Physical** — `File` at a path, `Leaf` as a node of one language's parse of it.
- **Semantic** — `Symbol`, and the relationships between symbols.

Crossings are one-way and explicit: `Leaf.symbol` goes up, `Relationship.evidence` goes down. Nothing
else crosses.

## Cross-file references

`mcp.json` points at `core.json` with `$id`-relative `$ref` — `{"$ref": "core.json#/$defs/Symbol"}` —
and nothing points back. Definition names are unique across both documents. Consumers must map `$id`
to a local file themselves; the specification does not mandate retrieval. Anything that can only read
a single document needs the two bundled first; draft 2020-12 compound schema documents are the
standard shape for that.

## Extension keys

Two keys carry contract facts JSON Schema has no keyword for:

- `rift:entryPoints` — the seams every other definition is reachable from. `mcp.json` declares
  `mcp.tools`, `mcp.resources`, `mcp.resources.read` and `mcp.error`. `core.json` declares none,
  because nothing enters the contract through it.
- `rift:axes` — the three axes, the definitions that identify each, and how the rest are filed.

## What the schemas cannot assert about themselves

Some guarantees are relational — adapter coverage, the environment equality a `Sync` commit reports,
retention ordering. No JSON Schema keyword ties one field's value to a digest of another, so those are
asserted by executable conformance tests rather than pretended here.

## Not here yet

The one-way SCIP projection is deferred. It was never a separable contract, so it will come back as a
mapping over `core.json` rather than as another document.

## Documentation

`/docs/protocol` renders these files directly. `docs/src/lib/protocol.ts` loads the JSON Schema half,
`docs/src/lib/proto.ts` loads the protobuf half, and the pages derive their table of contents and
search index from the same load. Nothing is generated into the docs and nothing is committed
alongside them, so a page cannot drift: editing a file here is the only way to change one.

The renderer refuses to be lossy by accident. It fails the build on any JSON Schema keyword the
components do not render, on a `$ref` that resolves to nothing, on an axis group seeded by a
definition that no longer exists, and on an operation name declared without a frame behind it.
