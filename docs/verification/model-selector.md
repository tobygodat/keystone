# Choose a session's provider and model

The composer has one provider and model button. Open it before the first Send to choose the provider on the left and a model on the right. The model names come from each provider's catalog. **Provider default** leaves the choice to the provider's configuration.

## Click through the native app

1. Create a Codex session from the welcome screen or with **Ctrl+Shift+N**. **Ctrl+N** creates a Claude Code session.
2. Click **Codex · Provider default** in the composer toolbar. The two-column menu opens with the current choices checked.
3. Choose **Claude Code** on the left. The model column changes, the requested model resets to **Provider default**, and the draft remains.
4. Choose a model on the right. The menu closes and the composer keeps focus. **Escape** also closes the menu.
5. Type a message and click **Send**, or press **Ctrl+Enter**. The provider and model become fixed for that session.
6. Hover over the model button after the reply. It reports the model named by the running provider. Clicking the started button opens no menu.
7. Quit with **Ctrl+Q** and reopen the same session. The requested choice remains fixed. Until the provider reports again, the label is muted and its tooltip says the model is requested and not yet reported.

The model choices keep one height whatever the catalog holds: six and a half rows, or a third of a short window's height. Loading, a failure, an unavailable folder, a long catalog and either provider all use it, so a catalog that lands while the menu is open never moves a row from under the pointer. The menu opens upward, so any growth would move every row above the button. Longer catalogs scroll inside it, and the half row shows there is more. A short window also keeps the provider and account list to a third of its height, so many accounts scroll instead of pushing the menu past the window's edge.

If catalog discovery fails, **Provider default** remains available. **Retry** requests the catalog again and stays in view below the message, which scrolls when it is long. A failed first launch still fixes the session's choice. Start a new session to choose another provider or model.

![The native model menu with Claude Code selected](https://github.com/tobygodat/keystone/blob/pr-assets/TOB-32/model-menu.png?raw=true)

![The native Codex session after the selected model replies](https://github.com/tobygodat/keystone/blob/pr-assets/TOB-32/model-reported.png?raw=true)

[Watch the native restart and model-choice recording](https://github.com/tobygodat/keystone/blob/pr-assets/TOB-32/model-selector.webm).

![The full requested-model tooltip after restart](https://github.com/tobygodat/keystone/blob/pr-assets/TOB-32/model-tooltip.png?raw=true)

## Verification

The final native run used the actual GPUI binary on a private 1280 by 900 desktop at Default density. Both scripted providers received the chosen model and reported it in their replies. The saved workspace confirmed the requested model, the draft, and the lock before and after restart. Provider changes reset the model and preserved the draft. Catalog failure, recovery through Retry, Escape, Send, Ctrl+Enter, and the read-only state were exercised.

The final open-trigger fix keeps its white selection fill while the pointer remains over it. Selected rows have a white fill, outline, check, and provider edge. The folder and model buttons are 28 px high at Default density.

The complete core checks passed. The final build, formatting check, UI suite, and native flow passed. The local `.audit/model-selector/native-final.md` report records binary identity, screenshots, workspace observations, and cleanup. The tooltip repair also passed native checks for the full model-status text and a title with its shortcut. Both app exits were 0 and cleanup reported no errors.

## Limits

The scripted replies are test data. They prove Keystone's provider handling without a model call. Live provider catalogs and real model execution were not repeated in this final pass. Light theme, long model lists, and the host file-manager action were not exercised. The fixed-height choices (TOB-43) were checked natively at 1280 by 900 with Codex ready, Claude loading and Claude ready. GPUI tests cover a long catalog's layout, short windows, Spacious density and scrolling a long account list. Scrolling a long catalog natively and choosing a model below the fold were not exercised. Laptop delivery remains with the workspace arrangement owner.

The provider marks use the Claude browser extension's `claude_icon.svg` and the OpenAI Blossom from the OpenAI brand kit. The approved Paper screenshots supplied the visual reference. The code also uses these marks on the welcome buttons, which were outside the approved model screenshots.
