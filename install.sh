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
target_tmp=$target_dir/.pbi.tmp.$$
provenance_tmp=$target_dir/.pbi.provenance.tmp.$$
link_tmp=$link_dir/.pbi.tmp.$$
target_backup=$target_dir/.pbi.backup.$$
provenance_backup=$target_dir/.pbi.provenance.backup.$$
link_backup=$link_dir/.pbi.backup.$$
had_target=false
had_provenance=false
had_link=false
published_target=false
published_provenance=false
published_link=false
committed=false

cleanup() {
  status=$?
  trap - 0 1 2 3 15
  set +e
  if [ "$committed" != true ]; then
    if [ "$had_link" = true ]; then rm -f -- "$link"; mv -fT -- "$link_backup" "$link"; elif [ "$published_link" = true ]; then rm -f -- "$link"; fi
    if [ "$had_provenance" = true ]; then rm -f -- "$provenance"; mv -fT -- "$provenance_backup" "$provenance"; elif [ "$published_provenance" = true ]; then rm -f -- "$provenance"; fi
    if [ "$had_target" = true ]; then rm -f -- "$target"; mv -fT -- "$target_backup" "$target"; elif [ "$published_target" = true ]; then rm -f -- "$target"; fi
  fi
  rm -f -- "$target_tmp" "$provenance_tmp" "$link_tmp" \
    "$target_backup" "$provenance_backup" "$link_backup"
  exit "$status"
}
trap cleanup 0 1 2 3 15

mkdir -p "$target_dir" "$link_dir"
if { [ -e "$link" ] || [ -L "$link" ]; } && [ ! -L "$link" ]; then
  printf 'install.sh: compatibility path is not a replaceable symlink: %s\n' "$link" >&2
  exit 1
fi
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

if [ -e "$target" ]; then mv -fT -- "$target" "$target_backup"; had_target=true; fi
mv -fT -- "$target_tmp" "$target"
target_tmp=
published_target=true
[ -f "$target" ] && [ ! -L "$target" ] && [ -x "$target" ] || {
  printf 'install.sh: published target is not a regular executable: %s\n' "$target" >&2
  exit 1
}
if [ -e "$provenance" ]; then mv -fT -- "$provenance" "$provenance_backup"; had_provenance=true; fi
mv -fT -- "$provenance_tmp" "$provenance"
provenance_tmp=
published_provenance=true
if [ -L "$link" ]; then mv -fT -- "$link" "$link_backup"; had_link=true; fi
mv -fT -- "$link_tmp" "$link"
link_tmp=
published_link=true
[ -f "$provenance" ] && [ ! -L "$provenance" ] && [ -L "$link" ] || {
  printf '%s\n' 'install.sh: publication validation failed' >&2
  exit 1
}
committed=true
