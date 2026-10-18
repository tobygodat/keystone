# Arrange a project layout

Customize edits the session list, conversation occurrences, and the single composer. All occurrences follow the active session. A project inherits the legacy sidebar until its first changed Apply. Opening Customize and fitting a smaller window do not create a saved project entry.

## Controls and shortcuts

- Click **Customize** or press Ctrl+Shift+L. Close or Escape discards the preview and closes Customize. Escape first closes a Borrow chooser, another open menu, or a higher dialog.
- Drag a component's **⠿** grip onto another component's left, right, top, or bottom edge. The highlighted target stages a move. Dropping beside the same occurrence changes nothing. **Tabs** on a conversation accepts another conversation.
- Click a grip or the component's named button to select it. The component buttons scroll horizontally when needed. **←**, **→**, **↑**, and **↓** move it beside its previous or next occurrence in tree order. Alt+Left/Right/Up/Down performs the same move while Customize is open. Ordinary arrows still edit text.
- **Duplicate conversation** adds a separate occurrence of the active conversation. **Remove** closes the selected occurrence. The last conversation and the single composer cannot be removed. **Add session list** restores a removed session list.
- Drag a split divider to resize its adjacent children. **Auto** retains intrinsic content sizing. **Pixels** sets a preferred 320 logical pixels. **Fill** sets one share of remaining space. Alt+[ and Alt+] decrease or increase the selected occurrence's nearest split size by 10 pixels or one Fill share. Auto stays intrinsic until another size mode is chosen. Dragging keeps Auto, Pixels, and Fill types. A lone Fill retains its weight beside a pixel-sized child.
- Choose **Claude Code**, describe an arrangement, then click **Preview** or press Ctrl+Enter in that prompt. The helper receives presentation only. A valid complete composition stages a preview. **Cancel** cancels a pending request. **Retry** retries a failed or unsupported request. Later direct edits, Borrow, Reset, Apply, Discard, or Close invalidate older answers. Request IDs survive close and reopen.
- **Apply** or Ctrl+S saves the destination project's layout. **Discard** restores its saved presentation. Both are disabled without changes, except Discard can cancel a pending request. **Reset** previews the default. **Undo** restores the exact prior saved document or absence. Undo is unavailable while a preview or helper request is pending.
- Send, Stop, steering, and approval actions remain explicit and usable during customization. The composer entity, draft, selection, and provider run survive presentation edits.

In a short window, the optional prompt and component controls scroll vertically. Commit controls and status stay outside that scroll area.

## Borrow

Open **Customize > Borrow layout**. This lists locally saved project layouts without checking whether their directories still exist. Select a source row and click **Preview layout**. Every source occurrence receives a fresh destination ID. Only presentation is copied.

Cancel, ×, or Escape closes only the chooser and keeps an existing preview. If a preview is already staged, **Replace this preview?** offers **Keep current preview** and **Replace preview**. The source is read again when Preview or Replace is activated.

**No other saved layouts** offers Close. **This layout cannot be used** offers Back to layouts. Neither changes the current preview. Apply failure keeps the candidate and history. Apply retries after storage is repaired. A directory-sync warning has already applied the layout. Its exact copy is `Layout applied. Could not confirm it will survive a system crash.` Undo is enabled and Apply/Discard are disabled. There is no Retry Apply action in that state.

## Fit, focus, and unread

Below the checked tree's component minimum widths or heights, conversations temporarily become tabs, the composer stays below them, and the sidebar button opens the session list over the content. Widening restores the preferred tree. Fitted tab selection and viewport resizing do not write the saved document. Preferred tab selections made outside Customize are also temporary.

The composer minimum measures the standard control row, including Stop while idle, with a 420 logical pixel floor. A saved pixel width remains a preference. Extend that measurement when integrating the model selector, then verify the actual streaming controls again.

Each conversation occurrence retains its own list, scroll, and native focus per active session. Tab and Shift+Tab navigate native focus. Clicking a transcript focuses that occurrence. Only a visible, focused conversation at its latest reply in an active native window can acknowledge that reply. Reveal pacing must finish first. Composer, sidebar, Customize prompt, menus, dialogs, hidden tabs, and visible unfocused panes do not establish observation. Selecting, leaving, sending, or completing a turn does not implicitly acknowledge earlier content.

Send scrolls the most recently focused visible conversation, with the first visible occurrence as fallback. Other occurrences keep their offsets. Send then returns focus to the composer.

## Native automation selectors

Entry and commit controls are `customize-toggle`, `customize-panel`, `customize-close`, `customize-borrow`, `customize-reset`, `customize-undo`, `customize-discard`, `customize-apply`, and `customize-status`.

Helper controls are `customize-provider-claude`, `customize-provider-codex`, `customize-prompt`, `customize-ask`, `customize-cancel`, and `customize-retry`. Only the applicable pending/error controls appear.

Components use `component-grip-{ViewId}`, `select-component-{ViewId}`, `component-duplicate`, `component-remove`, `component-add-sidebar`, `component-left/right/up/down`, and `component-size-auto/px/fill`. Drop targets are `drop-{ViewId}-left/right/above/below/tabs`. Split dividers are `split-grip-{NodePath:?}-{boundary_index}`, such as `split-grip-[]-0` or `split-grip-[1]-0`.

The default conversation ID 0 retains `conversation`, `transcript-row-{index}`, `transcript-status`, `turn-status`, `approval`, `approve`, `deny`, and `jump-to-latest`. Other occurrences append `-view-{ViewId}`, such as `conversation-view-3` and `approve-view-3`. A borrowed layout may have no ID 0. Read the actual selectors after every replacement. GPUI element identity is scoped by stable occurrence IDs.

Preferred tabs use `conversation-tab-{ViewId}`. Temporary fitted tabs use `fitted-tab-{ViewId}`. The composer remains singular, including `composer-field`, `send`, `stop`, and `approval-mode`.

Borrow uses `borrow-dialog`, `borrow-source-{row_index}`, `borrow-preview`, `borrow-cancel`, `borrow-close`, `borrow-keep`, `borrow-replace`, `borrow-back`, and `borrow-empty-close`. Read the displayed source path before selecting an indexed row.

## Scripted helper response

Build the fake binaries before running helper tests. Configure a wrapper through `KEYSTONE_CLAUDE_PATH`. Set `KEYSTONE_FAKE_CLAUDE_REPLY` to a complete strict document. `KEYSTONE_CODEX_PATH` is for ordinary Codex conversations only.

```json
{"kind":"change","version":1,"composition":{"version":1,"views":[{"id":0,"component":"conversation"},{"id":2,"component":"composer"}],"root":{"kind":"split","axis":"column","children":[{"node":{"kind":"view","id":0},"size":{"fill":1}},{"node":{"kind":"view","id":2},"size":"auto"}]}}}
```

An unsupported answer is `{"kind":"unsupported","version":1,"reason":"Short explanation"}`. Sidebar-only answers, extra fields, foreign session bindings, prose, fences, invalid component counts, and over-limit documents are rejected. Scripted output proves app handling, not production provider acceptance. The helper uses the restricted Claude Code constructor. It requires confirmed initialization, empty reported tools and MCP servers, plan mode, and an actually written request before accepting success. It launches with `--tools ""`, strict empty MCP configuration, disabled slash commands and hooks, safe mode, and no session persistence. It refuses reported capabilities or hooks that violate those restrictions. These checks do not establish a universal no-tools guarantee for every installed provider version.

## Restricted helper checks

Choose **Codex** and press Preview. Require an Unsupported explanation directing you to Claude Code or manual arrangement. Codex helper requests return before executable lookup, temporary-directory creation, or process launch. Ordinary Codex conversations remain supported.

Log the Claude wrapper's argv and initialize exchange. Require the complete current Composition in the written prompt and a changed nondefault complete proposal in Preview. Prose, fences, unknown semantics, and malformed documents must leave the current candidate and temporary navigation intact. A missing Claude executable shows the launch error and Retry while retaining the prompt.

## Evidence limits

Read the run report for the exact build and tested entry points. Native wheel scrolling must be verified separately when the headless driver's axis events do not reach GPUI. Simulated GPUI input is supporting evidence and does not replace that native proof. Recheck composer width after integrating the model toolbar. Applied durability-warning rendering requires native fault injection or remains explicitly unverified.
