use super::*;
use std::time::Duration;

fn scripted(provider: Provider) -> PathBuf {
    let name = match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    };
    let binary = keystone_core::test_support::built_binary(&format!("keystone-fake-{name}"));
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
                text: format!("orchestrator test: launch {target}"),
                attachments: Vec::new(),
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
        let delivery = state.store.workspace().delivery(2).unwrap();
        assert_eq!(delivery.parent(), 1);
        assert_eq!(
            *delivery.state(),
            keystone_core::DeliveryState::Observed,
            "the parent's own wait saw the child end, so no turn is started for it"
        );
        assert!(
            !transcript(state, 1).contains("Result from session"),
            "an observed result starts no turn"
        );
        let caller = state.orchestrator.as_ref().unwrap().last_caller.unwrap();
        assert!(state.caller(caller).unwrap_err().contains("running turn"));
    });
    let caller = state.read_with(cx, |state, _| {
        state.orchestrator.as_ref().unwrap().last_caller.unwrap()
    });
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: "long until stopped".into(),
                attachments: Vec::new(),
            },
            cx
        ))
    });
    // The provider's copy of the message follows its turn start, which is when the
    // caller counts as running for the orchestrator.
    wait_until(&state, cx, |state| {
        state.started(1).is_some_and(|c| c.is_running(1))
            && state.conversations[&1].rows.iter().any(|row| {
                matches!(row, Row::Item(item) if item.text == "long until stopped" && !item.id.starts_with("local:"))
            })
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
        // The log worker may hold SQLite's writer for a moment under a loaded test run.
        let handed = (0..50)
            .find_map(|_| {
                let result = state.hand_off(
                    1,
                    Provider::Claude,
                    "context".into(),
                    1,
                    WindowId::FIRST,
                    None,
                    cx,
                );
                if result.is_err() {
                    std::thread::sleep(Duration::from_millis(20));
                }
                result.ok()
            })
            .expect("the handoff saved")
            .value;
        let linked = state
            .orchestrate(caller, "session_read", json!({"session":handed}), cx)
            .unwrap();
        assert_eq!(
            linked["handed_off_from"],
            json!({"session":1,"second_opinion":false})
        );
        let unlinked = state
            .orchestrate(caller, "session_read", json!({"session":2}), cx)
            .unwrap();
        assert_eq!(unlinked["handed_off_from"], Value::Null);
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
                    patch: None,
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

#[gpui::test]
fn a_second_opinion_reads_but_never_launches_steers_or_stops_in_any_mode(
    cx: &mut gpui::TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let state = setup(directory.path(), Provider::Claude, cx);
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: "hello".into(),
                attachments: Vec::new(),
            },
            cx
        ))
    });
    wait_until(&state, cx, |state| {
        state
            .conversations
            .get(&1)
            .is_some_and(|c| !c.running && !c.rows.is_empty())
    });
    let (opinion, other) = state.update(cx, |state, cx| {
        let project = WorkingDirectory::new(directory.path()).unwrap();
        let other = state
            .create_unselected(Provider::Claude, &project, 2, 1, cx)
            .unwrap()
            .value;
        let opinion = state
            .second_opinion(
                1,
                Provider::Codex,
                String::new(),
                2,
                WindowId::FIRST,
                None,
                cx,
            )
            .unwrap()
            .value;
        assert!(state.send_message(
            ComposerDraft {
                session: opinion,
                text: "orchestrator test: launch claude".into(),
                attachments: Vec::new(),
            },
            cx
        ));
        (opinion, other)
    });
    wait_until(&state, cx, |state| {
        state
            .conversations
            .get(&opinion)
            .is_some_and(|c| !c.running)
            && transcript(state, opinion).contains("Scripted orchestrator")
    });
    assert_eq!(
        state.read_with(cx, |state, _| state.store.workspace().sessions().len()),
        3,
        "the second opinion launched nothing"
    );
    let caller = state.read_with(cx, |state, _| {
        state.orchestrator.as_ref().unwrap().last_caller.unwrap()
    });
    assert_eq!(caller.session, opinion);
    state.update(cx, |state, cx| {
        for session in [1, opinion] {
            assert!(state.send_message(
                ComposerDraft {
                    session,
                    text: "long until stopped".into(),
                    attachments: Vec::new(),
                },
                cx
            ));
        }
    });
    wait_until(&state, cx, |state| {
        [1, opinion]
            .into_iter()
            .all(|id| state.started(id).is_some_and(|c| c.is_running(id)))
    });
    let source = state.read_with(cx, |state, _| state.conversations[&1].rows.clone());
    state.update(cx, |state, cx| {
        // The user may widen its mode; it still only reads.
        for mode in [ApprovalMode::ReadOnly, ApprovalMode::Auto] {
            if mode != ApprovalMode::ReadOnly {
                state.set_approval_mode(opinion, mode, cx).unwrap();
            }
            for target in [1, other] {
                for (tool, arguments) in [
                    (
                        "session_send",
                        json!({"session":target,"message":"do it my way",
                            "client_request_id":format!("opinion-send-{target}-{mode:?}")}),
                    ),
                    ("session_interrupt", json!({"session":target})),
                    (
                        "session_launch",
                        json!({"provider":"claude","message":"child",
                            "client_request_id":format!("opinion-launch-{target}-{mode:?}")}),
                    ),
                ] {
                    let refused = state.orchestrate(caller, tool, arguments, cx).unwrap_err();
                    assert!(
                        refused
                            .contains("A second opinion cannot launch, send to or stop sessions"),
                        "{tool} {target} {mode:?}: {refused}"
                    );
                }
            }
        }
        let read = state
            .orchestrate(caller, "session_read", json!({"session":1}), cx)
            .unwrap();
        assert!(!read["rows"].as_array().unwrap().is_empty());
        let own = state
            .orchestrate(caller, "session_read", json!({"session":opinion}), cx)
            .unwrap();
        assert_eq!(
            own["handed_off_from"],
            json!({"session":1,"second_opinion":true})
        );
        assert!(
            state.started(1).is_some_and(|c| c.is_running(1)),
            "the source still runs"
        );
        assert_eq!(
            state.conversations[&1].rows, source,
            "the source is untouched"
        );
        state.stop_turn(1, cx);
        state.stop_turn(opinion, cx);
    });
    wait_until(&state, cx, |state| {
        [1, opinion]
            .into_iter()
            .all(|id| !state.started(id).is_some_and(|c| c.is_running(id)))
    });
}

/// A second opinion's handoffs and forks carry on its review, so they launch nothing
/// either, and a refused worktree launch is refused before git creates anything.
#[gpui::test]
fn a_second_opinions_handoffs_and_forks_launch_nothing_and_make_no_worktree(
    cx: &mut gpui::TestAppContext,
) {
    let repository = fixture_repository();
    let own = keystone_core::worktree::create(repository.path(), "parent", None).unwrap();
    let mut store = Store::open(repository.path().join("workspace.json")).unwrap();
    store
        .create_worktree_showing(
            Provider::Claude,
            &own,
            1,
            keystone_core::WindowId::FIRST,
            None,
        )
        .unwrap();
    cx.executor().allow_parking();
    let state = cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.claude.insert(
            None,
            ClaudeConnection::new(Some(scripted(Provider::Claude)), state.wake.clone()),
        );
        state
    });
    let sessions = state.update(cx, |state, cx| {
        let opinion = state
            .second_opinion(
                1,
                Provider::Claude,
                String::new(),
                2,
                WindowId::FIRST,
                None,
                cx,
            )
            .unwrap()
            .value;
        let continued = state
            .hand_off(
                opinion,
                Provider::Claude,
                String::new(),
                3,
                WindowId::FIRST,
                None,
                cx,
            )
            .unwrap()
            .value;
        let forked = state
            .fork(
                opinion,
                None,
                String::new(),
                Vec::new(),
                4,
                WindowId::FIRST,
                None,
                cx,
            )
            .unwrap()
            .value;
        [opinion, continued, forked]
    });
    for session in sessions {
        state.update(cx, |state, cx| {
            state.ensure_history(session);
            assert!(state.send_message(
                ComposerDraft {
                    session,
                    text: "orchestrator test: launch in new worktree sibling".into(),
                    attachments: Vec::new(),
                },
                cx
            ))
        });
        wait_until(&state, cx, |state| {
            state
                .conversations
                .get(&session)
                .is_some_and(|c| !c.running)
                && transcript(state, session).contains("Scripted orchestrator")
        });
        let reply = state.read_with(cx, |state, _| transcript(state, session));
        assert!(
            reply.contains("Scripted orchestrator failure"),
            "{session}: {reply}"
        );
    }
    assert!(
        !own.repository
            .path()
            .join(".keystone/worktrees/sibling")
            .exists()
    );
    state.read_with(cx, |state, _| {
        assert_eq!(state.store.workspace().sessions().len(), 4);
        assert!(state.store.workspace().deliveries().is_empty());
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

/// A Claude child an agent launched is evicted when proven idle like any session.
/// Dormant, it cannot be steered (`busy`, as an idle target always is), reads and
/// waits answer from Keystone's own copy without starting it, an interrupt is a no-op,
/// and the user's next Send resumes it.
#[gpui::test]
fn an_agent_launched_claude_child_is_evicted_and_answers_reads_while_dormant(
    cx: &mut gpui::TestAppContext,
) {
    use keystone_agent::idle::{IdlePolicy, ManualClock};
    let directory = tempfile::tempdir().unwrap();
    let wrapper = directory.path().join("claude-idle");
    keystone_core::test_support::write_executable(
        &wrapper,
        format!(
            "#!/bin/sh\nKEYSTONE_FAKE_CLAUDE_IDLE=1 KEYSTONE_FAKE_CLAUDE_DELAY_MS=1 KEYSTONE_FAKE_CLAUDE_ARGV_LOG={} exec {} \"$@\"\n",
            directory.path().join("argv").display(),
            scripted(Provider::Claude).display()
        ),
    );
    std::fs::write(directory.path().join("argv"), "").unwrap();
    let state = setup(directory.path(), Provider::Codex, cx);
    let clock = Arc::new(ManualClock::default());
    state.update(cx, |state, _| {
        let claude = ClaudeConnection::new(Some(wrapper), state.wake.clone());
        claude.enable_idle_eviction(IdlePolicy {
            timeout: Duration::from_secs(600),
            clock: clock.clone(),
        });
        state.claude.insert(None, claude);
    });
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: "orchestrator test: launch claude".into(),
                attachments: Vec::new(),
            },
            cx
        ))
    });
    wait_until(&state, cx, |state| {
        state.conversations.get(&1).is_some_and(|c| !c.running)
            && transcript(state, 1).contains("Scripted orchestrator")
            && state.conversations.get(&2).is_some_and(|c| !c.running)
            && state
                .claude
                .get(&None)
                .and_then(|c| c.residency(2))
                .is_some_and(|r| r == "armed")
    });
    assert!(
        state
            .read_with(cx, |state, _| transcript(state, 1))
            .contains("Scripted orchestrator success:")
    );
    clock.advance(Duration::from_secs(601));
    wait_until(&state, cx, |state| {
        state.claude.get(&None).is_some_and(|c| c.pid(2).is_none())
    });
    let rows = state.read_with(cx, |state, _| state.conversations[&2].rows.len());
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: "long until stopped".into(),
                attachments: Vec::new(),
            },
            cx
        ))
    });
    // The provider's copy of the message follows its turn start, which is when the
    // parent's agent counts as running for the orchestrator.
    wait_until(&state, cx, |state| {
        state.started(1).is_some_and(|c| c.is_running(1))
            && state.conversations[&1].rows.iter().any(|row| {
                matches!(row, Row::Item(item) if item.text == "long until stopped" && !item.id.starts_with("local:"))
            })
    });
    let caller = state.read_with(cx, |state, _| {
        state.orchestrator.as_ref().unwrap().last_caller.unwrap()
    });
    state.update(cx, |state, cx| {
        let send = state.orchestrate(
            caller,
            "session_send",
            json!({"session":2, "message":"wake up", "client_request_id":"dormant-send"}),
            cx,
        );
        assert!(send.unwrap_err().contains("busy"));
        let read = state
            .orchestrate(caller, "session_read", json!({"session":2}), cx)
            .unwrap();
        assert_eq!(read["running"], false);
        assert_eq!(read["rows"].as_array().unwrap().len(), rows);
        let wait = state
            .orchestrate(caller, "session_wait", json!({"session":2,"timeout":0}), cx)
            .unwrap();
        assert_eq!(wait["running"], false);
        assert!(
            state
                .orchestrate(caller, "session_interrupt", json!({"session":2}), cx)
                .is_ok()
        );
        state.stop_turn(1, cx);
    });
    std::thread::sleep(Duration::from_millis(200));
    state.read_with(cx, |state, _| {
        assert_eq!(
            state.claude.get(&None).unwrap().pid(2),
            None,
            "nothing started it"
        );
        assert_eq!(state.conversations[&2].rows.len(), rows);
    });
    wait_until(&state, cx, |state| {
        !state.started(1).is_some_and(|c| c.is_running(1))
    });
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 2,
                text: "resume the child".into(),
                attachments: Vec::new(),
            },
            cx
        ))
    });
    wait_until(&state, cx, |state| {
        state.conversations.get(&2).is_some_and(|c| !c.running)
            && transcript(state, 2).contains("resume the child")
    });
    let launches: Vec<Vec<String>> = std::fs::read_to_string(directory.path().join("argv"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        // The child runs Read only, so its chat launches name plan mode; version
        // checks and catalog probes do not.
        .filter(|args: &Vec<String>| args.iter().any(|a| a == "--permission-mode"))
        .collect();
    assert_eq!(launches.len(), 2, "{launches:?}");
    let thread = state.read_with(cx, |state, _| {
        state
            .session(2)
            .unwrap()
            .provider_thread()
            .unwrap()
            .to_owned()
    });
    let resume = launches[1].iter().position(|a| a == "--resume").unwrap();
    assert_eq!(launches[1][resume + 1], thread);
    state.update(cx, |state, cx| state.teardown(cx));
}

/// A scripted agent launches a session on the other provider and ends its turn, or
/// keeps streaming. Either way the launched session's result starts exactly one later
/// turn of the parent, after the turn that launched it.
fn delegation_scenario(provider: Provider, busy: bool, cx: &mut gpui::TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let state = setup(directory.path(), provider, cx);
    let target = match provider {
        Provider::Claude => "codex",
        Provider::Codex => "claude",
    };
    let suffix = if busy { " while busy" } else { "" };
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: format!("orchestrator test: delegate {target}{suffix}"),
                attachments: Vec::new(),
            },
            cx
        ))
    });
    wait_until(&state, cx, |state| {
        state.conversations.get(&1).is_some_and(|c| !c.running)
            && transcript(state, 1).contains("You wrote: Result from session 2")
    });
    state.update(cx, |state, cx| {
        assert!(matches!(
            *state.store.workspace().delivery(2).unwrap().state(),
            keystone_core::DeliveryState::Delivered { .. }
        ));
        let rows = &state.conversations[&1].rows;
        let position = |found: &dyn Fn(&keystone_agent::Item) -> bool| {
            rows.iter()
                .position(|row| matches!(row, Row::Item(item) if found(item)))
                .unwrap()
        };
        let started = position(&|item| item.text.starts_with("Scripted delegation started"));
        let results: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                matches!(row, Row::Item(item) if state.result_source(1, &item.text) == Some(2))
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(results.len(), 1, "{rows:?}");
        assert!(
            started < results[0],
            "the result follows the launching turn"
        );
        let read = crate::app_state::delivery::parse(match &rows[results[0]] {
            Row::Item(item) => &item.text,
            _ => unreachable!(),
        })
        .unwrap();
        assert_eq!(read.outcome, "It finished.");
        assert!(
            read.reply.contains("Scripted delegated task"),
            "{}",
            read.reply
        );
        assert_eq!(state.store.workspace().selected(), Some(1));
        state.teardown(cx);
    });
}

#[gpui::test]
fn an_idle_codex_parent_gets_its_claude_childs_result(cx: &mut gpui::TestAppContext) {
    delegation_scenario(Provider::Codex, false, cx);
}

#[gpui::test]
fn an_idle_claude_parent_gets_its_codex_childs_result(cx: &mut gpui::TestAppContext) {
    delegation_scenario(Provider::Claude, false, cx);
}

#[gpui::test]
fn a_busy_codex_parent_gets_its_claude_childs_result_after_its_turn(cx: &mut gpui::TestAppContext) {
    delegation_scenario(Provider::Codex, true, cx);
}

#[gpui::test]
fn a_busy_claude_parent_gets_its_codex_childs_result_after_its_turn(cx: &mut gpui::TestAppContext) {
    delegation_scenario(Provider::Claude, true, cx);
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

fn git_fixture(directory: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(directory)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

/// A repository with one commit, as the worktree journey starts from.
fn fixture_repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    git_fixture(directory.path(), &["init", "--initial-branch=main"]);
    let config = directory.path().join(".git/config");
    let mut settings = std::fs::read_to_string(&config).unwrap();
    settings.push_str("\n[user]\n\tname = Worktree test\n\temail = worktree@example.invalid\n");
    std::fs::write(config, settings).unwrap();
    std::fs::write(directory.path().join("tracked"), "base\n").unwrap();
    git_fixture(directory.path(), &["add", "tracked"]);
    git_fixture(directory.path(), &["commit", "-m", "base"]);
    directory
}

fn orchestrate_turn(
    state: &Entity<AppState>,
    text: String,
    cx: &mut gpui::TestAppContext,
) -> String {
    let before = state.read_with(cx, |state, _| transcript(state, 1).len());
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text,
                attachments: Vec::new(),
            },
            cx
        ))
    });
    wait_until(state, cx, |state| {
        state.conversations.get(&1).is_some_and(|c| !c.running)
            && transcript(state, 1)[before..].contains("Scripted orchestrator")
    });
    state.read_with(cx, |state, _| transcript(state, 1)[before..].to_owned())
}

#[gpui::test]
fn an_agent_launches_into_its_repositorys_worktree_and_nowhere_else(cx: &mut gpui::TestAppContext) {
    let repository = fixture_repository();
    let worktree = keystone_core::worktree::create(repository.path(), "child", None).unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    let state = setup(repository.path(), Provider::Claude, cx);
    let launched = orchestrate_turn(
        &state,
        format!(
            "orchestrator test: launch in {}",
            worktree.directory.as_str()
        ),
        cx,
    );
    assert!(
        launched.contains("Scripted orchestrator success: launched session 2"),
        "{launched}"
    );
    state.read_with(cx, |state, _| {
        let child = state.session(2).unwrap();
        assert_eq!(child.worktree(), Some(&worktree));
        let SessionContext::Recorded { directory, .. } = child.context() else {
            panic!("the child records its folder");
        };
        assert_eq!(directory, &worktree.directory);
    });
    let refused = orchestrate_turn(
        &state,
        format!(
            "orchestrator test: launch in {}",
            unrelated.path().canonicalize().unwrap().display()
        ),
        cx,
    );
    assert!(
        refused.contains("Scripted orchestrator failure: tool failed"),
        "{refused}"
    );
    let refused = orchestrate_turn(
        &state,
        "orchestrator test: launch in new worktree extra".into(),
        cx,
    );
    assert!(
        refused.contains("Scripted orchestrator failure: tool failed"),
        "{refused}"
    );
    assert!(!repository.path().join(".keystone/worktrees/extra").exists());
    state.read_with(cx, |state, _| {
        assert_eq!(state.store.workspace().sessions().len(), 2);
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn an_agent_in_a_worktree_may_create_another(cx: &mut gpui::TestAppContext) {
    let repository = fixture_repository();
    let own = keystone_core::worktree::create(repository.path(), "parent", None).unwrap();
    let mut store = Store::open(repository.path().join("workspace.json")).unwrap();
    store
        .create_worktree_showing(
            Provider::Claude,
            &own,
            1,
            keystone_core::WindowId::FIRST,
            None,
        )
        .unwrap();
    store.set_approval_mode(1, ApprovalMode::Ask).unwrap();
    cx.executor().allow_parking();
    let state = cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.claude.insert(
            None,
            ClaudeConnection::new(Some(scripted(Provider::Claude)), state.wake.clone()),
        );
        state
    });
    let launched = orchestrate_turn(
        &state,
        "orchestrator test: launch in new worktree sibling".into(),
        cx,
    );
    assert!(
        launched.contains("Scripted orchestrator success: launched session 2"),
        "{launched}"
    );
    state.read_with(cx, |state, _| {
        let child = state.session(2).unwrap().worktree().unwrap();
        assert_eq!(child.branch, "keystone/sibling");
        assert_eq!(child.repository, own.repository);
        assert_eq!(
            child.directory.path(),
            own.repository.path().join(".keystone/worktrees/sibling")
        );
        // A worktree launch brings its result back like any other.
        assert_eq!(state.store.workspace().delivery(2).unwrap().parent(), 1);
    });
    let caller = state.read_with(cx, |state, _| {
        state.orchestrator.as_ref().unwrap().last_caller.unwrap()
    });
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: "long".into(),
                attachments: Vec::new(),
            },
            cx
        ))
    });
    wait_until(&state, cx, |state| {
        state.started(1).is_some_and(|c| c.is_running(1))
    });
    state.update(cx, |state, cx| {
        let orchestrator = state.orchestrator.as_mut().unwrap();
        orchestrator.preparing.insert((1, "held".into()));
        let busy = state.orchestrate(
            caller,
            "session_launch",
            json!({"provider":"claude","message":"never sent","client_request_id":"held"}),
            cx,
        );
        assert!(busy.unwrap_err().contains("still being prepared"));
        assert_eq!(state.store.workspace().sessions().len(), 2);
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

#[test]
fn only_one_repositorys_root_and_worktrees_reach_each_other() {
    let root = tempfile::tempdir().unwrap();
    let folder = |path: &str| {
        let path = root.path().join(path);
        std::fs::create_dir_all(&path).unwrap();
        WorkingDirectory::new(&path).unwrap()
    };
    let repo = folder("repo");
    let nested = folder("repo/vendor/foreign");
    let subfolder = folder("repo/crates/x");
    let in_worktree = |name: &str| WorktreeContext {
        directory: folder(&format!("repo/.keystone/worktrees/{name}")),
        repository: repo.clone(),
        branch: format!("keystone/{name}"),
    };
    let (a, b) = (in_worktree("a"), in_worktree("b"));
    assert!(
        reachable(&repo, None, &a.directory, Some(&a)),
        "root reaches a worktree"
    );
    assert!(
        reachable(&a.directory, Some(&a), &b.directory, Some(&b)),
        "worktree reaches a sibling"
    );
    assert!(
        reachable(&a.directory, Some(&a), &repo, None),
        "worktree reaches the root"
    );
    assert!(
        reachable(&a.directory, Some(&a), &a.directory, Some(&a)),
        "its own folder"
    );
    assert!(
        !reachable(&nested, None, &a.directory, Some(&a)),
        "a nested foreign repository does not reach the enclosing repository's worktrees"
    );
    assert!(!reachable(&subfolder, None, &a.directory, Some(&a)));
    assert!(!reachable(&a.directory, Some(&a), &nested, None));
}

#[gpui::test]
fn a_read_only_agent_reads_the_browser_status_but_may_not_act(cx: &mut gpui::TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let state = setup(directory.path(), Provider::Claude, cx);
    state.update(cx, |state, cx| {
        state
            .set_approval_mode(1, ApprovalMode::ReadOnly, cx)
            .unwrap();
    });
    let reply = orchestrate_turn(&state, "orchestrator test: browser".into(), cx);
    assert!(
        reply.contains("browser status open=false servers=0, act refused"),
        "{reply}"
    );
    assert!(
        state
            .read_with(cx, |state, _| state.browsers.existing(
                WorkingDirectory::new(directory.path()).unwrap().as_str()
            ))
            .is_none(),
        "neither call started a browser page"
    );
}
