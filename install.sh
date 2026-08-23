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
target_tmp=$target_dir/.pbi.tmp.$$
provenance_tmp=$target_dir/.pbi.provenance.tmp.$$
link_tmp=$link_dir/.pbi.tmp.$$

cleanup() {
  rm -f -- "$target_tmp" "$provenance_tmp" "$link_tmp"
}
trap cleanup 0 1 2 3 15

mkdir -p "$target_dir"
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
mv -f -- "$target_tmp" "$target"
target_tmp=
mv -f -- "$provenance_tmp" "$provenance"
provenance_tmp=

mkdir -p "$link_dir"
if [ -d "$link" ] && [ ! -L "$link" ]; then
  printf 'install.sh: compatibility path is a directory: %s\n' "$link" >&2
  exit 1
fi
ln -s "$target" "$link_tmp"
mv -f -- "$link_tmp" "$link"
link_tmp=
