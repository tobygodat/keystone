//! The user's shortcuts: saved overrides of the palette commands' keys. A bad entry is
//! skipped with a reason, so a saved file never stops the app from starting.

use crate::commands::{COMMANDS, Command};
use gpui::{
    Action, App, Global, KeyBindingContextPredicate, KeyContext, Keystroke, Modifiers, SharedString,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) struct Keymap {
    overrides: BTreeMap<&'static str, String>,
    skipped: Vec<String>,
}

impl Global for Keymap {}

static DEFAULTS: Keymap = Keymap {
    overrides: BTreeMap::new(),
    skipped: Vec::new(),
};

/// The context stacks a focused surface dispatches keys through, outermost first, as
/// `window.context_stack()` reports them.
const ROOT: &str = "Keystone";
pub(crate) const COMPOSER: &[&str] = &[ROOT, "TextInput"];
pub(crate) const TRANSCRIPT: &[&str] = &[ROOT, "Conversation"];
pub(crate) const SIDEBAR: &[&str] = &[ROOT];
pub(crate) const DIALOG: &[&str] = &[ROOT, "Dialog"];
const SURFACES: [&[&str]; 4] = [COMPOSER, TRANSCRIPT, SIDEBAR, DIALOG];
/// The stacks a focused text field dispatches through where a command can tie with the
/// field's editing keys: the composer, which stands for every field outside a dialog
/// such as search; a dialog's field, such as Rename; and the palette's query. A field
/// inside the transcript, such as a widget's, is left out: the transcript's commands
/// match there at `Conversation`, above the field, so its own keys win.
pub(crate) const TEXT_FIELDS: [&[&str]; 3] = [
    COMPOSER,
    &[ROOT, "Dialog", "TextInput"],
    &[ROOT, "CommandPalette", "TextInput"],
];

pub(crate) const REFUSED: &str =
    "That key types in the composer, so it can't run this command there.";
pub(crate) const TYPED: &str = "Text fields type that key, so it can't run this command.";

impl Keymap {
    pub(crate) fn parse(saved: &Value) -> Self {
        let mut keymap = Self {
            overrides: BTreeMap::new(),
            skipped: Vec::new(),
        };
        match saved {
            Value::Null => {}
            Value::Object(entries) => {
                for (id, keys) in entries {
                    match entry(id, keys) {
                        Ok((id, keys)) => {
                            keymap.overrides.insert(id, keys);
                        }
                        Err(reason) => keymap.skipped.push(reason),
                    }
                }
            }
            _ => keymap
                .skipped
                .push("The saved shortcuts are not a list of commands, so none were read.".into()),
        }
        keymap
    }

    /// The keystrokes that run `command`: the user's, else its default. Empty for a
    /// palette-only command the user has not bound.
    pub(crate) fn keys(&self, command: &Command) -> &str {
        self.overrides
            .get(command.id)
            .map_or(command.keys.first().copied().unwrap_or(""), String::as_str)
    }

    pub(crate) fn overridden(&self, id: &str) -> bool {
        self.overrides.contains_key(id)
    }

    pub(crate) fn has_overrides(&self) -> bool {
        !self.overrides.is_empty()
    }

    /// What else `command`'s keys run where it runs. The defaults were chosen together,
    /// and where one shadows another it is on purpose, so only a clash with a key the
    /// user bound is named.
    pub(crate) fn clashes(&self, command: &Command, bindings: &gpui::Keymap) -> Vec<String> {
        let Some(keystroke) = parse(self.keys(command)) else {
            return Vec::new();
        };
        let mut labels = conflicts(command, &keystroke, bindings);
        if !self.overridden(command.id) {
            labels.retain(|label| {
                COMMANDS
                    .iter()
                    .any(|other| other.label == label && self.overridden(other.id))
            });
        }
        labels
    }

    /// Why each saved entry that does not apply was skipped.
    pub(crate) fn skipped(&self) -> &[String] {
        &self.skipped
    }
}

fn entry(id: &str, keys: &Value) -> Result<(&'static str, String), String> {
    let command = command(id).ok_or_else(|| format!("“{id}” is not a command."))?;
    let label = command.label;
    let Value::String(keys) = keys else {
        return Err(format!("{label}: {keys} is not a shortcut."));
    };
    let keystroke = parse(keys).ok_or_else(|| format!("{label}: “{keys}” is not a shortcut."))?;
    if let Some(reason) = refusal(command, &keystroke) {
        return Err(format!("{label}: {reason}"));
    }
    Ok((command.id, spell(&keystroke)))
}

/// The keymap in effect, or the defaults before one is applied.
pub(crate) fn current(cx: &App) -> &Keymap {
    cx.try_global::<Keymap>().unwrap_or(&DEFAULTS)
}

pub(crate) fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.id == id)
}

/// One keystroke, the only kind a command takes. A modifier alone is not one, and
/// neither is a key no keyboard sends, such as the empty one in `secondary-`.
pub(crate) fn parse(keys: &str) -> Option<Keystroke> {
    if keys.contains(char::is_whitespace) {
        return None;
    }
    let keystroke = Keystroke::parse(keys).ok()?;
    is_key(&keystroke.key).then_some(keystroke)
}

/// A key GPUI reports a press of: one printable character, or a named key.
pub(crate) fn is_key(key: &str) -> bool {
    const NAMED: [&str; 15] = [
        "space",
        "tab",
        "enter",
        "escape",
        "backspace",
        "delete",
        "insert",
        "left",
        "right",
        "up",
        "down",
        "home",
        "end",
        "pageup",
        "pagedown",
    ];
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(only), None) => !only.is_control() && !only.is_whitespace() && !only.is_uppercase(),
        _ => NAMED.contains(&key) || function_key(key),
    }
}

/// F1 to F35.
fn function_key(key: &str) -> bool {
    key.strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=35).contains(&n))
}

/// A key the composer would take as text: no Control or Command, and not F1 to F35.
pub(crate) fn types_text(keystroke: &Keystroke) -> bool {
    !keystroke.modifiers.control && !keystroke.modifiers.platform && !function_key(&keystroke.key)
}

/// A key any text field types as a character: a letter, digit, punctuation mark or
/// Space, alone or with Shift or Alt, which types one too on macOS.
fn types_a_character(keystroke: &Keystroke) -> bool {
    types_text(keystroke) && (keystroke.key == "space" || keystroke.key.chars().count() == 1)
}

/// Why `keystroke` can't be `command`'s, if it can't: it edits text in a field where
/// `command` runs as deep as the field's own keys, such as Select all or Paste; the
/// composer types it; or it types a character.
///
/// A field binds its editing keys under `TextInput`, so they beat a command bound
/// higher up, such as a transcript command while a widget's field has focus. Only a
/// command that matches as deep as the field ties with them, and a tie goes by the
/// order the bindings were made, so those commands never get one. A typed character
/// reaches a field only after every binding has had the key, so no depth saves it:
/// no command gets one, because every command runs above some field, the transcript's
/// above a widget's.
pub(crate) fn refusal(command: &Command, keystroke: &Keystroke) -> Option<String> {
    let predicate = predicate(command);
    let in_a_field = TEXT_FIELDS
        .iter()
        .any(|field| holds(predicate.as_ref(), &stack(field)));
    if in_a_field && let Some(what) = keystone_ui::text_input::editing_key(keystroke) {
        return Some(format!(
            "Text fields use that key to {what}, so it can't run this command."
        ));
    }
    if types_text(keystroke) && holds(predicate.as_ref(), &stack(COMPOSER)) {
        return Some(REFUSED.to_owned());
    }
    types_a_character(keystroke).then(|| TYPED.to_owned())
}

fn predicate(command: &Command) -> Option<KeyBindingContextPredicate> {
    command
        .context
        .map(|context| KeyBindingContextPredicate::parse(context).unwrap())
}

fn stack(surface: &[&str]) -> Vec<KeyContext> {
    surface
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect()
}

/// Whether a binding with `predicate` is live on `stack`, as GPUI decides it: some
/// prefix of the stack satisfies it.
fn holds(predicate: Option<&KeyBindingContextPredicate>, stack: &[KeyContext]) -> bool {
    predicate.is_none_or(|predicate| predicate.depth_of(stack).is_some())
}

/// What else `keystroke` runs on a surface where it would also run `command`: the
/// labels of every other binding, sorted.
pub(crate) fn conflicts(
    command: &Command,
    keystroke: &Keystroke,
    bindings: &gpui::Keymap,
) -> Vec<String> {
    let own = (command.action)().name();
    // Where the command runs: its own context, and that of any panel that binds the
    // same action with the user's keys, such as View diff in the Changes list.
    let first = predicate(command);
    let others: Vec<_> = bindings
        .bindings()
        .filter(|binding| binding.action().name() == own)
        .map(|binding| binding.predicate().as_deref().cloned())
        .filter(|other| *other != first)
        .collect();
    let ours: Vec<_> = std::iter::once(first).chain(others).collect();
    let mut surfaces: Vec<Vec<KeyContext>> =
        SURFACES.iter().map(|surface| stack(surface)).collect();
    surfaces.extend(ours.iter().flatten().flat_map(witnesses));
    let mut labels: Vec<String> = bindings
        .bindings()
        .filter(|binding| binding.action().name() != own)
        .filter(|binding| match binding.keystrokes() {
            [only] => {
                only.inner().modifiers == keystroke.modifiers && only.inner().key == keystroke.key
            }
            _ => false,
        })
        .filter(|binding| {
            let theirs = binding.predicate();
            let own_surfaces = theirs.as_deref().map(witnesses).unwrap_or_default();
            surfaces.iter().chain(&own_surfaces).any(|stack| {
                holds(theirs.as_deref(), stack)
                    && ours.iter().any(|ours| holds(ours.as_ref(), stack))
            })
        })
        .map(|binding| label(binding.action()))
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

/// Stacks under the window's root that name what `predicate` asks for, one per way it
/// can hold, so a binding of a panel this module doesn't list, such as the file tree,
/// is still checked where it runs.
fn witnesses(predicate: &KeyBindingContextPredicate) -> Vec<Vec<KeyContext>> {
    fn names(predicate: &KeyBindingContextPredicate) -> Vec<Vec<SharedString>> {
        use KeyBindingContextPredicate::*;
        match predicate {
            Identifier(name) => vec![vec![name.clone()]],
            And(left, right) | Descendant(left, right) => {
                let right = names(right);
                names(left)
                    .into_iter()
                    .flat_map(|left| {
                        right
                            .iter()
                            .map(move |right| [left.clone(), right.clone()].concat())
                    })
                    .collect()
            }
            Or(left, right) => [names(left), names(right)].concat(),
            Equal(..) | NotEqual(..) | Not(_) => vec![Vec::new()],
        }
    }
    names(predicate)
        .into_iter()
        .map(|path| {
            std::iter::once(SharedString::from(ROOT))
                .chain(path.into_iter().filter(|name| name.as_ref() != ROOT))
                .map(|name| {
                    let mut context = KeyContext::default();
                    context.add(name);
                    context
                })
                .collect()
        })
        .collect()
}

/// A command's label, else the action's name in words: `OpenPalette` is "Open palette".
fn label(action: &dyn Action) -> String {
    let name = action.name();
    if let Some(command) = COMMANDS
        .iter()
        .find(|command| (command.action)().name() == name)
    {
        return command.label.to_owned();
    }
    let short = name.rsplit_once("::").map_or(name, |(_, short)| short);
    let mut words = String::new();
    for (at, character) in short.chars().enumerate() {
        if at > 0 && character.is_uppercase() {
            words.push(' ');
            words.extend(character.to_lowercase());
        } else {
            words.push(character);
        }
    }
    words
}

/// `keystroke` as it is saved: `secondary-` for Command on macOS and Control elsewhere,
/// so a saved shortcut means the same on both.
pub(crate) fn spell(keystroke: &Keystroke) -> String {
    let secondary = Modifiers::secondary_key();
    if !keystroke.modifiers.secondary() {
        return keystroke.unparse();
    }
    let mut rest = keystroke.clone();
    rest.modifiers.platform &= !secondary.platform;
    rest.modifiers.control &= !secondary.control;
    format!("secondary-{}", rest.unparse())
}

/// Reads `saved` into the keymap, binds every key from it, and redraws each window so
/// every hint shows it.
pub(crate) fn apply(saved: &Value, cx: &mut App) {
    cx.set_global(Keymap::parse(saved));
    cx.clear_key_bindings();
    crate::bind_keys(cx);
    cx.defer(|cx| {
        for view in crate::open_windows(cx) {
            view.update(cx, |this, cx| this.refresh(cx));
        }
    });
}

#[cfg(test)]
#[path = "keymap_tests.rs"]
mod tests;
