# Keyboard transcript actions

The transcript uses one region focus and two reusable action handles per conversation occurrence and session, not one handle per history row. This is a bounded keyboard route, not a full accessibility or text-selection claim.

- Ctrl+Shift+T enters the most recently focused visible transcript. F6 and Shift+F6 cycle visible conversation occurrences. Hidden tabs are skipped.
- Up/Down select logical visible rows; Home/End select the first/last row. These keys reveal the selected row without running any of its actions.
- Tab/Shift+Tab cycle the row's copy actions, Jump to latest when present, and the row region. The row copy comes first, followed by fenced-code copies in document order. Enter/Space activate the focused button on release. A row without actions still has the region and optional Jump stop.
- Ctrl+Shift+C copies the focused action, or the row's first available copy action while the region holds focus. A region without a selected row first selects its visible anchor. Ctrl+C is unchanged. Alt+End focuses Jump to latest without activating it. Escape or Ctrl+L returns to the composer.
- User-message Copy is revealed by row focus as well as hover. Row and button focus have visible outlines. Action focus scrolls its actual button into view, including inside a tall reply.
- Entry preserves the viewport, including a tall latest reply's bottom. Row movement minimally reveals clipped or offscreen destinations. Send and Jump deliberately clear the old keyboard cursor, so re-entry cannot jump back to an obsolete row. Escape dismisses an open menu or Customize before it leaves the transcript.

Commands and displayed shortcuts come from commands.rs. Composer input, search, Files, Settings, palette and dialogs retain their own key contexts. Enter/Space here do not Send, approve or settle. Settings yields to transcript-entry commands as it does to other workspace commands.

## Native journey

Build the exact binary and corrected private driver using ../SKILL.md and ../setup.md. Use scripted providers only. Start a private headless Sway run; retain doctor, live PID/hash, named geometry and screenshots. Check a welcome frame before creating a session with Ctrl+Shift+N and sending through Ctrl+Enter.

For long-history coverage, quit with Ctrl+Q and require exit 0 and no window. Only then write a clearly labeled synthetic session log and relaunch. This fixture boundary proves handling of long history, not user-created history. Create a duplicate conversation through Customize, Apply, and close Customize. Do not inject layout or focus state to prove native navigation.

Use only keyboard input for transcript entry, row/action navigation and activation. Inspect exact Wayland clipboard bytes for a user message, a completed reply, a command with no output, command output and each fenced-code action. Use the private socket returned by start, never the host clipboard. Include a reply taller than its viewport and inspect its focused code button after Tab navigation. Read named row geometry to prove virtualization and independent pane offsets, not merely successful key delivery.

With both panes on different earlier rows, enter one last and explicitly Send from the composer. Only that occurrence follows the new reply; the other retains its offset. Complete a scripted streaming update with composer focus and require unread status to remain. Entering an earlier row or merely focusing Jump must not acknowledge. Activate Jump with Enter and Space in separate occurrences, require only the intended pane to move, and check deliberate focused-latest acknowledgement.

Switch preferred tabs and narrow fitted tabs while a copy action holds focus. Widen again and exercise Ctrl+L, search, Files, palette, Settings and a dialog; shortcuts must not be swallowed by an unrendered focus handle. Switch sessions/projects and back; each occurrence retains its logical row independently. No navigation/copy action may dispatch a provider turn or answer an approval.

While a reply streams, keep a fenced-code action focused across appended text and completion. It remains the same code ordinal even when completed-reply Copy appears. Check its clipboard bytes and visible focus again.

Quit gracefully, inspect exit 0, relaunch and exercise the keyboard route again. Stop and require errors:[] and scratch_retained:null. Retain failed assertions rather than replacing them with a passing screenshot. Run bash scripts/check.sh ui and the ignored perf_transcript_draw benchmark separately; automation runs disable sidebar caching and do not establish performance.
