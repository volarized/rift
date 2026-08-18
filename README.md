# Rift

[![Rift — agentic development toolkit for codebases](docs/public/og.png)](https://volar.sh/rift/)

Rift is an agentic development toolkit for reading, discovering, and editing codebases.

Rift provides contextual, parser-precise codebase reading and editing tools. It combines syntax,
semantic analysis, and history to give agents the best available context. Through MCP, agents read
declarations and provider facts, then precisely edit symbols with a clear blast radius. Changes can
be staged in filesystem projections, inspected, and published into the workspace.

📖 [Read the documentation](https://volar.sh/rift/docs/draft/)

## Protocol development

The protocol and `rift.toml` models live in `protocol/src/rift/models`. Generate their JSON Schema
and Protobuf outputs with:

```sh
cd protocol
uv run python -m rift.generate --check
```
