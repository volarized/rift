# Rift

Rift exposes Git-pinned code reads and validated change transactions to agents over MCP. Language
adapters provide the parsing, analysis, formatting, validation, and actions they implement.

The target protocol and repository configuration are defined as Pydantic models in
`protocol/src/rift/models`. Generation produces the MCP and `rift.toml` JSON Schemas and gRPC
Protobuf files under `protocol/`, and checks the repository's own `rift.toml` against that model.

```sh
uv run python -m rift.generate --check
```

The architecture, configuration, protocol rationale, and generated reference live in `docs/`.
