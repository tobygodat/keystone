# Questions (TOB-296), 2026-11-02

Native evidence from `python3 scripts/questions-journey.py` on an Apple Silicon Mac, with
the scripted providers (their replies are test data). Screenshots are GPUI's own frames at
the window's Retina size, so the macOS title bar is not in them.

- [Claude's questions answered with the mouse](questions/claude-answered-by-mouse.png):
  Blue, Apple and Cherry chosen, Submit on.
- [Answered with the keyboard alone](questions/claude-answered-by-keyboard.png): from the
  composer, Shift+Tab into the card, Space on Cherry and Apple, "Teal" typed as the
  Color's Other answer. Tab to Submit and Enter sent it.
- [An MCP form holding a bad number](questions/codex-form-bounds.png): Count holds "two",
  so Submit stays off and the card says why.
- [After closing a window](questions/after-closing-a-window.png): Blue and the typed
  "kept.txt" are still there in the window left.
- [After quitting with a question pending](questions/relaunched.png): no card, since the
  process that asked has ended; the turn reads "Stopped: Keystone closed during this turn".
