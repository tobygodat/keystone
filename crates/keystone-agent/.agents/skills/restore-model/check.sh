#!/usr/bin/env bash
# Model-checks RestoreJournal.tla with TLC: every rule on the code as it is, on the
# candidate fixes, with someone else writing between Keystone's calls, and each
# deliberate break that must make its rule fail. Prints one line per run and exits
# non-zero when any result differs from EXPECT.
#
#   bash crates/keystone-agent/.agents/skills/restore-model/check.sh          # all runs
#   bash crates/keystone-agent/.agents/skills/restore-model/check.sh --drift  # covered code changed since SYNC?
#   bash crates/keystone-agent/.agents/skills/restore-model/check.sh --trace RULE RACES FIX MUTATION
#                                                     # one run, TLC's full output
#
# Needs mise (https://mise.jdx.dev) for Java, and the network on the first run.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(git -C "$here" rev-parse --show-toplevel)

# The commit at which the model was last confirmed to describe the code below.
SYNC=243fec7
COVERED=(
  crates/keystone-agent/src/checkpoint.rs
  crates/keystone-desktop/src/checkpoints.rs
)
FUNCTIONS=(
  restore apply changed_since_read record set_aside delete place undo put_aside_back
  holds_files put_back unfinished write drop hold restore_folders parse fields list
  move_file digest unchanged_since_read restore_checkpoint idle_in
)

if [[ ${1:-} == --drift ]]; then
  # Git's built-in Rust driver names the enclosing fn in each hunk header.
  attributes=$(mktemp)
  trap 'rm -f "$attributes"' EXIT
  echo '*.rs diff=rust' >"$attributes"
  diff=$(git -C "$root" -c core.attributesFile="$attributes" diff -U0 "$SYNC" -- "${COVERED[@]}")
  echo "Covered files changed since $SYNC (committed or not):"
  git -C "$root" diff --stat "$SYNC" -- "${COVERED[@]}"
  echo
  echo "Covered functions with changed lines:"
  changed=0
  for fn in "${FUNCTIONS[@]}"; do
    if grep -qE "^@@.*fn ${fn}[<(]|^[-+].*fn ${fn}[<(]" <<<"$diff"; then
      echo "  $fn"
      changed=1
    fi
  done
  [[ $changed == 0 ]] && echo "  none"
  echo
  echo "Untracked Rust files in keystone-agent/src:"
  untracked=$(git -C "$root" ls-files --others --exclude-standard -- crates/keystone-agent/src \
    | grep '\.rs$' || true)
  echo "${untracked:-  none}" | sed 's/^\([^ ]\)/  \1/'
  echo
  echo "A hunk is matched to the fn its header names, which can be the fn above a"
  echo "change at the top of the next one. Read the whole diff before deciding:"
  echo "  git diff $SYNC -- ${COVERED[*]}"
  exit 0
fi

TLC_VERSION=1.7.4
TLC_SHA256=936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88
JAVA_TOOL=java@temurin-21

cache=${XDG_CACHE_HOME:-$HOME/.cache}/keystone
jar=$cache/tla2tools-$TLC_VERSION.jar
if [[ ! -f $jar ]]; then
  mkdir -p "$cache"
  curl -fsSL -o "$jar.part" \
    "https://github.com/tlaplus/tlaplus/releases/download/v$TLC_VERSION/tla2tools.jar"
  mv "$jar.part" "$jar"
fi
echo "$TLC_SHA256  $jar" | sha256sum -c --quiet - || { echo "tla2tools.jar checksum mismatch" >&2; exit 1; }
java=$(mise which java --tool "$JAVA_TOOL" 2>/dev/null) || {
  mise install "$JAVA_TOOL" >&2
  java=$(mise which java --tool "$JAVA_TOOL")
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Bounds: how often Keystone may stop mid-way, and how many files someone else
# may write. 3 and 3 give about 53,000 states a run. 2 and 1 found the same results.
MAX_CRASHES=3
MAX_USER_WRITES=3

# One run: a rule, the Races, Fix and Mutation constants, and the expected result.
#   holds  - TLC finds no violation
#   breaks - TLC finds one (a known finding, or a break that proves the rule can fail)
EXPECT=(
  # rule                    races  fix    mutation                   expected  why
  "NothingLost              FALSE  FALSE  none                       holds     code"
  "OthersWritesStay         FALSE  FALSE  none                       holds     code"
  "PutBackRestoresFiles     FALSE  FALSE  none                       holds     code"
  "PutBackRestoresFolders   FALSE  FALSE  none                       breaks    finding (TOB-352): a folder made but not yet recorded stays"
  "AsideNeverForgotten      FALSE  FALSE  none                       breaks    finding (TOB-352): a stop after the journal goes hides files aside"
  "NothingLost              TRUE   FALSE  none                       breaks    finding (TOB-353): a rename replaces a file written after its check"
  "OthersWritesStay         TRUE   FALSE  none                       breaks    finding (TOB-353): a file written after its check is moved aside"
  "NothingLost              FALSE  TRUE   none                       holds     candidate fix"
  "OthersWritesStay         FALSE  TRUE   none                       holds     candidate fix"
  "PutBackRestoresFiles     FALSE  TRUE   none                       holds     candidate fix"
  "PutBackRestoresFolders   FALSE  TRUE   none                       holds     candidate fix"
  "AsideNeverForgotten      FALSE  TRUE   none                       holds     candidate fix"
  "NothingLost              FALSE  TRUE   undo-forgets-aside         breaks    can fail"
  "OthersWritesStay         FALSE  TRUE   placed-undo-ignores-stamp  breaks    can fail"
  "PutBackRestoresFiles     FALSE  TRUE   placing-not-undone         breaks    can fail"
  "PutBackRestoresFolders   FALSE  TRUE   made-not-undone            breaks    can fail"
  "AsideNeverForgotten      FALSE  TRUE   unfinished-ignores-aside   breaks    can fail"
  "AsideNeverForgotten      FALSE  TRUE   notice-after-journal       breaks    can fail"
)

config() {  # rule races fix mutation
  cat <<EOF
CONSTANTS
    Races = $2
    Fix = $3
    Mutation = "$4"
    MaxCrashes = $MAX_CRASHES
    MaxUserWrites = $MAX_USER_WRITES
SPECIFICATION Spec
INVARIANT TypeOK
INVARIANT $1
EOF
}

tlc() {  # dir
  mkdir -p "$1/tmp"  # TLC unpacks its standard modules here; a shared folder races.
  (cd "$1" && "$java" -XX:+UseParallelGC -Djava.io.tmpdir="$1/tmp" -cp "$jar" tlc2.TLC \
    -workers 1 -difftrace -metadir "$1/states" -config RestoreJournal.cfg RestoreJournal.tla \
    >"$1/tlc.out" 2>&1) || true
}

if [[ ${1:-} == --trace ]]; then
  [[ $# == 5 ]] || { echo "usage: check.sh --trace RULE RACES FIX MUTATION" >&2; exit 2; }
  dir=$work/trace
  mkdir -p "$dir"
  config "$2" "$3" "$4" "$5" >"$dir/RestoreJournal.cfg"
  cp "$here/RestoreJournal.tla" "$dir/"
  tlc "$dir"
  cat "$dir/tlc.out"
  exit 0
fi

# Outputs of UNEXPECTED lines, one folder per checkout so worktrees don't clobber each other.
outputs=$cache/restore-model/$(printf %s "$root" | sha256sum | cut -c1-12)
rm -rf "$outputs"  # what a previous run in this checkout left

run() {
  local rule=$1 races=$2 fix=$3 mutation=$4 dir=$work/$1-$2-$3-$4
  mkdir -p "$dir"
  config "$rule" "$races" "$fix" "$mutation" >"$dir/RestoreJournal.cfg"
  cp "$here/RestoreJournal.tla" "$dir/"
  tlc "$dir"
  if grep -q "Model checking completed. No error has been found." "$dir/tlc.out"; then
    echo holds
  elif grep -qE "^Error: Invariant $rule is violated" "$dir/tlc.out"; then
    echo breaks
  else
    echo error
  fi
}

failed=0
pids=()
for i in "${!EXPECT[@]}"; do
  read -r rule races fix mutation _ <<<"${EXPECT[$i]}"
  (run "$rule" "$races" "$fix" "$mutation" >"$work/result-$i") &
  pids+=($!)
done
wait "${pids[@]}"

printf '%-24s %-5s %-5s %-26s %-7s %s\n' RULE RACES FIX MUTATION RESULT EXPECTED
for i in "${!EXPECT[@]}"; do
  read -r rule races fix mutation expected why <<<"${EXPECT[$i]}"
  got=$(cat "$work/result-$i")
  mark=ok
  if [[ $got != "$expected" ]]; then
    mark=UNEXPECTED
    failed=1
    mkdir -p "$outputs"
    keep=$outputs/$rule-$races-$fix-$mutation.out
    cp "$work/$rule-$races-$fix-$mutation/tlc.out" "$keep"
  fi
  printf '%-24s %-5s %-5s %-26s %-7s %-7s %s %s\n' \
    "$rule" "$races" "$fix" "$mutation" "$got" "$expected" "$mark" "$why"
  [[ $mark == UNEXPECTED ]] && echo "    TLC output: $keep"
done
exit $failed
