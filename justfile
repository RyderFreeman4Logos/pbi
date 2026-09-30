set shell := ["bash", "-euo", "pipefail", "-c"]

# Cargo must stay behind ionice and the repository's canonical just recipes.
fmt:
    ionice -c 3 rustfmt --edition 2021 --check src/lib.rs src/main.rs src/semantic.rs src/relevance_scope.rs tests/search_options.rs tests/relevance.rs

fmt-fix:
    ionice -c 3 rustfmt --edition 2021 src/lib.rs src/main.rs src/semantic.rs src/relevance_scope.rs tests/search_options.rs tests/relevance.rs

lock:
    ionice -c 3 cargo generate-lockfile --offline

lock-online:
    ionice -c 3 cargo generate-lockfile

test:
    ionice -c 3 cargo test --locked

test-search-options filter='':
    ionice -c 3 cargo test --locked --test search_options {{filter}}

test-relevance:
    ionice -c 3 cargo test --locked --test relevance

build:
    ionice -c 3 cargo build --locked

clippy:
    ionice -c 3 cargo clippy --locked --all-targets -- -D warnings

gate: fmt test clippy

acceptance: build
    bash scripts/acceptance.sh
