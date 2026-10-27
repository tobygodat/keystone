//! Attachments through the window, with the scripted providers recording the images
//! each message carried in a receipt file.

use super::*;
use crate::{AppState, Row};
use gpui::{ClipboardItem, ImageFormat, TestAppContext, VisualTestContext};
use keystone_agent::ItemKind;
use keystone_core::{Provider, Store, WorkingDirectory};
use std::os::unix::fs::PermissionsExt;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nkeystone attachment fixture";
const JPEG: &[u8] = b"\xff\xd8\xff\xe0keystone jpeg fixture";

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A scripted provider with short delays, writing its image receipt in `dir`.
fn wrapper(dir: &Path, name: &str) -> PathBuf {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("keystone-fake-{name}"));
    let environment = serde_json::json!({
        "KEYSTONE_FAKE_CODEX_DELAY_MS": "2",
        "KEYSTONE_FAKE_CLAUDE_DELAY_MS": "2",
        "KEYSTONE_FAKE_IMAGE_RECEIPT": dir.join("receipt.jsonl"),
    });
    let script = dir.join(format!("{name}-attach-wrapper"));
    std::fs::write(
        &script,
        format!(
            "#!/usr/bin/python3\nimport os, sys\nos.environ.update({environment})\nbinary = {}\nos.execv(binary, [binary, *sys.argv[1:]])\n",
            serde_json::to_string(&binary).unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// One selected `provider` session on an added account with an empty home, working in
/// a project folder, its composer focused.
fn open(
    provider: Provider,
    cx: &mut TestAppContext,
) -> (tempfile::TempDir, Entity<Keystone>, &mut VisualTestContext) {
    // Canonical, like the folders Keystone records: macOS's temporary folder sits
    // behind a symlink.
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let root = dir.path();
    let (home, folder) = (root.join("home"), root.join("project"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(root.join("data")).unwrap();
    let mut store = Store::open(root.join("data/workspace.json")).unwrap();
    let account = store
        .add_account(provider, "Work", home, &[])
        .unwrap()
        .value;
    let id = store
        .create(provider, &WorkingDirectory::new(&folder).unwrap(), 1)
        .unwrap()
        .value;
    store.set_account(id, Some(account)).unwrap();
    store.select(id).unwrap();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
    });
    cx.executor().allow_parking();
    let programs = (wrapper(root, "codex"), wrapper(root, "claude"));
    let (view, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = AppState::new(store, cx);
            state.test_programs = Some(programs);
            state
        });
        Keystone::new(state, false, window, cx)
    });
    view.update_in(cx, |this, window, cx| {
        this.composer.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();
    (dir, view, cx)
}

/// Runs the app with real time passing until `done` holds.
fn wait(
    cx: &mut VisualTestContext,
    view: &Entity<Keystone>,
    done: impl Fn(&Keystone, &App) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |view, cx| done(view, cx)) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn attached(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Vec<Attachment> {
    view.read_with(cx, |view, cx| view.draft_attachments(cx))
}

fn saved(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Vec<Attachment> {
    view.read_with(cx, |view, cx| {
        Store::open(view.state.read(cx).store().path())
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft_attachments()
            .to_vec()
    })
}

fn draft(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| view.composer.read(cx).value().to_owned())
}

fn notice(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Option<String> {
    view.read_with(cx, |view, _| {
        view.attach.notice.as_ref().map(|(_, n)| n.clone())
    })
}

fn receipt(dir: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("receipt.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn reply(view: &Keystone, cx: &App) -> Option<String> {
    let conversation = view.state.read(cx).conversations().get(&1)?;
    if conversation.running {
        return None;
    }
    conversation.rows.iter().rev().find_map(|row| match row {
        Row::Item(item) if item.kind == ItemKind::AgentMessage => Some(item.text.clone()),
        _ => None,
    })
}

/// Ctrl+V with an image on the clipboard attaches it as a chip that is saved with the
/// draft, and text on the clipboard still pastes as text. Send gives Claude the
/// image's bytes in an image block, logs a reference, shows it under the message and
/// clears the draft's images.
#[gpui::test]
fn a_pasted_image_is_saved_with_the_draft_and_reaches_claude_as_an_image_block(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = open(Provider::Claude, cx);
    cx.update(|_, cx| {
        cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::from_bytes(
            ImageFormat::Png,
            PNG.to_vec(),
        )))
    });
    cx.simulate_keystrokes("secondary-v");
    wait(cx, &view, |view, cx| view.draft_attachments(cx).len() == 1);
    let image = attached(&view, cx).remove(0);
    assert_eq!(image.name, "Image.png");
    assert_eq!(image.media_type, "image/png");
    assert_eq!(image.sha256, sha256(PNG));
    let copy = dir
        .path()
        .join(format!("data/attachments/{}.png", sha256(PNG)));
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        PNG,
        "copied into the data folder"
    );
    assert_eq!(
        saved(&view, cx),
        std::slice::from_ref(&image),
        "saved with the draft"
    );
    assert_eq!(draft(&view, cx), "", "an image is not pasted as text");
    assert!(cx.debug_bounds("attachment-0").is_some());
    assert!(cx.debug_bounds("attachment-remove-0").is_some());

    cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("Describe it".into())));
    cx.simulate_keystrokes("secondary-v");
    assert_eq!(draft(&view, cx), "Describe it");

    cx.simulate_keystrokes("secondary-enter");
    wait(cx, &view, |view, cx| reply(view, cx).is_some());
    let reply = view.read_with(cx, |view, cx| reply(view, cx).unwrap());
    assert!(
        reply.ends_with(&format!("**Images:** image/png {}", &sha256(PNG)[..8])),
        "{reply}"
    );
    let receipt = receipt(dir.path());
    assert_eq!(receipt.len(), 1);
    assert_eq!(receipt[0]["source"], "base64");
    assert_eq!(receipt[0]["media_type"], "image/png");
    assert_eq!(receipt[0]["sha256"], sha256(PNG));

    view.read_with(cx, |view, cx| {
        let rows = &view.state.read(cx).conversations()[&1].rows;
        let at = rows
            .iter()
            .position(|row| matches!(row, Row::Item(item) if item.text == "Describe it"))
            .unwrap();
        assert_eq!(rows[at + 1], Row::Attachments(vec![image.clone()]));
    });
    assert!(cx.debug_bounds("sent-attachments-1").is_some());
    assert_eq!(
        cx.update(|_, cx| cx.global::<AttachmentsDir>().0.clone()),
        dir.path().join("data/attachments"),
        "sent rows draw their thumbnails from the copies"
    );
    assert!(attached(&view, cx).is_empty(), "the draft's images went");
    assert!(saved(&view, cx).is_empty());
    assert!(cx.debug_bounds("attachment-0").is_none());
}

/// The attach button's picker attaches an image as a chip and mentions any other file
/// by its path. A chip's remove button takes it out of the saved draft. Codex is sent
/// each image as a `localImage` whose path is the copy in the data folder.
#[gpui::test]
fn picked_files_attach_images_and_mention_others_and_codex_reads_the_copy(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(Provider::Codex, cx);
    let (shot, notes) = (
        dir.path().join("shot.jpg"),
        dir.path().join("project/notes.txt"),
    );
    std::fs::write(&shot, JPEG).unwrap();
    std::fs::write(&notes, "notes").unwrap();
    std::fs::write(dir.path().join("other.png"), PNG).unwrap();
    click(cx, "attach");
    click(cx, "attach-files");
    assert!(cx.did_prompt_for_paths());
    assert!(cx.debug_bounds("attach-files").is_none(), "the menu closed");
    let picked = vec![shot.clone(), notes, dir.path().join("other.png")];
    cx.simulate_path_prompt_response(|options| {
        assert!(options.files && options.multiple && !options.directories);
        Some(picked)
    });
    wait(cx, &view, |view, cx| view.draft_attachments(cx).len() == 2);
    assert_eq!(draft(&view, cx), "@notes.txt ");
    assert_eq!(
        attached(&view, cx)
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["shot.jpg", "other.png"]
    );

    click(cx, "attachment-remove-1");
    assert_eq!(attached(&view, cx).len(), 1);
    assert_eq!(saved(&view, cx), attached(&view, cx));
    // The copy outlives the original.
    std::fs::remove_file(&shot).unwrap();

    cx.simulate_input("what is in it?");
    cx.simulate_keystrokes("secondary-enter");
    wait(cx, &view, |view, cx| reply(view, cx).is_some());
    let receipt = receipt(dir.path());
    assert_eq!(receipt.len(), 1);
    let copy = dir
        .path()
        .join(format!("data/attachments/{}.jpg", sha256(JPEG)));
    assert_eq!(receipt[0]["path"], copy.to_str().unwrap());
    assert_eq!(receipt[0]["exists"], true);
    assert_eq!(receipt[0]["sha256"], sha256(JPEG));
}

/// The attach menu's Capture screenshot runs the capture tool and attaches what it
/// wrote like a pasted image, which Claude receives. A failed capture says so under
/// the draft and a cancelled one leaves the draft as it was.
#[gpui::test]
fn a_captured_screenshot_is_attached_and_a_cancelled_one_changes_nothing(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(Provider::Claude, cx);
    let (fixture, runs) = (dir.path().join("shot.png"), dir.path().join("runs"));
    std::fs::write(&fixture, PNG).unwrap();
    let script = dir.path().join("capture");
    let capture_with = |body: &str, view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        std::fs::write(
            &script,
            format!("echo run >> '{}'\n{body}\n", runs.display()),
        )
        .unwrap();
        let command = vec!["/bin/sh".into(), script.to_str().unwrap().to_owned()];
        view.update(cx, |view, _| view.attach.screenshot_command = Some(command));
    };
    let idle =
        |view: &Keystone, _: &App| !view.attach.capturing && view.attach.importing.is_empty();
    let capture_from_menu = |cx: &mut VisualTestContext| {
        click(cx, "attach");
        click(cx, "capture-screenshot");
        assert!(
            cx.debug_bounds("capture-screenshot").is_none(),
            "the menu closed"
        );
    };

    cx.simulate_input("Look");
    click(cx, "attach");
    assert!(cx.debug_bounds("capture-screenshot").is_some());
    cx.simulate_keystrokes("escape");
    assert!(
        cx.debug_bounds("capture-screenshot").is_none(),
        "Escape closes the menu"
    );
    assert_eq!(draft(&view, cx), "Look");

    capture_with("echo 'no screen' >&2; exit 1", &view, cx);
    cx.simulate_keystrokes("secondary-alt-s");
    wait(cx, &view, idle);
    assert!(attached(&view, cx).is_empty());
    assert_eq!(
        notice(&view, cx).as_deref(),
        Some("Screenshot failed. no screen")
    );
    assert!(cx.debug_bounds("attachment-notice").is_some());

    capture_with(&format!("cp '{}' \"$1\"", fixture.display()), &view, cx);
    capture_from_menu(cx);
    wait(cx, &view, |view, cx| view.draft_attachments(cx).len() == 1);
    let image = attached(&view, cx).remove(0);
    assert_eq!(image.name, "Screenshot.png");
    assert_eq!(image.sha256, sha256(PNG));
    assert_eq!(saved(&view, cx), std::slice::from_ref(&image));
    assert_eq!(notice(&view, cx), None);
    assert!(cx.debug_bounds("attachment-0").is_some());

    capture_with("exit 0", &view, cx);
    capture_from_menu(cx);
    wait(cx, &view, idle);
    assert_eq!(attached(&view, cx), [image.clone()], "cancelled");
    assert_eq!(notice(&view, cx), None);
    assert_eq!(draft(&view, cx), "Look");

    assert_eq!(
        std::fs::read_to_string(&runs).unwrap(),
        "run\nrun\nrun\n",
        "one run per capture"
    );
    view.update(cx, |view, _| view.attach.capturing = true);
    cx.simulate_keystrokes("secondary-alt-s");
    view.update(cx, |view, _| view.attach.capturing = false);
    assert_eq!(
        std::fs::read_to_string(&runs).unwrap(),
        "run\nrun\nrun\n",
        "no capture starts while one is being picked"
    );

    cx.simulate_keystrokes("secondary-enter");
    wait(cx, &view, |view, cx| reply(view, cx).is_some());
    let receipt = receipt(dir.path());
    assert_eq!(receipt.len(), 1);
    assert_eq!(receipt[0]["media_type"], "image/png");
    assert_eq!(receipt[0]["sha256"], sha256(PNG));
}

/// Where no picker is available, a typed path attaches the file: relative to the
/// session's folder, or from `~/`. An image over the limit is refused with a reason
/// and leaves the draft as it was.
#[gpui::test]
fn a_typed_path_attaches_and_an_oversized_image_is_refused_with_a_reason(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(Provider::Claude, cx);
    std::fs::create_dir_all(dir.path().join("project/img")).unwrap();
    std::fs::write(dir.path().join("project/img/a.png"), PNG).unwrap();
    std::fs::write(dir.path().join("big.png"), [PNG, &[0; 64]].concat()).unwrap();
    view.update_in(cx, |view, window, cx| {
        view.state.update(cx, |state, _| {
            state.attachments = keystone_core::attachment::Attachments::new(
                state.attachments.dir().to_owned(),
                PNG.len() as u64 + 8,
            )
        });
        view.open_attach_path(window, cx)
    });
    cx.run_until_parked();
    assert_eq!(notice(&view, cx), None);
    assert!(cx.debug_bounds("attach-path").is_some());
    cx.simulate_input("img/a.png");
    cx.simulate_keystrokes("enter");
    wait(cx, &view, |view, cx| view.draft_attachments(cx).len() == 1);
    assert_eq!(notice(&view, cx), None);
    assert!(cx.debug_bounds("attach-path").is_none());
    assert_eq!(draft(&view, cx), "", "Enter did not reach the draft");

    view.update_in(cx, |view, window, cx| view.open_attach_path(window, cx));
    cx.run_until_parked();
    cx.simulate_input(dir.path().join("big.png").to_str().unwrap());
    cx.simulate_keystrokes("enter");
    wait(cx, &view, |view, _| view.attach.notice.is_some());
    let refused = notice(&view, cx).unwrap();
    assert!(
        refused.starts_with("big.png is ") && refused.contains("over the"),
        "{refused}"
    );
    assert!(cx.debug_bounds("attachment-notice").is_some());
    assert_eq!(attached(&view, cx).len(), 1, "nothing added");

    view.update_in(cx, |view, window, cx| view.open_attach_path(window, cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("escape");
    assert!(cx.debug_bounds("attach-path").is_none());
}

/// A file dropped on the composer attaches like a picked one.
#[gpui::test]
fn a_file_dropped_on_the_composer_is_attached(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(Provider::Claude, cx);
    let shot = dir.path().join("dropped.png");
    std::fs::write(&shot, PNG).unwrap();
    let position = cx.debug_bounds("composer-field").unwrap().center();
    cx.simulate_event(gpui::FileDropEvent::Entered {
        position,
        paths: gpui::ExternalPaths(vec![shot].into()),
    });
    cx.simulate_event(gpui::FileDropEvent::Submit { position });
    wait(cx, &view, |view, cx| view.draft_attachments(cx).len() == 1);
    assert_eq!(attached(&view, cx)[0].name, "dropped.png");
}

/// An image whose copy is gone when the message is sent keeps the draft and says
/// which one to attach again; nothing reaches the provider.
#[gpui::test]
fn a_missing_copy_keeps_the_draft_with_a_reason(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(Provider::Claude, cx);
    std::fs::write(dir.path().join("a.png"), PNG).unwrap();
    view.update_in(cx, |view, window, cx| {
        view.attach_paths(vec![dir.path().join("a.png")], window, cx)
    });
    wait(cx, &view, |view, cx| view.draft_attachments(cx).len() == 1);
    std::fs::remove_dir_all(dir.path().join("data/attachments")).unwrap();
    cx.simulate_input("still there?");
    cx.simulate_keystrokes("secondary-enter");
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        let error = view.state.read(cx).conversations()[&1]
            .error
            .clone()
            .unwrap();
        assert_eq!(
            error,
            "Not sent: a.png is no longer in Keystone's data folder. Remove it and attach it again."
        );
        assert_eq!(view.composer.read(cx).value(), "still there?");
        assert_eq!(view.draft_attachments(cx).len(), 1);
    });
    assert!(receipt(dir.path()).is_empty());
}

/// Send waits while an image is still being copied, so the message never goes out
/// without it; once the copy lands, the image goes with the message.
#[gpui::test]
fn send_waits_for_an_image_still_being_attached(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(Provider::Claude, cx);
    std::fs::write(dir.path().join("a.png"), PNG).unwrap();
    cx.simulate_input("look");
    view.update_in(cx, |view, window, cx| {
        view.attach_paths(vec![dir.path().join("a.png")], window, cx);
        let draft = view.composer_draft(cx).unwrap();
        view.send_message(draft, window, cx);
        assert_eq!(view.composer.read(cx).value(), "look", "not sent yet");
    });
    wait(cx, &view, |view, cx| view.draft_attachments(cx).len() == 1);
    assert!(receipt(dir.path()).is_empty());
    cx.simulate_keystrokes("secondary-enter");
    wait(cx, &view, |view, cx| reply(view, cx).is_some());
    assert_eq!(receipt(dir.path()).len(), 1);
    assert!(attached(&view, cx).is_empty());
}

/// A mention goes in at the caret as a word of its own, even where the draft goes on
/// after it.
#[gpui::test]
fn a_mention_is_its_own_word_at_the_caret(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(Provider::Claude, cx);
    std::fs::write(dir.path().join("project/notes.txt"), "notes").unwrap();
    cx.simulate_input("hello world ");
    cx.simulate_keystrokes("home right right right");
    view.update_in(cx, |view, window, cx| {
        view.attach_paths(vec![dir.path().join("project/notes.txt")], window, cx)
    });
    wait(cx, &view, |view, cx| {
        view.composer.read(cx).value().contains('@')
    });
    assert_eq!(draft(&view, cx), "hel @notes.txt lo world ");
    let tokens = view.read_with(cx, |view, cx| view.composer.read(cx).tokens().to_vec());
    assert_eq!(tokens.len(), 1, "one token, as the @ picker writes it");
    assert_eq!(tokens[0], 4..14);
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} not rendered"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}
