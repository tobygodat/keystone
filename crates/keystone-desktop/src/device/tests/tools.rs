use super::*;

#[gpui::test]
fn an_agent_opens_launches_screenshots_and_taps_through_the_tools(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    let reply = ask(
        &state,
        1,
        &format!("orchestrator test: device {PHONE} com.example.app"),
        cx,
    );
    assert!(
        reply.contains(
            "Scripted device success: listed=4 open=\"booted\" launch=done screenshot=603x1311 mime=image/png tap=done"
        ),
        "{reply}"
    );
    assert_eq!(
        host.work(),
        [
            format!("start {PHONE}"),
            format!("wait {PHONE}"),
            format!("act {PHONE} launch com.example.app"),
            format!("screenshot {PHONE}"),
            format!("act {PHONE} tap 0.500,0.500"),
        ],
        "the tap in screenshot pixels reaches the host as the screen's centre"
    );
    state.read_with(cx, |state, cx| {
        assert_eq!(beside(state, 1), Some(id(PHONE)));
        assert_eq!(life(state, PHONE), Some(Life::Booted));
        let record = state.devices.record(&id(PHONE)).unwrap();
        assert!(
            record.screen.read(cx).picture.is_some(),
            "the tool's screenshot feeds the panel"
        );
        assert_eq!(beside(state, 2), None, "only the caller's session");
    });
}

#[gpui::test]
fn a_read_only_session_and_a_second_opinion_look_but_never_touch(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::ReadOnly, &host, cx);
    let refused = ask(
        &state,
        1,
        &format!("orchestrator test: device {PHONE} com.example.app"),
        cx,
    );
    assert!(
        refused.contains(
            "Scripted device failure: device_open: A read-only session can't control devices"
        ),
        "{refused}"
    );
    assert_eq!(host.calls("start"), 0);
    open_for_user(&state, &[1], RUNNING, cx);
    let looked = ask(&state, 1, "orchestrator test: device look", cx);
    assert!(
        looked.contains(&format!(
            "open=\"{RUNNING}\" screenshot=603x1311 launch=refused"
        )),
        "{looked}"
    );
    let opinion = state.update(cx, |state, cx| {
        state
            .second_opinion(
                1,
                Provider::Claude,
                String::new(),
                3,
                WindowId::FIRST,
                None,
                cx,
            )
            .unwrap()
            .value
    });
    state.update(cx, |state, cx| {
        state
            .set_approval_mode(opinion, ApprovalMode::FullAccess, cx)
            .unwrap();
    });
    let reviewed = ask(&state, opinion, "orchestrator test: device look", cx);
    assert!(
        reviewed.contains(&format!("open=\"{RUNNING}\" screenshot=603x1311 launch=refused (device_launch: A second opinion only reads")),
        "{reviewed}"
    );
    assert_eq!(host.calls("act"), 0, "nothing reached the device");
}

#[gpui::test]
fn a_tap_before_any_screenshot_is_refused_rather_than_guessed(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    let nothing_open = ask(&state, 1, "orchestrator test: device tap", cx);
    assert!(
        nothing_open.contains("call device_open first"),
        "{nothing_open}"
    );
    open_for_user(&state, &[1], RUNNING, cx);
    let unseen = ask(&state, 1, "orchestrator test: device tap", cx);
    assert!(
        unseen.contains("Take a device_screenshot first"),
        "{unseen}"
    );
    assert_eq!(host.calls("act"), 0);
}

#[test]
fn the_models_copy_fits_its_edge_and_pixel_limits() {
    let phone = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        1206,
        2622,
        image::Rgba([10, 10, 10, 255]),
    ));
    let (image, (width, height)) = for_model(&phone).unwrap();
    assert_eq!(image.mime, "image/png");
    assert!(width.max(height) <= 1568 && width * height <= 1_150_000);
    assert_eq!((width, height), (721, 1568));
    let mut noise = image::RgbImage::new(1400, 1400);
    let mut seed = 7u32;
    for pixel in noise.pixels_mut() {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        *pixel = image::Rgb(seed.to_be_bytes()[..3].try_into().unwrap());
    }
    let (image, _) = for_model(&DynamicImage::ImageRgb8(noise)).unwrap();
    assert_eq!(image.mime, "image/jpeg", "a PNG over 1 MiB goes as JPEG");
}

#[gpui::test]
fn an_open_of_a_device_no_listing_names_leaves_the_open_one_and_says_what_there_is(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let host = FakeHost::new();
    let state = setup(directory.path(), ApprovalMode::Ask, &host, cx);
    open_for_user(&state, &[1], PHONE, cx);
    for (asked, says) in [
        (
            "android:No_Such_Phone",
            "No device android:No_Such_Phone here. Available: ",
        ),
        ("android:-help", "Unknown device id"),
    ] {
        let reply = ask(
            &state,
            1,
            &format!("orchestrator test: device open {asked}"),
            cx,
        );
        assert!(
            reply.contains(&format!("device_open: {says}")),
            "{asked}: {reply}"
        );
        state.read_with(cx, |state, _| {
            assert_eq!(beside(state, 1), Some(id(PHONE)), "{asked}: still open");
            assert_eq!(life(state, PHONE), Some(Life::Booted), "{asked}");
        });
    }
    let reply = ask(
        &state,
        1,
        "orchestrator test: device open android:No_Such_Phone",
        cx,
    );
    assert!(reply.contains(RUNNING) && reply.contains(PHONE), "{reply}");
    assert_eq!(host.calls("shutdown"), 0, "nothing was shut down");
    assert_eq!(host.calls("start android:No_Such_Phone"), 0);
}
