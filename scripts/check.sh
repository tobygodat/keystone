#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "${1:-}" == --unmeasured ]]; then
  shift
elif command -v python3 >/dev/null; then
  exec python3 scripts/dev-timing.py run --label "check ${1:-core}" -- bash scripts/check.sh --unmeasured "$@"
fi
cargo fmt --all --check
python3 scripts/guards.py colors
python3 scripts/guards.py shortcuts
python3 scripts/test_guards.py
python3 scripts/test_ui_strings.py
python3 scripts/ui-strings.py --check
python3 scripts/guards.py hints
case "${1:-core}" in
  core)
    cargo test -p keystone-core -p keystone-agent --locked
    bash scripts/case-insensitive-tests.sh ;;
  core-fast)
    if ! command -v cargo-nextest >/dev/null; then
      echo 'core-fast needs cargo-nextest, which is not on PATH. Install it from https://nexte.st/docs/installation/pre-built-binaries/ or run `bash scripts/check.sh core`.' >&2
      exit 2
    fi
    cargo nextest run -p keystone-core -p keystone-agent --locked --test-threads 16 --retries 0
    bash scripts/case-insensitive-tests.sh
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
  linux) bash scripts/linux-check.sh ;;
  *) echo 'Usage: bash scripts/check.sh [core|core-fast|ui|build|apple|linux]' >&2; exit 2 ;;
esac
