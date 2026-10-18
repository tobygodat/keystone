//! The full stack against a scripted Claude older than `msg_lifecycle_v1`. Its own
//! file, because the fake reads the setting from the environment.

use keystone_agent::ApprovalState;
use keystone_agent::{ClaudeConnection, Event, ItemKind, TurnStatus};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

const NO_MODE: ApprovalState = ApprovalState {
    mode: keystone_core::ApprovalMode::Provider,
    overridden: false,
};

/// Without lifecycle frames a steer would be mis-grouped, so the connection refuses it
/// and the caller keeps the text. The running turn goes on and ends normally.
///
/// The fake holds its reply after the first word until the release file exists, so
/// the steer is attempted mid-reply without a timing margin.
#[test]
fn a_claude_without_lifecycle_frames_refuses_steers() {
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("release");
    // SAFETY: the only test in this file.
    unsafe {
        std::env::set_var("KEYSTONE_FAKE_CLAUDE_DELAY_MS", "0");
        std::env::set_var("KEYSTONE_FAKE_CLAUDE_NO_LIFECYCLE", "1");
        std::env::set_var("KEYSTONE_FAKE_CLAUDE_RELEASE_FILE", &release);
    }
    let (tx, wake) = channel();
    let conn = ClaudeConnection::new(
        Some(PathBuf::from(env!("CARGO_BIN_EXE_keystone-fake-claude"))),
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    conn.send(1, "/tmp", None, None, NO_MODE, "long".into())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    let mut steered = false;
    loop {
        let batch: Vec<Event> = conn.drain().into_iter().map(|(_, e)| e).collect();
        for (i, event) in batch.iter().enumerate() {
            if !steered && matches!(event, Event::ItemDelta { .. }) {
                let error = conn
                    .send(1, "/tmp", None, None, NO_MODE, "change course".into())
                    .unwrap_err();
                assert!(error.contains("msg_lifecycle_v1"), "{error}");
                steered = true;
                // The rest of this batch was drained before the release, so with the
                // hold working it carries no further delta. A broken hold shows up here
                // only when later words landed in the same batch.
                let early = batch[i + 1..]
                    .iter()
                    .filter(|e| matches!(e, Event::ItemDelta { .. }))
                    .count();
                assert_eq!(
                    early, 0,
                    "deltas past the first word arrived before release"
                );
                std::fs::write(&release, "").unwrap();
            }
        }
        events.extend(batch);
        if events
            .iter()
            .any(|e| matches!(e, Event::TurnCompleted { .. }))
        {
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(wake.recv_timeout(left).is_ok(), "timed out; got {events:?}");
    }
    assert!(steered);
    assert_eq!(
        events.last(),
        Some(&Event::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
            stopped: false,
        })
    );
    let reply = events.iter().rev().find_map(|e| match e {
        Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage => Some(&item.text),
        _ => None,
    });
    assert!(
        reply.unwrap().ends_with("word200"),
        "the turn was not cut short"
    );
    assert!(!conn.is_running(1));
    // Idle again, so a message starts the next turn.
    conn.send(1, "/tmp", None, None, NO_MODE, "hello".into())
        .unwrap();
}
