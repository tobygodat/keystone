# Find in a session (TOB-456)

One bar over the top right of the transcript finds plain text in the session's whole saved conversation, ignoring case.

## Sub-features

- `find-open`: Ctrl+F (Cmd+F on macOS) with focus in the conversation or the composer opens `find-bar` with focus in `find-field`. Pressed again it selects the query. The palette row is "Find in session".
- `find-count`: typing marks every match on a quiet wash and the current one stronger. `find-count` shows `3 of 12`, or `No matches`. The first match is the first at or below the top of the view.
- `find-step`: Enter and `find-next` go to the next match, Shift+Enter and `find-previous` to the previous one, wrapping at the ends. The match scrolls into view a third of the way down, even inside a reply taller than the window.
- `find-collapsed`: a match in a collapsed reasoning, command output or tool row opens the row.
- `find-streaming`: a reply that streams more matches raises the count without moving the reader or the current match.
- `find-close`: Escape, `find-close` or the palette's "Close find" closes the bar and gives focus back where it was.

## Not searched

- Edit rows: their paths and diff. The diff view and Changes show those.
- Widgets, attachments, notes, turn errors and recovery reports.
- Tool output the log's cap cut off.

## Driving it with keystone-verify

Preconditions: a session long enough to scroll, with a word in several replies and in a command's output.

- Focus the composer, `input` `{"actions":[{"action":"keypress","keys":["ctrl","f"]},{"action":"type","text":"word"}]}`. Require `find-bar`, `find-count` reading `1 of N`, and the marked match on screen in the screenshot.
- Press Enter several times, then Shift+Enter. Require the count to step and wrap, and each match to come into view.
- Click `find-next` and `find-previous` and require the same, with focus still in `find-field`.
- Collapse a command row with a match in its output, then step to that match. Require `activity-body-<row>` to return.
- Press Escape. Require `find-bar` gone and focus back in the composer.
- Focus the transcript (Ctrl+Shift+T), open find, click `find-close`. Require focus back on the transcript.
- Open the terminal drawer, focus a shell and press Ctrl+F. On Linux the shell gets it (no `find-bar`); on macOS Cmd+F opens find.
