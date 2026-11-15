# Development iteration timing

`scripts/dev-timing.py` records how long development commands take. Each run writes one JSON file. Records go to `dev-timings/` in the git common directory, so every worktree shares them and they survive when a worktree is removed. Set `KEYSTONE_DEV_TIMING_DIR` to write them somewhere else.

## Automatic history

`bash scripts/check.sh [core|ui|build]` records itself with the label `check <mode>`. Its checks, output and exit status are unchanged. Use `bash scripts/check.sh --unmeasured <mode>` to bypass recording.

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

## Tests run under nextest

`check.sh core` and `check.sh ui` run their tests with `cargo nextest run --locked --retries 0`. `core` then runs the doctests with `cargo test --doc`, because nextest does not run doctests. Until 2026-11-15 nextest was the opt-in `core-fast` lane, and `core` and `ui` used `cargo test`.

Install nextest from the official pre-built binaries at <https://nexte.st/docs/installation/pre-built-binaries/>. Version 0.9.148 is the tested one, and CI installs that release. `check.sh` never installs nextest. When `cargo-nextest` is missing it exits with an error that names that page.

Nextest runs each test in its own process and runs test binaries side by side, where `cargo test` finishes one binary before it starts the next. The worker count is nextest's default, one per logical CPU. `RUST_TEST_THREADS` no longer limits a run. To limit one, set `NEXTEST_TEST_THREADS`, for example `NEXTEST_TEST_THREADS=8 bash scripts/check.sh ui` when other builds share the machine. Retries are off so a flaky test fails the run instead of hiding.

## Measured improvements

On 2026-10-16, the complete warm core check fell from **12.28 s to 5.77 s (53% less waiting)** using `core-fast` and the synchronized old-Claude fixture. Five alternating runs per side passed all 108 tests, with formatting, color checks and Cargo doctest invocation retained. The final ranges were 12.27–12.31 s before and 5.76–5.77 s after. These are warm test-loop measurements, not compilation speedups.

Three candidates were evaluated. Four nextest workers reduced the original check to 11.04 s; eight reduced it to 6.73 s and superseded the four-worker setting. Separately, replacing the legacy Claude fixture's per-word sleeps with a first-word release gate reduced ordinary `core` to 8.03 s. The combined result is 5.77 s; the individual savings are not additive. Default fake-provider streaming and production connection behavior stay the same. The gate is opt-in and only available without lifecycle frames.

The [raw timing samples and provenance](dev-iteration-hillclimb.json) include revision IDs, load ranges, failures and the excluded-warmup method. Reproduce with `CARGO_BUILD_JOBS=4`, a warm target per checkout, one excluded warmup, then five alternating runs of `bash scripts/check.sh core` at the original source and `bash scripts/check.sh core-fast` at the final source. Keep both worktrees' build directories separate. Record runner versions and verify the executed test counts before comparing medians.

## Second measurement pass

The next pass reduced the complete warm check from **5.77 s to 3.26 s**, another **43.5% reduction**. Against the original 12.28 s check, the combined reduction is **73.4%**. These figures describe test-loop waiting on this machine, not compilation speed.

Shortening three fake-Claude replies reduced the eight-worker check to 4.20 s. The running-mode test now waits at an approval request and deliberately holds the next message behind a mode confirmation. The startup test reuses its existing gate. The restart test retains its immediate-send ordering because adding a start gate skipped the queued-restart branch. All original assertions remain, with additional reply and mode checks.

A subsequent worker sweep measured 4.20 s with eight, 3.54 s with twelve, and 3.26 s with sixteen. Sixteen is the retained setting. Each variant ran five measured samples after an excluded warmup, with all 108 tests passing, no retries, and the doctest invocation retained. The sixteen-worker range was 3.25–3.27 s. The genuine two-second shutdown deadlines remain intact.

[Second-pass samples and provenance](dev-iteration-hillclimb-2.json) preserve both controlled experiments. The reported endpoints span the sequential fixture comparison and worker sweep. To reproduce the sweep, use the corrected fixture revision in that file, copy `check.sh` with only the worker count changed, then cycle through 8, 12 and 16 workers five times after warming each variant. Keep the full formatting, lint and doctest stages. Future measurements should target an actual content edit and rebuild, since the warm test check is now approaching its shutdown-test floor.

## Third pass: one slow test, then nextest for every lane

By 2026-11-14 the fastest recorded `check core` took 168 s. One test caused nearly all of it. `every_four_event_route_assignment_acknowledges_only_the_drained_prefix` took **141 s**, because its scripted host closed stdout once it had sent its frames. The link read that as the host dropping and reconnected after a 1 s backoff, and the test polled for the brief connected moments. Keeping the host's stdout open brought the test to **1.3 s**. Find a slow test with `cargo nextest run ... --status-level all`, which prints each test's time.

With that fixed, the core tests took a median **47.5 s under `cargo test` and 16.1 s under nextest** (ranges 47.1–47.9 s and 16.1–16.3 s, five alternating runs each, all 1,030 tests passing). Sixteen workers took 21.1 s, so the lanes now use nextest's default of one worker per logical CPU. Under nextest the lane is bound by its summed test time, about 290 s of mostly waiting, spread over 24 workers.

The ui tests took 42.9 s under `cargo test` and 42.1 s under nextest, so nextest does not make that lane faster yet. Nearly all 1,221 ui tests are in one binary, which both runners run in parallel, and one test takes 41.5 s. Nextest still helps there in two ways:

- `RUST_TEST_THREADS=2`, which agents set on a busy machine, no longer applies. In the recorded runs since 2026-11-10 the lane's median was 4.7 minutes with that setting and 1.2 without. Those limited runs were also on a busier machine.
- Each test is its own process. With three ui runs going at once, `cargo test` took 45–58 s and nextest 42.7–43.2 s. One `cargo test` run lost its whole binary to a SIGKILL, with 1,018 tests in it. The cause was not found.

The complete lanes, measured on a busy machine (load 9–29), took a median 24.9 s for `check core` and 48.7 s for `check ui`.

Later the same day the slowest desktop tests stopped waiting on real time: a test helper had waited 10 s for a host process that was already dead, and the usage-limit tests had waited on the wall clock. The ui tests then took 12.5 s under nextest (two runs, 12.50 and 12.52 s), and `check ui` took 19.3 s once built (one run).

These runs were not on an idle machine, which the rule above asks for. Other agents were building and testing, and each sample records the load at its start. The core result is far larger than the spread. The ui result is no measurable difference. Four test failures and one killed binary came up across these runs, each under load. They are listed with the [samples](dev-iteration-nextest.json).

`cargo test --workspace` is no longer part of the merge checks. When it was dropped, its 1,356 tests were all among the 2,255 that `check.sh core` and `check.sh ui` run, by comparing `cargo nextest list` for each. The workspace has no doctests. `core` still runs the doctest command for its two crates. `ui` does not, because that command rebuilds GPUI for a different feature set, about 44 s, to run nothing. Add it if `keystone-ui` gains a doctest.

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

Run formatting and the tests for the changed crate while editing. For a focused test, run `cargo nextest run -p keystone-core --locked -E 'test(<test-name>)'`, and record it with this wrapper when the time matters. Run the full required checks on the integrated change. Use native checks for affected desktop behavior and live providers at the integration gate described in [the conventions](CONVENTIONS.md#tests).

Keep separate Cargo target directories for worktrees that build concurrently. Reuse the existing compiler cache and the implementation worktree's incremental artifacts. For a native check, reuse a binary only when it represents the exact source being verified. Do not clean caches as part of ordinary iteration.

Tests: `python3 scripts/test_dev_timing.py`.
