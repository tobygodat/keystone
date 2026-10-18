//! The full stack against the scripted app-server: process host, connection, and driver.

mod gate;
use gate::Gate;
use keystone_agent::ApprovalState;
use keystone_agent::Command;
use keystone_agent::{Catalog, CodexConnection, Event, ItemKind, ModelInfo, TurnStatus};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const NO_MODE: ApprovalState = ApprovalState {
    mode: keystone_core::ApprovalMode::Provider,
    overridden: false,
};

fn connect() -> (CodexConnection, Receiver<()>) {
    // SAFETY: tests in this file set the same value and run no other code that reads it.
    unsafe { std::env::set_var("KEYSTONE_FAKE_CODEX_DELAY_MS", "20") };
    let (tx, rx) = channel();
    let wake = Arc::new(move || {
        let _ = tx.send(());
    });
    let program = PathBuf::from(env!("CARGO_BIN_EXE_keystone-fake-codex"));
    (CodexConnection::new(Some(program), "0.0.0-test", wake), rx)
}

/// Collects events until `done` matches one, waiting on wake-ups.
fn until(conn: &CodexConnection, wake: &Receiver<()>, done: impl Fn(&Event) -> bool) -> Vec<Event> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    loop {
        let batch = conn.drain();
        let finished = batch.iter().any(|(_, e)| done(e));
        events.extend(batch.into_iter().map(|(_, e)| e));
        if finished {
            return events;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(wake.recv_timeout(left).is_ok(), "timed out; got {events:?}");
    }
}

fn completed(event: &Event) -> bool {
    matches!(event, Event::TurnCompleted { .. })
}

#[test]
fn turns_stream_steer_stop_and_ask_for_approval() {
    let (conn, wake) = connect();
    conn.command(Command::Open {
        session: 1,
        cwd: "/tmp".into(),
        resume: None,
        model: None,
    });
    conn.command(Command::Send {
        session: 1,
        text: "hello".into(),
    });
    let events = until(&conn, &wake, completed);
    assert!(matches!(events[0], Event::ThreadReady { .. }));
    let reply = events.iter().find_map(|e| match e {
        Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage => {
            Some(item.text.clone())
        }
        _ => None,
    });
    assert!(reply.unwrap().contains("You wrote: hello"));

    // A message during the turn steers it.
    conn.command(Command::Send {
        session: 1,
        text: "long".into(),
    });
    until(&conn, &wake, |e| matches!(e, Event::ItemDelta { .. }));
    conn.command(Command::Send {
        session: 1,
        text: "change course".into(),
    });
    let events = until(&conn, &wake, completed);
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.text == "Steered (test data): change course")));

    // Stop ends the turn as interrupted.
    conn.command(Command::Send {
        session: 1,
        text: "another".into(),
    });
    until(&conn, &wake, |e| matches!(e, Event::ItemDelta { .. }));
    assert!(conn.is_running(1));
    conn.command(Command::Interrupt { session: 1 });
    let events = until(&conn, &wake, completed);
    assert_eq!(
        events.last(),
        Some(&Event::TurnCompleted {
            status: TurnStatus::Interrupted,
            error: None,
            stopped: true
        })
    );

    // A command needs approval; allowing it runs it.
    conn.command(Command::Send {
        session: 1,
        text: "please run it".into(),
    });
    let events = until(&conn, &wake, |e| matches!(e, Event::ApprovalRequested(_)));
    let Some(Event::ApprovalRequested(approval)) = events.last() else {
        unreachable!()
    };
    assert_eq!(approval.action, "echo scripted");
    conn.command(Command::Answer {
        session: 1,
        request: approval.request.clone(),
        allow: true,
    });
    let events = until(&conn, &wake, completed);
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.text == "scripted\n")));
}

#[test]
fn two_sessions_share_one_process() {
    let (conn, wake) = connect();
    for session in [1, 2] {
        conn.command(Command::Open {
            session,
            cwd: "/tmp".into(),
            resume: None,
            model: None,
        });
        conn.command(Command::Send {
            session,
            text: format!("session {session}"),
        });
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut done = Vec::new();
    while done.len() < 2 {
        for (session, event) in conn.drain() {
            if completed(&event) {
                done.push(session);
            }
        }
        if done.len() < 2 {
            assert!(wake.recv_timeout(deadline - Instant::now()).is_ok());
        }
    }
    done.sort();
    assert_eq!(done, vec![1, 2]);
}

#[test]
fn dropping_the_connection_stops_codex() {
    let (conn, wake) = connect();
    conn.send(1, "/tmp", None, None, NO_MODE, "hello".into());
    until(&conn, &wake, |e| matches!(e, Event::ThreadReady { .. }));
    let pid = conn.pid().expect("app-server running");
    drop(conn);
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        if state
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        {
            break;
        }
        assert!(Instant::now() < deadline, "app-server {pid} still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn after_a_crash_the_latest_thread_is_resumed_even_if_the_caller_has_none() {
    let (conn, wake) = connect();
    conn.send(1, "/tmp", None, None, NO_MODE, "hello".into());
    let events = until(&conn, &wake, completed);
    let Some(Event::ThreadReady { thread_id: first }) = events.first() else {
        panic!("expected a thread first, got {events:?}");
    };
    let first = first.clone();
    let pid = conn.pid().unwrap();
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while conn.pid().is_some() {
        assert!(Instant::now() < deadline, "exit not noticed");
        let _ = wake.recv_timeout(Duration::from_millis(100));
        conn.drain();
    }
    // The caller passes no saved thread, as if the ID never reached its storage.
    conn.send(1, "/tmp", None, None, NO_MODE, "again".into());
    let events = until(&conn, &wake, completed);
    assert!(
        events.contains(&Event::ThreadReady {
            thread_id: first.clone()
        }),
        "expected a resume of {first}, got {events:?}"
    );
}

#[test]
fn a_codex_that_cannot_start_fails_the_session_that_needed_it() {
    let (tx, wake) = channel();
    let conn = CodexConnection::new(
        Some(PathBuf::from("/nonexistent/codex")),
        "0.0.0-test",
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    conn.send(7, "/tmp", None, None, NO_MODE, "hello".into());
    let deadline = Instant::now() + Duration::from_secs(5);
    let events = loop {
        let events = conn.drain();
        if !events.is_empty() {
            break events;
        }
        assert!(wake.recv_timeout(deadline - Instant::now()).is_ok());
    };
    assert!(
        matches!(&events[..], [(7, Event::Failed { message })]
            if message.starts_with("Could not start Codex at /nonexistent/codex.")),
        "got {events:?}"
    );
    assert!(conn.pid().is_none());
}

/// Dropping the connection gives up on a stuck start after a bounded wait.
#[test]
fn dropping_gives_up_on_a_stuck_start() {
    let (conn, _wake) = connect();
    let gate = Gate::new(None);
    conn.hold_starts(gate.hook());
    conn.send(1, "/tmp", None, None, NO_MODE, "hello".into());
    gate.reached(1);
    let started = Instant::now();
    drop(conn);
    let took = started.elapsed();
    gate.release(1);
    assert!(took < Duration::from_secs(3), "drop took {took:?}");
}

/// A scripted Codex that keeps thread overrides in `dir`, so a restarted process
/// resumes a thread with its override, as real Codex does.
fn connect_keeping_overrides(dir: &std::path::Path) -> (CodexConnection, Receiver<()>) {
    use std::os::unix::fs::PermissionsExt;
    unsafe { std::env::set_var("KEYSTONE_FAKE_CODEX_DELAY_MS", "20") };
    let script = dir.join("codex");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nKEYSTONE_FAKE_CODEX_STATE={} exec {} \"$@\"\n",
            dir.join("state.json").display(),
            env!("CARGO_BIN_EXE_keystone-fake-codex")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (tx, rx) = channel();
    let wake = Arc::new(move || {
        let _ = tx.send(());
    });
    (CodexConnection::new(Some(script), "0.0.0-test", wake), rx)
}

fn approval(mode: keystone_core::ApprovalMode) -> ApprovalState {
    ApprovalState {
        mode,
        overridden: true,
    }
}

/// The turn's last reply.
fn reply(events: &[Event]) -> &str {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage => Some(&item.text),
            _ => None,
        })
        .expect("a reply")
}

/// The backticked value after `label` in the turn's last reply, such as the policy
/// after `**Approvals:**`.
fn reported(events: &[Event], label: &str) -> String {
    let reply = reply(events);
    let start = reply.find(label).expect(label) + label.len();
    reply[start..].split('`').nth(1).unwrap().to_owned()
}

/// The policy the scripted Codex reports at the end of the turn's last reply.
fn policy_of(events: &[Event]) -> String {
    reported(events, "**Approvals:**")
}

fn model_of(events: &[Event]) -> String {
    reported(events, "**Model:**")
}

fn catalog(event: &Event) -> bool {
    matches!(event, Event::Catalog(_))
}

/// The chosen model opens the thread, and opens it again after a restart; the thread's
/// model comes back from Codex with the thread; and the catalog lists what Codex
/// offers, hidden entries left out.
#[test]
fn a_chosen_model_reaches_codex_on_start_and_resume_and_the_catalog_is_listed() {
    let (conn, wake) = connect();
    conn.request_catalog(1);
    let events = until(&conn, &wake, catalog);
    assert!(events.contains(&Event::Catalog(Catalog::Ready {
        models: vec![
            ModelInfo {
                model: "fake-default".into(),
                label: "Fake Default".into(),
                resolved: None,
            },
            ModelInfo {
                model: "fake-fast".into(),
                label: "Fake Fast".into(),
                resolved: None,
            },
        ],
        default_model: None,
    })));

    conn.send(1, "/tmp", None, Some("fake-fast"), NO_MODE, "hello".into());
    let events = until(&conn, &wake, completed);
    assert!(events.contains(&Event::ModelReported {
        model: "fake-fast".into()
    }));
    assert_eq!(model_of(&events), "fake-fast");
    conn.send(2, "/tmp", None, None, NO_MODE, "plain".into());
    let events = until(&conn, &wake, completed);
    assert_eq!(model_of(&events), "fake-default");

    // A new process resumes the thread with the same model.
    kill_codex(&conn, &wake);
    conn.send(1, "/tmp", None, Some("fake-fast"), NO_MODE, "again".into());
    let events = until(&conn, &wake, completed);
    assert_eq!(model_of(&events), "fake-fast");
}

/// A catalog Codex cannot list fails with Codex's own reason, so the user can see why
/// and ask again.
#[test]
fn a_catalog_codex_cannot_list_fails_with_its_reason() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("codex");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nKEYSTONE_FAKE_CODEX_NO_CATALOG=1 exec {} \"$@\"\n",
            env!("CARGO_BIN_EXE_keystone-fake-codex")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (tx, rx) = channel();
    let wake = Arc::new(move || {
        let _ = tx.send(());
    });
    let conn = CodexConnection::new(Some(script), "0.0.0-test", wake);
    conn.request_catalog(1);
    let events = until(&conn, &rx, catalog);
    assert!(events.contains(&Event::Catalog(Catalog::Failed(
        "scripted catalog failure".into()
    ))));

    let (tx, rx) = channel();
    let wake = Arc::new(move || {
        let _ = tx.send(());
    });
    let conn = CodexConnection::new(Some("/nonexistent/codex".into()), "0.0.0-test", wake);
    conn.request_catalog(1);
    let events = until(&conn, &rx, catalog);
    assert!(
        matches!(&events[..], [Event::Catalog(Catalog::Failed(message))]
            if message.starts_with("Could not start Codex at /nonexistent/codex.")),
        "got {events:?}"
    );
}

fn kill_codex(conn: &CodexConnection, wake: &Receiver<()>) {
    let pid = conn.pid().unwrap();
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while conn.pid().is_some() {
        assert!(Instant::now() < deadline, "exit not noticed");
        let _ = wake.recv_timeout(Duration::from_millis(100));
        conn.drain();
    }
}

/// Codex exits after the user chose Read only on a thread that ran with Full access.
/// The new process's driver must still send Read only; a resumed thread that got no
/// policy would keep Full access.
#[test]
fn a_mode_chosen_before_codex_restarts_reaches_the_new_process() {
    use keystone_core::ApprovalMode::{FullAccess, ReadOnly};
    let dir = tempfile::tempdir().unwrap();
    let (conn, wake) = connect_keeping_overrides(dir.path());
    conn.send(1, "/tmp", None, None, approval(FullAccess), "first".into());
    let events = until(&conn, &wake, completed);
    assert_eq!(policy_of(&events), "never / dangerFullAccess");

    conn.set_approval(1, approval(ReadOnly));
    kill_codex(&conn, &wake);
    conn.send(1, "/tmp", None, None, approval(ReadOnly), "second".into());
    let events = until(&conn, &wake, completed);
    assert_eq!(policy_of(&events), "on-request / readOnly");
}

/// Back to Provider settings after a restart: the resumed thread still holds the old
/// override, so the user's own settings are sent.
#[test]
fn provider_settings_after_a_restart_restore_the_users_own() {
    use keystone_core::ApprovalMode::{FullAccess, Provider};
    let dir = tempfile::tempdir().unwrap();
    let (conn, wake) = connect_keeping_overrides(dir.path());
    conn.send(1, "/tmp", None, None, approval(FullAccess), "first".into());
    let events = until(&conn, &wake, completed);
    let Some(Event::ThreadReady { thread_id }) = events.first().cloned() else {
        panic!("expected a thread first, got {events:?}");
    };
    drop(conn);
    // A new connection, as after Keystone restarts.
    let (conn, wake) = connect_keeping_overrides(dir.path());
    conn.send(
        1,
        "/tmp",
        Some(&thread_id),
        None,
        approval(Provider),
        "again".into(),
    );
    let events = until(&conn, &wake, completed);
    assert_eq!(policy_of(&events), "on-request / workspaceWrite");
}

/// A steer Codex rejects because the turn ended becomes the next turn. It runs under
/// the mode chosen after it was sent.
#[test]
fn a_rejected_steer_retried_as_a_turn_uses_the_mode_chosen_since() {
    use keystone_core::ApprovalMode::{FullAccess, ReadOnly};
    let (conn, wake) = connect();
    conn.send(
        1,
        "/tmp",
        None,
        None,
        approval(FullAccess),
        "end on steer".into(),
    );
    until(&conn, &wake, |e| matches!(e, Event::ItemDelta { .. }));
    conn.send(1, "/tmp", None, None, approval(FullAccess), "late".into());
    // Codex holds the steer for 300 ms. Choose the mode once it has gone out, so the
    // change must reach the running driver, not only the queued send.
    std::thread::sleep(Duration::from_millis(100));
    conn.set_approval(1, approval(ReadOnly));
    // The turn ends without finishing its reply, then the steer comes back.
    until(&conn, &wake, completed);
    let events = until(&conn, &wake, completed);
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted(item) if item.text.contains("You wrote: late"))));
    assert_eq!(policy_of(&events), "on-request / readOnly");
}
