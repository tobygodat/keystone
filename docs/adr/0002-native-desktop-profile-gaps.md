# 0002. Where Keystone differs from the native desktop profile

Status: Accepted, 2026-10-27

## Context

Toby's project standard has a "Native desktop" profile: a Rust workspace split into `core`, `ui` and `app` crates, SQLite for local state, `cargo test` and `cargo clippy` before a PR, and proof by running the app with a screenshot or recording. Keystone predates the standard and differs from it in a few places.

## Decision

Keep Keystone's current layout and tooling. The gaps are deliberate:

- **Four crates, not three.** `keystone-core` (sessions, store, SQLite), `keystone-ui` (components) and `keystone-desktop` (the app) match `core`, `ui` and `app`. `keystone-agent` holds the provider drivers, logs and the orchestrator MCP server. It stays separate so provider tests don't compile GPUI.
- **`default-members` is `keystone-core` only.** Plain `cargo test` runs the fast core suite. The merge checks pass `--workspace` explicitly.
- **More checks than the profile names.** `scripts/check.sh` adds formatting, the theme color check and the UI string inventory. The GPUI simulated-input tests run behind the `ui-tests` feature through `check.sh ui`.
- **Python for verification tooling.** The native driver, smoke, stress, screen walk and probe scripts in `scripts/` are Python 3. They are dev tools, not shipped code.
- **No oxlint.** The standard's linter rule covers JavaScript and TypeScript. Keystone ships none, so clippy is the linter.
- **No `.env.example`.** Configuration is `KEYSTONE_*` environment variables, documented in the README and in `docs/CONVENTIONS.md`. Nothing needs a secret.
- **Clippy warnings are allowed.** `cargo clippy --workspace --locked` must exit 0. It passes with warnings today (34 on 2026-10-27), and `-D warnings` is not enforced.

## Consequences

- Renaming crates to `core`, `ui` and `app` would churn every path for no behaviour change, so we don't.
- An agent following the profile literally would run plain `cargo test` and miss the desktop, UI and agent suites. `AGENTS.md` names the `--workspace` commands.
- Turning on `-D warnings` later needs the existing warnings fixed first.
