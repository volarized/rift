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

# One run of every suite, live engines and the live model hub included: the
# engine tier's own code is only exercised against a real language server, and
# the semantic search tier's acquisition only against the real hub, so a
# hermetic run would report both uncovered. Needs rust-analyzer on the default
# toolchain, bun on the PATH, and network reach to huggingface.co; the model is
# cached per machine, so only the first run pays for the download. Coverage is
# this run's artifact, not a second run.
test:
    RIFT_ENGINE_LIVE=1 RIFT_SEARCH_LIVE=1 cargo llvm-cov --workspace --all-targets --all-features --lcov --output-path lcov.info --fail-under-lines 86

# The live-engine suites alone, for iterating on them without paying for
# the instrumented workspace run.
engine-test:
    RIFT_ENGINE_LIVE=1 cargo test -p rift-lsp --test live_rust_analyzer
    RIFT_ENGINE_LIVE=1 cargo test -p rift-mcp --test live_rust_analyzer
    RIFT_ENGINE_LIVE=1 cargo test -p rift-lsp --test live_typescript
    RIFT_ENGINE_LIVE=1 cargo test -p rift-mcp --test live_typescript

# The live semantic-search suite alone, for iterating on it without paying for
# the instrumented workspace run. Reaches the real model hub.
search-test:
    RIFT_SEARCH_LIVE=1 cargo test -p rift-mcp --test live_semantic_search

release-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_release.py

installer-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_installers.py

rust-gate: format dashes generate-check check clippy docs audit test release-test installer-test

# One signed tag on the commit `origin/main` names right now. The recipe reads
# that commit from the remote, so the local checkout's branch and its uncommitted
# work decide nothing. Pushing the tag starts `rift-release`: six target
# archives, the checksum manifest, the GitHub release, and the docs deploy.
release tag:
    #!/usr/bin/env bash
    set -euo pipefail
    tag="{{ tag }}"
    if [[ ! "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
        echo "error: release tag must match vX.Y.Z: $tag" >&2
        exit 1
    fi
    if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
        echo "error: origin already carries $tag" >&2
        exit 1
    fi
    git fetch --quiet origin main
    commit="$(git rev-parse FETCH_HEAD)"
    declared="$(git show "$commit:Cargo.toml" \
        | sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p')"
    if [ "$declared" != "${tag#v}" ]; then
        echo "error: origin/main declares $declared; bump the workspace version before tagging $tag" >&2
        exit 1
    fi
    echo "tagging $(git --no-pager log -1 --format='%h %s' "$commit")"
    git tag --sign --message "Rift $tag" "$tag" "$commit"
    git push --quiet origin "refs/tags/$tag" || { git tag --delete "$tag"; exit 1; }
    echo "$tag pushed; watch with: gh run list --workflow rift-release"
