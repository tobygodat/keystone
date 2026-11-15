# Conventions

## Run, test, deploy

Cargo lives in `~/.cargo/bin`. Put it on `PATH` (`export PATH=$HOME/.cargo/bin:$PATH`) in a non-login shell.

`rust-toolchain.toml` pins the exact Rust release, with clippy, rustfmt and the targets `check.sh apple` and `check.sh linux` need. CI installs the same file. rustup installs that toolchain the first time cargo runs in the checkout, unless auto-install is off (`rustup set auto-install disable` or `RUSTUP_AUTO_INSTALL=0`). Then run `rustup toolchain install` in the checkout. Moving to a new release is a PR that changes the file, so a new stable release can't change CI on its own.

- `cargo run -p keystone-desktop --locked` runs the app in a Linux graphical session or on an Apple Silicon Mac. Set `KEYSTONE_STATE_PATH` to an isolated path so a test run doesn't touch your real workspace.
- `cargo run -p keystone-ui --example gallery` shows every component in every state.
- `KEYSTONE_VISUAL_FIXTURE=1` shows a labeled sample transcript (Markdown, a list, a table and a code block) in the normal window.

Before a PR, run the merge checks in `AGENTS.md`. The lanes of `scripts/check.sh` are faster and narrower:

- `bash scripts/check.sh` (lane `core`): formatting, the theme color check, the shortcut-hint check, the UI string inventory, and the core and agent tests.
- `bash scripts/check.sh core-fast`: the same tests under nextest with 16 workers. Opt-in, see [development iteration timing](dev-iteration.md).
- `bash scripts/check.sh ui`: GPUI simulated-input tests of the app and its components (`ui-tests` feature).
- `bash scripts/check.sh providers`: every workspace and UI test again, with failing `claude` and `codex` first on `PATH`. It fails if any test ran one, and lists each call. It also checks that an app build run under a test binary's name still finds `claude` on `PATH`. CI runs it too.
- `bash scripts/check.sh build`: compiles the app and the component gallery.
- `bash scripts/check.sh apple`: type-checks every crate for Apple Silicon macOS from Linux. On a Mac, `python3 scripts/macos-smoke.py` runs the native smoke journey. See [macOS](macos.md).
- `bash scripts/check.sh linux`: type-checks every crate for x86_64 Linux from a Mac, the mirror of `apple`. Run it after changing code under a `cfg(target_os)`. See [macOS](macos.md).

Every lane first checks that `docs/ui-strings.txt`, the inventory of interface text, matches the sources. After changing interface text, run `python3 scripts/ui-strings.py` and review the diff. `check.sh` records its wall time through `scripts/dev-timing.py`.

There is no release pipeline. Linux users build from source, and the macOS bundle is TOB-220.

**Builds.** Dev builds optimize dependencies ([ADR 0005](adr/0005-optimize-dependencies-in-dev-builds.md)), so the first build in a fresh worktree rebuilds every dependency: about 205 s for the app and 70 s for the test build on 22 cores. Install sccache and mold (`sudo pacman -S --needed sccache mold`) to share that work across worktrees ([ADR 0006](adr/0006-sccache-and-mold-through-a-rustc-wrapper.md)). Builds still work without them. Keep each worktree's own `target/`, and don't `cargo clean` between edits. Use `cargo check` for compile feedback.

**Performance.** After a change that could affect build or render speed, run `python scripts/perf.py --note "what changed"` and commit the row it appends to `docs/perf-log.tsv`. See [performance](performance.md).

**Viewing on a laptop.** `scripts/laptop-preview` runs `scripts/preview.py` on the desktop under waypipe. The README covers it.

## Layout and naming

- Crates are `keystone-<role>`. `keystone-core` and `keystone-agent` never depend on GPUI. See [ARCHITECTURE](ARCHITECTURE.md).
- A module's tests live beside it in `<module>_tests.rs`, or in a `tests` submodule for small ones. Integration tests are in each crate's `tests/`. Recorded provider transcripts are in `crates/keystone-agent/tests/fixtures/`.
- Colors only in `crates/keystone-ui/src/theme.rs`, and shortcut hints only through `keystone_ui::shortcut_text("secondary-…")`. `check.sh` enforces both. The rest of the UI rules are in `DESIGN.md`.
- Interactive components accept `debug_selector`, and native verification finds elements by name, so give new controls stable names.
- Docs: `AGENTS.md` for agent rules, `PRODUCT.md` for the product, `DESIGN.md` for the visual system, this file and `ARCHITECTURE.md` for the code, `docs/adr/` for decisions, `docs/design/` for subsystem references. Work and bugs go in GitHub issues, not in the repo.

## Errors

- Validate at the boundaries: provider output, files, SQLite, the MCP server and user input. Internal types are trusted.
- A provider failure belongs to its session. It arrives as an `Event`, shows in that session's transcript with the reason, and keeps the user's draft. It never takes down other sessions.
- A save that fails keeps the session open, shows the error and keeps the text. Navigation and app-controlled close retry the visible draft first.
- Never fabricate output or a connection state to hide an error.

## Tests

Behavior tests drive the edit loop. Keep native desktop tests small: end-to-end-only testing makes session lifecycle failures harder to isolate and couples every iteration to graphics and provider availability.

- **Core and agent.** Test observable lifecycle rules, persistence, interruption, stale events and denied actions. Don't test getters or mirror private details. Use temporary project directories and isolated application data, never real provider configuration or user repositories. Scripted providers (`keystone-fake-claude`, `keystone-fake-codex`) cross the same protocol boundary as the live ones, and their output is labeled test data.
- **Provider programs.** A test never runs the `claude` or `codex` on `PATH`, which on a developer's machine are the user's own. `keystone_agent::find::program` stops the process instead of searching for one in `keystone-agent`'s unit tests, and in any process whose test setup called `find::refuse_path_search()`. `keystone_core::test_support::built_binary` and the desktop's test `AppState::new` call it; the app never does. A desktop test build reads no `KEYSTONE_*_PATH`: before a window opens, set `AppState::test_programs` to `test_support::scripted()`, or to `test_support::no_providers()` for a test that never talks to a provider. A connection started without it fails the test. `check.sh providers` catches what these miss.
- **GPUI.** `#[gpui::test]` gives simulated input and deterministic async. Use it for the composer, session selection, permission responses and restoration. Every desktop test calls `allow_parking`. These tests don't prove native rendering, window-manager behaviour, clipboard integration or GPU compatibility.
- **Native.** Prove a change in the real window with the [verify skill](../crates/keystone-desktop/.agents/skills/verify/SKILL.md), which runs the `keystone-verify` MCP server on a private headless Sway desktop. A named click doesn't prove the visual result, so inspect the screenshot. Check [the feature map](../crates/keystone-desktop/.agents/skills/verify/features/README.md) so a shortcut check doesn't stand in for an untested mouse entry point. `python3 scripts/screen-walk.py walk` screenshots every surface at 1920×1080 and 720 px wide (see [screen walk](verification/screen-walk.md)). On Hyprland, `python scripts/native-smoke.py` exercises keyboard creation, settlement, drafts and restart.
- **Stress.** After changes to the composer, text layout or the dependency profile, rerun `python3 scripts/native-stress.py --runs 10`. It types a long draft at 10 ms per key and fails if the save lags more than 1 s.
- **Providers.** Live Claude Code and Codex checks are opt-in, after adapter changes and before a release: streaming, cancellation, steering, approval round trips and resume, in a disposable repository. They use subscription limits, so they're never the default loop.

Native verification notes:

- An agent shell may not inherit the Hyprland session. Set `HYPRLAND_INSTANCE_SIGNATURE` from `/run/user/1000/hypr/` and `WAYLAND_DISPLAY=wayland-1` before running non-headless checks.
- Find a nested Sway window with `pgrep -x sway`, then match that PID in `hyprctl clients -j`. `pgrep -f` matches your own shell.
- Driver mouse actions need `"button":"left"`. A driver built without `computer-use-mcp-packed-rgb.patch` can't read the headless output's 24-bit buffers (TOB-214); `screenshot` then captures with `grim` and reports `capture: "grim"`. Rebuild the driver as the verify skill's setup describes.
- `native-smoke.py` env: `KEYSTONE_CAPTURE_DIR` picks the evidence directory, `KEYSTONE_TEST_CLOSE=1` checks that failed storage blocks a compositor close, `KEYSTONE_KEY_DELAY_MS` sets typing speed (default 100 ms), and `KEYSTONE_LONG_REPEAT` lengthens the long draft.
