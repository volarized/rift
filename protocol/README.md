# protocol

The Rift contract, as three JSON Schema documents. Together they cover the first-party MCP tools and
resources, the semantic model, the persistence models, and the newline-delimited JSON adapter IPC — 421
definitions, every one reachable from an entry point declared in `rift:entryPoints`.

The contract carries its own version: `ProtocolVersion` in `core.json` is a const the wire negotiates.
Nothing here names a release in prose, because that would be a second and unenforced source of truth.

They are **normative**. Generated clients, the adapter wire contract, and the documentation are all
projections of these files; where a projection disagrees with them, the schema wins.

| Document | Definitions | Holds |
| --- | --- | --- |
| `core.json` | 158 | Symbols, Elements, Types, Relationships, identity, source addressing, coverage, diagnostics |
| `mcp.json` | 128 | Five tools, five resource templates, the categorical error contract, durable records |
| `adapter.json` | 135 | `hello`, mirror lifecycle, `sync`, and the streaming analyze/match/resolve/validate operations |

## Why three, and why `core.json` exists

The split is measured, not chosen. `core.json` is exactly the vocabulary reachable from both the MCP
seams and the adapter seam — 158 definitions, larger than either exclusive set. A two-file split was
never available: whichever file owned the model, the other would depend on half its contents.

Cross-file references use `$id`-relative `$ref` — `{"$ref": "core.json#/$defs/Symbol"}`. The dependency
graph is a DAG and stays one: `adapter.json → core.json` (150 reference sites) and `mcp.json →
core.json` (111), with no edges back. JSON Schema would tolerate cycles here; the layering is for the
consumers, since a codegen backend that emits one module per document is much happier without them.

Consumers must map `$id` to a local file themselves — the specification does not mandate retrieval.
Anything that can only read a single document (`typify`, for one) needs the three bundled first; draft
2020-12 compound schema documents are the standard shape for that.

## Extension keys

Two keys carry contract facts JSON Schema has no keyword for:

- `rift:entryPoints` — the seams every other definition is reachable from. `mcp.json` declares
  `mcp.tools`, `mcp.resources`, `mcp.resources.read`, `mcp.error`, and `storage`; `adapter.json`
  declares `adapter.frame`. `core.json` declares none, because nothing enters the contract through it.
- `rift:targetTiers` — which entity-target tier each entry position admits. The three tiers nest, and
  the shared `Address` / `StructuralQuery` / `MatchQuery` types carry the widest one so the type graph
  never bifurcates into public and program variants. Admission is declared here and enforced by the
  server.

## What the schemas cannot assert about themselves

Four guarantees are relational — validator-result coverage, adapter coverage, the `sync.commit`
environment equality, and the retention ordering. No JSON Schema keyword ties one field's value to a
digest of another, so those are asserted by executable conformance tests rather than pretended here.
Eight further invariants are schema-shape lints run at generation time.

## Not here yet

The one-way SCIP projection is deferred. It was never a separable contract — of its nine types, three
were vocabulary the adapter advertised in `hello`, four were a single MCP resource, and two were the
projection's own accounting — so it will come back as a mapping over `core.json`, not as a fourth
document.

## Documentation

The `/docs/protocol` page renders these files directly — `docs/src/components/protocol` reads them and
`docs/src/lib/protocol.ts` derives the page's table of contents and search index from the same load.
Nothing is generated and nothing is committed alongside them, so the page cannot drift: editing a
schema is the only way to change it.

The renderer refuses to be lossy by accident. It fails the build on any JSON Schema keyword the
components do not render, and definitions carrying conditional keywords the layout cannot express also
print their verbatim JSON. A constraint may reach the page as a table, a list, or raw JSON — never as
silence.
