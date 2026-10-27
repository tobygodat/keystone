#!/usr/bin/env bash
# Runs the checkpoint tests that need a case-insensitive folder, as macOS has by default.
# On Linux it mounts a casefolded tmpfs (Linux 6.13 or later) in a private user and mount
# namespace, so it needs no root and leaves nothing mounted.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ "$(uname)" = Darwin ]; then
  folder=$(mktemp -d)
  trap 'rm -rf "$folder"' EXIT
  touch "$folder/a"
  if [ ! -e "$folder/A" ]; then
    echo 'Case-insensitive tests skipped: the temporary folder is on a case-sensitive volume.' >&2
    exit 0
  fi
  rm "$folder/a"
  KEYSTONE_CASE_INSENSITIVE_DIR="$folder" cargo test -p keystone-agent --locked --lib case_insensitive -- --ignored
  exit
fi
binary=$(cargo test -p keystone-agent --locked --lib --no-run --message-format=json |
  python3 -c 'import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if message.get("executable") and message["target"]["name"] == "keystone_agent":
        print(message["executable"])')
mount=$(mktemp -d)
trap 'rmdir "$mount"' EXIT
if ! unshare --user --map-root-user --mount sh -c 'mount -t tmpfs -o casefold tmpfs "$1"' sh "$mount" 2>/dev/null; then
  echo 'Case-insensitive tests skipped: no unprivileged user namespace or casefold tmpfs here.' >&2
  exit 0
fi
unshare --user --map-root-user --mount sh -c '
  mount -t tmpfs -o casefold tmpfs "$1"
  mkdir "$1/folder"
  chattr +F "$1/folder"
  KEYSTONE_CASE_INSENSITIVE_DIR="$1/folder" "$2" case_insensitive --ignored
' sh "$mount" "$binary"
