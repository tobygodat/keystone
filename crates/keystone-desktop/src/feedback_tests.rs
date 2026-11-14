use super::*;
use gpui::{TestAppContext, VisualTestContext};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct StartGate(Arc<(Mutex<bool>, Condvar)>);

impl StartGate {
    fn hook(&self) -> keystone_agent::StartHook {
        let gate = self.0.clone();
        Arc::new(move |_| {
            drop(
                gate.1
                    .wait_timeout_while(gate.0.lock().unwrap(), Duration::from_secs(15), |open| {
                        !*open
                    })
                    .unwrap(),
            );
        })
    }

    fn release(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

impl Drop for StartGate {
    fn drop(&mut self) {
        self.release();
    }
}

fn open<'a>(
    dir: &Path,
    cx: &'a mut TestAppContext,
) -> (
    Entity<Keystone>,
    Entity<AppState>,
    StartGate,
    &'a mut VisualTestContext,
) {
    let mut store = Store::open(dir.join("workspace.json")).unwrap();
    let directory = keystone_core::WorkingDirectory::new(dir).unwrap();
    store.create(Provider::Codex, &directory, 1).unwrap();
    store.lock_model(1).unwrap();
    store
        .set_draft(1, "Saved draft before Send".into())
        .unwrap();
    let binary = keystone_core::test_support::built_binary("keystone-fake-codex");
    let script = dir.join("codex-wrapper");
    keystone_core::test_support::write_executable(
        &script,
        format!(
            "#!/bin/sh\nexec /usr/bin/env -i PATH=/usr/bin:/bin KEYSTONE_FAKE_CODEX_DELAY_MS=1 {} \"$@\"\n",
            shlex::try_quote(binary.to_str().unwrap()).unwrap()
        ),
    );
    let state = cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
        install_app_state(store, cx)
    });
    let gate = StartGate::default();
    state.update(cx, |state, _| {
        let connection = CodexConnection::new(Some(script), "test", state.test_wake().clone());
        connection.hold_starts(gate.hook());
        state.test_set_codex(Some(connection));
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let view = Keystone::new(state.clone(), false, window, cx);
        view.composer.focus_handle(cx).focus(window, cx);
        view
    });
    wait(cx, &state, |state| {
        state
            .conversations()
            .get(&1)
            .is_some_and(|c| !c.is_loading())
    });
    (view, state, gate, cx)
}

fn wait(cx: &mut VisualTestContext, state: &Entity<AppState>, done: impl Fn(&AppState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if state.read_with(cx, |state, _| done(state)) {
            return;
        }
        assert!(Instant::now() < deadline, "runtime did not progress");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn finish(state: &Entity<AppState>, gate: &StartGate, cx: &mut VisualTestContext) {
    gate.release();
    wait(cx, state, |state| {
        let conversation = &state.conversations()[&1];
        !conversation.running
            && conversation.rows.iter().any(|row| matches!(row,
                Row::Item(item) if item.kind == ItemKind::AgentMessage && item.text.contains("Scripted Codex reply")
            ))
    });
    state.update(cx, |state, cx| {
        state.flush_logs(cx).unwrap();
        assert!(state.conversations()[&1].error.is_none());
    });
}

#[gpui::test]
fn settings_status_combines_distinct_errors_and_deduplicates_identical_feedback(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, _gate, cx) = open(dir.path(), cx);
    view.update_in(cx, |view, window, cx| view.toggle_settings(window, cx));
    let shutdown = "Quit failed; storage still needs repair.";
    let runtime = "A background session could not be saved.";
    let later = "Settings not saved; the later change failed.";
    for (runtime_error, view_error) in [
        (runtime, shutdown),
        (runtime, later),
        (shutdown, later),
        (shutdown, shutdown),
    ] {
        state.update(cx, |state, _| {
            state.shutdown_error = Some(shutdown.into());
            state.error = Some(runtime_error.into());
        });
        view.update(cx, |view, cx| {
            view.error = Some(view_error.into());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("settings-page").is_some());
        assert!(cx.debug_bounds("settings-error").is_some());
        view.read_with(cx, |view, cx| {
            let message = view.status_line(cx).unwrap().0;
            for error in [shutdown, runtime_error, view_error] {
                assert_eq!(
                    message.matches(error).count(),
                    1,
                    "missing or repeated feedback: {message}"
                );
            }
        });
    }
}

#[gpui::test]
fn successful_send_clears_stale_view_error_and_persists_the_empty_composer(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, gate, cx) = open(dir.path(), cx);
    view.update_in(cx, |view, window, cx| {
        view.error = Some("Draft not saved; this is stale feedback.".into());
        view.composer
            .update(cx, |input, cx| input.set_value("Fresh Send".into(), cx));
        assert!(view.draft_save_task.is_none());
        let draft = view.composer_draft(cx).unwrap();
        view.send_message(draft, window, cx);
        assert!(view.composer.read(cx).value().is_empty());
        assert!(
            view.draft_save_task.is_none(),
            "set_value does not emit Changed"
        );
        assert!(
            view.error.is_none(),
            "successful Send left stale feedback: {:?}",
            view.error
        );
    });
    assert!(state.read_with(cx, |state, _| state.session(1).unwrap().draft().is_empty()));
    assert!(
        Store::open(dir.path().join("workspace.json"))
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft()
            .is_empty()
    );
    finish(&state, &gate, cx);
}

#[cfg(target_os = "linux")]
pub(crate) fn directory_path(directory: &std::fs::File) -> std::io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

#[cfg(target_os = "macos")]
pub(crate) fn directory_path(directory: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes a NUL-terminated path of at most PATH_MAX bytes.
    if unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len])))
}

fn fail_empty_draft_sync(directory: &std::fs::File) -> std::io::Result<()> {
    let dir = directory_path(directory)?;
    let draft: Option<String> = rusqlite::Connection::open_with_flags(
        dir.join("workspace.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .and_then(|saved| {
        saved.query_row(
            "SELECT json_extract(payload, '$.draft') FROM sessions ORDER BY position LIMIT 1",
            [],
            |row| row.get(0),
        )
    })
    .map_err(|error| std::io::Error::other(error.to_string()))?;
    if draft.unwrap_or_default().is_empty() {
        writeln!(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("empty-draft-sync-attempts"))?,
            "attempt"
        )?;
        return Err(std::io::Error::other(
            "final empty draft directory sync failed",
        ));
    }
    directory.sync_all()
}

#[gpui::test]
fn successful_send_checks_the_final_empty_draft_sync_without_a_changed_event(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, gate, cx) = open(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_sync_directory(fail_empty_draft_sync)
    });
    view.update_in(cx, |view, window, cx| {
        let draft = view.composer_draft(cx).unwrap();
        view.send_message(draft, window, cx);
        assert!(view.composer.read(cx).value().is_empty());
        assert!(
            view.draft_save_task.is_none(),
            "there is no debounce to check the final save"
        );
    });
    crate::tests::settle(&state, cx);
    view.read_with(cx, |view, cx| {
        let status = view.status_line(cx).map(|(status, _)| status);
        assert!(
            status
                .as_deref()
                .is_some_and(|error| error.contains("final empty draft directory sync failed")),
            "the final durability warning was lost: {status:?}"
        );
    });
    assert!(
        std::fs::read_to_string(dir.path().join("empty-draft-sync-attempts"))
            .unwrap()
            .lines()
            .count()
            >= 1,
        "the final save is checked even though the runtime already cleared the draft"
    );
    assert!(
        Store::open(dir.path().join("workspace.json"))
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft()
            .is_empty()
    );
    state.update(cx, |state, _| {
        state.test_sync_directory(std::fs::File::sync_all)
    });
    view.update(cx, |view, cx| assert!(view.save_draft(cx)));
    crate::tests::settle(&state, cx);
    view.read_with(cx, |view, cx| assert!(view.status_line(cx).is_none()));
    finish(&state, &gate, cx);
}

fn block_the_next_snapshot(directory: &std::fs::File) -> std::io::Result<()> {
    directory.sync_all()?;
    let dir = directory_path(directory)?;
    std::fs::rename(
        dir.join("workspace.sqlite3"),
        dir.join("workspace-before-failed-draft.sqlite3"),
    )?;
    std::fs::create_dir(dir.join("workspace.sqlite3"))
}

#[gpui::test]
fn successful_send_reports_a_failed_final_empty_draft_snapshot_without_changed(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, gate, cx) = open(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_sync_directory(block_the_next_snapshot)
    });
    view.update_in(cx, |view, window, cx| {
        let draft = view.composer_draft(cx).unwrap();
        view.send_message(draft, window, cx);
        assert!(view.composer.read(cx).value().is_empty());
        assert!(view.draft_save_task.is_none());
    });
    crate::tests::settle(&state, cx);
    view.read_with(cx, |view, cx| {
        let status = view.status_line(cx).map(|(status, _)| status);
        assert!(
            status
                .as_deref()
                .is_some_and(|error| error.starts_with("Not saved.")),
            "accepted Send lost the final draft snapshot error: {status:?}"
        );
    });
    state.update(cx, |state, _| {
        state.test_sync_directory(std::fs::File::sync_all)
    });
    std::fs::remove_dir(dir.path().join("workspace.sqlite3")).unwrap();
    std::fs::rename(
        dir.path().join("workspace-before-failed-draft.sqlite3"),
        dir.path().join("workspace.sqlite3"),
    )
    .unwrap();
    view.update(cx, |view, cx| assert!(view.save_draft(cx)));
    assert!(
        Store::open(dir.path().join("workspace.json"))
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft()
            .is_empty()
    );
    finish(&state, &gate, cx);
}

#[gpui::test]
fn autosave_supersedes_the_last_failed_queued_draft_but_keeps_shutdown_feedback(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, state, gate, cx) = open(dir.path(), cx);
    let sticky = "An earlier Quit failed; explicit retry is still required.";
    state.update(cx, |state, cx| {
        state.test_sync_directory(block_the_next_snapshot);
        state.shutdown_error = Some(sticky.into());
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: "Accepted before draft failure".into(),
                attachments: Vec::new(),
            },
            cx
        ));
    });
    crate::tests::settle(&state, cx);
    state.update(cx, |state, _| {
        assert!(
            state
                .error
                .as_deref()
                .is_some_and(|error| error.starts_with("Not saved.")),
            "the failed final Draft must be queued: {:?}",
            state.error
        );
        state.test_sync_directory(std::fs::File::sync_all);
    });
    std::fs::remove_dir(dir.path().join("workspace.sqlite3")).unwrap();
    std::fs::rename(
        dir.path().join("workspace-before-failed-draft.sqlite3"),
        dir.path().join("workspace.sqlite3"),
    )
    .unwrap();
    view.update(cx, |view, cx| {
        view.composer
            .update(cx, |input, cx| input.set_value(String::new(), cx));
    });
    cx.simulate_input("A newer autosaved draft");
    cx.run_until_parked();
    view.read_with(cx, |view, _| assert!(view.draft_save_task.is_some()));
    cx.executor().advance_clock(Duration::from_millis(250));
    cx.run_until_parked();
    crate::tests::settle(&state, cx);
    state.read_with(cx, |state, _| {
        assert!(
            state.error.is_none(),
            "superseded Draft still reports a failure: {:?}",
            state.error
        );
        assert_eq!(state.shutdown_error.as_deref(), Some(sticky));
        assert_eq!(state.session(1).unwrap().draft(), "A newer autosaved draft");
    });
    view.read_with(cx, |view, cx| {
        assert!(view.error.is_none());
        assert_eq!(view.status_line(cx).unwrap().0, sticky);
    });
    assert_eq!(
        Store::open(dir.path().join("workspace.json"))
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft(),
        "A newer autosaved draft"
    );
    finish(&state, &gate, cx);
    assert_eq!(
        Store::open(dir.path().join("workspace.json"))
            .unwrap()
            .workspace()
            .sessions()[0]
            .draft(),
        "A newer autosaved draft",
        "later provider metadata must not replay the superseded empty draft"
    );
}
