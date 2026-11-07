//! A short sound for each event a notification announces, played by the platform's
//! own command-line player, so Keystone links no audio library: paplay, else pw-play,
//! on Linux and afplay on macOS. paplay comes first because it plays through
//! PulseAudio and through PipeWire's pulse server alike, and fails at once without
//! either; pw-play plays what paplay could not. With no player, sounds stay off. The
//! sounds are bundled and written to the data directory, where the player reads them.

use crate::notifications::Alert;
use gpui::{App, Task};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(all(test, feature = "ui-tests"))]
#[path = "sounds_tests.rs"]
mod tests;

/// How long a setting rests before its preview plays, so a volume drag plays once, at
/// the volume it lands on.
const PREVIEW_DELAY: Duration = Duration::from_millis(250);

/// How long a player may take over a sound of under half a second before the next
/// player gets it.
#[cfg(not(test))]
const PLAY_LIMIT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    PwPlay,
    Paplay,
    Afplay,
}

#[derive(Clone, Debug, PartialEq)]
struct Program {
    path: PathBuf,
    leading: Vec<String>,
    kind: Kind,
}

impl Program {
    fn new(path: PathBuf, kind: Kind) -> Self {
        Self {
            path,
            leading: Vec::new(),
            kind,
        }
    }

    /// The installed players, in the order to try them. A debug build runs
    /// `KEYSTONE_TEST_SOUND_COMMAND`, split like a shell, with paplay's arguments
    /// instead, so a scripted run records what would play; set but empty or
    /// unreadable, it finds none. Unit tests never find one.
    fn find() -> Vec<Self> {
        if cfg!(test) {
            return Vec::new();
        }
        #[cfg(debug_assertions)]
        if let Ok(command) = std::env::var("KEYSTONE_TEST_SOUND_COMMAND") {
            let command = shlex::split(&command).unwrap_or_default();
            return command
                .split_first()
                .map(|(path, leading)| Self {
                    path: path.into(),
                    leading: leading.to_vec(),
                    kind: Kind::Paplay,
                })
                .into_iter()
                .collect();
        }
        Self::installed(keystone_agent::find::program)
    }

    /// This platform's players that `find` locates, in the order to try them.
    fn installed(find: impl Fn(&str) -> Option<PathBuf>) -> Vec<Self> {
        let players: &[_] = if cfg!(target_os = "macos") {
            &[("afplay", Kind::Afplay)]
        } else {
            &[("paplay", Kind::Paplay), ("pw-play", Kind::PwPlay)]
        };
        players
            .iter()
            .filter_map(|&(name, kind)| find(name).map(|path| Self::new(path, kind)))
            .collect()
    }

    /// The command that plays `file` at `volume` percent, on a curve that sounds even:
    /// PulseAudio's volume is cubic already, and the others take a linear gain. pw-play
    /// reads its volume in the locale's number format, so the command runs in C's.
    fn command(&self, file: &Path, volume: u8) -> Command {
        let fraction = f32::from(volume.min(100)) / 100.;
        let gain = fraction.powi(3);
        let mut command = Command::new(&self.path);
        command.args(&self.leading);
        match self.kind {
            Kind::PwPlay => command.arg(format!("--volume={gain:.3}")),
            Kind::Paplay => command.arg(format!("--volume={}", (fraction * 65536.).round() as u32)),
            Kind::Afplay => command.arg("-v").arg(format!("{gain:.3}")),
        };
        command
            .arg(file)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }
}

/// The player found at launch, the folder its sounds are written to, and the preview
/// waiting for a setting to rest.
pub(crate) struct SoundPlayer {
    programs: Vec<Program>,
    dir: PathBuf,
    preview: Option<Task<()>>,
    /// Each command a play would have run, in place of running it.
    #[cfg(test)]
    played: Vec<Command>,
}

impl gpui::Global for SoundPlayer {}

impl SoundPlayer {
    pub(crate) fn install(data_dir: &Path, cx: &mut App) {
        cx.set_global(Self {
            programs: Program::find(),
            dir: data_dir.join("sounds"),
            preview: None,
            #[cfg(test)]
            played: Vec::new(),
        });
    }

    /// Whether this computer has a player, without which sounds stay off.
    pub(crate) fn available(cx: &App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|player| !player.programs.is_empty())
    }

    /// Plays `alert`'s sound at `volume` percent.
    pub(crate) fn play(alert: Alert, volume: u8, cx: &mut App) {
        if cx.has_global::<Self>() {
            cx.global_mut::<Self>().start(alert, volume);
        }
    }

    /// Plays `alert`'s sound once the setting just changed rests, replacing any
    /// preview still waiting; `None` only drops that one.
    pub(crate) fn preview(alert: Option<Alert>, volume: u8, cx: &mut App) {
        if !cx.has_global::<Self>() {
            return;
        }
        let task = alert.map(|alert| {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(PREVIEW_DELAY).await;
                cx.update(|cx| Self::play(alert, volume, cx));
            })
        });
        cx.global_mut::<Self>().preview = task;
    }

    /// Writes the sound and runs the players off the UI thread.
    fn start(&mut self, alert: Alert, volume: u8) {
        let Some(sound) = bundled(alert) else {
            return;
        };
        if self.programs.is_empty() || volume == 0 {
            return;
        }
        let (programs, dir) = (self.programs.clone(), self.dir.clone());
        #[cfg(test)]
        self.played
            .push(programs[0].command(&write(&dir, sound).unwrap(), volume));
        #[cfg(not(test))]
        std::thread::spawn(move || match write(&dir, sound) {
            Ok(file) => {
                play_first(&programs, &file, volume, PLAY_LIMIT);
            }
            Err(error) => eprintln!("Keystone could not write its sounds: {error}"),
        });
    }
}

/// Plays `file` with the first of `programs` that finishes it successfully within
/// `limit`, stopping any that does not, and returns which one did.
#[cfg_attr(all(test, not(feature = "ui-tests")), allow(dead_code))]
fn play_first(programs: &[Program], file: &Path, volume: u8, limit: Duration) -> Option<usize> {
    programs.iter().position(|program| {
        let mut child = match program.command(file, volume).spawn() {
            Ok(child) => child,
            Err(error) => {
                eprintln!("Keystone could not run {}: {error}", program.path.display());
                return false;
            }
        };
        let deadline = std::time::Instant::now() + limit;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    })
}

/// `alert`'s bundled sound, as its file name and bytes. Pull request alerts have none.
fn bundled(alert: Alert) -> Option<(&'static str, &'static [u8])> {
    match alert {
        Alert::TurnFinished => Some((
            "turn-finished.wav",
            include_bytes!("../sounds/turn-finished.wav"),
        )),
        Alert::Approval | Alert::Question => {
            Some(("approval.wav", include_bytes!("../sounds/approval.wav")))
        }
        Alert::TurnFailed => Some((
            "turn-failed.wav",
            include_bytes!("../sounds/turn-failed.wav"),
        )),
        Alert::CheckFailed | Alert::Review => None,
    }
}

/// The sound's file in `dir`, written from the bundled copy unless it already matches.
/// It is renamed into place, so a player never reads half of it.
fn write(dir: &Path, (name, bytes): (&str, &[u8])) -> std::io::Result<PathBuf> {
    let path = dir.join(name);
    if std::fs::read(&path).ok().as_deref() != Some(bytes) {
        std::fs::create_dir_all(dir)?;
        let mut partial = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut partial, bytes)?;
        partial.persist(&path)?;
    }
    Ok(path)
}
