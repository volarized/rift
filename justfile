set dotenv-load := false

format:
    cargo fmt --all --check

generate-check:
    uv run --project protocol python -m rift.generate --check

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

coverage:
    cargo llvm-cov --workspace --all-targets --all-features --lcov --output-path lcov.info --fail-under-lines 86

release-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests

installer-test:
    uv run --script scripts/tests/test_installers.py

rust-gate: format generate-check check test clippy docs audit coverage release-test installer-test
