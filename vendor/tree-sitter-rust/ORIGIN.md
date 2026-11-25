# Rust grammar in Rift

This crate is based on tree-sitter-rust 0.24.2 at commit `77a3747266f4d621d0757825e6b11edcbf991ca5`. The local grammar accepts the legacy `try!` macro, attributes on struct pattern fields, and opaque type declarations in extern blocks. Cargo builds the checked-in C sources under the upstream MIT license. [Upstream source](https://github.com/tree-sitter/tree-sitter-rust/tree/77a3747266f4d621d0757825e6b11edcbf991ca5)

## Regenerate the parser

Run the following from this directory with Node.js and Tree-sitter CLI 0.27.0. [Tree-sitter CLI 0.27.0](https://github.com/tree-sitter/tree-sitter/releases/tag/v0.27.0)

```sh
tree-sitter generate --abi 15
```

Run `just generate` from the Rift workspace to update its analyzer manifest.
