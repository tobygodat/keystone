use super::*;
use std::time::Duration;

fn scripted(provider: Provider) -> PathBuf {
    let name = match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    };
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("keystone-fake-{name}"));
    assert!(binary.is_file(), "build scripted provider binaries first");
    binary
}

fn wait_until(
    state: &Entity<AppState>,
    cx: &mut gpui::TestAppContext,
    predicate: impl Fn(&AppState) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        cx.run_until_parked();
        if state.read_with(cx, |state, _| predicate(state)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "scripted orchestration timed out: {:?}",
            state.read_with(cx, |state, _| state
                .conversations
                .iter()
                .map(|(id, c)| (*id, c.rows.clone(), c.error.clone()))
                .collect::<Vec<_>>())
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn setup(directory: &Path, provider: Provider, cx: &mut gpui::TestAppContext) -> Entity<AppState> {
    let mut store = Store::open(directory.join("workspace.json")).unwrap();
    let project = WorkingDirectory::new(directory).unwrap();
    store.create(provider, &project, 1).unwrap();
    store.set_approval_mode(1, ApprovalMode::Ask).unwrap();
    cx.executor().allow_parking();
    cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.codex.insert(
            None,
            CodexConnection::new(Some(scripted(Provider::Codex)), "test", state.wake.clone()),
        );
        state.claude.insert(
            None,
            ClaudeConnection::new(Some(scripted(Provider::Claude)), state.wake.clone()),
        );
        state
    })
}

fn transcript(state: &AppState, session: u64) -> String {
    state
        .conversations
        .get(&session)
        .into_iter()
        .flat_map(|c| &c.rows)
        .filter_map(|row| match row {
            Row::Item(item) => Some(item.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn cross_provider_scenario(provider: Provider, cx: &mut gpui::TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let state = setup(directory.path(), provider, cx);
    let target = match provider {
        Provider::Claude => "codex",
        Provider::Codex => "claude",
    };
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: format!("orchestrator test: launch {target}")
            },
            cx
        ))
    });
    wait_until(&state, cx, |state| {
        state.conversations.get(&1).is_some_and(|c| !c.running)
            && transcript(state, 1).contains("Scripted orchestrator")
    });
    state.update(cx, |state, _| {
        let text = transcript(state, 1);
        assert!(text.contains("Scripted orchestrator success:"), "{text}");
        assert!(text.contains("Scripted child reply"), "{text}");
        assert!(text.contains("active_send=ok"), "{text}");
        assert!(text.contains("active_interrupt=ok"), "{text}");
        assert_eq!(
            state.store.workspace().sessions().len(),
            2,
            "duplicate launch must not create a second child"
        );
        assert_eq!(state.store.workspace().selected(), Some(1));
        let child = state.session(2).unwrap();
        assert_eq!(child.approval_mode(), ApprovalMode::ReadOnly);
        assert_eq!(
            child.provider(),
            match provider {
                Provider::Claude => Provider::Codex,
                Provider::Codex => Provider::Claude,
            }
        );
        let orchestrator = state.orchestrator.as_ref().unwrap();
        assert_eq!(orchestrator.records.created_by(2), Some(1));
        let caller = orchestrator.last_caller.unwrap();
        assert!(state.caller(caller).unwrap_err().contains("running turn"));
    });
    let caller = state.read_with(cx, |state, _| {
        state.orchestrator.as_ref().unwrap().last_caller.unwrap()
    });
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: "long".into()
            },
            cx
        ))
    });
    wait_until(&state, cx, |state| {
        state.started(1).is_some_and(|c| c.is_running(1))
    });
    state.update(cx, |state, cx| {
        let refusal = state.orchestrate(
            caller,
            "session_launch",
            json!({"provider":target,
            "mode":"full-access","message":"never sent","client_request_id":"ceiling-refusal"}),
            cx,
        );
        assert!(refusal.unwrap_err().contains("ceiling"));
        let other = directory.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let other = WorkingDirectory::new(&other).unwrap();
        let other_id = state.create(Provider::Codex, &other, 1, cx).unwrap().value;
        let refused = state.orchestrate(caller, "session_read", json!({"session":other_id}), cx);
        assert!(refused.unwrap_err().contains("project"));
        let wrong_generation = Caller {
            generation: caller.generation + 100,
            ..caller
        };
        assert!(
            state
                .orchestrate(wrong_generation, "capabilities", json!({}), cx)
                .is_err()
        );
        let busy = state.orchestrate(
            caller,
            "session_send",
            json!({"session":2,
            "message":"must not start a turn","client_request_id":"idle-send"}),
            cx,
        );
        assert!(busy.unwrap_err().contains("busy"));
        let wait = state
            .orchestrate(caller, "session_wait", json!({"session":2,"timeout":0}), cx)
            .unwrap();
        assert_eq!(wait["running"], false);
        state.conversation(2).apply(Event::TurnStarted);
        let pending = state
            .orchestrate(caller, "session_wait", json!({"session":2,"timeout":0}), cx)
            .unwrap();
        assert_eq!(pending["running"], true);
        let stable = state.conversation(2).rows.len() - 1;
        let item = keystone_agent::Item {
            id: "synthetic-stream".into(),
            kind: keystone_agent::ItemKind::AgentMessage,
            text: "partial".into(),
            status: keystone_agent::ItemStatus::InProgress,
        };
        state
            .conversation(2)
            .apply(Event::ItemStarted(item.clone()));
        let page = state
            .orchestrate(caller, "session_read", json!({"session":2}), cx)
            .unwrap();
        assert_eq!(page["next"], stable);
        state
            .conversation(2)
            .apply(Event::ItemCompleted(keystone_agent::Item {
                text: "complete".into(),
                status: keystone_agent::ItemStatus::Completed,
                ..item
            }));
        let page = state
            .orchestrate(
                caller,
                "session_read",
                json!({"session":2,"after":stable}),
                cx,
            )
            .unwrap();
        assert_eq!(page["rows"][0]["item"]["text"], "complete");
        assert_eq!(page["next"], stable + 1);
        let long = "x".repeat(keystone_agent::OUTPUT_CAP * 3);
        state
            .conversation(2)
            .apply(Event::ItemCompleted(keystone_agent::Item {
                id: "synthetic-large".into(),
                kind: keystone_agent::ItemKind::AgentMessage,
                text: long.clone(),
                status: keystone_agent::ItemStatus::Completed,
            }));
        let page = state
            .orchestrate(
                caller,
                "session_read",
                json!({"session":2,"after":stable+1}),
                cx,
            )
            .unwrap();
        assert_eq!(page["rows"][0]["truncated"], true);
        assert!(
            page["rows"][0]["item"]["text"]
                .as_str()
                .unwrap()
                .contains("bytes omitted")
        );
        assert!(transcript(state, 2).contains(&long));
        let oversized = state.conversation(2).rows.len();
        let huge = "y".repeat(5 * 1024 * 1024);
        state
            .conversation(2)
            .apply(Event::ItemCompleted(keystone_agent::Item {
                id: "synthetic-command".into(),
                kind: keystone_agent::ItemKind::Command {
                    command: huge.clone(),
                    exit_code: Some(0),
                },
                text: "done".into(),
                status: keystone_agent::ItemStatus::Completed,
            }));
        let request = keystone_agent::RequestId("synthetic-approval".into());
        state
            .conversation(2)
            .apply(Event::ApprovalRequested(keystone_agent::Approval {
                request: request.clone(),
                item_id: None,
                action: huge.clone(),
                detail: None,
            }));
        assert!(state.conversation(2).decide(&request, true).is_some());
        state
            .conversation(2)
            .apply(Event::ItemCompleted(keystone_agent::Item {
                id: "synthetic-paths".into(),
                kind: keystone_agent::ItemKind::FileChange {
                    paths: vec!["p".repeat(1024); 1024],
                },
                text: String::new(),
                status: keystone_agent::ItemStatus::Completed,
            }));
        state.conversation(2).apply(Event::TurnCompleted {
            status: keystone_agent::TurnStatus::Failed,
            error: Some(huge.clone()),
            stopped: false,
        });
        let mut pages = Vec::new();
        for position in oversized..oversized + 4 {
            let page = state
                .orchestrate(
                    caller,
                    "session_read",
                    json!({"session":2,"after":position-1,"limit":1}),
                    cx,
                )
                .unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() < 320 * 1024);
            assert_eq!(page["next"], position);
            assert_eq!(page["rows"][0]["position"], position);
            assert_eq!(page["rows"][0]["truncated"], true);
            pages.push(page["rows"][0].clone());
        }
        let omitted = |text: &Value| text.as_str().unwrap().contains("bytes omitted");
        assert!(omitted(&pages[0]["item"]["kind"]["command"]));
        assert_eq!(pages[0]["item"]["text"], "done");
        assert!(omitted(&pages[1]["decision"]));
        assert!(pages[2]["omitted_bytes"].as_u64().unwrap() > 1024 * 1024);
        assert!(omitted(&pages[3]["error"]));
        let page = state
            .orchestrate(
                caller,
                "session_read",
                json!({"session":2,"after":oversized-1}),
                cx,
            )
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() < 320 * 1024);
        state.conversation(2).apply(Event::Failed { message: huge });
        let last = state.conversation(2).rows.len() - 1;
        for (tool, arguments) in [
            ("session_read", json!({"session":2,"after":last,"limit":1})),
            ("session_wait", json!({"session":2,"timeout":0})),
        ] {
            let response = state.orchestrate(caller, tool, arguments, cx).unwrap();
            assert!(serde_json::to_vec(&response).unwrap().len() < 320 * 1024);
            assert!(omitted(&response["error"]));
        }
        state.conversation(2).apply(Event::TurnCompleted {
            status: keystone_agent::TurnStatus::Interrupted,
            error: None,
            stopped: true,
        });
        state.stop_turn(1, cx);
    });
    wait_until(&state, cx, |state| {
        !state.started(1).is_some_and(|c| c.is_running(1))
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn claude_agent_launches_and_reads_codex(cx: &mut gpui::TestAppContext) {
    cross_provider_scenario(Provider::Claude, cx);
}

#[gpui::test]
fn codex_agent_launches_and_reads_claude(cx: &mut gpui::TestAppContext) {
    cross_provider_scenario(Provider::Codex, cx);
}

#[test]
fn a_launch_account_is_default_or_one_of_that_providers_added_accounts() {
    let mut store = Store::in_memory().unwrap();
    let work = store
        .add_account(
            Provider::Claude,
            "Work",
            "/accounts/claude-work".into(),
            &[],
        )
        .unwrap()
        .value;
    store
        .add_account(Provider::Codex, "Work", "/accounts/codex-work".into(), &[])
        .unwrap();
    for named in [None, Some("default")] {
        assert_eq!(launch_account(&store, Provider::Claude, named), Ok(None));
    }
    assert_eq!(
        launch_account(&store, Provider::Claude, Some("claude-work")),
        Ok(Some(work))
    );
    for (provider, named) in [
        (Provider::Codex, "claude-work"),
        (Provider::Claude, "codex-work"),
        (Provider::Claude, "claude-missing"),
        (Provider::Claude, "Default"),
    ] {
        let refused = launch_account(&store, provider, Some(named)).unwrap_err();
        assert!(refused.contains("Unknown"), "{named}: {refused}");
    }
}
