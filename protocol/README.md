# Protocol

The generated contract is documented under `docs/content/docs/protocol`.

Generate every protocol artifact from this directory:

```sh
uv run python -m rift.generate
```

Check that committed artifacts are current and valid:

```sh
uv run python -m rift.generate --check
```
