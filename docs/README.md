# Documentation site

This directory contains the Fumadocs site for Rift. Hand-written pages live under
`content/docs`; `protocol/mcp.json` carries the served MCP tool surface exported from the Rust
workspace.

Install the JavaScript dependencies and start the development server:

```sh
bun install
bun run dev
```

The site is then available at <http://localhost:3000>.

When a protocol model or a tool doc comment changes, regenerate the exported surface from the
repository root before building the site:

```sh
just generate
```

Use these checks for documentation changes:

```sh
bun run lint
bun run build
```

`bun run pack` copies the static export into `dist/rift` with the deployment base path applied.
Generated directories such as `.next`, `.source`, `out`, and `dist` aren't source files and shouldn't
be edited directly.
