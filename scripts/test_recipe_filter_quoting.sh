#!/bin/sh
# Prove every filter recipe passes its argument as one literal cargo argv.
# Cargo filters stay substrings. An empty filter runs the whole target.
# test-definition joins its module path and the caller filter into the
# one harness FILTER after "--". A second FILTER would OR, so a miss
# would still run the whole module. An empty caller filter is the module.
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

marker="${TMPDIR:?}/pbi-rs-filter-quoting-marker"
out="$marker.out"
rm -f "$marker" "$out"
# Single quotes would let a shell run the touch. The quoted recipe must not.
payload="does_not_exist|;touch ${marker};echo \$HOME \`date\` \"q\" space"

# just --dry-run writes the shell line to stderr. Capture it; do not execute it.
dry() {
    just --dry-run "$@" >"$out" 2>&1
    cat "$out"
}

fail=0
check() {
    recipe=$1
    got=$(dry "$recipe" "$payload") || {
        printf 'dry-run failed for %s\n' "$recipe" >&2
        fail=1
        return
    }
    case $recipe in
        test-definition) needle="'definition_intent_tests::$payload'" ;;
        *) needle="'$payload'" ;;
    esac
    case $got in
        *"$needle"*) ;;
        *)
            printf 'filter was not one literal argument in %s\n' "$recipe" >&2
            fail=1
            ;;
    esac
    case $got in
        *'ionice -c 3 cargo test --locked '*) ;;
        *)
            printf 'recipe lost ionice or cargo in %s\n' "$recipe" >&2
            fail=1
            ;;
    esac
}

check test-lib
check test-bin
check test-definition
check test-search-options

empty=$(dry test-search-options)
case $empty in
    "ionice -c 3 cargo test --locked --test search_options ''") ;;
    *)
        printf 'empty filter changed the recipe:\n%s\n' "$empty" >&2
        fail=1
        ;;
esac

empty_definition=$(dry test-definition)
case $empty_definition in
    "ionice -c 3 cargo test --locked --lib -- 'definition_intent_tests::'") ;;
    *)
        printf 'definition filter is still a cargo argument:\n%s\n' "$empty_definition" >&2
        fail=1
        ;;
esac

plain=$(dry test-search-options caller_failure_receipt)
case $plain in
    "ionice -c 3 cargo test --locked --test search_options 'caller_failure_receipt'") ;;
    *)
        printf 'substring filter was not preserved:\n%s\n' "$plain" >&2
        fail=1
        ;;
esac

selected=$(dry test-definition definition_query_uses_the_real_declaration)
case $selected in
    "ionice -c 3 cargo test --locked --lib -- 'definition_intent_tests::definition_query_uses_the_real_declaration'") ;;
    *)
        printf 'definition selector was not one harness filter:\n%s\n' "$selected" >&2
        fail=1
        ;;
esac

rm -f "$out"
if test -e "$marker"; then
    printf 'dry-run executed the filter payload\n' >&2
    fail=1
fi

test "$fail" -eq 0
printf '%s\n' 'recipe filter quoting: literal argv for all filter recipes'
