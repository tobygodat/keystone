#!/usr/bin/env bash
# Type-checks every crate for x86_64 Linux from a Mac with warnings as errors: once as
# users build it, and once with tests and every feature. It catches code that does not
# compile on Linux: a `cfg(target_os = "macos")` item with no Linux arm, a macOS-only
# libc call, a variable left unused by a cfg. It links nothing, C sources (SQLite,
# FreeType) compile to empty objects, and pkg-config answers every query with no flags.
# So it cannot catch a link error, a missing system library, a crash, or anything that
# differs at run time. Only Linux can.
set -euo pipefail
cd "$(dirname "$0")/.."

# Run with arguments, it stands in for the C compiler, the archiver and pkg-config.
# Each object or archive it writes is empty.
if [[ $# -gt 0 ]]; then
  case "$1" in
    --pkg-config)
      [[ " $* " == *" --modversion "* ]] && echo 99
      exit 0 ;;
    --ar)
      for arg in "${@:2}"; do
        [[ "$arg" == *.a ]] && { : >"$arg"; exit 0; }
      done
      exit 0 ;;
  esac
  out=
  while [[ $# -gt 0 ]]; do
    [[ "$1" == -o ]] && out=$2
    shift
  done
  [[ -n "$out" ]] || exit 1
  : >"$out"
  exit 0
fi

target=x86_64-unknown-linux-gnu
if ! rustup target list --installed | grep -qx "$target"; then
  echo "Run \`rustup target add $target\` first." >&2
  exit 2
fi

dir="${CARGO_TARGET_DIR:-target}/linux-check"
mkdir -p "$dir"
stub="$(cd "$dir" && pwd)/stub"
for tool in cc ar pkg-config; do
  printf '#!/usr/bin/env bash\nexec bash %q --%s "$@"\n' "$PWD/scripts/linux-check.sh" "$tool" >"$stub-$tool"
  chmod +x "$stub-$tool"
done
# The compiler stub takes its arguments as they are; `--cc` is not one of them.
printf '#!/usr/bin/env bash\nexec bash %q "$@"\n' "$PWD/scripts/linux-check.sh" >"$stub-cc"

export CC_x86_64_unknown_linux_gnu="$stub-cc"
export CXX_x86_64_unknown_linux_gnu="$stub-cc"
export AR_x86_64_unknown_linux_gnu="$stub-ar"
export PKG_CONFIG="$stub-pkg-config"
export PKG_CONFIG_ALLOW_CROSS=1
export RUSTFLAGS="${RUSTFLAGS:-} -D warnings"

check() {
  cargo check --target "$target" --target-dir "$dir" --locked --workspace "$@"
}

check
check --all-targets --features keystone-desktop/ui-tests,keystone-desktop/capture,keystone-ui/ui-tests
echo "linux-check: every crate type-checks for $target"
