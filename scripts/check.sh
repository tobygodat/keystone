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
python3 scripts/guards.py executables
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
  providers)
    # Every test with failing `claude` and `codex` first on PATH. Each logs its call and
    # exits 1, so a test that runs the CLI on PATH, which on a developer's machine is the
    # user's own, fails here even where a test build's own guards miss it (TOB-363).
    fakes="$PWD/.audit/path-providers"
    rm -rf "$fakes"
    mkdir -p "$fakes/bin"
    log="$fakes/calls.log"
    : > "$log"
    for name in claude codex; do
      cat > "$fakes/bin/$name" <<EOF
#!/bin/sh
printf '%s %s (in %s)\n' "$name" "\$*" "\$PWD" >> '$log'
echo 'a test ran the $name on PATH (TOB-363)' >&2
exit 1
EOF
      chmod +x "$fakes/bin/$name"
    done
    export PATH="$fakes/bin:$PATH"
    status=0
    cargo test --workspace --locked --no-fail-fast || status=$?
    # An app still finds the CLI on PATH, whatever its binary is named and whichever
    # features it was built with: the example searches as the app does, built with every
    # target's features and run under a test binary's name.
    cargo build --workspace --all-targets --locked
    app="$fakes/app"
    mkdir -p "$app/bin"
    printf '#!/bin/sh\nexit 0\n' > "$app/bin/claude"
    chmod +x "$app/bin/claude"
    found=
    if ln -f target/debug/examples/find_program "$app/find_program-0123456789abcdef"; then
      found=$(PATH="$app/bin" HOME="$app" "$app/find_program-0123456789abcdef" claude) || true
    fi
    if [[ "$found" != "claude"$'\t'"$app/bin/claude" ]]; then
      echo "An app build under a test binary's name did not find claude on PATH: ${found:-no output}" >&2
      status=1
    else
      echo "An app build under a test binary's name found claude on PATH."
    fi
    # After the workspace build, which leaves the scripted providers built with its own
    # features, as `ui` builds them.
    cargo build -p keystone-agent --bins --locked
    cargo test -p keystone-desktop -p keystone-ui --features keystone-desktop/ui-tests,keystone-ui/ui-tests --locked --no-fail-fast || status=$?
    if [[ -s "$log" ]]; then
      echo "Tests ran the claude or codex on PATH ($(wc -l < "$log") calls, logged in $log):" >&2
      cat "$log" >&2
      exit 1
    fi
    echo "No test ran the claude or codex on PATH."
    exit "$status" ;;
  build)
    cargo build -p keystone-desktop --locked
    cargo build -p keystone-ui --example gallery --locked
    ;;
  apple) bash scripts/apple-check.sh ;;
  linux) bash scripts/linux-check.sh ;;
  *) echo 'Usage: bash scripts/check.sh [core|core-fast|ui|providers|build|apple|linux]' >&2; exit 2 ;;
esac
