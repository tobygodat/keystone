# 0010. Bring the terminal, browser preview, scheduled tasks, editor, diff viewer, remote environments and device panel into scope

Status: Accepted, 2026-11-02 (TOB-295). Supersedes parts of ADR 0001, listed below.

## Context

[ADR 0001](0001-product-scope.md) deferred the integrated terminal and remote control through Tailscale. It removed scheduled tasks, the built-in file editor, the local Git diff viewer and the browser preview panel. It never mentioned a device panel. Its rule is that nothing outside its lists gets built without a new ADR.

Toby has decided to bring these seven features into Keystone, porting them from T3 Code (`~/t3code`, read-only). Each one has a build issue blocked by this ADR: TOB-301 to TOB-307. The rest of ADR 0001 stands.

## Decision

**In scope.** These seven features are now part of the product. This ADR records scope, not designs. Each build issue settles its own open questions, in the issue or in a design doc under `docs/design/`, before code that depends on the answer.

- **Integrated terminal** (TOB-301). A terminal per session, opened in the session's folder or worktree, with splits. Its output and scrollback survive a window close, and Quit ends the shell.
  - Open: which terminal emulator crate, under ADR 0003. Where the shell process lives, so that closing a window doesn't end it.
- **Browser preview panel** (TOB-302). A browser beside a session that the user can browse and agents can drive through orchestrator MCP tools: navigate, snapshot, act and screenshot. It finds the project's local dev servers.
  - Open: the embedding. A system webview, CEF, or an external Chrome driven over CDP. Each has a different cost in binary size, platform support and how the page sits inside a GPUI window.
- **Scheduled tasks** (TOB-303). Run a saved prompt in a project on a cron or interval schedule, as a new session or into an existing one. Each task shows its run history and can be run now, paused or deleted.
  - Open: whether Keystone has to be running for a task to fire, or a background process fires it. ADR 0001 says explicit Quit ends the application, so a background process would change what Quit means.
  - Open: what a run with nobody watching does when it hits an approval. ADR 0001's rules still apply, so it waits and never approves on the user's behalf.
- **Built-in file editor** (TOB-304). Open a file from Files, a mention or a transcript link, then edit and save it in Keystone with syntax highlighting. It catches a change made by an agent or another program before saving over it. External-editor links stay.
  - Open: which text buffer and highlighting crates, under ADR 0003. How an outside change is detected and shown.
- **Diff viewer** (TOB-305). Show what each turn changed, inline diffs on edit tool calls in the transcript, and the working tree beside Changes. Users can comment on a line and send the comments to the session.
  - Open: where a turn's diff comes from, such as checkpoints, provider edit events or Git. How a large diff renders without stalling the UI.
- **Remote environments** (TOB-306). Run sessions on another machine, linked over SSH or Tailscale, with that machine's own `claude`, `codex` and repositories, driven from the local window. A session survives a network drop.
  - Open: the process that runs on the remote machine. The protocol between it and the local app, which has no RPC today. How each side authenticates the other, given that Keystone holds no provider credentials. Which side owns storage for a remote session: its log, drafts and workspace state.
- **Device panel** (TOB-307). A live iOS Simulator or Android Emulator beside a session, with `device_*` orchestrator MCP tools so agents can open apps, tap and take screenshots.
  - Open: how frames reach the panel and input reaches the device. Whether a device can run on a remote machine, such as a Mac's simulator driven from Linux. The build follows the remote environments design for that reason.

**ADR 0003 constrains the terminal and the editor.** Zed's terminal and editor crates are GPL, so Keystone takes their patterns only. Any emulator, buffer or highlighting crate must have a permissive licence, and borrowed files are credited in `THIRD_PARTY_NOTICES.md`.

**Still removed.** These stay out, and bringing any of them back needs a new ADR:

- Side conversations through `/btw`.
- Follow-up message queues. Messages during active work still steer only.
- The dedicated plan-mode UI.
- Monos and app-owned persistent cross-project agents. Scheduled tasks come back without them.
- Session snoozing and project command buttons.
- Session export.

**Not decided here.** The build issues name follow-ups this ADR doesn't bring into scope: webhook triggers for scheduled tasks, a hosted relay or cloud and a mobile client for remote environments, and recording, device emulation and cookie import in the browser preview. Each needs a new ADR before it is built.

**What this supersedes in ADR 0001.** "Integrated terminal" and "Remote control through Tailscale" under Deferred. "Scheduled tasks", "Built-in file editor and local Git diff viewer" and "Browser preview panel" under Removed. Everything else in ADR 0001 stands, including its direction, the external-terminal fallback, the frozen Needs action grouping and settlement, and the rule that a feature outside its lists needs a new ADR. ADR 0001's text is unchanged, because an accepted ADR is never edited.

## Consequences

- TOB-301 to TOB-307 can start. TOB-302, TOB-303 and TOB-306 settle their open questions before building, and TOB-307 follows TOB-306's design.
- Remote environments end the rule that Keystone is one local process with no RPC between its parts. `docs/ARCHITECTURE.md` changes when that design lands.
- The browser preview and device panel add tools to the orchestrator MCP server. The [orchestrator design](../design/orchestrator.md) grows with them.
- Readers of ADR 0001 need this ADR beside it for the current scope. `PRODUCT.md` points to both.
