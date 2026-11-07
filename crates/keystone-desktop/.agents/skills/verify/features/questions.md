# Questions

A provider can stop to ask the user something: Claude's `AskUserQuestion`, Codex's `request_user_input`, or an MCP server's form. See "Questions" in `docs/design/providers.md`.

The scripted providers ask on cue. A prompt containing "ask me" makes the fake Claude ask two `AskUserQuestion` questions (Color: Red or Blue, single choice; Fruit: Apple, Banana or Cherry, any number; both take typed "Other" text), and the fake Codex ask two `request_user_input` questions (Color and Name, each with typed text). A prompt containing "fill a form" makes the fake Codex send an MCP form from a server named `probe`: Count (a whole number), Note (text), Rush order (Yes or No), Size (Small or Large, required) and Toppings (Cheese, Olives or Basil, any number). A prompt containing "run" and "ask during approval" makes the fake Claude ask its questions while the command's approval waits: the approval card shows first, and the question card only after Allow or Deny. Each reply starts with the answers it got, or says they were declined; that fake Claude reply also says whether the command ran.

On macOS, `python3 scripts/questions-journey.py` runs the whole journey below through the automation inbox and leaves screenshots in `.audit/questions-journey-*`.

## Entry points

| Entry point | Expected |
| --- | --- |
| Mouse: click options, type in a field, click Submit (`question-submit`) | The card (`question`) goes, the reply names the answers |
| Keyboard: from the composer, Shift+Tab into the card, Space on an option, type in a field, Tab to Submit, Enter | The same, and focus returns to the composer |
| Decline (`question-decline`) by mouse or Enter | The card goes, the reply says the questions were declined, and the turn goes on |
| Submit with a required field blank or a number field holding text | Submit is off, and the card says which field needs work |
| Sidebar while a question waits | The session reads "Waiting for your answer" under Needs action; Settle refuses |
| Answer partly, open a second window (Ctrl+Alt+N) and select the session there, close that window (Ctrl+Shift+W), select the session in the window left | Both windows show the same partial answers; none are lost |
| A question that arrives while an approval waits | The approval card shows and the question waits behind it; Deny denies the command, then the question card shows, and answering it approves nothing |
| Quit (Ctrl+Q) with a question waiting, relaunch | No card: the provider that asked ended. The turn reads "Stopped: Keystone closed during this turn" |

Option buttons are named `question-<field>-<option>` and text fields `question-<field>-text`, counting from 0. Check each screenshot, not only the names: a selected option shows a check mark.
