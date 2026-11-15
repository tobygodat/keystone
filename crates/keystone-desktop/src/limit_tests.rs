//! Usage-limit recovery through the app with the scripted Claude, whose account is at
//! its limit while its config home holds a `.fake-usage-limit` file: the card names
//! the limit and its reset, nothing is sent until the user chooses, a scheduled
//! continue goes once after the reset and survives a restart (one missed while closed
//! or asleep waits for a click), sending anything cancels it, and another account
//! continues in a linked session.

use crate::app_state::CONTINUE_TEXT;
use crate::limit::{CONTINUE_GRACE_MS, CONTINUE_MISSED_MS};
use crate::*;
use keystone_agent::ItemStatus;
use std::time::Duration;

/// The scripted providers, each run with this test's own default config home and
/// writing an environment receipt, so no run reads the user's own.
fn fake_programs(dir: &Path) -> (PathBuf, PathBuf) {
    let wrap = |name: &str, var: &str| {
        let binary = keystone_core::test_support::built_binary(&format!("keystone-fake-{name}"));
        let script = dir.join(format!("{name}-limit-wrapper"));
        let root = dir.display();
        keystone_core::test_support::write_executable(
            &script,
            format!(
                "#!/bin/sh\ncase \"${var}\" in '{root}'/*) ;; *) export {var}='{root}/default-{name}' ;; esac\n\
                 KEYSTONE_FAKE_ENV_RECEIPT='{root}/receipt' KEYSTONE_FAKE_CLAUDE_DELAY_MS=1 \
                 KEYSTONE_FAKE_CLAUDE_ECHO=1 KEYSTONE_FAKE_CODEX_DELAY_MS=1 exec '{}' \"$@\"\n",
                binary.display()
            ),
        );
        script
    };
    (
        wrap("codex", "CODEX_HOME"),
        wrap("claude", "CLAUDE_CONFIG_DIR"),
    )
}

/// Puts `home`'s account at its usage limit for `seconds` from now.
fn limit(home: &Path, seconds: u64) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join(".fake-usage-limit"), seconds.to_string()).unwrap();
}

/// Lets `ms` pass at once for the app's wall clock and for the scripted limit in
/// `home`, which counts from its file's modification time.
fn pass(home: &Path, ms: u64) {
    CLOCK_AHEAD_MS.set(CLOCK_AHEAD_MS.get() + ms);
    if let Ok(file) = std::fs::File::options()
        .write(true)
        .open(home.join(".fake-usage-limit"))
    {
        let set = file.metadata().unwrap().modified().unwrap();
        file.set_modified(set - Duration::from_millis(ms)).unwrap();
    }
}

/// [`pass`] while the app runs, with its timers moved on as far.
fn pass_running(vcx: &mut gpui::VisualTestContext, home: &Path, ms: u64) {
    pass(home, ms);
    vcx.executor().advance_clock(Duration::from_millis(ms));
    vcx.run_until_parked();
}

/// Runs the app's timers past `at`, and on for 3 s, while its wall clock stops a second
/// short of it, as a timer that runs ahead of the clock would.
fn wake_a_second_before(vcx: &mut gpui::VisualTestContext, home: &Path, at: u64) {
    let ahead = at - now_ms();
    pass(home, ahead - 1_000);
    vcx.executor().advance_clock(Duration::from_millis(ahead));
    for _ in 0..10 {
        vcx.executor().advance_clock(Duration::from_millis(300));
        vcx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// [`pass_running`] until a second after `reset_ms`, since the scripted limit lifts at
/// a whole second.
fn pass_the_reset(vcx: &mut gpui::VisualTestContext, home: &Path, reset_ms: u64) {
    pass_running(vcx, home, (reset_ms + 1_000).saturating_sub(now_ms()));
}

fn open<'a>(
    path: &Path,
    dir: &Path,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
    let folder = dir.join("project-run-long-crash-hook");
    std::fs::create_dir_all(&folder).unwrap();
    let mut store = Store::open(path).unwrap();
    if store
        .workspace()
        .preferences()
        .window_project(keystone_core::WindowId::FIRST)
        .is_none()
    {
        store
            .open_project(
                keystone_core::WindowId::FIRST,
                &keystone_core::ProjectId::new(&folder).unwrap(),
                None,
                None,
                1,
            )
            .unwrap();
    }
    cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
    });
    cx.executor().allow_parking();
    let programs = fake_programs(dir);
    let (view, cx) = cx.add_window_view(|window, cx| {
        Keystone::new(cx.new(|cx| AppState::new(store, cx)), false, window, cx)
    });
    view.update(cx, |view, cx| {
        view.state
            .update(cx, |state, _| state.test_programs = Some(programs))
    });
    (view, cx)
}

/// Runs the app, with simulated timers moving on, until `done` holds.
fn wait(
    cx: &mut gpui::VisualTestContext,
    view: &Entity<Keystone>,
    what: &str,
    done: impl Fn(&Keystone, &App) -> bool,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |view, cx| done(view, cx)) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            let sessions = view.read_with(cx, |view, cx| {
                let conversations = view.state.read(cx).conversations();
                let mut ids: Vec<_> = conversations.keys().copied().collect();
                ids.sort_unstable();
                ids.into_iter()
                    .map(|id| {
                        let c = &conversations[&id];
                        format!(
                            "\nsession {id}: running {}, error {:?}, approvals {:?}, rows {:?}",
                            c.running, c.error, c.approvals, c.rows
                        )
                    })
                    .collect::<String>()
            });
            panic!("timed out: {what}:{sessions}");
        }
        cx.executor().advance_clock(Duration::from_millis(300));
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn type_and_send(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, text: &str) {
    view.update_in(cx, |view, window, cx| {
        view.composer
            .update(cx, |input, cx| input.set_value(text.into(), cx));
        let draft = view.composer_draft(cx).unwrap();
        view.send_message(draft, window, cx);
    });
}

fn conversation<'a>(view: &'a Keystone, session: u64, cx: &'a App) -> Option<&'a Conversation> {
    view.state.read(cx).conversations().get(&session)
}

fn limited(view: &Keystone, session: u64, cx: &App) -> bool {
    conversation(view, session, cx).is_some_and(|c| c.usage_limit().is_some())
}

/// The user messages the provider took in `session`, oldest first.
fn sent(view: &Keystone, session: u64, cx: &App) -> Vec<String> {
    conversation(view, session, cx).map_or_else(Vec::new, |c| {
        c.rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) if item.kind == ItemKind::UserMessage => Some(item.text.clone()),
                _ => None,
            })
            .collect()
    })
}

fn replied(view: &Keystone, session: u64, text: &str, cx: &App) -> bool {
    conversation(view, session, cx).is_some_and(|c| {
        !c.running
            && c.rows.iter().any(|row| {
                matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage
                    && item.status == ItemStatus::Completed && item.text.contains(text))
            })
    })
}

fn continue_at(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Option<u64> {
    view.read_with(cx, |view, cx| {
        view.state.read(cx).session(1).and_then(|s| s.continue_at())
    })
}

fn click(cx: &mut gpui::VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} is not shown"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
fn the_card_names_the_limit_and_a_scheduled_continue_goes_once_after_the_reset(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let home = dir.path().join("default-claude");
    limit(&home, 6);
    let (view, vcx) = open(&path, dir.path(), cx);
    click(vcx, "welcome-claude");
    type_and_send(&view, vcx, "Fix the login bug");
    wait(vcx, &view, "the limit card", |view, cx| {
        limited(view, 1, cx)
    });

    let shown = view.read_with(vcx, |view, cx| {
        conversation(view, 1, cx).unwrap().usage_limit().cloned()
    });
    let shown = shown.unwrap();
    assert_eq!(shown.label.as_deref(), Some("5-hour"));
    assert!(!shown.inferred);
    let reset_ms = shown.resets_at.unwrap() * 1000 + CONTINUE_GRACE_MS;
    assert!(vcx.debug_bounds("limit").is_some());
    assert!(
        view.read_with(vcx, |view, cx| {
            let c = conversation(view, 1, cx).unwrap();
            !c.shows(c.rows.len() - 1) && c.shows(0)
        }),
        "the card says the limit once: the turn end it stands for is not drawn"
    );
    assert!(
        vcx.debug_bounds("limit-continue").is_none(),
        "Continue waits for the reset"
    );

    // Nothing goes until the user chooses.
    vcx.executor().advance_clock(Duration::from_secs(10));
    vcx.run_until_parked();
    assert_eq!(
        view.read_with(vcx, |view, cx| sent(view, 1, cx)),
        ["Fix the login bug"]
    );

    click(vcx, "limit-schedule");
    assert_eq!(continue_at(&view, vcx), Some(reset_ms));
    click(vcx, "limit-cancel");
    assert_eq!(continue_at(&view, vcx), None, "Cancel unschedules it");
    click(vcx, "limit-schedule");
    assert_eq!(continue_at(&view, vcx), Some(reset_ms));
    // Saved with the session, so a restart keeps it.
    assert_eq!(
        Store::open(&path).unwrap().workspace().sessions()[0].continue_at(),
        Some(reset_ms)
    );

    wake_a_second_before(vcx, &home, reset_ms);
    assert_eq!(
        continue_at(&view, vcx),
        Some(reset_ms),
        "nothing goes a second before the reset and its grace"
    );
    pass_running(vcx, &home, 1_000);
    wait(vcx, &view, "the continue after the reset", |view, cx| {
        replied(view, 1, "Continue where you left off", cx)
    });
    assert_eq!(
        view.read_with(vcx, |view, cx| sent(view, 1, cx)),
        ["Fix the login bug", CONTINUE_TEXT]
    );
    assert!(now_ms() >= reset_ms, "never before the reset and its grace");
    assert_eq!(continue_at(&view, vcx), None);
    assert!(vcx.debug_bounds("limit").is_none());
    vcx.executor().advance_clock(Duration::from_secs(10));
    vcx.run_until_parked();
    assert_eq!(
        view.read_with(vcx, |view, cx| sent(view, 1, cx)).len(),
        2,
        "it goes once"
    );
}

#[gpui::test]
fn sending_cancels_a_scheduled_continue_and_one_survives_a_restart(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let home = dir.path().join("default-claude");
    limit(&home, 30);
    let (view, vcx) = open(&path, dir.path(), cx);
    click(vcx, "welcome-claude");
    type_and_send(&view, vcx, "Fix the login bug");
    wait(vcx, &view, "the limit card", |view, cx| {
        limited(view, 1, cx)
    });
    click(vcx, "limit-schedule");
    let at = continue_at(&view, vcx).unwrap();

    // A message the user sends replaces the continue they scheduled.
    std::fs::remove_file(home.join(".fake-usage-limit")).unwrap();
    type_and_send(&view, vcx, "Never mind");
    wait(vcx, &view, "the user's message", |view, cx| {
        replied(view, 1, "Never mind", cx)
    });
    assert_eq!(continue_at(&view, vcx), None);
    assert!(vcx.debug_bounds("limit").is_none());

    // Limited again and scheduled, then Keystone quits before the reset.
    limit(&home, 6);
    type_and_send(&view, vcx, "One more thing");
    wait(vcx, &view, "the second limit", |view, cx| {
        limited(view, 1, cx)
    });
    click(vcx, "limit-schedule");
    let at = {
        let again = continue_at(&view, vcx).unwrap();
        assert!(again < at);
        again
    };
    view.update(vcx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state.save(Vec::new(), Vec::new(), Vec::new(), cx).unwrap();
            state.teardown(cx);
        })
    });

    // The next run waits for the same continue and sends it once due.
    let state = vcx.update(|_, cx| cx.new(|cx| AppState::new(Store::open(&path).unwrap(), cx)));
    let programs = fake_programs(dir.path());
    state.update(vcx, |state, _| state.test_programs = Some(programs));
    assert_eq!(
        state.read_with(vcx, |state, _| state
            .session(1)
            .and_then(|s| s.continue_at())),
        Some(at)
    );
    wake_a_second_before(vcx, &home, at);
    assert_eq!(
        state.read_with(vcx, |state, _| state
            .session(1)
            .and_then(|s| s.continue_at())),
        Some(at),
        "it waits until it is due"
    );
    pass_running(vcx, &home, 1_000);
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let sent_after = loop {
        vcx.run_until_parked();
        let texts: Option<Vec<String>> = state.read_with(vcx, |state, _| {
            let conversation = state.conversations().get(&1)?;
            let done = !conversation.running
                && conversation.rows.iter().any(|row| {
                    matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage
                        && item.status == ItemStatus::Completed
                        && item.text.contains("Continue where you left off"))
                });
            done.then(|| {
                conversation
                    .rows
                    .iter()
                    .filter_map(|row| match row {
                        Row::Item(item) if item.kind == ItemKind::UserMessage => {
                            Some(item.text.clone())
                        }
                        _ => None,
                    })
                    .collect()
            })
        });
        if let Some(texts) = texts {
            break texts;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no continue after the restart"
        );
        vcx.executor().advance_clock(Duration::from_millis(300));
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        sent_after,
        [
            "Fix the login bug",
            "Never mind",
            "One more thing",
            CONTINUE_TEXT
        ]
    );
    assert!(now_ms() >= at);
    assert_eq!(
        state.read_with(vcx, |state, _| state
            .session(1)
            .and_then(|s| s.continue_at())),
        None
    );
}

#[gpui::test]
fn another_account_continues_in_a_linked_session_on_that_account(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    limit(&dir.path().join("default-claude"), 600);
    let work_home = dir.path().join("work-claude");
    std::fs::create_dir_all(&work_home).unwrap();
    let work = Store::open(&path)
        .unwrap()
        .add_account(Provider::Claude, "Work", work_home.clone(), &[])
        .unwrap()
        .value;
    let (view, vcx) = open(&path, dir.path(), cx);
    click(vcx, "welcome-claude");
    type_and_send(&view, vcx, "Fix the login bug");
    wait(vcx, &view, "the limit card", |view, cx| {
        limited(view, 1, cx)
    });

    let (_, account, name) = view
        .read_with(vcx, |view, cx| view.limit_target(1, cx))
        .unwrap();
    assert_eq!(account.as_ref(), Some(&work));
    assert_eq!(name, "Claude Code · Work");
    click(vcx, "limit-schedule");
    assert!(continue_at(&view, vcx).is_some());
    click(vcx, "limit-switch");
    assert_eq!(
        continue_at(&view, vcx),
        None,
        "continuing elsewhere cancels the source's scheduled continue"
    );
    let new = view.read_with(vcx, |view, cx| {
        let state = view.state.read(cx);
        let new = state.session(2).unwrap();
        assert_eq!(new.handed_off_from(), Some(1));
        assert_eq!(new.account(), Some(&work));
        assert!(
            new.draft()
                .starts_with("Continuing from Keystone session 1")
        );
        assert!(new.draft().contains("Fix the login bug"));
        assert_eq!(view.selected(cx), Some(2));
        new.id()
    });
    assert_eq!(
        view.read_with(vcx, |view, cx| sent(view, 1, cx)),
        ["Fix the login bug"],
        "the source is unchanged"
    );

    let draft = view.read_with(vcx, |view, cx| view.composer.read(cx).value().to_string());
    type_and_send(&view, vcx, &draft);
    wait(vcx, &view, "the reply on the other account", |view, cx| {
        replied(view, new, "Continuing from Keystone session 1", cx)
    });
    let receipt = std::fs::read_to_string(dir.path().join("receipt")).unwrap();
    let last: serde_json::Value = serde_json::from_str(receipt.lines().last().unwrap()).unwrap();
    assert_eq!(
        last["CLAUDE_CONFIG_DIR"].as_str(),
        Some(work_home.to_str().unwrap()),
        "{receipt}"
    );
}

#[gpui::test]
fn continue_sends_once_and_never_into_a_running_turn(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let home = dir.path().join("default-claude");
    // Long enough that a loaded machine still starts the turn inside the limit.
    limit(&home, 5);
    let (view, vcx) = open(&path, dir.path(), cx);
    click(vcx, "welcome-claude");
    type_and_send(&view, vcx, "Fix the login bug");
    wait(vcx, &view, "the limit card", |view, cx| {
        limited(view, 1, cx)
    });
    let reset_ms = view.read_with(vcx, |view, cx| {
        conversation(view, 1, cx)
            .unwrap()
            .usage_limit()
            .unwrap()
            .resets_at
            .unwrap()
            * 1000
    });
    pass_the_reset(vcx, &home, reset_ms);

    // Pressed twice before anything is drawn again: only the first goes.
    let results = view.update(vcx, |view, cx| {
        view.state.update(cx, |state, cx| {
            (
                state.continue_after_limit(1, cx),
                state.continue_after_limit(1, cx),
            )
        })
    });
    assert_eq!(results, (true, false));
    wait(vcx, &view, "the continue", |view, cx| {
        replied(view, 1, "Continue where you left off", cx)
    });
    assert_eq!(
        view.read_with(vcx, |view, cx| sent(view, 1, cx)),
        ["Fix the login bug", CONTINUE_TEXT]
    );

    // Limited again and scheduled; a turn that starts before the reset cancels it.
    limit(&home, 600);
    type_and_send(&view, vcx, "One more thing");
    wait(vcx, &view, "the second limit", |view, cx| {
        limited(view, 1, cx)
    });
    click(vcx, "limit-schedule");
    assert!(continue_at(&view, vcx).is_some());
    std::fs::remove_file(home.join(".fake-usage-limit")).unwrap();
    view.update(vcx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state.receive(Vec::new(), vec![(1, Event::TurnStarted)], cx);
        })
    });
    assert_eq!(continue_at(&view, vcx), None, "a started turn cancels it");
    assert!(view.update(vcx, |view, cx| {
        !view
            .state
            .update(cx, |state, cx| state.continue_after_limit(1, cx))
    }));
    vcx.executor().advance_clock(Duration::from_secs(10));
    vcx.run_until_parked();
    assert_eq!(
        view.read_with(vcx, |view, cx| sent(view, 1, cx)),
        ["Fix the login bug", CONTINUE_TEXT, "One more thing"],
        "nothing goes into a running turn"
    );
}

#[gpui::test]
fn a_codex_limit_takes_its_reset_from_the_usage_reading_and_any_hand_off_cancels_the_schedule(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    limit(&dir.path().join("default-codex"), 600);
    let (view, vcx) = open(&path, dir.path(), cx);
    click(vcx, "welcome-codex");
    type_and_send(&view, vcx, "Fix the login bug");
    wait(vcx, &view, "the Codex limit card", |view, cx| {
        limited(view, 1, cx)
    });
    let shown = view.read_with(vcx, |view, cx| {
        conversation(view, 1, cx).unwrap().usage_limit().cloned()
    });
    assert_eq!(
        shown.unwrap().resets_at,
        None,
        "Codex's error names no reset"
    );
    assert!(
        view.read_with(vcx, |view, cx| {
            let c = conversation(view, 1, cx).unwrap();
            c.shows(c.rows.len() - 1)
        }),
        "the card cannot say all the error said, so its words stay above it"
    );
    assert!(
        vcx.debug_bounds("limit-continue").is_none(),
        "the usage reading's reset is still ahead"
    );
    click(vcx, "limit-schedule");
    let at = continue_at(&view, vcx).unwrap();
    assert!(at > now_ms() + 60_000, "scheduled for the reading's reset");

    view.update_in(vcx, |view, window, cx| view.hand_off(window, cx));
    vcx.run_until_parked();
    assert_eq!(
        view.read_with(vcx, |view, cx| view
            .state
            .read(cx)
            .session(2)
            .unwrap()
            .handed_off_from()),
        Some(1)
    );
    assert_eq!(
        continue_at(&view, vcx),
        None,
        "the plain Hand off cancels the source's scheduled continue too"
    );
}

#[gpui::test]
fn a_continue_that_came_due_while_closed_waits_for_a_click(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let home = dir.path().join("default-claude");
    limit(&home, 6);
    let reset_ms = {
        let (view, vcx) = open(&path, dir.path(), cx);
        click(vcx, "welcome-claude");
        type_and_send(&view, vcx, "Fix the login bug");
        wait(vcx, &view, "the limit card", |view, cx| {
            limited(view, 1, cx)
        });
        click(vcx, "limit-schedule");
        assert!(continue_at(&view, vcx).is_some());
        let reset_ms = view.read_with(vcx, |view, cx| {
            conversation(view, 1, cx)
                .unwrap()
                .usage_limit()
                .unwrap()
                .resets_at
                .unwrap()
                * 1000
        });
        view.update(vcx, |view, cx| {
            view.state.update(cx, |state, cx| {
                state.save(Vec::new(), Vec::new(), Vec::new(), cx).unwrap();
                state.teardown(cx);
            })
        });
        reset_ms
    };

    // Keystone stays closed past the reset and the scheduled time.
    pass(&home, (reset_ms + 1_000).saturating_sub(now_ms()));
    Store::open(&path)
        .unwrap()
        .set_continue_at(1, Some(now_ms() - 1_000))
        .unwrap();

    let (view, vcx) = open(&path, dir.path(), cx);
    view.update(vcx, |view, cx| view.select(1, cx));
    wait(vcx, &view, "the restored limit card", |view, cx| {
        limited(view, 1, cx)
    });
    for _ in 0..10 {
        vcx.executor().advance_clock(Duration::from_millis(300));
        vcx.run_until_parked();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        view.read_with(vcx, |view, cx| sent(view, 1, cx)),
        ["Fix the login bug"],
        "nothing sent at launch"
    );
    assert_eq!(continue_at(&view, vcx), None);
    assert_eq!(
        Store::open(&path).unwrap().workspace().sessions()[0].continue_at(),
        None,
        "the missed schedule is cleared on disk"
    );
    assert!(
        vcx.debug_bounds("limit-cancel").is_none(),
        "no schedule left to cancel"
    );

    click(vcx, "limit-continue");
    wait(vcx, &view, "the continue after the click", |view, cx| {
        replied(view, 1, "Continue where you left off", cx)
    });
    assert_eq!(
        view.read_with(vcx, |view, cx| sent(view, 1, cx)),
        ["Fix the login bug", CONTINUE_TEXT]
    );
}

#[gpui::test]
fn a_continue_found_overdue_on_wake_waits_for_a_click(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let home = dir.path().join("default-claude");
    limit(&home, 4);
    let (view, vcx) = open(&path, dir.path(), cx);
    click(vcx, "welcome-claude");
    type_and_send(&view, vcx, "Fix the login bug");
    wait(vcx, &view, "the limit card", |view, cx| {
        limited(view, 1, cx)
    });
    let reset_ms = view.read_with(vcx, |view, cx| {
        conversation(view, 1, cx)
            .unwrap()
            .usage_limit()
            .unwrap()
            .resets_at
            .unwrap()
            * 1000
    });
    pass_the_reset(vcx, &home, reset_ms);
    let schedule = |vcx: &mut gpui::VisualTestContext, at: u64| {
        view.update(vcx, |view, cx| {
            view.state
                .update(cx, |state, cx| state.schedule_continue(1, Some(at), cx))
        });
    };

    // The timer wakes, as after the computer slept, well past the scheduled time.
    schedule(vcx, now_ms() - CONTINUE_MISSED_MS - 60_000);
    for _ in 0..10 {
        vcx.executor().advance_clock(Duration::from_millis(300));
        vcx.run_until_parked();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        view.read_with(vcx, |view, cx| sent(view, 1, cx)),
        ["Fix the login bug"],
        "nothing sent on wake"
    );
    assert_eq!(continue_at(&view, vcx), None);
    assert_eq!(
        Store::open(&path).unwrap().workspace().sessions()[0].continue_at(),
        None,
        "the missed schedule is cleared on disk"
    );
    assert!(vcx.debug_bounds("limit-cancel").is_none());
    assert!(vcx.debug_bounds("limit-continue").is_some());

    // Found late but within the grace, it still goes by itself.
    schedule(vcx, now_ms() - CONTINUE_MISSED_MS / 2);
    wait(vcx, &view, "the continue within the grace", |view, cx| {
        replied(view, 1, "Continue where you left off", cx)
    });
    assert_eq!(
        view.read_with(vcx, |view, cx| sent(view, 1, cx)),
        ["Fix the login bug", CONTINUE_TEXT]
    );
}
