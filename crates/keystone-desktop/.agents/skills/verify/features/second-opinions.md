# Second opinions (TOB-56)

A second opinion puts a session's conversation, or the part of it up to one reply, to another provider or account in a new session beside it. It reuses [handoff](handoff.md)'s text and link. The source session is never changed, and a finished second opinion never starts a turn there.

## Entry points

- The top bar's Second opinion button (`second-opinion`), beside Hand off. It asks about the conversation.
- Ctrl+Alt+O. With a finished reply focused in the transcript (row navigation), it asks about that reply; anywhere else, about the conversation.
- The command palette: "Second opinion…". Like the shortcut, it should ask about a reply when the palette returns focus to a focused reply; only the conversation case is tested.
- A finished reply's Second opinion button (`second-opinion-<row>`, view-scoped like other selectors), beside Copy reply. It asks about that reply.

## Expected behaviour

- A new session opens in a tab beside the source, in the source's folder, titled "Second opinion: <source title>" ("Second opinion on session N" for a placeholder title). The `workspace` projection shows `handed_off_from` set to the source and `second_opinion: true`.
- Its approval mode is Read only. The user can change it, and the provider, account and model, before Send. The model selector opens on the other provider's default account.
- Its composer holds the handoff text, which opens "For a second opinion on Keystone session N…", then a question to edit at the end, next to the cursor. For one reply, the text stops at that reply and the question names it by position (`#N`).
- The second opinion shows "Second opinion on" (`opinion-of`) at the top, opening the source.
- Once it has sent, the source shows "Second opinion from <provider> · running" (`opinion-<id>`), opening the second opinion. It reads "waiting for you" while it waits for an approval, a widget or a recovery choice, "reading…" while its history loads, "done" once its latest turn completed with a reply, "failed", or "stopped" for a turn the user stopped, one cut off when Keystone closed, or one that ended without a reply. Second opinions never show as "Continued in".
- When it is done, Use in conversation (`opinion-use-<id>`) adds "Second opinion from <provider> (Keystone session N):" and the reply that answered its latest message to the source's composer, after any draft there. It reads the reply when pressed, so a second opinion that has started another turn offers nothing; pressing it twice adds it once. Nothing is sent and no turn starts.
- Through the orchestrator a second opinion can read its source (`session_read` names it with `second_opinion: true`), but it launches, sends to and stops nothing, in any approval mode. A handoff or fork of a second opinion is held to the same.
- A launched session's result never goes to a second opinion, even one sent on the result's parent: the parent gets it as it would without the second opinion.
- The rows, links and Use in conversation survive a restart.
- It is refused with an error, as Hand off is, when the source has not sent anything, has no folder, is still loading, or its saved history could not be read.

## Native journey

Use fixture homes and the scripted providers. Give a fake Claude session two turns. Ask for a second opinion from the top bar, keep the fake Codex default, and Send. Then check:

- The fake Codex reply ends with the turn's approval policy and sandbox (`on-request`, `readOnly`), and the method log or `KEYSTONE_FAKE_CODEX_STORE` thread shows the same `turn/start` override.
- The source's row reads running, then done (screenshots; the driver reads names, not the word), and Use in conversation appears once it is done.
- Use in conversation fills the source's composer; the source's turn count and stored history are unchanged.
- After Quit and relaunch, the row, the link and Use in conversation still work, and `workspace` still holds the link.

Also ask about one reply with its row button, and check that the draft stops at that reply.
