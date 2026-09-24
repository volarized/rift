set dotenv-load := false

rift_dev := "uv run --locked --python 3.12 --project dev rift-dev"

format:
    cargo fmt --all --check

generate:
    {{ rift_dev }} generate

generate-check:
    {{ rift_dev }} generate --check

check:
    cargo metadata --locked --format-version 1 > /dev/null
    cargo check --workspace --all-targets --all-features --locked
    {{ rift_dev }} rust-architecture

dashes *args:
    {{ rift_dev }} dashes {{ args }}

# The MCP specification's own conformance runner, over a throwaway workspace
# one foreground server serves. `tools/mcp-conformance/expected-failures.yml`
# carries the scenarios the served surface fails today; anything else fails
# the gate.
conformance *args:
    {{ rift_dev }} conformance {{ args }}

# An in-memory OTLP/HTTP collector for timing `traced!`/`traced_async!` spans on
# port 4318. Point a build compiled with the `otlp` feature at it with
# `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318`; Ctrl-C prints one JSON line
# per operation.
trace-collector *args:
    {{ rift_dev }} trace-collector {{ args }}

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps

# Stable Rust exposes doctests through rustdoc; nextest runs the other Rust tests.
doctest:
    cargo test --doc --workspace --all-features --locked

audit:
    cargo audit
    cargo deny check

clean:
    {{ rift_dev }} clean

# Archive the unit and live suites once; both execution jobs reuse these
# binaries. The execution job's limit counts the transfer as well as the run, so
# the archive carries less of both: the filterset leaves out the corpus binaries,
# which only the corpus profile runs and which need the optimized build, and the
# compression level trades build time for bytes. Measured over one revision,
# level 9 costs 17 seconds where level 19 costs six minutes for 15% more.
fast-archive:
    cargo llvm-cov nextest-archive --workspace --all-targets --all-features --locked --profile ci --archive-file target/fast.tar.zst --zstd-level 9 -E 'not binary(/^corpus_/)'

test *args:
    {{ rift_dev }} test unit {{ args }}

live-test *args:
    {{ rift_dev }} test live {{ args }}

release-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_release.py

installer-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_installers.py

testing-check:
    uv run --locked --python 3.12 --project dev ruff check dev
    uv run --locked --python 3.12 --project dev ty check dev
    uv run --locked --python 3.12 --project dev pytest dev/tests

corpus-sync *args:
    {{ rift_dev }} corpus sync {{ args }}

# One archive supplies every integration job. Save the plain CLI before test builds.
# The archive carries the corpus suites and nothing else. `--all-targets` built
# and linked every test binary in the workspace, and each one links the whole
# workspace; the live suites moved to the fast archive, which is built once for
# every pull request. `dev/tests/test_delivery.py` refuses a selection that
# leaves out a suite the corpus profile runs.
integration-archive:
    cargo build --locked --profile corpus -p rift
    tar --zstd -cf target/integration-cli.tar.zst -C target/corpus rift
    cargo llvm-cov nextest-archive --workspace --all-features --locked --cargo-profile corpus --profile corpus --archive-file target/integration.tar.zst --test corpus_bun --test corpus_fastapi --test corpus_nextjs

corpus-test *args:
    {{ rift_dev }} test corpus {{ args }}

artifact-test *args:
    {{ rift_dev }} artifact {{ args }}

agent-test *args:
    {{ rift_dev }} agent {{ args }}

coldstart-test *args:
    {{ rift_dev }} coldstart {{ args }}

integration-test:
    {{ rift_dev }} integration-test

rust-gate: format dashes generate-check check clippy docs doctest audit test release-test installer-test testing-check

# One signed tag on the commit `origin/main` names right now; pushing it starts
# `rift-release`.
release tag:
    {{ rift_dev }} release {{ tag }}
