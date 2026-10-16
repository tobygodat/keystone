#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "${1:-}" == --unmeasured ]]; then
  shift
elif command -v python3 >/dev/null; then
  exec python3 scripts/dev-timing.py run --label "check ${1:-core}" -- bash scripts/check.sh --unmeasured "$@"
fi
cargo fmt --all --check
# Matches GPUI color constructors and Hsla or Rgba struct literals, but not `-> Hsla {` signatures.
if grep -rnP '\b(rgba?|hsla|black|white|transparent_black|transparent_white|red|blue|green|yellow|opaque_grey)\(|(?<!-> )(?<!for )\b(Hsla|Rgba)\s*\{' crates --include='*.rs' | grep -v '^crates/keystone-ui/src/theme.rs:'; then
  echo 'Colors belong in crates/keystone-ui/src/theme.rs. Use a theme token or Color.' >&2
  exit 1
fi
case "${1:-core}" in
  core) cargo test -p keystone-core -p keystone-agent --locked ;;
  core-fast)
    if ! command -v cargo-nextest >/dev/null; then
      echo 'core-fast needs cargo-nextest, which is not on PATH. Install it from https://nexte.st/docs/installation/pre-built-binaries/ or run `bash scripts/check.sh core`.' >&2
      exit 2
    fi
    cargo nextest run -p keystone-core -p keystone-agent --locked --test-threads 16 --retries 0
    cargo test -p keystone-core -p keystone-agent --locked --doc
    ;;
  ui)
    cargo build -p keystone-agent --bins --locked
    cargo test -p keystone-desktop -p keystone-ui --features keystone-desktop/ui-tests,keystone-ui/ui-tests --locked ;;
  build)
    cargo build -p keystone-desktop --locked
    cargo build -p keystone-ui --example gallery --locked
    ;;
  *) echo 'Usage: bash scripts/check.sh [core|core-fast|ui|build]' >&2; exit 2 ;;
esac
