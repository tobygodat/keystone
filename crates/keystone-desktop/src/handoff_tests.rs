// Included into main.rs's test module, whose helpers these tests use.

#[gpui::test]
fn hand_off_opens_a_linked_session_with_the_context_in_its_composer(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = open_with_session(dir.path(), cx);
    wait_until(cx, &view, |v, cx| {
        !v.state.read(cx).conversations()[&1].is_loading()
    });
    view.update_in(cx, |v, window, cx| {
        v.hand_off(window, cx);
        assert_eq!(
            v.error.as_deref(),
            Some("Nothing sent yet.")
        );
        v.error = None;
        v.state.update(cx, |state, cx| {
            state.lock_model(1, cx).unwrap();
            state.rename(1, "Fix the login bug", cx).unwrap();
            let conversation = state.test_conversation(1);
            conversation.push_local("Why does login fail?");
            conversation.apply(Event::ItemCompleted(Item {
                id: "command".into(),
                kind: ItemKind::Command {
                    command: "cargo test".into(),
                    exit_code: Some(0),
                },
                text: "test output".into(),
                status: ItemStatus::Completed,
            }));
            conversation.apply(Event::ItemCompleted(Item {
                id: "reply".into(),
                kind: ItemKind::AgentMessage,
                text: "The token expires early.".into(),
                status: ItemStatus::Completed,
            }));
        });
        v.commit(1, cx);
        v.composer
            .update(cx, |input, cx| input.set_value("half a thought".into(), cx));
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("handoff-from").is_none());

    activity_keys(cx, "ctrl-shift-h");

    let source = view.read_with(cx, |v, cx| {
        let workspace = v.state.read(cx).store().workspace();
        assert_eq!(workspace.sessions().len(), 2, "{:?}", v.error);
        assert_eq!(workspace.selected(), Some(2));
        let new = &workspace.sessions()[1];
        assert_eq!(new.handed_off_from(), Some(1));
        assert_eq!(new.provider(), Provider::Claude);
        assert_eq!(new.title(), "Fix the login bug");
        assert!(!new.started());
        let draft = new.draft();
        assert!(draft.starts_with("Continuing from Keystone session 1, \u{201c}Fix the login bug\u{201d} (Codex)"));
        assert!(draft.contains(
            "User (#0):\nWhy does login fail?\n\n- #1 Ran `cargo test` · exit 0\n\nCodex (#2):\nThe token expires early."
        ));
        assert!(draft.contains("Budget: about "));
        assert!(!draft.contains("test output"));
        assert_eq!(v.composer.read(cx).value(), draft);
        assert_eq!(v.model_menu, Some(2));
        workspace.sessions()[0].clone()
    });
    assert_eq!(source.draft(), "half a thought");
    assert_eq!(source.handed_off_from(), None);
    assert!(cx.debug_bounds("model-column").is_some());

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    click(cx, "handoff-from");
    view.read_with(cx, |v, cx| {
        assert_eq!(v.state.read(cx).store().workspace().selected(), Some(1));
        assert_eq!(v.composer.read(cx).value(), "half a thought");
        assert_eq!(v.state.read(cx).store().workspace().sessions()[0], source);
    });
    assert!(cx.debug_bounds("handoff-from").is_none());
    assert!(
        cx.debug_bounds("handoff-to-2").is_none(),
        "an unsent continuation does not mark the source"
    );
    view.update(cx, |v, cx| {
        v.state.update(cx, |state, cx| state.lock_model(2, cx).unwrap());
    });
    cx.run_until_parked();
    click(cx, "handoff-to-2");
    view.read_with(cx, |v, cx| {
        assert_eq!(v.state.read(cx).store().workspace().selected(), Some(2));
    });

    click(cx, "handoff-from");
    view.update_in(cx, |v, window, cx| v.focus.focus(window, cx));
    run_from_palette(cx, "hand off");
    view.read_with(cx, |v, cx| {
        let workspace = v.state.read(cx).store().workspace();
        assert_eq!(workspace.selected(), Some(3));
        assert_eq!(workspace.sessions()[2].handed_off_from(), Some(1));
    });
    cx.simulate_keystrokes("escape");
    view.update(cx, |v, cx| {
        v.state.update(cx, |state, cx| state.lock_model(3, cx).unwrap());
    });
    click(cx, "handoff-from");
    assert!(cx.debug_bounds("handoff-to-2").is_some());
    assert!(cx.debug_bounds("handoff-to-3").is_some());
}

#[gpui::test]
fn hand_off_waits_for_the_whole_history(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = open_with_session(dir.path(), cx);
    view.update(cx, |v, cx| {
        v.state.update(cx, |state, cx| state.lock_model(1, cx).unwrap());
    });
    let unreadable = || {
        let mut conversation = Conversation::loading();
        conversation.restore(Err("disk unreadable".into()));
        conversation
    };
    for (conversation, error) in [
        (Conversation::loading(), "Still loading"),
        (unreadable(), "Could not read the history"),
    ] {
        view.update_in(cx, |v, window, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversations_mut().insert(1, conversation);
            });
            v.hand_off(window, cx);
        });
        view.read_with(cx, |v, cx| {
            assert_eq!(v.state.read(cx).store().workspace().sessions().len(), 1);
            assert!(v.error.as_deref().is_some_and(|e| e.contains(error)), "{:?}", v.error);
        });
    }
}
