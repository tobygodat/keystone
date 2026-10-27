// Included into main.rs's test module, whose helpers these tests use.

use crate::import::ImportSessions;

/// A Claude Code user record in session `session` that ran in `cwd`.
fn claude_record(session: &str, uuid: &str, parent: Option<&str>, cwd: &str, text: &str) -> String {
    let record = serde_json::json!({"type": "user", "uuid": uuid, "parentUuid": parent,
        "cwd": cwd, "sessionId": session, "message": {"role": "user", "content": text}});
    format!("{record}\n")
}

/// Every file under `root` with its bytes.
fn files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut folders = vec![root.to_owned()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(folder).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                folders.push(path);
            } else {
                found.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    found.sort();
    found
}

/// The rows the open import list shows: each label and its detail.
fn import_rows(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Vec<(String, String)> {
    view.read_with(cx, |view, cx| {
        view.import_list.as_ref().map_or_else(Vec::new, |list| {
            list.state
                .read(cx)
                .matches()
                .map(|item| {
                    (
                        item.label.to_string(),
                        item.detail.as_deref().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
    })
}

/// Opens Import sessions… and picks the account on row `account`, waiting for its list.
fn list_account(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, account: usize) {
    cx.dispatch_action(ImportSessions);
    cx.run_until_parked();
    assert!(cx.debug_bounds("import").is_some(), "the import list opens");
    for _ in 0..account {
        cx.simulate_keystrokes("down");
    }
    cx.simulate_keystrokes("enter");
    wait_until(cx, view, |v, cx| {
        v.import_list
            .as_ref()
            .is_some_and(|list| list.state.read(cx).matches().next().is_some())
    });
}

/// A window on one Codex session whose providers are the scripted ones, run with
/// `env`, and whose default Claude account keeps its files in `claude_home`.
fn open_for_import<'a>(
    dir: &Path,
    claude_home: &Path,
    env: &str,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
    let programs = (
        wrapper(dir, "keystone-fake-codex", env),
        wrapper(dir, "keystone-fake-claude", env),
    );
    let (view, cx) = open_with_session(dir, cx);
    let home = claude_home.to_owned();
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, _| {
            state.test_programs = Some(programs);
            state.test_claude_home = Some(home);
        })
    });
    (view, cx)
}

fn imported_session(v: &Keystone, provider: Provider, thread: &str, cx: &App) -> Option<u64> {
    v.state
        .read(cx)
        .store()
        .workspace()
        .imported(provider, thread)
        .map(|s| s.id())
}

#[gpui::test]
fn a_claude_session_imports_its_newest_turns_and_resumes_on_its_account(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = working_directory().as_str().to_owned();
    let home = dir.path().join("claude-home");
    let folder = home.join("projects/scripted");
    std::fs::create_dir_all(&folder).unwrap();
    let mut long = String::new();
    for n in 0..52 {
        let parent = (n > 0).then(|| format!("l{}", n - 1));
        long += &claude_record(
            "long-session",
            &format!("l{n}"),
            parent.as_deref(),
            &cwd,
            &format!("Long session message {n}"),
        );
    }
    std::fs::write(folder.join("long-session.jsonl"), long).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let short = claude_record("short-session", "s0", None, &cwd, "Short session question");
    std::fs::write(folder.join("short-session.jsonl"), short).unwrap();
    let elsewhere = claude_record("elsewhere", "e0", None, "/elsewhere", "Not here");
    std::fs::write(folder.join("elsewhere.jsonl"), elsewhere).unwrap();
    let before = files(&home);
    let argv = dir.path().join("claude-argv");
    std::fs::write(&argv, "").unwrap();
    let env = format!(
        "KEYSTONE_FAKE_CLAUDE_DELAY_MS=1 KEYSTONE_FAKE_CODEX_DELAY_MS=1 \
         CLAUDE_CONFIG_DIR='{}' KEYSTONE_FAKE_CLAUDE_ARGV_LOG='{}'",
        home.display(),
        argv.display()
    );
    let (view, cx) = open_for_import(dir.path(), &home, &env, cx);

    list_account(&view, cx, 0);
    let rows = import_rows(&view, cx);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].0, "Short session question");
    assert!(rows[0].1.ends_with("1 turn"), "{rows:?}");
    assert_eq!(rows[1].0, "Long session message 0");
    assert!(rows[1].1.ends_with("52 turns"), "{rows:?}");

    cx.simulate_keystrokes("down enter");
    wait_until(cx, &view, |v, cx| {
        imported_session(v, Provider::Claude, "long-session", cx).is_some()
    });
    let session = view.read_with(cx, |v, cx| {
        imported_session(v, Provider::Claude, "long-session", cx).unwrap()
    });
    view.read_with(cx, |v, cx| {
        let state = v.state.read(cx);
        assert_eq!(state.store().workspace().selected(), Some(session));
        assert!(v.import_list.is_none());
        let saved = state.session(session).unwrap();
        assert_eq!(saved.title(), "Long session message 0");
        assert!(saved.started());
        let rows = &state.conversations()[&session].rows;
        assert!(matches!(rows[0], Row::OlderNotImported), "{:?}", rows[0]);
        let texts: Vec<&str> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) => Some(item.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), keystone_agent::import::TURNS);
        assert_eq!(texts[0], "Long session message 2");
    });
    assert_eq!(files(&home), before, "importing never writes to Claude's folder");

    send_text(&view, cx, "carry on");
    wait_until(cx, &view, move |v, cx| {
        v.state.read(cx).conversations()[&session]
            .rows
            .iter()
            .any(|row| matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage))
    });
    let launches = std::fs::read_to_string(&argv).unwrap();
    assert!(
        launches.contains(r#""--resume","long-session""#),
        "{launches}"
    );

    // Listed again, it shows as imported, and choosing it opens it, never a copy.
    view.update_in(cx, |view, window, cx| view.focus.focus(window, cx));
    list_account(&view, cx, 0);
    let rows = import_rows(&view, cx);
    assert!(rows[1].1.ends_with("Imported"), "{rows:?}");
    assert!(!rows[0].1.contains("Imported"), "{rows:?}");
    let count = view.read_with(cx, |v, cx| v.state.read(cx).store().workspace().sessions().len());
    view.update(cx, |view, cx| view.select(1, cx));
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        let workspace = v.state.read(cx).store().workspace();
        assert_eq!(workspace.sessions().len(), count);
        assert_eq!(workspace.selected(), Some(session));
    });
}

#[gpui::test]
fn a_codex_thread_imports_and_its_next_message_resumes_it(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = working_directory().as_str().to_owned();
    let codex_home = dir.path().join("codex-home");
    let threads = codex_home.join("keystone-fake-threads");
    std::fs::create_dir_all(&threads).unwrap();
    let stored = serde_json::json!({
        "thread": {"name": "Fixture Codex thread", "preview": "Codex question", "cwd": cwd,
            "createdAt": 1_700_000_000, "updatedAt": 1_700_000_000},
        "turns": [{"id": "turn-1", "status": "completed", "itemsView": "full", "items": [
            {"type": "userMessage", "id": "cu1", "content": [{"type": "text", "text": "Codex question"}]},
            {"type": "agentMessage", "id": "ca1", "text": "Codex answer"}
        ]}]
    });
    std::fs::write(threads.join("codex-thread-1.json"), stored.to_string()).unwrap();
    let before = files(&codex_home);
    let thread_log = dir.path().join("codex-threads");
    let env = format!(
        "KEYSTONE_FAKE_CODEX_DELAY_MS=1 CODEX_HOME='{}' KEYSTONE_FAKE_CODEX_THREAD_LOG='{}'",
        codex_home.display(),
        thread_log.display()
    );
    let empty = dir.path().join("claude-empty");
    let (view, cx) = open_for_import(dir.path(), &empty, &env, cx);

    list_account(&view, cx, 1);
    let rows = import_rows(&view, cx);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "Fixture Codex thread");
    assert!(rows[0].1.ends_with("1 turn"), "{rows:?}");

    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, |v, cx| {
        imported_session(v, Provider::Codex, "codex-thread-1", cx).is_some()
    });
    let session = view.read_with(cx, |v, cx| {
        imported_session(v, Provider::Codex, "codex-thread-1", cx).unwrap()
    });
    view.read_with(cx, |v, cx| {
        let rows = &v.state.read(cx).conversations()[&session].rows;
        let texts: Vec<&str> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) => Some(item.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Codex question", "Codex answer"]);
        assert!(!rows.iter().any(|row| matches!(row, Row::OlderNotImported)));
    });
    assert_eq!(files(&codex_home), before, "importing never writes to Codex's store");

    send_text(&view, cx, "carry on");
    wait_until(cx, &view, move |v, cx| {
        v.state.read(cx).conversations()[&session]
            .rows
            .iter()
            .filter(|row| matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage))
            .count()
            == 2
    });
    let requests = std::fs::read_to_string(&thread_log).unwrap();
    assert!(
        requests.lines().any(|line| line == "thread/resume codex-thread-1"),
        "{requests}"
    );
    assert!(
        requests.lines().any(|line| line == "turn/start codex-thread-1"),
        "{requests}"
    );
}

/// A conversation that changes after it was listed, so that it now names another
/// folder, is not imported; leaving the project closes the list.
#[gpui::test]
fn a_conversation_that_changed_or_a_project_left_imports_nothing(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = working_directory().as_str().to_owned();
    let home = dir.path().join("claude-home");
    let folder = home.join("projects/scripted");
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("moved.jsonl");
    std::fs::write(&file, claude_record("moved", "m0", None, &cwd, "Moved one")).unwrap();
    let env = "KEYSTONE_FAKE_CLAUDE_DELAY_MS=1 KEYSTONE_FAKE_CODEX_DELAY_MS=1";
    let (view, cx) = open_for_import(dir.path(), &home, env, cx);
    let sessions = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
        view.read_with(cx, |v, cx| v.state.read(cx).store().workspace().sessions().len())
    };
    let count = sessions(&view, cx);

    list_account(&view, cx, 0);
    assert_eq!(import_rows(&view, cx)[0].0, "Moved one");
    std::fs::write(&file, claude_record("moved", "m0", None, "/elsewhere", "Moved one")).unwrap();
    cx.simulate_keystrokes("enter");
    wait_until(cx, &view, |v, _| v.error.is_some());
    view.read_with(cx, |v, cx| {
        assert_eq!(
            v.error.as_deref(),
            Some("Not imported. It changed since it was listed.")
        );
        assert!(imported_session(v, Provider::Claude, "moved", cx).is_none());
    });
    assert_eq!(sessions(&view, cx), count);

    std::fs::write(&file, claude_record("moved", "m0", None, &cwd, "Moved one")).unwrap();
    view.update(cx, |v, _| v.error = None);
    view.update_in(cx, |view, window, cx| view.focus.focus(window, cx));
    list_account(&view, cx, 0);
    let other = tempfile::tempdir().unwrap();
    let other = keystone_core::ProjectId::new(other.path()).unwrap();
    view.update(cx, |view, cx| view.open_project(other, cx));
    cx.run_until_parked();
    view.read_with(cx, |v, _| assert!(v.import_list.is_none()));
    assert_eq!(sessions(&view, cx), count);
}
