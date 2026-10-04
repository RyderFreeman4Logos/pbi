#!/bin/sh
# Prove every filter recipe passes its argument as one literal cargo argv.
# Cargo filters stay substrings. An empty filter runs the whole file.
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
    case $got in
        *"'$payload'"*) ;;
        *)
            printf 'filter was not one literal argument in %s:\n%s\n' "$recipe" "$got" >&2
            fail=1
            ;;
    esac
    case $got in
        *'ionice -c 3 cargo test --locked '*) ;;
        *)
            printf 'recipe lost ionice or cargo in %s:\n%s\n' "$recipe" "$got" >&2
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

plain=$(dry test-search-options caller_failure_receipt)
case $plain in
    "ionice -c 3 cargo test --locked --test search_options 'caller_failure_receipt'") ;;
    *)
        printf 'substring filter was not preserved:\n%s\n' "$plain" >&2
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
