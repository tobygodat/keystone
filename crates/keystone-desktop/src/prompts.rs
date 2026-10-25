//! Inserting saved prompts: the palette's "Insert prompt…" list, and the insert the
//! `/` menu shares. Inserting fills the composer; it never sends.
//! The prompts themselves are edited in Settings → Prompts.

use crate::Keystone;
use gpui::{
    Context, Entity, EntityInputHandler, FocusHandle, Focusable, IntoElement, Window, prelude::*,
};
use keystone_core::Prompt;
use keystone_ui::{CommandPalette, CommandPaletteState, PaletteItem};

gpui::actions!(keystone, [InsertPrompt]);

const ITEM: &str = "prompt-";

/// The open prompt list and the focus it returns to when it closes.
pub(crate) struct PromptList {
    pub(crate) state: Entity<CommandPaletteState>,
    pub(crate) restore: Option<FocusHandle>,
}

/// Where a prompt is offered: `/shortcut` and its project's name.
pub(crate) fn prompt_detail(prompt: &Prompt) -> String {
    let scope = prompt.project().map(|project| project.name().to_owned());
    [prompt.shortcut().map(|s| format!("/{s}")), scope]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

impl Keystone {
    /// The prompts this window offers, in the user's order: the global ones and its
    /// project's.
    pub(crate) fn window_prompts<'a>(&self, cx: &'a gpui::App) -> Vec<&'a Prompt> {
        let project = self.presentation.project.as_ref().ok();
        self.state
            .read(cx)
            .store()
            .workspace()
            .prompts()
            .iter()
            .filter(|prompt| prompt.shown_in(project))
            .collect()
    }

    /// The prompts offered to the selected session; none without one, since only a
    /// session shows a composer.
    fn shown_prompts(&self, cx: &gpui::App) -> Vec<Prompt> {
        if self.selected(cx).is_none() {
            return Vec::new();
        }
        self.window_prompts(cx).into_iter().cloned().collect()
    }

    /// Puts prompt `id`'s text in the composer, in place of the whole draft when
    /// `replace`, else at the cursor. A prompt no longer offered
    /// to the selected session, as another window may have just changed, is not
    /// inserted, and the window says so.
    pub(crate) fn insert_prompt(
        &mut self,
        id: u64,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(body) = self
            .shown_prompts(cx)
            .into_iter()
            .find(|prompt| prompt.id() == id)
            .map(|prompt| prompt.body().to_owned())
        else {
            if self.selected(cx).is_some() {
                self.error = Some("Prompt no longer available.".into());
                self.refresh(cx);
            }
            return;
        };
        self.composer.update(cx, |input, cx| {
            if replace {
                input.set_value(body, cx);
            } else {
                input.replace_text_in_range(None, &body, window, cx);
            }
        });
        self.composer.focus_handle(cx).focus(window, cx);
        self.save_draft(cx);
        self.refresh(cx);
    }

    /// "Insert prompt…": lists the prompts offered to the selected session, or opens
    /// Settings → Prompts when there are none, or no session whose composer it could
    /// fill.
    pub(crate) fn open_prompt_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompts = self.shown_prompts(cx);
        if prompts.is_empty() {
            return self.open_prompts(window, cx);
        }
        let restore = self
            .prompt_list
            .take()
            .and_then(|list| list.restore)
            .or_else(|| self.palette.take().and_then(|palette| palette.restore))
            .or_else(|| self.projects.take().and_then(|list| list.restore))
            .or_else(|| window.focused(cx));
        let items = prompts
            .iter()
            .map(|prompt| {
                PaletteItem::new(format!("{ITEM}{}", prompt.id()), prompt.title().to_owned())
                    .detail(prompt_detail(prompt))
            })
            .collect();
        let state = cx.new(|cx| CommandPaletteState::with_placeholder(items, "Search prompts", cx));
        state.focus_handle(cx).focus(window, cx);
        self.prompt_list = Some(PromptList { state, restore });
        self.refresh(cx);
    }

    fn close_prompt_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(list) = self.prompt_list.take() else {
            return;
        };
        list.restore
            .unwrap_or_else(|| self.focus.clone())
            .focus(window, cx);
        self.refresh(cx);
    }

    fn confirm_prompt(&mut self, item: &PaletteItem, window: &mut Window, cx: &mut Context<Self>) {
        self.close_prompt_list(window, cx);
        if let Some(id) = item.id.strip_prefix(ITEM).and_then(|id| id.parse().ok()) {
            self.insert_prompt(id, false, window, cx);
        }
    }

    pub(crate) fn prompt_list_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let list = self.prompt_list.as_ref()?;
        let dismiss = self.scoped_listener(cx, |this, _: &(), window, cx| {
            this.close_prompt_list(window, cx)
        });
        Some(
            CommandPalette::new("prompts", &list.state)
                .on_confirm(
                    self.scoped_listener(cx, |this, item: &PaletteItem, window, cx| {
                        this.confirm_prompt(item, window, cx)
                    }),
                )
                .on_dismiss(move |window, cx| dismiss(&(), window, cx)),
        )
    }
}
