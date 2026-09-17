set dotenv-load := false

format:
    cargo fmt --all --check

generate:
    cargo run -q -p rift-mcp --bin rift-schema-export -- docs plugins/claude
    printf '$ rift --help\n' > docs/public/cli-help.txt
    cargo run -q -p rift -- --help >> docs/public/cli-help.txt
    printf '\n$ rift server --help\n' >> docs/public/cli-help.txt
    cargo run -q -p rift -- server --help >> docs/public/cli-help.txt
    printf '\n$ rift server logs --help\n' >> docs/public/cli-help.txt
    cargo run -q -p rift -- server logs --help >> docs/public/cli-help.txt

generate-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo run -q -p rift-mcp --bin rift-schema-export -- --check docs plugins/claude
    fresh="$(mktemp)"
    trap 'rm -f "$fresh"' EXIT
    printf '$ rift --help\n' > "$fresh"
    cargo run -q -p rift -- --help >> "$fresh"
    printf '\n$ rift server --help\n' >> "$fresh"
    cargo run -q -p rift -- server --help >> "$fresh"
    printf '\n$ rift server logs --help\n' >> "$fresh"
    cargo run -q -p rift -- server logs --help >> "$fresh"
    cmp -s docs/public/cli-help.txt "$fresh" || {
        echo "error: \`docs/public/cli-help.txt\` does not match the CLI help; regenerate it with \`just generate\`" >&2
        exit 1
    }

check:
    cargo metadata --locked --format-version 1 > /dev/null
    cargo check --workspace --all-targets --all-features --locked
    uv run --locked --project dev rift-dev rust-architecture

# The em-dash ban, over every surface a reader meets: the docs pages, the
# app shell and the components it renders, the prose inside the crates, the
# README, the artifacts `just generate` writes, and the CI configuration's
# own comments. The scanner itself is not among them: it spells the banned
# characters.
dashes:
    uv run --locked --project dev rift-dev dashes \
        docs/content docs/src/app docs/src/components crates README.md \
        docs/public .github plugins .claude-plugin

# The MCP specification's own conformance runner, over a throwaway workspace
# one foreground server serves. `tools/mcp-conformance/expected-failures.yml`
# carries the scenarios the served surface fails today; anything else fails
# the gate.
conformance binary="":
    uv run --locked --project dev rift-dev conformance {{ if binary == "" { "" } else { "--binary " + quote(binary) } }}


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
    #!/usr/bin/env bash
    set -euo pipefail
    git worktree list --porcelain | sed -n 's/^worktree //p' | while read -r tree; do
        if [ -f "$tree/Cargo.toml" ]; then
            echo "cleaning $tree"
            cargo clean --manifest-path "$tree/Cargo.toml" || echo "skipped $tree: broken checkout"
        fi
    done

# Archive unit tests once; execution jobs reuse the compiled binaries. The
# execution job's limit counts the transfer as well as the run, so the archive
# carries less of both: the filterset leaves out the binaries `profile.ci`
# never runs, and the compression level trades build time for bytes. Measured
# over one revision, the two together take 1.161gb to 982mb, and level 9 costs
# 17 seconds where level 19 costs six minutes for 15% more.
fast-archive:
    cargo llvm-cov nextest-archive --workspace --all-targets --all-features --locked --profile ci --archive-file target/fast.tar.zst --zstd-level 9 -E 'not binary(/^corpus_/) and not binary(/^live_/)'

# The directory cargo-llvm-cov builds into and nextest extracts an archive into.
# Nextest will not create it, so it exists before an archive run. Cargo writes a
# target directory's `CACHEDIR.TAG` only when it creates that directory itself,
# and cargo-llvm-cov refuses to clean stale objects out of one carrying no tag:
# a report taken over an uncleaned directory counts every source file twice,
# once from a stale object with no hits, and the floor fails on a green suite.
[private]
coverage-target:
    #!/usr/bin/env bash
    set -euo pipefail
    target="${CARGO_LLVM_COV_TARGET_DIR:-target/llvm-cov-target}"
    mkdir -p "$target"
    tag="$target/CACHEDIR.TAG"
    if [ ! -f "$tag" ]; then
        {
            echo "Signature: 8a477f597d28d172789f06886806bc55"
            echo "# This file is a cache directory tag created by cargo."
            echo "# For information about cache directory tags see https://bford.info/cachedir/"
        } > "$tag"
    fi

# Unit tests use local fixtures and require no language servers or model downloads.
test archive="": coverage-target
    cargo llvm-cov nextest {{ if archive == "" { "--workspace --all-targets --all-features --locked" } else { "--archive-file " + quote(archive) + " --extract-overwrite --workspace-remap ." } }} --profile ci --no-tests fail --lcov --output-path lcov.info --fail-under-lines 86

# Live integrations share the corpus archive and its optimized Cargo profile.
live-test archive="": coverage-target
    RIFT_ENGINE_LIVE=1 RIFT_SEARCH_LIVE=1 cargo llvm-cov nextest --no-report --profile integration --no-tests fail {{ if archive == "" { "--workspace --all-targets --all-features --locked --cargo-profile corpus" } else { "--archive-file " + quote(archive) + " --extract-overwrite --workspace-remap ." } }} -E 'binary(/^live_/) or test(/^live_/)'

release-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_release.py

installer-test:
    uv run --locked --project tools/rift-release pytest tools/rift-release/tests/test_installers.py

testing-check:
    uv run --locked --python 3.12 --project dev ruff check dev
    uv run --locked --python 3.12 --project dev ty check dev
    uv run --locked --python 3.12 --project dev pytest dev/tests

corpus-sync name="":
    uv run --locked --python 3.12 --project dev rift-dev corpus sync {{ if name == "" { "" } else { quote(name) } }}

# One archive supplies every integration job. Save the plain CLI before test builds.
integration-archive:
    cargo build --locked --profile corpus -p rift
    tar --zstd -cf target/integration-cli.tar.zst -C target/corpus rift
    cargo llvm-cov nextest-archive --workspace --all-targets --all-features --locked --cargo-profile corpus --profile integration --archive-file target/integration.tar.zst

corpus-test name test_name="" archive="": coverage-target
    cargo llvm-cov nextest --no-report --profile corpus --no-tests fail --run-ignored all {{ if archive == "" { "--locked -p rift --test " + quote("corpus_" + name) + " --cargo-profile corpus" } else { "--archive-file " + quote(archive) + " --extract-overwrite --workspace-remap . -E " + quote("binary(=corpus_" + name + ")") } }} {{ if test_name == "" { "" } else { "-- --exact " + quote(test_name) } }}

artifact-test *args:
    uv run --locked --python 3.12 --project dev rift-dev artifact {{ args }}

agent-test *args:
    uv run --locked --python 3.12 --project dev rift-dev agent {{ args }}

coldstart-test *args:
    uv run --locked --python 3.12 --project dev rift-dev coldstart {{ args }}

# Corpus servers run sequentially on development machines.
integration-test:
    just corpus-sync
    just corpus-test fastapi
    just corpus-test bun
    just corpus-test nextjs
    just live-test
    just artifact-test
    just agent-test
    just conformance

rust-gate: format dashes generate-check check clippy docs doctest audit test release-test installer-test testing-check

# One signed tag on the commit `origin/main` names right now. The recipe reads
# that commit from the remote, so the local checkout's branch and its uncommitted
# work decide nothing. Pushing the tag starts `rift-release`: six target
# archives, the checksum manifest, the GitHub release, and the docs deploy.
release tag:
    #!/usr/bin/env bash
    set -euo pipefail
    tag={{ quote(tag) }}
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
