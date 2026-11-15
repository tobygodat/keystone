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
    let too_new = format!(
        "speaks protocol 99, and Keystone needs {}",
        remote::PROTOCOL
    );
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
            too_new.as_str(),
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
    let refused = format!("Not sent. {missing} isn't a folder on desk");
    wait_until(cx, &state, "the host to refuse B's folder", |state| {
        state.conversations[&second].error.as_deref() == Some(refused.as_str())
    });

    state.read_with(cx, |state, _| {
        for session in [first, second] {
            assert!(state.checkpoints(session).is_none(), "no checkpoint read");
            // Its catalog is the machine's, asked there.
            assert!(matches!(
                state
                    .catalog_key(session, Provider::Claude)
                    .map(|key| key.place),
                Some(CatalogPlace::Remote(..))
            ));
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
    // host, beside the copy the app saved as it sent it. The app's copy is logged
    // again under its ID once the host takes it, which replaces it on restore.
    let messages: Vec<&keystone_agent::Item> = entries
        .iter()
        .filter_map(|entry| match entry {
            keystone_agent::log::Entry::Item { item, .. } if item.kind == ItemKind::UserMessage => {
                Some(item)
            }
            _ => None,
        })
        .collect();
    let echoes = messages
        .iter()
        .filter(|item| !item.id.starts_with("local:"))
        .count();
    let sent: std::collections::BTreeSet<_> = messages
        .iter()
        .filter(|item| item.id.starts_with("local:"))
        .map(|item| &item.id)
        .collect();
    assert_eq!(
        echoes,
        sent.len(),
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

/// A message the machine refuses because its image is gone from the host's cache stays
/// in its bubble, "Not sent", and the composer is never filled. Send again sends it
/// under its own ID, uploading the image again, and it runs once.
#[gpui::test]
fn a_refused_message_with_an_image_sent_again_uploads_it_and_runs_once(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let receipts = tempfile::tempdir().unwrap();
    let sent = receipts.path().join("sent");
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, teed(&host.argv(), &sent), "desk", cx), None);
    let work = folder(&[]);
    let session = new_remote(&state, work.path(), cx);
    let picture = state.read_with(cx, |state, _| {
        state
            .attachments
            .import_bytes(
                b"\x89PNG\r\n\x1a\na picture the host loses",
                "picture.png".into(),
            )
            .unwrap()
    });
    let uploads = || {
        std::fs::read_to_string(&sent)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(r#""op":"put_attachment""#))
            .count()
    };
    let replies = |n: usize| {
        move |state: &AppState| {
            agent_text(state, session)
                .matches("Claude test reply")
                .count()
                == n
                && !state.conversations[&session].running
        }
    };
    assert!(send_images(
        &state,
        session,
        "first",
        vec![picture.clone()],
        cx
    ));
    wait_until(cx, &state, "the first reply", replies(1));
    std::fs::remove_file(
        host.0
            .path()
            .join(format!("attachments/{}.png", picture.sha256)),
    )
    .unwrap();

    assert!(send_images(
        &state,
        session,
        "second",
        vec![picture.clone()],
        cx
    ));
    wait_until(cx, &state, "the refusal", |state| {
        state.conversations[&session]
            .error
            .as_deref()
            .is_some_and(|error| {
                error == "Not sent. An image in this message is no longer on desk. Send it again to upload it"
            })
    });
    let refused = state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert!(saved.draft().is_empty(), "the composer is never filled");
        assert!(saved.draft_attachments().is_empty());
        let refused = saved.pending().iter().find(|p| p.refused).unwrap().clone();
        assert_eq!(refused.attachments, [picture.clone()]);
        let conversation = &state.conversations[&session];
        assert!(!conversation.running);
        let last = conversation.rows.iter().rev().find_map(|row| match row {
            Row::Item(item) if item.kind == ItemKind::UserMessage => Some(item.clone()),
            _ => None,
        });
        let last = last.unwrap();
        assert_eq!(
            (last.text.as_str(), last.status),
            ("second", keystone_agent::ItemStatus::Failed),
            "its bubble says it wasn't sent"
        );
        assert_eq!(last.id, refused.id);
        refused.id
    });

    // Send again: the same message, under its own ID.
    assert!(state.update(cx, |state, cx| state.send_again(session, &refused, cx)));
    wait_until(cx, &state, "the retry's reply", replies(2));
    assert_eq!(uploads(), 2, "the retry sent the image again");
    assert_eq!(
        std::fs::read_to_string(&sent)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(&format!(r#""send":"{refused}""#)))
            .count(),
        2,
        "both sends carried the one ID"
    );
    assert_eq!(
        std::fs::read_to_string(work.path().join(".fake-claude-messages"))
            .unwrap()
            .lines()
            .count(),
        2,
        "only the two that went reached Claude"
    );

    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert!(saved.pending().is_empty());
        let second: Vec<_> = bubbles(state, session)
            .into_iter()
            .filter(|(text, _)| text == "second")
            .collect();
        assert_eq!(
            second,
            [("second".into(), keystone_agent::ItemStatus::Completed)]
        );
    });

    // After a relaunch the message reads as sent, once.
    quit(state, false, cx);
    let state = relaunch(dir.path(), &host, cx);
    state.update(cx, |state, _| state.ensure_history(session));
    wait_until(cx, &state, "the history", |state| {
        state
            .conversations
            .get(&session)
            .is_some_and(|c| !c.is_loading())
    });
    state.read_with(cx, |state, _| {
        let failed: Vec<&str> = state.conversations[&session]
            .rows
            .iter()
            .filter(|row| crate::conversation::visible(row))
            .filter_map(|row| match row {
                Row::Item(item) if item.status == keystone_agent::ItemStatus::Failed => {
                    Some(item.text.as_str())
                }
                _ => None,
            })
            .collect();
        assert!(failed.is_empty(), "{failed:?}");
    });
}

/// A session on another machine lists that machine's models for its provider and
/// login, from the host's catalog, and runs the one chosen. The machine's usage is
/// kept as that login's on that machine, never as this machine's.
#[gpui::test]
fn a_remote_session_lists_its_machines_models_and_runs_the_chosen_one(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let work = folder(&[]);
    let claude = new_remote(&state, work.path(), cx);
    let codex = new_remote(&state, work.path(), cx);
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
            .set_provider_account(codex, Provider::Codex, Some(login.clone()), cx)
            .unwrap();
        state.request_catalog(claude, Provider::Claude, cx);
        state.request_catalog(codex, Provider::Codex, cx);
        login
    });
    let models = |state: &AppState, session: u64, provider: Provider| -> Vec<String> {
        match state.model_selection(session, provider).choices {
            Choices::Ready(models) => models.iter().map(|m| m.model.clone()).collect(),
            _ => Vec::new(),
        }
    };
    wait_until(cx, &state, "both machines' catalogs", |state| {
        !models(state, claude, Provider::Claude).is_empty()
            && !models(state, codex, Provider::Codex).is_empty()
    });
    state.read_with(cx, |state, _| {
        let listed = models(state, claude, Provider::Claude);
        assert!(listed.contains(&"fake-haiku".to_owned()), "{listed:?}");
        let listed = models(state, codex, Provider::Codex);
        assert!(listed.contains(&"fake-fast".to_owned()), "{listed:?}");
        assert!(
            state.claude.is_empty() && state.codex.is_empty(),
            "nothing asked here"
        );
    });
    state.update(cx, |state, cx| {
        let choose = |model: &str| ModelChoice {
            model: Some(model.into()),
            ..ModelChoice::default()
        };
        state.set_choice(claude, choose("fake-haiku"), cx).unwrap();
        state.set_choice(codex, choose("fake-fast"), cx).unwrap();
    });
    assert!(send(&state, claude, "which model", cx));
    assert!(send(&state, codex, "which model, usage 85", cx));
    wait_until(cx, &state, "both replies", |state| {
        ended(state, claude) && ended(state, codex)
    });
    state.read_with(cx, |state, _| {
        let text = agent_text(state, claude);
        assert!(text.contains("**Model:** `fake-haiku`"), "{text}");
        let text = agent_text(state, codex);
        assert!(text.contains("**Model:** `fake-fast`"), "{text}");
        let machine = desk(state);
        let usage = state.usage(Some(&machine), Provider::Codex, Some(&login));
        assert!(
            usage.is_some_and(|u| !u.readings.is_empty()),
            "the machine's usage is kept"
        );
        assert!(state.usage(None, Provider::Codex, Some(&login)).is_none());
        assert!(
            state.usage(None, Provider::Codex, None).is_none(),
            "never as this machine's"
        );
    });
}

/// A login whose home the machine refuses, here a folder inside desk's default
/// `~/.codex`, leaves no dead end. The refusal says how to go on, the session's
/// provider and login can be chosen again, and Send again sends the same message, under
/// its own ID, through the other login, once. Moving the refused login to a folder of
/// its own makes it work too.
#[gpui::test]
fn a_session_whose_login_the_machine_refused_picks_another_and_sends(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let default = host.0.path().join(".codex");
    std::fs::create_dir_all(default.join("sub")).unwrap();
    let work = tempfile::tempdir().unwrap();
    let session = new_remote(&state, work.path(), cx);
    let (sneaky, own) = state.update(cx, |state, cx| {
        let machine = desk(state);
        let mut add = |label: &str, home: &Path| {
            state
                .add_remote_account(Provider::Codex, label, home.to_str().unwrap(), &machine, cx)
                .unwrap()
                .value
        };
        let sneaky = add("Sneaky", &default.join("sub"));
        let own = add("Own", &host.0.path().join("logins/own"));
        state
            .set_provider_account(session, Provider::Codex, Some(sneaky.clone()), cx)
            .unwrap();
        (sneaky, own)
    });
    assert!(send(&state, session, "hello there", cx));
    wait_until(cx, &state, "the refusal", |state| {
        state.conversations[&session]
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("Not sent."))
    });
    state.read_with(cx, |state, _| {
        let error = state.conversations[&session].error.clone().unwrap();
        assert!(error.contains("overlaps desk's default home"), "{error}");
        assert!(error.ends_with(WAY_OUT), "{error}");
        let saved = state.session(session).unwrap();
        assert!(!saved.started(), "a refused send fixes nothing");
        assert_eq!(saved.last_turn(), None, "and starts no turn");
        assert!(saved.draft().is_empty(), "the composer is never filled");
        assert!(!state.model_selection(session, Provider::Codex).started);
    });
    let refused = state.read_with(cx, |state, _| {
        state.session(session).unwrap().pending()[0].id.clone()
    });

    // Another login, then Send again: the same message goes, once.
    state.update(cx, |state, cx| {
        state
            .set_provider_account(session, Provider::Codex, Some(own.clone()), cx)
            .unwrap();
        assert!(state.send_again(session, &refused, cx));
    });
    wait_until(cx, &state, "the reply on the other login", |state| {
        ended(state, session)
    });
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert!(saved.started());
        assert_eq!(saved.account(), Some(&own));
        assert!(saved.pending().is_empty());
        assert_eq!(
            bubbles(state, session),
            [("hello there".into(), keystone_agent::ItemStatus::Completed)]
        );
        let text = agent_text(state, session);
        assert_eq!(text.matches("You wrote: hello there").count(), 1, "{text}");
        assert!(state.conversations[&session].error.is_none());
    });

    // The refused login, given its own folder, goes too.
    let second = new_remote(&state, work.path(), cx);
    let moved = host.0.path().join("logins/sneaky");
    state.update(cx, |state, cx| {
        state
            .set_provider_account(second, Provider::Codex, Some(sneaky.clone()), cx)
            .unwrap();
        state
            .set_remote_account_home(&sneaky, moved.to_str().unwrap(), cx)
            .unwrap();
    });
    assert!(send(&state, second, "hello again", cx));
    wait_until(cx, &state, "the reply on the moved login", |state| {
        ended(state, second)
    });
    assert!(moved.is_dir(), "the host made the new folder");
    assert_eq!(
        std::fs::read_dir(&default).unwrap().count(),
        1,
        "nothing was made in desk's default home"
    );
    let refused = state.update(cx, |state, cx| {
        state
            .set_remote_account_home(&sneaky, "/somewhere/else", cx)
            .unwrap_err()
    });
    assert!(refused.contains("has sent with this login"), "{refused}");
}

/// A send the machine refused leaves the session as if it never started a turn: its
/// saved last turn isn't Running after a relaunch, and a journal replaced later finds
/// no turn of it to end.
#[gpui::test]
fn a_refused_send_leaves_no_turn_running_across_a_relaunch_and_a_new_journal(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new();
    let state = runtime(dir.path(), cx);
    assert_eq!(link(&state, host.argv(), "desk", cx), None);
    let missing = format!("/nonexistent/keystone-refused-{}", std::process::id());
    let session = state.update(cx, |state, cx| {
        let folder = RemoteFolder::new(&missing).unwrap();
        state
            .create_remote(&desk(state), &folder, WindowId::FIRST, None, cx)
            .unwrap()
            .value
    });
    assert!(send(&state, session, "hello", cx));
    wait_until(cx, &state, "the refusal", |state| {
        state.conversations[&session]
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("Not sent."))
    });
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert_eq!(saved.last_turn(), None);
        assert!(!saved.started());
    });
    quit(state, false, cx);
    wipe_journal(&host);
    let state = launch_linked(dir.path(), host.argv(), session, cx);
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert_ne!(saved.last_turn(), Some(keystone_core::LastTurn::Running));
        let lost = state.conversations[&session].rows.iter().any(|row| {
            matches!(row, Row::TurnEnded { error: Some(error), .. } if error.contains("lost its record"))
        });
        assert!(!lost, "no turn of a refused message is ended as lost");
    });
}

/// The routes of a machine's sessions are registered before its link reads anything.
/// Its host's first frame is a Codex event, and the app's saves are blocked: the
/// event must hold the acknowledgement back until it is saved, which only a route
/// registered on arrival does. Started before the routes, the link acknowledges it at
/// once, and a kill then would lose it.
#[gpui::test]
fn a_links_routes_are_registered_before_it_reads_its_first_frame(cx: &mut gpui::TestAppContext) {
    use keystone_agent::remote::frame::{self, Frame, Routed, Welcome};
    let dir = tempfile::tempdir().unwrap();
    let session = {
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        let desk = store.link_machine("desk", "client-1".into()).unwrap().value;
        let folder = RemoteFolder::new("/srv/app").unwrap();
        let session = store
            .create_remote_showing(&desk, &folder, 1, WindowId::FIRST, None)
            .unwrap()
            .value;
        store.set_provider(session, Provider::Codex).unwrap();
        store.flush().unwrap();
        session
    };
    let script = tempfile::tempdir().unwrap();
    let frames = [
        Frame::Welcome(Welcome {
            protocol: remote::PROTOCOL,
            version: "test".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            hostname: "desk".into(),
            providers: Vec::new(),
            oldest: 1,
            latest: 1,
            journal: "journal-1".into(),
        }),
        Frame::Event(Routed {
            seq: 1,
            provider: Provider::Codex,
            account: None,
            session,
            event: Event::ItemCompleted(keystone_agent::Item {
                id: "reply-1".into(),
                kind: ItemKind::AgentMessage,
                text: "a reply the host kept".into(),
                status: keystone_agent::ItemStatus::Completed,
            }),
        }),
        Frame::Synced { seq: 1 },
        Frame::Sessions {
            sessions: Vec::new(),
        },
    ];
    let text: Vec<u8> = frames
        .iter()
        .flat_map(|frame| frame::encode(frame).unwrap())
        .collect();
    std::fs::write(script.path().join("frames"), text).unwrap();
    let sent = script.path().join("sent");
    let argv = vec![
        "sh".into(),
        "-c".into(),
        r#"cat "$0/frames"; exec cat >> "$0/sent""#.into(),
        script.path().into(),
    ];
    let acked = |sent: &Path| {
        std::fs::read_to_string(sent)
            .unwrap_or_default()
            .lines()
            .any(|line| line.contains(r#""t":"ack""#) && line.contains(r#""seq":1"#))
    };
    let state = runtime(dir.path(), cx);
    let lock = rusqlite::Connection::open(dir.path().join("workspace.sqlite3")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    state.update(cx, |state, _| {
        state.test_transport = Some(argv);
        state.test_route_pause = Some(Duration::from_millis(500));
    });
    let deadline = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < deadline {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        state.read_with(cx, |state, _| state.links.len() == 1),
        "the machine's link was made"
    );
    assert!(
        !acked(&sent),
        "nothing acknowledged the event before it was saved"
    );
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);
    wait_until(cx, &state, "the saved event's acknowledgement", |_| {
        acked(&sent)
    });
}

/// A steer sent while the first message is still in flight joins its way back: the
/// machine refuses both, the later first, and the session is as before either, free to
/// pick another login, with no turn running and both messages in their bubbles.
#[gpui::test]
fn a_steer_in_flight_on_a_refused_login_leaves_the_session_movable(cx: &mut gpui::TestAppContext) {
    use keystone_agent::remote::frame::{self, Frame, Outcome, Welcome};
    let dir = tempfile::tempdir().unwrap();
    let session = {
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        let desk = store.link_machine("desk", "client-1".into()).unwrap().value;
        let folder = RemoteFolder::new("/srv/app").unwrap();
        let session = store
            .create_remote_showing(&desk, &folder, 1, WindowId::FIRST, None)
            .unwrap()
            .value;
        store.flush().unwrap();
        session
    };
    let script = tempfile::tempdir().unwrap();
    let write = |name: &str, frames: &[Frame]| {
        let text: Vec<u8> = frames
            .iter()
            .flat_map(|frame| frame::encode(frame).unwrap())
            .collect();
        std::fs::write(script.path().join(name), text).unwrap();
    };
    write(
        "welcome",
        &[
            Frame::Welcome(Welcome {
                protocol: remote::PROTOCOL,
                version: "test".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                hostname: "desk".into(),
                providers: Vec::new(),
                oldest: 0,
                latest: 0,
                journal: "journal-1".into(),
            }),
            Frame::Synced { seq: 0 },
            Frame::Sessions {
                sessions: Vec::new(),
            },
        ],
    );
    for id in 1..=40 {
        write(
            &format!("reply-{id}"),
            &[Frame::Reply {
                id,
                outcome: Outcome::Login(
                    "The account \u{201c}Sneaky\u{201d} overlaps desk's default home".into(),
                ),
            }],
        );
    }
    // Answers the two sends once both have arrived, the steer first.
    let script_text = r#"read -r hello; cat "$0/welcome"
ids=""
while read -r line; do
  case "$line" in
    *'"t":"call"'*'"op":"send"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      ids="$id $ids"
      set -- $ids
      if [ "$#" -ge 2 ]; then for id in $ids; do cat "$0/reply-$id"; done; fi;;
  esac
done"#;
    let argv = vec![
        "sh".into(),
        "-c".into(),
        script_text.into(),
        script.path().into(),
    ];
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| state.test_transport = Some(argv));
    wait_until(cx, &state, "the link", |state| {
        state
            .links
            .values()
            .any(|link| link.status() == remote::Status::Connected)
    });
    state.update(cx, |state, cx| {
        let draft = |text: &str| ComposerDraft {
            session,
            text: text.into(),
            attachments: Vec::new(),
        };
        assert!(state.send_message(draft("first message"), cx));
        assert!(state.runs_turn(session), "the next message steers");
        assert!(state.send_message(draft("a steer"), cx));
    });
    wait_until(cx, &state, "both refusals", |state| {
        state
            .session(session)
            .is_some_and(|s| s.pending().len() == 2 && s.pending().iter().all(|p| p.refused))
    });
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert!(saved.draft().is_empty(), "the composer is never filled");
        assert!(!saved.started(), "the session can pick another login");
        assert_eq!(saved.last_turn(), None, "no turn runs");
        let error = state.conversations[&session].error.clone().unwrap();
        assert!(error.ends_with(WAY_OUT), "{error}");
        let refused: Vec<_> = state.conversations[&session]
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) if item.kind == ItemKind::UserMessage => {
                    Some((item.text.as_str(), item.status))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            refused,
            [
                ("first message", keystone_agent::ItemStatus::Failed),
                ("a steer", keystone_agent::ItemStatus::Failed)
            ],
            "both bubbles read \"Not sent\""
        );
    });
}

/// A machine scripted in `sh`, for answers in orders no real host gives. Once the app
/// says hello it sends `welcome`, then runs a script over the app's lines, with this
/// folder of frame files as `$0`; `$id` is a call's number, read with [`CALL_ID`].
struct Scripted(tempfile::TempDir);

/// Sets `id` to the number of the call on `$line`.
const CALL_ID: &str =
    r#"id=$(printf '%s' "$line" | sed -n 's/^{"t":"call","id":\([0-9]*\).*/\1/p')"#;

impl Scripted {
    fn new() -> Self {
        use keystone_agent::remote::frame::{Frame, Welcome};
        let scripted = Self(tempfile::tempdir().unwrap());
        scripted.write(
            "welcome",
            &[
                Frame::Welcome(Welcome {
                    protocol: remote::PROTOCOL,
                    version: "test".into(),
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    hostname: "desk".into(),
                    providers: Vec::new(),
                    oldest: 0,
                    latest: 0,
                    journal: "journal-1".into(),
                }),
                Frame::Synced { seq: 0 },
                Frame::Sessions {
                    sessions: Vec::new(),
                },
            ],
        );
        scripted
    }

    fn write(&self, name: &str, frames: &[keystone_agent::remote::frame::Frame]) {
        let text: Vec<u8> = frames
            .iter()
            .flat_map(|frame| keystone_agent::remote::frame::encode(frame).unwrap())
            .collect();
        std::fs::write(self.0.path().join(name), text).unwrap();
    }

    /// Writes `<name>-<id>` for every call number a test may use, from `frames`.
    fn each(&self, name: &str, frames: impl Fn(u64) -> Vec<keystone_agent::remote::frame::Frame>) {
        for id in 1..=60 {
            self.write(&format!("{name}-{id}"), &frames(id));
        }
    }

    /// Welcomes the app with a journal whose newest place is `latest`, as a host does
    /// that journaled events in an earlier run.
    fn welcome_at(&self, latest: u64) {
        use keystone_agent::remote::frame::{Frame, Welcome};
        self.write(
            "welcome",
            &[Frame::Welcome(Welcome {
                protocol: remote::PROTOCOL,
                version: "test".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                hostname: "desk".into(),
                providers: Vec::new(),
                oldest: 0,
                latest,
                journal: "journal-1".into(),
            })],
        );
    }

    /// A transport that can't reach the machine while `flag` exists, as `ssh` exits
    /// when the network is down, and otherwise runs `script` as [`Scripted::argv`] does.
    fn argv_unless(&self, flag: &Path, script: &str) -> Vec<std::ffi::OsString> {
        let script = format!(
            "if [ -e \"$1\" ]; then echo 'ssh: connect to host desk port 22: Network is unreachable' >&2; exit 255; fi\nread -r hello; cat \"$0/welcome\"\n{script}"
        );
        vec![
            "sh".into(),
            "-c".into(),
            script.into(),
            self.0.path().into(),
            flag.into(),
        ]
    }

    /// Every line the app sent, in order.
    fn lines(&self) -> String {
        std::fs::read_to_string(self.0.path().join("lines")).unwrap_or_default()
    }

    fn argv(&self, script: &str) -> Vec<std::ffi::OsString> {
        let script = format!("read -r hello; cat \"$0/welcome\"\n{script}");
        vec![
            "sh".into(),
            "-c".into(),
            script.into(),
            self.0.path().into(),
        ]
    }
}

fn reply(
    id: u64,
    outcome: keystone_agent::remote::frame::Outcome,
) -> keystone_agent::remote::frame::Frame {
    keystone_agent::remote::frame::Frame::Reply { id, outcome }
}

/// The frames of a host that took call `id`, which carried `text`: its reply, then the
/// turn's start and the provider's copy of the message, at `seq` and after.
fn taken(id: u64, session: u64, seq: u64, text: &str) -> Vec<keystone_agent::remote::frame::Frame> {
    use keystone_agent::remote::frame::{Frame, Outcome, Routed};
    let event = |seq, event| {
        Frame::Event(Routed {
            seq,
            provider: Provider::Claude,
            account: None,
            session,
            event,
        })
    };
    vec![
        reply(id, Outcome::Ok(())),
        event(seq, Event::TurnStarted),
        event(
            seq + 1,
            Event::ItemCompleted(keystone_agent::Item {
                id: format!("echo-{text}"),
                kind: ItemKind::UserMessage,
                text: text.into(),
                status: keystone_agent::ItemStatus::Completed,
            }),
        ),
    ]
}

/// A session on the linked machine "desk", saved before the app starts.
fn remote_session(dir: &Path) -> u64 {
    let mut store = Store::open(dir.join("workspace.json")).unwrap();
    let desk = store.link_machine("desk", "client-1".into()).unwrap().value;
    let folder = RemoteFolder::new("/srv/app").unwrap();
    let session = store
        .create_remote_showing(&desk, &folder, 1, WindowId::FIRST, None)
        .unwrap()
        .value;
    store.flush().unwrap();
    session
}

fn connected(state: &Entity<AppState>, cx: &mut gpui::TestAppContext) {
    wait_until(cx, state, "the link", |state| {
        state
            .links
            .values()
            .any(|link| link.status() == LinkStatus::Connected)
    });
}

/// Each user message's text and status, in the order they show.
fn bubbles(state: &AppState, session: u64) -> Vec<(String, keystone_agent::ItemStatus)> {
    state.conversations[&session]
        .rows
        .iter()
        .filter(|row| crate::conversation::visible(row))
        .filter_map(|row| match row {
            Row::Item(item) if item.kind == ItemKind::UserMessage => {
                Some((item.text.clone(), item.status))
            }
            _ => None,
        })
        .collect()
}

/// A message sent with a picture and a second sent while the first is in flight: the
/// host refuses one and takes the other, in either order. Each answer marks its own
/// message, the taken one shows once, and the refused one's text and picture come back.
fn one_refused_one_taken(first_refused: bool, cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::{Completed, Failed};
    use keystone_agent::remote::frame::Outcome;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let host = Scripted::new();
    let gone = "An image in this message is no longer on desk. Send it again to upload it";
    host.each("refuse", |id| vec![reply(id, Outcome::Err(gone.into()))]);
    host.each("take-first", |id| {
        taken(id, session, 1, "first, with a picture")
    });
    host.each("take-second", |id| taken(id, session, 1, "a steer"));
    let (first, second) = if first_refused {
        ("refuse", "take-second")
    } else {
        ("take-first", "refuse")
    };
    let script = format!(
        r#"n=0
while read -r line; do
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      n=$((n+1))
      if [ "$n" -eq 1 ]; then one=$id; fi
      if [ "$n" -eq 2 ]; then cat "$0/{first}-$one"; cat "$0/{second}-$id"; fi;;
  esac
done"#
    );
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_transport = Some(host.argv(&script))
    });
    connected(&state, cx);
    let picture = state.update(cx, |state, cx| {
        let picture = state
            .attachments
            .import_bytes(b"\x89PNG\r\n\x1a\nfirst picture", "first.png".into())
            .unwrap();
        assert!(state.send_message(
            ComposerDraft {
                session,
                text: "first, with a picture".into(),
                attachments: vec![picture.clone()],
            },
            cx
        ));
        assert!(send_now(state, session, "a steer", cx));
        picture
    });
    wait_until(cx, &state, "both answers", |state| {
        state
            .session(session)
            .is_some_and(|s| s.pending().iter().all(|p| p.refused))
            && bubbles(state, session)
                .iter()
                .all(|(_, status)| *status != keystone_agent::ItemStatus::InProgress)
            // The taken message's turn has started: its copy replaced its bubble.
            && state.conversations[&session].rows.iter().any(
                |row| matches!(row, Row::Item(item) if item.id.starts_with("echo-")),
            )
    });
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert!(saved.started(), "the taken message fixes the session");
        let error = state.conversations[&session].error.clone();
        assert_eq!(
            error.as_deref(),
            Some(format!("Not sent. {gone}").as_str()),
            "the refusal's reason outlasts the other message's turn start"
        );
        assert_eq!(saved.last_turn(), Some(keystone_core::LastTurn::Running));
        let shown = bubbles(state, session);
        if first_refused {
            assert_eq!(
                shown,
                [
                    ("first, with a picture".into(), Failed),
                    ("a steer".into(), Completed)
                ]
            );
            assert!(saved.draft().is_empty() && saved.draft_attachments().is_empty());
            let refused = &saved.pending()[0];
            assert_eq!(
                (refused.text.as_str(), refused.refused),
                ("first, with a picture", true)
            );
            assert_eq!(refused.attachments, [picture]);
        } else {
            assert_eq!(
                shown,
                [
                    ("first, with a picture".into(), Completed),
                    ("a steer".into(), Failed)
                ]
            );
            assert!(saved.draft().is_empty() && saved.draft_attachments().is_empty());
            let refused = &saved.pending()[0];
            assert_eq!((refused.text.as_str(), refused.refused), ("a steer", true));
        }
    });
}

fn send_now(state: &mut AppState, session: u64, text: &str, cx: &mut Context<AppState>) -> bool {
    state.send_message(
        ComposerDraft {
            session,
            text: text.into(),
            attachments: Vec::new(),
        },
        cx,
    )
}

#[gpui::test]
fn a_refused_message_then_a_taken_steer_mark_their_own_bubbles(cx: &mut gpui::TestAppContext) {
    one_refused_one_taken(true, cx);
}

#[gpui::test]
fn a_taken_message_then_a_refused_steer_mark_their_own_bubbles(cx: &mut gpui::TestAppContext) {
    one_refused_one_taken(false, cx);
}

/// Three messages in flight, answered out of order: the third refused, the first
/// taken, the second refused. Each bubble shows its own answer and the composer is
/// never filled. Each refused one's Send again works on its own, under its own ID, and
/// moves it to the end, where its reply follows it.
#[gpui::test]
fn three_messages_answered_out_of_order_each_show_their_own_answer(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::{Completed, Failed};
    use keystone_agent::remote::frame::Outcome;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let host = Scripted::new();
    host.each("refuse", |id| {
        vec![reply(id, Outcome::Err("refused".into()))]
    });
    host.each("take", |id| taken(id, session, 1, "one"));
    host.each("again-three", |id| taken(id, session, 3, "three"));
    host.each("again-two", |id| taken(id, session, 5, "two"));
    let script = format!(
        r#"n=0
while read -r line; do
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      n=$((n+1))
      eval "call$n=$id"
      if [ "$n" -eq 3 ]; then cat "$0/refuse-$call3"; cat "$0/take-$call1"; cat "$0/refuse-$call2"; fi
      if [ "$n" -eq 4 ]; then cat "$0/again-three-$id"; fi
      if [ "$n" -eq 5 ]; then cat "$0/again-two-$id"; fi;;
  esac
done"#
    );
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_transport = Some(host.argv(&script))
    });
    connected(&state, cx);
    state.update(cx, |state, cx| {
        for text in ["one", "two", "three"] {
            assert!(send_now(state, session, text, cx));
        }
    });
    wait_until(cx, &state, "three answers", |state| {
        state
            .session(session)
            .is_some_and(|s| s.pending().iter().all(|p| p.refused))
    });
    state.read_with(cx, |state, _| {
        assert_eq!(
            bubbles(state, session),
            [
                ("one".into(), Completed),
                ("two".into(), Failed),
                ("three".into(), Failed)
            ]
        );
        let saved = state.session(session).unwrap();
        assert!(saved.draft().is_empty(), "the composer is never filled");
        assert!(saved.started());
    });
    let ids = state.read_with(cx, |state, _| {
        let pending = state.session(session).unwrap().pending().to_vec();
        assert_eq!(pending.len(), 2);
        (pending[0].id.clone(), pending[1].id.clone())
    });
    // Each refused message sent again on its own: "three" first, then "two".
    state.update(cx, |state, cx| {
        assert!(state.send_again(session, &ids.1, cx))
    });
    wait_until(cx, &state, "three taken", |state| {
        bubbles(state, session)[2] == ("three".into(), Completed)
    });
    state.read_with(cx, |state, _| {
        assert_eq!(bubbles(state, session)[1], ("two".into(), Failed));
    });
    state.update(cx, |state, cx| {
        assert!(state.send_again(session, &ids.0, cx))
    });
    wait_until(cx, &state, "two taken", |state| {
        state
            .session(session)
            .is_some_and(|s| s.pending().is_empty())
    });
    // Each moved to the end as it was sent again, so its reply follows it.
    state.read_with(cx, |state, _| {
        assert_eq!(
            bubbles(state, session),
            [
                ("one".into(), Completed),
                ("three".into(), Completed),
                ("two".into(), Completed)
            ]
        );
    });
}

/// Two messages sent to a machine that hasn't answered when the app quits. They are
/// saved as pending, not as a turn, and the next launch asks the machine about each.
/// Returns the session and the data folder.
fn quit_with_two_pending(cx: &mut gpui::TestAppContext) -> (u64, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let silent = Scripted::new();
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_transport = Some(silent.argv("while read -r line; do :; done"));
    });
    connected(&state, cx);
    state.update(cx, |state, cx| {
        assert!(send_now(state, session, "r1", cx));
        assert!(send_now(state, session, "r2", cx));
    });
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert_eq!(saved.pending().len(), 2);
        assert!(
            !saved.started(),
            "nothing is fixed before the machine answers"
        );
        assert_eq!(saved.last_turn(), None, "and no turn runs");
    });
    quit(state, false, cx);
    (session, dir)
}

/// The script of a machine that answers each `recall` from `<name>-<id>`.
fn recalling(name: &str) -> String {
    format!(
        r#"while read -r line; do
  case "$line" in
    *'"op":"recall"'*)
      {CALL_ID}
      cat "$0/{name}-$id";;
  esac
done"#
    )
}

/// Two messages pending across a relaunch on a login the machine refuses: the next
/// launch asks about them, both read "Not sent" and stay in their bubbles, out of the
/// draft, and the session can move to another login, with the way out shown.
#[gpui::test]
fn two_pending_messages_a_machine_refused_stay_in_their_bubbles_after_a_relaunch(
    cx: &mut gpui::TestAppContext,
) {
    use keystone_agent::ItemStatus::Failed;
    use keystone_agent::remote::frame::Outcome;
    let (session, dir) = quit_with_two_pending(cx);
    let host = Scripted::new();
    let refused = "The account \u{201c}Sneaky\u{201d} overlaps desk's default home";
    host.each("login", |id| {
        vec![reply(id, Outcome::Login(refused.into()))]
    });
    let state = launch_linked(dir.path(), host.argv(&recalling("login")), session, cx);
    wait_until(cx, &state, "both answers", |state| {
        state
            .session(session)
            .is_some_and(|s| s.pending().iter().all(|p| p.refused))
    });
    state.read_with(cx, |state, _| {
        assert_eq!(
            bubbles(state, session),
            [("r1".into(), Failed), ("r2".into(), Failed)]
        );
        let saved = state.session(session).unwrap();
        assert!(!saved.started());
        assert_eq!(saved.last_turn(), None);
        assert!(saved.draft().is_empty(), "the composer is never filled");
        assert_eq!(
            saved.pending().len(),
            2,
            "both wait in their bubbles to be sent again"
        );
        assert!(!state.model_selection(session, saved.provider()).started);
        let error = state.conversations[&session].error.clone().unwrap();
        assert!(error.ends_with(WAY_OUT), "{error}");
    });
}

/// A message pending across a relaunch that the machine took: the next launch learns
/// so, and it is the session's running turn, with its provider and login fixed.
#[gpui::test]
fn a_pending_message_the_machine_took_runs_after_a_relaunch(cx: &mut gpui::TestAppContext) {
    use keystone_agent::remote::frame::Outcome;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let silent = Scripted::new();
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_transport = Some(silent.argv("while read -r line; do :; done"));
    });
    connected(&state, cx);
    state.update(cx, |state, cx| {
        assert!(send_now(state, session, "taken", cx))
    });
    quit(state, false, cx);
    let host = Scripted::new();
    host.each("ok", |id| vec![reply(id, Outcome::Ok(()))]);
    let state = launch_linked(dir.path(), host.argv(&recalling("ok")), session, cx);
    wait_until(cx, &state, "the answer", |state| {
        state
            .session(session)
            .is_some_and(|s| s.pending().iter().all(|p| p.refused))
    });
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert!(saved.started());
        assert_eq!(saved.last_turn(), Some(keystone_core::LastTurn::Running));
        assert_eq!(
            bubbles(state, session),
            [("taken".into(), keystone_agent::ItemStatus::Completed)]
        );
        assert!(saved.draft().is_empty());
    });
}

/// A message pending across a relaunch that the machine has no record of: it reads
/// "Not sent", saying it may already have run, and stays in its bubble, to be sent
/// again.
#[gpui::test]
fn a_pending_message_the_machine_never_received_is_not_sent(cx: &mut gpui::TestAppContext) {
    use keystone_agent::remote::frame::Outcome;
    let (session, dir) = quit_with_two_pending(cx);
    let host = Scripted::new();
    host.each("none", |id| vec![reply(id, Outcome::Unrecorded)]);
    let state = launch_linked(dir.path(), host.argv(&recalling("none")), session, cx);
    wait_until(cx, &state, "both answers", |state| {
        state
            .session(session)
            .is_some_and(|s| s.pending().iter().all(|p| p.refused))
    });
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert!(!saved.started());
        assert!(saved.draft().is_empty(), "the composer is never filled");
        assert_eq!(
            saved.pending().len(),
            2,
            "both wait in their bubbles to be sent again"
        );
        let error = state.conversations[&session].error.clone().unwrap();
        assert!(error.contains("may already have run"), "{error}");
        // Send again's tooltip says so too, from what is saved.
        assert!(saved.pending().iter().all(|p| p.unrecorded));
    });
}

/// The routed frame of `event` at place `seq` for `session` on Claude's default login.
fn routed(seq: u64, session: u64, event: Event) -> keystone_agent::remote::frame::Frame {
    keystone_agent::remote::frame::Frame::Event(keystone_agent::remote::frame::Routed {
        seq,
        provider: Provider::Claude,
        account: None,
        session,
        event,
    })
}

fn echo(text: &str) -> Event {
    Event::ItemCompleted(keystone_agent::Item {
        id: format!("echo-{text}"),
        kind: ItemKind::UserMessage,
        text: text.into(),
        status: keystone_agent::ItemStatus::Completed,
    })
}

fn turn_end() -> Event {
    Event::TurnCompleted {
        status: keystone_agent::TurnStatus::Completed,
        error: None,
        stopped: false,
    }
}

/// The frames a host replays for the message `send` it took in an earlier run, from
/// place `seq`: its journaled `Taken`, then the turn whole.
fn ran(
    session: u64,
    seq: u64,
    send: &str,
    text: &str,
) -> Vec<keystone_agent::remote::frame::Frame> {
    use keystone_agent::remote::frame::Frame;
    vec![
        routed(seq, session, Event::Taken { send: send.into() }),
        routed(seq + 1, session, Event::TurnStarted),
        routed(seq + 2, session, echo(text)),
        routed(seq + 3, session, turn_end()),
        Frame::Synced { seq: seq + 3 },
        Frame::Sessions {
            sessions: Vec::new(),
        },
    ]
}

/// A message sent to a machine whose answer never came back before the app quit.
/// Returns the session, the data folder and the message's ID.
fn quit_with_one_pending(
    text: &str,
    cx: &mut gpui::TestAppContext,
) -> (u64, tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let silent = Scripted::new();
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_transport = Some(silent.argv("while read -r line; do :; done"));
    });
    connected(&state, cx);
    state.update(cx, |state, cx| assert!(send_now(state, session, text, cx)));
    let send = state.read_with(cx, |state, _| {
        state.session(session).unwrap().pending()[0].id.clone()
    });
    quit(state, false, cx);
    (session, dir, send)
}

/// A relaunch whose machine can't be reached while `flag` exists, with `session`'s
/// history loaded once the link has tried and failed.
fn launch_unreached(
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
    wait_until(cx, &state, "the link to fail", move |state| {
        state
            .unreached(session)
            .is_some_and(|(reason, _)| reason == "Couldn't reach desk")
            && state
                .conversations
                .get(&session)
                .is_some_and(|c| !c.is_loading())
    });
    // Past the 30 s an earlier build waited before it counted a message as not sent.
    cx.executor().advance_clock(Duration::from_secs(35));
    cx.run_until_parked();
    state
}

/// The check's sequence: the machine took a message, the app quit before the answer
/// came, the next launch can't reach the machine, and then it can. The message waits,
/// out of the draft, while the machine is out of reach, and its outcome comes from the
/// machine: one bubble, sent, and no second run.
#[gpui::test]
fn a_taken_message_waits_out_an_unreachable_machine_and_settles_from_its_record(
    cx: &mut gpui::TestAppContext,
) {
    use keystone_agent::ItemStatus::{Completed, InProgress};
    use keystone_agent::remote::frame::Outcome;
    let (session, dir, send) = quit_with_one_pending("taken while away", cx);
    let host = Scripted::new();
    host.welcome_at(4);
    host.write("replay", &ran(session, 1, &send, "taken while away"));
    host.each("ok", |id| vec![reply(id, Outcome::Ok(()))]);
    let script = format!(
        r#"cat "$0/replay"
while read -r line; do
  printf '%s\n' "$line" >> "$0/lines"
  case "$line" in
    *'"op":"recall"'*)
      {CALL_ID}
      cat "$0/ok-$id";;
  esac
done"#
    );
    let down = dir.path().join("down");
    std::fs::write(&down, "").unwrap();
    let state = launch_unreached(dir.path(), host.argv_unless(&down, &script), session, cx);
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert_eq!(saved.pending().len(), 1, "the message still waits");
        assert!(saved.draft().is_empty(), "and its text isn't in the draft");
        assert_eq!(saved.last_turn(), None);
        assert_eq!(
            bubbles(state, session),
            [("taken while away".into(), InProgress)]
        );
        assert!(state.conversations[&session].error.is_none());
    });
    std::fs::remove_file(&down).unwrap();
    wait_until(cx, &state, "the machine's answer", |state| {
        state.session(session).is_some_and(|s| {
            s.pending().iter().all(|p| p.refused)
                && s.last_turn() == Some(keystone_core::LastTurn::Finished)
        })
    });
    state.read_with(cx, |state, _| {
        assert_eq!(
            bubbles(state, session),
            [("taken while away".into(), Completed)]
        );
        let saved = state.session(session).unwrap();
        assert!(saved.started());
        assert!(saved.draft().is_empty());
        assert!(state.conversations[&session].error.is_none());
    });
    assert!(
        !host.lines().contains(r#""op":"send""#),
        "the message never went again: {}",
        host.lines()
    );
}

/// Discard on a message whose machine can't be reached frees the session without
/// resending or touching the draft. When the machine is reached and shows the message
/// ran, its turn shows and the bubble reads sent.
#[gpui::test]
fn a_discarded_message_the_machine_ran_shows_its_turn(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::{Completed, Interrupted};
    let (session, dir, send) = quit_with_one_pending("discarded", cx);
    let host = Scripted::new();
    host.welcome_at(4);
    host.write("replay", &ran(session, 1, &send, "discarded"));
    let script = r#"cat "$0/replay"
while read -r line; do printf '%s\n' "$line" >> "$0/lines"; done"#;
    let down = dir.path().join("down");
    std::fs::write(&down, "").unwrap();
    let state = launch_unreached(dir.path(), host.argv_unless(&down, script), session, cx);
    state.update(cx, |state, cx| state.discard_send(session, &send, cx));
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert!(saved.pending().is_empty());
        assert!(!saved.started(), "the session is free to move");
        assert!(saved.draft().is_empty());
        assert_eq!(bubbles(state, session), [("discarded".into(), Interrupted)]);
    });
    std::fs::remove_file(&down).unwrap();
    wait_until(cx, &state, "the machine's record", |state| {
        state
            .session(session)
            .is_some_and(|s| s.last_turn() == Some(keystone_core::LastTurn::Finished))
    });
    state.read_with(cx, |state, _| {
        assert_eq!(bubbles(state, session), [("discarded".into(), Completed)]);
        assert!(state.session(session).unwrap().started());
    });
    assert!(!host.lines().contains(r#""op":"send""#), "{}", host.lines());
    assert!(
        !host.lines().contains(r#""op":"recall""#),
        "{}",
        host.lines()
    );
}

/// A turn running at quit whose machine can't be reached at the next launch, its
/// message's answer not yet back: the turn isn't ended as failed. The session says it
/// waits for the machine, and the machine's record ends the turn once it is reached.
#[gpui::test]
fn a_running_turn_waits_out_an_unreachable_machine_and_finishes(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let first = Scripted::new();
    first.write(
        "start",
        &[
            routed(1, session, Event::TurnStarted),
            routed(2, session, echo("long turn")),
        ],
    );
    // The turn starts, and the answer to its message is still on its way at quit.
    let script = r#"while read -r line; do
  case "$line" in
    *'"op":"send"'*) cat "$0/start";;
  esac
done"#;
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_transport = Some(first.argv(script))
    });
    connected(&state, cx);
    state.update(cx, |state, cx| {
        assert!(send_now(state, session, "long turn", cx))
    });
    wait_until(cx, &state, "the turn to run", |state| {
        state.session(session).is_some_and(|s| {
            s.pending().len() == 1 && s.last_turn() == Some(keystone_core::LastTurn::Running)
        })
    });
    quit(state, false, cx);

    let host = Scripted::new();
    host.welcome_at(3);
    host.write(
        "end",
        &[
            routed(3, session, turn_end()),
            keystone_agent::remote::frame::Frame::Synced { seq: 3 },
            keystone_agent::remote::frame::Frame::Sessions {
                sessions: Vec::new(),
            },
        ],
    );
    let down = dir.path().join("down");
    std::fs::write(&down, "").unwrap();
    host.each("ok", |id| {
        vec![reply(id, keystone_agent::remote::frame::Outcome::Ok(()))]
    });
    let script = format!("cat \"$0/end\"\n{}", recalling("ok"));
    let state = launch_unreached(dir.path(), host.argv_unless(&down, &script), session, cx);
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert_eq!(saved.last_turn(), Some(keystone_core::LastTurn::Running));
        assert!(failed_ends(dir.path(), session).is_empty());
        assert_eq!(
            state.turn_waits(session).as_deref(),
            Some("Waiting for desk to say how this turn went")
        );
        // The open turn says it went on there, never that it stopped.
        let rows = &state.conversations[&session].rows;
        assert!(
            !rows.iter().any(|row| matches!(row, Row::TurnEnded { .. })),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| matches!(
                row,
                Row::Note(note) if note == "Keystone was closed during this turn on desk."
            )),
            "{rows:?}"
        );
    });
    std::fs::remove_file(&down).unwrap();
    wait_until(cx, &state, "the turn's end", |state| {
        state.session(session).is_some_and(|s| {
            s.pending().iter().all(|p| p.refused)
                && s.last_turn() == Some(keystone_core::LastTurn::Finished)
        })
    });
    state.read_with(cx, |state, _| {
        assert!(state.turn_waits(session).is_none());
        assert!(!state.conversations[&session].running);
    });
    assert!(failed_ends(dir.path(), session).is_empty());
}

/// The machine refused a message, and then its events show it ran: the bubble marked
/// "Not sent" reads sent, in place, once, the "Not sent" line goes, and nothing is
/// left to send again.
#[gpui::test]
fn a_message_shown_not_sent_that_ran_is_corrected_in_place(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::{Completed, Failed};
    use keystone_agent::remote::frame::Outcome;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let host = Scripted::new();
    host.each("refuse", |id| {
        vec![reply(id, Outcome::Err("refused".into()))]
    });
    let mut later = ran(session, 1, "SENDID", "ran anyway");
    later.truncate(4);
    host.write("later", &later);
    let script = format!(
        r#"while read -r line; do
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      send=$(printf '%s' "$line" | sed -n 's/.*"send":"\([^"]*\)".*/\1/p')
      cat "$0/refuse-$id"
      until [ -e "$0/go" ]; do sleep 0.05; done
      sed "s/SENDID/$send/" "$0/later";;
  esac
done"#
    );
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_transport = Some(host.argv(&script))
    });
    connected(&state, cx);
    state.update(cx, |state, cx| {
        assert!(send_now(state, session, "ran anyway", cx))
    });
    wait_until(cx, &state, "the refusal", |state| {
        state
            .session(session)
            .is_some_and(|s| s.pending().iter().all(|p| p.refused))
    });
    state.read_with(cx, |state, _| {
        assert_eq!(bubbles(state, session), [("ran anyway".into(), Failed)]);
        assert!(state.session(session).unwrap().draft().is_empty());
    });
    std::fs::write(host.0.path().join("go"), "").unwrap();
    wait_until(cx, &state, "the turn the machine ran", |state| {
        state
            .session(session)
            .is_some_and(|s| s.last_turn() == Some(keystone_core::LastTurn::Finished))
    });
    state.read_with(cx, |state, _| {
        assert_eq!(bubbles(state, session), [("ran anyway".into(), Completed)]);
        let saved = state.session(session).unwrap();
        assert!(saved.started());
        assert!(saved.pending().is_empty(), "nothing is left to send again");
        assert!(state.conversations[&session].error.is_none());
    });
}

/// A message the machine took during a turn that runs as the next one: the provider's
/// copy of it replaces its own bubble, after the first turn's end, so each message
/// shows once.
#[gpui::test]
fn a_steer_taken_in_one_turn_and_run_in_the_next_shows_once(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::Completed;
    use keystone_agent::remote::frame::{Frame, Outcome, Routed};
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let host = Scripted::new();
    let event = |seq, event| {
        Frame::Event(Routed {
            seq,
            provider: Provider::Claude,
            account: None,
            session,
            event,
        })
    };
    let end = || Event::TurnCompleted {
        status: keystone_agent::TurnStatus::Completed,
        error: None,
        stopped: false,
    };
    let echo = |text: &str| {
        Event::ItemCompleted(keystone_agent::Item {
            id: format!("echo-{text}"),
            kind: ItemKind::UserMessage,
            text: text.into(),
            status: Completed,
        })
    };
    host.each("first", |id| taken(id, session, 1, "first"));
    host.each("second", |id| {
        vec![
            reply(id, Outcome::Ok(())),
            event(3, end()),
            event(4, Event::TurnStarted),
            event(5, echo("second")),
            event(6, end()),
        ]
    });
    let script = format!(
        r#"n=0
while read -r line; do
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      n=$((n+1))
      if [ "$n" -eq 1 ]; then cat "$0/first-$id"; fi
      if [ "$n" -eq 2 ]; then cat "$0/second-$id"; fi;;
  esac
done"#
    );
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_transport = Some(host.argv(&script))
    });
    connected(&state, cx);
    state.update(cx, |state, cx| {
        assert!(send_now(state, session, "first", cx))
    });
    wait_until(cx, &state, "the first turn", |state| {
        state.session(session).is_some_and(|s| s.started())
    });
    state.update(cx, |state, cx| {
        assert!(send_now(state, session, "second", cx))
    });
    wait_until(cx, &state, "the second turn's end", |state| {
        state.session(session).is_some_and(|s| {
            s.pending().iter().all(|p| p.refused)
                && s.last_turn() == Some(keystone_core::LastTurn::Finished)
        })
    });
    state.read_with(cx, |state, _| {
        assert_eq!(
            bubbles(state, session),
            [("first".into(), Completed), ("second".into(), Completed)]
        );
    });
}

/// A message pending across a relaunch whose turn the machine replays before it
/// answers the recall: the turn's end stands, the session isn't left working, and the
/// provider's copy replaces the message's bubble, so it shows once.
#[gpui::test]
fn a_turn_replayed_before_its_message_is_answered_ends_and_shows_once(
    cx: &mut gpui::TestAppContext,
) {
    use keystone_agent::remote::frame::{Frame, Outcome, Routed};
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let silent = Scripted::new();
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_transport = Some(silent.argv("while read -r line; do :; done"));
    });
    connected(&state, cx);
    state.update(cx, |state, cx| {
        assert!(send_now(state, session, "replayed", cx))
    });
    quit(state, false, cx);

    let host = Scripted::new();
    let event = |seq, event| {
        Frame::Event(Routed {
            seq,
            provider: Provider::Claude,
            account: None,
            session,
            event,
        })
    };
    host.write(
        "turn",
        &[
            event(1, Event::TurnStarted),
            event(
                2,
                Event::ItemCompleted(keystone_agent::Item {
                    id: "echo-replayed".into(),
                    kind: ItemKind::UserMessage,
                    text: "replayed".into(),
                    status: keystone_agent::ItemStatus::Completed,
                }),
            ),
            event(
                3,
                Event::TurnCompleted {
                    status: keystone_agent::TurnStatus::Completed,
                    error: None,
                    stopped: false,
                },
            ),
        ],
    );
    host.each("ok", |id| vec![reply(id, Outcome::Ok(()))]);
    let script = format!("cat \"$0/turn\"\n{}", recalling("ok"));
    let state = launch_linked(dir.path(), host.argv(&script), session, cx);
    wait_until(cx, &state, "the answer", |state| {
        state
            .session(session)
            .is_some_and(|s| s.pending().iter().all(|p| p.refused))
    });
    state.read_with(cx, |state, _| {
        let saved = state.session(session).unwrap();
        assert!(saved.started());
        assert_eq!(saved.last_turn(), Some(keystone_core::LastTurn::Finished));
        assert_eq!(
            bubbles(state, session),
            [("replayed".into(), keystone_agent::ItemStatus::Completed)]
        );
    });
}

/// A window on `session`, selected, whose machine is reached through `argv`, with its
/// composer focused.
fn window<'a>(
    dir: &Path,
    session: u64,
    argv: Vec<std::ffi::OsString>,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<crate::Keystone>, &'a mut gpui::VisualTestContext) {
    let mut store = Store::open(dir.join("workspace.json")).unwrap();
    store.select(session).unwrap();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = AppState::new(store, cx);
            state.test_transport = Some(argv);
            state.ensure_history(session);
            state
        });
        crate::Keystone::new(state, false, window, cx)
    });
    view.update_in(cx, |this, window, cx| {
        this.composer.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();
    (view, cx)
}

/// Runs the window with real time passing until `done` holds.
#[track_caller]
fn wait_window(
    cx: &mut gpui::VisualTestContext,
    view: &Entity<crate::Keystone>,
    what: &str,
    done: impl Fn(&AppState) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |view, cx| done(view.state.read(cx))) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn composer(view: &Entity<crate::Keystone>, cx: &mut gpui::VisualTestContext) -> String {
    view.read_with(cx, |view, cx| view.composer.read(cx).value().to_owned())
}

fn press(cx: &mut gpui::VisualTestContext, selector: String) {
    let bounds = cx
        .debug_bounds(Box::leak(selector.clone().into_boxed_str()))
        .unwrap_or_else(|| panic!("{selector} not rendered"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

/// The row of the message the app sent as `send`.
fn row_of(state: &AppState, session: u64, send: &str) -> usize {
    state.conversations[&session]
        .rows
        .iter()
        .position(|row| matches!(row, Row::Item(item) if item.id == send))
        .unwrap()
}

fn statuses(
    view: &Entity<crate::Keystone>,
    session: u64,
    cx: &mut gpui::VisualTestContext,
) -> Vec<(String, keystone_agent::ItemStatus)> {
    view.read_with(cx, |view, cx| bubbles(view.state.read(cx), session))
}

/// The check's sequence: the machine refuses a message, then its record shows it ran.
/// The composer was never filled, so pressing Send on what it holds sends nothing, the
/// message runs once, and its bubble reads sent.
#[gpui::test]
fn a_refused_message_that_ran_never_fills_the_composer(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::{Completed, Failed};
    use keystone_agent::remote::frame::Outcome;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let host = Scripted::new();
    host.each("refuse", |id| {
        vec![reply(id, Outcome::Err("refused".into()))]
    });
    let mut later = ran(session, 1, "SENDID", "ran anyway");
    later.truncate(4);
    host.write("later", &later);
    let script = format!(
        r#"while read -r line; do
  printf '%s\n' "$line" >> "$0/lines"
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      send=$(printf '%s' "$line" | sed -n 's/.*"send":"\([^"]*\)".*/\1/p')
      cat "$0/refuse-$id"
      until [ -e "$0/go" ]; do sleep 0.05; done
      sed "s/SENDID/$send/" "$0/later";;
  esac
done"#
    );
    let (view, cx) = window(dir.path(), session, host.argv(&script), cx);
    wait_window(cx, &view, "the link", |state| {
        state
            .links
            .values()
            .any(|l| l.status() == LinkStatus::Connected)
    });
    cx.simulate_input("ran anyway");
    cx.simulate_keystrokes("secondary-enter");
    wait_window(cx, &view, "the refusal", |state| {
        bubbles(state, session) == [("ran anyway".into(), Failed)]
    });
    assert_eq!(composer(&view, cx), "", "the composer is never filled");
    std::fs::write(host.0.path().join("go"), "").unwrap();
    wait_window(cx, &view, "the machine's record", |state| {
        state
            .session(session)
            .is_some_and(|s| s.last_turn() == Some(keystone_core::LastTurn::Finished))
    });
    assert_eq!(composer(&view, cx), "");
    // The user presses Send on whatever the composer holds: nothing.
    cx.simulate_keystrokes("secondary-enter");
    cx.run_until_parked();
    assert_eq!(
        statuses(&view, session, cx),
        [("ran anyway".into(), Completed)]
    );
    assert_eq!(
        host.lines().matches(r#""op":"send""#).count(),
        1,
        "it ran once"
    );
}

/// Send again on a message its machine had in fact taken, though the app was told it
/// wasn't: the machine answers `ok` from its record and runs nothing, and the bubble
/// reads sent. The composer is never filled.
#[gpui::test]
fn send_again_on_a_message_the_machine_took_runs_nothing(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::{Completed, Failed};
    use keystone_agent::remote::frame::Outcome;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let host = Scripted::new();
    host.each("refuse", |id| {
        vec![reply(id, Outcome::Err("refused".into()))]
    });
    host.each("recorded", |id| vec![reply(id, Outcome::Ok(()))]);
    let script = format!(
        r#"n=0
while read -r line; do
  printf '%s\n' "$line" >> "$0/lines"
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      n=$((n+1))
      if [ "$n" -eq 1 ]; then cat "$0/refuse-$id"; else cat "$0/recorded-$id"; fi;;
  esac
done"#
    );
    let (view, cx) = window(dir.path(), session, host.argv(&script), cx);
    wait_window(cx, &view, "the link", |state| {
        state
            .links
            .values()
            .any(|l| l.status() == LinkStatus::Connected)
    });
    cx.simulate_input("taken after all");
    cx.simulate_keystrokes("secondary-enter");
    wait_window(cx, &view, "the refusal", |state| {
        bubbles(state, session) == [("taken after all".into(), Failed)]
    });
    let (send, row) = view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        let send = state.session(session).unwrap().pending()[0].id.clone();
        let row = row_of(state, session, &send);
        (send, row)
    });
    press(cx, format!("send-again-{row}"));
    wait_window(cx, &view, "the recorded ok", |state| {
        bubbles(state, session) == [("taken after all".into(), Completed)]
    });
    assert_eq!(composer(&view, cx), "");
    let lines = host.lines();
    assert_eq!(
        lines.matches(&format!(r#""send":"{send}""#)).count(),
        2,
        "sent again under its own ID: {lines}"
    );
    view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        assert!(state.session(session).unwrap().pending().is_empty());
        assert!(state.session(session).unwrap().started());
        assert!(
            !state.conversations[&session]
                .rows
                .iter()
                .any(|row| matches!(
                    row,
                    Row::Item(item) if item.kind == ItemKind::AgentMessage
                )),
            "nothing ran"
        );
    });
}

/// Two messages the machine refused: each one's Send again sends that one alone,
/// under its own ID, and the composer is never filled.
#[gpui::test]
fn each_refused_messages_send_again_works_on_its_own(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::{Completed, Failed};
    use keystone_agent::remote::frame::Outcome;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let host = Scripted::new();
    host.each("refuse", |id| {
        vec![reply(id, Outcome::Err("refused".into()))]
    });
    host.each("second", |id| taken(id, session, 1, "second"));
    host.each("first", |id| taken(id, session, 3, "first"));
    let script = format!(
        r#"n=0
while read -r line; do
  printf '%s\n' "$line" >> "$0/lines"
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      n=$((n+1))
      if [ "$n" -le 2 ]; then cat "$0/refuse-$id"; fi
      if [ "$n" -eq 3 ]; then cat "$0/second-$id"; fi
      if [ "$n" -eq 4 ]; then cat "$0/first-$id"; fi;;
  esac
done"#
    );
    let (view, cx) = window(dir.path(), session, host.argv(&script), cx);
    wait_window(cx, &view, "the link", |state| {
        state
            .links
            .values()
            .any(|l| l.status() == LinkStatus::Connected)
    });
    for text in ["first", "second"] {
        cx.simulate_input(text);
        cx.simulate_keystrokes("secondary-enter");
        wait_window(cx, &view, "the refusal", |state| {
            bubbles(state, session).last() == Some(&(text.into(), Failed))
        });
    }
    assert_eq!(composer(&view, cx), "");
    let ids = view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        let pending = state.session(session).unwrap().pending();
        assert_eq!(pending.len(), 2);
        (pending[0].id.clone(), pending[1].id.clone())
    });
    let row = view.read_with(cx, |view, cx| row_of(view.state.read(cx), session, &ids.1));
    press(cx, format!("send-again-{row}"));
    wait_window(cx, &view, "second taken", |state| {
        bubbles(state, session) == [("first".into(), Failed), ("second".into(), Completed)]
    });
    let row = view.read_with(cx, |view, cx| row_of(view.state.read(cx), session, &ids.0));
    press(cx, format!("send-again-{row}"));
    // Sent again last, it shows last, where its reply follows it.
    wait_window(cx, &view, "first taken", |state| {
        bubbles(state, session) == [("second".into(), Completed), ("first".into(), Completed)]
    });
    assert_eq!(composer(&view, cx), "");
    let lines = host.lines();
    for id in [&ids.0, &ids.1] {
        assert_eq!(
            lines.matches(&format!(r#""send":"{id}""#)).count(),
            2,
            "{lines}"
        );
    }
}

/// Edit takes a refused message's text into an empty composer and marks its bubble
/// "Edited"; with text in the composer it is off, says why, and changes nothing.
#[gpui::test]
fn edit_fills_only_an_empty_composer(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::Failed;
    use keystone_agent::remote::frame::Outcome;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let host = Scripted::new();
    host.each("refuse", |id| {
        vec![reply(id, Outcome::Err("refused".into()))]
    });
    let script = format!(
        r#"while read -r line; do
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      cat "$0/refuse-$id";;
  esac
done"#
    );
    let (view, cx) = window(dir.path(), session, host.argv(&script), cx);
    wait_window(cx, &view, "the link", |state| {
        state
            .links
            .values()
            .any(|l| l.status() == LinkStatus::Connected)
    });
    cx.simulate_input("fix the typo");
    cx.simulate_keystrokes("secondary-enter");
    wait_window(cx, &view, "the refusal", |state| {
        bubbles(state, session) == [("fix the typo".into(), Failed)]
    });
    let row = view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        let send = state.session(session).unwrap().pending()[0].id.clone();
        row_of(state, session, &send)
    });
    // With something in the composer, Edit is off and changes nothing.
    cx.simulate_input("something else");
    cx.run_until_parked();
    press(cx, format!("edit-message-{row}"));
    assert_eq!(composer(&view, cx), "something else");
    assert_eq!(
        statuses(&view, session, cx),
        [("fix the typo".into(), Failed)]
    );
    view.read_with(cx, |view, cx| {
        assert_eq!(
            view.state
                .read(cx)
                .session(session)
                .unwrap()
                .pending()
                .len(),
            1
        );
    });
    // Emptied, Edit fills it and marks the bubble.
    view.update_in(cx, |view, _, cx| {
        view.composer
            .update(cx, |input, cx| input.set_value(String::new(), cx))
    });
    cx.run_until_parked();
    press(cx, format!("edit-message-{row}"));
    assert_eq!(composer(&view, cx), "fix the typo");
    assert_eq!(
        statuses(&view, session, cx),
        [("fix the typo".into(), crate::conversation::EDITED)]
    );
    view.read_with(cx, |view, cx| {
        assert!(
            view.state
                .read(cx)
                .session(session)
                .unwrap()
                .pending()
                .is_empty()
        );
    });
    assert!(
        cx.debug_bounds(Box::leak(format!("edit-message-{row}").into_boxed_str()))
            .is_none()
    );
    assert!(
        cx.debug_bounds(Box::leak(format!("send-again-{row}").into_boxed_str()))
            .is_none()
    );
}

/// Presses and releases each key, as the transcript's row actions act on release.
fn keys(cx: &mut gpui::VisualTestContext, strokes: &str) {
    for stroke in strokes.split_whitespace() {
        cx.simulate_keystrokes(stroke);
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse(stroke).unwrap(),
        });
        cx.run_until_parked();
    }
}

/// A machine that refuses the first message and takes every later one, logging the
/// app's lines, for a session it ran nothing in before.
fn refuse_then_take(session: u64) -> (Scripted, String) {
    use keystone_agent::remote::frame::Outcome;
    let host = Scripted::new();
    host.each("refuse", |id| {
        vec![reply(id, Outcome::Err("refused".into()))]
    });
    host.each("take", |id| taken(id, session, 1, "by keyboard"));
    let script = format!(
        r#"n=0
while read -r line; do
  printf '%s\n' "$line" >> "$0/lines"
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      n=$((n+1))
      if [ "$n" -eq 1 ]; then cat "$0/refuse-$id"; else cat "$0/take-$id"; fi;;
  esac
done"#
    );
    (host, script)
}

/// Send again is reached from the keyboard: into the transcript, to the refused
/// message, Tab past its Copy to Send again, and Enter (on release) sends it again,
/// under its ID, to the end.
#[gpui::test]
fn send_again_is_reached_from_the_keyboard(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::{Completed, Failed};
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let (host, script) = refuse_then_take(session);
    let (view, cx) = window(dir.path(), session, host.argv(&script), cx);
    wait_window(cx, &view, "the link", |state| {
        state
            .links
            .values()
            .any(|l| l.status() == LinkStatus::Connected)
    });
    cx.simulate_input("by keyboard");
    cx.simulate_keystrokes("secondary-enter");
    wait_window(cx, &view, "the refusal", |state| {
        bubbles(state, session) == [("by keyboard".into(), Failed)]
    });
    let send = view.read_with(cx, |view, cx| {
        view.state.read(cx).session(session).unwrap().pending()[0]
            .id
            .clone()
    });
    keys(cx, "secondary-shift-t end tab tab enter");
    wait_window(cx, &view, "sent again", |state| {
        bubbles(state, session) == [("by keyboard".into(), Completed)]
    });
    let lines = host.lines();
    assert_eq!(
        lines.matches(&format!(r#""send":"{send}""#)).count(),
        2,
        "{lines}"
    );
    assert_eq!(composer(&view, cx), "");
}

/// Edit is reached from the keyboard too: Copy, Send again, then Edit; Enter takes the
/// refused message into the empty composer.
#[gpui::test]
fn edit_is_reached_from_the_keyboard(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::Failed;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let (host, script) = refuse_then_take(session);
    let (view, cx) = window(dir.path(), session, host.argv(&script), cx);
    wait_window(cx, &view, "the link", |state| {
        state
            .links
            .values()
            .any(|l| l.status() == LinkStatus::Connected)
    });
    cx.simulate_input("edit by keyboard");
    cx.simulate_keystrokes("secondary-enter");
    wait_window(cx, &view, "the refusal", |state| {
        bubbles(state, session) == [("edit by keyboard".into(), Failed)]
    });
    keys(cx, "secondary-shift-t end tab tab tab enter");
    assert_eq!(composer(&view, cx), "edit by keyboard");
    assert_eq!(
        statuses(&view, session, cx),
        [("edit by keyboard".into(), crate::conversation::EDITED)]
    );
}

/// Discard is reached from the keyboard: a message whose machine can't be reached,
/// Tab past its Copy to Discard, and Enter gives it up.
#[gpui::test]
fn discard_is_reached_from_the_keyboard(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::Interrupted;
    let (session, dir, _) = quit_with_one_pending("discard by keyboard", cx);
    let host = Scripted::new();
    let down = dir.path().join("down");
    std::fs::write(&down, "").unwrap();
    let (view, cx) = window(
        dir.path(),
        session,
        host.argv_unless(&down, "while read -r line; do :; done"),
        cx,
    );
    wait_window(cx, &view, "the link to fail", |state| {
        state.unreached(session).is_some() && !state.conversations[&session].is_loading()
    });
    keys(cx, "secondary-shift-t end tab tab enter");
    assert_eq!(
        statuses(&view, session, cx),
        [("discard by keyboard".into(), Interrupted)]
    );
    view.read_with(cx, |view, cx| {
        assert!(
            view.state
                .read(cx)
                .session(session)
                .unwrap()
                .pending()
                .is_empty()
        );
    });
}

/// While its machine can't be reached, Send again is off, for the reason the composer's
/// Send gives, and the keyboard skips it; Edit still works.
#[gpui::test]
fn send_again_is_off_while_the_machine_cant_be_reached(cx: &mut gpui::TestAppContext) {
    use keystone_agent::ItemStatus::Failed;
    use keystone_agent::remote::frame::Outcome;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let host = Scripted::new();
    host.each("refuse", |id| {
        vec![reply(id, Outcome::Err("refused".into()))]
    });
    let script = format!(
        r#"while read -r line; do
  printf '%s\n' "$line" >> "$0/lines"
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      cat "$0/refuse-$id";;
  esac
done"#
    );
    let state = runtime(dir.path(), cx);
    state.update(cx, |state, _| {
        state.test_transport = Some(host.argv(&script))
    });
    connected(&state, cx);
    state.update(cx, |state, cx| {
        assert!(send_now(state, session, "while away", cx))
    });
    wait_until(cx, &state, "the refusal", |state| {
        bubbles(state, session) == [("while away".into(), Failed)]
    });
    quit(state, false, cx);
    let down = dir.path().join("down");
    std::fs::write(&down, "").unwrap();
    let (view, cx) = window(dir.path(), session, host.argv_unless(&down, &script), cx);
    wait_window(cx, &view, "the link to fail", |state| {
        state.offline(session).is_some() && !state.conversations[&session].is_loading()
    });
    let (row, send) = view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        let send = state.session(session).unwrap().pending()[0].id.clone();
        (row_of(state, session, &send), send)
    });
    view.read_with(cx, |view, cx| {
        let reason = view.state.read(cx).offline(session).unwrap();
        assert!(reason.contains("desk"), "{reason}");
        assert_eq!(
            view.message_actions(session, row, cx),
            [view_state::CopyTarget::EditMessage],
            "the keyboard skips Send again"
        );
    });
    press(cx, format!("send-again-{row}"));
    let refused = view.update(cx, |view, cx| {
        view.state
            .update(cx, |state, cx| state.send_again(session, &send, cx))
    });
    assert!(!refused, "it doesn't send");
    assert_eq!(
        statuses(&view, session, cx),
        [("while away".into(), Failed)]
    );
    assert_eq!(host.lines().matches(r#""op":"send""#).count(), 1);
}

/// A refused message with an image has its "Not sent" line, with Send again and Edit,
/// under the image, not between the bubble and it.
#[gpui::test]
fn a_refused_messages_status_sits_under_its_image(cx: &mut gpui::TestAppContext) {
    use keystone_agent::remote::frame::Outcome;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let host = Scripted::new();
    host.each("refuse", |id| {
        vec![reply(id, Outcome::Err("refused".into()))]
    });
    let script = format!(
        r#"while read -r line; do
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      cat "$0/refuse-$id";;
  esac
done"#
    );
    let (view, cx) = window(dir.path(), session, host.argv(&script), cx);
    wait_window(cx, &view, "the link", |state| {
        state
            .links
            .values()
            .any(|l| l.status() == LinkStatus::Connected)
    });
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            let picture = state
                .attachments
                .import_bytes(b"\x89PNG\r\n\x1a\nunder the image", "picture.png".into())
                .unwrap();
            assert!(state.send_message(
                ComposerDraft {
                    session,
                    text: "with a picture".into(),
                    attachments: vec![picture],
                },
                cx
            ));
        })
    });
    wait_window(cx, &view, "the refusal", |state| {
        state
            .session(session)
            .is_some_and(|s| s.pending().iter().all(|p| p.refused))
    });
    cx.run_until_parked();
    let row = view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        row_of(
            state,
            session,
            &state.session(session).unwrap().pending()[0].id,
        )
    });
    let image = cx
        .debug_bounds(Box::leak(
            format!("sent-attachments-{}", row + 1).into_boxed_str(),
        ))
        .expect("the image row");
    let again = cx
        .debug_bounds(Box::leak(format!("send-again-{row}").into_boxed_str()))
        .expect("Send again");
    assert!(
        again.top() >= image.bottom(),
        "Send again {again:?} is under the image {image:?}"
    );
}

/// A message taken out to edit whose machine then shows it ran says so, in one muted
/// line.
#[gpui::test]
fn an_edited_message_that_ran_says_so(cx: &mut gpui::TestAppContext) {
    use keystone_agent::remote::frame::Outcome;
    let dir = tempfile::tempdir().unwrap();
    let session = remote_session(dir.path());
    let host = Scripted::new();
    host.each("refuse", |id| {
        vec![reply(id, Outcome::Err("refused".into()))]
    });
    let mut later = ran(session, 1, "SENDID", "ran anyway");
    later.truncate(4);
    host.write("later", &later);
    let script = format!(
        r#"while read -r line; do
  case "$line" in
    *'"op":"send"'*)
      {CALL_ID}
      send=$(printf '%s' "$line" | sed -n 's/.*"send":"\([^"]*\)".*/\1/p')
      cat "$0/refuse-$id"
      until [ -e "$0/go" ]; do sleep 0.05; done
      sed "s/SENDID/$send/" "$0/later";;
  esac
done"#
    );
    let (view, cx) = window(dir.path(), session, host.argv(&script), cx);
    wait_window(cx, &view, "the link", |state| {
        state
            .links
            .values()
            .any(|l| l.status() == LinkStatus::Connected)
    });
    cx.simulate_input("ran anyway");
    cx.simulate_keystrokes("secondary-enter");
    wait_window(cx, &view, "the refusal", |state| {
        bubbles(state, session) == [("ran anyway".into(), keystone_agent::ItemStatus::Failed)]
    });
    let row = view.read_with(cx, |view, cx| {
        let state = view.state.read(cx);
        row_of(
            state,
            session,
            &state.session(session).unwrap().pending()[0].id,
        )
    });
    press(cx, format!("edit-message-{row}"));
    assert_eq!(composer(&view, cx), "ran anyway");
    assert!(
        cx.debug_bounds(Box::leak(format!("ran-after-all-{row}").into_boxed_str()))
            .is_none()
    );
    std::fs::write(host.0.path().join("go"), "").unwrap();
    wait_window(cx, &view, "the machine's record", |state| {
        state.conversations[&session].ran_after_edit.contains(&row)
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds(Box::leak(format!("ran-after-all-{row}").into_boxed_str()))
            .is_some()
    );
}
