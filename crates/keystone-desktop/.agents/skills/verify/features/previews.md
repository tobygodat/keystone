# Previews

See images in a conversation as thumbnails, open one over the window at fit or 1:1, and open videos in the system player. Nothing is fetched from the network.

## Sub-features

- `preview-reply` shows a reply's Markdown image as a thumbnail when it is a paragraph of nothing but images whose paths are inside the session folder. `%20` escapes are decoded. A path outside the folder, a missing file or a file that isn't an image shows its alt text or path. An image in a sentence, or one with a URL (`https:`, `//host/…`), stays a link showing its alt text or URL, and nothing is requested.
- `preview-change` shows an image or video that a file-change row touched (Claude Code's Write or Edit, Codex's fileChange) under the row. Until the change has completed, only a file inside the session folder shows.
- `preview-sent` shows a message's attached images as thumbnails. A copy the retention sweep removed is the chip with a file icon.
- `preview-thumbnail` is a 360 by 200 tile (362 by 202 with its border), the image shrunk to fit and centered. It holds that size while the file loads; a file that can't be shown is then replaced by its path. A file is read again for each new row that shows it, so a rewritten image shows its new content in later rows.
- `preview-video` shows an mp4, webm or mov as Open in player, after the file's name in a reply. The button is disabled while the file is checked; a missing file shows nothing under a file change and its path in a reply.
- `preview-overlay` opens from a thumbnail: Fit (the default) shrinks the image to the window and never enlarges it; 1:1 shows it at its own size in pixels and scrolls; an image over 4096 pixels is decoded at 4096 and enlarged to that size. While it shows, app shortcuts and Tab are off. Open externally and Open in player run `xdg-open` (`open` on macOS) on the file; an opener that can't start or exits with an error shows `Couldn't open` beside the button. Copy path copies the file's full path and reads Copied for a moment. Escape, the close button or a press outside the image closes it and gives focus back.
- `preview-decode` reads and decodes on a background thread. Thumbnails decode at no more than 720 pixels on the longest side, the overlay at 4096, chips at 64; files over 64 MiB are not decoded, and photos turn upright from their EXIF orientation. A file a row touched is read again when the row finishes. The 48 most recently shown thumbnails and chips stay decoded, with any on screen; the overlay's image is dropped when it closes, with its GPU texture.

## How to get to it (user POV)

- Have an agent reply with `![](./shot.png)`, or write or edit an image or video, then click the thumbnail.
- Attach an image, send it, and click its thumbnail under the message.

## Driving it with keystone-verify

The scripted Claude replies with `KEYSTONE_FAKE_CLAUDE_REPLY` exactly and, with `KEYSTONE_FAKE_CLAUDE_WRITE` set to a path relative to its working directory, reports a completed Write of it before the reply (it writes nothing). `start` does not pass those two, so point `KEYSTONE_CLAUDE_PATH` at a wrapper script that exports them and execs `keystone-fake-claude`. Put a stand-in `xdg-open` that logs its arguments first on `PATH`:

```json
{"env": {"KEYSTONE_CLAUDE_PATH": "/tmp/<fixture>/bin/claude", "KEYSTONE_PROJECT_DIR": "/tmp/<fixture>/project", "PATH": "/tmp/<fixture>/bin:/usr/bin:/bin"}}
```

with the wrapper setting `KEYSTONE_FAKE_CLAUDE_REPLY` to `Done.\n\n![](./shot.png)\n\n![](http://127.0.0.1:<port>/remote.png)` and `KEYSTONE_FAKE_CLAUDE_WRITE` to `out/demo.mp4`.

Serve the fixture's parent with `python3 -m http.server <port> --bind 127.0.0.1` and require its log to stay empty.

- Send any message with `welcome-claude`, `composer-field` and `send`, then `wait_for` `thumbnail-<row>-0` (row is the reply's) and `video-<row>-0-open` (row is the file change's).
- `click` the thumbnail, `wait_for` `preview-image`, then `click` `preview-actual` and `preview-fit`; compare `preview-image` bounds with the PNG's size.
- `click` `preview-open` or `video-<row>-0-open` and read the stand-in's log. `click` `preview-copy` and read `wl-paste` on the run's socket.
- Press Escape and `wait_for` `preview-overlay` with `"present":false`.

Named elements: `thumbnail-<row>-<n>`, `video-<row>-<n>`, `video-<row>-<n>-open`, `video-<row>-<n>-open-error`, `preview-open-error`, `sent-attachment-<row>-<n>`, `preview-overlay`, `preview-image`, `preview-fit`, `preview-actual`, `preview-open`, `preview-copy`, `preview-close`. `<n>` counts a reply's local images, or a file change's paths, from 0.

## Not covered

- Video playback inside Keystone; videos open in the system player.
- Remote images are never fetched.
- Animated GIF and WebP show their first frame.
