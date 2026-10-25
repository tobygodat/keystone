// Included into main.rs's test module, whose helpers these tests use.

use crate::conversation::ForkFrom;
use keystone_core::ForkPoint;

fn press(cx: &mut gpui::VisualTestContext, selector: &str) {
    let bounds = cx
        .debug_bounds(Box::leak(selector.to_owned().into_boxed_str()))
        .unwrap_or_else(|| panic!("{selector} not rendered"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

/// A window on one Codex session whose provider is the scripted Codex, logging the
/// methods it receives to `methods`.
fn open_with_codex<'a>(
    dir: &Path,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext, PathBuf) {
    let home = dir.join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    let methods = dir.join("methods");
    let codex = wrapper(
        dir,
        "keystone-fake-codex",
        &format!(
            "KEYSTONE_FAKE_CODEX_DELAY_MS=1 CODEX_HOME='{}' KEYSTONE_FAKE_CODEX_METHOD_LOG='{}'",
            home.display(),
            methods.display()
        ),
    );
    let (view, cx) = open_with_session(dir, cx);
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, _| {
            state.test_set_codex(Some(CodexConnection::new(
                Some(codex),
                "test",
                state.test_wake().clone(),
            )));
        })
    });
    wait_until(cx, &view, |v, cx| {
        !v.state.read(cx).conversations()[&1].is_loading()
    });
    (view, cx, methods)
}

fn ended_with(session: u64, text: &'static str) -> impl Fn(&Keystone, &App) -> bool {
    move |v, cx| {
        v.state
            .read(cx)
            .conversations()
            .get(&session)
            .is_some_and(|c| {
                !c.running
                    && c.rows.iter().any(|row| {
                        matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage
                            && item.status == ItemStatus::Completed && item.text.contains(text))
                    })
            })
    }
}

/// The texts of a conversation's user and agent messages.
fn messages(v: &Keystone, session: u64, cx: &App) -> Vec<String> {
    v.state.read(cx).conversations()[&session]
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Item(item)
                if matches!(item.kind, ItemKind::UserMessage | ItemKind::AgentMessage) =>
            {
                Some(match item.text.split_once("You wrote: ") {
                    Some((_, wrote)) => format!("You wrote: {}", wrote.split('.').next().unwrap()),
                    None => item.text.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

fn reply_row(v: &Keystone, session: u64, text: &str, cx: &App) -> usize {
    v.state.read(cx).conversations()[&session]
        .rows
        .iter()
        .position(|row| {
            matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage && item.text.contains(text))
        })
        .unwrap()
}

/// Three turns in session 1, each with the fork point Codex named for it.
fn three_turns(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Vec<ForkPoint> {
    for (text, reply) in [
        ("first", "You wrote: first"),
        ("second", "You wrote: second"),
        ("third", "You wrote: third"),
    ] {
        send_text(view, cx, text);
        wait_until(cx, view, ended_with(1, reply));
    }
    view.read_with(cx, |v, cx| {
        let conversation = &v.state.read(cx).conversations()[&1];
        assert_eq!(conversation.forks.len(), 3, "{:?}", conversation.forks);
        conversation
            .forks
            .iter()
            .map(|mark| mark.point.clone())
            .collect()
    })
}

#[gpui::test]
fn fork_from_here_opens_a_native_fork_beside_the_source_through_that_turn(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx, methods) = open_with_codex(dir.path(), cx);
    view.update(cx, |v, cx| {
        v.state
            .update(cx, |state, cx| state.rename(1, "Plan the release", cx).unwrap());
    });
    let points = three_turns(&view, cx);
    let (source_rows, source_thread, row) = view.read_with(cx, |v, cx| {
        let state = v.state.read(cx);
        (
            state.conversations()[&1].rows.clone(),
            state.session(1).unwrap().provider_thread().unwrap().to_owned(),
            reply_row(v, 1, "You wrote: second", cx),
        )
    });
    assert!(points.iter().all(|p| p.thread == source_thread));
    view.update_in(cx, |v, window, cx| {
        v.composer
            .update(cx, |input, cx| input.set_value("half a thought".into(), cx));
        v.focus.focus(window, cx);
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("fork-from").is_none());

    press(cx, &format!("fork-{row}"));

    let from = view.read_with(cx, |v, cx| {
        let state = v.state.read(cx);
        let workspace = state.store().workspace();
        assert_eq!(workspace.sessions().len(), 2, "{:?}", v.error);
        assert_eq!(workspace.selected(), Some(2));
        let (source, fork) = (&workspace.sessions()[0], &workspace.sessions()[1]);
        assert_eq!(fork.forked_from(), Some(1));
        assert_eq!(fork.fork_point(), Some(&points[1]));
        assert_eq!(fork.provider(), source.provider());
        assert_eq!(fork.account(), source.account());
        assert_eq!(fork.model(), source.model());
        assert_eq!(fork.context(), source.context());
        assert_eq!(fork.title(), "Plan the release (fork)");
        assert!(fork.started());
        assert_eq!(fork.provider_thread(), None);
        assert_eq!(fork.draft(), "");
        assert_eq!(source.draft(), "half a thought");
        assert_eq!(v.composer.read(cx).value(), "");
        assert_eq!(
            messages(v, 2, cx),
            ["first", "You wrote: first", "second", "You wrote: second"]
        );
        assert_eq!(state.conversations()[&1].rows, source_rows);
        assert_eq!(state.conversations()[&2].forks.len(), 2);
        let tabs: Vec<u64> = v.session_tabs(cx).iter().map(|(_, s)| s).collect();
        assert_eq!(tabs, [1, 2], "the fork opens in a tab beside its source");
        let (fork_view, _) = v.session_tabs(cx).iter().find(|(_, s)| *s == 2).unwrap();
        arrangement::selector("fork-from", fork_view).to_string()
    });
    assert!(cx.debug_bounds(Box::leak(from.clone().into_boxed_str())).is_some());
    assert!(
        std::fs::read_to_string(&methods)
            .unwrap()
            .lines()
            .all(|m| m != "thread/fork"),
        "nothing is forked before the fork's first message"
    );

    send_text(&view, cx, "in the fork");
    wait_until(cx, &view, ended_with(2, "You wrote: in the fork"));
    view.read_with(cx, |v, cx| {
        let state = v.state.read(cx);
        let thread = state.session(2).unwrap().provider_thread().unwrap();
        assert_ne!(thread, source_thread);
        assert_eq!(state.conversations()[&1].rows, source_rows);
        let forks = &state.conversations()[&2].forks;
        assert_eq!(forks.len(), 3);
        assert_eq!(forks[2].point.thread, thread);
        assert_eq!(
            messages(v, 2, cx),
            [
                "first",
                "You wrote: first",
                "second",
                "You wrote: second",
                "in the fork",
                "You wrote: in the fork"
            ]
        );
    });
    let methods = std::fs::read_to_string(&methods).unwrap();
    assert_eq!(methods.lines().filter(|m| *m == "thread/fork").count(), 1);

    press(cx, &from);
    view.read_with(cx, |v, cx| {
        assert_eq!(v.state.read(cx).store().workspace().selected(), Some(1));
        assert_eq!(v.composer.read(cx).value(), "half a thought");
    });
    press(cx, "fork-to-2");
    view.read_with(cx, |v, cx| {
        assert_eq!(v.state.read(cx).store().workspace().selected(), Some(2));
    });

    let store = Store::open(dir.path().join("workspace.json")).unwrap();
    assert_eq!(store.workspace().sessions()[1].forked_from(), Some(1));
}

#[gpui::test]
fn the_keyboard_forks_the_focused_message(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx, _) = open_with_codex(dir.path(), cx);
    let points = three_turns(&view, cx);
    let row = view.read_with(cx, |v, cx| {
        v.state.read(cx).conversations()[&1]
            .rows
            .iter()
            .position(|row| matches!(row, Row::Item(item) if item.text == "first"))
            .unwrap()
    });
    assert_eq!(row, 0);
    // The first row, then its actions: Copy message, then Fork from here.
    activity_keys(cx, "ctrl-shift-t home tab tab");
    view.read_with(cx, |v, _| {
        let key = v.presentation.current.views.keyboard_state(MAIN_1).unwrap();
        assert_eq!(key.row, Some(0));
        assert_eq!(key.action, Some(view_state::CopyTarget::Fork));
    });
    activity_keys(cx, "enter");
    view.read_with(cx, |v, cx| {
        let workspace = v.state.read(cx).store().workspace();
        assert_eq!(workspace.sessions().len(), 2, "{:?}", v.error);
        assert_eq!(workspace.sessions()[1].fork_point(), Some(&points[0]));
        assert_eq!(messages(v, 2, cx), ["first", "You wrote: first"]);
    });

    view.update(cx, |v, cx| v.select(1, cx));
    cx.run_until_parked();
    activity_keys(cx, "ctrl-shift-t end ctrl-shift-f");
    view.read_with(cx, |v, cx| {
        let workspace = v.state.read(cx).store().workspace();
        assert_eq!(workspace.sessions().len(), 3, "{:?}", v.error);
        assert_eq!(workspace.sessions()[2].fork_point(), Some(&points[2]));
        assert_eq!(messages(v, 3, cx).len(), 6);
    });
}

#[gpui::test]
fn a_turn_with_no_native_point_forks_as_a_labelled_handoff(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = open_with_session(dir.path(), cx);
    wait_until(cx, &view, |v, cx| {
        !v.state.read(cx).conversations()[&1].is_loading()
    });
    view.update(cx, |v, cx| {
        v.state.update(cx, |state, cx| {
            state.lock_model(1, cx).unwrap();
            state.rename(1, "Old history", cx).unwrap();
            let conversation = state.test_conversation(1);
            for (question, answer) in [("one", "answer one"), ("two", "answer two")] {
                conversation.push_local(question);
                conversation.apply(Event::TurnStarted);
                conversation.apply(Event::ItemCompleted(Item {
                    id: format!("reply-{question}"),
                    kind: ItemKind::AgentMessage,
                    text: answer.into(),
                    status: ItemStatus::Completed,
                }));
                conversation.apply(Event::TurnCompleted {
                    status: TurnStatus::Completed,
                    error: None,
                    stopped: false,
                });
            }
            conversation.push_local("three");
            conversation.apply(Event::TurnStarted);
        });
        v.commit(1, cx);
    });
    cx.run_until_parked();
    let tooltip = view.read_with(cx, |v, cx| {
        let conversation = &v.state.read(cx).conversations()[&1];
        assert_eq!(conversation.fork_from(4), ForkFrom::Running);
        crate::fork::label(&conversation.fork_from(1))
    });
    assert_eq!(tooltip, "Fork from here (handoff)");

    view.update_in(cx, |v, window, cx| v.fork_from(MAIN_1, 4, window, cx));
    view.read_with(cx, |v, cx| {
        assert_eq!(v.state.read(cx).store().workspace().sessions().len(), 1);
        assert!(v.error.as_deref().is_some_and(|e| e.contains("still running")));
    });

    press(cx, "fork-1");
    view.read_with(cx, |v, cx| {
        let workspace = v.state.read(cx).store().workspace();
        assert_eq!(workspace.sessions().len(), 2, "{:?}", v.error);
        let fork = &workspace.sessions()[1];
        assert_eq!(fork.forked_from(), Some(1));
        assert_eq!(fork.fork_point(), None);
        assert_eq!(fork.title(), "Old history (handoff fork)");
        assert!(!fork.started());
        assert_eq!(fork.provider(), Provider::Codex);
        let draft = fork.draft();
        assert!(draft.starts_with("Continuing from Keystone session 1"), "{draft}");
        assert!(draft.contains("answer one") && !draft.contains("answer two"), "{draft}");
        assert_eq!(v.composer.read(cx).value(), draft);
        assert!(v.state.read(cx).conversations()[&2].rows.is_empty());
    });
}
