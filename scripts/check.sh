#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all --check
if grep -rnE '\b(rgba?|hsla|black|white|transparent_black|transparent_white|red|blue|green|yellow|opaque_grey)\(' crates --include='*.rs' | grep -v '^crates/keystone-ui/src/theme.rs:'; then
  echo 'Colors belong in crates/keystone-ui/src/theme.rs. Use a theme token or Color.' >&2
  exit 1
fi
case "${1:-core}" in
  core) cargo test -p keystone-core --locked ;;
  ui) cargo test -p keystone-desktop -p keystone-ui --features keystone-desktop/ui-tests,keystone-ui/ui-tests --locked ;;
  build)
    cargo build -p keystone-desktop --locked
    cargo build -p keystone-ui --example gallery --locked
    ;;
  *) echo 'Usage: bash scripts/check.sh [core|ui|build]' >&2; exit 2 ;;
esac
