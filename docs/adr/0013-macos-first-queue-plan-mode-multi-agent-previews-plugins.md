# 0013. Develop for macOS first, and bring in queued messages, plan mode, multi-agent work, HTML previews, artifacts and plugins

Status: Accepted, 2026-11-14 (TOB-471). Supersedes parts of [ADR 0001](0001-product-scope.md) and [ADR 0010](0010-terminal-browser-scheduling-editor-diffs-remote-devices.md), listed below.

## Context

On 2026-11-14 Toby compared Keystone with his T3 Code fork and with Zeron, an open-source Rust control plane for coding agents with a plugin system. He decided to move Keystone closer to T3 Code's workflow while it keeps its own interface:

- Toby develops on a Mac now. Keystone's proof tools still assume Linux: the verify skill drives a headless Sway desktop, and `native-smoke.py` needs Hyprland.
- ADR 0001 made messages during a turn steer only, with no follow-up queue and no plan-mode UI. ADR 0010 kept both out. Toby wants both.
- The orchestrator can launch one session and deliver its result (`docs/design/orchestrator.md`). It has no child sessions in the sidebar, no fan-out and no workflows. T3 Code has all three.
- An agent's HTML has nowhere to show in Keystone, and a Claude artifact link opens in an outside browser.
- Changing Keystone's look or adding a widget means changing Keystone's code. Zeron's plugins do this from sandboxed Luau scripts with permissions the user grants. ADR 0001's "plugin management" means Claude and Codex plugins, not plugins for Keystone.

## Decision

**macOS first** ([TOB-472](https://linear.app/tobygodat/issue/TOB-472)). Apple Silicon macOS is Keystone's target platform. Linux is not a target for now. Agents prove changes on a Mac, and a change may leave Linux unproven. Linux code and the Sway driver stay in the repo until a later decision removes them or makes Linux a target again.

**Queued messages and plan mode** ([TOB-473](https://linear.app/tobygodat/issue/TOB-473)). A message sent during a turn can wait for the turn to end and then send. The user can still steer the running turn instead. A session can run in a plan mode, where the agent plans before it edits and the user approves the plan.

- Open: whether a message waits or steers by default, and how the composer shows the choice.
- Open: how plan mode maps onto Claude Code's and Codex's own plan modes.

**Multi-agent work** ([TOB-474](https://linear.app/tobygodat/issue/TOB-474)). A launched session is the child of the session that launched it. A child can run in its own worktree on any provider, account and model. An agent or the user can fan one task out to several children and compare their results. A workflow, a saved script of agent steps, can run from the palette or from an agent.

- Open: the workflow format, and where workflows are saved.

**HTML previews and artifacts** ([TOB-475](https://linear.app/tobygodat/issue/TOB-475)). An agent can show an HTML file or inline HTML beside its session through an orchestrator MCP tool. A Claude artifact link opens beside the session.

- Open: whether previews reuse the browser preview's headless Chrome, and what a preview page may reach.

**Plugins for Keystone** ([TOB-476](https://linear.app/tobygodat/issue/TOB-476)). Keystone loads plugins of its own, in two stages. Stage 1 is appearance packs: a palette and images, with no code. Stage 2 is Luau plugins that add nodes in named slots and draw only with `keystone-ui` components and semantic colors. Each plugin runs in a sandbox, reaches only what its manifest asks for and the user allowed, and is allowed in a window that plugins can't draw into.

- Open: the slot names, the manifest format and the permission list.
- A plugin can never approve a request or answer an agent's question for the user. The "Approval is the user's" principle stands.

**Still removed.** These stay out, and bringing any of them back needs a new ADR:

- Side conversations through `/btw`.
- Monos and app-owned persistent cross-project agents. Child sessions and workflows come in without them.
- Session snoozing and project command buttons.
- Session export.

**Not decided here.** Plugin services on remote hosts, plugin signing and sharing, and replacing whole screens with a plugin. Each needs a new ADR before it is built.

**What this supersedes.** In ADR 0001, "Messages during active work steer only. There is no follow-up queue." and "No separate plan-mode UI." under Decision, and "Follow-up message queues and dedicated plan-mode controls" under Removed. In ADR 0010, "Follow-up message queues. Messages during active work still steer only." and "The dedicated plan-mode UI." under Still removed. Everything else in both ADRs stands, including Keystone's own interface, the frozen Needs action grouping, and the rule that settlement never approves a request. Neither ADR's text changes, because an accepted ADR is never edited.

## Consequences

- TOB-472 to TOB-476 can start. Each settles its open questions in its issue or in a design doc under `docs/design/` before code that depends on the answer.
- Until TOB-472 lands, the verify skill still needs Linux. On a Mac, prove a change with `cargo run -p keystone-desktop --locked` and `python3 scripts/macos-smoke.py`.
- `session_send` in the orchestrator stops being steer-only once TOB-473 lands. The [orchestrator design](../design/orchestrator.md) and the quit model change with it.
- Plugins add a scripting runtime to the app. Zeron uses `mlua` with the `luau` feature under the MIT licence, which ADR 0003 allows. Any borrowed file is credited in `THIRD_PARTY_NOTICES.md`.
- Readers of ADR 0001 need ADR 0010 and this ADR beside it for the current scope. `PRODUCT.md` points to all three.
