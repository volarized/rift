set dotenv-load := false

format:
    cargo fmt --all --check

generate:
    cargo run -q -p rift-mcp --bin rift-schema-export -- docs
    printf '$ rift --help\n' > docs/public/cli-help.txt
    cargo run -q -p rift -- --help >> docs/public/cli-help.txt
    printf '\n$ rift server --help\n' >> docs/public/cli-help.txt
    cargo run -q -p rift -- server --help >> docs/public/cli-help.txt

generate-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo run -q -p rift-mcp --bin rift-schema-export -- --check docs
    fresh="$(mktemp)"
    trap 'rm -f "$fresh"' EXIT
    printf '$ rift --help\n' > "$fresh"
    cargo run -q -p rift -- --help >> "$fresh"
    printf '\n$ rift server --help\n' >> "$fresh"
    cargo run -q -p rift -- server --help >> "$fresh"
    cmp -s docs/public/cli-help.txt "$fresh" || {
        echo "error: \`docs/public/cli-help.txt\` does not match the CLI help; regenerate it with \`just generate\`" >&2
        exit 1
    }

check:
    cargo metadata --locked --format-version 1 > /dev/null
    uv run --script scripts/check_rust_architecture.py

# The em-dash ban, over every surface a reader meets: the docs pages and
# the app shell, the prose inside the crates, the README, the artifacts
# `just generate` writes, and the CI configuration's own comments. The
# scanner itself is not among them: it spells the banned characters.
dashes:
    uv run --script scripts/check_dashes.py \
        docs/content docs/src/app crates README.md docs/public .github

# One run of every suite, live engines included: the engine tier's own code
# is only exercised against a real language server, so a hermetic run would
# report it uncovered. Coverage is this run's artifact, not a second run.
test:
    RIFT_ENGINE_LIVE=1 cargo llvm-cov --workspace --all-targets --all-features --lcov --output-path lcov.info --fail-under-lines 86

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps

audit:
    cargo audit
    cargo deny check

clean:
    #!/usr/bin/env bash
    set -euo pipefail
    git worktree list --porcelain | sed -n 's/^worktree //p' | while read -r tree; do
        if [ -f "$tree/Cargo.toml" ]; then
            echo "cleaning $tree"
            cargo clean --manifest-path "$tree/Cargo.toml" || echo "skipped $tree: broken checkout"
        fi
    done


# The live-engine suites: gated by RIFT_ENGINE_LIVE, run in CI's engines
# job, and outside `rust-gate` so the local default gate stays hermetic.
engine-test:
    RIFT_ENGINE_LIVE=1 cargo test -p rift-lsp --test live_rust_analyzer
    RIFT_ENGINE_LIVE=1 cargo test -p rift-mcp --test live_rust_analyzer

release-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_release.py

installer-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_installers.py

rust-gate: format dashes generate-check check clippy docs audit test release-test installer-test
