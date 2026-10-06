set dotenv-load := false

rift_dev := "uv run --locked --project dev rift-dev"

format:
    {{ rift_dev }} fmt --all --check

generate:
    {{ rift_dev }} generate
    cargo xtask errors generate

generate-check:
    {{ rift_dev }} generate --check
    cargo xtask errors check

check:
    cargo metadata --locked --format-version 1 > /dev/null
    {{ rift_dev }} check --workspace --all-targets --all-features --locked
    {{ rift_dev }} rust-architecture

dashes *args:
    {{ rift_dev }} dashes {{ args }}

# The MCP specification's own conformance runner, over a throwaway workspace
# one foreground server serves. `tools/mcp-conformance/expected-failures.yml`
# carries the scenarios the served surface fails today; anything else fails
# the gate.
conformance *args:
    {{ rift_dev }} conformance {{ args }}

clippy:
    {{ rift_dev }} clippy --workspace --all-targets --all-features -- -D warnings

docs:
    {{ rift_dev }} docs --workspace --all-features --no-deps

# Stable Rust exposes doctests through rustdoc; nextest runs the other Rust tests.
doctest:
    {{ rift_dev }} test doctest

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
    {{ rift_dev }} test archive --workspace --all-targets --all-features --locked --profile ci --archive-file target/fast.tar.zst --zstd-level 9 -E 'not binary(/^corpus_/)'

test *args:
    {{ rift_dev }} test unit {{ args }}

live-test *args:
    {{ rift_dev }} test live {{ args }}

release-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_release.py

installer-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_installers.py

corpus-sync *args:
    {{ rift_dev }} corpus sync {{ args }}

# The plain CLI the artifact job serves, from the corpus profile.
integration-cli:
    {{ rift_dev }} build --locked --profile corpus -p rift
    tar --zstd -cf target/integration-cli.tar.zst -C target/corpus rift

# The archive carries the three corpus suites; the live suites use the fast archive.
integration-archive:
    {{ rift_dev }} test archive --workspace --all-features --locked --cargo-profile corpus --profile corpus --archive-file target/integration.tar.zst --test corpus_bun --test corpus_fastapi --test corpus_nextjs

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

quick-gate: format dashes generate-check check clippy

rust-gate: quick-gate docs doctest audit test release-test installer-test

# One signed tag on the commit `origin/main` names right now; pushing it starts
# `rift-release`.
release tag:
    {{ rift_dev }} release {{ tag }}
