//! Sessions on another machine, driven through the real `keystone-host attach` and the
//! scripted Claude in place of `ssh` and `claude`. The "machine" shares this one's
//! filesystem, so each test says which side may touch a folder.

use super::*;
use keystone_agent::ItemKind;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

/// A built binary beside the test executable.
fn binary(name: &str) -> PathBuf {
    let path = keystone_core::test_support::built_binary(name);
    path
}

/// A host folder for one machine, private as the host requires.
struct Host(tempfile::TempDir);

impl Host {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(dir)
    }

    /// `keystone-host attach` with this machine's environment, as SSH would run it.
    fn argv(&self) -> Vec<std::ffi::OsString> {
        host_argv(self.0.path())
    }

    /// [`Host::argv`] with `extra` variables in the host's environment, which its
    /// providers inherit.
    fn argv_with(&self, extra: &[(&str, &Path)]) -> Vec<std::ffi::OsString> {
        let mut argv = self.argv();
        let at = argv.len() - 2;
        for (name, value) in extra {
            argv.insert(at, format!("{name}={}", value.display()).into());
        }
        argv
    }

    /// The daemon's process, while it holds its lock.
    fn daemon(&self) -> Option<i32> {
        let lock = self.0.path().join("run").join("host.lock");
        std::fs::read_to_string(lock).ok()?.trim().parse().ok()
    }
}

/// `keystone-host attach` for the machine whose host folder is `dir`.
fn host_argv(dir: &Path) -> Vec<std::ffi::OsString> {
    let dir = dir.to_str().unwrap();
    vec![
        "env".into(),
        format!("KEYSTONE_HOST_DIR={dir}").into(),
        format!("HOME={dir}").into(),
        format!(
            "KEYSTONE_CLAUDE_PATH={}",
            binary("keystone-fake-claude").display()
        )
        .into(),
        format!(
            "KEYSTONE_CODEX_PATH={}",
            binary("keystone-fake-codex").display()
        )
        .into(),
        binary("keystone-host").into(),
        "attach".into(),
    ]
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon() {
            // SAFETY: kill only signals the test's own daemon.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn shell(script: &str) -> Vec<std::ffi::OsString> {
    vec!["sh".into(), "-c".into(), script.into()]
}

/// A folder the scripted Claude takes its settings from, as `.fake-claude-<name>`.
fn folder(settings: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for file in ["pids", "messages"] {
        std::fs::write(dir.path().join(format!(".fake-claude-{file}")), "").unwrap();
    }
    for (name, value) in settings {
        std::fs::write(dir.path().join(format!(".fake-claude-{name}")), value).unwrap();
    }
    dir
}

fn runtime(dir: &Path, cx: &mut gpui::TestAppContext) -> Entity<AppState> {
    let store = Store::open(dir.join("workspace.json")).unwrap();
    cx.executor().allow_parking();
    cx.new(|cx| AppState::new(store, cx))
}

#[track_caller]
fn wait_until(
    cx: &mut gpui::TestAppContext,
    state: &Entity<AppState>,
    what: &str,
    done: impl Fn(&AppState) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        if state.read_with(cx, |state, _| done(state)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            state.read_with(cx, |state, _| (
                state.linking.as_ref().map(|l| l.failed.clone()),
                state
                    .links
                    .iter()
                    .map(|(id, link)| (id.clone(), link.status()))
                    .collect::<Vec<_>>(),
                state
                    .conversations
                    .iter()
                    .map(|(id, c)| (*id, c.running, c.rows.clone(), c.error.clone()))
                    .collect::<Vec<_>>()
            ))
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Links a machine through `argv` and returns why it failed, or `None` once it is saved.
fn link(
    state: &Entity<AppState>,
    argv: Vec<std::ffi::OsString>,
    destination: &str,
    cx: &mut gpui::TestAppContext,
) -> Option<String> {
    state.update(cx, |state, cx| {
        state.test_transport = Some(argv);
        state.link_machine(destination, cx);
    });
    wait_until(cx, state, "the link to answer", |state| {
        state.linking.as_ref().is_none_or(|l| l.failed.is_some())
    });
    state.read_with(cx, |state, _| {
        state.linking.as_ref().and_then(|l| l.failed.clone())
    })
}

fn send(state: &Entity<AppState>, session: u64, text: &str, cx: &mut gpui::TestAppContext) -> bool {
    state.update(cx, |state, cx| {
        state.send_message(
            ComposerDraft {
                session,
                text: text.into(),
                attachments: Vec::new(),
            },
            cx,
        )
    })
}

fn ended(state: &AppState, session: u64) -> bool {
    state.conversations.get(&session).is_some_and(|c| {
        !c.running
            && c.rows
                .iter()
                .any(|row| matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage && item.status == keystone_agent::ItemStatus::Completed))
    })
}

/// Each item row's kind, text and status, without the IDs a provider makes up.
fn items(state: &AppState, session: u64) -> Vec<(String, String)> {
    state.conversations[&session]
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Item(item) => Some((
                format!("{:?} {:?}", item.kind, item.status),
                item.text.clone(),
            )),
            _ => None,
        })
        .collect()
}

fn desk(state: &AppState) -> MachineId {
    state.store.workspace().machines()[0].id().clone()
}

#[gpui::test]
fn linking_saves_a_machine_only_once_its_host_answers(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    let failures = [
        (
            shell("echo 'dev@desk: Permission denied (publickey).' >&2; exit 255"),
            "SSH refused the login",
        ),
        (
            shell("echo 'Host key verification failed.' >&2; exit 255"),
            "Keystone never accepts a host key",
        ),
        (
            shell(
                "printf '@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n@@@@@@@@\nHost key verification failed.\n' >&2; exit 255",
            ),
            "host key changed since you last connected",
        ),
        (
            shell("exit 127"),
            "keystone-host isn't installed on that machine",
        ),
        (
            shell(
                "read hello; echo '{\"t\":\"welcome\",\"protocol\":99,\"version\":\"9.0.0\",\"os\":\"linux\",\"arch\":\"x86_64\",\"hostname\":\"desk\",\"providers\":[],\"oldest\":0,\"latest\":0}'; sleep 5",
            ),
            "speaks protocol 99, and Keystone needs 3",
        ),
        (
            shell(
                "echo 'ssh: Could not resolve hostname desk: Name or service not known' >&2; exit 255",
            ),
            "Could not resolve hostname desk",
        ),
    ];
    for (argv, reason) in failures {
        let failed = link(&state, argv, "desk", cx).expect("linking fails");
        assert!(failed.contains(reason), "{failed:?} says {reason:?}");
        state.read_with(cx, |state, _| {
            assert!(
                state.store.workspace().machines().is_empty(),
                "nothing saved"
            );
        });
    }
    state.update(cx, |state, cx| state.link_machine("-oProxyCommand=x", cx));
    state.read_with(cx, |state, _| {
        let failed = state.linking.as_ref().unwrap().failed.clone().unwrap();
        assert!(failed.contains("isn't an SSH destination"), "{failed}");
    });

    let host = Host::new();
    assert_eq!(link(&state, host.argv(), "dev@desk", cx), None);
    state.read_with(cx, |state, _| {
        let machine = &state.store.workspace().machines()[0];
        assert_eq!(machine.destination(), "dev@desk");
        assert_eq!(state.machine_status(machine.id()), LinkStatus::Connected);
        let welcome = state.machine_welcome(machine.id()).unwrap();
        assert!(
            welcome
                .providers
                .iter()
                .any(|found| found.provider == Provider::Claude && found.version.is_some())
        );
    });
    let again = link(&state, host.argv(), "dev@desk", cx).unwrap();
    assert!(again.contains("already linked"), "{again}");
}

/// The gate between this machine's folders and another's. Session A's folder exists
/// here too, as a Git repository with checkpoints on, and B's exists nowhere. A turn
/// on A runs on the host and writes no checkpoint here. B's send is taken here and
/// refused by the host, so nothing here checked either folder.
#[gpui::test]
fn no_local_folder_check_sees_a_remote_sessions_folder(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| state.test_checkpoints = true);
    let host = Host::new();
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let a = folder(&[("delay-ms", "1")]);
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(a.path())
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "start",
    ]);
    let missing = format!("/nonexistent/keystone-remote-{}", std::process::id());
    let (first, second) = state.update(cx, |state, cx| {
        let machine = desk(state);
        let make = |state: &mut AppState, path: &str, cx: &mut Context<AppState>| {
            let folder = RemoteFolder::new(path).unwrap();
            state
                .create_remote(&machine, &folder, WindowId::FIRST, None, cx)
                .unwrap()
                .value
        };
        (
            make(state, a.path().to_str().unwrap(), cx),
            make(state, &missing, cx),
        )
    });

    assert!(send(&state, first, "hello", cx));
    wait_until(cx, &state, "the remote turn to end", |state| {
        ended(state, first)
    });
    assert!(send(&state, second, "hello", cx), "taken here, unchecked");
    let refused = format!("{missing} isn't a folder on this machine");
    wait_until(cx, &state, "the host to refuse B's folder", |state| {
        state.conversations[&second].rows.iter().any(
            |row| matches!(row, Row::TurnEnded { error: Some(error), .. } if *error == refused),
        ) || state.conversations[&second].error.as_deref() == Some(refused.as_str())
    });

    state.read_with(cx, |state, _| {
        for session in [first, second] {
            assert!(state.checkpoints(session).is_none(), "no checkpoint read");
            assert!(state.catalog_key(session, Provider::Claude).is_none());
            assert!(state.conversations[&session].recovery.is_none());
        }
        assert!(!state.has_running_session(Provider::Claude, None, true));
        assert!(state.claude.is_empty(), "no local Claude ran");
    });
    assert_eq!(
        git(&["for-each-ref", "--format=%(refname)", "refs/keystone"]),
        "",
        "no checkpoint written in A's folder"
    );
    assert_eq!(
        std::fs::read_to_string(a.path().join(".fake-claude-messages"))
            .unwrap()
            .lines()
            .count(),
        1,
        "the host's Claude took the message"
    );
}

/// A turn whose link drops partway through reads the same as one whose link held:
/// every event arrives once and in order. Send is refused while the link is down.
#[gpui::test]
fn a_turn_survives_a_dropped_link_with_every_event_once(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    let host = Host::new();
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let steady = folder(&[("delay-ms", "4")]);
    let dropped = folder(&[("delay-ms", "4")]);
    let (reference, session) = state.update(cx, |state, cx| {
        let machine = desk(state);
        let mut make = |path: &Path| {
            let folder = RemoteFolder::new(path.to_str().unwrap()).unwrap();
            state
                .create_remote(&machine, &folder, WindowId::FIRST, None, cx)
                .unwrap()
                .value
        };
        (make(steady.path()), make(dropped.path()))
    });
    assert!(send(&state, reference, "hello long", cx));
    wait_until(cx, &state, "the steady turn to end", |state| {
        ended(state, reference)
    });

    assert!(send(&state, session, "hello long", cx));
    wait_until(cx, &state, "the turn to stream", |state| {
        state.conversations[&session]
            .rows
            .iter()
            .any(|row| matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage && !item.text.is_empty()))
    });
    let transport = state.read_with(cx, |state, _| {
        state.links[&desk(state)].transport_pid().unwrap() as i32
    });
    // SAFETY: kill only signals the link's own transport, as a dropped network would end it.
    unsafe { libc::kill(transport, libc::SIGKILL) };
    wait_until(cx, &state, "the link to drop", |state| {
        state.machine_status(&desk(state)) == LinkStatus::Reconnecting
    });
    assert!(
        state.read_with(cx, |state, _| !ended(state, session)),
        "the link dropped during the turn"
    );
    assert!(!send(&state, session, "not now", cx));
    state.read_with(cx, |state, _| {
        assert_eq!(
            state.conversations[&session].error.as_deref(),
            Some("The machine isn't connected, so the message wasn't sent")
        );
    });
    wait_until(cx, &state, "the turn to end after the reconnect", |state| {
        state.machine_status(&desk(state)) == LinkStatus::Connected && ended(state, session)
    });
    assert_eq!(
        state.read_with(cx, |state, _| state.conversations[&session].error.clone()),
        None,
        "the note that the message wasn't sent goes once the machine is back"
    );
    state.read_with(cx, |state, _| {
        assert_eq!(items(state, session), items(state, reference));
    });
    assert_eq!(
        std::fs::read_to_string(dropped.path().join(".fake-claude-messages"))
            .unwrap()
            .lines()
            .count(),
        1,
        "the message reached Claude once"
    );
}

/// Quits as Quit does: saves, then tears down, which tells each machine.
fn quit(state: Entity<AppState>, idle: bool, cx: &mut gpui::TestAppContext) {
    state.update(cx, |state, cx| {
        assert!(state.reserve_quit(!idle));
        state.save(Vec::new(), Vec::new(), Vec::new(), cx).unwrap();
        state.teardown(cx);
    });
    drop(state);
    cx.run_until_parked();
}

/// The next launch, with the same machine reached the same way.
fn relaunch(dir: &Path, host: &Host, cx: &mut gpui::TestAppContext) -> Entity<AppState> {
    let state = runtime(dir, cx);
    state.update(cx, |state, _| state.test_transport = Some(host.argv()));
    state
}

fn agent_text(state: &AppState, session: u64) -> String {
    state.conversations[&session]
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Item(item) if item.kind == ItemKind::AgentMessage => Some(item.text.clone()),
            _ => None,
        })
        .collect()
}

/// Quit leaves a turn on another machine running. The next launch shows it running,
/// then its end, with the reply whole and once, and the open turn marked as having
/// gone on there rather than interrupted.
#[gpui::test]
fn quit_leaves_a_remote_turn_running_and_the_next_launch_catches_up(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let work = folder(&[("delay-ms", "20")]);
    let session = state.update(cx, |state, cx| {
        let folder = RemoteFolder::new(work.path().to_str().unwrap()).unwrap();
        state
            .create_remote(&desk(state), &folder, WindowId::FIRST, None, cx)
            .unwrap()
            .value
    });
    assert!(send(&state, session, "hello long", cx));
    wait_until(cx, &state, "the turn to stream", |state| {
        agent_text(state, session).contains("word")
    });
    quit(state, false, cx);
    assert!(host.daemon().is_some(), "the machine's host keeps running");

    let state = relaunch(dir.path(), &host, cx);
    state.update(cx, |state, _| state.ensure_history(session));
    wait_until(cx, &state, "the caught-up turn to end", |state| {
        ended(state, session)
    });
    state.read_with(cx, |state, _| {
        let text = agent_text(state, session);
        assert!(text.contains("word200"), "{text}");
        assert_eq!(text.matches("word200").count(), 1, "{text}");
        let rows = &state.conversations[&session].rows;
        assert!(
            rows.iter()
                .any(|row| matches!(row, Row::Note(note) if note.starts_with(REMOTE_CLOSED))),
            "{rows:?}"
        );
        assert!(
            !rows.iter().any(|row| matches!(
                row,
                Row::TurnEnded {
                    status: TurnStatus::Interrupted,
                    ..
                }
            )),
            "{rows:?}"
        );
    });
    assert_eq!(
        std::fs::read_to_string(work.path().join(".fake-claude-messages"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

/// An approval that waits when Keystone quits waits on the machine, and its card is
/// back on the next launch, where answering it finishes the turn.
#[gpui::test]
fn an_approval_waiting_at_quit_is_answerable_after_the_next_launch(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let work = folder(&[("delay-ms", "1")]);
    let session = state.update(cx, |state, cx| {
        let folder = RemoteFolder::new(work.path().to_str().unwrap()).unwrap();
        state
            .create_remote(&desk(state), &folder, WindowId::FIRST, None, cx)
            .unwrap()
            .value
    });
    assert!(send(&state, session, "run the tests", cx));
    wait_until(cx, &state, "the approval", |state| {
        !state.conversations[&session].approvals.is_empty()
    });
    quit(state, false, cx);

    let state = relaunch(dir.path(), &host, cx);
    state.update(cx, |state, _| state.ensure_history(session));
    wait_until(cx, &state, "the approval card to come back", |state| {
        let c = &state.conversations[&session];
        !c.is_loading() && c.approvals.len() == 1 && c.running
    });
    let request = state.read_with(cx, |state, _| {
        state.conversations[&session].approvals[0].request.clone()
    });
    state.update(cx, |state, cx| state.answer(session, request, true, cx));
    wait_until(cx, &state, "the answered turn to end", |state| {
        ended(state, session)
    });
}

/// With the machine set to end its sessions, Quit ends a running turn there. The
/// quit that follows closing the last window never does, whatever the setting.
#[gpui::test]
fn only_a_user_quit_ends_a_remote_turn_on_a_machine_set_to_end_them(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let work = folder(&[("delay-ms", "20")]);
    let session = state.update(cx, |state, cx| {
        let machine = desk(state);
        state.set_machine_end_on_quit(&machine, true, cx).unwrap();
        let folder = RemoteFolder::new(work.path().to_str().unwrap()).unwrap();
        state
            .create_remote(&machine, &folder, WindowId::FIRST, None, cx)
            .unwrap()
            .value
    });
    let streaming = |state: &AppState| agent_text(state, session).contains("word");

    assert!(send(&state, session, "hello long", cx));
    wait_until(cx, &state, "the turn to stream", streaming);
    quit(state, true, cx);
    let state = relaunch(dir.path(), &host, cx);
    state.update(cx, |state, _| state.ensure_history(session));
    wait_until(cx, &state, "the turn the idle quit left to end", |state| {
        ended(state, session)
    });
    state.read_with(cx, |state, _| {
        assert!(agent_text(state, session).contains("word200"));
    });

    assert!(send(&state, session, "hello long again", cx));
    wait_until(cx, &state, "the second turn to stream", |state| {
        agent_text(state, session).matches("word1 ").count() >= 2
    });
    quit(state, false, cx);
    let state = relaunch(dir.path(), &host, cx);
    let failed = |state: &AppState| {
        state.attention(state.session(session).unwrap()) == Some(keystone_core::Attention::Failed)
    };
    assert!(
        !state.read_with(cx, |state, _| failed(state)),
        "before the host reports, the turn isn't called failed"
    );
    state.update(cx, |state, _| state.ensure_history(session));
    wait_until(cx, &state, "the ended turn to arrive", |state| {
        state.conversations.get(&session).is_some_and(|c| {
            !c.is_loading()
                && !c.running
                && c.rows.iter().any(|row| {
                    matches!(
                        row,
                        Row::TurnEnded {
                            status: TurnStatus::Interrupted,
                            ..
                        }
                    )
                })
        })
    });
    assert!(
        !state.read_with(cx, |state, _| failed(state)),
        "the sidebar says what the transcript says: the user's Quit stopped it"
    );
}

/// The folders the kill test shares with the app it kills: its state, its machine's
/// host folder and the session's folder there.
const KILLED_APP: [&str; 3] = [
    "KEYSTONE_TEST_KILLED_STATE",
    "KEYSTONE_TEST_KILLED_HOST",
    "KEYSTONE_TEST_KILLED_WORK",
];

/// The app the kill test starts as a process of its own and kills: it links a
/// machine, starts a 200-word turn there, says so in `streaming`, then keeps applying
/// what arrives and says in `received` once the whole turn has. Run alone it does
/// nothing.
#[gpui::test]
#[ignore = "a process a_kill_between_receiving_and_saving_loses_nothing starts and kills"]
fn the_app_a_kill_test_kills(cx: &mut gpui::TestAppContext) {
    let Some([state_dir, host, work]) = KILLED_APP
        .map(|name| std::env::var_os(name).map(PathBuf::from))
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .and_then(|dirs| <[PathBuf; 3]>::try_from(dirs).ok())
    else {
        return;
    };
    let state = runtime(&state_dir, cx);
    assert_eq!(link(&state, host_argv(&host), "desk", cx), None);
    let session = state.update(cx, |state, cx| {
        let folder = RemoteFolder::new(work.to_str().unwrap()).unwrap();
        state
            .create_remote(&desk(state), &folder, WindowId::FIRST, None, cx)
            .unwrap()
            .value
    });
    assert!(send(&state, session, "hello long", cx));
    wait_until(cx, &state, "the turn to stream", |state| {
        agent_text(state, session).contains("word1")
    });
    std::fs::write(state_dir.join("streaming"), session.to_string()).unwrap();
    loop {
        cx.run_until_parked();
        if state.read_with(cx, |state, _| ended(state, session)) {
            std::fs::write(state_dir.join("received"), "").unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// How many of the session's events the machine's journal still holds whose frame
/// names `kind`.
fn journaled(host: &Path, session: u64, kind: &str) -> i64 {
    let db = rusqlite::Connection::open_with_flags(
        host.join("journal.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row(
        "SELECT COUNT(*) FROM events WHERE session = ?1 AND CAST(frame AS TEXT) LIKE ?2",
        rusqlite::params![session as i64, format!("%\"{kind}\"%")],
        |row| row.get(0),
    )
    .unwrap()
}

/// The session's saved log, as a restore reads it.
fn saved_log(state_dir: &Path, session: u64) -> Vec<keystone_agent::log::Entry> {
    keystone_core::Database::open_read_only(state_dir.join("workspace.sqlite3"))
        .unwrap()
        .read_logs(session)
        .unwrap()
        .iter()
        .filter_map(|payload| serde_json::from_str(payload).ok())
        .collect()
}

/// The "done when" of TOB-319. An app process is killed after it received a remote
/// turn and before it could save the rows from it: another connection holds the
/// workspace database's write lock, so every log write fails and waits. The host
/// must still hold those events, and the next launch must save every word once and
/// in order. Acknowledging what was drained, rather than what was saved, lets the
/// host trim them, and the turn is gone.
#[gpui::test]
fn a_kill_between_receiving_and_saving_loses_nothing(cx: &mut gpui::TestAppContext) {
    let state_dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let work = folder(&[("delay-ms", "10")]);
    let log = std::fs::File::create(state_dir.path().join("killed-app.log")).unwrap();
    let mut app = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "app_state::remote_tests::the_app_a_kill_test_kills",
            "--ignored",
            "--nocapture",
        ])
        .envs(
            KILLED_APP
                .iter()
                .zip([state_dir.path(), host.0.path(), work.path()]),
        )
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let wait_for_file = |name: &str, app: &mut std::process::Child| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !state_dir.path().join(name).exists() {
            assert!(
                app.try_wait().unwrap().is_none(),
                "the app exited before {name}: {}",
                std::fs::read_to_string(state_dir.path().join("killed-app.log")).unwrap()
            );
            assert!(Instant::now() < deadline, "timed out waiting for {name}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for_file("streaming", &mut app);
    let session: u64 = std::fs::read_to_string(state_dir.path().join("streaming"))
        .unwrap()
        .parse()
        .unwrap();
    let lock = rusqlite::Connection::open(state_dir.path().join("workspace.sqlite3")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    wait_for_file("received", &mut app);
    // Long enough for any acknowledgement the app would send to reach the host.
    std::thread::sleep(Duration::from_secs(1));
    app.kill().unwrap();
    app.wait().unwrap();
    drop(lock);
    assert_eq!(
        journaled(host.0.path(), session, "TurnCompleted"),
        1,
        "the host still holds the turn's end, which the killed app never saved"
    );
    assert!(
        journaled(host.0.path(), session, "ItemCompleted") > 0,
        "the host still holds the reply, which the killed app never saved"
    );

    let state = relaunch(state_dir.path(), &host, cx);
    state.update(cx, |state, _| state.ensure_history(session));
    wait_until(cx, &state, "the replay to end the turn", |state| {
        ended(state, session)
    });
    quit(state, false, cx);
    let entries = saved_log(state_dir.path(), session);
    let replies: Vec<&str> = entries
        .iter()
        .filter_map(|entry| match entry {
            keystone_agent::log::Entry::Item { item, .. }
                if item.kind == ItemKind::AgentMessage
                    && item.status == keystone_agent::ItemStatus::Completed =>
            {
                Some(item.text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(replies.len(), 1, "the reply is saved once: {entries:?}");
    let words: Vec<&str> = replies[0]
        .split_whitespace()
        .filter(|word| word.starts_with("word"))
        .collect();
    let expected: Vec<String> = (1..=200).map(|n| format!("word{n}")).collect();
    assert_eq!(words, expected, "every word once and in order");
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(entry, keystone_agent::log::Entry::TurnEnded { .. }))
            .count(),
        1,
        "the turn's end is saved once: {entries:?}"
    );
}

/// What a machine's host loses when its state folder is wiped or `keystone-host` is
/// reinstalled: the daemon, and the journal with every event and session it held.
fn wipe_journal(host: &Host) {
    if let Some(pid) = host.daemon() {
        // SAFETY: kill only signals the test's own daemon.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while host.daemon().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(host.0.path().join(format!("journal.sqlite3{suffix}")));
    }
}

/// Each saved reply's words, and how many turn ends are saved.
fn saved_turns(state_dir: &Path, session: u64) -> (Vec<usize>, usize) {
    let entries = saved_log(state_dir, session);
    let replies: Vec<usize> = entries
        .iter()
        .filter_map(|entry| match entry {
            keystone_agent::log::Entry::Item { item, .. }
                if item.kind == ItemKind::AgentMessage
                    && item.status == keystone_agent::ItemStatus::Completed =>
            {
                let words: Vec<&str> = item
                    .text
                    .split_whitespace()
                    .filter(|word| word.starts_with("word"))
                    .collect();
                let expected: Vec<String> = (1..=words.len()).map(|n| format!("word{n}")).collect();
                assert_eq!(words, expected, "every word once and in order");
                Some(words.len())
            }
            _ => None,
        })
        .collect();
    let ends = entries
        .iter()
        .filter(|entry| matches!(entry, keystone_agent::log::Entry::TurnEnded { .. }))
        .count();
    // Claude's own copy of each message, which comes early in its turn from the
    // host, beside the copy the app saved as it sent it.
    let messages = |local: bool| {
        entries
            .iter()
            .filter(|entry| {
                matches!(entry, keystone_agent::log::Entry::Item { item, .. }
                    if item.kind == ItemKind::UserMessage
                        && item.id.starts_with("local:") == local)
            })
            .count()
    };
    assert_eq!(
        messages(false),
        messages(true),
        "each sent message's echo is saved once: {entries:?}"
    );
    (replies, ends)
}

/// The turn ends saved with the note that the host lost the session's record.
fn lost_notes(state_dir: &Path, session: u64) -> usize {
    saved_log(state_dir, session)
        .iter()
        .filter(|entry| {
            matches!(entry, keystone_agent::log::Entry::TurnEnded { error: Some(error), .. }
                if error.contains("lost its record of this session"))
        })
        .count()
}

/// The next launch, with the machine connected, its sessions reported and `session`'s
/// history loaded.
fn launch_linked(
    dir: &Path,
    argv: Vec<std::ffi::OsString>,
    session: u64,
    cx: &mut gpui::TestAppContext,
) -> Entity<AppState> {
    let state = runtime(dir, cx);
    state.update(cx, |state, _| {
        state.test_transport = Some(argv);
        state.ensure_history(session);
    });
    wait_until(
        cx,
        &state,
        "the machine to connect and report",
        move |state| {
            state
                .links
                .values()
                .all(|link| link.status() == LinkStatus::Connected && link.reports() > 0)
                && state
                    .conversations
                    .get(&session)
                    .is_some_and(|c| !c.is_loading())
        },
    );
    state
}

/// Runs a whole long turn in `session`, whose `n`th reply it is.
fn whole_turn(state: &Entity<AppState>, session: u64, n: usize, cx: &mut gpui::TestAppContext) {
    assert!(send(state, session, "hello long", cx));
    wait_until(cx, state, "the turn to end", move |state| {
        agent_text(state, session).matches("word200").count() == n
            && !state.conversations[&session].running
    });
}

fn new_remote(state: &Entity<AppState>, work: &Path, cx: &mut gpui::TestAppContext) -> u64 {
    state.update(cx, |state, cx| {
        let folder = RemoteFolder::new(work.to_str().unwrap()).unwrap();
        state
            .create_remote(&desk(state), &folder, WindowId::FIRST, None, cx)
            .unwrap()
            .value
    })
}

/// Waits until the machine's host has journaled the end of every turn it runs for
/// `sessions`, with the app closed.
fn host_finishes(host: &Host, sessions: &[u64]) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while sessions
        .iter()
        .any(|session| journaled(host.0.path(), *session, "TurnCompleted") == 0)
    {
        assert!(
            Instant::now() < deadline,
            "the host never finished the turns"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The finding of TOB-319's final check. A machine's journal is wiped after two
/// saved turns, so the database holds a cursor counted in a journal that is gone.
/// The next launch starts a turn and quits while it streams; the host finishes it in
/// its new journal, numbered from 1 again. Comparing the old cursor with those numbers
/// would drop the turn and acknowledge it, losing it for good. Every reply must be
/// saved once, and nothing left running.
#[gpui::test]
fn a_wiped_journal_loses_no_turn_the_next_launches_catch_up(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let work = folder(&[("delay-ms", "1")]);
    let session = new_remote(&state, work.path(), cx);
    whole_turn(&state, session, 1, cx);
    whole_turn(&state, session, 2, cx);
    quit(state, false, cx);
    wipe_journal(&host);

    std::fs::write(work.path().join(".fake-claude-delay-ms"), "20").unwrap();
    let state = launch_linked(dir.path(), host.argv(), session, cx);
    assert!(send(&state, session, "hello long", cx));
    wait_until(cx, &state, "the third turn to stream", move |state| {
        agent_text(state, session).matches("word5 ").count() == 3
    });
    quit(state, false, cx);
    host_finishes(&host, &[session]);

    let state = launch_linked(dir.path(), host.argv(), session, cx);
    wait_until(cx, &state, "the caught-up turn to end", move |state| {
        agent_text(state, session).matches("word200").count() == 3
            && !state.conversations[&session].running
    });
    let last_turn = state.read_with(cx, |state, _| state.session(session).unwrap().last_turn());
    quit(state, false, cx);
    assert_eq!(saved_turns(dir.path(), session), (vec![200, 200, 200], 3));
    assert_ne!(
        last_turn,
        Some(keystone_core::LastTurn::Running),
        "nothing stays Working"
    );
}

/// The turn ends saved with an error: a turn the host or the app ended because its
/// process or its record was lost.
fn failed_ends(state_dir: &Path, session: u64) -> Vec<String> {
    saved_log(state_dir, session)
        .into_iter()
        .filter_map(|entry| match entry {
            keystone_agent::log::Entry::TurnEnded { error, .. } => error,
            _ => None,
        })
        .collect()
}

/// A journal wiped while the app is linked and a turn streams: the daemon dies with
/// its provider, and the link reconnects to a new daemon with a new journal. Either
/// the new daemon ends the turn, when the old provider's watcher file is still there
/// to say it ran, or the app does, when nothing there remembers it. Either way it ends
/// once, and the session takes the next message.
#[gpui::test]
fn a_journal_wiped_while_linked_ends_the_running_turn_once(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let work = folder(&[("delay-ms", "20")]);
    let session = new_remote(&state, work.path(), cx);
    assert!(send(&state, session, "hello long", cx));
    wait_until(cx, &state, "the turn to stream", move |state| {
        agent_text(state, session).contains("word5 ")
    });
    let old = state.read_with(cx, |state, _| state.links[&desk(state)].transport_pid());
    // The daemon's death ends the transport, and the link reconnects to a new one.
    wipe_journal(&host);
    wait_until(cx, &state, "the lost turn to end", move |state| {
        let link = &state.links[&desk(state)];
        !state.conversations[&session].running
            && link.status() == LinkStatus::Connected
            && link.transport_pid().is_some_and(|pid| Some(pid) != old)
    });
    std::fs::write(work.path().join(".fake-claude-delay-ms"), "1").unwrap();
    whole_turn(&state, session, 1, cx);
    quit(state, false, cx);
    let ends = failed_ends(dir.path(), session);
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert!(
        ends[0].contains("keystone-host restarted") || ends[0].contains("lost its record"),
        "{ends:?}"
    );
    assert_eq!(saved_turns(dir.path(), session), (vec![200], 2));
}

/// A transport whose host folder is the one `file` names, so a test can point the
/// machine's SSH name at another machine while the app is linked.
fn switchable_argv(file: &Path) -> Vec<std::ffi::OsString> {
    shell(&format!(
        "dir=$(cat '{}'); exec env KEYSTONE_HOST_DIR=\"$dir\" HOME=\"$dir\" KEYSTONE_CLAUDE_PATH='{}' '{}' attach",
        file.display(),
        binary("keystone-fake-claude").display(),
        binary("keystone-host").display()
    ))
}

/// While the app is linked and a turn streams, the machine's SSH name starts reaching
/// another machine, with its own journal, which never heard of the session. The turn
/// can't end there, so the app ends it once with a note that says so, and doesn't add
/// another at the next launch.
#[gpui::test]
fn a_journal_replaced_while_linked_ends_the_lost_turn_once(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (first, other) = (Host::new(), Host::new());
    let target = dir.path().join("reaches");
    std::fs::write(&target, first.0.path().to_str().unwrap()).unwrap();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, switchable_argv(&target), "desk", cx), None);
    let work = folder(&[("delay-ms", "20")]);
    let session = new_remote(&state, work.path(), cx);
    assert!(send(&state, session, "hello long", cx));
    wait_until(cx, &state, "the turn to stream", move |state| {
        agent_text(state, session).contains("word5 ")
    });
    std::fs::write(&target, other.0.path().to_str().unwrap()).unwrap();
    let transport = state.read_with(cx, |state, _| {
        state.links[&desk(state)].transport_pid().unwrap() as i32
    });
    // SAFETY: kill only signals the link's own transport.
    unsafe { libc::kill(transport, libc::SIGKILL) };
    wait_until(cx, &state, "the lost turn to end", move |state| {
        !state.conversations[&session].running
            && state.machine_status(&desk(state)) == LinkStatus::Connected
    });
    state.read_with(cx, |state, _| {
        assert!(
            state.conversations[&session]
                .rows
                .iter()
                .any(|row| matches!(
                    row,
                    Row::TurnEnded { error: Some(error), .. } if error.contains("lost its record")
                )),
            "the transcript says why"
        );
    });
    quit(state, false, cx);
    assert_eq!(lost_notes(dir.path(), session), 1);

    let state = launch_linked(dir.path(), switchable_argv(&target), session, cx);
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    quit(state, false, cx);
    assert_eq!(
        lost_notes(dir.path(), session),
        1,
        "ended once, not again at the next launch"
    );
}

/// A journal wiped between two launches, with no turn in the launch between: the
/// cursor saved in the old journal must not hold back the turn after.
#[gpui::test]
fn a_journal_wiped_between_launches_holds_nothing_back(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let work = folder(&[("delay-ms", "1")]);
    let session = new_remote(&state, work.path(), cx);
    whole_turn(&state, session, 1, cx);
    whole_turn(&state, session, 2, cx);
    quit(state, false, cx);
    wipe_journal(&host);
    let state = launch_linked(dir.path(), host.argv(), session, cx);
    quit(state, false, cx);
    let state = launch_linked(dir.path(), host.argv(), session, cx);
    whole_turn(&state, session, 3, cx);
    quit(state, false, cx);
    assert_eq!(saved_turns(dir.path(), session), (vec![200, 200, 200], 3));
    assert_eq!(
        lost_notes(dir.path(), session),
        0,
        "no turn was running to lose"
    );
}

/// A wiped journal that has grown past the old cursor before the app relaunches:
/// three sessions stream at once in the new journal and the app quits, so the next
/// launch catches up with numbers above and below the cursor the first session saved
/// in the old journal, the first session's reply and turn end among those below it.
/// Not one event may be dropped.
#[gpui::test]
fn a_new_journal_grown_past_an_old_cursor_drops_nothing(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let first_work = folder(&[("delay-ms", "1")]);
    let first = new_remote(&state, first_work.path(), cx);
    whole_turn(&state, first, 1, cx);
    whole_turn(&state, first, 2, cx);
    quit(state, false, cx);
    wipe_journal(&host);

    // The first session's turn ends early in the new journal, below its old cursor,
    // while the others carry the journal past it.
    std::fs::write(first_work.path().join(".fake-claude-delay-ms"), "5").unwrap();
    let state = launch_linked(dir.path(), host.argv(), first, cx);
    let works = [folder(&[("delay-ms", "20")]), folder(&[("delay-ms", "20")])];
    let mut sessions = vec![first];
    for work in &works {
        let session = new_remote(&state, work.path(), cx);
        sessions.push(session);
    }
    for &session in &sessions {
        assert!(send(&state, session, "hello long", cx));
    }
    let streaming = sessions.clone();
    wait_until(cx, &state, "every turn to stream", move |state| {
        streaming
            .iter()
            .all(|session| agent_text(state, *session).contains("word5 "))
    });
    quit(state, false, cx);
    host_finishes(&host, &sessions);

    let state = launch_linked(dir.path(), host.argv(), first, cx);
    state.update(cx, |state, _| {
        for &session in &sessions {
            state.ensure_history(session);
        }
    });
    let ended = sessions.clone();
    wait_until(cx, &state, "every caught-up turn to end", move |state| {
        ended.iter().all(|session| {
            let replies = if *session == ended[0] { 3 } else { 1 };
            state
                .conversations
                .get(session)
                .is_some_and(|c| !c.is_loading() && !c.running)
                && agent_text(state, *session).matches("word200").count() == replies
        })
    });
    quit(state, false, cx);
    assert_eq!(saved_turns(dir.path(), first), (vec![200, 200, 200], 3));
    for &session in &sessions[1..] {
        assert_eq!(saved_turns(dir.path(), session), (vec![200], 1));
    }
}

/// The SSH name reaches another machine, with its own journal: the turn the first
/// machine was running at Quit can't be caught up there, so it is ended once with
/// the note, and the reply saved before stays once.
#[gpui::test]
fn a_destination_that_now_reaches_another_machine_ends_the_lost_turn(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let first = Host::new();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, first.argv(), "desk", cx), None);
    let work = folder(&[("delay-ms", "1")]);
    let session = new_remote(&state, work.path(), cx);
    whole_turn(&state, session, 1, cx);
    std::fs::write(work.path().join(".fake-claude-delay-ms"), "20").unwrap();
    assert!(send(&state, session, "hello long", cx));
    wait_until(cx, &state, "the second turn to stream", move |state| {
        agent_text(state, session).matches("word5 ").count() == 2
    });
    quit(state, false, cx);

    let other = Host::new();
    let state = launch_linked(dir.path(), other.argv(), session, cx);
    wait_until(cx, &state, "the lost turn to end", move |state| {
        state.session(session).unwrap().last_turn() != Some(keystone_core::LastTurn::Running)
    });
    quit(state, false, cx);
    assert_eq!(lost_notes(dir.path(), session), 1);
    assert_eq!(saved_turns(dir.path(), session).0, vec![200]);
}

/// A turn saved before a launch whose host never heard it acknowledged: here a
/// transport that drops every `ack` the app sends. The next launch's replay holds the
/// whole turn again, and each event at or below the session's saved cursor must be
/// dropped, not saved a second time.
#[gpui::test]
fn a_saved_turn_replayed_at_the_next_launch_is_saved_once(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let quoted: Vec<String> = host
        .argv()
        .iter()
        .map(|arg| format!("'{}'", arg.to_str().unwrap()))
        .collect();
    let deaf = shell(&format!(
        "grep --line-buffered -v '\"t\":\"ack\"' | {}",
        quoted.join(" ")
    ));
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, deaf, "desk", cx), None);
    let work = folder(&[("delay-ms", "1")]);
    let session = new_remote(&state, work.path(), cx);
    whole_turn(&state, session, 1, cx);
    quit(state, false, cx);
    assert_eq!(saved_turns(dir.path(), session), (vec![200], 1));
    assert_eq!(
        journaled(host.0.path(), session, "TurnCompleted"),
        1,
        "the host never heard the turn acknowledged"
    );

    let state = launch_linked(dir.path(), host.argv(), session, cx);
    for _ in 0..50 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    quit(state, false, cx);
    assert_eq!(saved_turns(dir.path(), session), (vec![200], 1));
    assert_eq!(
        journaled(host.0.path(), session, "TurnCompleted"),
        0,
        "acknowledged once the replay was dropped as saved"
    );
}

/// Copies the machine's journal files to `to`, or back from it, with its daemon
/// stopped so the copy is whole.
fn copy_journal(host: &Host, to: &Path, back: bool) {
    if let Some(pid) = host.daemon() {
        // SAFETY: kill only signals the test's own daemon.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while host.daemon().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    for suffix in ["", "-wal", "-shm"] {
        let name = format!("journal.sqlite3{suffix}");
        let (from, into) = if back {
            (to.join(&name), host.0.path().join(&name))
        } else {
            (host.0.path().join(&name), to.join(&name))
        };
        let _ = std::fs::remove_file(&into);
        if from.exists() {
            std::fs::copy(&from, &into).unwrap();
        }
    }
}

/// The machine's journal is restored from a backup taken before the last turn, so it
/// keeps its identity while its numbers go back below the cursor the app saved. The
/// next turn, quit while it streams and caught up at the launch after, is saved whole
/// and once, and nothing is refused. This proves the outcome, not which rule gives it:
/// here the link's own rule for a journal behind a place is enough. That the host
/// takes a new identity, which makes it last across launches, is
/// `a_host_whose_journal_went_back_takes_a_new_identity` in `tests/remote_host.rs`, and
/// the cursor model's `no-rotate` break shows what it prevents.
#[gpui::test]
fn a_journal_restored_from_a_backup_saves_every_turn_once(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let backup = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let work = folder(&[("delay-ms", "1")]);
    let session = new_remote(&state, work.path(), cx);
    whole_turn(&state, session, 1, cx);
    quit(state, false, cx);
    copy_journal(&host, backup.path(), false);

    let state = launch_linked(dir.path(), host.argv(), session, cx);
    whole_turn(&state, session, 2, cx);
    quit(state, false, cx);
    copy_journal(&host, backup.path(), true);

    std::fs::write(work.path().join(".fake-claude-delay-ms"), "20").unwrap();
    let state = launch_linked(dir.path(), host.argv(), session, cx);
    assert!(send(&state, session, "hello long", cx));
    wait_until(cx, &state, "the third turn to stream", move |state| {
        agent_text(state, session).matches("word5 ").count() == 3
    });
    quit(state, false, cx);
    host_finishes(&host, &[session]);

    let state = launch_linked(dir.path(), host.argv(), session, cx);
    wait_until(cx, &state, "the caught-up turn to end", move |state| {
        agent_text(state, session).matches("word200").count() == 3
            && !state.conversations[&session].running
    });
    quit(state, false, cx);
    assert_eq!(saved_turns(dir.path(), session), (vec![200, 200, 200], 3));
    assert_eq!(
        lost_notes(dir.path(), session),
        0,
        "no turn was running to lose"
    );
}

/// The machine's journal's latest number for any client.
fn journal_latest(host: &Path) -> i64 {
    let db = rusqlite::Connection::open_with_flags(
        host.join("journal.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row("SELECT COALESCE(MAX(latest), 0) FROM clients", [], |row| {
        row.get(0)
    })
    .unwrap()
}

/// Where each commit of the write-ahead log ends, oldest first, for the frames of its
/// current generation (a frame whose "database size" field isn't 0 ends a commit).
fn wal_commit_ends(wal: &[u8]) -> Vec<usize> {
    let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let page = be(&wal[8..12]);
    let salt = &wal[16..24];
    let frame = 24 + page;
    let (mut at, mut ends) = (32, vec![32]);
    while at + frame <= wal.len() && &wal[at + 8..at + 16] == salt {
        at += frame;
        if be(&wal[at - frame + 4..at - frame + 8]) != 0 {
            ends.push(at);
        }
    }
    ends
}

/// A power loss on the machine during a turn: the daemon and its provider die, as in a
/// reboot, and the commits the journal's setting doesn't sync are gone. Under `NORMAL`
/// the last few commits may never have reached the disk, which here takes the last
/// `lost` of them out of the write-ahead log. Under `FULL` a commit is on disk before it
/// returns, so a power loss takes nothing the daemon committed, and nothing is cut.
fn host_power_loss(host: &Host, work: &Path, lost: i64) {
    if let Some(pid) = host.daemon() {
        // SAFETY: kill only signals the test's own daemon.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let pids = std::fs::read_to_string(work.join(".fake-claude-pids")).unwrap();
    for pid in pids
        .split_whitespace()
        .filter_map(|pid| pid.parse::<i32>().ok())
    {
        // SAFETY: kill only signals the test's own scripted Claude.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while host.daemon().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if lost == 0 {
        return;
    }
    let back_to = journal_latest(host.0.path()) - lost;
    let wal_path = host.0.path().join("journal.sqlite3-wal");
    let wal = std::fs::read(&wal_path).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    for cut in wal_commit_ends(&wal).into_iter().rev() {
        std::fs::copy(
            host.0.path().join("journal.sqlite3"),
            scratch.path().join("journal.sqlite3"),
        )
        .unwrap();
        std::fs::write(scratch.path().join("journal.sqlite3-wal"), &wal[..cut]).unwrap();
        let _ = std::fs::remove_file(scratch.path().join("journal.sqlite3-shm"));
        if journal_latest(scratch.path()) <= back_to {
            std::fs::write(&wal_path, &wal[..cut]).unwrap();
            let _ = std::fs::remove_file(host.0.path().join("journal.sqlite3-shm"));
            return;
        }
    }
    panic!("the journal never went back to {back_to}");
}

/// What this proves, and what it doesn't: the host folder here is on `/tmp`, which is
/// in memory on these machines, and under `FULL` [`host_power_loss`] cuts nothing, so
/// this is a test of a killed daemon and its restart events. It is no durability proof;
/// `the_journal_syncs_every_commit` in `journal.rs` guards the setting that is. Set back
/// to `NORMAL`, it cuts two or three commits and fails, as the build before did.
///
/// A power loss on the machine mid-turn, with the link down while it reboots. The
/// daemon that starts next ends the turn with its restart events, numbered after every
/// event the app has seen, so the app applies them, the turn ends once, and nothing
/// stays Working, then or after a relaunch. On the build before, the journal committed
/// with `NORMAL`, a power loss could take back two or three commits the app had already
/// saved, and the restart events took those numbers: the app dropped the turn's end as
/// saved, and the host trimmed it.
fn a_power_loss_on_the_machine_loses_no_turn_end(
    lost_under_normal: i64,
    cx: &mut gpui::TestAppContext,
) {
    let lost = if keystone_agent::remote::JOURNAL_SYNCHRONOUS == "FULL" {
        0
    } else {
        lost_under_normal
    };
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let down = dir.path().join("down");
    let quoted: Vec<String> = host
        .argv()
        .iter()
        .map(|arg| format!("'{}'", arg.to_str().unwrap()))
        .collect();
    let argv = shell(&format!(
        "if [ -e '{}' ]; then echo 'Network is unreachable' >&2; exit 255; fi; exec {}",
        down.display(),
        quoted.join(" ")
    ));
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, argv.clone(), "desk", cx), None);
    let work = folder(&[("delay-ms", "1")]);
    let session = new_remote(&state, work.path(), cx);
    assert!(send(&state, session, "hello long", cx));
    std::fs::write(work.path().join(".fake-claude-delay-ms"), "2500").unwrap();
    wait_until(cx, &state, "the reply to start", move |state| {
        agent_text(state, session).contains("**Mode:**")
    });
    for _ in 0..100 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    let old = state.read_with(cx, |state, _| state.links[&desk(state)].transport_pid());
    std::fs::write(&down, "").unwrap();
    host_power_loss(&host, work.path(), lost);
    std::fs::remove_file(&down).unwrap();
    wait_until(
        cx,
        &state,
        "the turn to end after the reboot",
        move |state| {
            let link = &state.links[&desk(state)];
            link.status() == LinkStatus::Connected
                && link.transport_pid().is_some_and(|pid| Some(pid) != old)
                && !state.conversations[&session].running
        },
    );
    quit(state, false, cx);
    let ends = failed_ends(dir.path(), session);
    assert_eq!(ends.len(), 1, "the turn ends once: {ends:?}");
    let state = launch_linked(dir.path(), argv, session, cx);
    for _ in 0..100 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    let (running, last_turn) = state.read_with(cx, |state, _| {
        let last = state.session(session).unwrap().last_turn();
        (state.conversations[&session].running, last)
    });
    quit(state, false, cx);
    assert!(!running, "the turn isn't running after the relaunch");
    assert_ne!(
        last_turn,
        Some(keystone_core::LastTurn::Running),
        "nothing stays Working"
    );
    assert_eq!(
        failed_ends(dir.path(), session).len(),
        1,
        "and it ends once"
    );
}

#[gpui::test]
fn a_power_loss_that_would_take_two_commits_loses_no_turn_end(cx: &mut gpui::TestAppContext) {
    a_power_loss_on_the_machine_loses_no_turn_end(2, cx);
}

#[gpui::test]
fn a_power_loss_that_would_take_three_commits_loses_no_turn_end(cx: &mut gpui::TestAppContext) {
    a_power_loss_on_the_machine_loses_no_turn_end(3, cx);
}

/// `argv` with every line the app sends copied to `copy` on the way to the host.
fn teed(argv: &[std::ffi::OsString], copy: &Path) -> Vec<std::ffi::OsString> {
    let quoted: Vec<String> = argv
        .iter()
        .map(|arg| format!("'{}'", arg.to_string_lossy().replace('\'', "'\\''")))
        .collect();
    shell(&format!(
        "tee -a '{}' | {}",
        copy.display(),
        quoted.join(" ")
    ))
}

fn send_images(
    state: &Entity<AppState>,
    session: u64,
    text: &str,
    attachments: Vec<keystone_core::Attachment>,
    cx: &mut gpui::TestAppContext,
) -> bool {
    state.update(cx, |state, cx| {
        state.send_message(
            ComposerDraft {
                session,
                text: text.into(),
                attachments,
            },
            cx,
        )
    })
}

/// A Codex session on a login on the machine looks and behaves like a local one: its
/// reply streams and ends, the host runs Codex there with that login's own home, and
/// nothing runs here. An image it sends twice goes to the host once, by its hash, and
/// Codex reads the same bytes both times.
#[gpui::test]
fn a_codex_session_on_a_login_there_sends_an_image_once(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let receipts = tempfile::tempdir().unwrap();
    let (env, images, sent) = (
        receipts.path().join("env"),
        receipts.path().join("images"),
        receipts.path().join("sent"),
    );
    let argv = teed(
        &host.argv_with(&[
            ("KEYSTONE_FAKE_ENV_RECEIPT", &env),
            ("KEYSTONE_FAKE_IMAGE_RECEIPT", &images),
            ("KEYSTONE_FAKE_CODEX_DELAY_MS", Path::new("1")),
        ]),
        &sent,
    );
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, argv, "desk", cx), None);
    let work = tempfile::tempdir().unwrap();
    let session = new_remote(&state, work.path(), cx);
    let home = host.0.path().join("logins/codex-work");
    let login = state.update(cx, |state, cx| {
        let machine = desk(state);
        let login = state
            .add_remote_account(
                Provider::Codex,
                "Work",
                home.to_str().unwrap(),
                &machine,
                cx,
            )
            .unwrap()
            .value;
        state
            .set_provider_account(session, Provider::Codex, Some(login.clone()), cx)
            .unwrap();
        login
    });
    assert!(!home.exists(), "nothing here made the login's home");
    let picture = state.read_with(cx, |state, _| {
        state
            .attachments
            .import_bytes(b"\x89PNG\r\n\x1a\nremote picture", "picture.png".into())
            .unwrap()
    });
    for (n, text) in [(1, "look at this"), (2, "and again")] {
        assert!(send_images(
            &state,
            session,
            text,
            vec![picture.clone()],
            cx
        ));
        wait_until(cx, &state, "the Codex reply to end", move |state| {
            agent_text(state, session)
                .matches("Scripted Codex reply")
                .count()
                == n
                && !state.conversations[&session].running
        });
    }
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert_eq!(saved.provider(), Provider::Codex);
        assert_eq!(saved.account(), Some(&login));
        assert!(
            state.codex.is_empty() && state.claude.is_empty(),
            "nothing ran here"
        );
        let text = agent_text(state, session);
        assert!(
            text.contains("look at this") && text.contains("and again"),
            "{text}"
        );
    });
    assert!(home.is_dir(), "the host made the login's home there");
    let launched: Vec<serde_json::Value> = std::fs::read_to_string(&env)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|process| {
            process["provider"] == "codex" && process["argv"] != serde_json::json!(["--version"])
        })
        .collect();
    assert!(!launched.is_empty());
    for process in &launched {
        assert_eq!(process["CODEX_HOME"].as_str(), home.to_str(), "{process}");
    }
    let lines = std::fs::read_to_string(&sent).unwrap();
    assert_eq!(
        lines
            .lines()
            .filter(|line| line.contains(r#""op":"put_attachment""#))
            .count(),
        1,
        "the image went once"
    );
    let received = std::fs::read_to_string(&images).unwrap();
    assert_eq!(
        received
            .lines()
            .filter(|line| line.contains(&picture.sha256))
            .count(),
        2,
        "Codex read the image's bytes both times: {received}"
    );
}

/// A launch makes a connection for every provider and login the machine's sessions
/// run on before the link reads a frame, so a Codex turn that went on while Keystone
/// was closed is held for its session rather than waiting unclaimed.
#[gpui::test]
fn the_next_launch_catches_up_a_codex_turn_on_the_machine(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let argv = host.argv_with(&[("KEYSTONE_FAKE_CODEX_DELAY_MS", Path::new("15"))]);
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, argv.clone(), "desk", cx), None);
    let work = tempfile::tempdir().unwrap();
    let session = new_remote(&state, work.path(), cx);
    state.update(cx, |state, cx| {
        state.set_provider(session, Provider::Codex, cx).unwrap();
    });
    assert!(send(&state, session, "hello", cx));
    wait_until(cx, &state, "the reply to stream", |state| {
        agent_text(state, session).contains("Scripted")
    });
    quit(state, false, cx);

    let state = launch_linked(dir.path(), argv, session, cx);
    state.read_with(cx, |state, _| {
        let machine = desk(state);
        assert!(
            state.remote.contains_key(&(machine, Provider::Codex, None)),
            "the Codex route was registered at launch"
        );
    });
    wait_until(cx, &state, "the caught-up Codex turn to end", |state| {
        ended(state, session) && agent_text(state, session).contains("stopping can be checked")
    });
    state.read_with(cx, |state, _| {
        let text = agent_text(state, session);
        assert_eq!(text.matches("Scripted Codex reply").count(), 1, "{text}");
    });
}
