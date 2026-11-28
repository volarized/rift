# Documentation site

This directory contains the Fumadocs site for Rift. Hand-written pages live under
`content/docs`. Installation, the MCP surface, and usage are top-level pages, while
architecture, implementation, configuration, testing, the error registry, and protocol details
live under `content/docs/developer`.

Install the JavaScript dependencies and start the development server:

```sh
bun install --frozen-lockfile
bun run dev
```

The site is then available at <http://localhost:3000/rift/>.

`bun run dev` and `bun run build` run `cargo xtask docs` from the repository root. That command
exports the served schemas, error registry, and CLI help from Rust. It captures the example
project, resource reads, and tool responses from a local Rift MCP server. Run
`cargo xtask docs-examples generate` to refresh only the example captures, or
`cargo xtask docs-examples check` to compare them with a fresh capture.

Run `bun run build:static` once those files are available to reuse the generated artifacts.

Use these checks for documentation changes:

```sh
bun run lint
bun run build
```

Write prose without middle dots, semicolons, em dashes, or en dashes. Preserve literal punctuation
in code examples and protocol output.

`bun run pack` copies the static export into `dist/rift` with the deployment base path applied.
Generated directories such as `.next`, `.source`, `out`, and `dist` aren't source files and shouldn't
be edited directly.
