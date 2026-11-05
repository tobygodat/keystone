#![cfg(unix)]

use keystone_agent::{ApprovalState, ClaudeConnection, CodexConnection, Connection, Event};
use keystone_core::ModelChoice;
use serde::Deserialize;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

#[derive(Deserialize)]
struct Fixture {
    provider: String,
    version: String,
    outcome: String,
    range: String,
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn collect(
    conn: &dyn Connection,
    rx: &Receiver<()>,
    done: impl Fn(&Event) -> bool,
) -> Vec<(u64, Event)> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut events = Vec::new();
    loop {
        let batch = conn.drain();
        let finished = batch.iter().any(|(_, event)| done(event));
        events.extend(batch);
        if finished {
            return events;
        }
        assert!(
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_ok(),
            "timed out: {events:?}"
        );
    }
}

#[test]
fn scripted_versions_warn_or_refuse_before_sending_and_gate_optional_fields() {
    let fixtures: Vec<Fixture> =
        serde_json::from_str(include_str!("fixtures/provider-versions.json")).unwrap();
    for fixture in fixtures {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("provider");
        let log = dir.path().join("initialize.json");
        let (binary, variable) = if fixture.provider == "claude" {
            (env!("CARGO_BIN_EXE_keystone-fake-claude"), "CLAUDE")
        } else {
            (env!("CARGO_BIN_EXE_keystone-fake-codex"), "CODEX")
        };
        script(
            &program,
            &format!(
                "export KEYSTONE_FAKE_{variable}_VERSION='{}'\nexport KEYSTONE_FAKE_{variable}_DELAY_MS=1\nexport KEYSTONE_FAKE_CODEX_INITIALIZE_LOG='{}'\nexec '{binary}' \"$@\"",
                fixture.version,
                log.display()
            ),
        );
        let (tx, rx) = channel();
        let wake = Arc::new(move || {
            let _ = tx.send(());
        });
        let conn: Box<dyn Connection> = if fixture.provider == "claude" {
            Box::new(ClaudeConnection::new(Some(program), wake))
        } else {
            Box::new(CodexConnection::new(Some(program), "test", wake))
        };
        conn.send(
            1,
            dir.path().to_str().unwrap(),
            None,
            &ModelChoice::default(),
            ApprovalState::default(),
            "hello".into(),
            Vec::new(),
        )
        .unwrap();
        let events = collect(conn.as_ref(), &rx, |e| {
            matches!(e, Event::TurnCompleted { .. } | Event::Failed { .. })
        });
        if fixture.outcome == "incompatible" {
            assert!(
                !events
                    .iter()
                    .any(|(_, e)| matches!(e, Event::TurnStarted | Event::ThreadReady { .. }))
            );
            assert!(!log.exists());
            assert!(events.iter().any(|(_, e)| matches!(e, Event::Failed { message } if message.contains("incompatible") && message.contains(&fixture.version) && message.contains(&fixture.range) && message.contains("Install"))));
            conn.request_catalog(1, dir.path().to_str().unwrap());
            let catalog = collect(conn.as_ref(), &rx, |e| matches!(e, Event::Catalog(_)));
            assert!(catalog.iter().any(|(_, e)| matches!(e, Event::Catalog(keystone_agent::Catalog::Failed(message)) if message.contains("incompatible") && message.contains(&fixture.version))));
            continue;
        }
        assert!(
            events.iter().any(|(_, e)| matches!(
                e,
                Event::TurnCompleted {
                    status: keystone_agent::TurnStatus::Completed,
                    ..
                }
            )),
            "{events:?}"
        );
        let notice = events.iter().find_map(|(_, e)| match e {
            Event::ProviderNotice { message } => message.as_deref(),
            _ => None,
        });
        if fixture.outcome == "tested" {
            assert_eq!(notice, None);
        } else {
            let notice = notice.unwrap();
            assert!(
                notice.contains(&fixture.version) && notice.contains(&fixture.range),
                "{notice}"
            );
            assert!(notice.contains(if fixture.outcome == "untested" {
                "is newer than tested"
            } else {
                "could not be verified"
            }));
        }
        if fixture.provider == "codex" {
            let init: Value = serde_json::from_slice(&std::fs::read(log).unwrap()).unwrap();
            assert!(init.get("capabilities").is_some());
            // Only versions checked to take the experimental `dynamicTools` field opt in.
            let widgets = matches!(fixture.version.as_str(), "0.163.0" | "0.163.1");
            assert_eq!(
                init["capabilities"].get("experimentalApi").is_some(),
                widgets,
                "{}",
                fixture.version
            );
            conn.send(
                2,
                dir.path().to_str().unwrap(),
                None,
                &ModelChoice::default(),
                ApprovalState::default(),
                "hello".into(),
                Vec::new(),
            )
            .unwrap();
            let second = collect(conn.as_ref(), &rx, |e| {
                matches!(e, Event::TurnCompleted { .. })
            });
            assert!(!second.iter().any(|(session, e)| *session == 2
                && matches!(e, Event::ProviderNotice { message: Some(_) })));
        }
    }
}

#[test]
fn unavailable_version_probe_is_bounded_and_does_not_prevent_a_safe_turn() {
    for behavior in [
        "sleep 5",
        "exit 0",
        "echo 'codex-cli 0.162.1'; exit 1",
        "echo 'development build' >&2; exit 0",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("provider");
        script(
            &program,
            &format!(
                "if [ \"$1\" = --version ]; then {behavior}; exit 0; fi\nexport KEYSTONE_FAKE_CODEX_DELAY_MS=1\nexec '{}' \"$@\"",
                env!("CARGO_BIN_EXE_keystone-fake-codex")
            ),
        );
        let (tx, rx) = channel();
        let conn = CodexConnection::new(
            Some(program),
            "test",
            Arc::new(move || {
                let _ = tx.send(());
            }),
        );
        Connection::send(
            &conn,
            1,
            dir.path().to_str().unwrap(),
            None,
            &ModelChoice::default(),
            ApprovalState::default(),
            "hello".into(),
            Vec::new(),
        )
        .unwrap();
        let events = collect(&conn, &rx, |e| matches!(e, Event::TurnCompleted { .. }));
        assert!(events.iter().any(|(_, e)| matches!(e, Event::ProviderNotice { message: Some(message) } if message.contains("could not be verified") && message.contains("0.162.1–0.163.0"))));
    }
}

#[test]
fn claude_handshake_controls_queued_cancellation_and_safe_steering() {
    for (version, missing) in [
        ("2.1.302", false),
        ("2.1.302", true),
        ("2.1.301", false),
        ("2.1.300", true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("claude");
        let log = dir.path().join("interrupt.json");
        script(
            &program,
            &format!(
                "export KEYSTONE_FAKE_CLAUDE_VERSION={version}\nexport KEYSTONE_FAKE_CLAUDE_NO_CANCEL_QUEUED={}\nexport KEYSTONE_FAKE_CLAUDE_SUBAGENT_INIT=1\nexport KEYSTONE_FAKE_CLAUDE_DELAY_MS=30\nexport KEYSTONE_FAKE_CLAUDE_INTERRUPT_LOG='{}'\nexec '{}' \"$@\"",
                u8::from(missing),
                log.display(),
                env!("CARGO_BIN_EXE_keystone-fake-claude")
            ),
        );
        let (tx, rx) = channel();
        let conn = ClaudeConnection::new(
            Some(program),
            Arc::new(move || {
                let _ = tx.send(());
            }),
        );
        conn.send(
            1,
            dir.path().to_str().unwrap(),
            None,
            &ModelChoice::default(),
            ApprovalState::default(),
            "long".into(),
        )
        .unwrap();
        collect(&conn, &rx, |e| matches!(e, Event::ItemDelta { .. }));
        let steer = conn.send(
            1,
            dir.path().to_str().unwrap(),
            None,
            &ModelChoice::default(),
            ApprovalState::default(),
            "long change".into(),
        );
        if missing {
            assert!(steer.unwrap_err().contains("interrupt_cancel_queued_v1"));
        } else {
            steer.unwrap();
        }
        conn.command(keystone_agent::Command::Interrupt { session: 1 });
        collect(&conn, &rx, |e| matches!(e, Event::TurnCompleted { .. }));
        let request: Value = serde_json::from_slice(&std::fs::read(log).unwrap()).unwrap();
        assert_eq!(request.get("cancel_queued").is_some(), !missing);
    }
}

#[test]
fn rejected_codex_notification_capability_retries_plain_handshake_without_losing_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("codex");
    let log = dir.path().join("initialize.json");
    script(
        &program,
        &format!(
            "export KEYSTONE_FAKE_CODEX_VERSION=0.163.1\nexport KEYSTONE_FAKE_CODEX_NO_NOTIFICATION_FILTER=1\nexport KEYSTONE_FAKE_CODEX_DELAY_MS=1\nexport KEYSTONE_FAKE_CODEX_INITIALIZE_LOG='{}'\nexec '{}' \"$@\"",
            log.display(),
            env!("CARGO_BIN_EXE_keystone-fake-codex")
        ),
    );
    let (tx, rx) = channel();
    let conn = CodexConnection::new(
        Some(program),
        "test",
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    Connection::send(
        &conn,
        1,
        dir.path().to_str().unwrap(),
        None,
        &ModelChoice::default(),
        ApprovalState::default(),
        "hello".into(),
        Vec::new(),
    )
    .unwrap();
    let events = collect(&conn, &rx, |e| matches!(e, Event::TurnCompleted { .. }));
    assert!(events.iter().any(|(_, e)| matches!(
        e,
        Event::TurnCompleted {
            status: keystone_agent::TurnStatus::Completed,
            ..
        }
    )));
    let initialize: Value = serde_json::from_slice(&std::fs::read(log).unwrap()).unwrap();
    assert!(initialize.get("capabilities").is_none());
}

#[test]
fn observed_non_inference_handshakes_accept_the_installed_cli_versions_and_early_hooks() {
    use keystone_agent::{Outputs, claude::ClaudeDriver, codex::CodexDriver};
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/provider-connect-probes.json")).unwrap();
    let mut out = Outputs::default();
    let mut codex = CodexDriver::new("probe", &mut out);
    let initialize: Value = serde_json::from_str(&out.writes.remove(0)).unwrap();
    assert!(initialize["params"]["capabilities"]["optOutNotificationMethods"].is_array());
    codex.on_line(
        fixture["codex"]["initialize_with_notification_opt_out"]
            .to_string()
            .as_bytes(),
        &mut out,
    );
    assert_eq!(
        out.writes,
        [
            r#"{"method":"initialized"}"#,
            r#"{"id":"keystone-rate-limits","method":"account/rateLimits/read","params":{"excludeResetCreditDetails":true}}"#
        ]
    );
    assert!(out.events.is_empty());
    let mut out = Outputs::default();
    let mut claude =
        ClaudeDriver::new(1, false, 1, keystone_core::ApprovalMode::Provider, &mut out);
    let initialize: Value = serde_json::from_str(&out.writes.remove(0)).unwrap();
    for original in fixture["claude"]["frames"].as_array().unwrap() {
        let mut frame = original.clone();
        if frame["type"] == "control_response" {
            frame["response"]["request_id"] = initialize["request_id"].clone();
        }
        claude.on_line(frame.to_string().as_bytes(), &mut out);
    }
    assert!(!claude.is_running());
    let written: Vec<Value> = out
        .writes
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        written,
        [
            serde_json::json!({"type": "control_request", "request_id": "keystone-usage",
            "request": {"subtype": "get_usage", "skip_behaviors": true}})
        ]
    );
    assert!(
        !out.events
            .iter()
            .any(|(_, e)| matches!(e, Event::Failed { .. } | Event::TurnStarted))
    );
}

#[test]
fn a_tested_claude_reconnect_clears_the_run_local_notice() {
    let dir = tempfile::tempdir().unwrap();
    let version = dir.path().join("version");
    std::fs::write(&version, "development-build").unwrap();
    let program = dir.path().join("claude");
    script(
        &program,
        &format!(
            "export KEYSTONE_FAKE_CLAUDE_VERSION=\"$(cat '{}')\"\nexport KEYSTONE_FAKE_CLAUDE_DELAY_MS=1\nexec '{}' \"$@\"",
            version.display(),
            env!("CARGO_BIN_EXE_keystone-fake-claude")
        ),
    );
    let (tx, rx) = channel();
    let conn = ClaudeConnection::new(
        Some(program),
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    conn.send(
        1,
        dir.path().to_str().unwrap(),
        None,
        &ModelChoice::default(),
        ApprovalState::default(),
        "hello".into(),
    )
    .unwrap();
    let first = collect(&conn, &rx, |e| matches!(e, Event::TurnCompleted { .. }));
    assert!(
        first
            .iter()
            .any(|(_, e)| matches!(e, Event::ProviderNotice { message: Some(_) }))
    );
    std::fs::write(version, "2.1.301").unwrap();
    conn.send(
        1,
        dir.path().to_str().unwrap(),
        None,
        &ModelChoice::default(),
        ApprovalState {
            mode: keystone_core::ApprovalMode::FullAccess,
            overridden: true,
        },
        "hello".into(),
    )
    .unwrap();
    let second = collect(&conn, &rx, |e| matches!(e, Event::TurnCompleted { .. }));
    assert!(
        second
            .iter()
            .any(|(_, e)| matches!(e, Event::ProviderNotice { message: None }))
    );
}

#[test]
fn a_claude_version_timeout_does_not_prevent_the_handshake_and_turn() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("claude");
    script(
        &program,
        &format!(
            "if [ \"$1\" = --version ]; then sleep 5; exit 0; fi\nexport KEYSTONE_FAKE_CLAUDE_DELAY_MS=1\nexec '{}' \"$@\"",
            env!("CARGO_BIN_EXE_keystone-fake-claude")
        ),
    );
    let (tx, rx) = channel();
    let conn = ClaudeConnection::new(
        Some(program),
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    conn.send(
        1,
        dir.path().to_str().unwrap(),
        None,
        &ModelChoice::default(),
        ApprovalState::default(),
        "hello".into(),
    )
    .unwrap();
    let events = collect(&conn, &rx, |e| matches!(e, Event::TurnCompleted { .. }));
    assert!(events.iter().any(|(_, e)| matches!(e, Event::ProviderNotice { message: Some(message) } if message.contains("timed out"))));
    assert!(events.iter().any(|(_, e)| matches!(
        e,
        Event::TurnCompleted {
            status: keystone_agent::TurnStatus::Completed,
            ..
        }
    )));
}

/// An import listing runs on the app-server for no session, so a version notice
/// waits for the first session that can show it.
#[test]
fn a_version_notice_is_kept_for_the_first_session_after_an_import_listing() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("provider");
    script(
        &program,
        &format!(
            "if [ \"$1\" = --version ]; then echo 'development build' >&2; exit 0; fi\nexport KEYSTONE_FAKE_CODEX_DELAY_MS=1 CODEX_HOME='{}'\nexec '{}' \"$@\"",
            dir.path().display(),
            env!("CARGO_BIN_EXE_keystone-fake-codex")
        ),
    );
    let (tx, rx) = channel();
    let conn = CodexConnection::new(
        Some(program),
        "test",
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    conn.list_threads(dir.path().to_str().unwrap(), 1);
    let listed = collect(&conn, &rx, |e| matches!(e, Event::ThreadsListed { .. }));
    assert!(
        !listed
            .iter()
            .any(|(_, e)| matches!(e, Event::ProviderNotice { .. }))
    );
    Connection::send(
        &conn,
        1,
        dir.path().to_str().unwrap(),
        None,
        &ModelChoice::default(),
        ApprovalState::default(),
        "hello".into(),
        Vec::new(),
    )
    .unwrap();
    let events = collect(&conn, &rx, |e| matches!(e, Event::TurnCompleted { .. }));
    assert!(events.iter().any(|(session, e)| *session == 1
        && matches!(e, Event::ProviderNotice { message: Some(message) } if message.contains("could not be verified"))));
}
