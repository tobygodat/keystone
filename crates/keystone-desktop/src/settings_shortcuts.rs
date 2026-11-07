//! Settings → Shortcuts: every palette command with the keys that run it. Its button
//! records new keys, and the reset beside it, or Reset all, goes back to the defaults.
//! Each command keeps the context it runs in.

use crate::Keystone;
use crate::app_state::AppState;
use crate::commands::{COMMANDS, Command};
use crate::keymap::{self, Keymap};
use crate::settings_page::SettingsPage;
use gpui::{
    AnyElement, App, Context, FocusHandle, Keystroke, Subscription, Window, div, prelude::*, px,
};
use keystone_core::Applied;
use keystone_ui::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;

/// The section's own state, kept while the settings page is open.
pub(crate) struct ShortcutsPage {
    reset_all: FocusHandle,
    /// Focus for each row's buttons, made as rows first render.
    stops: RefCell<HashMap<String, FocusHandle>>,
    recording: Option<Recording>,
    /// The command whose last recording was refused, and why: the key edits text or
    /// types in the composer where the command runs.
    refused: Option<(&'static str, String)>,
}

struct Recording {
    command: &'static Command,
    /// Takes this window's keystrokes while the shortcut's button has focus, until it is
    /// dropped, as closing the page does.
    _keys: Subscription,
}

impl ShortcutsPage {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            reset_all: cx.focus_handle().tab_stop(true),
            stops: RefCell::default(),
            recording: None,
            refused: None,
        }
    }

    pub(crate) fn stop_recording(&mut self) {
        self.recording = None;
        self.refused = None;
    }

    fn stop(&self, id: &str, cx: &App) -> FocusHandle {
        self.stops
            .borrow_mut()
            .entry(id.to_owned())
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }
}

/// "A", "A and B", "A, B and C".
fn listed(labels: &[String]) -> String {
    match labels {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// "Also bound to A and B." A label that ends in "…", such as "Hand off…", ends the
/// sentence itself.
fn also_bound(labels: &[String]) -> String {
    let text = format!("Also bound to {}", listed(labels));
    if text.ends_with('…') {
        text
    } else {
        text + "."
    }
}

impl Keystone {
    fn shortcuts_page(&mut self) -> Option<&mut ShortcutsPage> {
        self.settings.as_mut().map(|page| &mut page.shortcuts)
    }

    /// The Shortcuts section's content, below its title.
    pub(crate) fn shortcuts_section(
        &self,
        page: &SettingsPage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = &page.shortcuts;
        let keymap = keymap::current(cx);
        let bindings = cx.key_bindings();
        let bindings = bindings.borrow();
        let rows: Vec<_> = COMMANDS
            .iter()
            .map(|command| {
                let clashes = keymap.clashes(command, &bindings);
                self.shortcut_row(page, keymap, command, clashes, cx)
            })
            .collect();
        let skipped = keymap.skipped().to_vec();
        let saved = keymap.has_overrides() || !skipped.is_empty();
        v_flex()
            .automation_id("settings-shortcuts")
            .gap(spacing::MD)
            .child(
                h_flex()
                    .gap(spacing::MD)
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new("Select a shortcut, then press the keys to run it with.")
                                .color(Color::Muted),
                        ),
                    )
                    .child(
                        Button::new("settings-shortcuts-reset-all", "Reset all")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .disabled(!saved)
                            .track_focus(&page.reset_all)
                            .debug_selector(|| "settings-shortcuts-reset-all".into())
                            .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                                this.save_shortcuts(|state, cx| state.reset_shortcuts(cx), cx);
                                this.focus_shortcut(COMMANDS[0].id, window, cx);
                            })),
                    ),
            )
            .when(!skipped.is_empty(), |d| {
                d.child(
                    v_flex()
                        .automation_id("settings-shortcuts-skipped")
                        .gap(px(2.))
                        .child(
                            Label::new("Skipped saved shortcuts")
                                .weight(gpui::FontWeight::MEDIUM)
                                .color(Color::Warning),
                        )
                        .children(skipped.into_iter().map(|reason| {
                            Label::new(reason).size(TextSize::Sm).color(Color::Warning)
                        })),
                )
            })
            .child(v_flex().children(rows))
            .into_any_element()
    }

    fn shortcut_row(
        &self,
        page: &ShortcutsPage,
        keymap: &Keymap,
        command: &'static Command,
        clashes: Vec<String>,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = command.id;
        let recording = page
            .recording
            .as_ref()
            .is_some_and(|recording| recording.command.id == id);
        let shortcut = format!("settings-shortcut-{id}");
        let reset = format!("settings-shortcut-{id}-reset");
        let keys = if recording {
            "Press a shortcut…".to_owned()
        } else {
            match keymap.keys(command) {
                "" => "Not set".to_owned(),
                keys => keystone_ui::shortcut_text(keys),
            }
        };
        let warning = if let Some((_, reason)) = page.refused.as_ref().filter(|(at, _)| *at == id) {
            Some(reason.clone())
        } else if !clashes.is_empty() {
            Some(also_bound(&clashes))
        } else {
            None
        };
        v_flex()
            .py(spacing::SM)
            .hairline_t(cx)
            .child(
                h_flex()
                    .gap(spacing::MD)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(command.label).truncate()),
                    )
                    .when(keymap.overridden(id), |d| {
                        let selector = reset.clone();
                        d.child(
                            IconButton::new(reset.clone(), IconName::Restore)
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .track_focus(&page.stop(&reset, cx))
                                .debug_selector(move || selector)
                                .aria_label("Reset to default")
                                .tooltip(Tooltip::text("Reset to default"))
                                .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                                    this.save_shortcuts(
                                        |state, cx| state.set_shortcut(id, None, cx),
                                        cx,
                                    );
                                    this.focus_shortcut(id, window, cx);
                                })),
                        )
                    })
                    .child({
                        let selector = shortcut.clone();
                        Button::new(shortcut.clone(), keys)
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .selected(recording)
                            .track_focus(&page.stop(&shortcut, cx))
                            .debug_selector(move || selector)
                            .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                                this.record_shortcut(command, window, cx)
                            }))
                    }),
            )
            .children(warning.map(|text| {
                div()
                    .automation_id(format!("settings-shortcut-{id}-warning"))
                    .child(Label::new(text).size(TextSize::Sm).color(Color::Warning))
            }))
    }

    /// Takes the next keystroke in this window as `id`'s keys, so none of them runs a
    /// command or types while it records.
    fn record_shortcut(
        &mut self,
        command: &'static Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = self.shortcuts_page() else {
            return;
        };
        let focus = page.stop(&format!("settings-shortcut-{}", command.id), cx);
        window.focus(&focus, cx);
        let view = cx.weak_entity();
        let handle = window.window_handle();
        let keys = cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle() != handle {
                return;
            }
            if !focus.is_focused(window) {
                let _ = view.update(cx, |this, cx| this.stop_recording(cx));
                return;
            }
            cx.stop_propagation();
            let keystroke = event.keystroke.clone();
            let _ = view.update(cx, |this, cx| this.recorded(keystroke, cx));
        });
        let Some(page) = self.shortcuts_page() else {
            return;
        };
        page.refused = None;
        page.recording = Some(Recording {
            command,
            _keys: keys,
        });
        self.refresh(cx);
    }

    /// Focus left the shortcut, by a click elsewhere or another section, so its key
    /// goes where focus went.
    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.shortcuts_page() {
            page.stop_recording();
            self.refresh(cx);
        }
    }

    /// Escape cancels. A key that edits text or types in the composer where the command
    /// runs is refused; any other is saved, or the override removed when it is the
    /// default.
    fn recorded(&mut self, keystroke: Keystroke, cx: &mut Context<Self>) {
        if !keymap::is_key(&keystroke.key) {
            return;
        }
        let Some(page) = self.shortcuts_page() else {
            return;
        };
        let Some(recording) = page.recording.take() else {
            return;
        };
        let command = recording.command;
        if keystroke.key == "escape" && !keystroke.modifiers.modified() {
            return self.refresh(cx);
        }
        if let Some(reason) = keymap::refusal(command, &keystroke) {
            page.refused = Some((command.id, reason));
            return self.refresh(cx);
        }
        let keys = keymap::spell(&keystroke);
        let default = command
            .keys
            .first()
            .and_then(|keys| keymap::parse(keys))
            .map(|default| keymap::spell(&default));
        let keys = (default.as_deref() != Some(keys.as_str())).then_some(keys);
        self.save_shortcuts(
            |state, cx| state.set_shortcut(command.id, keys.as_deref(), cx),
            cx,
        );
    }

    /// A reset button goes once its override does, so focus moves to a shortcut and
    /// the window's keys keep working.
    fn focus_shortcut(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = self.shortcuts_page() {
            let stop = page.stop(&format!("settings-shortcut-{id}"), cx);
            window.focus(&stop, cx);
        }
    }

    /// Saves through `save` and binds what was saved, or shows why it was not.
    fn save_shortcuts(
        &mut self,
        save: impl FnOnce(
            &mut AppState,
            &mut Context<AppState>,
        ) -> Result<Applied<()>, Box<dyn std::error::Error>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(page) = self.shortcuts_page() {
            page.recording = None;
        }
        // The writer commits the change later; a commit that fails shows "Not saved." on
        // the status line, as every other setting does.
        self.error = match self.state.update(cx, save) {
            Ok(_) => {
                let saved = self
                    .state
                    .read(cx)
                    .store()
                    .workspace()
                    .preferences()
                    .shortcuts()
                    .clone();
                keymap::apply(&saved, cx);
                None
            }
            Err(error) => Some(format!("Settings not saved. {error}")),
        };
        self.refresh(cx);
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "settings_shortcuts_tests.rs"]
mod tests;

#[cfg(test)]
mod sentence {
    #[test]
    fn a_clash_with_a_label_ending_in_an_ellipsis_gets_no_extra_period() {
        let labels = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            super::also_bound(&labels(&["Change project color…"])),
            "Also bound to Change project color…"
        );
        assert_eq!(
            super::also_bound(&labels(&["Hand off…", "Quit"])),
            "Also bound to Hand off… and Quit."
        );
    }
}
