# Rift

Rift gives each agent a scratchpad copy of a workspace. Agents read semantic facts and make
validated changes over MCP, tools that only understand directories work against the same copy, and
`publish` carries the result into the workspace once you've accepted it.

The protocol and `rift.toml` models live in `protocol/src/rift/models`. Generate their JSON Schema
and Protobuf outputs with:

```sh
cd protocol
uv run python -m rift.generate --check
```

Architecture and protocol documentation live in `docs/`.
