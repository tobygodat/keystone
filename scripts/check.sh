#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all --check
case "${1:-core}" in
  core) cargo test -p keystone-core --locked ;;
  ui) cargo test -p keystone-desktop --features ui-tests --locked ;;
  build) cargo build -p keystone-desktop --locked ;;
  *) echo 'Usage: bash scripts/check.sh [core|ui|build]' >&2; exit 2 ;;
esac
