#!/bin/sh
set -eu
line=2
case "$*" in
  *compression*) line=1 ;;
esac
printf 'Pattern: fixture\nPath: %s\nFile: %s/src/lib.rs, Lines: %s-%s\n' "$PWD" "$PWD" "$line" "$line"
