//! Launched sessions' results through the app runtime, with scripted providers: a
//! result starts an idle parent's turn once, waits for a busy, settled or answering
//! parent, and survives a restart without being sent again.

use super::*;
use crate::app_state::delivery::{Hold, Outcome, message, parse};
use keystone_core::DeliveryState;

/// Sessions 1 (Codex) and 2 (Claude) with scripted providers, and a Codex session 3
/// that an agent in `parent` launched. Both parent and child histories are loaded.
fn launched(dir: &Path, parent: u64, cx: &mut gpui::TestAppContext) -> Entity<AppState> {
    let state = runtime(dir, cx);
    connect(&state, dir, 1, false, cx);
    let directory = WorkingDirectory::new(dir).unwrap();
    state.update(cx, |state, _| {
        let child = state
            .store
            .create_unselected(Provider::Codex, &directory, 3, parent)
            .unwrap()
            .value;
        assert_eq!(child, 3);
        state.ensure_history(parent);
        state.ensure_history(3);
    });
    wait_until(cx, &state, |state| {
        [parent, 3]
            .iter()
            .all(|id| state.conversations.get(id).is_some_and(|c| !c.is_loading()))
    });
    state
}

/// The launched session 3 replies and its turn ends, delivered as provider events are.
fn child_finishes(state: &Entity<AppState>, end: Event, cx: &mut gpui::TestAppContext) {
    state.update(cx, |state, cx| {
        let reply = Event::ItemCompleted(Item {
            id: "child-reply".into(),
            kind: ItemKind::AgentMessage,
            text: "Child found the bug in parser.rs.".into(),
            status: ItemStatus::Completed,
        });
        state.receive(
            Vec::new(),
            vec![(3, Event::TurnStarted), (3, reply), (3, end)],
            cx,
        );
        state.pump(cx);
    });
}

fn finished() -> Event {
    Event::TurnCompleted {
        status: TurnStatus::Completed,
        error: None,
        stopped: false,
    }
}

fn state_of(state: &AppState) -> DeliveryState {
    state.store.workspace().delivery(3).unwrap().state().clone()
}

/// The parent's rows that are results from session 3.
fn results(state: &AppState, parent: u64) -> Vec<String> {
    state.conversations[&parent]
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Item(item) if item.kind == ItemKind::UserMessage => Some(&item.text),
            _ => None,
        })
        .filter(|text| state.result_source(parent, text) == Some(3))
        .cloned()
        .collect()
}

/// The parent's turn with the result ran: its scripted provider echoed the result.
fn answered(state: &AppState, parent: u64) -> bool {
    let conversation = &state.conversations[&parent];
    !conversation.running
        && conversation.rows.iter().any(|row| {
            matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage
                && item.text.contains("You wrote: Result from session 3"))
        })
}

#[gpui::test]
fn an_idle_parent_starts_one_turn_with_its_launched_sessions_result(cx: &mut gpui::TestAppContext) {
    for parent in [1, 2] {
        let dir = tempfile::tempdir().unwrap();
        let state = launched(dir.path(), parent, cx);
        child_finishes(&state, finished(), cx);
        state.read_with(cx, |state, _| {
            assert!(matches!(state_of(state), DeliveryState::Delivered { .. }));
            assert!(state.conversations[&parent].running, "a new turn started");
            let shown = results(state, parent);
            assert_eq!(shown.len(), 1);
            let read = parse(&shown[0]).unwrap();
            assert_eq!(read.child, 3);
            assert_eq!(read.outcome, "It finished.");
            assert_eq!(read.reply, "Child found the bug in parser.rs.");
            assert!(shown[0].contains("New Codex session"), "{}", shown[0]);
        });
        wait_until(cx, &state, |state| answered(state, parent));
        // Later pumps, and the parent's own turn ending, send nothing again.
        for _ in 0..3 {
            state.update(cx, |state, cx| {
                state.deliveries_due();
                state.pump(cx);
            });
        }
        cx.run_until_parked();
        state.read_with(cx, |state, _| {
            assert!(!state.conversations[&parent].running);
            assert_eq!(results(state, parent).len(), 1);
            assert!(state.waiting_results(parent).is_empty());
        });
        let saved = Store::open(dir.path().join("workspace.json")).unwrap();
        assert!(matches!(
            *saved.workspace().delivery(3).unwrap().state(),
            DeliveryState::Delivered { .. }
        ));
        state.update(cx, |state, cx| state.teardown(cx));
    }
}

#[gpui::test]
fn a_result_delivered_while_waiting_to_quit_runs_its_turn_before_keystone_quits(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    wait_to_quit(&state, cx);
    child_finishes(&state, finished(), cx);
    wait_until(cx, &state, |state| state.phase == Phase::Exiting);
    state.read_with(cx, |state, _| {
        assert!(matches!(state_of(state), DeliveryState::Delivered { .. }));
        assert!(
            answered(state, 1),
            "the parent's turn with the result ran to its end before Keystone quit"
        );
    });
}

#[gpui::test]
fn a_busy_parent_gets_the_result_once_its_turn_ends(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    state.update(cx, |state, cx| {
        state.receive(Vec::new(), vec![(1, Event::TurnStarted)], cx)
    });
    child_finishes(
        &state,
        Event::TurnCompleted {
            status: TurnStatus::Interrupted,
            error: None,
            stopped: true,
        },
        cx,
    );
    let rows = state.read_with(cx, |state, _| {
        assert!(matches!(state_of(state), DeliveryState::Ready { .. }));
        assert_eq!(
            state.waiting_results(1),
            [(3, "New Codex session".to_owned(), Hold::Turn)]
        );
        assert!(
            results(state, 1).is_empty(),
            "no steer into the running turn"
        );
        state.conversations[&1].rows.len()
    });
    assert_eq!(
        Hold::Turn.notice("New Codex session"),
        "Result from \u{201c}New Codex session\u{201d} arrives when this turn ends."
    );
    state.update(cx, |state, cx| {
        state.receive(Vec::new(), vec![(1, finished())], cx);
        state.pump(cx);
    });
    state.read_with(cx, |state, _| {
        assert!(matches!(state_of(state), DeliveryState::Delivered { .. }));
        let shown = results(state, 1);
        assert_eq!(shown.len(), 1);
        assert_eq!(parse(&shown[0]).unwrap().outcome, "It was stopped.");
        assert!(state.conversations[&1].rows.len() > rows);
    });
    wait_until(cx, &state, |state| answered(state, 1));
    state.read_with(cx, |state, _| assert_eq!(results(state, 1).len(), 1));
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn a_settled_parent_is_never_woken_and_gets_the_result_when_unsettled(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    state.update(cx, |state, cx| state.set_settled(1, true, cx).unwrap());
    child_finishes(
        &state,
        Event::Failed {
            message: "codex could not start".into(),
        },
        cx,
    );
    state.read_with(cx, |state, _| {
        assert!(matches!(state_of(state), DeliveryState::Ready { .. }));
        assert_eq!(
            state.waiting_results(1),
            [(3, "New Codex session".to_owned(), Hold::Settled)]
        );
        assert!(!state.conversations[&1].running);
        assert!(results(state, 1).is_empty());
        assert!(
            state.test_codex().unwrap().pid().is_none(),
            "nothing started"
        );
    });
    state.update(cx, |state, cx| state.set_settled(1, false, cx).unwrap());
    wait_until(cx, &state, |state| answered(state, 1));
    state.read_with(cx, |state, _| {
        let shown = results(state, 1);
        assert_eq!(shown.len(), 1);
        assert_eq!(
            parse(&shown[0]).unwrap().outcome,
            "It failed: codex could not start"
        );
        assert!(matches!(state_of(state), DeliveryState::Delivered { .. }));
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn a_parent_with_a_waiting_widget_is_not_woken_until_it_is_answered(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    let surface = keystone_agent::surface::parse(
        "widget-call",
        &keystone_agent::surface::examples::checklist(),
    )
    .unwrap();
    state.update(cx, |state, cx| {
        state.receive(
            Vec::new(),
            vec![(
                1,
                Event::SurfaceShown {
                    surface,
                    waiting: true,
                },
            )],
            cx,
        )
    });
    child_finishes(&state, finished(), cx);
    state.read_with(cx, |state, _| {
        assert!(!state.conversations[&1].running);
        assert_eq!(
            state.waiting_results(1),
            [(3, "New Codex session".to_owned(), Hold::Answer)]
        );
        assert!(results(state, 1).is_empty());
    });
    // The provider stops waiting for the widget; nothing holds the result then.
    state.update(cx, |state, cx| {
        state.receive(Vec::new(), vec![(1, finished())], cx);
        state.pump(cx);
    });
    wait_until(cx, &state, |state| answered(state, 1));
    state.read_with(cx, |state, _| assert_eq!(results(state, 1).len(), 1));
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn a_ready_result_survives_a_restart_and_a_delivered_one_is_never_sent_again(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    state.update(cx, |state, cx| state.set_settled(1, true, cx).unwrap());
    child_finishes(&state, finished(), cx);
    state.read_with(cx, |state, _| {
        assert!(matches!(state_of(state), DeliveryState::Ready { .. }))
    });
    state.update(cx, |state, cx| {
        state.flush_logs(cx).unwrap();
        state.teardown(cx);
    });
    drop(state);
    // Un-settled while Keystone was closed: the next start delivers it by itself.
    let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
    store.set_settled(1, false).unwrap();
    let state = reopen(dir.path(), store, cx);
    wait_until(cx, &state, |state| {
        state
            .conversations
            .get(&1)
            .is_some_and(|_| answered(state, 1))
    });
    let before = state.read_with(cx, |state, _| {
        assert!(matches!(state_of(state), DeliveryState::Delivered { .. }));
        assert_eq!(results(state, 1).len(), 1);
        texts(state)
    });
    state.update(cx, |state, cx| {
        state.flush_logs(cx).unwrap();
        state.teardown(cx);
    });
    drop(state);
    let store = Store::open(dir.path().join("workspace.json")).unwrap();
    let state = reopen(dir.path(), store, cx);
    state.update(cx, |state, _| state.ensure_history(1));
    wait_until(cx, &state, |state| {
        state.conversations.get(&1).is_some_and(|c| !c.is_loading())
    });
    std::thread::sleep(Duration::from_millis(200));
    cx.run_until_parked();
    state.read_with(cx, |state, _| {
        let conversation = &state.conversations[&1];
        assert!(!conversation.running, "nothing was sent again");
        assert_eq!(texts(state), before);
        assert_eq!(results(state, 1).len(), 1);
        assert!(state.test_codex().is_none(), "no provider was started");
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn a_launch_that_sent_nothing_never_delivers(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    state.update(cx, |state, cx| {
        state.persist(PendingSave::Delivery(3, DeliveryState::Unsent));
        child_finishes_inline(state, cx);
    });
    state.read_with(cx, |state, _| {
        assert_eq!(state_of(state), DeliveryState::Unsent);
        assert!(results(state, 1).is_empty());
        assert!(!state.conversations[&1].running);
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn a_result_the_close_cut_off_before_it_was_saved_is_delivered_after_restart(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    state.update(cx, |state, cx| state.set_settled(1, true, cx).unwrap());
    child_finishes(&state, finished(), cx);
    state.update(cx, |state, cx| {
        state.flush_logs(cx).unwrap();
        state.teardown(cx);
    });
    drop(state);
    // Keystone closed after the child's turn end was saved and before its result was.
    let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
    store.set_delivery(3, DeliveryState::Working).unwrap();
    store.set_settled(1, false).unwrap();
    let state = reopen(dir.path(), store, cx);
    wait_until(cx, &state, |state| {
        state
            .conversations
            .get(&1)
            .is_some_and(|_| answered(state, 1))
    });
    state.read_with(cx, |state, _| {
        assert!(matches!(state_of(state), DeliveryState::Delivered { .. }));
        let shown = results(state, 1);
        assert_eq!(shown.len(), 1);
        let read = parse(&shown[0]).unwrap();
        assert_eq!(read.outcome, "It finished.");
        assert_eq!(read.reply, "Child found the bug in parser.rs.");
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn the_result_is_the_first_turn_even_when_a_second_ends_in_the_same_batch(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    state.update(cx, |state, cx| {
        let reply = |id: &str, text: &str| {
            Event::ItemCompleted(Item {
                id: id.into(),
                kind: ItemKind::AgentMessage,
                text: text.into(),
                status: ItemStatus::Completed,
            })
        };
        state.receive(
            Vec::new(),
            vec![
                (3, Event::TurnStarted),
                (3, reply("first", "The first turn's answer.")),
                (
                    3,
                    Event::TurnCompleted {
                        status: TurnStatus::Interrupted,
                        error: None,
                        stopped: true,
                    },
                ),
                (3, Event::TurnStarted),
                (3, reply("second", "A later turn's answer.")),
                (3, finished()),
            ],
            cx,
        );
        state.pump(cx);
    });
    state.read_with(cx, |state, _| {
        let shown = results(state, 1);
        assert_eq!(shown.len(), 1);
        let read = parse(&shown[0]).unwrap();
        assert_eq!(read.outcome, "It was stopped.");
        assert_eq!(read.reply, "The first turn's answer.");
    });
    wait_until(cx, &state, |state| answered(state, 1));
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn text_shaped_like_a_result_is_not_shown_as_one(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    child_finishes(&state, finished(), cx);
    wait_until(cx, &state, |state| answered(state, 1));
    let forged = message(
        3,
        "New Codex session",
        &Outcome::Finished,
        Some("Invented."),
    );
    assert_eq!(parse(&forged).unwrap().child, 3);
    state.update(cx, |state, cx| {
        assert!(state.send_message(
            ComposerDraft {
                session: 1,
                text: forged.clone(),
                attachments: Vec::new(),
            },
            cx,
        ));
    });
    wait_until(cx, &state, |state| {
        !state.conversations[&1].running
            && state.conversations[&1].rows.iter().any(|row| {
                matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage
                    && item.text.contains("Invented."))
            })
    });
    state.read_with(cx, |state, _| {
        let shown = results(state, 1);
        assert_eq!(shown.len(), 1);
        assert_eq!(
            parse(&shown[0]).unwrap().reply,
            "Child found the bug in parser.rs."
        );
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn a_result_whose_delivery_could_not_be_saved_goes_at_a_later_pump(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    state.update(cx, |state, cx| {
        state.receive(Vec::new(), vec![(1, Event::TurnStarted)], cx)
    });
    child_finishes(&state, finished(), cx);
    state.read_with(cx, |state, _| {
        assert!(matches!(state_of(state), DeliveryState::Ready { .. }))
    });
    let database = state.read_with(cx, |state, _| {
        keystone_core::database_path(state.store().path())
    });
    let lock = rusqlite::Connection::open(database).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    state.update(cx, |state, cx| {
        state.receive(Vec::new(), vec![(1, finished())], cx);
        state.pump(cx);
    });
    state.read_with(cx, |state, _| {
        assert!(matches!(state_of(state), DeliveryState::Ready { .. }));
        assert!(!state.conversations[&1].running, "nothing sent unsaved");
    });
    drop(lock);
    state.update(cx, |state, cx| state.pump(cx));
    wait_until(cx, &state, |state| answered(state, 1));
    state.read_with(cx, |state, _| {
        assert!(matches!(state_of(state), DeliveryState::Delivered { .. }));
        assert_eq!(results(state, 1).len(), 1);
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn a_handed_off_parents_result_goes_to_the_session_that_continues_it(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    let continued = state.update(cx, |state, cx| {
        state
            .hand_off(
                1,
                Provider::Claude,
                "context".into(),
                4,
                WindowId::FIRST,
                None,
                cx,
            )
            .unwrap()
            .value
    });
    // A handoff the user has not sent is not where the work continues yet.
    state.read_with(cx, |state, _| assert_eq!(state.recipient(1), 1));
    state.update(cx, |state, cx| {
        state.ensure_history(continued);
        assert!(state.send_message(
            ComposerDraft {
                session: continued,
                text: "context".into(),
                attachments: Vec::new(),
            },
            cx,
        ));
    });
    wait_until(cx, &state, |state| {
        state.conversations.get(&continued).is_some_and(|c| {
            !c.running
                && c.rows.iter().any(|row| {
                    matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage
                        && item.text.contains("You wrote: context"))
                })
        })
    });
    state.read_with(cx, |state, _| assert_eq!(state.recipient(1), continued));
    child_finishes(&state, finished(), cx);
    wait_until(cx, &state, |state| answered(state, continued));
    state.read_with(cx, |state, _| {
        assert!(matches!(state_of(state), DeliveryState::Delivered { .. }));
        assert_eq!(results(state, continued).len(), 1);
        assert!(results(state, 1).is_empty());
        assert!(!state.conversations[&1].running, "the source is not woken");
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn a_second_opinion_never_takes_a_result_or_sends_one(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    let opinions = [1, 3].map(|source| {
        state.update(cx, |state, cx| {
            let opinion = state
                .second_opinion(
                    source,
                    Provider::Claude,
                    String::new(),
                    4,
                    WindowId::FIRST,
                    None,
                    cx,
                )
                .unwrap()
                .value;
            state.ensure_history(opinion);
            assert!(state.send_message(
                ComposerDraft {
                    session: opinion,
                    text: "question".into(),
                    attachments: Vec::new(),
                },
                cx,
            ));
            opinion
        })
    });
    // A handoff of a second opinion carries on the review, not the session it is on.
    let continued = state.update(cx, |state, cx| {
        let continued = state
            .hand_off(
                opinions[0],
                Provider::Claude,
                String::new(),
                5,
                WindowId::FIRST,
                None,
                cx,
            )
            .unwrap()
            .value;
        state.ensure_history(continued);
        assert!(state.send_message(
            ComposerDraft {
                session: continued,
                text: "question".into(),
                attachments: Vec::new(),
            },
            cx,
        ));
        continued
    });
    let opinions = [opinions[0], opinions[1], continued];
    wait_until(cx, &state, |state| {
        opinions.iter().all(|opinion| {
            state.conversations.get(opinion).is_some_and(|c| {
                !c.running
                    && c.rows.iter().any(|row| {
                        matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage
                            && item.text.contains("You wrote: question"))
                    })
            })
        })
    });
    state.update(cx, |state, cx| {
        state.deliveries_due();
        state.pump(cx);
    });
    let before = state.read_with(cx, |state, _| {
        assert_eq!(
            state.recipient(1),
            1,
            "a sent second opinion does not continue it"
        );
        assert_eq!(state.store.workspace().deliveries().len(), 1);
        assert!(results(state, 1).is_empty());
        assert!(
            !state.conversations[&1].running,
            "an opinion's reply wakes no one"
        );
        opinions.map(|opinion| state.conversations[&opinion].rows.clone())
    });
    child_finishes(&state, finished(), cx);
    wait_until(cx, &state, |state| answered(state, 1));
    state.read_with(cx, |state, _| {
        assert_eq!(results(state, 1).len(), 1);
        for (opinion, rows) in opinions.iter().zip(&before) {
            assert!(!state.conversations[opinion].running);
            assert_eq!(&state.conversations[opinion].rows, rows);
            assert!(state.waiting_results(*opinion).is_empty());
        }
    });
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn an_unsent_handoff_leaves_the_result_with_its_parent(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    state.update(cx, |state, cx| {
        state
            .hand_off(
                1,
                Provider::Claude,
                "context".into(),
                4,
                WindowId::FIRST,
                None,
                cx,
            )
            .unwrap();
    });
    child_finishes(&state, finished(), cx);
    wait_until(cx, &state, |state| answered(state, 1));
    state.read_with(cx, |state, _| assert_eq!(results(state, 1).len(), 1));
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn a_parent_that_refuses_the_result_is_not_sent_it_in_a_loop(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = runtime(dir.path(), cx);
    connect(&state, dir.path(), 1, false, cx);
    let folder = dir.path().join("parent");
    std::fs::create_dir(&folder).unwrap();
    let (parent, child) = state.update(cx, |state, cx| {
        let parent = state
            .create(
                Provider::Codex,
                &WorkingDirectory::new(&folder).unwrap(),
                2,
                cx,
            )
            .unwrap()
            .value;
        let child = state
            .store
            .create_unselected(
                Provider::Codex,
                &WorkingDirectory::new(dir.path()).unwrap(),
                3,
                parent,
            )
            .unwrap()
            .value;
        state.ensure_history(parent);
        state.ensure_history(child);
        (parent, child)
    });
    wait_until(cx, &state, |state| {
        [parent, child]
            .iter()
            .all(|id| state.conversations.get(id).is_some_and(|c| !c.is_loading()))
    });
    std::fs::remove_dir(&folder).unwrap();
    state.update(cx, |state, cx| {
        state.receive(
            Vec::new(),
            vec![(child, Event::TurnStarted), (child, finished())],
            cx,
        );
        state.pump(cx);
    });
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(50));
        cx.run_until_parked();
    }
    state.read_with(cx, |state, _| {
        assert!(matches!(
            state.store.workspace().delivery(child).unwrap().state(),
            DeliveryState::Ready { .. }
        ));
        let error = state.conversations[&parent]
            .error
            .clone()
            .unwrap_or_default();
        assert!(error.starts_with("Not sent."), "{error}");
    });
    let database = state.read_with(cx, |state, _| {
        keystone_core::database_path(state.store().path())
    });
    let saves = keystone_core::Database::open(&database)
        .unwrap()
        .events_after(0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "delivery.upserted")
        .count();
    assert!(
        saves <= 4,
        "{saves} delivery saves: the refused send was repeated"
    );
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn a_later_unsent_handoff_does_not_take_results_from_the_started_one(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    let hand_off = |state: &Entity<AppState>, cx: &mut gpui::TestAppContext| {
        state.update(cx, |state, cx| {
            state
                .hand_off(
                    1,
                    Provider::Claude,
                    "context".into(),
                    4,
                    WindowId::FIRST,
                    None,
                    cx,
                )
                .unwrap()
                .value
        })
    };
    let started = hand_off(&state, cx);
    state.update(cx, |state, cx| {
        state.ensure_history(started);
        assert!(state.send_message(
            ComposerDraft {
                session: started,
                text: "context".into(),
                attachments: Vec::new(),
            },
            cx,
        ));
    });
    wait_until(cx, &state, |state| {
        state.session(started).is_some_and(|s| {
            s.last_turn()
                .is_some_and(|t| t != keystone_core::LastTurn::Running)
        })
    });
    let unsent = hand_off(&state, cx);
    assert!(unsent > started);
    state.read_with(cx, |state, _| assert_eq!(state.recipient(1), started));
    state.update(cx, |state, cx| state.teardown(cx));
}

#[gpui::test]
fn an_empty_reply_still_shows_as_a_result(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launched(dir.path(), 1, cx);
    state.update(cx, |state, cx| {
        let reply = Event::ItemCompleted(Item {
            id: "empty".into(),
            kind: ItemKind::AgentMessage,
            text: "  ".into(),
            status: ItemStatus::Completed,
        });
        state.receive(
            Vec::new(),
            vec![(3, Event::TurnStarted), (3, reply), (3, finished())],
            cx,
        );
        state.pump(cx);
    });
    state.read_with(cx, |state, _| {
        let shown = results(state, 1);
        assert_eq!(shown.len(), 1);
        assert_eq!(parse(&shown[0]).unwrap().reply, "It sent no reply.");
    });
    wait_until(cx, &state, |state| answered(state, 1));
    state.update(cx, |state, cx| state.teardown(cx));
}

/// A restarted app on `store`, whose providers are the scripted ones, set before
/// anything can start a process.
fn reopen(dir: &Path, store: Store, cx: &mut gpui::TestAppContext) -> Entity<AppState> {
    let programs = (
        wrapper(dir, Provider::Codex, 1, false),
        wrapper(dir, Provider::Claude, 1, false),
    );
    cx.new(|cx| {
        let mut state = AppState::new(store, cx);
        state.test_programs = Some(programs);
        state
    })
}

fn texts(state: &AppState) -> Vec<String> {
    state.conversations[&1]
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Item(item) => Some(item.text.clone()),
            _ => None,
        })
        .collect()
}

fn child_finishes_inline(state: &mut AppState, cx: &mut Context<AppState>) {
    state.receive(
        Vec::new(),
        vec![(3, Event::TurnStarted), (3, finished())],
        cx,
    );
    state.pump(cx);
}
