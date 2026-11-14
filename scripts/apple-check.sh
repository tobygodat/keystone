#!/usr/bin/env bash
# Type-checks every crate for Apple Silicon macOS from Linux with warnings as errors:
# once as users build it, and once with tests and every feature. It catches code that
# does not compile on macOS: a `cfg(target_os = "linux")` item with no macOS arm, a
# Linux-only libc call, a variable left unused by a cfg. It links nothing, C sources
# (SQLite) compile to empty objects, and the Metal shaders that only a macOS build
# script compiles are empty placeholders. So it cannot catch a link error, a shader
# error, a crash, or anything that differs at run time. Only a Mac can.
set -euo pipefail
cd "$(dirname "$0")/.."

# Run with arguments, it is the C compiler: each object it writes is empty.
if [[ $# -gt 0 ]]; then
  out=
  while [[ $# -gt 0 ]]; do
    [[ "$1" == -o ]] && out=$2
    shift
  done
  [[ -n "$out" ]] || exit 1
  exec clang --target=arm64-apple-macos11 -x c -c /dev/null -o "$out"
fi

target=aarch64-apple-darwin
if ! rustup target list --installed | grep -qx "$target"; then
  echo "Run \`rustup target add $target\` first." >&2
  exit 2
fi
command -v clang >/dev/null || { echo 'apple-check needs clang.' >&2; exit 2; }
ar=$(command -v llvm-ar || true)
[[ -n "$ar" ]] || { echo 'apple-check needs llvm-ar.' >&2; exit 2; }

export CC_aarch64_apple_darwin="$PWD/scripts/apple-check.sh"
export AR_aarch64_apple_darwin="$ar"
export RUSTFLAGS="${RUSTFLAGS:-} -D warnings"
dir="${CARGO_TARGET_DIR:-target}/apple-check"
mkdir -p "$dir"
log="$dir/check.log"

# gpui-pre-apple's build script compiles the shaders only on a macOS host. Give it
# empty files where it would have written them, then check again. Cargo 1.100 moved a
# build script's output from build/<package>-<hash>/out to build/<package>/<hash>/out.
placeholders() {
  for out in "$dir/$target"/debug/build/gpui-pre-apple-*/out \
    "$dir/$target"/debug/build/gpui-pre-apple/*/out; do
    [[ -d "$out" ]] || continue
    [[ -e "$out/shaders.metallib" ]] || : >"$out/shaders.metallib"
    [[ -e "$out/stitched_shaders.metal" ]] || : >"$out/stitched_shaders.metal"
  done
}

check() {
  local args=(check --target "$target" --target-dir "$dir" --locked --workspace "$@")
  placeholders
  if cargo "${args[@]}" 2>"$log"; then
    return
  fi
  if grep -q 'shaders.metallib\|stitched_shaders.metal' "$log"; then
    placeholders
    cargo "${args[@]}"
  else
    cat "$log" >&2
    exit 1
  fi
}

# Without font-kit GPUI's macOS text system draws no text at all, and says so only in a
# log Keystone does not show. A type check cannot see that, so look at the features.
if ! cargo tree --locked --target "$target" -e features -p keystone-desktop -i gpui-pre-macos |
  grep -q 'gpui-pre-platform feature "font-kit"'; then
  echo 'keystone-desktop must turn on gpui_platform/font-kit on macOS, or no text renders there.' >&2
  exit 1
fi

check
check --all-targets --features keystone-desktop/ui-tests,keystone-desktop/capture,keystone-ui/ui-tests
echo "apple-check: every crate type-checks for $target"
