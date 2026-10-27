// Included into main.rs's test module, whose helpers these tests use.

/// Session 1 with a sent conversation: a question, a command and a finished reply at
/// row 2, then a second question and a finished reply at row 4.
fn opinion_source<'a>(
    dir: &Path,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
    let (view, cx) = open_with_session(dir, cx);
    wait_until(cx, &view, |v, cx| {
        !v.state.read(cx).conversations()[&1].is_loading()
    });
    view.update(cx, |v, cx| {
        v.state.update(cx, |state, cx| {
            state.lock_model(1, cx).unwrap();
            state.rename(1, "Fix the login bug", cx).unwrap();
            let conversation = state.test_conversation(1);
            conversation.push_local("Why does login fail?");
            for (id, kind, text) in [
                (
                    "command",
                    ItemKind::Command {
                        command: "cargo test".into(),
                        exit_code: Some(0),
                    },
                    "test output",
                ),
                ("reply-1", ItemKind::AgentMessage, "The token expires early."),
            ] {
                conversation.apply(Event::ItemCompleted(Item {
                    id: id.into(),
                    kind,
                    text: text.into(),
                    status: ItemStatus::Completed,
                }));
            }
            conversation.push_local("Should we refresh it?");
            conversation.apply(Event::ItemCompleted(Item {
                id: "reply-2".into(),
                kind: ItemKind::AgentMessage,
                text: "Refresh it a minute early.".into(),
                status: ItemStatus::Completed,
            }));
        });
        v.commit(1, cx);
    });
    cx.run_until_parked();
    (view, cx)
}

/// `base` as named in the active view.
fn active(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, base: &str) -> &'static str {
    let shown = cx
        .read(|cx| view.read(cx).active_view(cx))
        .expect("an active view");
    arrangement::selector(base, shown).to_string().leak()
}

/// Whether the active view shows the element named `base`.
fn shows(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, base: &str) -> bool {
    let name = active(view, cx, base);
    cx.debug_bounds(name).is_some()
}

/// Clicks the element named `base` in the active view.
fn press(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, base: &str) {
    let name = active(view, cx, base);
    click(cx, name);
}

/// Waits until `session`'s history has been read, as it is once a view shows it.
fn loaded(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, session: u64) {
    view.update(cx, |v, cx| {
        v.state.update(cx, |state, _| state.ensure_history(session))
    });
    wait_until(cx, view, |v, cx| {
        !v.state.read(cx).conversations()[&session].is_loading()
    });
}

#[gpui::test]
fn a_second_opinion_opens_read_only_beside_its_source_and_its_reply_waits_there(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = opinion_source(dir.path(), cx);
    view.update(cx, |v, cx| {
        v.composer
            .update(cx, |input, cx| input.set_value("half a thought".into(), cx));
        cx.notify();
    });
    cx.run_until_parked();
    let before = view.read_with(cx, |v, cx| {
        v.state.read(cx).store().workspace().sessions()[0].clone()
    });

    activity_keys(cx, "secondary-alt-o");

    view.read_with(cx, |v, cx| {
        let workspace = v.state.read(cx).store().workspace();
        assert_eq!(workspace.sessions().len(), 2, "{:?}", v.error);
        assert_eq!(workspace.selected(), Some(2));
        let new = &workspace.sessions()[1];
        assert!(new.second_opinion());
        assert_eq!(new.handed_off_from(), Some(1));
        assert_eq!(new.provider(), Provider::Claude);
        assert_eq!(new.approval_mode(), ApprovalMode::ReadOnly);
        assert_eq!(new.title(), "Second opinion: Fix the login bug");
        assert!(!new.started());
        let draft = new.draft();
        assert!(draft.starts_with(
            "For a second opinion on Keystone session 1, \u{201c}Fix the login bug\u{201d} (Codex)"
        ));
        assert!(draft.contains("Codex (#4):\nRefresh it a minute early."));
        assert!(draft.ends_with(
            "Give a second opinion on this conversation, especially Codex's latest reply. Read \
             only: change nothing. Where do you agree, where do you disagree and why, and what \
             would you do instead?"
        ));
        assert_eq!(v.composer.read(cx).value(), draft);
        assert_eq!(v.model_menu, Some(2));
        let tabs = v.session_tabs(cx);
        assert!(tabs.view_of(1).is_some(), "the source keeps its tab");
        assert!(
            tabs.view_of(2).is_some(),
            "the second opinion opens beside it"
        );
        assert_eq!(workspace.sessions()[0].draft(), "half a thought");
        assert_eq!(workspace.sessions()[0].last_turn(), before.last_turn());
        assert_eq!(workspace.sessions()[0].approval_mode(), before.approval_mode());
    });
    assert!(shows(&view, cx, "opinion-of"));
    assert!(!shows(&view, cx, "handoff-from"));

    // Unsent, it does not mark the source.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    press(&view, cx, "opinion-of");
    view.read_with(cx, |v, cx| {
        assert_eq!(v.state.read(cx).store().workspace().selected(), Some(1));
        assert_eq!(v.composer.read(cx).value(), "half a thought");
    });
    assert!(!shows(&view, cx, "opinion-2"));

    loaded(&view, cx, 2);
    view.update(cx, |v, cx| {
        v.state.update(cx, |state, cx| {
            state.lock_model(2, cx).unwrap();
            state.test_conversation(2).push_local("What do you think?");
            state.test_conversation(2).running = true;
        });
        v.refresh(cx);
    });
    cx.run_until_parked();
    assert!(shows(&view, cx, "opinion-2"));
    assert!(!shows(&view, cx, "handoff-to-2"));
    assert!(
        !shows(&view, cx, "opinion-use-2"),
        "nothing to use while it runs"
    );

    view.update(cx, |v, cx| {
        v.state.update(cx, |state, cx| {
            let conversation = state.test_conversation(2);
            conversation.apply(Event::ItemCompleted(Item {
                id: "opinion".into(),
                kind: ItemKind::AgentMessage,
                text: "I agree, but also retry once.".into(),
                status: ItemStatus::Completed,
            }));
            conversation.running = false;
            state.record_turn_end(2, false, 10, cx).unwrap();
        });
        v.commit(2, cx);
        v.refresh(cx);
    });
    cx.run_until_parked();
    let rows = view.read_with(cx, |v, cx| v.state.read(cx).conversations()[&1].rows.len());

    press(&view, cx, "opinion-use-2");
    press(&view, cx, "opinion-use-2");

    view.read_with(cx, |v, cx| {
        let state = v.state.read(cx);
        assert_eq!(state.store().workspace().selected(), Some(1));
        let expected = "half a thought\n\nSecond opinion from Claude Code (Keystone session 2):\n\n\
                        I agree, but also retry once.";
        assert_eq!(v.composer.read(cx).value(), expected);
        assert_eq!(state.store().workspace().sessions()[0].draft(), expected);
        assert_eq!(state.conversations()[&1].rows.len(), rows, "nothing is sent");
        assert_eq!(
            state.store().workspace().sessions()[0].last_turn(),
            before.last_turn()
        );
    });

    press(&view, cx, "opinion-2");
    view.read_with(cx, |v, cx| {
        assert_eq!(v.state.read(cx).store().workspace().selected(), Some(2));
    });
}

#[gpui::test]
fn a_second_opinion_on_one_reply_carries_the_conversation_up_to_it(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = opinion_source(dir.path(), cx);
    press(&view, cx, "second-opinion-2");
    view.read_with(cx, |v, cx| {
        let workspace = v.state.read(cx).store().workspace();
        assert_eq!(workspace.selected(), Some(2), "{:?}", v.error);
        let draft = workspace.sessions()[1].draft();
        assert!(draft.contains("Codex (#2):\nThe token expires early."));
        assert!(!draft.contains("Should we refresh it?"));
        assert!(draft.contains("Give a second opinion on Codex's reply #2. Read only"));
    });
}

#[gpui::test]
fn the_shortcut_asks_about_the_reply_focused_in_the_transcript(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = opinion_source(dir.path(), cx);
    view.update_in(cx, |v, window, cx| {
        let key = v.view_key(v.active_view(cx).unwrap(), cx).unwrap();
        v.presentation.current.views.keyboard(key, cx).row = Some(2);
        v.presentation.current.views.focus(key, cx).focus(window, cx);
    });
    cx.run_until_parked();
    activity_keys(cx, "secondary-alt-o");
    view.read_with(cx, |v, cx| {
        let workspace = v.state.read(cx).store().workspace();
        assert_eq!(workspace.selected(), Some(2), "{:?}", v.error);
        let draft = workspace.sessions()[1].draft();
        assert!(draft.contains("on Codex's reply #2."), "{draft}");
    });

    // From the palette, focus is not in the transcript, so it covers the conversation.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    press(&view, cx, "opinion-of");
    view.update_in(cx, |v, window, cx| v.focus.focus(window, cx));
    run_from_palette(cx, "second opinion");
    view.read_with(cx, |v, cx| {
        let workspace = v.state.read(cx).store().workspace();
        assert_eq!(workspace.selected(), Some(3), "{:?}", v.error);
        assert!(
            workspace.sessions()[2]
                .draft()
                .contains("especially Codex's latest reply")
        );
    });
}

#[gpui::test]
fn a_second_opinion_is_refused_until_there_is_something_to_ask_about(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = open_with_session(dir.path(), cx);
    wait_until(cx, &view, |v, cx| {
        !v.state.read(cx).conversations()[&1].is_loading()
    });
    view.update_in(cx, |v, window, cx| {
        v.ask_second_opinion(None, window, cx);
        assert_eq!(
            v.error.as_deref(),
            Some("Nothing sent yet.")
        );
        v.state.update(cx, |state, cx| {
            state.lock_model(1, cx).unwrap();
            state
                .test_conversations_mut()
                .insert(1, Conversation::loading());
        });
        v.ask_second_opinion(None, window, cx);
        assert_eq!(
            v.error.as_deref(),
            Some("Still loading.")
        );
        assert_eq!(v.state.read(cx).store().workspace().sessions().len(), 1);
    });
}

/// Session 2, a sent second opinion on [`opinion_source`]'s session 1 whose first turn
/// answered "First answer.", shown beside it with the source selected.
fn answered_opinion(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) {
    activity_keys(cx, "secondary-alt-o");
    cx.simulate_keystrokes("escape");
    loaded(view, cx, 2);
    view.update(cx, |v, cx| {
        v.state.update(cx, |state, cx| {
            state.lock_model(2, cx).unwrap();
            let conversation = state.test_conversation(2);
            conversation.push_local("What do you think?");
            conversation.apply(Event::ItemCompleted(Item {
                id: "answer".into(),
                kind: ItemKind::AgentMessage,
                text: "First answer.".into(),
                status: ItemStatus::Completed,
            }));
            conversation.apply(Event::TurnCompleted {
                status: TurnStatus::Completed,
                error: None,
                stopped: false,
            });
            state.record_turn_end(2, false, 10, cx).unwrap();
        });
        v.commit(2, cx);
    });
    cx.run_until_parked();
    press(view, cx, "opinion-of");
    assert!(shows(view, cx, "opinion-use-2"));
}

#[gpui::test]
fn a_stopped_second_opinion_offers_nothing_to_use(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = opinion_source(dir.path(), cx);
    answered_opinion(&view, cx);

    // A follow-up stopped before it answered leaves only the earlier answer, which
    // no longer answers the latest message.
    view.update(cx, |v, cx| {
        v.state.update(cx, |state, cx| {
            let conversation = state.test_conversation(2);
            conversation.push_local("And the refresh?");
            conversation.apply(Event::ItemStarted(Item {
                id: "partial".into(),
                kind: ItemKind::AgentMessage,
                text: "Half an".into(),
                status: ItemStatus::InProgress,
            }));
            conversation.apply(Event::TurnCompleted {
                status: TurnStatus::Interrupted,
                error: None,
                stopped: true,
            });
            state.record_turn_end(2, false, 11, cx).unwrap();
        });
        v.commit(2, cx);
        v.refresh(cx);
    });
    cx.run_until_parked();
    assert!(shows(&view, cx, "opinion-2"));
    assert!(!shows(&view, cx, "opinion-use-2"));
    view.read_with(cx, |v, cx| {
        assert_eq!(
            v.state.read(cx).session(2).unwrap().last_turn(),
            Some(keystone_core::LastTurn::Finished),
            "a user Stop finishes the turn, so the transcript must tell"
        );
    });
}

#[gpui::test]
fn use_in_conversation_reads_the_answer_when_pressed(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = opinion_source(dir.path(), cx);
    answered_opinion(&view, cx);
    // Another window starts a follow-up after the button was drawn, and the press
    // lands before this window draws again.
    view.update_in(cx, |v, window, cx| {
        v.state.update(cx, |state, _| {
            let conversation = state.test_conversation(2);
            conversation.push_local("Say more");
            conversation.running = true;
        });
        let key = v.view_key(v.active_view(cx).unwrap(), cx).unwrap();
        v.use_second_opinion(key, 2, window, cx);
    });
    // Or it failed after an intermediate reply, which leaves no row behind.
    view.update_in(cx, |v, window, cx| {
        v.state.update(cx, |state, cx| {
            let conversation = state.test_conversation(2);
            conversation.apply(Event::ItemCompleted(Item {
                id: "intermediate".into(),
                kind: ItemKind::AgentMessage,
                text: "Looking into it.".into(),
                status: ItemStatus::Completed,
            }));
            conversation.apply(Event::Failed {
                message: "connection lost".into(),
            });
            state.record_turn_end(2, true, 12, cx).unwrap();
        });
        let key = v.view_key(v.active_view(cx).unwrap(), cx).unwrap();
        v.use_second_opinion(key, 2, window, cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        assert_eq!(v.composer.read(cx).value(), "");
        assert!(
            v.error
                .as_deref()
                .is_some_and(|e| e.contains("No finished reply")),
            "{:?}",
            v.error
        );
    });
}
