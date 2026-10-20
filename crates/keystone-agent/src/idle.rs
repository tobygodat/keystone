//! Idle eviction for Claude chat sessions: the policy, its clock, and the one deadline
//! scheduler a connection owns. What counts as idle is the connection's and driver's
//! business; this module only keeps time.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long a Claude session's process must stay proven idle before it is ended.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long Claude may take to answer a quiescence check before the process is left
/// running.
pub(crate) const QUIESCE_TIMEOUT: Duration = Duration::from_secs(2);

/// Monotonic time for idle deadlines.
pub trait IdleClock: Send + Sync {
    /// Time since some fixed start.
    fn now(&self) -> Duration;
    /// Calls `wake` whenever the time moves other than by itself, so a scheduler
    /// waiting on real time looks again. A clock that follows real time never does.
    fn on_change(&self, _wake: Box<dyn Fn() + Send + Sync>) {}
}

/// Real monotonic time.
pub struct SystemClock(Instant);

impl Default for SystemClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl IdleClock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// A clock that moves only when told, for tests.
#[doc(hidden)]
#[derive(Default)]
pub struct ManualClock {
    now: Mutex<Duration>,
    wakes: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
}

impl ManualClock {
    pub fn advance(&self, by: Duration) {
        *self.now.lock().unwrap() += by;
        for wake in self.wakes.lock().unwrap().iter() {
            wake();
        }
    }
}

impl IdleClock for ManualClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap()
    }

    fn on_change(&self, wake: Box<dyn Fn() + Send + Sync>) {
        self.wakes.lock().unwrap().push(wake);
    }
}

/// When a connection ends idle processes. No Claude profile has a proven quiescence
/// contract today, so connections start without one and every process stays
/// resident; see `docs/design/providers.md`.
#[derive(Clone)]
pub struct IdlePolicy {
    pub timeout: Duration,
    pub clock: Arc<dyn IdleClock>,
}

impl IdlePolicy {
    /// Ends a process proven idle for `timeout` of real time.
    pub fn after(timeout: Duration) -> Self {
        Self {
            timeout,
            clock: Arc::new(SystemClock::default()),
        }
    }
}

/// A session process the connection proved idle, offered to the app to save
/// everything before it, then to confirm with [`crate::Connection::evict_idle`]. It
/// names the process generation, the activity epoch the proof holds for, and the
/// conversation that a later message resumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdleCandidate {
    pub generation: u64,
    pub epoch: u64,
    pub thread: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimerKind {
    /// The proven-idle interval ended: check again, then offer the process.
    Expired,
    /// A quiescence check went unanswered too long: the one at the interval's end if
    /// `confirm`, else the one before it.
    CheckTimedOut { confirm: bool },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Timer {
    pub session: u64,
    pub generation: u64,
    pub epoch: u64,
    pub kind: TimerKind,
}

#[derive(Default)]
struct Pending {
    timers: Vec<(Duration, Timer)>,
    closed: bool,
}

/// Deadlines waiting for the scheduler thread.
#[derive(Default)]
pub(crate) struct Timers {
    pending: Mutex<Pending>,
    changed: Condvar,
}

impl Timers {
    /// Fires `timer` once the clock reaches `at`.
    pub fn at(&self, at: Duration, timer: Timer) {
        self.pending.lock().unwrap().timers.push((at, timer));
        self.changed.notify_all();
    }

    fn wake(&self) {
        let _guard = self.pending.lock().unwrap();
        self.changed.notify_all();
    }

    /// Ends the scheduler thread; later timers never fire.
    pub fn close(&self) {
        self.pending.lock().unwrap().closed = true;
        self.changed.notify_all();
    }
}

/// Starts the thread that fires `timers` by `clock`. `fire` returns false once its
/// connection is gone, which ends the thread too. A stale timer is for `fire` to
/// ignore: timers are never cancelled, only outlived.
pub(crate) fn schedule(
    timers: Arc<Timers>,
    clock: Arc<dyn IdleClock>,
    fire: impl Fn(Timer) -> bool + Send + 'static,
) -> JoinHandle<()> {
    {
        let timers = timers.clone();
        clock.on_change(Box::new(move || timers.wake()));
    }
    std::thread::Builder::new()
        .name("claude-idle".into())
        .spawn(move || {
            let mut pending = timers.pending.lock().unwrap();
            loop {
                if pending.closed {
                    return;
                }
                let now = clock.now();
                let (due, waiting): (Vec<_>, Vec<_>) =
                    pending.timers.drain(..).partition(|(at, _)| *at <= now);
                pending.timers = waiting;
                if !due.is_empty() {
                    drop(pending);
                    for (_, timer) in due {
                        if !fire(timer) {
                            return;
                        }
                    }
                    pending = timers.pending.lock().unwrap();
                    continue;
                }
                let next = pending.timers.iter().map(|(at, _)| *at).min();
                pending = match next {
                    Some(at) => timers.changed.wait_timeout(pending, at - now).unwrap().0,
                    None => timers.changed.wait(pending).unwrap(),
                };
            }
        })
        .expect("spawn idle scheduler")
}
