# tree-sitter-typescript

This directory contains tree-sitter-typescript 0.23.2 under its original MIT license. The original crate archive has SHA-256 `6c5f76ed8d947a75cc446d5fccd8b602ebf0cde64ccf2ffa434d873d7a575eff`. Rift uses the original Rust binding and scanners.

The grammar adds `global_namespace` from upstream pull request 302, commit `fc72ea1daaa1b7ff525a7ff34d56dc8714168765`. This accepts a bare `global` block inside an ambient module while preserving the original source bytes. Malformed declarations and blocks still produce parser errors.

Both parsers were generated with tree-sitter CLI 0.24.4 and ABI 14. The macOS arm64 generator has SHA-256 `e4715136bcafc041d9ed72e9c04ee5581b97d7dceac442a967e338f5b77a7f97`. Generation uses the original tree-sitter-javascript 0.23.1 `grammar.js`, SHA-256 `230e330dd914d94297e5e58d53942cd5debf9c069852da93b6a6b1daae1967dc`.

Run `tree-sitter generate --abi 14` in `typescript` and `tsx`, with tree-sitter-javascript 0.23.1 available in `node_modules`. The analyzer manifest includes the grammar, generated parsers, Rust binding and scanners once; captured source validation keeps its existing digest.

Sources: <https://github.com/tree-sitter/tree-sitter-typescript/tree/v0.23.2>, <https://github.com/tree-sitter/tree-sitter-typescript/pull/302>, <https://github.com/tree-sitter/tree-sitter/releases/tag/v0.24.4>.
