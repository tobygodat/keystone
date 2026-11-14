//! The window's surfaces beside a session on another machine: none of them falls back
//! to this machine's project folder.

use crate::{Keystone, bind_keys};
use gpui::{AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};
use keystone_core::{ProjectId, RemoteFolder, Store, WindowId};
use std::path::Path;

/// A workspace with `desk` linked and a session on it, opened in a window whose project
/// is `project`, a folder on this machine.
fn remote_window<'a>(
    path: &Path,
    project: &Path,
    cx: &'a mut TestAppContext,
) -> (Entity<Keystone>, &'a mut VisualTestContext) {
    remote_window_in(path, project, "/srv/app", false, cx)
}

/// As [`remote_window`], with the session's folder `folder` on the machine, and with
/// `local` a session working in that same path here as well, the remote one selected.
fn remote_window_in<'a>(
    path: &Path,
    project: &Path,
    folder: &str,
    local: bool,
    cx: &'a mut TestAppContext,
) -> (Entity<Keystone>, &'a mut VisualTestContext) {
    let mut store = Store::open(path).unwrap();
    if local {
        let here = keystone_core::WorkingDirectory::new(Path::new(folder)).unwrap();
        store
            .create(keystone_core::Provider::Claude, &here, 1)
            .unwrap();
    }
    let desk = store.link_machine("desk", "client".into()).unwrap().value;
    let folder = RemoteFolder::new(folder).unwrap();
    let remote = store
        .create_remote_showing(&desk, &folder, 2, WindowId::FIRST, None)
        .unwrap()
        .value;
    store.select(remote).unwrap();
    // This machine's project had a browser page open before; a remote session never
    // loads it.
    store
        .set_browser_page(project.to_str().unwrap(), "http://localhost:5173/")
        .unwrap();
    store.flush().unwrap();
    drop(store);
    let store = Store::open(path).unwrap();
    cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
        crate::terminal::install(cx);
        crate::terminal::use_shell("/bin/sh", &[], cx);
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        Keystone::new(
            cx.new(|cx| crate::AppState::new(store, cx)),
            false,
            window,
            cx,
        )
    });
    view.update(cx, |view, _| {
        view.presentation.project = ProjectId::new(project).map_err(|e| e.to_string());
    });
    cx.simulate_resize(gpui::size(gpui::px(1280.), gpui::px(800.)));
    cx.run_until_parked();
    (view, cx)
}

fn shown(cx: &mut VisualTestContext, selector: &'static str) -> bool {
    cx.debug_bounds(selector).is_some()
}

#[gpui::test]
fn the_browser_says_it_isnt_available_for_a_session_on_another_machine(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = remote_window(&dir.path().join("workspace.json"), dir.path(), cx);
    view.update_in(cx, |view, window, cx| view.toggle_browser(window, cx));
    cx.run_until_parked();
    assert!(
        shown(cx, "browser-elsewhere"),
        "the panel names the machine instead of this project's dev servers"
    );
    assert!(!shown(cx, "browser-empty"));
    view.read_with(cx, |view, cx| {
        assert_eq!(
            view.browser.read(cx).folder(),
            None,
            "no local folder is browsed"
        );
    });
    let project = dir.path().to_str().unwrap().to_owned();
    let saved = view.update(cx, |view, cx| {
        view.state
            .update(cx, |state, _| state.browser_to_restore(&project))
    });
    assert_eq!(
        saved.as_deref(),
        Some("http://localhost:5173/"),
        "this machine's saved page was left for a local session"
    );
}

#[gpui::test]
fn a_terminal_only_command_in_a_remote_session_offers_no_local_command(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = remote_window(&dir.path().join("workspace.json"), dir.path(), cx);
    view.update_in(cx, |view, window, cx| view.focus_composer(window, cx));
    cx.simulate_input("/login");
    cx.simulate_keystrokes("secondary-enter");
    cx.run_until_parked();
    assert!(
        shown(cx, "slash-terminal-elsewhere"),
        "the notice says the command runs on the machine"
    );
    assert!(
        !shown(cx, "slash-terminal-copy"),
        "no command for this machine's terminal is offered"
    );
    view.read_with(cx, |view, cx| {
        assert_eq!(view.composer.read(cx).value(), "/login", "the draft stays");
    });
}

#[gpui::test]
fn the_terminal_says_it_isnt_available_for_a_session_on_another_machine(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = remote_window(&dir.path().join("workspace.json"), dir.path(), cx);
    let session = view.read_with(cx, |view, cx| view.selected(cx)).unwrap();
    let no_shell = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| crate::terminal::session_terminal(session, cx).is_none())
    };
    view.update_in(cx, |view, window, cx| view.focus_composer(window, cx));
    cx.simulate_keystrokes("secondary-j");
    cx.run_until_parked();
    assert!(
        shown(cx, "terminal-elsewhere"),
        "the drawer names the machine instead of opening a shell here"
    );
    assert!(!shown(cx, "terminal-drawer"));
    assert!(no_shell(cx), "no shell starts on this machine");

    cx.simulate_keystrokes("secondary-shift-d");
    cx.run_until_parked();
    assert!(
        shown(cx, "terminal-elsewhere"),
        "Split shows the same refusal"
    );
    assert!(no_shell(cx));

    let toggle = cx
        .debug_bounds("terminal-toggle")
        .expect("the top bar button");
    cx.simulate_click(toggle.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(!shown(cx, "terminal-elsewhere"), "the button hides it");
    let toggle = cx
        .debug_bounds("terminal-toggle")
        .expect("the top bar button");
    cx.simulate_click(toggle.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(shown(cx, "terminal-elsewhere"), "and shows it again");
    assert!(no_shell(cx));
}

/// Every kind of row whose text names a file in `folder`: a mention and a path in the
/// user's message, a command's output, file changes naming an image, a video and a text
/// file, by absolute and relative path, and a reply with Markdown images and links.
fn rows_naming_files(folder: &Path) -> Vec<keystone_agent::Event> {
    use keystone_agent::{Event, Item, ItemKind, ItemStatus, TurnStatus};
    let at = folder.display();
    let item = |id: &str, kind: ItemKind, text: String| {
        Event::ItemCompleted(Item {
            id: id.into(),
            kind,
            text,
            status: ItemStatus::Completed,
        })
    };
    let change = |id: &str, path: String| {
        item(
            id,
            ItemKind::FileChange {
                paths: vec![path],
                patch: None,
            },
            String::new(),
        )
    };
    // An edit with its diff, which comes from the agent, not from a file here.
    let edit = |id: &str, path: String| {
        use keystone_agent::diff::{FileDiff, Hunk, Patch};
        let hunk = Hunk::new(1, 1, ["-on the other machine", "+edited there"]).unwrap();
        let patch = Patch::edit(vec![FileDiff {
            path: path.clone(),
            hunks: vec![hunk],
            ..FileDiff::default()
        }]);
        item(
            id,
            ItemKind::FileChange {
                paths: vec![path],
                patch: Some(patch),
            },
            String::new(),
        )
    };
    vec![
        Event::TurnStarted,
        item(
            "u",
            ItemKind::UserMessage,
            format!("look at @notes.txt and {at}/shot.png"),
        ),
        item(
            "c",
            ItemKind::Command {
                command: format!("cat {at}/notes.txt"),
                exit_code: Some(0),
            },
            format!("{at}/notes.txt: written on the other machine"),
        ),
        change("f1", format!("{at}/shot.png")),
        change("f2", "shot.png".into()),
        change("f3", format!("{at}/clip.mp4")),
        edit("f4", format!("{at}/notes.txt")),
        item(
            "a",
            ItemKind::AgentMessage,
            format!(
                "Done.\n\n![shot]({at}/shot.png)\n\n![relative](shot.png)\n\n[notes]({at}/notes.txt) and [link](file://{at}/notes.txt)"
            ),
        ),
        Event::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
            stopped: false,
        },
    ]
}

/// Lets real time pass so any file a row asked for is read and drawn.
fn settle(cx: &mut VisualTestContext) {
    for _ in 0..30 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The media a transcript drew: each thumbnail or video card, by its row and place.
fn media(cx: &mut VisualTestContext) -> Vec<String> {
    let mut found = Vec::new();
    for row in 0..16 {
        for n in 0..3 {
            for kind in ["thumbnail", "video"] {
                let id: &'static str = format!("{kind}-{row}-{n}").leak();
                if cx.debug_bounds(id).is_some() {
                    found.push(id.to_owned());
                }
            }
        }
    }
    found
}

/// The paths a session's rows resolved to files on this machine under `folder`.
fn resolved_under(folder: &Path) -> Vec<std::path::PathBuf> {
    crate::local_files::tests::RESOLVED
        .lock()
        .unwrap()
        .iter()
        .filter(|path| path.starts_with(folder))
        .cloned()
        .collect()
}

/// A session on another machine names paths there. Each path here holds a different
/// file, which must never be shown, opened or read as the agent's: no row draws a
/// thumbnail or video card, clicking every row opens nothing, mentioning and attaching
/// anything but an image picked here by its full path refuse before anything is read,
/// and no transcript path resolves to a file here. The
/// same rows on a session working in that folder here still preview.
#[gpui::test]
fn no_row_of_a_remote_session_reads_this_machines_files(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().canonicalize().unwrap().join("app");
    std::fs::create_dir(&folder).unwrap();
    image::RgbaImage::from_pixel(8, 8, image::Rgba([0, 0, 255, 255]))
        .save(folder.join("shot.png"))
        .unwrap();
    std::fs::write(folder.join("notes.txt"), "this machine's notes").unwrap();
    std::fs::write(folder.join("clip.mp4"), "this machine's video").unwrap();
    let (view, cx) = remote_window_in(
        &dir.path().join("workspace.json"),
        dir.path(),
        folder.to_str().unwrap(),
        true,
        cx,
    );
    // Tall enough that every row is drawn even when a wrongly given handle draws
    // thumbnails, so a leak fails the media check below, not the count of rows.
    cx.simulate_resize(gpui::size(gpui::px(1280.), gpui::px(4000.)));
    cx.run_until_parked();
    let (local, remote) = (1, 2);
    assert_eq!(
        view.read_with(cx, |view, cx| view.selected(cx)),
        Some(remote)
    );
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            for session in [local, remote] {
                for event in rows_naming_files(&folder) {
                    state.test_conversation(session).apply(event);
                }
            }
            cx.notify();
        });
        view.commit(local, cx);
        view.commit(remote, cx);
    });
    settle(cx);

    let drawn: Vec<&'static str> = (0..16)
        .map(|row| -> &'static str { format!("transcript-row-{row}").leak() })
        .filter(|id| cx.debug_bounds(id).is_some())
        .collect();
    assert!(
        drawn.len() >= 6,
        "the remote session's rows are drawn: {drawn:?}"
    );
    assert_eq!(
        media(cx),
        Vec::<String>::new(),
        "no row of the remote session draws media"
    );
    for id in drawn {
        if let Some(bounds) = cx.debug_bounds(id) {
            cx.simulate_click(bounds.center(), Modifiers::none());
            settle(cx);
        }
    }
    assert!(
        !shown(cx, "preview-overlay"),
        "clicking a row opens nothing"
    );
    assert_eq!(media(cx), Vec::<String>::new());
    // The edit row opens to its own diff, its path shown from the remote folder.
    if shown(cx, "activity-collapsed-5") {
        let toggle = cx.debug_bounds("activity-collapsed-5").unwrap();
        cx.simulate_click(toggle.center(), Modifiers::none());
        settle(cx);
    }
    assert!(shown(cx, "edit-5-line-1"), "the remote edit draws its diff");
    view.read_with(cx, |view, cx| {
        let path = format!("{}/notes.txt", folder.display());
        assert_eq!(view.edit_folder(remote, cx).shown(&path), "notes.txt");
    });
    assert_eq!(media(cx), Vec::<String>::new());

    // A path the transcript could name is no file of the session's, so a relative
    // one is refused unread, and a file that isn't an image isn't mentioned there.
    view.update_in(cx, |view, window, cx| {
        view.attach_paths(
            vec!["shot.png".into(), folder.join("notes.txt")],
            window,
            cx,
        );
    });
    settle(cx);
    view.read_with(cx, |view, cx| {
        let notice = view
            .attach
            .notice
            .as_ref()
            .map(|(_, n)| n.clone())
            .unwrap_or_default();
        assert!(
            notice.contains("shot.png isn't a full path on this machine")
                && notice.contains("notes.txt isn't an image. Only images go to sessions on desk"),
            "{notice}"
        );
        assert!(view.draft_attachments(cx).is_empty());
        let draft = view
            .state
            .read(cx)
            .session(remote)
            .unwrap()
            .draft()
            .to_owned();
        assert!(!draft.contains("notes.txt"), "nothing mentioned: {draft}");
    });
    assert_eq!(resolved_under(&folder), Vec::<std::path::PathBuf>::new());
    // An image the user picks here by its full path goes by its bytes.
    view.update_in(cx, |view, window, cx| {
        view.attach_paths(vec![folder.join("shot.png")], window, cx);
    });
    settle(cx);
    view.read_with(cx, |view, cx| {
        assert_eq!(
            view.draft_attachments(cx).len(),
            1,
            "{:?}",
            view.attach.notice
        );
    });
    let sha = view.read_with(cx, |view, cx| view.draft_attachments(cx)[0].sha256.clone());
    view.update_in(cx, |view, window, cx| {
        view.remove_attachment(remote, &sha, window, cx)
    });
    settle(cx);

    view.update_in(cx, |view, window, cx| view.focus_composer(window, cx));
    cx.simulate_input("@not");
    settle(cx);
    assert!(shown(cx, "mention-menu"), "@ says why it lists nothing");
    view.read_with(cx, |view, cx| {
        assert_eq!(view.mention_root(cx), None, "no folder here is listed");
    });
    assert_eq!(resolved_under(&folder), Vec::<std::path::PathBuf>::new());

    view.update(cx, |view, cx| view.select(local, cx));
    settle(cx);
    let drawn = media(cx);
    assert!(
        drawn.iter().any(|id| id.starts_with("thumbnail")),
        "the same rows on a session here still preview: {drawn:?}"
    );
    assert!(!resolved_under(&folder).is_empty());
}

/// A device host that lists one local emulator and records whatever it is asked.
#[derive(Default)]
struct Watched(std::sync::Mutex<Vec<String>>);

impl Watched {
    fn asked(&self, what: String) {
        self.0.lock().unwrap().push(what);
    }

    fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl keystone_agent::device::DeviceHost for Watched {
    fn id(&self) -> keystone_agent::device::HostId {
        keystone_agent::device::HostId::Local
    }

    fn inventory(&self, _: keystone_agent::device::Deadline) -> keystone_agent::device::Inventory {
        self.asked("inventory".into());
        keystone_agent::device::Inventory {
            devices: vec![keystone_agent::device::Device {
                id: "android:Pixel_8".parse().unwrap(),
                name: "Pixel 8".into(),
                os: "Android".into(),
                power: keystone_agent::device::Power::Booted,
            }],
            platforms: Vec::new(),
        }
    }

    fn start(
        &self,
        device: &keystone_agent::device::DeviceId,
        _: keystone_agent::device::Deadline,
    ) -> Result<keystone_agent::device::Start, keystone_agent::device::DeviceError> {
        self.asked(format!("start {device}"));
        Ok(keystone_agent::device::Start::Booted)
    }

    fn wait(
        &self,
        device: &keystone_agent::device::DeviceId,
        _: keystone_agent::device::Deadline,
    ) -> Result<keystone_agent::device::BootState, keystone_agent::device::DeviceError> {
        self.asked(format!("wait {device}"));
        Ok(keystone_agent::device::BootState::Booted)
    }

    fn screenshot(
        &self,
        device: &keystone_agent::device::DeviceId,
        _: keystone_agent::device::Deadline,
    ) -> Result<keystone_agent::device::Frame, keystone_agent::device::DeviceError> {
        self.asked(format!("screenshot {device}"));
        Err(keystone_agent::device::DeviceError::NotBooted)
    }

    fn act(
        &self,
        device: &keystone_agent::device::DeviceId,
        _: &keystone_agent::device::Action,
        _: keystone_agent::device::Deadline,
    ) -> Result<(), keystone_agent::device::DeviceError> {
        self.asked(format!("act {device}"));
        Ok(())
    }

    fn shutdown(
        &self,
        device: &keystone_agent::device::DeviceId,
        _: keystone_agent::device::Deadline,
    ) -> Result<(), keystone_agent::device::DeviceError> {
        self.asked(format!("shutdown {device}"));
        Ok(())
    }

    fn stop(&self) {}
}

#[gpui::test]
fn the_device_panel_says_it_isnt_available_for_a_session_on_another_machine(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = remote_window(&dir.path().join("workspace.json"), dir.path(), cx);
    let host = std::sync::Arc::new(Watched::default());
    let session = view.read_with(cx, |view, cx| view.selected(cx)).unwrap();
    view.update(cx, |view, cx| {
        let host = host.clone();
        view.state.update(cx, |state, _| {
            state.devices = crate::device::Devices::new(host)
        });
    });
    view.update_in(cx, |view, window, cx| view.focus_composer(window, cx));
    cx.simulate_keystrokes("secondary-alt-v");
    cx.run_until_parked();
    assert!(
        shown(cx, "device-elsewhere"),
        "the panel names the machine instead of this machine's devices"
    );
    assert!(!shown(cx, "device-picker"), "no local device is offered");
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state.open_device(session, "android:Pixel_8".parse().unwrap(), cx)
        })
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(100));
    cx.run_until_parked();
    assert!(!shown(cx, "device-frame"), "nothing opens beside it");
    assert_eq!(
        host.calls(),
        Vec::<String>::new(),
        "no device tool ran here"
    );

    let refused = view.read_with(cx, |view, cx| {
        let caller = keystone_agent::mcp::Caller {
            session,
            generation: 0,
        };
        view.state
            .read(cx)
            .device_call(caller, "device_screenshot", &serde_json::json!({}))
            .err()
    });
    assert!(
        refused.as_deref().is_some_and(|why| why.contains("desk")),
        "a device tool refuses the caller by its machine: {refused:?}"
    );
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} rendered"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

/// A login on another machine is offered only to that machine's sessions, and names
/// which machine, so the choice reads as one login in one place. A session here never
/// sees it, and it never counts as an account of this machine's.
#[gpui::test]
fn a_login_on_a_machine_is_offered_only_to_that_machines_sessions(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().canonicalize().unwrap().join("app");
    std::fs::create_dir(&folder).unwrap();
    let (view, cx) = remote_window_in(
        &dir.path().join("workspace.json"),
        dir.path(),
        folder.to_str().unwrap(),
        true,
        cx,
    );
    let (local, remote) = (1, 2);
    let work = view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            let desk = state.store().workspace().machines()[0].id().clone();
            state
                .add_remote_account(
                    keystone_core::Provider::Claude,
                    "Work",
                    "/home/dev/.claude-work",
                    &desk,
                    cx,
                )
                .unwrap()
                .value
        })
    });
    click(cx, "model-selector");
    assert!(shown(cx, "account-claude-default"));
    assert!(shown(cx, "account-claude-work"));
    click(cx, "account-claude-work");
    view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        assert_eq!(state.session(remote).unwrap().account(), Some(&work));
        let selection = state.model_selection(remote, keystone_core::Provider::Claude);
        let (trigger, tooltip) = crate::model_selector::trigger(selection, false, cx);
        drop(trigger);
        assert!(
            tooltip.starts_with("Claude Code · Work on desk"),
            "{tooltip}"
        );
    });

    view.update(cx, |view, cx| view.select(local, cx));
    cx.run_until_parked();
    click(cx, "model-selector");
    assert!(
        !shown(cx, "account-claude-default") && !shown(cx, "account-claude-work"),
        "a session here has no account but its default"
    );
    view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        let refused = state
            .store()
            .workspace()
            .account(&work)
            .map(|account| account.home());
        assert_eq!(refused, Some(None), "the login's home has no path here");
    });
    let wrong = view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state
                .set_provider_account(
                    local,
                    keystone_core::Provider::Claude,
                    Some(work.clone()),
                    cx,
                )
                .unwrap_err()
                .to_string()
        })
    });
    assert!(wrong.contains("another machine"), "{wrong}");
}
