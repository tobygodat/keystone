use super::*;
use crate::{AppState, Keystone, open_windows};
use gpui::{AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};
use keystone_agent::{Approval, Event, RequestId, TurnStatus};
use keystone_core::{Notifications, Provider, Sounds, Store, WindowId, WorkingDirectory};
use std::ffi::OsStr;

/// A workspace with three sessions, the first selected, and `sounds` saved.
fn seeded(dir: &Path, sounds: Sounds) -> PathBuf {
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    let directory = WorkingDirectory::new(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    for n in 0..3u64 {
        store.create(Provider::Claude, &directory, n).unwrap();
        store.rename(n + 1, &format!("Session {}", n + 1)).unwrap();
    }
    store.select(1).unwrap();
    store.set_sounds(sounds).unwrap();
    path
}

fn on() -> Sounds {
    Sounds {
        enabled: true,
        ..Sounds::default()
    }
}

fn give_player(cx: &mut App) {
    cx.global_mut::<SoundPlayer>().programs = vec![Program::new("pw-play".into(), Kind::PwPlay)];
}

/// Each play so far, as the sound's file name and the volume argument.
fn played(cx: &App) -> Vec<(String, String)> {
    cx.global::<SoundPlayer>()
        .played
        .iter()
        .map(|command| {
            let args: Vec<&OsStr> = command.get_args().collect();
            let file = Path::new(args[args.len() - 1]);
            (
                file.file_name().unwrap().to_string_lossy().into_owned(),
                args[0].to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// Opens the saved windows, active, with a player unless `player` is false.
fn launch(path: &Path, player: bool, cx: &mut TestAppContext) -> Entity<AppState> {
    let store = Store::open(path).unwrap();
    cx.executor().allow_parking();
    let state = cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state = crate::test_support::install(store, crate::test_support::no_providers(), cx);
        if player {
            give_player(cx);
        }
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            crate::windows::open(state.clone(), id, placement, true, false, cx).unwrap();
        }
        state
    });
    let first = cx
        .update(|cx| open_windows(cx))
        .into_iter()
        .find(|v| cx.read(|cx| v.read(cx).window_id) == WindowId::FIRST)
        .unwrap();
    cx.read(|cx| first.read(cx).handle)
        .update(cx, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
    state
}

fn finished() -> Event {
    Event::TurnCompleted {
        status: TurnStatus::Completed,
        error: None,
        stopped: false,
    }
}

fn failed() -> Event {
    Event::Failed {
        message: "could not start".into(),
    }
}

fn approval() -> Event {
    Event::ApprovalRequested(Approval {
        request: RequestId("a".into()),
        item_id: None,
        action: "echo scripted".into(),
        detail: None,
    })
}

fn receive(state: &Entity<AppState>, events: Vec<(u64, Event)>, cx: &mut TestAppContext) {
    state.update(cx, |state, cx| state.receive(Vec::new(), events, cx));
    cx.run_until_parked();
}

fn sounds_on(state: &Entity<AppState>, sounds: Sounds, cx: &mut TestAppContext) {
    state
        .update(cx, |state, cx| state.set_sounds(sounds, cx))
        .unwrap();
}

#[gpui::test]
fn off_by_default_and_once_on_an_unseen_turn_end_plays_its_sound_once(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), Sounds::default()), true, cx);
    receive(&state, vec![(2, finished())], cx);
    assert!(cx.read(played).is_empty(), "sounds are off by default");
    assert_eq!(
        cx.shown_system_notifications().len(),
        1,
        "the notification still went"
    );

    sounds_on(&state, on(), cx);
    receive(&state, vec![(3, finished())], cx);
    assert_eq!(
        cx.read(played),
        [("turn-finished.wav".into(), "--volume=0.343".into())]
    );
    let file = dir.path().join("sounds/turn-finished.wav");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        include_bytes!("../sounds/turn-finished.wav"),
        "the bundled sound is written where the player reads it"
    );

    receive(&state, vec![(3, finished())], cx);
    receive(&state, vec![(1, finished())], cx);
    assert_eq!(
        cx.read(played).len(),
        1,
        "coalesced like its notification, and silent for the session on screen"
    );
}

#[gpui::test]
fn a_batch_plays_only_its_most_urgent_sound(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), on()), true, cx);
    receive(
        &state,
        vec![(3, approval()), (2, failed()), (2, finished())],
        cx,
    );
    receive(&state, vec![(3, finished()), (3, failed())], cx);
    assert_eq!(
        cx.read(played),
        [
            ("approval.wav".into(), "--volume=0.343".into()),
            ("turn-failed.wav".into(), "--volume=0.343".into())
        ],
        "approval over failed over finished, whatever the order"
    );
}

#[gpui::test]
fn pull_request_alerts_notify_but_play_no_sound(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    launch(&seeded(dir.path(), on()), true, cx);
    cx.update(|cx| {
        crate::notifications::offer(vec![(2, Alert::CheckFailed), (3, Alert::Review)], cx)
    });
    cx.run_until_parked();
    assert!(
        cx.read(played).is_empty(),
        "pull request alerts play no sound"
    );
    assert_eq!(
        cx.shown_system_notifications().len(),
        2,
        "they still notify"
    );

    cx.update(|cx| {
        crate::notifications::offer(vec![(2, Alert::Review), (3, Alert::TurnFinished)], cx)
    });
    cx.run_until_parked();
    assert_eq!(
        cx.read(played),
        [("turn-finished.wav".into(), "--volume=0.343".into())],
        "a turn alert in the same batch still plays"
    );
}

#[gpui::test]
fn each_event_switch_holds_its_sound_whatever_notifications_do(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(
        &seeded(
            dir.path(),
            Sounds {
                turn_failed: false,
                ..on()
            },
        ),
        true,
        cx,
    );
    state
        .update(cx, |state, cx| {
            state.set_notifications(
                Notifications {
                    enabled: false,
                    ..Notifications::default()
                },
                cx,
            )
        })
        .unwrap();
    receive(&state, vec![(2, failed())], cx);
    assert!(cx.read(played).is_empty(), "the failed switch is off");
    receive(&state, vec![(3, approval())], cx);
    assert_eq!(
        cx.read(played).len(),
        1,
        "notifications off leave sounds on"
    );
    assert!(cx.shown_system_notifications().is_empty());
}

#[gpui::test]
fn without_a_player_sounds_stay_off(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = launch(&seeded(dir.path(), on()), false, cx);
    receive(&state, vec![(2, finished())], cx);
    assert!(cx.read(played).is_empty());
    assert!(!dir.path().join("sounds").exists());
}

#[test]
fn each_player_gets_the_volume_on_its_own_scale_in_c_number_format() {
    let file = Path::new("/data/sounds/approval.wav");
    let args = |program: Program, volume| {
        let command = program.command(file, volume);
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            [(OsStr::new("LC_ALL"), Some(OsStr::new("C")))]
        );
        command
            .get_args()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let program = |kind| Program::new("player".into(), kind);
    assert_eq!(
        args(program(Kind::PwPlay), 100),
        ["--volume=1.000", "/data/sounds/approval.wav"]
    );
    assert_eq!(
        args(program(Kind::Paplay), 50),
        ["--volume=32768", "/data/sounds/approval.wav"]
    );
    assert_eq!(
        args(program(Kind::Afplay), 50),
        ["-v", "0.125", "/data/sounds/approval.wav"]
    );
    let test = Program {
        path: "python3".into(),
        leading: vec!["record.py".into()],
        kind: Kind::PwPlay,
    };
    assert_eq!(
        args(test, 70),
        ["record.py", "--volume=0.343", "/data/sounds/approval.wav"]
    );
}

fn open_settings<'a>(
    path: &Path,
    player: bool,
    cx: &'a mut TestAppContext,
) -> (Entity<Keystone>, &'a mut VisualTestContext) {
    let store = Store::open(path).unwrap();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let view = Keystone::new(
            cx.new(|cx| {
                AppState::new(store, cx).with_test_programs(crate::test_support::no_providers())
            }),
            false,
            window,
            cx,
        );
        view.focus.focus(window, cx);
        view
    });
    if player {
        cx.update(|_, cx| give_player(cx));
    }
    cx.simulate_keystrokes("secondary-,");
    cx.run_until_parked();
    click(cx, "settings-section-sounds");
    (view, cx)
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} rendered"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

fn rest(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(PREVIEW_DELAY);
    cx.run_until_parked();
}

fn saved(path: &Path) -> Sounds {
    Store::open(path)
        .unwrap()
        .workspace()
        .preferences()
        .sounds()
}

fn previews(cx: &mut VisualTestContext) -> Vec<(String, String)> {
    cx.update(|_, cx| played(cx))
}

#[gpui::test]
fn the_sounds_switches_save_and_play_what_they_turn_on(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(
        dir.path(),
        Sounds {
            turn_finished: false,
            ..Sounds::default()
        },
    );
    let (_view, cx) = open_settings(&path, true, cx);

    click(cx, "settings-sound-approval");
    assert!(
        !saved(&path).enabled && saved(&path).approval,
        "an event switch does nothing while sounds are off"
    );
    click(cx, "settings-sounds");
    assert!(saved(&path).enabled);
    assert!(
        previews(cx).is_empty(),
        "the preview waits for the setting to rest"
    );
    rest(cx);
    assert_eq!(
        previews(cx),
        [("approval.wav".into(), "--volume=0.343".into())],
        "turning sounds on plays the first event that has one"
    );

    click(cx, "settings-sound-approval");
    assert!(!saved(&path).approval);
    rest(cx);
    assert_eq!(previews(cx).len(), 1, "turning one off is silent");
    click(cx, "settings-sound-approval");
    click(cx, "settings-sound-approval");
    rest(cx);
    assert_eq!(
        previews(cx).len(),
        1,
        "turned off before it rests, its preview never plays"
    );
    click(cx, "settings-sound-turn-failed");
    click(cx, "settings-sound-turn-failed");
    rest(cx);
    assert_eq!(
        previews(cx)[1..],
        [("turn-failed.wav".into(), "--volume=0.343".into())]
    );

    let track = cx.debug_bounds("settings-sound-volume").unwrap();
    for at in [0.1, 0.2, 0.3] {
        cx.simulate_click(
            gpui::point(track.left() + track.size.width * at, track.center().y),
            Modifiers::none(),
        );
        cx.run_until_parked();
    }
    rest(cx);
    assert_eq!(saved(&path).volume, 30);
    assert_eq!(
        previews(cx)[2..],
        [("turn-failed.wav".into(), "--volume=0.027".into())],
        "steps in a row play once, at the volume they land on"
    );
}

#[gpui::test]
fn without_a_player_the_page_names_one_and_hides_the_rest(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), on());
    let (_view, cx) = open_settings(&path, false, cx);
    for control in [
        "settings-sounds",
        "settings-sound-volume",
        "settings-sound-turn-finished",
    ] {
        assert!(cx.debug_bounds(control).is_none(), "{control} is hidden");
    }
    assert_eq!(saved(&path), on(), "the saved choice is kept");
}

#[test]
fn the_next_player_takes_a_sound_the_last_one_failed_or_held() {
    let program = |path: &str, leading: &[&str]| Program {
        path: path.into(),
        leading: leading.iter().map(|arg| arg.to_string()).collect(),
        kind: Kind::Afplay,
    };
    let file = Path::new("/nonexistent/turn-finished.wav");
    let limit = Duration::from_millis(300);
    let failing = program("false", &[]);
    let holding = program("sh", &["-c", "exec sleep 30"]);
    let playing = program("true", &[]);
    let started = std::time::Instant::now();
    assert_eq!(
        play_first(
            &[failing.clone(), holding, playing.clone()],
            file,
            50,
            limit
        ),
        Some(2)
    );
    let took = started.elapsed();
    assert!(
        took >= limit && took < Duration::from_secs(5),
        "a held player is stopped at the limit: {took:?}"
    );
    assert_eq!(
        play_first(&[playing, failing.clone()], file, 50, limit),
        Some(0)
    );
    assert_eq!(play_first(&[failing], file, 50, limit), None);
}

#[test]
fn paplay_is_tried_before_pw_play_and_either_alone_plays() {
    let found = |names: &'static [&'static str]| {
        Program::installed(|name| {
            names
                .contains(&name)
                .then(|| format!("/usr/bin/{name}").into())
        })
        .into_iter()
        .map(|program| (program.path, program.kind))
        .collect::<Vec<_>>()
    };
    if cfg!(target_os = "macos") {
        assert_eq!(
            found(&["afplay"]),
            [("/usr/bin/afplay".into(), Kind::Afplay)]
        );
        return;
    }
    assert_eq!(
        found(&["pw-play", "paplay"]),
        [
            ("/usr/bin/paplay".into(), Kind::Paplay),
            ("/usr/bin/pw-play".into(), Kind::PwPlay)
        ]
    );
    assert_eq!(
        found(&["pw-play"]),
        [("/usr/bin/pw-play".into(), Kind::PwPlay)]
    );
    assert!(found(&[]).is_empty());
}
