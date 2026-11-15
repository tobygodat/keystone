# Image input: do the tested CLIs take images natively? (TOB-59 probe)

Run on a Linux desktop on 2026-10-21 with Claude Code 2.1.301 and Codex 0.163.0, by
`scripts/probe-image-input.py`. Raw output: [image-input-probe.json](image-input-probe.json).

No inference: the whole probe ran under `unshare -rn`, whose only network is a private
loopback. Each CLI's model API pointed at a fake server on that loopback, which recorded
each request and answered it with a 400 error. Every CLI ran with a fresh `HOME`, a fresh
`CLAUDE_CONFIG_DIR` or `CODEX_HOME`, and a dummy key that only the fake server accepts.

Each CLI got one message per fixture, sent the way Keystone sends it: the text
`What is in this image?` and one image. The fixtures were a 64 × 48 gradient as png,
jpeg, gif and webp, and a 2400 × 1800 noise png of 13 MB.

## Claude Code 2.1.301 (stream-json)

- **It takes Anthropic image blocks in the `user` line.** Content
  `[{"type":"text",..}, {"type":"image","source":{"type":"base64","media_type":..,"data":..}}]`
  was accepted for all four types. The first model request carried the image with the
  same media type and the same bytes (SHA-256 equal) for png, jpeg, gif and webp.
- **It resizes large images itself.** The 13 MB png reached the model as a 477 KB jpeg.
- `--replay-user-messages` echoes the user message with its image blocks; the text block
  comes back unchanged.
- **It does not read `@path` mentions in a message with images.** The project held
  `notes.txt` with a sentinel string. `Summarise @notes.txt and the image` sent as a text
  block beside a png reached the model as typed, with no sentinel in any request. The same
  text sent alone as string content, the control, had the sentinel in every request, as
  the TOB-62 probe found. So in a message with images, a mention is only a path the model
  can choose to read with its own tools, as it is for Codex.

## Codex 0.163.0 (app-server)

- **It takes `localImage` input items on `turn/start`.** Input
  `[{"type":"text",..}, {"type":"localImage","path":..}]` was accepted for all four types.
  Codex reads the file and sends it as an `input_image` data URL.
- png, jpeg and webp reached the model with the same media type and bytes.
- **gif is converted to png** (a different byte hash, media type `image/png`). It is
  accepted, not refused.
- **It resizes large images itself.** The 13 MB png reached the model as a 9.5 MB png.
- The `userMessage` item it reports back lists the `localImage` with the path Keystone
  sent.
- `turn/steer` was not probed: the fake server's refusal ends a turn before a steer could
  reach it. Keystone sends a steer's images the same way, as `localImage` items.

## What Keystone does

Keystone sends each provider its native form and never resizes or re-encodes:

- Claude gets a base64 `image` block per attachment after the text block, with the
  media type Keystone detected from the file's bytes.
- Codex gets a `localImage` item per attachment after the text item, whose path is the
  copy in Keystone's data directory.

A mention sent with images is not read by Claude Code itself (above). Keystone does not
work around this: the mention stays in the text, and the agent can read the file.

Neither CLI refused any of the four types, so no type needs a fallback today. If a
provider refuses an image later, its error shows in the conversation like any other
provider error.
