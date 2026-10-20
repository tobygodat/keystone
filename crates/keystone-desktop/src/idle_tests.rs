//! Claude idle eviction through the app runtime, with no view: a proven-idle process
//! ends only after its log and conversation ID are saved, leaves no failed turn or
//! recovery behind, and the next Send resumes it on its own account; a conversation
//! that is not being saved keeps its process.

use super::*;
use crate::conversation::Recovery;
use keystone_agent::idle::{IdlePolicy, ManualClock};
use serde_json::{Value, json};

const TEN_MINUTES: Duration = Duration::from_secs(600);

/// The scripted Claude in idle mode, logging its launches, environment and messages.
fn idle_claude(dir: &Path) -> PathBuf {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("keystone-fake-claude");
    let environment = json!({
        "KEYSTONE_FAKE_CLAUDE_IDLE": "1",
        "KEYSTONE_FAKE_CLAUDE_DELAY_MS": "1",
        "KEYSTONE_FAKE_CLAUDE_CHILD": "1",
        "KEYSTONE_FAKE_CLAUDE_LOG_DIR": dir.to_str().unwrap(),
        "KEYSTONE_FAKE_CLAUDE_ARGV_LOG": dir.join("argv").to_str().unwrap(),
        "KEYSTONE_FAKE_ENV_RECEIPT": dir.join("env").to_str().unwrap(),
    });
    let script = dir.join("claude-idle-wrapper");
    std::fs::write(
        &script,
        format!(
            "#!/usr/bin/python3\nimport os, sys\nos.environ.update({environment})\nbinary = {}\nos.execv(binary, [binary, *sys.argv[1:]])\n",
            serde_json::to_string(&binary).unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("argv"), "").unwrap();
    std::fs::write(dir.join(".fake-claude-messages"), "").unwrap();
    script
}

/// One Claude session on an added account, asking for `fake-haiku` in Ask mode, with
/// idle eviction on `clock`.
fn idle_app(
    dir: &Path,
    clock: &Arc<ManualClock>,
    cx: &mut gpui::TestAppContext,
) -> (Entity<AppState>, PathBuf) {
    let mut store = Store::open(dir.join("workspace.json")).unwrap();
    let directory = WorkingDirectory::new(dir).unwrap();
    let home = dir.join("claude-work");
    std::fs::create_dir_all(&home).unwrap();
    let work = store
        .add_account(Provider::Claude, "Work", home.clone(), &[])
        .unwrap()
        .value;
    let id = store.create(Provider::Claude, &directory, 1).unwrap().value;
    store.set_account(id, Some(work)).unwrap();
    store.set_model(id, Some("fake-haiku".into())).unwrap();
    store.set_approval_mode(id, ApprovalMode::Ask).unwrap();
    cx.executor().allow_parking();
    let state = cx.new(|cx| AppState::new(store, cx));
    let claude = idle_claude(dir);
    state.update(cx, |state, _| {
        state.test_programs = Some((PathBuf::from("/nonexistent-codex"), claude));
        state.test_idle = Some(IdlePolicy {
            timeout: TEN_MINUTES,
            clock: clock.clone(),
        });
        state.ensure_history(1);
    });
    wait_until(cx, &state, |state| {
        state.conversations.get(&1).is_some_and(|c| !c.is_loading())
    });
    (state, home)
}

fn send(state: &Entity<AppState>, text: &str, cx: &mut gpui::TestAppContext) {
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: text.into(),
            },
            cx
        ))
    });
}

fn residency(state: &Entity<AppState>, want: &str, cx: &mut gpui::TestAppContext) {
    wait_until(cx, state, |state| {
        !state.conversations[&1].running
            && state
                .test_claude()
                .and_then(|c| c.residency(1))
                .is_some_and(|now| now.contains(want))
    });
}

fn pid(state: &Entity<AppState>, cx: &mut gpui::TestAppContext) -> Option<u32> {
    state.read_with(cx, |state, _| state.test_claude().and_then(|c| c.pid(1)))
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        s.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

/// Chat launches, without the version checks.
fn launches(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("env"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| {
            !line["argv"]
                .as_array()
                .unwrap()
                .contains(&json!("--version"))
        })
        .collect()
}

fn flag<'a>(launch: &'a Value, name: &str) -> Option<&'a str> {
    let args = launch["argv"].as_array().unwrap();
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1)?.as_str()
}

#[gpui::test]
fn an_idle_claude_process_ends_after_saving_and_resumes_on_its_own_account(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let clock = Arc::new(ManualClock::default());
    let (state, home) = idle_app(dir, &clock, cx);
    send(&state, "hello", cx);
    residency(&state, "armed", cx);
    let first = pid(&state, cx).unwrap();
    let thread = state.read_with(cx, |state, _| {
        let conversation = &state.conversations[&1];
        assert_eq!(conversation.reported_model.as_deref(), Some("fake-haiku"));
        state
            .session(1)
            .unwrap()
            .provider_thread()
            .unwrap()
            .to_owned()
    });
    let before = state.read_with(cx, |state, _| state.conversations[&1].rows.clone());

    clock.advance(TEN_MINUTES - Duration::from_secs(1));
    std::thread::sleep(Duration::from_millis(200));
    cx.run_until_parked();
    assert!(alive(first), "no eviction before ten minutes");
    clock.advance(Duration::from_secs(2));
    wait_until(cx, &state, |state| {
        state.test_claude().is_some_and(|c| c.pid(1).is_none())
            && state.conversations[&1].reported_model.is_none()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(first) {
        assert!(Instant::now() < deadline, "{first} survived");
        std::thread::sleep(Duration::from_millis(10));
    }
    for child in std::fs::read_to_string(dir.join(".fake-claude-children"))
        .unwrap()
        .lines()
    {
        assert!(!alive(child.parse().unwrap()), "child {child} survived");
    }
    state.read_with(cx, |state, _| {
        let conversation = &state.conversations[&1];
        // Eviction is not a crash: no turn end, failure, recovery or unread change.
        assert_eq!(conversation.rows, before);
        assert_eq!(conversation.recovery, None);
        assert_eq!(conversation.error, None);
        assert!(!conversation.running);
        assert_eq!(conversation.reported_model, None);
        let saved = state.session(1).unwrap();
        assert_eq!(saved.model(), Some("fake-haiku"));
        assert_eq!(saved.provider_thread(), Some(thread.as_str()));
        assert_eq!(restored(&dir.join("sessions"), 1), before);
    });

    send(&state, "again", cx);
    wait_until(cx, &state, |state| {
        let conversation = &state.conversations[&1];
        !conversation.running && conversation.reported_model.is_some()
    });
    let second = pid(&state, cx).unwrap();
    assert_ne!(first, second);
    let launches = launches(dir);
    assert_eq!(launches.len(), 2, "{launches:?}");
    for launch in &launches {
        assert_eq!(launch["CLAUDE_CONFIG_DIR"], home.to_str().unwrap());
        assert_eq!(flag(launch, "--model"), Some("fake-haiku"));
        assert_eq!(flag(launch, "--permission-mode"), Some("default"));
    }
    assert_eq!(flag(&launches[1], "--resume"), Some(thread.as_str()));
    let messages = std::fs::read_to_string(dir.join(".fake-claude-messages")).unwrap();
    assert_eq!(
        messages.lines().collect::<Vec<_>>(),
        [r#""hello""#, r#""again""#]
    );
    state.read_with(cx, |state, _| {
        let conversation = &state.conversations[&1];
        assert_eq!(conversation.reported_model.as_deref(), Some("fake-haiku"));
        assert!(
            !conversation
                .rows
                .iter()
                .any(|row| matches!(row, Row::TurnEnded { .. } | Row::Recovery(_)))
        );
    });
}

/// Recovery's gate still holds Send for a session whose process was evicted, and
/// starts nothing.
#[gpui::test]
fn a_dormant_session_still_waits_for_recovery_before_sending(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let clock = Arc::new(ManualClock::default());
    let (state, _) = idle_app(dir, &clock, cx);
    send(&state, "hello", cx);
    residency(&state, "armed", cx);
    clock.advance(TEN_MINUTES);
    wait_until(cx, &state, |state| {
        state.test_claude().is_some_and(|c| c.pid(1).is_none())
    });
    state.update(cx, |state, cx| {
        state.test_conversation(1).recovery = Some(Recovery::Unavailable("scripted".into()));
        assert!(!state.send_message(
            ComposerDraft {
                session: 1,
                text: "held".into(),
            },
            cx
        ));
    });
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(pid(&state, cx), None);
    assert_eq!(launches(dir).len(), 1);
}

/// A conversation whose log cannot save keeps its process past the deadline. Once
/// saving works again, its next activity proves it idle anew.
#[gpui::test]
fn a_conversation_that_is_not_being_saved_keeps_its_process(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let clock = Arc::new(ManualClock::default());
    let (state, _) = idle_app(dir, &clock, cx);
    send(&state, "hold tool", cx);
    wait_until(cx, &state, |state| {
        state.conversations[&1]
            .rows
            .iter()
            .any(|row| matches!(row, Row::Item(item) if item.kind != ItemKind::UserMessage))
    });
    let path = state.read_with(cx, |state, _| state.store.path().to_owned());
    let writer = crate::tests::lock_database(&path);
    std::fs::write(dir.join(".fake-claude-release-tool"), "").unwrap();
    wait_until(cx, &state, |state| {
        let conversation = &state.conversations[&1];
        !conversation.running && conversation.save_error.is_some()
    });
    let resident = pid(&state, cx).unwrap();
    for _ in 0..3 {
        clock.advance(TEN_MINUTES);
        std::thread::sleep(Duration::from_millis(100));
        cx.run_until_parked();
    }
    assert_eq!(pid(&state, cx), Some(resident));
    assert!(alive(resident));
    residency(&state, "Keystone kept it", cx);

    drop(writer);
    send(&state, "next", cx);
    residency(&state, "armed", cx);
    wait_until(cx, &state, |state| {
        state.conversations[&1].save_error.is_none()
    });
    clock.advance(TEN_MINUTES);
    wait_until(cx, &state, |state| {
        state.test_claude().is_some_and(|c| c.pid(1).is_none())
    });
}
