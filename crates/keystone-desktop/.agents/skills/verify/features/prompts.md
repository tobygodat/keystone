# Saved prompts

Prompts the user saves once and inserts into the composer. Inserting fills the composer and never sends. A prompt is global by default or belongs to one project.

## Sub-features

- `prompts-store` saves each prompt in the `prompts` projection (schema 4, after the `projects` table of schema 3) with a `prompt.upserted` or `prompt.removed` event: id, title, body, optional `shortcut` (without its `/`), optional `project` (the #69 project key, its canonical folder), and `uses`. Prompt IDs come from the same counter as session IDs. The driver's `workspace` lists them under `workspace.prompts`.
- `prompts-library` is Settings → Prompts: each prompt with its first line, `/shortcut`, project and use count, with Move up, Move down, Edit and Delete (`prompt-up-<id>`, `prompt-down-<id>`, `prompt-edit-<id>`, `prompt-delete-<id>`). The form below (`prompt-title`, `prompt-shortcut`, `prompt-body`, `prompt-scope`, `prompt-save`) adds one, or saves the edited one. Enter in the title or shortcut saves; Shift+Enter is a newline in the text. A blank title or text, a shortcut that is not one word, or a shortcut another prompt offered in the same windows uses is refused in `settings-error`.
- `prompts-scope` offers "All projects" and this window's project. A project prompt is offered only in windows with that project open (Ctrl+O switches).
- `prompts-slash` adds the session's prompts to the `/` menu with source Saved, as `/shortcut` or, without one, by title. Choosing one replaces the draft with its text.
- `prompts-conflict`: a provider command of the same name as a shortcut wins. Sending `/name` runs the command; in the `/` menu the command is listed first and the prompt's row says "Saved · command wins". The library row shows `prompt-conflict-<id>` for Terminal only built-ins and for commands the selected session's account lists; opening the library asks for that list.
- `prompts-palette` is the palette's "Insert prompt…" (Ctrl+Shift+I): a list (`prompts`) of the prompts offered here plus "Manage saved prompts…". Enter inserts at the caret. With no prompts offered it opens Settings → Prompts.

## How to get to it (user POV)

- Ctrl+Comma, then Prompts. Fill Title, optional /shortcut, the text, choose a scope, Add.
- In a session's composer type `/` and part of the shortcut or title, then Enter.
- Ctrl+K, "Insert prompt…", Enter; or Ctrl+Shift+I.

## Driving it with keystone-verify

- `start` with the scripted providers and `KEYSTONE_PROJECT_DIR`. Create a Claude session (Ctrl+N), open Settings → Prompts, `click` each form field and `type`; use `keypress` Shift+Return for line breaks. `click` `prompt-save` and read `workspace` until `prompts` holds it.
- Ctrl+Q, `launch`, and require the prompt in `workspace` and its `prompt-row-<id>` in the library.
- In the composer type `/` and part of the shortcut; wait until `slash-menu-list-row-0` shows without `slash-menu-status`. Enter, then require the session's `draft` to equal the body and the scripted Claude's `.fake-claude-messages` to stay empty.
- With a fixture `CLAUDE_CONFIG_DIR/commands/<name>.md`, save a prompt with `/name` and require "Saved · command wins" in the `/` menu.
- Delete every prompt, Ctrl+Q, `launch`, and require `prompts-empty`.

## Gotchas

- Template variables are not supported; the body is inserted as written.
- Going back to a build before schema 4 needs a database restore.
