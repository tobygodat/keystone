#!/usr/bin/env bash
# Model-checks QuitWhenIdle.tla with TLC: every rule on the code as it is, and each
# deliberate break that must make its rule fail.
# Prints one line per run and exits non-zero when any result differs from EXPECT.
#
#   bash crates/keystone-desktop/.agents/skills/quit-model/check.sh          # all runs
#   bash crates/keystone-desktop/.agents/skills/quit-model/check.sh --drift  # covered code changed since SYNC?
#
# Needs mise (https://mise.jdx.dev) for Java, and the network on the first run.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(git -C "$here" rev-parse --show-toplevel)

# The commit at which the model was last confirmed to describe the code below.
SYNC=11b9013
COVERED=(
  crates/keystone-desktop/src/app_state.rs
  crates/keystone-desktop/src/main.rs
  crates/keystone-desktop/src/delivery.rs
  crates/keystone-desktop/src/scheduled.rs
  crates/keystone-desktop/src/windows.rs
  crates/keystone-desktop/src/notifications.rs
  crates/keystone-desktop/src/conversation.rs
  crates/keystone-desktop/src/orchestrator.rs
  crates/keystone-desktop/src/checkpoints.rs
  crates/keystone-core/src/delivery.rs
  crates/keystone-agent/src/codex.rs
  crates/keystone-agent/src/claude.rs
  crates/keystone-agent/src/connection.rs
)
FUNCTIONS=(
  quit_if_idle publish refresh has_work runs_turn asking reserve_quit quit_failed save teardown
  pump receive save_results deliver_results result_hold send_message_from
  close_window request_quit save_and_teardown
  check_schedules arm_scheduler quitting run_task start_run reopen show
  push_sent apply awaiting_approval launch_job set_delivery
  turn_ended take_turn_end is_running hold_follow_ups release_follow_ups
  started route send_inner unqueue deliver on_response flush enqueue carry finish
  take_recheck spawn run start
)
# The model relies on how tGPUI runs cx.defer callbacks (App::finish_update, flush_effects).
GPUI_REV=59c962b4207236d1e99840399be25d1c52a469f7

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
  echo "Untracked Rust files in the covered crates:"
  untracked=$(git -C "$root" ls-files --others --exclude-standard -- \
    crates/keystone-desktop/src crates/keystone-core/src crates/keystone-agent/src | grep '\.rs$' || true)
  echo "${untracked:-  none}" | sed 's/^\([^ ]\)/  \1/'
  echo
  if grep -q "$GPUI_REV" "$root/crates/keystone-desktop/Cargo.toml"; then
    echo "tGPUI is still pinned at the rev the model was checked against."
  else
    echo "tGPUI's rev changed: check that cx.defer still runs after the outermost update,"
    echo "in order, before any other update (App::finish_update, flush_effects)."
  fi
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

rm -f "$cache"/quit-model-*.out  # outputs of a previous run's UNEXPECTED lines
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# One run: a rule, the Mutation constant, and the expected result.
#   holds  - TLC finds no violation
#   breaks - TLC finds one (a break that proves the rule can fail)
EXPECT=(
  # rule                        mutation                  expected  why
  "OnlyQuitStopsTurns           none                      holds     code"
  "NoScheduledRunWhileWaiting   none                      holds     code"
  "WindowlessMeansWaiting       none                      holds     code"
  "WindowlessEventuallyEnds     none                      holds     code"
  "OnlyQuitStopsTurns           close-ignores-turns       breaks    can fail"
  "OnlyQuitStopsTurns           quit-skips-recheck        breaks    can fail: a delivered result's turn (TOB-350)"
  "OnlyQuitStopsTurns           turns-ignore-connections  breaks    can fail: a held follow-up's turn (TOB-350)"
  "OnlyQuitStopsTurns           work-ignores-queued       breaks    can fail: a follow-up the worker has not taken (TOB-350)"
  "OnlyQuitStopsTurns           work-ignores-steer        breaks    can fail: a steer Codex rejects after the end (TOB-350)"
  "NoScheduledRunWhileWaiting   scheduler-ignores-wait    breaks    can fail"
  "WindowlessMeansWaiting       close-forgets-wait        breaks    can fail"
  "WindowlessEventuallyEnds     close-forgets-wait        breaks    can fail"
  "WindowlessEventuallyEnds     steer-settles-silently    breaks    can fail: a late steer's answer wakes nothing (TOB-350)"
)

run() {
  local rule=$1 mutation=$2 dir=$work/$1-$2
  mkdir -p "$dir"
  local kind=INVARIANT
  [[ $rule == WindowlessEventuallyEnds ]] && kind=PROPERTY
  cat >"$dir/QuitWhenIdle.cfg" <<EOF
CONSTANTS
    Parent = Parent
    Child = Child
    Task = Task
    Mutation = "$mutation"
SPECIFICATION Spec
INVARIANT TypeOK
$kind $rule
EOF
  cp "$here/QuitWhenIdle.tla" "$dir/"
  mkdir -p "$dir/tmp"  # TLC unpacks its standard modules here; a shared folder races.
  (cd "$dir" && "$java" -XX:+UseParallelGC -Djava.io.tmpdir="$dir/tmp" -cp "$jar" tlc2.TLC -workers 1 \
    -metadir "$dir/states" -config QuitWhenIdle.cfg QuitWhenIdle.tla >"$dir/tlc.out" 2>&1) || true
  if grep -q "Model checking completed. No error has been found." "$dir/tlc.out"; then
    echo holds
  elif grep -qE "^Error: (Invariant $rule is violated|Temporal properties were violated)" "$dir/tlc.out"; then
    echo breaks
  else
    echo error
  fi
}

failed=0
pids=()
for i in "${!EXPECT[@]}"; do
  read -r rule mutation _ <<<"${EXPECT[$i]}"
  (run "$rule" "$mutation" >"$work/result-$i") &
  pids+=($!)
done
wait "${pids[@]}"

printf '%-28s %-24s %-7s %s\n' RULE MUTATION RESULT EXPECTED
for i in "${!EXPECT[@]}"; do
  read -r rule mutation expected why <<<"${EXPECT[$i]}"
  got=$(cat "$work/result-$i")
  mark=ok
  if [[ $got != "$expected" ]]; then
    mark=UNEXPECTED
    failed=1
    keep=$cache/quit-model-$rule-$mutation.out
    cp "$work/$rule-$mutation/tlc.out" "$keep"
  fi
  printf '%-28s %-24s %-7s %-7s %s %s\n' "$rule" "$mutation" "$got" "$expected" "$mark" "$why"
  [[ $mark == UNEXPECTED ]] && echo "    TLC output: $keep"
done
exit $failed
