use super::*;

#[gpui::test]
fn a_second_open_joins_a_slow_boot_and_close_answers_while_it_boots(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    send(
        &state,
        1,
        &format!("orchestrator test: device open {SLOW}"),
        cx,
    );
    wait_until(&state, cx, "Keystone waits on the boot", |state, _| {
        matches!(life(state, SLOW), Some(Life::Booting { .. }))
    });
    send(
        &state,
        2,
        &format!("orchestrator test: device open {SLOW}"),
        cx,
    );
    wait_until(&state, cx, "the second open joins", |state, _| {
        beside(state, 2) == Some(id(SLOW))
    });
    cx.executor().advance_clock(Duration::from_secs(60));
    for session in [1, 2] {
        let opened = reply(&state, session, cx);
        assert!(opened.contains("open=\"booting\""), "{opened}");
    }
    assert_eq!(host.calls("start"), 1, "the second open joins the first");
    let first = ask(&state, 1, "orchestrator test: device close", cx);
    assert!(first.contains("close shut_down=false"), "{first}");
    let last = ask(&state, 2, "orchestrator test: device close", cx);
    assert!(last.contains("close shut_down=true"), "{last}");
    assert!(
        !host.release.load(Ordering::Acquire),
        "both closes answered while the boot was still being waited on"
    );
    state.read_with(cx, |state, _| {
        assert_eq!(beside(state, 1), None);
        assert_eq!(beside(state, 2), None);
        assert_eq!(life(state, SLOW), None, "a device shut down is forgotten");
    });
    host.release.store(true, Ordering::Release);
}

#[gpui::test]
fn close_shuts_down_only_what_keystone_booted_and_no_one_else_has_open(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1, 2], PHONE, cx);
    state.update(cx, |state, cx| state.close_device(1, cx));
    wait_until(&state, cx, "session 1 lets go", |state, _| {
        beside(state, 1).is_none()
    });
    assert_eq!(host.calls("shutdown"), 0, "session 2 still has it open");
    state.update(cx, |state, cx| state.close_device(2, cx));
    wait_until(&state, cx, "session 2 lets go", |state, _| {
        beside(state, 2).is_none()
    });
    assert_eq!(host.work().last().unwrap(), &format!("shutdown {PHONE}"));
    open_for_user(&state, &[1], RUNNING, cx);
    state.update(cx, |state, cx| state.close_device(1, cx));
    wait_until(&state, cx, "it closes", |state, _| {
        beside(state, 1).is_none()
    });
    assert_eq!(
        host.calls("shutdown"),
        1,
        "a device Keystone did not boot is never shut down"
    );
}

#[gpui::test]
fn a_boot_another_start_took_over_is_followed_but_never_shut_down(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    host.taken.store(true, Ordering::Release);
    open_for_user(&state, &[1], PHONE, cx);
    assert_eq!(
        host.calls(&format!("wait {PHONE}")),
        2,
        "a second wait followed the boot"
    );
    assert!(
        state.read_with(cx, |state, _| !lifecycle(state, PHONE).unwrap().owned()),
        "the device is not Keystone's"
    );
    state.update(cx, |state, cx| state.close_device(1, cx));
    wait_until(&state, cx, "it closes", |state, _| {
        beside(state, 1).is_none()
    });
    state.update(cx, |state, cx| state.teardown(cx));
    assert_eq!(
        host.calls("shutdown"),
        0,
        "neither Close nor Quit shuts down another's"
    );
}

#[gpui::test]
fn two_closes_at_once_still_shut_down_what_keystone_booted(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1, 2], PHONE, cx);
    state.update(cx, |state, cx| {
        state.close_device(1, cx);
        state.close_device(2, cx);
    });
    wait_until(&state, cx, "both let go", |state, _| {
        beside(state, 1).is_none() && beside(state, 2).is_none()
    });
    assert_eq!(host.calls("shutdown"), 1);
    assert_eq!(host.power(&id(PHONE)), Some(Power::Shutdown));
}

#[gpui::test]
fn a_close_never_shuts_down_a_device_another_session_is_opening(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], PHONE, cx);
    state.update(cx, |state, cx| {
        state.open_device(2, id(PHONE), cx);
        state.close_device(1, cx);
    });
    wait_until(&state, cx, "session 2 has it running", |state, _| {
        beside(state, 1).is_none() && life(state, PHONE) == Some(Life::Booted)
    });
    assert_eq!(host.calls("shutdown"), 0);
    state.update(cx, |state, cx| state.close_device(2, cx));
    wait_until(&state, cx, "session 2 lets go", |state, _| {
        beside(state, 2).is_none()
    });
    assert_eq!(
        host.calls("shutdown"),
        1,
        "Keystone still owns the device it booted and shuts it down last"
    );
}

#[gpui::test]
fn replacing_a_device_lets_go_of_it_the_way_close_does(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1, 2], PHONE, cx);
    open_for_user(&state, &[1], RUNNING, cx);
    assert_eq!(host.calls("shutdown"), 0, "session 2 still has the phone");
    open_for_user(&state, &[2], RUNNING, cx);
    wait_until(&state, cx, "the phone shuts down", |_, _| {
        host.calls("shutdown") == 1
    });
    assert_eq!(host.power(&id(PHONE)), Some(Power::Shutdown));
}

#[gpui::test]
fn an_older_open_never_replaces_a_newer_one(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    send(
        &state,
        1,
        &format!("orchestrator test: device open {SLOW}"),
        cx,
    );
    wait_until(&state, cx, "the slow boot is waited on", |state, _| {
        matches!(life(state, SLOW), Some(Life::Booting { .. }))
    });
    open_for_user(&state, &[1], RUNNING, cx);
    let older = reply(&state, 1, cx);
    assert!(
        older.contains("no longer open beside this session"),
        "{older}"
    );
    host.release.store(true, Ordering::Release);
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(50));
    cx.run_until_parked();
    state.read_with(cx, |state, _| {
        assert_eq!(beside(state, 1), Some(id(RUNNING)));
    });
    assert_eq!(
        host.calls(&format!("shutdown {SLOW}")),
        1,
        "the slow device Keystone booted was let go"
    );
}

#[gpui::test]
fn a_failed_shutdown_keeps_the_device_and_says_why(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    let opened = ask(
        &state,
        1,
        &format!("orchestrator test: device open {PHONE}"),
        cx,
    );
    assert!(opened.contains("open=\"booted\""), "{opened}");
    host.refuse_shutdown.store(true, Ordering::Release);
    let refused = ask(&state, 1, "orchestrator test: device close", cx);
    assert!(
        refused.contains("Couldn't shut the device down: simctl refused"),
        "{refused}"
    );
    state.read_with(cx, |state, _| {
        assert_eq!(beside(state, 1), Some(id(PHONE)), "still open beside it");
        assert_eq!(life(state, PHONE), Some(Life::Booted));
        assert!(
            lifecycle(state, PHONE)
                .and_then(|l| l.refused)
                .is_some_and(|e| e.contains("simctl refused")),
            "the panel says why"
        );
    });
    host.refuse_shutdown.store(false, Ordering::Release);
    state.update(cx, |state, cx| state.close_device(1, cx));
    wait_until(&state, cx, "a second Close shuts it down", |state, _| {
        beside(state, 1).is_none()
    });
    assert_eq!(host.power(&id(PHONE)), Some(Power::Shutdown));
}

#[gpui::test]
fn a_stalled_boot_says_so_and_a_listing_that_finds_it_running_resumes_it(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    state.update(cx, |state, cx| state.open_device(1, id(STUCK), cx));
    wait_until(&state, cx, "Keystone stops waiting", |state, _| {
        life(state, STUCK) == Some(Life::Stalled)
    });
    host.set_power(&id(STUCK), Power::Booted);
    state.update(cx, |state, cx| state.list_devices(cx));
    wait_until(&state, cx, "the listing finds it running", |state, _| {
        life(state, STUCK) == Some(Life::Booted)
    });
}

#[gpui::test]
fn open_devices_are_not_restored_after_a_restart(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], RUNNING, cx);
    drop(state);
    cx.run_until_parked();
    let store = Store::open(directory.path().join("workspace.json")).unwrap();
    let again = cx.new(|cx| AppState::new(store, cx));
    again.read_with(cx, |state, _| assert!(beside(state, 1).is_none()));
}

#[gpui::test]
fn a_device_keystone_booted_that_stopped_and_was_booted_again_elsewhere_is_not_shut_down(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], PHONE, cx);
    state.update(cx, |state, cx| {
        state.device_drawn(WindowId::FIRST, Some(1), true, cx)
    });
    host.set_power(&id(PHONE), Power::Shutdown);
    advance_until(&state, cx, "a capture finds it stopped", |state| {
        life(state, PHONE) == Some(Life::Off)
    });
    state.read_with(cx, |state, cx| {
        let screen = state.devices.record(&id(PHONE)).unwrap().screen.read(cx);
        assert_eq!(
            screen.error, None,
            "the phase says Not running; the agent's words stay out of the panel"
        );
    });
    host.set_power(&id(PHONE), Power::Booted);
    state.update(cx, |state, cx| state.list_devices(cx));
    wait_until(&state, cx, "Refresh finds it running", |state, _| {
        life(state, PHONE) == Some(Life::Booted)
    });
    state.update(cx, |state, cx| state.close_device(1, cx));
    wait_until(&state, cx, "it closes", |state, _| {
        beside(state, 1).is_none()
    });
    assert_eq!(host.calls("shutdown"), 0, "the user booted it this time");
}

#[gpui::test]
fn a_failed_shutdown_during_a_boot_leaves_it_booting_not_running(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    state.update(cx, |state, cx| state.open_device(1, id(SLOW), cx));
    wait_until(&state, cx, "Keystone waits on the boot", |state, _| {
        matches!(life(state, SLOW), Some(Life::Booting { .. }))
    });
    host.refuse_shutdown.store(true, Ordering::Release);
    state.update(cx, |state, cx| state.close_device(1, cx));
    wait_until(&state, cx, "the shutdown fails", |state, _| {
        lifecycle(state, SLOW).is_some_and(|l| l.refused.is_some())
    });
    state.read_with(cx, |state, _| {
        assert!(
            matches!(life(state, SLOW), Some(Life::Booting { .. })),
            "{:?}",
            life(state, SLOW)
        );
        assert_eq!(beside(state, 1), Some(id(SLOW)), "still open beside it");
    });
    host.release.store(true, Ordering::Release);
    wait_until(&state, cx, "the boot it still waits on", |state, _| {
        life(state, SLOW) == Some(Life::Booted)
    });
}

#[gpui::test]
fn an_open_that_joined_a_failed_shutdown_is_answered_when_it_fails(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], PHONE, cx);
    host.refuse_shutdown.store(true, Ordering::Release);
    host.hold("shutdown");
    state.update(cx, |state, cx| state.close_device(1, cx));
    wait_until(&state, cx, "Keystone shuts it down", |_, _| {
        host.calls("shutdown") == 1
    });
    send(
        &state,
        2,
        &format!("orchestrator test: device open {PHONE}"),
        cx,
    );
    wait_until(&state, cx, "the open joins the shutdown", |state, _| {
        beside(state, 2) == Some(id(PHONE))
    });
    host.let_go("shutdown");
    let opened = reply(&state, 2, cx);
    assert!(opened.contains("open=\"booted\""), "{opened}");
    state.read_with(cx, |state, _| {
        assert_eq!(life(state, PHONE), Some(Life::Booted));
        assert_eq!(beside(state, 1), Some(id(PHONE)));
    });
}

#[gpui::test]
fn a_device_no_session_holds_that_would_not_shut_down_is_kept_and_tried_again(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], PHONE, cx);
    host.refuse_shutdown.store(true, Ordering::Release);
    open_for_user(&state, &[1], RUNNING, cx);
    wait_until(
        &state,
        cx,
        "the replaced phone's shutdown fails",
        |state, _| lifecycle(state, PHONE).is_some_and(|l| l.refused.is_some()),
    );
    state.read_with(cx, |state, _| {
        let kept = lifecycle(state, PHONE).unwrap();
        assert!(kept.owned(), "still Keystone's to shut down: {kept:?}");
        assert_eq!(kept.life, Life::Booted);
        assert!(state.devices.beside.values().all(|b| b.device != id(PHONE)));
    });
    host.refuse_shutdown.store(false, Ordering::Release);
    state.update(cx, |state, cx| state.list_devices(cx));
    wait_until(&state, cx, "a listing tries again", |state, _| {
        life(state, PHONE).is_none()
    });
    assert_eq!(host.power(&id(PHONE)), Some(Power::Shutdown));
}

#[gpui::test]
fn reopening_a_running_device_keeps_its_frames_and_input_flowing(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], RUNNING, cx);
    state.update(cx, |state, cx| {
        state.device_drawn(WindowId::FIRST, Some(1), true, cx)
    });
    advance_until(&state, cx, "frames arrive", |state| {
        state.devices.captures > 0
    });
    host.hold("start");
    state.update(cx, |state, cx| {
        state.open_device(2, id(RUNNING), cx);
        assert_eq!(life(state, RUNNING), Some(Life::Booted), "never Opening…");
        state.device_input(1, Action::Tap(Point::new(0.5, 0.5).unwrap()), cx);
    });
    wait_until(&state, cx, "the host is asked again", |_, _| {
        host.calls("start") == 2
    });
    host.let_go("start");
    wait_until(&state, cx, "the tap reaches the device", |_, _| {
        host.calls("act") == 1
    });
    let captured = state.read_with(cx, |state, _| state.devices.captures);
    advance_until(&state, cx, "frames keep coming", |state| {
        state.devices.captures > captured
    });
    state.read_with(cx, |state, _| {
        assert_eq!(life(state, RUNNING), Some(Life::Booted));
        assert_eq!(beside(state, 2), Some(id(RUNNING)));
    });
}

#[gpui::test]
fn a_close_that_outlasts_a_slow_start_says_closing_and_goes_through(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    host.hold("start");
    state.update(cx, |state, cx| state.open_device(1, id(PHONE), cx));
    send(&state, 1, "orchestrator test: device close", cx);
    wait_until(&state, cx, "the close waits on the start", |state, _| {
        state
            .devices
            .record(&id(PHONE))
            .is_some_and(|r| r.waiting.iter().any(|w| w.request.is_some()))
    });
    cx.executor().advance_clock(Duration::from_secs(30));
    let answered = reply(&state, 1, cx);
    assert!(answered.contains("state=\"closing\""), "{answered}");
    host.let_go("start");
    wait_until(&state, cx, "the close goes through", |state, _| {
        beside(state, 1).is_none() && life(state, PHONE).is_none()
    });
    assert_eq!(host.power(&id(PHONE)), Some(Power::Shutdown));
}

#[gpui::test]
fn a_refresh_while_a_listing_runs_lists_once_more_after_it(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    host.hold("inventory");
    state.update(cx, |state, cx| state.list_devices(cx));
    wait_until(&state, cx, "the listing runs", |_, _| {
        host.calls("list") == 1
    });
    host.set_power(&id(PHONE), Power::Booted);
    state.update(cx, |state, cx| {
        state.list_devices(cx);
        state.list_devices(cx);
    });
    host.let_go("inventory");
    wait_until(&state, cx, "a listing that saw the change", |state, _| {
        state.devices.list == Fetch::Idle
            && state
                .devices
                .inventory
                .as_ref()
                .and_then(|i| i.device(&id(PHONE)))
                .is_some_and(|d| d.power == Power::Booted)
    });
    assert_eq!(host.calls("list"), 2, "one more, however many asked");
}

#[gpui::test]
fn a_listing_that_panics_says_so_and_the_next_one_runs(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    host.broken_listing.store(true, Ordering::Release);
    state.update(cx, |state, cx| state.list_devices(cx));
    wait_until(&state, cx, "the listing fails", |state, _| {
        state.devices.list == Fetch::Idle && state.devices.list_error.is_some()
    });
    host.broken_listing.store(false, Ordering::Release);
    state.update(cx, |state, cx| state.list_devices(cx));
    wait_until(&state, cx, "the next listing", |state, _| {
        state.devices.inventory.is_some() && state.devices.list_error.is_none()
    });
}

#[gpui::test]
fn a_start_that_panics_fails_the_open_and_the_next_open_asks_again(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    host.broken_start.store(true, Ordering::Release);
    state.update(cx, |state, cx| state.open_device(1, id(PHONE), cx));
    wait_until(&state, cx, "the open fails", |state, _| {
        life(state, PHONE) == Some(Life::Failed(PANICKED.into()))
    });
    host.broken_start.store(false, Ordering::Release);
    open_for_user(&state, &[1], PHONE, cx);
    assert_eq!(host.calls("start"), 2);
}

#[gpui::test]
fn an_open_or_close_a_shutdown_outlasts_says_it_is_shutting_down(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], PHONE, cx);
    host.hold("shutdown");
    send(&state, 1, "orchestrator test: device close", cx);
    wait_until(&state, cx, "the shutdown runs", |_, _| {
        host.calls("shutdown") == 1
    });
    send(
        &state,
        2,
        &format!("orchestrator test: device open {PHONE}"),
        cx,
    );
    wait_until(&state, cx, "the open joins the shutdown", |state, _| {
        state.devices.record(&id(PHONE)).is_some_and(|r| {
            r.waiting
                .iter()
                .any(|w| w.session == 2 && w.request.is_some())
        })
    });
    cx.executor().advance_clock(Duration::from_secs(60));
    let closed = reply(&state, 1, cx);
    assert!(closed.contains("state=\"shutting_down\""), "{closed}");
    let opened = reply(&state, 2, cx);
    assert!(opened.contains("open=\"shutting_down\""), "{opened}");
    host.let_go("shutdown");
}
