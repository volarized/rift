# Rust client generator selection

`cargo xtask docs` exports `docs/public/global-api.openapi.json` from the endpoint definitions,
protocol schemas, and examples in Rust. `just generate` feeds that OpenAPI document to `oas3-gen`
to update the checked wire module. The 0.28.0 generator ships as a command, and Cargo builds use
the checked Rust module without installing it.

## Selected generator

Selected version: `oas3-gen 0.28.0`.

Source reviewed before use:

- `oas3-gen` `Cargo.toml`, `README.md`, `src/main.rs`, and `src/generator/mode.rs` from the
  published 0.28.0 crate
- [oas3-gen source](https://github.com/eklipse2k8/oas3-gen)

Type generation command used by `just generate`:

```sh
oas3-gen generate types -q --enum-mode relaxed --no-ordered-collections \
  -i docs/public/global-api.openapi.json \
  -o crates/rift-cloud-client/src/generated.rs
```

`just generate-check` exports fresh OpenAPI, applies the same generator adaptations as
`just generate`, and compares the resulting Rust bytes with the checked module. The generator
input omits the edge request's object-presence condition because the 0.28.0 generator emits an
invalid constructor for it. The output spells two large bounds as `u64` literals so the generated
validators compile. The published OpenAPI document retains the full edge request condition.
Keep generated source rustfmt-skipped. Types mode emits operation request
and query types, response parsers, and status enums without a transport client or default endpoint.
`GlobalClient` owns configured endpoint selection, bounds, retries, caches, and response streaming.
Public exports contain the generated wire types needed by callers.

## Rejected generator

OpenAPI Generator 7.25.0 was tested with:

```sh
docker run --rm \
  -v "$PWD/docs/public/global-api.openapi.json:/local/spec.json:ro" \
  -v "$output:/local/out" \
  openapitools/openapi-generator-cli:v7.25.0 \
  generate -i /local/spec.json -g rust -o /local/out
```

Two runs produced equal output. `cargo check` on that output failed with 70 errors, beginning with
duplicate `String` enum variants and ending with non-exhaustive matches. OpenAPI Generator records
OpenAPI 3.1 support as beta, and its
[Rust OpenAPI 3.1 issue](https://github.com/OpenAPITools/openapi-generator/issues/16119) records a
valid 3.1 contract producing invalid Rust.

## Runtime dependencies

Reviewed resolved sources before the HTTP runtime was written:

- `reqwest 0.13.5`: reusable client, redirect policy, request timeout, response headers, content
  length, and bounded chunk streaming
- `tokio 1.53.1`: total deadlines, cancellation through future drop, mutexes, read-write locks,
  and semaphore permit behavior
- `serde 1.0.229` and `serde_json 1.0.151`: generated request and response encoding
- `tracing 0.1.44`: structured request records
- `axum 0.8.9`: test-only fixture HTTP peer

The runtime sets redirect policy to none, bounds response streaming itself, wraps every attempt and
delay in one deadline, and uses one semaphore across attempts and pages.
