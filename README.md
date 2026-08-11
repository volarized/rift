# Rift

Rift gives each agent a writable projection of a workspace. Agents read semantic facts and make
validated changes over MCP. Rift FS keeps the projection available to ordinary filesystem tools,
and `publish` writes reviewed changes into the workspace.

The protocol and `rift.toml` models live in `protocol/src/rift/models`. Generate their JSON Schema
and Protobuf outputs with:

```sh
cd protocol
uv run python -m rift.generate --check
```

Architecture and protocol documentation live in `docs/`.
