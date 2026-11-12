#![cfg(unix)]

use keystone_agent::{ApprovalState, CodexConnection, Event};
use keystone_core::ModelChoice;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[test]
fn incompatible_codex_is_refused_before_a_thread_or_turn_starts() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("codex");
    keystone_core::test_support::write_executable(
        &program,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.20.0'; exit 0; fi\nexec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_keystone-fake-codex")
        ),
    );
    let (tx, rx) = mpsc::channel();
    let connection = CodexConnection::new(
        Some(program),
        "test",
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    connection.send(
        1,
        dir.path().to_str().unwrap(),
        None,
        &ModelChoice::default(),
        ApprovalState::default(),
        "hello".into(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        for (_, event) in connection.drain() {
            match event {
                Event::Failed { message } => {
                    assert!(
                        message.contains("0.20.0")
                            && message.contains("incompatible")
                            && message.contains("Install")
                    );
                    return;
                }
                Event::ThreadReady { .. } | Event::TurnStarted => {
                    panic!("Incompatible Codex was allowed to start: {event:?}")
                }
                _ => {}
            }
        }
        assert!(
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_ok()
        );
    }
}
