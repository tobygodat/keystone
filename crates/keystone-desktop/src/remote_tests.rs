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
        let dir = self.0.path().to_str().unwrap();
        vec![
            "env".into(),
            format!("KEYSTONE_HOST_DIR={dir}").into(),
            format!("HOME={dir}").into(),
            format!(
                "KEYSTONE_CLAUDE_PATH={}",
                binary("keystone-fake-claude").display()
            )
            .into(),
            binary("keystone-host").into(),
            "attach".into(),
        ]
    }

    /// The daemon's process, while it holds its lock.
    fn daemon(&self) -> Option<i32> {
        let lock = self.0.path().join("run").join("host.lock");
        std::fs::read_to_string(lock).ok()?.trim().parse().ok()
    }
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
            "speaks protocol 99, and Keystone needs 1",
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
