//! What Keystone knows of one device's power, whether Keystone booted it, and how each
//! answer from the host changes that. Nothing here touches GPUI or the host, so every
//! transition is tested on its own.

use keystone_agent::device::{BootState, DeviceError, Power, Start};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Life {
    /// Keystone asked the host to start it and has no answer yet.
    Starting,
    /// Booting, and Keystone waits for it through boot watch `watch`.
    Booting {
        watch: u64,
    },
    Booted,
    /// Keystone stopped waiting on a boot that had not finished.
    Stalled,
    /// The host says it is not running.
    Off,
    /// Keystone is shutting it down; `was` comes back if the shutdown fails.
    ShuttingDown {
        was: Box<Life>,
    },
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Origin {
    /// Keystone booted it, so Keystone shuts it down once no session holds it.
    Keystone,
    Elsewhere,
}

/// One device's [`Life`], who booted it, and what Keystone waits on the host for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Lifecycle {
    pub(crate) life: Life,
    pub(crate) origin: Origin,
    /// The start Keystone asked the host for and has no answer to yet.
    pub(crate) asked: Option<u64>,
    /// Why Keystone's latest shutdown failed. Until a listing, an open or a close
    /// tries again, no session holding the device doesn't start another.
    pub(crate) refused: Option<String>,
}

/// What an open still waiting is told.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Opened {
    Booted,
    /// Keystone stopped waiting; the boot may still finish.
    Booting,
    Stopped,
    Failed(String),
}

/// What becomes of a device once a change settles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Settle {
    /// A start or a shutdown has to answer first.
    Wait,
    /// No session holds it and Keystone booted it, so Keystone shuts it down now.
    ShutDown,
    /// The closing sessions let go now. With `forget`, so does Keystone.
    LetGo { forget: bool },
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            life: Life::Off,
            origin: Origin::Elsewhere,
            asked: None,
            refused: None,
        }
    }
}

impl Lifecycle {
    fn set(&mut self, life: Life) {
        // Once it is off, a later boot is no longer Keystone's to undo.
        if life == Life::Off {
            self.origin = Origin::Elsewhere;
        }
        self.life = life;
    }

    /// A session opens the device. Returns whether Keystone asks the host to start it,
    /// as start `ask`; otherwise the open joins the start, boot or shutdown under way.
    pub(crate) fn open(&mut self, ask: u64) -> bool {
        self.refused = None;
        if self.asked.is_some()
            || matches!(self.life, Life::Booting { .. } | Life::ShuttingDown { .. })
        {
            return false;
        }
        // A running device stays running while the host confirms it, so its frames and
        // input carry on.
        if self.life != Life::Booted {
            self.life = Life::Starting;
        }
        self.asked = Some(ask);
        true
    }

    /// The host answered start `ask`. Returns whether Keystone waits for the boot
    /// through boot watch `watch`.
    pub(crate) fn started(
        &mut self,
        ask: u64,
        result: Result<Start, DeviceError>,
        watch: u64,
    ) -> bool {
        if self.asked != Some(ask) {
            return false;
        }
        self.asked = None;
        match result {
            Ok(Start::Booted) => self.set(Life::Booted),
            Ok(start) => {
                if start == Start::Started {
                    self.origin = Origin::Keystone;
                }
                self.set(Life::Booting { watch });
                return true;
            }
            // The host couldn't say, which doesn't contradict a device seen running.
            Err(_) if self.life == Life::Booted => {}
            Err(error) => self.set(Life::Failed(error.to_string())),
        }
        false
    }

    /// Boot watch `watch` ended, maybe while Keystone was shutting the device down.
    /// Returns whether that changed anything. A device another start runs now is no
    /// longer Keystone's, and stays booting while a new watch `watch` follows it.
    pub(crate) fn waited(&mut self, watch: u64, result: Result<BootState, DeviceError>) -> bool {
        let life = match &mut self.life {
            Life::ShuttingDown { was } => was.as_mut(),
            life => life,
        };
        if *life != (Life::Booting { watch }) {
            return false;
        }
        *life = match result {
            Ok(BootState::Elsewhere) => {
                self.origin = Origin::Elsewhere;
                return true;
            }
            Ok(BootState::Booted) => Life::Booted,
            Ok(BootState::Booting) => Life::Stalled,
            Err(DeviceError::NotBooted) => Life::Off,
            Err(error) => Life::Failed(error.to_string()),
        };
        if self.life == Life::Off {
            self.origin = Origin::Elsewhere;
        }
        true
    }

    /// A capture, screenshot or action came back: `Ok` when the host reached the
    /// device. Returns whether that changed anything.
    pub(crate) fn seen(&mut self, result: Result<(), &DeviceError>) -> bool {
        let life = match (result, &self.life) {
            (Ok(()), Life::Off | Life::Stalled | Life::Failed(_)) => Life::Booted,
            (Err(DeviceError::NotBooted), Life::Booted) => Life::Off,
            _ => return false,
        };
        self.set(life);
        true
    }

    /// A listing found the device at `power`, or didn't list it. Returns whether that
    /// changed anything, a refused shutdown being tried again included. While a session
    /// `held` it, a refused shutdown isn't tried again, so its reason stays.
    pub(crate) fn listed(&mut self, power: Option<Power>, held: bool) -> bool {
        let retry = !held && self.refused.take().is_some();
        let life = match (&self.life, power) {
            (Life::Off | Life::Stalled | Life::Failed(_), Some(Power::Booted)) => Life::Booted,
            (Life::Stalled, Some(Power::Shutdown)) => Life::Off,
            _ => return retry,
        };
        self.set(life);
        true
    }

    /// A session lets go of it, which tries a refused shutdown again.
    pub(crate) fn close(&mut self) {
        self.refused = None;
    }

    /// Decides what becomes of the device; `held` says whether a session that isn't
    /// closing holds it.
    pub(crate) fn settle(&mut self, held: bool) -> Settle {
        match &self.life {
            Life::ShuttingDown { .. } => Settle::Wait,
            _ if held => Settle::LetGo { forget: false },
            _ if self.asked.is_some() => Settle::Wait,
            Life::Off | Life::Starting => Settle::LetGo { forget: true },
            _ if self.origin == Origin::Elsewhere => Settle::LetGo { forget: true },
            _ if self.refused.is_some() => Settle::LetGo { forget: false },
            _ => {
                let was = std::mem::replace(&mut self.life, Life::Off);
                self.life = Life::ShuttingDown { was: Box::new(was) };
                Settle::ShutDown
            }
        }
    }

    /// Keystone's shutdown answered. `None` when Keystone wasn't shutting it down;
    /// otherwise whether it shut the device down, or the error to show when it failed,
    /// the device keeping the life it had. A boot watch that said meanwhile that another
    /// launch runs the device leaves it that life too: the shutdown left that launch
    /// running, so the device isn't off.
    pub(crate) fn shut_down(
        &mut self,
        result: Result<(), DeviceError>,
    ) -> Option<Result<bool, String>> {
        let Life::ShuttingDown { was } = &mut self.life else {
            return None;
        };
        let was = std::mem::replace(was.as_mut(), Life::Off);
        Some(match result {
            // Only a boot watch's Elsewhere changes who booted it during a shutdown.
            Ok(()) if self.origin == Origin::Elsewhere => {
                self.set(was);
                Ok(false)
            }
            Ok(()) => {
                self.set(Life::Off);
                Ok(true)
            }
            Err(error) => {
                let message = format!("Couldn't shut the device down: {error}");
                self.set(was);
                self.refused = Some(message.clone());
                Err(message)
            }
        })
    }

    /// What an open waiting on the device is told now, or `None` while a start,
    /// boot or shutdown is still to answer.
    pub(crate) fn opened(&self) -> Option<Opened> {
        if self.asked.is_some() {
            return None;
        }
        match &self.life {
            Life::Booted => Some(Opened::Booted),
            Life::Stalled => Some(Opened::Booting),
            Life::Off => Some(Opened::Stopped),
            Life::Failed(reason) => Some(Opened::Failed(reason.clone())),
            Life::Starting | Life::Booting { .. } | Life::ShuttingDown { .. } => None,
        }
    }

    /// Whether Keystone booted it and it may still run, so Quit shuts it down.
    pub(crate) fn owned(&self) -> bool {
        self.origin == Origin::Keystone && self.life != Life::Off
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn booted_by_keystone() -> Lifecycle {
        let mut device = Lifecycle::default();
        assert!(device.open(1));
        assert!(device.started(1, Ok(Start::Started), 2));
        assert!(device.waited(2, Ok(BootState::Booted)));
        device
    }

    #[test]
    fn a_failed_shutdown_keeps_the_life_the_device_had_and_never_claims_a_boot() {
        let mut device = booted_by_keystone();
        assert_eq!(device.settle(false), Settle::ShutDown);
        let refused = DeviceError::Failed("simctl refused".into());
        assert_eq!(
            device.shut_down(Err(refused.clone())),
            Some(Err("Couldn't shut the device down: simctl refused".into()))
        );
        assert_eq!(device.life, Life::Booted);
        assert_eq!(device.origin, Origin::Keystone);

        let mut booting = Lifecycle::default();
        booting.open(1);
        booting.started(1, Ok(Start::Started), 2);
        assert_eq!(booting.settle(false), Settle::ShutDown);
        assert!(booting.shut_down(Err(refused)).unwrap().is_err());
        assert_eq!(booting.life, Life::Booting { watch: 2 }, "not Running");
        assert!(
            booting.waited(2, Ok(BootState::Booted)),
            "the boot watch still counts"
        );
        assert_eq!(booting.life, Life::Booted);
    }

    #[test]
    fn a_boot_watch_ending_during_a_shutdown_is_what_a_failed_shutdown_restores() {
        let mut device = Lifecycle::default();
        device.open(1);
        device.started(1, Ok(Start::Started), 2);
        device.settle(false);
        assert!(device.waited(2, Ok(BootState::Booting)));
        device.shut_down(Err(DeviceError::TimedOut));
        assert_eq!(device.life, Life::Stalled);
        assert_eq!(device.shut_down(Ok(())), None, "no shutdown is under way");
    }

    #[test]
    fn a_refused_shutdown_is_not_retried_until_a_listing_an_open_or_a_close() {
        let mut device = booted_by_keystone();
        device.settle(false);
        device.shut_down(Err(DeviceError::TimedOut));
        assert_eq!(device.settle(false), Settle::LetGo { forget: false });
        assert!(device.owned(), "Quit still shuts it down");
        assert!(
            !device.listed(Some(Power::Booted), true),
            "a listing keeps the reason while a session holds it"
        );
        assert!(device.refused.is_some());
        assert!(
            device.listed(Some(Power::Booted), false),
            "a listing tries again once none does"
        );
        assert_eq!(device.settle(false), Settle::ShutDown);
        assert_eq!(device.shut_down(Ok(())), Some(Ok(true)));
        assert_eq!((device.life.clone(), device.owned()), (Life::Off, false));
        assert_eq!(device.settle(false), Settle::LetGo { forget: true });

        let mut closed = booted_by_keystone();
        closed.settle(false);
        closed.shut_down(Err(DeviceError::TimedOut));
        closed.close();
        assert_eq!(closed.settle(false), Settle::ShutDown);
    }

    #[test]
    fn reopening_a_running_device_keeps_it_running_while_the_host_confirms() {
        let mut device = Lifecycle::default();
        device.open(1);
        device.started(1, Ok(Start::Booted), 2);
        assert!(device.open(3), "the host is asked again");
        assert_eq!(device.life, Life::Booted);
        assert_eq!(device.opened(), None, "an open waits for the host's word");
        assert!(!device.open(4), "a second open joins that start");
        assert!(!device.started(1, Ok(Start::Started), 5), "an old start");
        assert!(!device.started(3, Err(DeviceError::TimedOut), 5));
        assert_eq!(device.life, Life::Booted);
        assert_eq!(device.opened(), Some(Opened::Booted));
        assert_eq!(device.origin, Origin::Elsewhere);

        device.open(6);
        assert!(
            device.started(6, Ok(Start::Started), 7),
            "it had stopped, so the start booted it"
        );
        assert_eq!(
            (device.life.clone(), device.origin),
            (Life::Booting { watch: 7 }, Origin::Keystone)
        );
    }

    #[test]
    fn nothing_shuts_down_while_a_start_is_still_to_answer() {
        let mut device = booted_by_keystone();
        device.open(5);
        assert_eq!(device.settle(false), Settle::Wait);
        assert_eq!(device.settle(true), Settle::LetGo { forget: false });
        device.started(5, Ok(Start::Booted), 6);
        assert_eq!(device.settle(false), Settle::ShutDown);
    }

    #[test]
    fn a_device_off_or_never_keystones_is_forgotten_once_no_session_holds_it() {
        let mut elsewhere = Lifecycle::default();
        elsewhere.open(1);
        elsewhere.started(1, Ok(Start::Booted), 2);
        assert_eq!(elsewhere.settle(false), Settle::LetGo { forget: true });

        let mut stopped = booted_by_keystone();
        assert!(stopped.seen(Err(&DeviceError::NotBooted)));
        assert_eq!(stopped.origin, Origin::Elsewhere);
        assert!(stopped.seen(Ok(())), "booted again outside Keystone");
        assert_eq!(stopped.settle(false), Settle::LetGo { forget: true });
    }

    #[test]
    fn a_boot_keystone_started_that_ends_off_is_no_longer_keystones() {
        let mut device = Lifecycle::default();
        device.open(1);
        device.started(1, Ok(Start::Started), 2);
        assert!(device.waited(2, Err(DeviceError::NotBooted)));
        assert_eq!(
            (device.life.clone(), device.origin),
            (Life::Off, Origin::Elsewhere)
        );
        assert!(!device.owned());
    }

    #[test]
    fn a_boot_another_start_took_over_is_never_keystones_to_shut_down() {
        let mut device = Lifecycle::default();
        device.open(1);
        device.started(1, Ok(Start::Started), 2);
        assert!(device.waited(2, Ok(BootState::Elsewhere)));
        assert_eq!(
            (device.life.clone(), device.origin),
            (Life::Booting { watch: 2 }, Origin::Elsewhere),
            "still booting, followed by the same watch"
        );
        assert!(!device.owned());
        assert!(device.waited(2, Ok(BootState::Booted)));
        assert_eq!(device.settle(false), Settle::LetGo { forget: true });
    }

    #[test]
    fn a_shutdown_that_another_launch_outlived_leaves_the_device_booting_not_off() {
        let mut device = Lifecycle::default();
        device.open(1);
        device.started(1, Ok(Start::Started), 2);
        assert_eq!(device.settle(false), Settle::ShutDown);
        assert!(device.waited(2, Ok(BootState::Elsewhere)));
        assert_eq!(
            device.shut_down(Ok(())),
            Some(Ok(false)),
            "Keystone didn't shut it down"
        );
        assert_eq!(
            (device.life.clone(), device.origin),
            (Life::Booting { watch: 2 }, Origin::Elsewhere),
            "still booting elsewhere, followed by the same watch"
        );
        assert_eq!(device.settle(false), Settle::LetGo { forget: true });
    }

    #[test]
    fn an_open_is_answered_once_the_life_settles_it() {
        let mut device = Lifecycle::default();
        device.open(1);
        assert_eq!(device.opened(), None);
        device.started(1, Err(DeviceError::UnknownDevice), 2);
        assert!(matches!(device.opened(), Some(Opened::Failed(_))));
        device.open(3);
        device.started(3, Ok(Start::Booting), 4);
        assert_eq!(device.opened(), None);
        device.waited(4, Ok(BootState::Booting));
        assert_eq!(device.opened(), Some(Opened::Booting));
    }
}
