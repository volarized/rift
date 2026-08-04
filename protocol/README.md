# protocol

The Rift contract: what an agent may ask the server, and what the server may ask a compiler.

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

## Why the seams are described differently

MCP is JSON-RPC, so its surface is JSON Schema. The adapter seam is gRPC over a Unix domain socket, so
its surface is a `.proto` with a service definition. Neither is a translation of the other, and neither
side knows the other exists:

- **MCP knows nothing about adapters.** No workspace path, no refresh and no path claim reaches it. Those are how the server keeps a compiler warm, which is the server's
  problem. MCP publishes `LanguageSupport` — what Rift can do for a language — and stops there.
- **The adapter knows nothing about agents.** It has no notion of a tool call, a cursor, or a resource
  URI. It is handed a working tree and told what state that tree holds.

## Who owns the filesystem

Rift materializes a working tree per adapter with `git checkout-index` and passes the path, which is
also the workspace's identity. No `.git` lands in it, so an adapter cannot reach your branches, your
remotes or the credentials in your git config — which a linked `git worktree` would have left one
gitdir link away.

Rift writes source and the adapter does not. A fix or a refactor comes back from `Resolve` as
`Edit`, and Rift applies it. That makes a change reviewable before it lands, and it keeps one
agent's `rustfmt` run out of the tree another agent is reading.

Compilers write regardless — `Cargo.lock` at the workspace root, `node_modules` beside its package —
so an adapter declares those paths as `WriteClaim`s in `Describe` and Rift stops reading them as source.
Everything redirectable should be redirected into `state_root` instead, because a claim is a hole in
what Rift can see.

## Where the model lives

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
no newtype and a one-field wrapper would only add indirection; the URI pattern lives in `core.json`.

## What gRPC already does

`adapter.proto` carries no framing of its own — no envelope, no request id, no operation name, no
sequence number, no cancel operation. An RPC method is the operation, a stream is ordered, a
cancelled context or an expired deadline ends the work, and failures are `google.rpc.Status` with
`Refusal` or `StaleState` in `status.details`.

## Axes

`core.json` declares its own grouping under `rift:axes`, so which definition belongs to which axis is
part of the contract rather than a list in the documentation code:

- **Versioning** — `Revision` is the question, `Snapshot` is what it resolved to.
- **Filesystem** — `File` at a path, `Leaf` as a node of one language's parse of it.
- **Semantic** — `Symbol`, and the relationships between symbols.
- **Discovery** — `Filter` and the two query grammars. Written by the caller, before an answer exists.
- **Reachability** — `Coverage` and `Diagnostic`: how much of an answer Rift could see, and what the
  compiler said on the way.
- **Operations** — `Address`, `CodeActionDescriptor`, and the keys that pin a discovery to the state it
  was made in.

Crossings are one-way and explicit: `Leaf.symbol` goes up, `Relationship.evidence` goes down. Nothing
else crosses.

## Cross-file references

`mcp.json` points at `core.json` with `$id`-relative `$ref` — `{"$ref": "core.json#/$defs/Symbol"}` —
and nothing points back. Definition names are unique across both documents. Consumers must map `$id`
to a local file themselves; the specification does not mandate retrieval. Anything that can only read
a single document needs them bundled first; draft 2020-12 compound schema documents are the standard
shape for that.

## Extension keys

These carry contract facts JSON Schema has no keyword for:

- `rift:entryPoints` — the seams every other definition is reachable from. `mcp.json` declares
  `mcp.tools`, `mcp.resources`, `mcp.resources.read` and `mcp.error`. `core.json` declares none,
  because nothing enters the contract through it.
- `rift:axes` — the axes, the definitions that identify each, and how the rest are filed.

## What the schemas cannot assert about themselves

Some guarantees are relational — adapter coverage, the snapshot equality a `Refresh` reports,
retention ordering, that two `Edit`s in one set do not overlap. No JSON Schema keyword ties one
field's value to a digest of another, so those are asserted by executable conformance tests rather
than pretended here.

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
