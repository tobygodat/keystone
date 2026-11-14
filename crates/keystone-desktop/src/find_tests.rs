// Included into main.rs's test module, whose helpers these tests use.

/// A workspace whose session 1 has `turns` turns of a question, a command and a reply,
/// rows `3t`, `3t + 1` and `3t + 2`. The reply of each turn in `needles` mentions the
/// needle, and so does the command output of each turn in `outputs`.
fn find_workspace(dir: &Path, turns: usize, needles: &[usize], outputs: &[usize]) -> PathBuf {
    use keystone_agent::log::{Entry, SessionLog};
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    store
        .create(Provider::Codex, &working_directory(), 1)
        .unwrap();
    drop(store);
    let mut log = SessionLog::open(&dir.join("sessions"), 1).unwrap();
    for turn in 0..turns {
        let item = |id: &str, kind: ItemKind, text: String| Entry::Item {
            item: Item {
                id: format!("{id}-{turn}"),
                kind,
                text,
                status: ItemStatus::Completed,
            },
            seq: None,
            run: None,
        };
        log.append(&Entry::TurnStarted).unwrap();
        log.append(&item("u", ItemKind::UserMessage, format!("Question {turn}")))
            .unwrap();
        let output = if outputs.contains(&turn) {
            "running 12 tests\nfound the Needle in the output".to_owned()
        } else {
            "running 12 tests\ntest result: ok".to_owned()
        };
        log.append(&item(
            "c",
            ItemKind::Command {
                command: "cargo test".into(),
                exit_code: Some(0),
            },
            output,
        ))
        .unwrap();
        let reply = if needles.contains(&turn) {
            format!("Reply {turn}. Here is the **needle** you asked for.")
        } else {
            format!("Reply {turn}. ").repeat(20)
        };
        log.append(&item("a", ItemKind::AgentMessage, reply)).unwrap();
        log.append(&Entry::TurnEnded {
            status: TurnStatus::Completed,
            error: None,
            limit: None,
            seq: None,
            run: None,
        })
        .unwrap();
    }
    path
}

fn open_find_window<'a>(
    path: &Path,
    rows: usize,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
    let (view, cx) = open_window(path, cx);
    wait_until(cx, &view, move |view, cx| {
        view.state
            .read(cx)
            .conversations()
            .get(&1)
            .is_some_and(|c| !c.is_loading() && c.rows.len() == rows)
    });
    view.update_in(cx, |this, window, cx| this.focus_composer(window, cx));
    cx.run_until_parked();
    (view, cx)
}

/// Waits for the bar to show `expected`.
fn shows(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, expected: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        let shown = view.read_with(cx, |v, cx| {
            v.finds.bars.get(&MAIN_1).and_then(|bar| bar.count(cx))
        });
        if shown.as_deref() == Some(expected) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the bar shows {shown:?}, not {expected:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn current_row(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> usize {
    view.read_with(cx, |v, _| {
        let bar = &v.finds.bars[&MAIN_1];
        bar.matches[bar.current.unwrap()].row
    })
}

fn row_drawn(cx: &mut gpui::VisualTestContext, row: usize) -> bool {
    cx.debug_bounds(format!("transcript-row-{row}").leak())
        .is_some()
}

#[gpui::test]
fn find_counts_every_match_in_the_session_and_steps_through_them_wrapping(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = find_workspace(dir.path(), 200, &[2, 100, 150], &[]);
    let (view, cx) = open_find_window(&path, 600, cx);
    assert!(!row_drawn(cx, 302), "the reader starts at the latest rows");

    cx.simulate_keystrokes("secondary-f");
    assert!(cx.debug_bounds("find-bar").is_some(), "Ctrl+F opens the bar");
    cx.simulate_input("NEEDLE");
    shows(&view, cx, "1 of 3");
    assert_eq!(current_row(&view, cx), 8);
    assert!(row_drawn(cx, 8), "the first match scrolls into view");

    cx.simulate_keystrokes("enter");
    shows(&view, cx, "2 of 3");
    cx.run_until_parked();
    assert!(row_drawn(cx, 302), "Enter shows the next match, rows away");
    click(cx, "find-next");
    shows(&view, cx, "3 of 3");
    cx.simulate_keystrokes("enter");
    shows(&view, cx, "1 of 3");
    cx.simulate_keystrokes("shift-enter");
    shows(&view, cx, "3 of 3");
    click(cx, "find-previous");
    shows(&view, cx, "2 of 3");

    cx.simulate_input("zzz");
    shows(&view, cx, "No matches");

    cx.simulate_keystrokes("escape");
    assert!(cx.debug_bounds("find-bar").is_none(), "Escape closes the bar");
    view.update_in(cx, |this, window, cx| {
        assert!(
            this.composer.focus_handle(cx).is_focused(window),
            "focus goes back to the composer"
        )
    });
}

#[gpui::test]
fn find_from_the_transcript_gives_focus_back_to_it(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = find_workspace(dir.path(), 4, &[1], &[]);
    let (view, cx) = open_find_window(&path, 12, cx);
    cx.simulate_keystrokes("secondary-shift-t");
    cx.run_until_parked();
    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input("needle");
    shows(&view, cx, "1 of 1");
    click(cx, "find-close");
    assert!(cx.debug_bounds("find-bar").is_none(), "the close button closes the bar");
    view.update_in(cx, |this, window, cx| {
        let views = &mut this.presentation.current.views;
        assert!(views.focus(MAIN_1, cx).is_focused(window), "focus is back on the transcript");
    });
}

#[gpui::test]
fn a_match_in_a_collapsed_row_opens_the_row(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = find_workspace(dir.path(), 3, &[], &[1]);
    let (view, cx) = open_find_window(&path, 9, cx);
    click(cx, "activity-expanded-4");
    assert!(cx.debug_bounds("activity-body-4").is_none(), "the output is collapsed");

    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input("needle in the");
    shows(&view, cx, "1 of 1");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("activity-body-4").is_some(),
        "finding text in the output opens it"
    );
}

#[gpui::test]
fn a_match_arriving_in_a_streaming_reply_is_counted_without_moving_the_reader(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = find_workspace(dir.path(), 200, &[2, 100], &[]);
    let (view, cx) = open_find_window(&path, 600, cx);
    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input("needle");
    shows(&view, cx, "1 of 2");
    cx.simulate_keystrokes("enter");
    shows(&view, cx, "2 of 2");
    cx.run_until_parked();
    let top = view.read_with(cx, |v, _| {
        v.presentation
            .current
            .views
            .get(MAIN_1)
            .unwrap()
            .logical_scroll_top()
    });

    view.update(cx, |this, cx| {
        this.state.update(cx, |state, _| {
            let conversation = state.test_conversation(1);
            conversation.apply(Event::TurnStarted);
            conversation.apply(Event::ItemStarted(Item {
                id: "streaming".into(),
                kind: ItemKind::AgentMessage,
                text: "Looking for it".into(),
                status: ItemStatus::InProgress,
            }));
            conversation.apply(Event::ItemDelta {
                item_id: "streaming".into(),
                text: ", and here is another needle.".into(),
            });
        });
        this.commit(1, cx);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(std::time::Duration::from_millis(200));
    shows(&view, cx, "2 of 3");
    let after = view.read_with(cx, |v, _| {
        v.presentation
            .current
            .views
            .get(MAIN_1)
            .unwrap()
            .logical_scroll_top()
    });
    assert_eq!(
        (after.item_ix, after.offset_in_item),
        (top.item_ix, top.offset_in_item),
        "the reader stays where they were"
    );
    assert_eq!(current_row(&view, cx), 302, "the current match stays");
}

#[gpui::test]
fn typing_more_of_a_query_reads_no_row_again(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = find_workspace(dir.path(), 1700, &[5, 1600], &[]);
    let (view, cx) = open_find_window(&path, 5100, cx);
    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input("nee");
    shows(&view, cx, "1 of 2");
    let first = view.read_with(cx, |v, _| v.finds.bars[&MAIN_1].rows_read);
    assert_eq!(first, 5100, "the first search reads every row once");
    cx.simulate_input("dle");
    shows(&view, cx, "1 of 2");
    let second = view.read_with(cx, |v, _| v.finds.bars[&MAIN_1].rows_read);
    assert_eq!(second, 0, "a longer query searches the texts already read");
}

#[gpui::test]
fn enter_waits_for_a_new_query_and_the_count_keeps_the_field_still(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let needles: Vec<usize> = (0..12).map(|n| 10 + n * 15).collect();
    let path = find_workspace(dir.path(), 200, &needles, &[]);
    let (view, cx) = open_find_window(&path, 600, cx);
    cx.simulate_keystrokes("secondary-f");
    let empty = cx.debug_bounds("find-field").unwrap();
    cx.simulate_input("needle");
    shows(&view, cx, "1 of 12");
    let one = cx.debug_bounds("find-field").unwrap();
    for _ in 0..9 {
        cx.simulate_keystrokes("enter");
    }
    shows(&view, cx, "10 of 12");
    let ten = cx.debug_bounds("find-field").unwrap();
    assert_eq!(
        (empty.size.width, one.size.width),
        (ten.size.width, ten.size.width),
        "the count's slot keeps one width"
    );

    let row = current_row(&view, cx);
    view.update(cx, |this, cx| {
        let input = this.finds.bars[&MAIN_1].input.clone();
        input.update(cx, |input, cx| input.set_value("needle you".to_owned(), cx));
        this.search_find(MAIN_1, crate::find::Reason::Typed, cx);
        this.step_find(MAIN_1, false, cx);
        let bar = &this.finds.bars[&MAIN_1];
        assert_eq!(bar.count(cx), None, "the old count goes as the query changes");
        assert_eq!(bar.matches[bar.current.unwrap()].row, row, "Enter waits");
    });
    // The new query lands on the first match from the top of the view.
    shows(&view, cx, "10 of 12");
}

#[gpui::test]
fn an_edit_row_is_searched_by_its_paths_and_preview_lines(cx: &mut gpui::TestAppContext) {
    use keystone_agent::diff::{FileDiff, Hunk, Patch};
    use keystone_agent::log::{Entry, SessionLog};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    store.create(Provider::Codex, &working_directory(), 1).unwrap();
    drop(store);
    let mut log = SessionLog::open(&dir.path().join("sessions"), 1).unwrap();
    let item = |id: &str, kind: ItemKind, text: &str| Entry::Item {
        item: Item {
            id: id.into(),
            kind,
            text: text.into(),
            status: ItemStatus::Completed,
        },
        seq: None,
        run: None,
    };
    let lines = ["+fn first() {}", "+// the giraffe line", "+fn last() {}"]
        .map(String::from)
        .to_vec();
    let patch = Patch::edit(vec![FileDiff {
        path: "src/zebra_edit.rs".into(),
        hunks: vec![Hunk::new(1, 1, lines).unwrap()],
        ..FileDiff::default()
    }]);
    log.append(&Entry::TurnStarted).unwrap();
    log.append(&item("u", ItemKind::UserMessage, "Add the functions")).unwrap();
    log.append(&item(
        "edit",
        ItemKind::FileChange {
            paths: vec!["src/zebra_edit.rs".into()],
            patch: Some(patch),
        },
        "",
    ))
    .unwrap();
    log.append(&item("a", ItemKind::AgentMessage, "Done.")).unwrap();
    log.append(&Entry::TurnEnded {
        status: TurnStatus::Completed,
        error: None,
        limit: None,
        seq: None,
        run: None,
    })
    .unwrap();
    let (view, cx) = open_find_window(&path, 3, cx);
    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input("zebra_edit");
    shows(&view, cx, "1 of 1");
    assert_eq!(current_row(&view, cx), 1, "the edit's label holds its path");

    view.update_in(cx, |this, window, cx| {
        let input = this.finds.bars[&MAIN_1].input.clone();
        input.update(cx, |input, cx| input.select_all(&keystone_ui::text_input::SelectAll, window, cx));
    });
    cx.simulate_input("GIRAFFE");
    shows(&view, cx, "1 of 1");
    cx.run_until_parked();
    let marks = view.read_with(cx, |v, _| v.finds.bars[&MAIN_1].marks(1));
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].text, 2, "the label, then the preview's lines");
    assert!(
        cx.debug_bounds("edit-1-line-1").is_some(),
        "finding a line of the preview opens the edit"
    );
}

/// A workspace of sessions 1 to 3, titled Alpha, Needle notes and Gamma, with Gamma
/// settled. Each session's messages are `messages[n]`, oldest first; session 1 also has
/// `filler` turns before its messages, so it is long enough to scroll.
fn search_workspace(dir: &Path, messages: [&[&str]; 3], filler: usize) -> PathBuf {
    use keystone_agent::log::{Entry, SessionLog};
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    for (id, title) in [(1, "Alpha"), (2, "Needle notes"), (3, "Gamma")] {
        store
            .create(Provider::Codex, &working_directory(), id)
            .unwrap();
        store.rename(id, title).unwrap();
    }
    store.set_settled(3, true).unwrap();
    store.select(2).unwrap();
    store.flush().unwrap();
    drop(store);
    for (n, texts) in messages.iter().enumerate() {
        let session = n as u64 + 1;
        let mut log = SessionLog::open(&dir.join("sessions"), session).unwrap();
        let fill = if session == 1 { filler } else { 0 };
        let lines: Vec<String> = (0..fill)
            .map(|turn| format!("Filler reply {turn}. ").repeat(20))
            .chain(texts.iter().map(|text| text.to_string()))
            .collect();
        for (turn, text) in lines.iter().enumerate() {
            log.append(&Entry::TurnStarted).unwrap();
            log.append(&Entry::Item {
                item: Item {
                    id: format!("m{session}-{turn}"),
                    kind: if turn % 2 == 0 {
                        ItemKind::AgentMessage
                    } else {
                        ItemKind::UserMessage
                    },
                    text: text.clone(),
                    status: ItemStatus::Completed,
                },
                seq: None,
                run: None,
            })
            .unwrap();
            log.append(&Entry::TurnEnded {
                status: TurnStatus::Completed,
                error: None,
                limit: None,
                seq: None,
                run: None,
            })
            .unwrap();
        }
    }
    path
}

/// Waits for `done`, letting the test clock run, as find's settle timer needs.
fn wait_with_clock(
    cx: &mut gpui::VisualTestContext,
    view: &Entity<Keystone>,
    done: impl Fn(&Keystone, &App) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |view, cx| done(view, cx)) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out");
        cx.executor().advance_clock(std::time::Duration::from_millis(100));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn type_search(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, text: &str) {
    view.update_in(cx, |this, window, cx| this.focus_composer(window, cx));
    cx.simulate_keystrokes("secondary-shift-f");
    cx.simulate_input(text);
    let text = text.to_owned();
    wait_until(cx, view, move |v, _| {
        v.messages.searched.as_deref() == Some(text.as_str())
    });
}

#[gpui::test]
fn the_sidebar_search_finds_messages_under_the_title_matches(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = search_workspace(
        dir.path(),
        [
            &["Nothing yet", "where is the needle?"],
            &["the needle again"],
            &["a settled needle", "unrelated"],
        ],
        0,
    );
    let (view, cx) = open_window(&path, cx);
    cx.run_until_parked();
    assert!(cx.debug_bounds("message-results").is_none(), "no query, no results");

    type_search(&view, cx, "NEEDLE");
    assert!(cx.debug_bounds("session-2").is_some(), "the title match shows");
    assert!(
        cx.debug_bounds("message-result-2").is_none(),
        "a title match is not listed again"
    );
    // Session 3's log was written last, so its match is the newest.
    let (older, newest) = (
        cx.debug_bounds("message-result-1").expect("a message match shows"),
        cx.debug_bounds("message-result-3")
            .expect("a settled session's message shows"),
    );
    assert!(
        newest.top() > cx.debug_bounds("session-2").unwrap().top(),
        "messages come under the title matches"
    );
    assert!(older.top() > newest.top(), "the newest match comes first");
    let hits = view.read_with(cx, |v, cx| v.message_rows(cx));
    assert_eq!(hits[1].snippet.text, "where is the needle?");
    assert_eq!(&hits[1].snippet.text[hits[1].snippet.marked.clone()], "needle");

    cx.simulate_keystrokes("secondary-a backspace");
    cx.run_until_parked();
    assert!(cx.debug_bounds("message-results").is_none(), "an empty query is the plain list");
}

#[gpui::test]
fn opening_a_message_result_lands_on_the_match_with_find_open(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = search_workspace(
        dir.path(),
        [&["the haystack holds a needle", "later words"], &[], &[]],
        150,
    );
    let (view, cx) = open_window(&path, cx);
    cx.run_until_parked();
    type_search(&view, cx, "needle");
    cx.simulate_keystrokes("down");
    view.read_with(cx, |v, _| assert_eq!(v.messages.highlighted, Some(0)));
    cx.simulate_keystrokes("enter");
    wait_with_clock(cx, &view, |v, cx| {
        v.selected(cx) == Some(1)
            && v.finds.bars.get(&MAIN_1).is_some_and(|bar| {
                bar.count(cx).as_deref() == Some("1 of 1")
            })
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("transcript-row-150").is_some(),
        "the session opens scrolled to the message"
    );
    view.update_in(cx, |this, window, cx| {
        let bar = &this.finds.bars[&MAIN_1];
        assert_eq!(bar.input.read(cx).value(), "needle", "the bar holds the same words");
        assert!(bar.input.focus_handle(cx).is_focused(window));
    });
}

#[gpui::test]
fn clicking_a_message_result_opens_its_session(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = search_workspace(dir.path(), [&[], &[], &["a settled needle"]], 0);
    let (view, cx) = open_window(&path, cx);
    cx.run_until_parked();
    type_search(&view, cx, "settled needle");
    click(cx, "message-result-3");
    let key = ViewKey {
        view: ViewId::MAIN,
        session: 3,
    };
    wait_with_clock(cx, &view, move |v, cx| {
        v.selected(cx) == Some(3)
            && v.finds
                .bars
                .get(&key)
                .is_some_and(|bar| bar.count(cx).as_deref() == Some("1 of 1"))
    });
}

#[gpui::test]
fn the_sidebar_finds_a_reply_by_the_text_it_draws(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = search_workspace(
        dir.path(),
        [&["See [the docs](https://zebra.example/guide) for the **sharp needle**."], &[], &[]],
        0,
    );
    let (view, cx) = open_window(&path, cx);
    cx.run_until_parked();
    type_search(&view, cx, "zebra");
    assert!(
        cx.debug_bounds("message-result-1").is_none(),
        "a link's address is not what the reply draws"
    );
    cx.simulate_keystrokes("secondary-a");
    cx.simulate_input("docs for the sharp");
    let typed = "docs for the sharp".to_owned();
    wait_until(cx, &view, move |v, _| v.messages.searched.as_deref() == Some(typed.as_str()));
    let hits = view.read_with(cx, |v, cx| v.message_rows(cx));
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].snippet.text, "See the docs for the sharp needle.");
}

#[gpui::test]
fn enter_during_a_scan_opens_the_new_query_s_first_result(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = search_workspace(
        dir.path(),
        [&["the haystack holds it"], &[], &["a settled needle"]],
        0,
    );
    let (view, cx) = open_window(&path, cx);
    cx.run_until_parked();
    type_search(&view, cx, "needle");
    assert_eq!(view.read_with(cx, |v, cx| v.message_rows(cx).len()), 1);

    view.update(cx, |this, cx| {
        this.search
            .update(cx, |input, cx| input.set_value("haystack".to_owned(), cx));
        this.search_messages(cx);
    });
    assert!(
        view.read_with(cx, |v, cx| v.message_rows(cx).is_empty()),
        "the old query's results open nothing while the new scan runs"
    );
    cx.simulate_keystrokes("enter");
    wait_with_clock(cx, &view, |v, cx| {
        v.selected(cx) == Some(1)
            && v.finds.bars.get(&MAIN_1).is_some_and(|bar| {
                bar.input.read(cx).value() == "haystack"
                    && bar.count(cx).as_deref() == Some("1 of 1")
            })
    });
}

/// Presses Enter while the sidebar's scan for "notes" runs, then `moves_on` before the
/// scan lands. Session 2's title holds "notes" and session 1's message does, so Enter
/// alone would open session 1.
fn enter_then_move_on(
    cx: &mut gpui::TestAppContext,
    moves_on: impl FnOnce(&mut gpui::VisualTestContext),
) -> (tempfile::TempDir, Entity<Keystone>, &mut gpui::VisualTestContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = search_workspace(dir.path(), [&["the meeting notes are here"], &[], &[]], 0);
    let (view, cx) = open_window(&path, cx);
    cx.run_until_parked();
    type_search(&view, cx, "note");
    view.update(cx, |this, cx| {
        this.search
            .update(cx, |input, cx| input.set_value("notes".to_owned(), cx));
        this.search_messages(cx);
    });
    cx.update(|window, cx| {
        window.dispatch_keystroke(gpui::Keystroke::parse("enter").unwrap(), cx)
    });
    moves_on(cx);
    wait_until(cx, &view, |v, _| v.messages.searched.as_deref() == Some("notes"));
    cx.run_until_parked();
    (dir, view, cx)
}

#[gpui::test]
fn ctrl_n_after_enter_mid_scan_keeps_the_new_session(cx: &mut gpui::TestAppContext) {
    let (_dir, view, cx) = enter_then_move_on(cx, |cx| cx.simulate_keystrokes("secondary-n"));
    view.read_with(cx, |v, cx| {
        let selected = v.selected(cx);
        assert!(
            selected.is_some_and(|s| s > 3),
            "the new session stays selected, not the old result: {selected:?}"
        );
        assert!(v.finds.bars.is_empty(), "no find bar opens behind it");
    });
}

#[gpui::test]
fn a_click_after_enter_mid_scan_opens_nothing_old(cx: &mut gpui::TestAppContext) {
    let (_dir, view, cx) = enter_then_move_on(cx, |cx| click(cx, "session-2"));
    view.read_with(cx, |v, cx| {
        assert_eq!(v.selected(cx), Some(2), "the clicked session stays");
        assert!(v.finds.bars.is_empty());
    });
}

#[gpui::test]
fn escape_after_enter_mid_scan_opens_nothing(cx: &mut gpui::TestAppContext) {
    let (_dir, view, cx) = enter_then_move_on(cx, |cx| cx.simulate_keystrokes("escape"));
    view.read_with(cx, |v, cx| {
        assert_eq!(v.selected(cx), Some(2), "the selection is where it was");
        assert!(v.finds.bars.is_empty());
    });
}

#[gpui::test]
fn the_sidebar_finds_a_delivered_result_by_the_reply_it_draws(cx: &mut gpui::TestAppContext) {
    use crate::app_state::delivery::{Outcome, message};
    use keystone_agent::log::{Entry, SessionLog};
    use keystone_core::DeliveryState;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    store.create(Provider::Claude, &working_directory(), 1).unwrap();
    store.rename(1, "Alpha").unwrap();
    store
        .create_unselected(Provider::Codex, &working_directory(), 2, 1)
        .unwrap();
    store.rename(2, "Beta").unwrap();
    let delivered = message(
        2,
        "Beta",
        &Outcome::Finished,
        Some("The **zebra** answer, see [docs](https://wrapper.example)."),
    );
    store
        .set_delivery(
            2,
            DeliveryState::Delivered {
                message: delivered.clone(),
            },
        )
        .unwrap();
    store.select(1).unwrap();
    store.flush().unwrap();
    drop(store);
    let mut log = SessionLog::open(&dir.path().join("sessions"), 1).unwrap();
    log.append(&Entry::TurnStarted).unwrap();
    log.append(&Entry::Item {
        item: Item {
            id: "result".into(),
            kind: ItemKind::UserMessage,
            text: delivered,
            status: ItemStatus::Completed,
        },
        seq: None,
        run: None,
    })
    .unwrap();
    log.sync().unwrap();
    let (view, cx) = open_window(&path, cx);
    cx.run_until_parked();
    for wrapper in ["which you launched", "session_read", "wrapper.example"] {
        type_search(&view, cx, wrapper);
        assert!(
            view.read_with(cx, |v, cx| v.message_rows(cx).is_empty()),
            "{wrapper:?} is not what the result's row draws"
        );
        cx.simulate_keystrokes("secondary-a backspace");
    }
    type_search(&view, cx, "zebra answer");
    let hits = view.read_with(cx, |v, cx| v.message_rows(cx));
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session, 1);
    assert_eq!(hits[0].snippet.text, "The zebra answer, see docs.");
}

fn ms(since: std::time::Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.
}

fn median(mut samples: Vec<f64>) -> (f64, f64, f64) {
    samples.sort_by(f64::total_cmp);
    (samples[samples.len() / 2], samples[0], samples[samples.len() - 1])
}

/// Find in a 5,100-row session: what a keystroke costs the UI thread, how long the
/// background search takes cold and warm, and a frame with the bar and marks drawn.
#[gpui::test]
#[ignore]
fn bench_find_in_a_long_session(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let needles: Vec<usize> = (0..1700).step_by(17).collect();
    let path = find_workspace(dir.path(), 1700, &needles, &[]);
    let (view, cx) = open_find_window(&path, 5100, cx);
    cx.simulate_keystrokes("secondary-f");
    let (mut ui, mut cold, mut warm, mut draw) = (vec![], vec![], vec![], vec![]);
    for run in 0..5 {
        view.update(cx, |this, _| this.finds.index = Default::default());
        for (query, samples) in [("needle", &mut cold), ("needl", &mut warm)] {
            view.update(cx, |this, cx| {
                let input = this.finds.bars[&MAIN_1].input.clone();
                input.update(cx, |input, cx| input.set_value(query.into(), cx));
            });
            let start = std::time::Instant::now();
            view.update(cx, |this, cx| this.search_find(MAIN_1, crate::find::Reason::Typed, cx));
            ui.push(ms(start));
            let start = std::time::Instant::now();
            cx.run_until_parked();
            samples.push(ms(start));
            let (count, read) = view.read_with(cx, |v, cx| {
                let bar = &v.finds.bars[&MAIN_1];
                (bar.count(cx), bar.rows_read)
            });
            assert!(count.as_ref().is_some_and(|c| c.ends_with("of 100")), "run {run}: {count:?}");
            assert_eq!(read, if query == "needle" { 5100 } else { 0 });
        }
        view.update(cx, |_, cx| cx.notify());
        cx.update(|window, cx| {
            let start = std::time::Instant::now();
            window.draw(cx).clear(cx);
            draw.push(ms(start));
        });
    }
    for (name, samples) in [
        ("find_keystroke_ui_ms", ui),
        ("find_search_cold_ms", cold),
        ("find_search_warm_ms", warm),
        ("find_draw_ms", draw),
    ] {
        let (med, low, high) = median(samples);
        eprintln!("PERF {name}={med:.2} range {low:.2}..{high:.2}");
    }
}

/// The sidebar search over 500 sessions: the scan of every saved message, and a
/// sidebar refresh with the results listed.
#[gpui::test]
#[ignore]
fn bench_sidebar_search_across_500_sessions(cx: &mut gpui::TestAppContext) {
    use keystone_agent::log::{Entry, SessionLog};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    for id in 1..=500u64 {
        store.create(Provider::Codex, &working_directory(), id).unwrap();
        store.rename(id, &format!("Session {id}")).unwrap();
    }
    store.flush().unwrap();
    drop(store);
    for session in 1..=500u64 {
        let mut log = SessionLog::open(&dir.path().join("sessions"), session).unwrap();
        for turn in 0..60 {
            let text = if turn == 30 && session % 2 == 0 {
                format!("Turn {turn}: the needle is here. ").repeat(4)
            } else {
                format!("Turn {turn} of session {session}: words, words and more words. ")
                    .repeat(8)
            };
            log.append(&Entry::Item {
                item: Item {
                    id: format!("m{session}-{turn}"),
                    kind: ItemKind::AgentMessage,
                    text,
                    status: ItemStatus::Completed,
                },
                seq: None,
                run: None,
            })
            .unwrap();
        }
    }
    let (view, cx) = open_window(&path, cx);
    cx.run_until_parked();
    view.update_in(cx, |this, window, cx| this.focus_composer(window, cx));
    cx.simulate_keystrokes("secondary-shift-f");
    let (mut scan, mut refresh) = (vec![], vec![]);
    for query in ["needle", "needl", "needle", "needl", "needle"] {
        view.update(cx, |this, cx| {
            this.search.update(cx, |input, cx| input.set_value(query.into(), cx));
        });
        let start = std::time::Instant::now();
        view.update(cx, |this, cx| this.search_messages(cx));
        wait_until(cx, &view, move |v, _| v.messages.searched.as_deref() == Some(query));
        scan.push(ms(start));
        assert_eq!(view.read_with(cx, |v, cx| v.message_rows(cx).len()), 250);
        view.update(cx, |this, cx| this.refresh(cx));
        cx.update(|window, cx| {
            let start = std::time::Instant::now();
            window.draw(cx).clear(cx);
            refresh.push(ms(start));
        });
    }
    for (name, samples) in [
        ("search_scan_500_sessions_ms", scan),
        ("search_sidebar_refresh_ms", refresh),
    ] {
        let (med, low, high) = median(samples);
        eprintln!("PERF {name}={med:.2} range {low:.2}..{high:.2}");
    }
}
