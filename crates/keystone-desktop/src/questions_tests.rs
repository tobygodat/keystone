use super::*;
use crate::{Keystone, close_window, open_windows};
use gpui::{AnyWindowHandle, Focusable, TestAppContext, VisualTestContext, WeakEntity};
use keystone_agent::{Choice, Event, Field, Question};
use keystone_core::{Attention, Provider, Store, WindowId, WorkingDirectory};
use std::path::Path;

/// A workspace with one Claude session, selected, in one window.
fn launch(dir: &Path, cx: &mut TestAppContext) -> Entity<AppState> {
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    store
        .create(Provider::Claude, &working_directory(dir), 0)
        .unwrap();
    store.select(1).unwrap();
    cx.executor().allow_parking();
    let state = cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state = crate::test_support::install(store, crate::test_support::no_providers(), cx);
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            crate::windows::open(state.clone(), id, placement, true, false, cx).unwrap();
        }
        state
    });
    let first = view(cx);
    handle(&first, cx)
        .update(cx, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
    state
}

/// A scratch git repository, so the session these tests open never reads the real
/// checkout.
fn working_directory(dir: &Path) -> WorkingDirectory {
    let repo = dir.join("project");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# project\n").unwrap();
    let status = std::process::Command::new("git")
        .args(["-c", "maintenance.auto=false", "init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(status.success());
    WorkingDirectory::new(&repo).unwrap()
}

fn view(cx: &mut TestAppContext) -> Entity<Keystone> {
    cx.update(|cx| open_windows(cx))
        .into_iter()
        .find(|v| cx.read(|cx| v.read(cx).window_id) == WindowId::FIRST)
        .unwrap()
}

fn handle(view: &Entity<Keystone>, cx: &mut TestAppContext) -> AnyWindowHandle {
    cx.read(|cx| view.read(cx).handle)
}

fn choice(label: &str) -> Choice {
    Choice {
        label: label.into(),
        description: None,
    }
}

/// Claude's scripted questions: one color, or typed text; any number of fruits.
fn asked(request: &str) -> Vec<(u64, Event)> {
    let question = Question {
        request: RequestId(request.into()),
        server: None,
        message: None,
        fields: vec![
            Field {
                header: "Color".into(),
                prompt: Some("Which color do you prefer?".into()),
                input: Input::Choice {
                    options: vec![choice("Red"), choice("Blue")],
                    multiple: false,
                    other: true,
                },
                required: true,
                bounds: Default::default(),
            },
            Field {
                header: "Fruit".into(),
                prompt: Some("Which fruits do you like?".into()),
                input: Input::Choice {
                    options: vec![choice("Apple"), choice("Banana"), choice("Cherry")],
                    multiple: true,
                    other: true,
                },
                required: true,
                bounds: Default::default(),
            },
        ],
    };
    vec![(1, Event::TurnStarted), (1, Event::QuestionAsked(question))]
}

/// Applies `events`, then waits for the session's history to load: until it has, a
/// question or approval is held rather than shown, and on a loaded machine the load
/// can take longer than one pass of the executor.
fn receive(state: &Entity<AppState>, events: Vec<(u64, Event)>, cx: &mut TestAppContext) {
    state.update(cx, |state, cx| state.receive(Vec::new(), events, cx));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        let loading = cx.read(|cx| {
            state
                .read(cx)
                .conversations()
                .get(&1)
                .is_none_or(|c| c.is_loading())
        });
        if !loading {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the history never loaded"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn answers(state: &Entity<AppState>, cx: &TestAppContext) -> Option<Vec<Answer>> {
    cx.read(|cx| {
        state.read(cx).conversations()[&1]
            .questions
            .first()
            .map(|q| q.answers.clone())
    })
}

fn answer(chosen: &[usize], text: &str) -> Answer {
    Answer {
        chosen: chosen.to_vec(),
        text: text.into(),
    }
}

fn click(cx: &mut VisualTestContext, name: &'static str) {
    let bounds = cx
        .debug_bounds(name)
        .unwrap_or_else(|| panic!("{name} is drawn"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

/// Presses each key and lets it go, as a person does: a focused button clicks on the
/// release of Enter or Space.
fn keys(cx: &mut VisualTestContext, strokes: &str) {
    for key in strokes.split_whitespace() {
        cx.simulate_keystrokes(key);
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse(key).unwrap(),
        });
        cx.run_until_parked();
    }
}

#[gpui::test]
fn a_question_is_answered_with_the_mouse_then_with_the_keyboard_alone(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(dir.path(), cx);
    let first = view(cx);
    receive(&state, asked("q1"), cx);
    let (attention, blocker) = cx.read(|cx| {
        let state = state.read(cx);
        let live = &state.conversations()[&1];
        let session = &state.store().workspace().sessions()[0];
        (
            session.attention(
                !live.approvals.is_empty(),
                live.awaiting_input(),
                live.running,
            ),
            live.settle_blocker(),
        )
    });
    assert_eq!(
        attention,
        Some(Attention::Input),
        "the session waits for your answer, as for a widget"
    );
    assert_eq!(
        blocker,
        Some("Answer the question before settling this session.")
    );

    let mut vcx = VisualTestContext::from_window(handle(&first, cx), cx);
    assert!(
        vcx.debug_bounds("question-submit").is_some(),
        "the card shows under the transcript"
    );
    click(&mut vcx, "question-0-1");
    click(&mut vcx, "question-1-0");
    click(&mut vcx, "question-1-2");
    assert_eq!(
        answers(&state, &vcx),
        Some(vec![answer(&[1], ""), answer(&[0, 2], "")])
    );
    click(&mut vcx, "question-submit");
    assert_eq!(answers(&state, &vcx), None, "submitted");

    receive(&state, asked("q2"), &mut vcx);
    first.update_in(&mut vcx, |view, window, cx| {
        view.composer.focus_handle(cx).focus(window, cx)
    });
    // From the composer, Shift+Tab reaches Decline (Submit waits for complete
    // answers), the Fruit field, then Cherry, Banana and Apple, then the Color field.
    keys(
        &mut vcx,
        "shift-tab shift-tab shift-tab space shift-tab shift-tab space shift-tab",
    );
    vcx.simulate_input("Teal");
    vcx.run_until_parked();
    assert_eq!(
        answers(&state, &vcx),
        Some(vec![answer(&[], "Teal"), answer(&[2, 0], "")])
    );
    keys(&mut vcx, "tab tab tab tab tab enter");
    assert_eq!(answers(&state, &vcx), None, "Enter on Submit sends it");
    let composer_focused = first.update_in(&mut vcx, |view, window, cx| {
        view.composer.focus_handle(cx).is_focused(window)
    });
    assert!(
        composer_focused,
        "answering hands focus back to the composer"
    );
}

#[gpui::test]
fn a_half_answered_question_survives_closing_every_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(dir.path(), cx);
    let first = view(cx);
    receive(&state, asked("q1"), cx);
    let mut vcx = VisualTestContext::from_window(handle(&first, cx), cx);
    click(&mut vcx, "question-0-1");
    click(&mut vcx, "question-1-text");
    vcx.simulate_input("Kiwi");
    vcx.run_until_parked();

    let root: WeakEntity<Keystone> = first.downgrade();
    handle(&first, cx)
        .update(cx, |_, window, cx| {
            assert!(close_window(&root, window, cx));
            window.remove_window();
        })
        .unwrap();
    cx.run_until_parked();
    assert!(cx.update(|cx| open_windows(cx)).is_empty());
    assert_eq!(
        answers(&state, cx),
        Some(vec![answer(&[1], ""), answer(&[], "Kiwi")]),
        "the answers wait with the app, not the window"
    );

    cx.update(|cx| crate::windows::reopen(state.clone(), cx));
    cx.run_until_parked();
    let reopened = view(cx);
    let mut vcx = VisualTestContext::from_window(handle(&reopened, cx), cx);
    vcx.run_until_parked();
    let typed: Vec<String> = vcx.read(|cx| {
        reopened
            .read(cx)
            .question_controls
            .borrow()
            .fields
            .values()
            .map(|(input, _)| input.read(cx).value().to_owned())
            .collect()
    });
    assert!(
        typed.contains(&"Kiwi".to_owned()),
        "the reopened card's field holds the typed answer: {typed:?}"
    );
    click(&mut vcx, "question-submit");
    assert_eq!(answers(&state, &vcx), None);
    let kept = vcx.read(|cx| reopened.read(cx).question_controls.borrow().fields.len());
    assert_eq!(kept, 0, "an answered question's editors are freed");
}

#[gpui::test]
fn a_question_that_arrives_while_an_approval_waits_waits_behind_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(dir.path(), cx);
    let first = view(cx);
    let mut events = asked("q1");
    events.insert(
        1,
        (
            1,
            Event::ApprovalRequested(keystone_agent::Approval {
                request: RequestId("a1".into()),
                item_id: None,
                action: "rm -rf build".into(),
                detail: None,
            }),
        ),
    );
    receive(&state, events, cx);
    let mut vcx = VisualTestContext::from_window(handle(&first, cx), cx);
    assert!(vcx.debug_bounds("approve").is_some(), "the approval shows");
    assert!(
        vcx.debug_bounds("question-submit").is_none(),
        "the question waits behind it"
    );
    let attention = vcx.read(|cx| {
        let state = state.read(cx);
        let live = &state.conversations()[&1];
        state.store().workspace().sessions()[0].attention(
            !live.approvals.is_empty(),
            live.awaiting_input(),
            live.running,
        )
    });
    assert_eq!(attention, Some(Attention::Approval));

    // Answering the question, as another window's card could, leaves the approval.
    state.update(&mut vcx, |state, cx| {
        state.edit_answer(1, &RequestId("q1".into()), 0, |a| a.chosen = vec![0], cx);
        state.edit_answer(1, &RequestId("q1".into()), 1, |a| a.chosen = vec![0], cx);
        state.reply(1, &RequestId("q1".into()), false, cx);
    });
    vcx.run_until_parked();
    let approvals: Vec<String> = vcx.read(|cx| {
        state.read(cx).conversations()[&1]
            .approvals
            .iter()
            .map(|a| a.request.0.clone())
            .collect()
    });
    assert_eq!(approvals, ["a1"], "answering a question never approves");
    assert_eq!(answers(&state, &vcx), None, "the question was answered");
    assert!(
        vcx.debug_bounds("approve").is_some(),
        "the approval still waits"
    );
}
