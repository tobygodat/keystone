//! Usage displays through the app with the scripted Claude: its synthetic
//! `rate_limit_event` frames reach the footer meter only at the threshold, the reading
//! is saved and loaded again with the time it was taken after a restart, and the meter
//! opens Settings → Accounts.

use crate::*;
use keystone_agent::ItemStatus;
use std::os::unix::fs::PermissionsExt;

fn fake_claude(dir: &Path) -> PathBuf {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("keystone-fake-claude");
    assert!(
        binary.exists(),
        "build it with `cargo build -p keystone-agent --bins`"
    );
    let script = dir.join("claude-usage-wrapper");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nKEYSTONE_FAKE_CLAUDE_DELAY_MS=1 exec '{}' \"$@\"\n",
            binary.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn open<'a>(
    path: &Path,
    claude: &Path,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
    let store = Store::open(path).unwrap();
    cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        Keystone::new(cx.new(|cx| AppState::new(store, cx)), false, window, cx)
    });
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, _| {
            state.test_set_claude(Some(ClaudeConnection::new(
                Some(claude.to_owned()),
                state.test_wake().clone(),
            )));
        })
    });
    (view, cx)
}

fn send(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, text: &str) {
    view.update_in(cx, |view, window, cx| {
        view.composer
            .update(cx, |input, cx| input.set_value(text.into(), cx));
        let draft = view.composer_draft(cx).unwrap();
        view.send_message(draft, window, cx);
    });
    let expected = format!("You wrote: {text}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        let done = view.read_with(cx, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| {
                    !c.running
                        && c.rows.iter().any(|row| {
                            matches!(row, Row::Item(item) if item.status == ItemStatus::Completed
                                && item.text.contains(&expected))
                        })
                })
        });
        if done {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "no reply to {text}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn five_hour(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Option<f64> {
    view.read_with(cx, |view, cx| {
        let usage = view.state.read(cx).usage(Provider::Claude, None)?;
        usage
            .readings
            .iter()
            .find(|r| r.window.id == "five_hour")
            .map(|r| r.window.used_percent)
    })
}

#[gpui::test]
fn the_footer_meter_shows_only_above_the_threshold_and_the_reading_survives_a_restart(
    cx: &mut gpui::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let claude = fake_claude(dir.path());
    let (view, vcx) = open(&path, &claude, cx);
    let bounds = vcx.debug_bounds("welcome-claude").unwrap();
    vcx.simulate_click(bounds.center(), gpui::Modifiers::none());
    vcx.run_until_parked();

    send(&view, vcx, "usage 40");
    assert_eq!(five_hour(&view, vcx), Some(40.0));
    assert!(vcx.debug_bounds("usage-meter").is_none(), "40% stays quiet");
    assert_eq!(
        view.read_with(vcx, |view, cx| view.state.read(cx).conversations()[&1]
            .context),
        Some((40_000, Some(200_000)))
    );

    send(&view, vcx, "usage 85");
    assert_eq!(five_hour(&view, vcx), Some(85.0));
    assert!(
        vcx.debug_bounds("usage-meter").is_some(),
        "85% shows the meter"
    );
    // The keyboard reaches it: focused, Enter opens Settings → Accounts.
    view.update_in(vcx, |view, window, cx| view.usage_focus.focus(window, cx));
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("settings-accounts").is_some());
    let back = vcx.debug_bounds("settings-back").unwrap();
    vcx.simulate_click(back.center(), gpui::Modifiers::none());
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("settings-accounts").is_none());
    let meter = vcx.debug_bounds("usage-meter").unwrap();
    vcx.simulate_click(meter.center(), gpui::Modifiers::none());
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("settings-accounts").is_some());
    assert!(
        vcx.debug_bounds("account-usage-claude-default-five_hour")
            .is_some()
    );
    assert!(
        vcx.debug_bounds("account-usage-claude-default-seven_day")
            .is_some()
    );

    let saved = Store::open(&path).unwrap().database().usage().unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].0, "claude/default");
    let five = &saved[0].1.readings[0];
    assert_eq!(
        (five.window.id.as_str(), five.window.used_percent),
        ("five_hour", 85.0)
    );
    // A restart loads the saved reading, with the time it was taken.
    let observed = five.observed_at;
    let reopened = vcx.update(|_, cx| cx.new(|cx| AppState::new(Store::open(&path).unwrap(), cx)));
    let restored = reopened.read_with(vcx, |state, _| {
        state.usage(Provider::Claude, None).cloned().unwrap()
    });
    assert_eq!(restored.readings[0].observed_at, observed);
    assert_eq!(restored.readings[0].window.used_percent, 85.0);
}
