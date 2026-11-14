#!/usr/bin/env bash
# Model-checks RemoteCursor.tla with TLC: every rule on the code as it is, and each
# deliberate break that must make its rule fail. Prints one line per run and exits
# non-zero when any result differs from EXPECT.
#
#   bash crates/keystone-desktop/.agents/skills/cursor-model/check.sh          # all runs
#   bash crates/keystone-desktop/.agents/skills/cursor-model/check.sh --drift  # covered code changed since SYNC?
#   bash crates/keystone-desktop/.agents/skills/cursor-model/check.sh --trace RULE MUTATION BOUNDS
#                                                     # one run, TLC's full output
#
# Needs mise (https://mise.jdx.dev) for Java, and the network on the first run.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(git -C "$here" rev-parse --show-toplevel)

# The commit at which the model was last confirmed to describe the code below.
SYNC=b5942e54f
COVERED=(
  crates/keystone-agent/src/remote/link.rs
  crates/keystone-agent/src/remote/connection.rs
  crates/keystone-agent/src/remote/serve.rs
  crates/keystone-agent/src/remote/journal.rs
  crates/keystone-agent/src/log.rs
  crates/keystone-core/src/database.rs
  crates/keystone-desktop/src/app_state.rs
)
FUNCTIONS=(
  route drain ack saved with_cursors resend connect_once read drain_placed
  replay hello append_logs remote_cursors append_through retry through
  pump receive receive_placed commit open_link watch_links new adopt end_lost_turns
  journal_changes has_events open id went_back rotate seen journal_events take deliver
  refusal
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
  echo "Untracked Rust files in keystone-agent/src/remote and keystone-desktop/src:"
  untracked=$(git -C "$root" ls-files --others --exclude-standard -- \
    crates/keystone-agent/src/remote crates/keystone-desktop/src | grep '\.rs$' || true)
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

# TLC's state files are large; /tmp on this kind of machine has a small quota.
mkdir -p "$cache"
work=$(mktemp -d "$cache/cursor-model-work.XXXXXX")
trap 'rm -rf "$work"' EXIT

# Sets of bounds, as "events-per-journal resets rewinds crashes kills drops grows-back". The
# full product is too large to explore (over 9 minutes and 7 GB for three events, one
# reset, one kill and one drop, without finishing), so each set spends its budget
# where it matters:
#   kills:  one journal, 4 events, 2 kills, 1 drop. About 5.5 million distinct states.
#   reset:  2 events in each of 2 journals, 1 replacement, 1 kill, 1 drop.
#   rewind: 2 events, the journal goes back once and takes a new identity when the
#           app shows it a place past its latest, 1 kill, 1 drop. It numbers no new
#           event before the app next connects.
#   grows:  as rewind, but it may number new events before the app connects, as its
#           own restart events would. Then nothing in it is behind the app's place,
#           and a journal that went back can't be told from one that didn't.
#   crash:  2 events, the host crashes or loses power once at any point, losing only
#           commits it never sent, and numbers its restart events at once. 1 kill,
#           1 drop.
# Every session and row assignment is checked in each.
declare -A BOUNDS=(
  [kills]="4 0 0 0 2 1 FALSE"
  [reset]="2 1 0 0 1 1 FALSE"
  [rewind]="2 0 1 0 1 1 FALSE"
  [grows]="2 0 1 0 1 1 TRUE"
  [crash]="2 0 0 1 1 1 FALSE"
)

# One run: a rule, the Mutation constant, the bounds, and the expected result.
#   holds  - TLC finds no violation
#   breaks - TLC finds one (a known finding, or a break that proves the rule can fail)
EXPECT=(
  # rule          mutation       bounds  expected  why
  "NothingLost    none           kills   holds     code"
  "SavedOnce      none           kills   holds     code"
  "SavedInOrder   none           kills   holds     code"
  "NothingLost    none           reset   holds     code"
  "SavedOnce      none           reset   holds     code"
  "SavedInOrder   none           reset   holds     code"
  "NothingLost    none           rewind  holds     code"
  "SavedOnce      none           rewind  holds     code"
  "SavedInOrder   none           rewind  holds     code"
  "NothingLost    none           grows   breaks    finding (TOB-455): a journal that went back and grew to the app's place before it reconnected"
  "NothingLost    none           crash   holds     code"
  "SavedOnce      none           crash   holds     code"
  "SavedInOrder   none           crash   holds     code"
  "NothingLost    ack-on-drain   kills   breaks    can fail"
  "NothingLost    cursor-apart   kills   breaks    can fail"
  "SavedOnce      no-skip        kills   breaks    can fail"
  "SavedInOrder   no-skip        kills   breaks    can fail"
  "NothingLost    cross-journal  reset   breaks    can fail: round one's cursors across journals"
  "NothingLost    no-rotate      rewind  breaks    can fail: a journal that went back keeps its identity"
  "NothingLost    lossy-crash    crash   breaks    can fail: a host crash takes back commits it had sent"
)

config() {  # rule mutation bounds
  local events resets rewinds crashes kills drops grows
  read -r events resets rewinds crashes kills drops grows <<<"${BOUNDS[$3]}"
  cat <<EOF
CONSTANTS
    N = $events
    MaxResets = $resets
    MaxRewinds = $rewinds
    MaxCrashes = $crashes
    MaxKills = $kills
    MaxDrops = $drops
    GrowsBack = $grows
    Mutation = "$2"
SPECIFICATION Spec
INVARIANT TypeOK
INVARIANT $1
EOF
}

tlc() {  # dir
  mkdir -p "$1/tmp"  # TLC unpacks its standard modules here; a shared folder races.
  (cd "$1" && "$java" -XX:+UseParallelGC -Djava.io.tmpdir="$1/tmp" -cp "$jar" tlc2.TLC \
    -workers 2 -deadlock -difftrace -metadir "$1/states" -config RemoteCursor.cfg \
    RemoteCursor.tla >"$1/tlc.out" 2>&1) || true
}

if [[ ${1:-} == --trace ]]; then
  [[ $# == 4 ]] || { echo "usage: check.sh --trace RULE MUTATION BOUNDS" >&2; exit 2; }
  dir=$work/trace
  mkdir -p "$dir"
  config "$2" "$3" "$4" >"$dir/RemoteCursor.cfg"
  cp "$here/RemoteCursor.tla" "$dir/"
  tlc "$dir"
  cat "$dir/tlc.out"
  exit 0
fi

# Outputs of UNEXPECTED lines, one folder per checkout so worktrees don't clobber each other.
outputs=$cache/cursor-model/$(printf %s "$root" | sha256sum | cut -c1-12)
rm -rf "$outputs"  # what a previous run in this checkout left

run() {
  local rule=$1 mutation=$2 bounds=$3 dir=$work/$1-$2-$3
  mkdir -p "$dir"
  config "$rule" "$mutation" "$bounds" >"$dir/RemoteCursor.cfg"
  cp "$here/RemoteCursor.tla" "$dir/"
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
  read -r rule mutation bounds _ <<<"${EXPECT[$i]}"
  (run "$rule" "$mutation" "$bounds" >"$work/result-$i") &
  pids+=($!)
done
wait "${pids[@]}"

printf '%-14s %-14s %-7s %-7s %s\n' RULE MUTATION BOUNDS RESULT EXPECTED
for i in "${!EXPECT[@]}"; do
  read -r rule mutation bounds expected why <<<"${EXPECT[$i]}"
  got=$(cat "$work/result-$i")
  mark=ok
  if [[ $got != "$expected" ]]; then
    mark=UNEXPECTED
    failed=1
    mkdir -p "$outputs"
    keep=$outputs/$rule-$mutation-$bounds.out
    cp "$work/$rule-$mutation-$bounds/tlc.out" "$keep"
  fi
  printf '%-14s %-14s %-7s %-7s %-7s %s %s\n' "$rule" "$mutation" "$bounds" "$got" "$expected" "$mark" "$why"
  [[ $mark == UNEXPECTED ]] && echo "    TLC output: $keep"
done
exit $failed
