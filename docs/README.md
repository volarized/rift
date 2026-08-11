# Documentation site

This directory contains the Fumadocs site for Rift. Hand-written pages live under
`content/docs`; reference components read the generated MCP and `rift.toml` JSON Schemas and
Protobuf files from `../protocol`.

Install the JavaScript dependencies and start the development server:

```sh
bun install
bun run dev
```

The site is then available at <http://localhost:3000>.

When a Pydantic model, configuration field, or protocol description changes, regenerate the schemas
and Protobuf files from the repository root before building the site:

```sh
cd protocol
uv run python -m rift.generate
```

Use these checks for documentation changes:

```sh
bun run lint
bun run build
```

`bun run pack` copies the static export into `dist/rift` with the deployment base path applied.
Generated directories such as `.next`, `.source`, `out`, and `dist` aren't source files and shouldn't
be edited directly.
