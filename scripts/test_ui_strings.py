#!/usr/bin/env python3
"""Interface text inventory tests: python3 scripts/test_ui_strings.py."""
import contextlib
import importlib.util
import io
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('ui_strings', Path(__file__).with_name('ui-strings.py'))
ui = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ui)


class StringsTest(unittest.TestCase):
    def test_labels_tooltips_and_errors_are_kept(self):
        source = '''
fn render() {
    Label::new("Local workspace");
    IconButton::new("settings", IconName::Settings).tooltip(Tooltip::text("Settings"));
    Label::new("preview");
    self.error = Some(format!("Layout not changed. {problem}"));
    TextInput::new("Write a draft\\u{2026}", InputMode::Multiline, cx);
}
'''
        self.assertEqual(ui.strings(source), [
            'Local workspace', 'Settings', 'preview', 'Layout not changed. {problem}', 'Write a draft…'])

    def test_selectors_keys_contexts_and_logs_are_dropped(self):
        source = '''
const APP: &str = "!Dialog && !CommandPalette";
const CONTEXT: &str = "FileTree";
const KEY_CONTEXT: &str = "Dialog";
fn render() {
    div().debug_selector(|| "new-session".into()).automation_id("Composer Field");
    KeyBinding::new("ctrl-shift-c", CopyPath, Some(CONTEXT));
    tracing::warn!("Could not read the session");
    let home = std::env::var_os("KEYSTONE_STATE_PATH");
    let sql = "SELECT payload FROM accounts";
    let svg = "<path d='M5 12h14'/>";
    let key = "mcpServers";
    let flag = "-C";
    let method = "item/agentMessage/delta";
    let key = format!("{USAGE_PREFIX}{account}");
    let error = format!("{PROVIDER} has failed");
}
'''
        self.assertEqual(ui.strings(source), ['{PROVIDER} has failed'])

    def test_test_code_is_skipped_but_following_code_is_not(self):
        source = '''
#[cfg(test)]
mod tests {
    #[test]
    fn shows() { assert_eq!(label(), "Hidden in tests {"); Label::new("Test only"); }
}
#[cfg(test)]
fn helper() { Label::new("Helper only"); }
#[cfg(not(test))]
fn shipped() { Label::new("Shipped text"); }
fn after() { Label::new("After the tests"); }
'''
        self.assertEqual(ui.strings(source), ['Shipped text', 'After the tests'])

    def test_quotes_inside_comments_chars_and_raw_strings(self):
        source = '''
// "Not a string"
/* "Nor this" /* nested "one" */ */
fn render<'a>(x: &'a str) {
    let quote = '"';
    let escaped = '\\'';
    Label::new(r#"Raw "quoted" text"#);
    let bytes = b"Byte Text";
    Label::new("Line one \\
                continues");
}
'''
        self.assertEqual(ui.strings(source), ['Raw "quoted" text', 'Line one continues'])

    def test_labels_passed_as_constructor_arguments_are_kept_whatever_their_case(self):
        source = '''
fn render() {
    Button::new("save", "save");
    MenuItem::new("api", "API");
    ConfirmDialog::new("rename-dialog", focus, "Rename", "shown");
}
'''
        self.assertEqual(ui.strings(source), ['save', 'API', 'Rename', 'shown'])

    def test_whole_diagnostic_calls_and_key_contexts_are_dropped(self):
        source = '''
fn render() {
    debug_assert!(panes.len() > 0, "a split needs at least one pane");
    div().key_context("Tree");
    cx.bind_keys([KeyBinding::new("ctrl-c", Copy, Some("MarkdownText selection"))]);
    let message = "Shown later";
}
'''
        self.assertEqual(ui.strings(source), ['Shown later'])

    def test_a_test_only_attribute_hides_only_its_own_item(self):
        source = '''
struct S {
    #[cfg(test)]
    fixture: bool,
}
#[cfg(test)]
const FIXTURE: &str = "Test only text";
#[cfg(not (test))]
fn shipped() { Label::new("Shipped text"); }
#[cfg(not(any(test, feature = "x")))]
fn also_shipped() { Label::new("Also shipped"); }
#[cfg(any(test, target_os = "linux"))]
fn linux() { Label::new("On Linux"); }
impl S { fn render() { Label::new("Visible label"); } }
'''
        self.assertEqual(ui.strings(source), ['Shipped text', 'Also shipped', 'On Linux', 'Visible label'])

    def test_unusual_literals_lex_without_failing(self):
        source = '''
#[doc = "]"]
fn render() {
    let raw = br"Raw Bytes";
    Label::new("Smile \\u{1F_600}");
}
'''
        self.assertEqual(ui.strings(source), ['Smile \U0001F600'])

    def test_lowercase_labels_behind_defaults_helpers_and_keycaps_are_kept(self):
        source = '''
fn render() {
    Label::new(language.unwrap_or_else(|| "text".into()));
    recovered.push(plural(count, "message", "messages"));
    IconButton::new("rename").tooltip(Tooltip::with_key("Rename", "F2"));
    Kbd::new("esc");
    PaletteItem::new("open", "Open").kbd("ctrl");
    Switch::new("wrap").label("wrap lines");
    const MARKER: usize = "Bytes Left Out".len();
}
'''
        self.assertEqual(ui.strings(source), [
            'text', 'message', 'messages', 'Rename', 'F2', 'esc', 'Open', 'ctrl', 'wrap lines'])


class InventoryTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root)
        patch = mock.patch.multiple(ui, ROOT=self.root, INVENTORY=self.root / 'ui-strings.txt', SOURCES=['src'],
                                    SKIP_FILES={'src/skipped.rs'}, SKIP_DIRS=('src/bin/',))
        patch.start()
        self.addCleanup(patch.stop)
        for name, text in {'b.rs': 'Label::new("Beta\\tline");', 'a.rs': 'Label::new("Alpha");',
                           'skipped.rs': 'Label::new("Skipped file");', 'bin/tool.rs': 'Label::new("Bin");',
                           'a_tests.rs': 'Label::new("Test file");'}.items():
            (self.root / 'src' / name).parent.mkdir(parents=True, exist_ok=True)
            (self.root / 'src' / name).write_text(text)

    def test_inventory_is_sorted_escaped_and_leaves_out_skipped_files(self):
        self.assertEqual(ui.inventory(), ui.inventory())
        self.assertEqual(ui.inventory().splitlines()[len(ui.HEADER):],
                         ['', 'src/a.rs', '\tAlpha', '', 'src/b.rs', '\tBeta\\tline'])

    def test_check_fails_until_the_inventory_is_regenerated(self):
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(ui.main(['ui-strings.py', '--check']), 1)
            self.assertEqual(ui.main(['ui-strings.py']), 0)
            self.assertEqual(ui.main(['ui-strings.py', '--check']), 0)
            (self.root / 'src/a.rs').write_text('Label::new("Changed");')
            self.assertEqual(ui.main(['ui-strings.py', '--check']), 1)


if __name__ == '__main__':
    unittest.main()
