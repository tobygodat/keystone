# Development iteration timing

`scripts/dev-timing.py` records how long development commands take. Each run writes one JSON file. Records go to `dev-timings/` in the git common directory, so every worktree shares them and they survive when a worktree is removed. Set `KEYSTONE_DEV_TIMING_DIR` to write them somewhere else.

## Automatic history

`bash scripts/check.sh [core|core-fast|ui|build]` records itself with the label `check <mode>`. Its checks, output and exit status are unchanged. Without `python3` it runs the checks unrecorded. Use `bash scripts/check.sh --unmeasured <mode>` to bypass recording, including when the local Python installation is broken.

## Explicit commands

Wrap native and build commands that `check.sh` does not run:

```bash
python3 scripts/dev-timing.py run --label verify-build --cache-state warm -- \
  cargo build -p keystone-desktop --locked --offline -j 2
python3 scripts/dev-timing.py summary
```

The command after `--` runs as given, with no shell. Output streams as usual. The exit status and any terminating signal pass through. SIGINT, SIGTERM and SIGHUP go to the command's whole process group. Use this wrapper for noninteractive checks. Commands that open `/dev/tty` directly and terminal job control such as Ctrl-Z are not supported, because the command runs in its own session.

A record holds the label, exact argv, working directory, worktree, full HEAD, dirty state, UTC start, host, CPU, `rustc -V`, an allowlist of build variables (`CARGO_TARGET_DIR`, `CARGO_BUILD_JOBS`, `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `RUSTC_WRAPPER`, `CARGO_INCREMENTAL`, `RUST_TEST_THREADS` and their aliases), the cache label, the one-minute load before and after, wall seconds, and the exit code, signal or launch error. It records no other environment, no CPU time and no sccache counters. The sccache daemon is shared between concurrent builds, so its counter deltas cannot be attributed to one run.

## Reading the summary

`summary` groups runs by label, command, revision and dirty state, host, toolchain, build variables and cache label. Compare groups only when their differences are the changes you intend to measure. Matching groups do not establish identical inputs. Different local edits can share the same dirty HEAD, and unknown metadata does not establish equivalence. Each group shows its run, success and failure counts. The median, minimum and maximum cover successful runs only. Failed runs are listed separately, so a command that fails fast is never a speedup. Invalid or unknown-version records print a warning and are skipped.

## Cache label

The cache label is your claim, not a measurement. It defaults to `unknown`. Pass `--cache-state warm|cold|touched`, or set `KEYSTONE_CACHE_STATE` for `check.sh`. Nothing inspects `target/` or sccache to infer it.

## Claiming a speedup

History shows trends. It does not prove a speedup, because load, cache contents and concurrent builds differ between runs. To compare two variants, run each five times, interleaved (A B A B ...), with the same workload and cache preparation on an idle machine. Change only the source or settings being evaluated. Compare the successful medians and check the load ranges. Wall time covers the child command. Metadata collection and record publication add overhead outside that timed region.

This tool does not schedule or serialize builds. Concurrent builds still slow each other down, and only the load fields show it.

The [initial baseline](dev-iteration-baseline.json) records the original checks before instrumentation. It separates the first invocation in a new worktree from five unchanged warm runs. Warm build samples are freshness checks, not edited-source rebuilds. Keep this distinction when comparing later measurements.

## Faster core tests with nextest

`bash scripts/check.sh core-fast` runs the same formatting and color checks as `core`, then `cargo nextest run -p keystone-core -p keystone-agent --locked --test-threads 16 --retries 0`, then `cargo test -p keystone-core -p keystone-agent --locked --doc`. It is opt-in. `core` keeps the standard Cargo runner and nothing switches between them automatically.

Install nextest from the official pre-built binaries at <https://nexte.st/docs/installation/pre-built-binaries/>. Version 0.9.148 is the tested one. `check.sh` never installs nextest automatically. When `cargo-nextest` is missing it exits with an error that names that page and the `core` fallback.

Sixteen test workers suit this suite because its tests are I/O-bound rather than CPU-bound. Keep four build jobs (`CARGO_BUILD_JOBS=4`) when another build shares the machine. Retries are off so a flaky test fails the run instead of hiding. Nextest does not run doctests, which is why the second command runs them with Cargo's runner. The two commands together cover the same tests as `core`.

## Measured improvements

On 2026-10-16, the complete warm core check fell from **12.28 s to 5.77 s (53% less waiting)** using `core-fast` and the synchronized old-Claude fixture. Five alternating runs per side passed all 108 tests, with formatting, color checks and Cargo doctest invocation retained. The final ranges were 12.27–12.31 s before and 5.76–5.77 s after. These are warm test-loop measurements, not compilation speedups.

Three candidates were evaluated. Four nextest workers reduced the original check to 11.04 s; eight reduced it to 6.73 s and superseded the four-worker setting. Separately, replacing the legacy Claude fixture's per-word sleeps with a first-word release gate reduced ordinary `core` to 8.03 s. The combined result is 5.77 s; the individual savings are not additive. Default fake-provider streaming and production connection behavior stay the same. The gate is opt-in and only available without lifecycle frames.

The [raw timing samples and provenance](dev-iteration-hillclimb.json) include revision IDs, load ranges, failures and the excluded-warmup method. Reproduce with `CARGO_BUILD_JOBS=4`, a warm target per checkout, one excluded warmup, then five alternating runs of `bash scripts/check.sh core` at the original source and `bash scripts/check.sh core-fast` at the final source. Keep both worktrees' build directories separate. Record runner versions and verify the executed test counts before comparing medians.

## Second measurement pass

The next pass reduced the complete warm check from **5.77 s to 3.26 s**, another **43.5% reduction**. Against the original 12.28 s check, the combined reduction is **73.4%**. These figures describe test-loop waiting on this machine, not compilation speed.

Shortening three fake-Claude replies reduced the eight-worker check to 4.20 s. The running-mode test now waits at an approval request and deliberately holds the next message behind a mode confirmation. The startup test reuses its existing gate. The restart test retains its immediate-send ordering because adding a start gate skipped the queued-restart branch. All original assertions remain, with additional reply and mode checks.

A subsequent worker sweep measured 4.20 s with eight, 3.54 s with twelve, and 3.26 s with sixteen. Sixteen is the retained setting. Each variant ran five measured samples after an excluded warmup, with all 108 tests passing, no retries, and the doctest invocation retained. The sixteen-worker range was 3.25–3.27 s. The genuine two-second shutdown deadlines remain intact.

[Second-pass samples and provenance](dev-iteration-hillclimb-2.json) preserve both controlled experiments. The reported endpoints span the sequential fixture comparison and worker sweep. To reproduce the sweep, use the corrected fixture revision in that file, copy `check.sh` with only the worker count changed, then cycle through 8, 12 and 16 workers five times after warming each variant. Keep the full formatting, lint and doctest stages. Future measurements should target an actual content edit and rebuild, since the warm test check is now approaching its shutdown-test floor.

## Native verification workflow

Use the existing private headless desktop and named controls in the [native verification skill](../crates/keystone-desktop/.agents/skills/verify/SKILL.md). Build the changed app once, reuse the existing driver, then keep the desktop alive for the related journey. Wait for named elements, observe each state transition, and check the saved state. Reusing a driver does not permit reusing a stale app binary.

For the existing typing and restart regression journey:

```bash
cargo build -p keystone-desktop --locked --offline -j 4
python3 scripts/dev-timing.py run --label "native typing and restart" --cache-state warm -- \
  python3 scripts/native-stress.py --runs 1
```

A smoke run preserved the full 1,093-character workload, passed both graceful quits and restart, and retained screenshots and state evidence with no cleanup errors. This is a reusable correctness workflow, not a measured native speedup. It covers typing and restart, not all UI interactions. Keep the full stress workload for its regression gate; a shorter feature-specific journey must name its narrower coverage.

See [workflow research and follow-up experiments](research/dev-workflow.md) for the Grok CLI search findings and remaining hypotheses.

## Keep the edit loop small

Run formatting and the tests for the changed crate while editing. For a focused core test, record `cargo test -p keystone-core <test-name> --locked` with this wrapper. Run the full required checks on the integrated change. Use native checks for affected desktop behavior and live providers at the integration gate described in [the conventions](CONVENTIONS.md#tests).

Keep separate Cargo target directories for worktrees that build concurrently. Reuse the existing compiler cache and the implementation worktree's incremental artifacts. For a native check, reuse a binary only when it represents the exact source being verified. Do not clean caches as part of ordinary iteration.

Tests: `python3 scripts/test_dev_timing.py`.
