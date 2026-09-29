set shell := ["bash", "-euo", "pipefail", "-c"]

# Cargo must stay behind ionice and the repository's canonical just recipes.
fmt:
    ionice -c 3 rustfmt --edition 2021 --check src/lib.rs src/main.rs src/semantic.rs

fmt-fix:
    ionice -c 3 rustfmt --edition 2021 src/lib.rs src/main.rs src/semantic.rs

lock:
    ionice -c 3 cargo generate-lockfile --offline

lock-online:
    ionice -c 3 cargo generate-lockfile

test:
    ionice -c 3 cargo test --locked

build:
    ionice -c 3 cargo build --locked

clippy:
    ionice -c 3 cargo clippy --locked --all-targets -- -D warnings

gate: fmt test clippy

acceptance: build
    bash scripts/acceptance.sh
