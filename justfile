set shell := ["bash", "-euo", "pipefail", "-c"]

# Cargo must stay behind ionice and the repository's canonical just recipes.
fmt:
    ionice -c 3 rustfmt --edition 2021 --check src/lib.rs src/main.rs src/native_search.rs src/raw_session.rs src/strict_query.rs src/semantic.rs src/explicit_config_route_tests.rs src/debug_config_route_tests.rs src/relevance_scope.rs src/definition_intent_tests.rs src/definition_intent_self_type_tests.rs src/definition_intent_binding_completeness_tests.rs tests/search_options.rs tests/relevance.rs

fmt-fix:
    ionice -c 3 rustfmt --edition 2021 src/lib.rs src/main.rs src/native_search.rs src/raw_session.rs src/strict_query.rs src/semantic.rs src/explicit_config_route_tests.rs src/debug_config_route_tests.rs src/relevance_scope.rs src/definition_intent_tests.rs src/definition_intent_self_type_tests.rs src/definition_intent_binding_completeness_tests.rs tests/search_options.rs tests/relevance.rs

lock:
    ionice -c 3 cargo generate-lockfile --offline

lock-online:
    ionice -c 3 cargo generate-lockfile

test:
    ionice -c 3 cargo test --locked

test-lib filter='':
    ionice -c 3 cargo test --locked --lib {{quote(filter)}}

test-bin filter='':
    ionice -c 3 cargo test --locked --bin pbi-rs {{quote(filter)}}

test-definition filter='':
    ionice -c 3 cargo test --locked --lib definition_intent {{quote(filter)}}

test-search-options filter='':
    ionice -c 3 cargo test --locked --test search_options {{quote(filter)}}

test-relevance:
    ionice -c 3 cargo test --locked --test relevance

build:
    ionice -c 3 cargo build --locked

build-release:
    ionice -c 3 cargo build --locked --release
    cp target/release/pbi-rs target/release/pbi

# Build a Cargo-installed candidate in a caller-owned staging root. Promotion
# to a shared command is a separate, verified operation.
install-stage:
    test -n "${PBI_INSTALL_STAGE:-}"
    test -d "$PBI_INSTALL_STAGE"
    ionice -c 3 cargo install --offline --locked --path . --bin pbi-rs --root "$PBI_INSTALL_STAGE" --target-dir ./target --force

# Copy the release binary to a caller-owned directory as both pbi-rs and pbi.
# Does not replace /usr/local/bin/pbi.
install-release dest:
    test -n "{{dest}}"
    test -d "{{dest}}"
    test -x target/release/pbi-rs
    install -m 0755 target/release/pbi-rs "{{dest}}/pbi-rs"
    install -m 0755 target/release/pbi-rs "{{dest}}/pbi"

clippy:
    ionice -c 3 cargo clippy --locked --all-targets -- -D warnings

gate: fmt test clippy

acceptance: build
    bash scripts/acceptance.sh
