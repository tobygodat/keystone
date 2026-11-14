//! The `@` picker and Files' "Mention in composer" through the window, with the
//! scripted Claude logging what it is sent.

use super::*;
use crate::AppState;
use gpui::{TestAppContext, VisualTestContext};
use keystone_core::{Provider, Store, WorkingDirectory};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// The scripted Claude with short delays, logging its messages in the session folder.
fn wrapper(dir: &Path) -> PathBuf {
    let binary = keystone_core::test_support::built_binary("keystone-fake-claude");
    let script = dir.join("claude-mention-wrapper");
    keystone_core::test_support::write_executable(
        &script,
        format!(
            "#!/usr/bin/python3\nimport os, sys\nos.environ['KEYSTONE_FAKE_CLAUDE_DELAY_MS'] = '2'\nbinary = {}\nos.execv(binary, [binary, *sys.argv[1:]])\n",
            serde_json::to_string(&binary).unwrap()
        ),
    );
    script
}

/// A selected Claude session in a git folder with source files, a file whose path has
/// spaces and an ignored build folder, its saved draft `draft` and its composer focused.
fn open<'a>(
    draft: &str,
    cx: &'a mut TestAppContext,
) -> (
    tempfile::TempDir,
    Entity<Keystone>,
    &'a mut VisualTestContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("project");
    std::fs::create_dir_all(folder.join(".git")).unwrap();
    write(&folder.join(".gitignore"), "build/\n");
    for file in [
        "build/mod-03.o",
        "src/mod-03/lib.rs",
        "src/mod-30/lib.rs",
        "src/main.rs",
        "my notes/a b.txt",
        "README.md",
    ] {
        write(&folder.join(file), "");
    }
    let (view, cx) = open_in(dir.path(), &folder, draft, cx);
    (dir, view, cx)
}

/// A selected Claude session in `folder` with the saved draft `draft`, its store and
/// scripted Claude in `root`, and its composer focused.
fn open_in<'a>(
    root: &Path,
    folder: &Path,
    draft: &str,
    cx: &'a mut TestAppContext,
) -> (Entity<Keystone>, &'a mut VisualTestContext) {
    write(&folder.join(".fake-claude-messages"), "");
    let mut store = Store::open(root.join("workspace.json")).unwrap();
    let directory = WorkingDirectory::new(folder).unwrap();
    let id = store.create(Provider::Claude, &directory, 1).unwrap().value;
    store.select(id).unwrap();
    if !draft.is_empty() {
        store.set_draft(id, draft.into()).unwrap();
    }
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
    });
    cx.executor().allow_parking();
    let claude = wrapper(root);
    let (view, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = AppState::new(store, cx);
            state.test_programs = Some((claude.clone(), claude));
            state
        });
        Keystone::new(state, false, window, cx)
    });
    view.update_in(cx, |this, window, cx| {
        this.composer.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();
    (view, cx)
}

/// Runs the app with real time passing until `done` holds.
fn wait(
    cx: &mut VisualTestContext,
    view: &Entity<Keystone>,
    done: impl Fn(&Keystone, &App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |view, cx| done(view, cx)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The picker's rows once they are ranked for the query in the draft.
fn rows(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Vec<String> {
    wait(cx, view, |view, cx| {
        view.mention_target(cx)
            .is_some_and(|target| view.mention_rows(&target).1)
    });
    view.read_with(cx, |view, cx| {
        let target = view.mention_target(cx).unwrap();
        view.mention_rows(&target)
            .0
            .iter()
            .map(|found| found.path.clone())
            .collect()
    })
}

fn draft(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> (String, Vec<Range<usize>>) {
    view.read_with(cx, |view, cx| {
        let input = view.composer.read(cx);
        (input.value().to_owned(), input.tokens().to_vec())
    })
}

fn saved(dir: &tempfile::TempDir) -> String {
    Store::open(dir.path().join("workspace.json"))
        .unwrap()
        .workspace()
        .sessions()[0]
        .draft()
        .to_owned()
}

fn composer_focused(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> bool {
    let handle = view.read_with(cx, |view, cx| view.composer.focus_handle(cx));
    cx.update(|window, _| handle.is_focused(window))
}

/// `@` lists the folder's files without ignored ones, typing filters them, Enter
/// inserts the best match as one token that Backspace removes whole, and Send
/// delivers the text as typed.
#[gpui::test]
fn a_mention_is_chosen_as_one_token_and_sent_as_typed(cx: &mut TestAppContext) {
    let (dir, view, cx) = open("", cx);
    cx.simulate_input("Read @");
    assert!(
        cx.debug_bounds("mention-menu").is_some(),
        "the picker opens at once"
    );
    let all = rows(&view, cx);
    assert_eq!(
        all[..3],
        ["README.md", "my notes/a b.txt", "src/main.rs"],
        "shallowest first"
    );
    assert_eq!(all.len(), 5);
    assert!(all.iter().all(|path| !path.starts_with("build/")));
    assert!(cx.debug_bounds("mention-menu-list-row-0").is_some());

    cx.simulate_input("mod-03");
    assert_eq!(rows(&view, cx), ["src/mod-03/lib.rs"]);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        draft(&view, cx),
        ("Read @src/mod-03/lib.rs ".into(), vec![5..23])
    );
    assert!(
        cx.debug_bounds("mention-menu").is_none(),
        "choosing closes it"
    );
    assert_eq!(
        saved(&dir),
        "Read @src/mod-03/lib.rs ",
        "the draft is saved"
    );

    cx.simulate_keystrokes("backspace");
    assert!(
        cx.debug_bounds("mention-menu").is_none(),
        "a whole token does not reopen the picker"
    );
    cx.simulate_keystrokes("backspace");
    assert_eq!(draft(&view, cx), ("Read ".into(), vec![]));

    cx.simulate_input("@mod-03");
    rows(&view, cx);
    cx.simulate_keystrokes("tab");
    cx.simulate_input("please");
    cx.simulate_keystrokes("secondary-enter");
    let messages = dir.path().join("project/.fake-claude-messages");
    wait(cx, &view, |_, _| {
        std::fs::read_to_string(&messages).is_ok_and(|m| !m.is_empty())
    });
    assert_eq!(
        std::fs::read_to_string(&messages).unwrap().trim(),
        "\"Read @src/mod-03/lib.rs please\""
    );
}

/// Up and Down move the highlight without moving the caret, a path with spaces is
/// quoted, and Escape closes the picker until the draft changes.
#[gpui::test]
fn the_keyboard_drives_the_picker_and_escape_closes_it(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open("", cx);
    cx.simulate_input("@a");
    let all = rows(&view, cx);
    let notes = all.iter().position(|p| p == "my notes/a b.txt").unwrap();
    let highlighted = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        view.read_with(cx, |view, cx| view.mentions.list.read(cx).highlighted())
    };
    assert_eq!(highlighted(&view, cx), Some(0));
    for _ in 0..notes {
        cx.simulate_keystrokes("down");
    }
    cx.simulate_keystrokes("down up");
    assert_eq!(highlighted(&view, cx), Some(notes));
    assert_eq!(draft(&view, cx).0, "@a", "the caret stayed in the draft");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        draft(&view, cx),
        ("@\"my notes/a b.txt\" ".into(), vec![0..19])
    );

    cx.simulate_input("@sr");
    rows(&view, cx);
    cx.simulate_keystrokes("escape");
    assert!(
        cx.debug_bounds("mention-menu").is_none(),
        "Escape closes it"
    );
    cx.simulate_keystrokes("enter");
    assert_eq!(
        draft(&view, cx).0,
        "@\"my notes/a b.txt\" @sr\n",
        "with the picker closed, Enter is a new line"
    );
    cx.simulate_input("@s");
    assert!(
        cx.debug_bounds("mention-menu").is_some(),
        "an edit opens it again"
    );

    // While the rows shown were ranked for an earlier query, a click chooses the file
    // shown, and Enter waits for this query's rows and chooses the best of them.
    let stale = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        view.update(cx, |view, _| {
            let search = view.mentions.search.as_mut().unwrap();
            search.key.2 = "s".into();
            search.found = Arc::new(vec![Found {
                path: "README.md".into(),
                positions: Vec::new(),
            }]);
        })
    };
    cx.simulate_input("rc/mod-30");
    rows(&view, cx);
    stale(&view, cx);
    cx.simulate_keystrokes("enter");
    wait(cx, &view, |view, cx| {
        view.composer.read(cx).value().ends_with("lib.rs ")
    });
    assert!(draft(&view, cx).0.ends_with("@sr\n@src/mod-30/lib.rs "));

    cx.simulate_input("@mod-30");
    rows(&view, cx);
    // This query's ranking still running, behind the rows of an earlier one.
    view.update(cx, |view, cx| {
        let search = view.mentions.search.as_mut().unwrap();
        search.found = Arc::new(vec![Found {
            path: "README.md".into(),
            positions: Vec::new(),
        }]);
        search.running = Some(cx.spawn(async |_, _| std::future::pending::<()>().await));
    });
    cx.run_until_parked();
    let row = cx
        .debug_bounds("mention-menu-list-row-0")
        .expect("a shown row");
    cx.simulate_click(row.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(
        draft(&view, cx).0.ends_with("lib.rs @README.md "),
        "{:?}",
        draft(&view, cx)
    );
    assert!(composer_focused(&view, cx));
}

/// An Enter that waits for its rows replaces the `@word` it came in for, so nothing
/// once the caret has moved to another one.
#[gpui::test]
fn a_waiting_enter_does_not_follow_the_caret_to_another_word(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open("", cx);
    cx.simulate_input("@mod-30 @s");
    rows(&view, cx);
    // This query's ranking still running, behind the rows of an earlier one.
    view.update(cx, |view, cx| {
        let search = view.mentions.search.as_mut().unwrap();
        search.running = Some(cx.spawn(async |_, _| std::future::pending::<()>().await));
    });
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("home right");
    view.update(cx, |_, cx| cx.notify());
    rows(&view, cx);
    assert_eq!(draft(&view, cx), ("@mod-30 @s".into(), vec![]));
}

/// The palette over the picker hides it and takes Escape.
#[gpui::test]
fn the_palette_hides_the_picker_and_takes_escape(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open("", cx);
    cx.simulate_input("@sr");
    rows(&view, cx);
    cx.simulate_keystrokes("secondary-shift-p");
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.palette.is_some()));
    assert!(cx.debug_bounds("mention-menu").is_none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        view.read_with(cx, |view, _| view.palette.is_none()),
        "Escape closes the palette"
    );
}

/// A saved draft's mentions of existing files are tokens again when it loads, and a
/// loaded `@word` does not open the picker until it is edited.
#[gpui::test]
fn a_loaded_draft_marks_mentions_of_existing_files(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open("see @src/main.rs and @gone.rs @sr", cx);
    assert_eq!(
        draft(&view, cx),
        ("see @src/main.rs and @gone.rs @sr".into(), vec![4..16])
    );
    assert!(cx.debug_bounds("mention-menu").is_none());
    cx.simulate_input("c");
    assert!(cx.debug_bounds("mention-menu").is_some());
}

/// A draft typed into an `@word` and shown again, as on returning to its session,
/// does not reopen the picker until it is edited.
#[gpui::test]
fn a_draft_shown_again_does_not_reopen_the_picker(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open("", cx);
    cx.simulate_input("@sr");
    rows(&view, cx);
    view.update(cx, |view, cx| {
        view.save_draft(cx);
        view.sync_draft(cx)
    });
    cx.run_until_parked();
    assert_eq!(draft(&view, cx).0, "@sr");
    assert!(cx.debug_bounds("mention-menu").is_none());
}

/// Files offers "Mention in composer" only for a file whose path has a token.
#[gpui::test]
fn files_offers_no_mention_for_a_path_with_a_quote(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("project");
    write(&folder.join("q\"dir/plain.rs"), "");
    let (view, cx) = open_in(dir.path(), &folder, "", cx);
    cx.simulate_keystrokes("secondary-shift-e");
    cx.run_until_parked();
    // Rows: the folder, q"dir, then plain.rs once expanded.
    cx.simulate_keystrokes("down right down");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("copy-path").is_some(),
        "a file row is highlighted"
    );
    assert!(cx.debug_bounds("mention-file").is_none());
    cx.simulate_keystrokes("secondary-shift-m");
    cx.run_until_parked();
    assert_eq!(draft(&view, cx), (String::new(), vec![]));
}

/// Files' "Mention in composer", by its shortcut on the highlighted row or its button,
/// inserts the same token at the caret and returns focus to the composer.
#[gpui::test]
fn files_rows_mention_their_file_in_the_composer(cx: &mut TestAppContext) {
    let (dir, view, cx) = open("", cx);
    cx.simulate_input("Compare");
    cx.simulate_keystrokes("secondary-shift-e");
    cx.run_until_parked();
    // Rows: the folder, "my notes", "src", README.md.
    cx.simulate_keystrokes("down down down");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("mention-file").is_some(),
        "a highlighted file shows the button"
    );
    cx.simulate_keystrokes("secondary-shift-m");
    cx.run_until_parked();
    assert_eq!(
        draft(&view, cx),
        ("Compare @README.md ".into(), vec![8..18])
    );
    assert!(composer_focused(&view, cx), "the composer has focus again");
    assert_eq!(saved(&dir), "Compare @README.md ");

    cx.simulate_input("with");
    cx.simulate_keystrokes("secondary-shift-e");
    cx.run_until_parked();
    cx.simulate_keystrokes("up right");
    cx.run_until_parked();
    // Under src: mod-03, mod-30, main.rs.
    cx.simulate_keystrokes("down down down");
    cx.run_until_parked();
    let button = cx.debug_bounds("mention-file").expect("the button");
    cx.simulate_click(button.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(draft(&view, cx).0, "Compare @README.md with @src/main.rs ");
    assert!(composer_focused(&view, cx));
}

/// Typing `@mod-03` in the 50,000-file fixture: the time to ranked rows and the
/// longest frame after each key. Listing and ranking run on workers; the test
/// executor runs them on this thread too, inside each simulated key, so they count
/// toward the time to rows but not toward any frame, as in Files' benchmark.
#[gpui::test]
#[ignore = "benchmark; builds a 50,000-file fixture"]
fn perf_mention_rows(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("fixture");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/make-file-fixture.py");
    let status = std::process::Command::new("python3")
        .arg("-I")
        .arg(script)
        .arg(&fixture)
        .arg("50000")
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "fixture script failed");
    let (view, cx) = open_in(dir.path(), &fixture, "", cx);
    let frame = |cx: &mut VisualTestContext| {
        while cx.dispatcher.tick(true) {}
        let start = Instant::now();
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        start.elapsed()
    };
    let start = Instant::now();
    let mut longest = Duration::ZERO;
    for key in ["@", "m", "o", "d", "-", "0", "3"] {
        cx.simulate_input(key);
        longest = longest.max(frame(cx));
    }
    let shown = loop {
        longest = longest.max(frame(cx));
        let ready = view.read_with(cx, |view, cx| {
            view.mention_target(cx)
                .map(|target| view.mention_rows(&target))
                .filter(|(_, current)| *current)
                .map(|(found, _)| found)
        });
        if let Some(found) = ready
            && cx.debug_bounds("mention-menu-list-row-0").is_some()
        {
            break found;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "rows never drew");
    };
    let rows = start.elapsed();
    eprintln!(
        "PERF mention_rows_ms={:.2} mention_longest_frame_ms={:.2} first={}",
        rows.as_secs_f64() * 1000.,
        longest.as_secs_f64() * 1000.,
        shown[0].path,
    );
    assert!(shown.iter().all(|found| !found.path.starts_with("build/")));
    assert!(
        longest < Duration::from_millis(33),
        "longest frame took {longest:?}, over 33 ms"
    );
}
