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
# A Ctrl shortcut is Cmd on macOS, and GPUI's secondary- modifier is both.
if grep -rnP '"(?:[^"\n]*[ -])?ctrl-(?!(?:shift-)?tab\b)' crates --include='*.rs'; then
  echo 'Spell shortcuts secondary-… (Cmd on macOS, Ctrl elsewhere). Only ctrl-tab and ctrl-shift-tab stay ctrl-.' >&2
  exit 1
fi
python3 scripts/test_ui_strings.py
python3 scripts/ui-strings.py --check
# A shortcut hint written out as Ctrl+… reads wrong on macOS.
if awk '/^crates\// { file = $0 } /Ctrl[+ ]/ && file != "crates/keystone-agent/src/lib.rs" && file != "crates/keystone-ui/src/kbd.rs" { print file ": " $0; found = 1 } END { exit !found }' docs/ui-strings.txt; then
  echo 'Write shortcut hints with keystone_ui::shortcut_text("secondary-…"), which shows Ctrl+… on Linux and ⌘ on macOS.' >&2
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
  apple) bash scripts/apple-check.sh ;;
  *) echo 'Usage: bash scripts/check.sh [core|core-fast|ui|build|apple]' >&2; exit 2 ;;
esac
