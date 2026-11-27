# Kotlin grammar in Rift

This crate uses tree-sitter-kotlin-ng 1.1.0 from commit
`5e83fcb8df1b472a2bb69c1b544a74d5cc4df96c`. The Rust language binding and C scanner keep
their bytes from that revision. `LICENSE` carries their MIT license. `LICENSE.tree-sitter`
carries the generator and parser header license. [Grammar source](https://github.com/lzehrung/tree-sitter-kotlin/tree/5e83fcb8df1b472a2bb69c1b544a74d5cc4df96c)

The local grammar accepts `context(...)` prefixes on function types, keeps `context` valid as an
identifier, and allows trailing commas in type parameter lists. The build script decompresses
the parser into Cargo's output directory.

## Regenerate the parser

Run from this directory with Node.js, Tree-sitter CLI 0.25.10 and gzip. The checked-in gzip
uses Apple gzip 487.40.2 with its filename and timestamp omitted. [Tree-sitter CLI 0.25.10](https://github.com/tree-sitter/tree-sitter/releases/tag/v0.25.10)

```sh
tree-sitter generate --abi 14
gzip -n -9 -c src/parser.c > src/parser.c.gz
```

- `src/parser.c` SHA-256 `a922364ba750ee9ff4faed7c62510f51f36d1359238aa6c5ebaaf7b6a8836128`
- `src/parser.c.gz` SHA-256 `b8c8cc9df9c9712b7e336e25ddbb89a3dad335ac53c7c9dec33a42b4fd6f2c1b`

Keep `src/node-types.json`, `src/tree_sitter/parser.h`, and `src/parser.c.gz`. Update
`PARSER_BYTES` in `bindings/rust/build.rs` when the decoded size changes. Run `just generate`
from the Rift workspace to update the analyzer manifest.
