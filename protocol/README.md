# Protocol

The generated contract is documented under `docs/content/docs/protocol`.

Generate every protocol artifact from this directory. This writes `mcp.json`,
`rift.schema.json`, and the Protobuf files, and validates the repository's real
`rift.toml` against the typed configuration model:

```sh
uv run python -m rift.generate
```

Check that committed artifacts are current and valid:

```sh
uv run python -m rift.generate --check
```
