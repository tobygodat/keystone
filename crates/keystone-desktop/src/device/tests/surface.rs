use super::*;

#[gpui::test]
fn captures_run_only_while_a_window_shows_the_device(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], RUNNING, cx);
    assert_eq!(host.calls("screenshot"), 0, "nothing shows it yet");
    let window = WindowId::FIRST;
    state.update(cx, |state, cx| {
        state.device_drawn(window, Some(1), true, cx)
    });
    wait_until(&state, cx, "a frame arrives", |state, cx| {
        state
            .devices
            .record(&id(RUNNING))
            .is_some_and(|r| r.screen.read(cx).picture.is_some())
    });
    state.read_with(cx, |state, _| {
        assert_eq!(state.devices.pace(&id(RUNNING)), Some(ACTIVE));
    });
    let notified = Rc::new(Cell::new(0));
    let counted = notified.clone();
    let _watch = cx.update(|cx| cx.observe(&state, move |_, _| counted.set(counted.get() + 1)));
    let captured = state.read_with(cx, |state, _| state.devices.captures);
    advance_until(&state, cx, "more frames arrive", |state| {
        state.devices.captures >= captured + 2
    });
    assert_eq!(
        notified.get(),
        0,
        "new frames redraw the device's panes, not every window"
    );
    state.update(cx, |state, cx| {
        state.device_drawn(window, Some(1), false, cx)
    });
    state.read_with(cx, |state, _| {
        assert_eq!(state.devices.pace(&id(RUNNING)), Some(INACTIVE));
    });
    state.update(cx, |state, cx| state.device_drawn(window, None, false, cx));
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(100));
    cx.run_until_parked();
    let after_hiding = host.calls("screenshot");
    cx.executor().advance_clock(Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(100));
    cx.run_until_parked();
    assert_eq!(
        host.calls("screenshot"),
        after_hiding,
        "no capture once no window shows it"
    );
    state.read_with(cx, |state, _| {
        assert_eq!(state.devices.pace(&id(RUNNING)), None);
        assert!(matches!(
            state.devices.record(&id(RUNNING)).unwrap().capture,
            Capture::Idle
        ));
    });
}

#[test]
fn a_click_maps_through_the_fitted_image() {
    let pane = Bounds {
        origin: gpui::point(px(100.), px(0.)),
        size: gpui::size(px(400.), px(400.)),
    };
    let image = fitted(pane, (100, 200));
    assert_eq!(image.origin, gpui::point(px(200.), px(0.)));
    assert_eq!(image.size, gpui::size(px(200.), px(400.)));
    assert_eq!(
        fraction(image, gpui::point(px(300.), px(100.))),
        Point::new(0.5, 0.25)
    );
    assert_eq!(fraction(image, gpui::point(px(150.), px(100.))), None);
}

/// A window showing session 1, over `host`, with its state.
fn surface(
    directory: &Path,
    host: &Arc<FakeHost>,
    cx: &mut TestAppContext,
) -> (&'static mut gpui::VisualTestContext, Entity<AppState>) {
    use crate::windows::open;
    use crate::{install_app_state, open_windows};
    let path = directory.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    let project = WorkingDirectory::new(directory).unwrap();
    store.create(Provider::Claude, &project, 1).unwrap();
    let project = keystone_core::ProjectId::new(directory).unwrap();
    store
        .open_project(WindowId::FIRST, &project, None, Some(1), 10)
        .unwrap();
    drop(store);
    cx.executor().allow_parking();
    let fake = host.clone();
    let state = cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state: Entity<AppState> = install_app_state(Store::open(&path).unwrap(), cx);
        state.update(cx, |state, _| state.devices = Devices::new(fake));
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            open(state.clone(), id, placement, true, false, cx).unwrap();
        }
        state
    });
    cx.run_until_parked();
    let view = cx.update(|cx| open_windows(cx))[0].clone();
    let handle = cx.read(|cx| view.read(cx).handle);
    handle
        .update(cx, |_, window, _| window.activate_window())
        .unwrap();
    let cx = gpui::VisualTestContext::from_window(handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(1280.), px(800.)));
    cx.run_until_parked();
    (cx, state)
}

fn until(cx: &mut gpui::VisualTestContext, what: &str, id: &'static str) {
    let start = Instant::now();
    while cx.debug_bounds(id).is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
        cx.run_until_parked();
    }
}

fn click(cx: &mut gpui::VisualTestContext, id: &'static str) {
    let bounds = cx.debug_bounds(id).unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
fn the_surface_lists_devices_opens_one_and_taps_where_clicked(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let (cx, _) = surface(directory.path(), &host, cx);
    cx.dispatch_action(ToggleDevice);
    until(cx, "the picker lists devices", "device-open-2");
    click(cx, "device-open-1");
    until(cx, "the slow device's screen", "device-close");
    click(cx, "device-close");
    until(
        cx,
        "Close during the boot goes back to the picker",
        "device-open-2",
    );
    assert_eq!(
        host.calls(&format!("shutdown {SLOW}")),
        1,
        "Close shut down the device Keystone was booting"
    );
    assert!(
        !host.release.load(Ordering::Acquire),
        "while it still booted"
    );
    click(cx, "device-open-2");
    until(cx, "the running device's first frame", "device-frame");
    cx.run_until_parked();
    let frame = cx.debug_bounds("device-frame").unwrap();
    let image = cx.debug_bounds("device-image").expect("the image is drawn");
    assert!(
        frame.contains(&image.origin)
            && frame.contains(&(image.bottom_right() - gpui::point(px(1.), px(1.)))),
        "the image stays inside its frame: {image:?} in {frame:?}"
    );
    assert!(
        frame.bottom() <= px(800.),
        "the frame fits the pane, not the image's own height: {frame:?}"
    );
    click(cx, "device-frame");
    let start = Instant::now();
    while host.calls("act") == 0 {
        assert!(start.elapsed() < Duration::from_secs(10), "the click taps");
        std::thread::sleep(Duration::from_millis(10));
        cx.run_until_parked();
    }
    assert_eq!(
        host.log().iter().find(|l| l.starts_with("act")).unwrap(),
        &format!("act {RUNNING} tap 0.500,0.500"),
        "a click on the frame's centre taps the screen's centre"
    );
    click(cx, "device-close");
    until(cx, "back to the picker", "device-open-2");
    assert_eq!(
        host.calls(&format!("shutdown {RUNNING}")),
        0,
        "the user booted it, so it keeps running"
    );
    host.release.store(true, Ordering::Release);
}

#[gpui::test]
fn the_surface_follows_a_device_closed_and_opened_again_while_hidden(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let (cx, state) = surface(directory.path(), &host, cx);
    cx.dispatch_action(ToggleDevice);
    until(cx, "the picker lists devices", "device-open-2");
    click(cx, "device-open-2");
    until(cx, "the running device's first frame", "device-image");
    cx.dispatch_action(ToggleDevice);
    cx.run_until_parked();
    let before = state.read_with(cx, |state, _| {
        state
            .devices
            .record(&id(RUNNING))
            .unwrap()
            .screen
            .entity_id()
    });
    state.update(cx, |state, cx| state.close_device(1, cx));
    cx.run_until_parked();
    state.update(cx, |state, cx| state.open_device(1, id(RUNNING), cx));
    let start = Instant::now();
    while state.read_with(cx, |state, _| life(state, RUNNING)) != Some(Life::Booted) {
        assert!(start.elapsed() < Duration::from_secs(10), "it opens again");
        std::thread::sleep(Duration::from_millis(10));
        cx.run_until_parked();
    }
    state.read_with(cx, |state, cx| {
        let record = state.devices.record(&id(RUNNING)).unwrap();
        assert_ne!(record.screen.entity_id(), before, "a new screen");
        assert!(record.screen.read(cx).picture.is_none());
    });
    cx.dispatch_action(ToggleDevice);
    until(cx, "a frame on the new screen is drawn", "device-image");
}

#[test]
fn a_labelled_frame_ticks_each_second_and_an_unlabelled_one_when_it_turns_stale() {
    let young = Duration::from_millis(300);
    assert_eq!(next_tick(young, true), Duration::from_millis(700));
    assert_eq!(next_tick(young, false), STALE - young);
    let old = STALE + Duration::from_millis(250);
    assert_eq!(next_tick(old, false), Duration::from_millis(750));
}

#[gpui::test]
fn the_age_of_a_frame_counts_up_while_no_new_frame_comes(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let (cx, state) = surface(directory.path(), &host, cx);
    cx.dispatch_action(ToggleDevice);
    until(cx, "the picker lists devices", "device-open-2");
    click(cx, "device-open-2");
    until(cx, "the first frame", "device-image");
    host.hold("screenshot");
    cx.executor().advance_clock(ACTIVE);
    let start = Instant::now();
    while host.calls("screenshot") < 2 {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "a capture sticks"
        );
        std::thread::sleep(Duration::from_millis(10));
        cx.run_until_parked();
    }
    state.update(cx, |state, cx| {
        let screen = &state.devices.record(&id(RUNNING)).unwrap().screen;
        screen.update(cx, |screen, _| {
            let picture = screen.picture.as_mut().unwrap();
            picture.at = Instant::now() - Duration::from_secs(6);
        });
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("device-stale").is_none(), "not redrawn yet");
    cx.executor().advance_clock(STALE);
    until(cx, "the label comes up on its own", "device-stale");
    host.let_go("screenshot");
}

#[gpui::test]
fn a_device_keystone_could_not_shut_down_says_so_in_the_picker(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let (cx, state) = surface(directory.path(), &host, cx);
    cx.dispatch_action(ToggleDevice);
    until(cx, "the picker lists devices", "device-open-2");
    state.update(cx, |state, cx| state.open_device(1, id(PHONE), cx));
    let start = Instant::now();
    while state.read_with(cx, |state, _| life(state, PHONE)) != Some(Life::Booted) {
        assert!(start.elapsed() < Duration::from_secs(10), "the phone boots");
        std::thread::sleep(Duration::from_millis(10));
        cx.run_until_parked();
    }
    host.refuse_shutdown.store(true, Ordering::Release);
    state.update(cx, |state, cx| state.open_device(1, id(RUNNING), cx));
    until(cx, "the running device opens", "device-close");
    click(cx, "device-close");
    until(
        cx,
        "the phone's row says it kept running",
        "device-refused-0",
    );
}

#[gpui::test]
fn a_failed_close_says_why_while_frames_keep_coming(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let (cx, state) = surface(directory.path(), &host, cx);
    state.update(cx, |state, cx| state.open_device(1, id(PHONE), cx));
    cx.dispatch_action(ToggleDevice);
    until(cx, "the phone's first frame", "device-image");
    host.refuse_shutdown.store(true, Ordering::Release);
    click(cx, "device-close");
    until(cx, "the panel says why", "device-refused");
    let captured = host.calls("screenshot");
    let start = Instant::now();
    while host.calls("screenshot") < captured + 2 {
        assert!(start.elapsed() < Duration::from_secs(10), "captures go on");
        cx.executor().advance_clock(ACTIVE);
        std::thread::sleep(Duration::from_millis(10));
        cx.run_until_parked();
    }
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("device-refused").is_some(),
        "a capture that worked keeps the reason"
    );
    assert!(cx.debug_bounds("device-image").is_some());
    let listed = host.calls("list");
    state.update(cx, |state, cx| state.list_devices(cx));
    let start = Instant::now();
    while host.calls("list") == listed || state.read_with(cx, |s, _| s.devices.list != Fetch::Idle)
    {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the listing ends"
        );
        std::thread::sleep(Duration::from_millis(10));
        cx.run_until_parked();
    }
    assert!(
        cx.debug_bounds("device-refused").is_some(),
        "a listing keeps the reason while the session still has the device"
    );
}

#[test]
fn a_picker_row_says_what_keystone_holds_before_an_older_listing() {
    use super::super::pane::row_state;
    assert_eq!(
        row_state(Some(&Life::Booted), Power::Booting),
        "Running",
        "running beside another session, listed while it booted"
    );
    assert_eq!(row_state(Some(&Life::Off), Power::Booted), "Off");
    assert_eq!(
        row_state(Some(&Life::Stalled), Power::Booting),
        "Still booting"
    );
    assert_eq!(row_state(None, Power::Booted), "Running");
    assert_eq!(row_state(None, Power::Shutdown), "Off");
}

#[gpui::test]
fn a_drag_let_go_past_the_frame_swipes_to_its_edge(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let (cx, _state) = surface(directory.path(), &host, cx);
    cx.dispatch_action(ToggleDevice);
    until(cx, "the picker lists devices", "device-open-2");
    click(cx, "device-open-2");
    until(cx, "the running device's first frame", "device-frame");
    cx.run_until_parked();
    let frame = cx.debug_bounds("device-frame").unwrap();
    let from = frame.center();
    let past = gpui::point(frame.origin.x - px(200.), from.y);
    cx.simulate_mouse_down(from, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.simulate_mouse_move(past, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.simulate_mouse_up(past, gpui::MouseButton::Left, gpui::Modifiers::none());
    let start = Instant::now();
    while host.calls("act") == 0 {
        assert!(start.elapsed() < Duration::from_secs(10), "the drag swipes");
        std::thread::sleep(Duration::from_millis(10));
        cx.run_until_parked();
    }
    let swipe = host
        .log()
        .into_iter()
        .find(|l| l.starts_with("act"))
        .unwrap();
    assert!(
        swipe.contains("Swipe") && swipe.contains("x: 0.0,"),
        "to the left edge, at the height it was let go: {swipe}"
    );
}
