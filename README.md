# Rift

Rift exposes persistent filesystem projections and validated source transactions to agents over MCP
and to local tools through a user-mounted filesystem. A workspace store owns current source state;
Git is used only to import existing history and publish a squash integration. Language adapters
provide the parsing, analysis, formatting, validation, and actions they implement.

The target protocol and repository configuration are defined as Pydantic models in
`protocol/src/rift/models`. Generation produces the MCP and `rift.toml` JSON Schemas and gRPC
Protobuf files under `protocol/`, and checks the repository's own `rift.toml` against that model.

```sh
uv run python -m rift.generate --check
```

The architecture, configuration, protocol rationale, and generated reference live in `docs/`.
