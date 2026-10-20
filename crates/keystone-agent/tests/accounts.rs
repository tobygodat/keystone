//! Account isolation through the real process host and connections: every process
//! started for an account, including version checks and probes, gets that account's
//! home and no other, while the default account keeps the inherited environment.

use keystone_agent::{
    ApprovalState, Catalog, ClaudeConnection, CodexConnection, Event, LaunchProfile,
};
use keystone_core::{ApprovalMode, Provider};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const PROVIDER: ApprovalState = ApprovalState {
    mode: ApprovalMode::Provider,
    overridden: false,
};

/// The inherited default homes and a receipt file for this run, set once per binary
/// before either test starts a process.
fn environment() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("account-isolation");
    ONCE.call_once(|| {
        std::fs::create_dir_all(root.join("inherited-claude")).unwrap();
        std::fs::create_dir_all(root.join("inherited-codex")).unwrap();
        // SAFETY: runs once, before any thread of this binary reads the environment
        // or starts a process.
        unsafe {
            std::env::set_var("KEYSTONE_FAKE_ENV_RECEIPT", receipt_file(&root));
            std::env::set_var("KEYSTONE_FAKE_CLAUDE_DELAY_MS", "5");
            std::env::set_var("KEYSTONE_FAKE_CODEX_DELAY_MS", "5");
            std::env::set_var("CLAUDE_CONFIG_DIR", root.join("inherited-claude"));
            std::env::set_var("CODEX_HOME", root.join("inherited-codex"));
            for name in INHERITED {
                std::env::set_var(name, "inherited-test-value");
            }
        }
    });
    root
}

/// Every inherited credential and routing variable the fakes report by name.
const INHERITED: [&str; 8] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// One file per test run, so receipts from an earlier run never count.
fn receipt_file(root: &Path) -> PathBuf {
    root.join(format!("receipt-{}.jsonl", std::process::id()))
}

fn receipts(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(receipt_file(root))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn wake() -> (keystone_agent::Wake, Receiver<()>) {
    let (tx, rx) = channel();
    (
        Arc::new(move || {
            let _ = tx.send(());
        }),
        rx,
    )
}

fn until(
    drain: impl Fn() -> Vec<(u64, Event)>,
    wake: &Receiver<()>,
    done: impl Fn(u64, &Event) -> bool,
) -> Vec<(u64, Event)> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut events = Vec::new();
    loop {
        let batch = drain();
        let finished = batch.iter().any(|(session, event)| done(*session, event));
        events.extend(batch);
        if finished {
            return events;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(wake.recv_timeout(left).is_ok(), "timed out; got {events:?}");
    }
}

fn home(root: &Path, name: &str) -> PathBuf {
    let home = root.join(name);
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn thread_of(events: &[(u64, Event)]) -> String {
    events
        .iter()
        .find_map(|(_, event)| match event {
            Event::ThreadReady { thread_id } => Some(thread_id.clone()),
            _ => None,
        })
        .expect("the conversation opened")
}

#[test]
fn claude_accounts_get_their_own_config_dir_on_every_process_and_never_another() {
    let root = environment();
    let claude = PathBuf::from(env!("CARGO_BIN_EXE_keystone-fake-claude"));
    let work = home(&root, "claude-work");
    let personal = home(&root, "claude-personal");
    let (wake_work, rx_work) = wake();
    let (wake_personal, rx_personal) = wake();
    let (wake_default, rx_default) = wake();
    let on_work = ClaudeConnection::with_profile(
        Some(claude.clone()),
        LaunchProfile::account(Provider::Claude, "Work", &work),
        wake_work,
    );
    let on_personal = ClaudeConnection::with_profile(
        Some(claude.clone()),
        LaunchProfile::account(Provider::Claude, "Personal", &personal),
        wake_personal,
    );
    let on_default = ClaudeConnection::new(Some(claude.clone()), wake_default);
    let cwd = root.to_str().unwrap();
    for (connection, session) in [(&on_work, 1), (&on_personal, 2), (&on_default, 3)] {
        connection
            .send(session, cwd, None, None, PROVIDER, "hello".into())
            .unwrap();
    }
    let done = |_: u64, event: &Event| matches!(event, Event::TurnCompleted { .. });
    let work_events = until(|| on_work.drain(), &rx_work, done);
    until(|| on_personal.drain(), &rx_personal, done);
    until(|| on_default.drain(), &rx_default, done);
    let pids = [on_work.pid(1), on_personal.pid(2), on_default.pid(3)].map(Option::unwrap);
    assert!(pids[0] != pids[1] && pids[1] != pids[2]);

    // A model probe, a mode change that needs a new process, and a resume after the
    // connection is gone all stay on the account.
    on_work.request_catalog(1, cwd);
    until(
        || on_work.drain(),
        &rx_work,
        |_, event| matches!(event, Event::Catalog(Catalog::Ready { .. })),
    );
    let full = ApprovalState {
        mode: ApprovalMode::FullAccess,
        overridden: true,
    };
    on_work.set_approval(1, full);
    on_work
        .send(1, cwd, None, None, full, "again".into())
        .unwrap();
    until(|| on_work.drain(), &rx_work, done);
    assert_ne!(on_work.pid(1).unwrap(), pids[0]);
    drop(on_work);
    let (wake_resumed, rx_resumed) = wake();
    let resumed = ClaudeConnection::with_profile(
        Some(claude),
        LaunchProfile::account(Provider::Claude, "Work", &work),
        wake_resumed,
    );
    let restored = ApprovalState {
        mode: ApprovalMode::Provider,
        overridden: true,
    };
    let thread = thread_of(&work_events);
    resumed
        .send(1, cwd, Some(&thread), None, restored, "resumed".into())
        .unwrap();
    until(|| resumed.drain(), &rx_resumed, done);

    let receipts: Vec<Value> = receipts(&root)
        .into_iter()
        .filter(|receipt| receipt["provider"] == "claude")
        .collect();
    let on = |dir: &Path| -> Vec<&Value> {
        receipts
            .iter()
            .filter(|receipt| receipt["CLAUDE_CONFIG_DIR"] == dir.to_str().unwrap())
            .collect()
    };
    let (work_receipts, personal_receipts) = (on(&work), on(&personal));
    let default_receipts = on(&root.join("inherited-claude"));
    assert_eq!(
        work_receipts.len() + personal_receipts.len() + default_receipts.len(),
        receipts.len(),
        "every Claude process ran on one of the three homes: {receipts:#?}"
    );
    let argv = |receipt: &Value| receipt["argv"].to_string();
    let has = |list: &[&Value], flag: &str| list.iter().any(|r| argv(r).contains(flag));
    assert!(
        has(&work_receipts, "--version"),
        "the version check runs on the account"
    );
    assert!(
        has(&work_receipts, "bypassPermissions"),
        "the replacement keeps the account"
    );
    assert!(has(&work_receipts, &thread), "the resume keeps the account");
    assert!(
        work_receipts.len() >= 6,
        "version checks, launches, the replacement and both probes: {work_receipts:#?}"
    );
    for receipt in work_receipts.iter().chain(&personal_receipts) {
        assert_eq!(
            receipt["credentials"],
            json!(["OPENAI_API_KEY", "CODEX_API_KEY"]),
            "{receipt}"
        );
        assert_eq!(receipt["HOME"], std::env::var("HOME").unwrap());
    }
    assert!(personal_receipts.iter().any(|r| r["pid"] == pids[1]));
    assert!(work_receipts.iter().any(|r| r["pid"] == pids[0]));
    for receipt in &default_receipts {
        assert_eq!(receipt["credentials"], json!(INHERITED));
    }
    assert!(default_receipts.iter().any(|r| r["pid"] == pids[2]));
}

#[test]
fn each_codex_account_runs_its_own_app_server_on_its_own_home() {
    let root = environment();
    let codex = PathBuf::from(env!("CARGO_BIN_EXE_keystone-fake-codex"));
    let work = home(&root, "codex-work");
    let (wake_work, rx_work) = wake();
    let (wake_default, rx_default) = wake();
    let on_work = CodexConnection::with_profile(
        Some(codex.clone()),
        "test",
        LaunchProfile::account(Provider::Codex, "Work", &work),
        wake_work,
    );
    let on_default = CodexConnection::new(Some(codex.clone()), "test", wake_default);
    let cwd = root.to_str().unwrap();
    let done = |session: u64| {
        move |s: u64, event: &Event| s == session && matches!(event, Event::TurnCompleted { .. })
    };
    on_work.send(10, cwd, None, None, PROVIDER, "one".into());
    on_work.send(11, cwd, None, None, PROVIDER, "two".into());
    on_default.send(12, cwd, None, None, PROVIDER, "three".into());
    let work_events = until(|| on_work.drain(), &rx_work, done(10));
    if !work_events
        .iter()
        .any(|(s, e)| *s == 11 && matches!(e, Event::TurnCompleted { .. }))
    {
        until(|| on_work.drain(), &rx_work, done(11));
    }
    until(|| on_default.drain(), &rx_default, done(12));
    let (work_pid, default_pid) = (on_work.pid().unwrap(), on_default.pid().unwrap());
    assert_ne!(work_pid, default_pid);
    assert!(
        on_work.is_open(10) && on_work.is_open(11),
        "two sessions share one app-server"
    );

    let missing = root.join("codex-missing");
    let (wake_missing, rx_missing) = wake();
    let on_missing = CodexConnection::with_profile(
        Some(codex),
        "test",
        LaunchProfile::account(Provider::Codex, "Gone", &missing),
        wake_missing,
    );
    on_missing.send(13, cwd, None, None, PROVIDER, "four".into());
    let failed = until(
        || on_missing.drain(),
        &rx_missing,
        |_, event| matches!(event, Event::Failed { .. }),
    );
    let message = failed
        .iter()
        .find_map(|(_, event)| match event {
            Event::Failed { message } => Some(message),
            _ => None,
        })
        .unwrap();
    assert!(message.contains(missing.to_str().unwrap()), "{message}");

    let receipts: Vec<Value> = receipts(&root)
        .into_iter()
        .filter(|receipt| receipt["provider"] == "codex")
        .collect();
    let servers = |dir: &Path| -> Vec<&Value> {
        receipts
            .iter()
            .filter(|r| {
                r["CODEX_HOME"] == dir.to_str().unwrap() && r["argv"] == json!(["app-server"])
            })
            .collect()
    };
    let work_servers = servers(&work);
    assert_eq!(work_servers.len(), 1, "{receipts:#?}");
    assert_eq!(work_servers[0]["pid"], work_pid);
    assert_eq!(
        work_servers[0]["credentials"],
        json!([
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY"
        ])
    );
    let default_servers = servers(&root.join("inherited-codex"));
    assert_eq!(default_servers.len(), 1, "{receipts:#?}");
    assert_eq!(default_servers[0]["pid"], default_pid);
    assert_eq!(default_servers[0]["credentials"], json!(INHERITED));
    assert!(receipts.iter().all(|r| {
        [work.as_path(), &root.join("inherited-codex"), &missing]
            .iter()
            .any(|dir| r["CODEX_HOME"] == dir.to_str().unwrap())
    }));
    assert!(
        receipts.iter().any(
            |r| r["CODEX_HOME"] == work.to_str().unwrap() && r["argv"] == json!(["--version"])
        ),
        "the version check runs on the account"
    );
}
