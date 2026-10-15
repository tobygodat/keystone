# MonoCode interface investigation

The current reference is the user's new-session screenshot. It shows a quiet heading above a centered composer, workspace sidebar, activity rail, and tabs. Those are visual observations only. Model, permission, command, and attachment menus have not been opened in this session.

## Target

The desktop entry at `~/.local/share/applications/MonoCode-fork.desktop` points to `~/Applications/MonoCode-fork.AppImage`. The mounted executable is `/tmp/.mount_MonoCoJPIDlK/usr/bin/monocode`.

REA `open_binary` bound the ELF x86-64 executable with SHA-256 `ce4cb7ab28291f10248286ce8f11523fe23bf04adcefb6f546ea78a3c21021cd`. This identifies the selected installed artifact. It does not establish any menu behavior.

## Questions

- [ ] How does creating a session choose and persist its provider?
- [ ] Which provider/model choices are exposed and when can they change?
- [ ] Which composer menus are available, and what do their actions invoke?

REA searches for `create_session` procedures and `permission` strings failed with `provider_timeout` while waiting for Hopper bridge readiness. Launcher output records plugin discovery but does not identify the cause. A loader or license dialog is only a possible cause stated by the tool. CUA cannot inspect it in this session. Do not repeat the searches before resolving this blocker.

REA objdump symbol Evidence `ev_d03721a54933fd51abbb0dc3cdda3b6d638a76512e5a16086e878a2a9ce4528e` is preserved in `.audit/monocode-interface-evidence.json`. Static symbols include `monocode_lib`, `session_store`, `worktrees`, and Tauri types. These identify investigation candidates, not feature behavior. The producer explicitly reports `output_truncated: true` and `completion_status: unknown`; absence from this output cannot establish absence from MonoCode. Transport paging did not support this operation, so the retained record was exported instead. All three feature questions remain unresolved. The native REA session was closed after export.

## Keystone design decision

Two independent source sketches compared a separate welcome chooser with a shared composer. Choose one composer that is centered before a transcript and bottom-aligned after content. When no session exists, show actual Claude Code/Codex creation buttons. After creation, focus the same draft entity. This avoids a second input buffer or draft transfer state. Remove the inert send arrow while provider submission is unavailable.

The welcome heading is 20 logical pixels with normal weight to follow the reference's restrained hierarchy. Footer controls can wrap at narrow widths. Existing draft persistence and save-before-navigation ownership remain unchanged.

## Verification boundary

CUA reports `apps=[]` and `browsers=[]`, and native computer APIs are disabled in this session. Live MonoCode click-through and new Keystone screenshots are blocked. Earlier screenshots describe the previous build, not this new layout. Do not reuse them as evidence of the current change.

## Current implementation checks

All three GPUI behavior tests passed, including welcome creation, input focus, and draft restoration after switching sessions. These checks do not validate rendered pixels or live provider behavior.
