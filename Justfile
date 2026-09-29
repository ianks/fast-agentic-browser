# List the recipes.
default:
    @just --list

# Build fab and fab-bench, and run every test.
test:
    cargo test --workspace --locked --all-features

# Publish the crates whose version is not on crates.io yet (fab-core first).
publish: test
    #!/usr/bin/env bash
    set -euo pipefail
    [ -z "$(git status --porcelain)" ] || { echo "the tree has uncommitted changes"; exit 1; }
    [ "$(git branch --show-current)" = main ] || { echo "publish from main"; exit 1; }
    git fetch -q origin main
    [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "main differs from origin/main: push or pull first"; exit 1; }
    for crate in fab-core fast-agentic-browser; do
        version=$(cargo metadata --no-deps --format-version 1 | python3 -c "import json,sys; print(next(p['version'] for p in json.load(sys.stdin)['packages'] if p['name'] == '$crate'))")
        if curl -sf -A "fab-publish (github.com/ianks/fast-agentic-browser)" "https://crates.io/api/v1/crates/$crate/$version" >/dev/null; then
            echo "$crate $version is already on crates.io"
        else
            echo "publishing $crate $version"
            cargo publish --locked -p "$crate"
        fi
    done
