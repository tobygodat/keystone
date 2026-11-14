use super::*;

#[gpui::test]
fn quit_shuts_down_the_devices_keystone_booted(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], PHONE, cx);
    open_for_user(&state, &[2], RUNNING, cx);
    state.update(cx, |state, cx| state.teardown(cx));
    assert_eq!(host.calls(&format!("shutdown {PHONE}")), 1);
    assert_eq!(
        host.calls("shutdown"),
        1,
        "a device Keystone did not boot keeps running"
    );
    assert_eq!(
        host.log().last().map(String::as_str),
        Some("stop"),
        "Quit ends what still runs once its shutdowns are done"
    );
    assert_eq!(host.calls("stop"), 1);
}

#[gpui::test]
fn quit_shuts_down_a_device_whose_start_is_running_and_runs_nothing_queued(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[2], RUNNING, cx);
    host.hold("start");
    state.update(cx, |state, cx| {
        state.open_device(1, id(PHONE), cx);
        state.open_device(2, id(RUNNING), cx);
        let tap = Action::Tap(Point::new(0.5, 0.5).unwrap());
        state.act(id(PHONE), tap, Deadline::after(INPUT_LIMIT), None, cx);
    });
    wait_until(&state, cx, "both starts run", |_, _| {
        host.calls("start") == 3
    });
    let releasing = host.clone();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        releasing.let_go("start");
    });
    state.update(cx, |state, cx| state.teardown(cx));
    release.join().unwrap();
    assert_eq!(
        host.work().last().unwrap(),
        &format!("shutdown {PHONE}"),
        "the start Quit waited on booted the phone, so it shut down"
    );
    assert_eq!(host.power(&id(PHONE)), Some(Power::Shutdown));
    assert_eq!(host.calls(&format!("shutdown {RUNNING}")), 0, "the user's");
    assert_eq!(
        host.calls("act"),
        0,
        "the queued tap never reached the host"
    );
}

#[gpui::test]
fn quit_shuts_down_a_device_keystone_booted_while_a_start_holds_its_lane(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], PHONE, cx);
    host.hold("start");
    state.update(cx, |state, cx| state.open_device(2, id(PHONE), cx));
    wait_until(&state, cx, "the second start runs", |_, _| {
        host.calls("start") == 2
    });
    state.update(cx, |state, cx| state.teardown(cx));
    assert_eq!(
        host.calls(&format!("shutdown {PHONE}")),
        1,
        "the stuck start did not keep Quit from it"
    );
    host.let_go("start");
}

#[gpui::test]
fn quit_waits_for_a_shutdown_already_running_and_never_starts_a_second(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], PHONE, cx);
    host.hold("shutdown");
    state.update(cx, |state, cx| state.close_device(1, cx));
    wait_until(&state, cx, "the shutdown runs", |_, _| {
        host.calls("shutdown") == 1
    });
    let releasing = host.clone();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        releasing.let_go("shutdown");
    });
    let started = Instant::now();
    state.update(cx, |state, cx| state.teardown(cx));
    assert_eq!(
        host.power(&id(PHONE)),
        Some(Power::Shutdown),
        "Quit waited for the shutdown on the lane"
    );
    assert!(started.elapsed() < QUIT_LIMIT, "no longer than it took");
    release.join().unwrap();
    assert_eq!(host.calls("shutdown"), 1, "no second shutdown at once");
    assert_eq!(host.log().last().map(String::as_str), Some("stop"));
}

#[gpui::test]
fn quit_waits_for_a_shutdown_on_the_lane_no_longer_than_its_budget(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], PHONE, cx);
    host.hold("shutdown");
    state.update(cx, |state, cx| state.close_device(1, cx));
    wait_until(&state, cx, "the shutdown runs", |_, _| {
        host.calls("shutdown") == 1
    });
    let started = Instant::now();
    state.update(cx, |state, cx| state.teardown(cx));
    let took = started.elapsed();
    assert!(
        took >= QUIT_LIMIT - Duration::from_millis(100) && took < QUIT_LIMIT * 2,
        "{took:?}"
    );
    assert_eq!(
        host.log().last().map(String::as_str),
        Some("stop"),
        "the host's stop ends what the shutdown left running"
    );
    host.let_go("shutdown");
}
