# Attachments

Attach images to a draft by paste, drop or the attach button, see them as chips, keep them through a restart, and send them to Claude Code or Codex in the provider's own image form. Any other file is mentioned by its path instead.

## Sub-features

- `attach-paste` attaches an image on the clipboard with Ctrl+V in the composer. Text on the clipboard still pastes as text.
- `attach-drop` attaches files dropped on the composer.
- `attach-button` opens the platform file picker from the paperclip button, Ctrl+Shift+U or the palette's "Attach files…". Where no picker is available it shows a typed-path field with "No file picker is available here. Type the file's path instead." A relative path is read from the session's folder and `~/` is the home folder. Enter attaches; Escape closes the field.
- `attach-chips` shows each image under the draft as a chip with a thumbnail, its name and a remove button. A file that is not a png, jpeg, gif or webp image is mentioned at the caret as `@path` instead.
- `attach-refuse` refuses an image over the limit (20 MiB, or `KEYSTONE_ATTACHMENT_MAX_MB`) with its size and the limit, in red under the draft, and leaves the draft as it was.
- `attach-restore` saves the draft's images with the draft, so they return after Quit and relaunch.
- `attach-send` sends the images with the message, or alone. Claude Code gets base64 image blocks after the text; Codex gets `localImage` items whose paths are the copies in the data folder. The message's row shows the images as thumbnails under it, and the draft's images clear.

## How to get to it (user POV)

- Select a session, copy an image, click the draft and press Ctrl+V.
- Click the paperclip left of Send, or press Ctrl+Shift+U, and pick files. Without a picker, type a path and press Enter.
- Drag image files from a file manager onto the composer.
- Hover a chip's × and click it to remove that image.

## Driving it with keystone-verify

Pass the scripted providers as in [conversations](conversations.md), plus a receipt file the scripted providers append each received image to:

```json
{"env": {"KEYSTONE_CODEX_PATH": "<workspace>/target/debug/keystone-fake-codex", "KEYSTONE_CLAUDE_PATH": "<workspace>/target/debug/keystone-fake-claude", "KEYSTONE_PROJECT_DIR": "/tmp/<disposable git repo>", "KEYSTONE_FAKE_IMAGE_RECEIPT": "/tmp/<run>/receipt.jsonl", "KEYSTONE_ATTACHMENT_MAX_MB": "1"}}
```

Each receipt line is one image: from the scripted Claude `{"provider":"claude","source":"base64","media_type":…,"sha256":…,"bytes":…}`, from the scripted Codex `{"provider":"codex","path":…,"exists":…,"sha256":…,"bytes":…}`. Each scripted reply also ends with `**Images:** <type> <first 8 hex of sha256>` per image. The copies are in `attachments/` beside the run's state database, named `<sha256>.<ext>`.

- Paste: run `wl-copy --type image/png < shot.png` with `WAYLAND_DISPLAY` set to the run's Sway socket, `click` `composer-field`, press Ctrl+V, then `wait_for` `attachment-0`. `workspace` shows the session's `draft_attachments`.
- Typed path: the run's private bus has no xdg-desktop-portal, so the paperclip falls back to the typed-path field. `click` `attach`, `wait_for` `attach-path`, type the path and press Enter, then `wait_for` `attachment-1`.
- Refusal: attach an image larger than `KEYSTONE_ATTACHMENT_MAX_MB` and `wait_for` `attachment-notice`.
- Remove: `click` `attachment-remove-<n>`.
- Restart: Quit with Ctrl+Q, `launch`, and require the same chips.
- Send: type a message and press Ctrl+Enter. Read the receipt and compare each hash with `sha256sum` of the original. The row under the message is `sent-attachments-<row>`.

Named elements: `attach`, `attach-path`, `attach-path-confirm`, `attachment-<n>`, `attachment-remove-<n>`, `attachment-notice`, `sent-attachments-<row>`, `sent-attachment-<row>-<n>`.

## Not covered

- A native drop needs a Wayland drag source on the run's socket, which the driver does not provide; the GPUI tests cover drops.
- Screenshot capture is TOB-61. Sent images' thumbnails and their preview are in [previews](previews.md).
