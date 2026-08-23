set dotenv-load := false

format:
    cargo fmt --all --check

generate:
    cargo run -q -p rift-mcp --bin rift-schema-export -- docs

generate-check:
    cargo run -q -p rift-mcp --bin rift-schema-export -- --check docs

check:
    cargo check --workspace --all-targets --all-features
    uv run --script scripts/check_rust_architecture.py

test:
    cargo test --workspace --all-targets --all-features

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
            cargo clean --manifest-path "$tree/Cargo.toml"
        fi
    done

coverage:
    cargo llvm-cov --workspace --all-targets --all-features --lcov --output-path lcov.info --fail-under-lines 86

release-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_release.py

installer-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_installers.py

rust-gate: format generate-check check test clippy docs audit coverage release-test installer-test
