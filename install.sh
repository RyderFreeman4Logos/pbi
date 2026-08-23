#!/bin/sh
set -eu

script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
source=${PBI_INSTALL_SOURCE:-$script_dir/pbi}
target=${PBI_INSTALL_TARGET:-/usr/local/bin/pbi}
home=${PBI_INSTALL_HOME:-${HOME:?HOME is required; use PBI_INSTALL_HOME for tests}}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --source)
      [ "$#" -ge 2 ] || { printf '%s\n' 'install.sh: --source requires a path' >&2; exit 2; }
      source=$2
      shift 2
      ;;
    --target)
      [ "$#" -ge 2 ] || { printf '%s\n' 'install.sh: --target requires a path' >&2; exit 2; }
      target=$2
      shift 2
      ;;
    --home)
      [ "$#" -ge 2 ] || { printf '%s\n' 'install.sh: --home requires a path' >&2; exit 2; }
      home=$2
      shift 2
      ;;
    *)
      printf 'install.sh: unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

[ -f "$source" ] && [ -x "$source" ] || {
  printf 'install.sh: source executable is missing or invalid: %s\n' "$source" >&2
  exit 1
}
[ -n "$target" ] || { printf '%s\n' 'install.sh: target must not be empty' >&2; exit 2; }
[ -n "$home" ] || { printf '%s\n' 'install.sh: home must not be empty' >&2; exit 2; }

target_dir=$(dirname "$target")
provenance=$target.provenance
link_dir=$home/.local/bin
link=$link_dir/pbi
source_dir=$(dirname "$source")
source_commit=unknown
if source_commit=$(git -C "$source_dir" rev-parse --verify HEAD 2>/dev/null); then
  :
else
  source_commit=unknown
fi
source_sha=$(sha256sum "$source" | cut -d' ' -f1)
if { [ -e "$target" ] || [ -L "$target" ]; } && { [ ! -f "$target" ] || [ -L "$target" ]; }; then
  printf 'install.sh: target is not a replaceable regular file: %s\n' "$target" >&2
  exit 1
fi
if { [ -e "$provenance" ] || [ -L "$provenance" ]; } && { [ ! -f "$provenance" ] || [ -L "$provenance" ]; }; then
  printf 'install.sh: provenance is not a replaceable regular file: %s\n' "$provenance" >&2
  exit 1
fi
target_txn=
link_txn=
target_tmp=
provenance_tmp=
link_tmp=
target_backup=
provenance_backup=
link_backup=
prior_target=false
prior_provenance=false
prior_link=false
transaction_started=false
committed=false

rollback_path() {
  public=$1
  temporary=$2
  backup=$3
  prior_present=$4
  if [ -n "$backup" ] && { [ -e "$backup" ] || [ -L "$backup" ]; }; then
    rm -f -- "$public"
    mv -fT -- "$backup" "$public"
  elif [ "$prior_present" = false ] &&
       [ -n "$temporary" ] && [ ! -e "$temporary" ] && [ ! -L "$temporary" ]; then
    rm -f -- "$public"
  fi
}

cleanup() {
  status=$1
  trap - 0 1 2 3 15
  set +e
  if [ "$committed" != true ] && [ "$transaction_started" = true ]; then
    rollback_path "$link" "$link_tmp" "$link_backup" "$prior_link"
    rollback_path "$provenance" "$provenance_tmp" "$provenance_backup" "$prior_provenance"
    rollback_path "$target" "$target_tmp" "$target_backup" "$prior_target"
  fi
  rm -f -- "$target_tmp" "$provenance_tmp" "$link_tmp"
  if [ "$committed" = true ]; then
    [ -z "$target_txn" ] || rm -rf -- "$target_txn"
    [ -z "$link_txn" ] || rm -rf -- "$link_txn"
  else
    [ -z "$target_txn" ] || rmdir -- "$target_txn" 2>/dev/null
    [ -z "$link_txn" ] || rmdir -- "$link_txn" 2>/dev/null
  fi
  exit "$status"
}
trap 'cleanup "$?"' 0
trap 'cleanup 129' 1
trap 'cleanup 130' 2
trap 'cleanup 131' 3
trap 'cleanup 143' 15

mkdir -p "$target_dir" "$link_dir"
if { [ -e "$link" ] || [ -L "$link" ]; } && [ ! -L "$link" ]; then
  printf 'install.sh: compatibility path is not a replaceable symlink: %s\n' "$link" >&2
  exit 1
fi
[ -e "$target" ] && prior_target=true
[ -e "$provenance" ] && prior_provenance=true
[ -L "$link" ] && prior_link=true
target_txn=$(mktemp -d "$target_dir/.pbi.transaction.XXXXXX")
link_txn=$(mktemp -d "$link_dir/.pbi.transaction.XXXXXX")
target_tmp=$target_txn/.pbi.tmp.$$
provenance_tmp=$target_txn/.pbi.provenance.tmp.$$
link_tmp=$link_txn/.pbi.tmp.$$
target_backup=$target_txn/target.backup
provenance_backup=$target_txn/provenance.backup
link_backup=$link_txn/link.backup
install -D -m 0755 "$source" "$target_tmp"
[ -f "$target_tmp" ] && [ ! -L "$target_tmp" ] && [ -x "$target_tmp" ] || {
  printf 'install.sh: failed to create executable temporary target: %s\n' "$target_tmp" >&2
  exit 1
}
installed_sha=$(sha256sum "$target_tmp" | cut -d' ' -f1)
[ "$installed_sha" = "$source_sha" ] || {
  printf '%s\n' 'install.sh: installed bytes changed during copy' >&2
  exit 1
}
printf 'source_commit=%s\nsha256=%s\ntarget=%s\n' \
  "$source_commit" "$installed_sha" "$target" > "$provenance_tmp"
ln -s "$target" "$link_tmp"

transaction_started=true
if [ -e "$target" ]; then mv -fT -- "$target" "$target_backup"; fi
mv -fT -- "$target_tmp" "$target"
[ -f "$target" ] && [ ! -L "$target" ] && [ -x "$target" ] || {
  printf 'install.sh: published target is not a regular executable: %s\n' "$target" >&2
  exit 1
}
if [ -e "$provenance" ]; then mv -fT -- "$provenance" "$provenance_backup"; fi
mv -fT -- "$provenance_tmp" "$provenance"
if [ -L "$link" ]; then mv -fT -- "$link" "$link_backup"; fi
mv -fT -- "$link_tmp" "$link"
[ -f "$provenance" ] && [ ! -L "$provenance" ] && [ -L "$link" ] || {
  printf '%s\n' 'install.sh: publication validation failed' >&2
  exit 1
}
committed=true
