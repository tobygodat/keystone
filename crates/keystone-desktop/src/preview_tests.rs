//! Previews through the window. The scripted Claude replies with a local image, one
//! outside the session folder and a remote one, after a Write of a video.

use super::*;
use crate::{AppState, Keystone, Row};
use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
use keystone_agent::ItemKind;
use keystone_core::{Provider, Store, WorkingDirectory};
use std::os::unix::fs::PermissionsExt;

const REPLY: &str =
    "Here it is.\n\n![](./shot.png)\n\n![](../outside.png)\n\n![](http://127.0.0.1:9/remote.png)";

fn claude(dir: &Path) -> PathBuf {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("keystone-fake-claude");
    let environment = serde_json::json!({
        "KEYSTONE_FAKE_CLAUDE_DELAY_MS": "2",
        "KEYSTONE_FAKE_CLAUDE_REPLY": REPLY,
        "KEYSTONE_FAKE_CLAUDE_WRITE": "out/demo.mp4",
    });
    let script = dir.join("claude-preview-wrapper");
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

fn png(path: &Path, width: u32, height: u32) {
    image::RgbaImage::from_pixel(width, height, image::Rgba([200, 80, 40, 255]))
        .save(path)
        .unwrap();
}

/// A Claude session working in `project/`, which holds `shot.png` (4000 by 2000) and
/// `out/demo.mp4`, with `outside.png` beside the folder.
fn open(cx: &mut TestAppContext) -> (tempfile::TempDir, Entity<Keystone>, &mut VisualTestContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (home, folder) = (root.join("home"), root.join("project"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(folder.join("out")).unwrap();
    std::fs::create_dir_all(root.join("data")).unwrap();
    png(&folder.join("shot.png"), 4000, 2000);
    png(&root.join("outside.png"), 8, 8);
    std::fs::write(folder.join("out/demo.mp4"), b"not really a video").unwrap();
    let mut store = Store::open(root.join("data/workspace.json")).unwrap();
    let account = store
        .add_account(Provider::Claude, "Work", home, &[])
        .unwrap()
        .value;
    let id = store
        .create(
            Provider::Claude,
            &WorkingDirectory::new(&folder).unwrap(),
            1,
        )
        .unwrap()
        .value;
    store.set_account(id, Some(account)).unwrap();
    store.select(id).unwrap();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
    });
    cx.executor().allow_parking();
    let programs = (root.join("no-codex"), claude(root));
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
fn wait(cx: &mut VisualTestContext, mut done: impl FnMut(&mut VisualTestContext) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn row(view: &Entity<Keystone>, cx: &mut VisualTestContext, kind: fn(&ItemKind) -> bool) -> usize {
    view.read_with(cx, |view, cx| {
        view.state.read(cx).conversations()[&1]
            .rows
            .iter()
            .position(|row| matches!(row, Row::Item(item) if kind(&item.kind)))
            .unwrap()
    })
}

fn selector(name: String) -> &'static str {
    name.leak()
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} not rendered"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

/// A reply's image inside the session folder is a thumbnail, downscaled to fit 360 by
/// 200; one outside the folder stays its path, and a remote one a link. A Write of a
/// video is a card with Open in player. The thumbnail opens the overlay at fit; 1:1
/// shows the image's own size, Copy path copies its path, and Escape closes it.
#[gpui::test]
fn reply_images_are_thumbnails_that_open_a_preview(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx);
    cx.simulate_input("show me");
    cx.simulate_keystrokes("secondary-enter");
    wait(cx, |cx| {
        view.read_with(cx, |view, cx| {
            let conversation = &view.state.read(cx).conversations()[&1];
            !conversation.running
                && conversation
                    .rows
                    .iter()
                    .any(|row| matches!(row, Row::Item(item) if item.text == REPLY))
        })
    });
    let reply = row(&view, cx, |kind| *kind == ItemKind::AgentMessage);
    let change = row(&view, cx, |kind| {
        matches!(kind, ItemKind::FileChange { .. })
    });
    let thumbnail = selector(format!("thumbnail-{reply}-0"));
    wait(cx, |cx| cx.debug_bounds(thumbnail).is_some());
    let bounds = cx.debug_bounds(thumbnail).unwrap();
    assert_eq!(
        (f32::from(bounds.size.width), f32::from(bounds.size.height)),
        (362., 202.),
        "a 360 by 200 tile, inside a one-pixel border, for 4000 by 2000 at 360 by 180"
    );
    let document = keystone_ui::markdown::Document::parse(REPLY);
    assert!(matches!(
        &document.blocks[3].block,
        keystone_ui::markdown::Block::Paragraph(inline)
            if inline.links[0].1 == "http://127.0.0.1:9/remote.png"
    ));
    assert!(
        cx.debug_bounds(selector(format!("thumbnail-{reply}-1")))
            .is_none()
    );
    wait(cx, |cx| {
        cx.debug_bounds(selector(format!("video-{change}-0-open")))
            .is_some()
    });

    click(cx, thumbnail);
    assert!(cx.debug_bounds("preview-overlay").is_some());
    wait(cx, |cx| cx.debug_bounds("preview-image").is_some());
    let fit = cx.debug_bounds("preview-image").unwrap().size;
    click(cx, "preview-actual");
    let actual = cx.debug_bounds("preview-image").unwrap().size;
    let scale = cx.update(|window, _| window.scale_factor());
    assert_eq!(f32::from(actual.width), 4000. / scale);
    assert!(fit.width < actual.width, "{fit:?} {actual:?}");

    click(cx, "preview-copy");
    let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
    let shot = dir.path().join("project/shot.png").canonicalize().unwrap();
    assert_eq!(copied.as_deref(), shot.to_str());

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("preview-overlay").is_none());
    view.update_in(cx, |this, _, _| assert!(this.preview.is_none()));
}

/// A click on a thumbnail while a session's right-click menu is open closes the menu as
/// it opens the preview, so Escape then closes the preview.
#[gpui::test]
fn a_thumbnail_click_closes_a_right_click_menu_before_its_preview(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx);
    cx.simulate_input("show me");
    cx.simulate_keystrokes("secondary-enter");
    wait(cx, |cx| {
        view.read_with(cx, |view, cx| {
            !view.state.read(cx).conversations()[&1].running
        })
    });
    let reply = row(&view, cx, |kind| *kind == ItemKind::AgentMessage);
    let thumbnail = selector(format!("thumbnail-{reply}-0"));
    wait(cx, |cx| cx.debug_bounds(thumbnail).is_some());
    let session = cx.debug_bounds("session-1").unwrap().center();
    cx.simulate_mouse_down(session, gpui::MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(session, gpui::MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    assert!(cx.debug_bounds("menu-open-in-new-window").is_some());
    click(cx, thumbnail);
    assert!(cx.debug_bounds("menu-open-in-new-window").is_none());
    assert!(cx.debug_bounds("preview-overlay").is_some());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("preview-overlay").is_none());
    assert!(view.read_with(cx, |view, _| view.context_menu.is_none()));
}

/// While the overlay shows, app shortcuts and Tab stay off and focus stays in it. Escape
/// closes it and gives focus back to what had it, here the composer.
#[gpui::test]
fn escape_closes_the_preview_and_restores_focus(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx);
    let shot = dir.path().join("project/shot.png");
    view.update_in(cx, |this, window, cx| {
        this.open_preview(crate::local_files::LocalPath::for_test(shot), window, cx)
    });
    cx.run_until_parked();
    for keys in ["tab", "secondary-l", "secondary-k", "secondary-enter"] {
        cx.simulate_keystrokes(keys);
        cx.run_until_parked();
        view.update_in(cx, |this, window, cx| {
            let overlay = this.preview.as_ref().expect(keys).overlay();
            assert!(overlay.focus_handle(cx).is_focused(window), "{keys}");
            assert!(this.palette.is_none(), "{keys}");
        });
    }
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("preview-overlay").is_none());
    view.update_in(cx, |this, window, cx| {
        assert!(this.composer.focus_handle(cx).is_focused(window));
    });
}

/// A press outside the image closes the overlay, including one in the toolbar's row
/// beside its buttons; one on the image or a button does not.
#[gpui::test]
fn a_press_outside_the_image_closes_the_preview(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx);
    let shot = dir.path().join("project/shot.png");
    for corner in [false, true] {
        view.update_in(cx, |this, window, cx| {
            this.open_preview(
                crate::local_files::LocalPath::for_test(shot.clone()),
                window,
                cx,
            )
        });
        wait(cx, |cx| cx.debug_bounds("preview-image").is_some());
        click(cx, "preview-image");
        click(cx, "preview-fit");
        assert!(cx.debug_bounds("preview-overlay").is_some());
        let overlay = cx.debug_bounds("preview-overlay").unwrap();
        let fit = cx.debug_bounds("preview-fit").unwrap();
        let press = if corner {
            overlay.bottom_left() + gpui::point(gpui::px(4.), gpui::px(-4.))
        } else {
            gpui::point(overlay.left() + gpui::px(40.), fit.center().y)
        };
        cx.simulate_click(press, Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("preview-overlay").is_none(), "{press:?}");
    }
}
