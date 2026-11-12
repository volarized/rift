# Semantic collection

`rift-collect` runs a collector over an admitted inventory of UTF-8 sources. It emits one JSON line per selected source and category. Each line names the producer, its implementation and context revisions, the canonical source unit, the source digest, coverage, and the original observations.

The Python collector uses the standard library AST. It records explicit calls, including repeated and module-level occurrences. The JavaScript collector uses TypeScript 5.9.3 for JavaScript, JSX, TypeScript and TSX. It also records same-source signature declarations as candidates. Both parse target source without executing it. The reference collectors report unsupported coverage for categories other than calls.

## Run and rerun

Build the collector runner with `cargo build --locked -p rift-collect`. Create a manifest with a stable producer ID and lowercase SHA-256 digests for its implementation and context. Use the implementation digest for the collector and adapter code. Use the context digest for engine and runtime versions, compiler options, and other inputs outside the admitted inventory. Change the corresponding digest when one of those inputs changes.

For a Python run from the repository root:

```sh
printf 'def greet(): pass\ngreet()\n' > example.py
revision=$(shasum -a 256 tools/semantic-collectors/python.py | cut -d ' ' -f 1)
context=$(python3 --version | shasum -a 256 | cut -d ' ' -f 1)
cat > sources.json <<JSON
{
  "producer": "python.calls",
  "revision": "$revision",
  "context": "$context",
  "category": "calls",
  "sources": [{
    "unit": "rift://source/project/example.py",
    "path": "example.py",
    "filename": "example.py"
  }]
}
JSON
cargo run --locked -q -p rift-collect -- collect sources.json checkpoints.sqlite -- \
  python3 -B -S tools/semantic-collectors/python.py
```

For JavaScript, use a JavaScript or TypeScript source and a distinct producer ID. Pass `node tools/semantic-collectors/javascript.mjs /path/to/typescript/lib/typescript.js` after `--`. Include the engine and runtime in the context digest.

The manifest resolves `path` relative to its own directory. The collector uses `filename` to select parser behavior and `unit` to identify the source. Set `"selected": false` on a dependency that collectors can read without collecting its own batch.

Repeat the command with the same database. The runner reuses a checkpoint when the producer, context, category, selected source and supporting source witnesses match. A read of a missing unit is also a witness. Adding that unit invalidates the dependent batch. Removing a unit from the selection removes it from subsequent output. Complete batches commit independently. A restart after failure reuses them. A partial batch remains due, and the CLI exits with status 2 after returning its output. Other failures exit with status 1.

Use a separate database for each collection. A new database has a 1 GiB page limit. The runner accepts up to 20,000 sources, 128 MiB of total source text and 1 MiB per source. It admits up to 4,096 observations and 512 supporting units per batch. The CLI gives each request 30 seconds. A complete batch establishes coverage of the selected source and category. A candidate does not establish runtime dispatch.

## Use a collector from another host

Implement `rift_collect::Collector` for an in-process Rust provider. The trait takes an admitted `CollectionInput` and returns a `Batch`. The provider can read supporting units through `CollectionInput::read`. Disable the default `runtime` feature to use the model without process I/O or SQLite. Use `rift_collect::CollectionCache` when local checkpoints are required.

Python modules can export `collect(request, read_source)` and be passed after `python.py`. JavaScript modules can export `collect(ts, request, readSource)` and be passed after the TypeScript entry point. Each call to `readSource` must finish before another starts. A module may also call the built-in collector directly. JavaScript can supply an AST and checker from its own program for the exact admitted source.

`rift-collect schema` emits the request and result schemas. The stdio adapter sends `extractor/extract` using Content-Length framed JSON-RPC. A collector can issue `facts/request` with a canonical `unit` while extraction is pending. The runner returns the admitted source or `null`. A physical target must include the digest and byte range of the inspected source. The runner validates occurrence, enclosing, and target coordinates before checkpointing. Collector code runs with the privileges of the caller. Use stdout for protocol messages and stderr for diagnostics.
