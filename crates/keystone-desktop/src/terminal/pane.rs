//! One shell: its PTY, the IO thread alacritty runs for it, and the emulated screen.

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, State};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Cell as GridCell;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Pty};
use futures::channel::mpsc::UnboundedSender;
use gpui::{Bounds, FocusHandle, Pixels, WindowId};
use keystone_agent::host::{Root, Tree, unreaped};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SCROLLBACK: usize = 10_000;

/// How long a closed shell gets to exit after SIGHUP before SIGKILL.
pub(crate) const GRACE: Duration = Duration::from_secs(1);

/// The program each pane runs: the user's login shell unless a test chose another.
#[derive(Clone, Debug)]
pub(crate) struct Shell {
    pub program: String,
    pub args: Vec<String>,
    /// Set in the shell's environment before the variables Keystone keeps out of it
    /// are removed.
    pub env: Vec<(String, String)>,
}

impl Shell {
    pub(crate) fn login() -> Self {
        let fallback = if cfg!(target_os = "macos") {
            "/bin/zsh"
        } else {
            "/bin/bash"
        };
        let program = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| fallback.into());
        Self {
            program,
            args: vec!["-l".into()],
            env: Vec::new(),
        }
    }

    /// `env -u` for each variable Keystone keeps out of the shell, then the shell.
    /// The PTY only adds variables, so the shell starts through `env` to drop them.
    fn command(&self) -> tty::Shell {
        let mut args = Vec::new();
        let inherited = std::env::vars_os().filter_map(|(key, _)| key.into_string().ok());
        let given = self.env.iter().map(|(key, _)| key.clone());
        let mut names: Vec<String> = inherited.chain(given).filter(|key| withheld(key)).collect();
        names.extend(["ALACRITTY_WINDOW_ID", "WINDOWID"].map(String::from));
        names.sort();
        names.dedup();
        for name in names {
            args.push("-u".into());
            args.push(name);
        }
        args.push(self.program.clone());
        args.extend(self.args.iter().cloned());
        tty::Shell::new("/usr/bin/env".into(), args)
    }
}

/// Whether the shell's environment leaves out `key`: Keystone's own settings, and the
/// multiplexer variables that would make programs in the shell think they run inside
/// the tmux, screen or Zellij that started Keystone. T3 Code's list, for Keystone.
fn withheld(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.starts_with("KEYSTONE_")
        || key.starts_with("ZELLIJ")
        || matches!(key.as_str(), "TMUX" | "TMUX_PANE" | "STY" | "WINDOW")
}

/// A screen of `columns` by `lines` cells, each `cell` pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GridSize {
    pub columns: usize,
    pub lines: usize,
    pub cell: gpui::Size<Pixels>,
}

impl Default for GridSize {
    fn default() -> Self {
        Self {
            columns: 80,
            lines: 24,
            cell: gpui::size(gpui::px(8.), gpui::px(16.)),
        }
    }
}

impl GridSize {
    fn window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.lines as u16,
            num_cols: self.columns as u16,
            cell_width: f32::from(self.cell.width) as u16,
            cell_height: f32::from(self.cell.height) as u16,
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// Forwards the IO thread's events to the UI thread, tagged with the pane. Output
/// comes in reads of up to 64 KiB, each followed by a wakeup, so a flood would queue
/// one per read: instead one queued wakeup stands for all output until the UI applies
/// it. Events Keystone has no use for are never queued. Replies a program asked for and
/// the shell's exit always are.
#[derive(Clone)]
pub(crate) struct Listener {
    pane: u64,
    events: UnboundedSender<(u64, Event)>,
    woken: Arc<AtomicBool>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup if self.woken.swap(true, Ordering::AcqRel) => return,
            // No title, pointer shape, blink or bell is shown, and no program reads the
            // clipboard.
            Event::Title(_)
            | Event::ResetTitle
            | Event::MouseCursorDirty
            | Event::CursorBlinkingChange
            | Event::Bell
            | Event::ClipboardLoad(..) => return,
            _ => {}
        }
        let _ = self.events.unbounded_send((self.pane, event));
    }
}

/// Where a window last painted the grid, and the grid that would fill it.
#[derive(Clone, Copy)]
pub(crate) struct View {
    pub bounds: Bounds<Pixels>,
    pub fits: GridSize,
    /// The screen's size when the window last painted it, so a test can tell a stale
    /// frame.
    #[cfg(test)]
    pub painted: GridSize,
}

type Io = JoinHandle<(EventLoop<Pty, Listener>, State)>;

pub(crate) struct Pane {
    pub id: u64,
    pub term: Arc<FairMutex<Term<Listener>>>,
    pub pid: u32,
    pub focus: FocusHandle,
    /// Each window showing the pane, so a pointer maps to a cell through that
    /// window's own layout.
    views: RefCell<HashMap<WindowId, View>>,
    /// Wheel movement short of a whole line, carried to the next event.
    pub scroll_remainder: Cell<f32>,
    /// The visible cells, copied for each paint so the screen's lock is held only for
    /// the copy. Kept between frames to reuse its allocation.
    pub cells: Cell<Vec<(alacritty_terminal::index::Point, GridCell)>>,
    /// What an input method is composing, shown at the cursor until it commits.
    pub marked: Option<String>,
    /// The shell exited by itself, so it and its group are not signalled.
    pub exited: bool,
    /// A wakeup is queued for the UI. Cleared as the UI applies it, so output after
    /// that queues another.
    woken: Arc<AtomicBool>,
    sender: EventLoopSender,
    size: Cell<GridSize>,
    /// The PTY's controller side, open until the IO thread is joined.
    master: RawFd,
    shell: Option<Root>,
    /// The IO thread. Joining it drops the PTY, which waits for the shell.
    io: Option<Io>,
}

impl Pane {
    pub(crate) fn spawn(
        id: u64,
        shell: &Shell,
        directory: &Path,
        events: UnboundedSender<(u64, Event)>,
        focus: FocusHandle,
    ) -> std::io::Result<Self> {
        let size = GridSize::default();
        let mut env = HashMap::from([
            ("TERM".into(), "xterm-256color".into()),
            ("COLORTERM".into(), "truecolor".into()),
        ]);
        env.extend(shell.env.iter().cloned());
        let options = tty::Options {
            shell: Some(shell.command()),
            working_directory: Some(directory.to_path_buf()),
            drain_on_exit: false,
            env,
        };
        let pty = tty::new(&options, size.window_size(), id)?;
        let pid = pty.child().id();
        let master = pty.file().as_raw_fd();
        let root = pin(pid);
        if root.is_none() {
            eprintln!(
                "keystone: could not pin terminal shell {pid}, so closing its pane signals it and its group by PID"
            );
        }
        let woken = Arc::new(AtomicBool::new(false));
        let listener = Listener {
            pane: id,
            events,
            woken: woken.clone(),
        };
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, listener.clone())));
        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let sender = event_loop.channel();
        let io = event_loop.spawn();
        Ok(Self {
            id,
            term,
            pid,
            focus,
            views: RefCell::default(),
            scroll_remainder: Cell::new(0.),
            cells: Cell::default(),
            marked: None,
            exited: false,
            woken,
            sender,
            size: Cell::new(size),
            master,
            shell: root,
            io: Some(io),
        })
    }

    /// Takes the queued wakeup, so the next output queues another.
    pub(crate) fn wake(&self) {
        self.woken.store(false, Ordering::Release);
    }

    /// Sends what the user typed: the view returns to the bottom and the selection
    /// clears, as in other terminals.
    pub(crate) fn typed(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        {
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
        }
        self.write(bytes);
    }

    pub(crate) fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.sender.send(Msg::Input(bytes.into()));
    }

    /// Records that `window` paints the pane in `bounds`, where `fits` would fill it,
    /// and fits the screen and the PTY to it and the other windows in `showing`.
    /// Returns whether the size changed, so the other windows' frames are stale.
    pub(crate) fn paint_in(
        &self,
        window: WindowId,
        bounds: Bounds<Pixels>,
        fits: GridSize,
        showing: &[WindowId],
    ) -> bool {
        self.views.borrow_mut().insert(
            window,
            View {
                bounds,
                fits,
                #[cfg(test)]
                painted: fits,
            },
        );
        let resized = self.fit(|id| id == window || showing.contains(&id));
        #[cfg(test)]
        if let Some(view) = self.views.borrow_mut().get_mut(&window) {
            view.painted = self.size.get();
        }
        resized
    }

    /// Forgets the windows `keep` rejects and fits the screen and the PTY to the ones
    /// left. The PTY has one size, so it takes the fewest columns and the fewest lines
    /// any of them fits, as tmux does for clients of different sizes. A window's own
    /// paint cannot change the others' fits, so the size settles. Larger windows draw
    /// that grid with room to spare. Returns whether the size changed.
    pub(crate) fn fit(&self, keep: impl Fn(WindowId) -> bool) -> bool {
        let mut views = self.views.borrow_mut();
        views.retain(|id, _| keep(*id));
        let Some(size) = views
            .values()
            .map(|view| view.fits)
            .reduce(|size, fits| GridSize {
                columns: size.columns.min(fits.columns),
                lines: size.lines.min(fits.lines),
                cell: size.cell,
            })
        else {
            return false;
        };
        if self.size.replace(size) == size {
            return false;
        }
        self.term.lock().resize(size);
        let _ = self.sender.send(Msg::Resize(size.window_size()));
        true
    }

    /// Where `window` last painted the pane.
    pub(crate) fn view(&self, window: WindowId) -> Option<View> {
        self.views.borrow().get(&window).copied()
    }

    /// The screen's size, which is the PTY's.
    pub(crate) fn size(&self) -> GridSize {
        self.size.get()
    }

    pub(crate) fn window_size(&self) -> WindowSize {
        self.size.get().window_size()
    }

    pub(crate) fn scroll(&self, scroll: Scroll) {
        self.term.lock().scroll_display(scroll);
    }

    /// Every line of output, scrollback included, with trailing blanks trimmed.
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        use alacritty_terminal::index::{Column, Point};
        let term = self.term.lock();
        let start = Point::new(term.topmost_line(), Column(0));
        let end = Point::new(term.bottommost_line(), term.last_column());
        let text = term.bounds_to_string(start, end);
        text.lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_owned()
    }

    /// The lines on screen now, as drawn.
    #[cfg(test)]
    pub(crate) fn screen_text(&self) -> String {
        use alacritty_terminal::index::{Column, Line, Point};
        let term = self.term.lock();
        let offset = term.grid().display_offset() as i32;
        let start = Point::new(Line(-offset), Column(0));
        let end = Point::new(
            Line(term.screen_lines() as i32 - 1 - offset),
            term.last_column(),
        );
        term.bounds_to_string(start, end)
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Ends the shell: the IO thread stops and the PTY closes. Unless the shell
    /// exited by itself, it, the terminal's foreground job, and everything they
    /// started get SIGHUP now, and [`Stopping`] kills what is left.
    pub(crate) fn stop(mut self) -> Stopping {
        let targets = (!self.exited).then(|| match &self.shell {
            Some(shell) => {
                // SAFETY: the controller stays open until the IO thread is joined, below.
                let foreground = unsafe { libc::tcgetpgrp(self.master) };
                let groups = [self.pid, u32::try_from(foreground).unwrap_or(self.pid)];
                Targets::Pinned(shell.tree(&groups))
            }
            None => Targets::Unpinned(self.pid),
        });
        let _ = self.sender.send(Msg::Shutdown);
        if let Some(targets) = &targets {
            signalled(self.pid);
            targets.signal(libc::SIGHUP);
        }
        let stopping = Stopping(Arc::new(Ending {
            targets: Mutex::new(targets),
            reaped: AtomicBool::new(self.io.is_none()),
        }));
        if let Some(io) = self.io.take() {
            let ending = stopping.0.clone();
            let master = self.master;
            std::thread::spawn(move || {
                // A panicked IO thread has dropped the PTY already, so `master` may
                // name another file by now.
                if let Ok(stopped) = io.join() {
                    close_controller(master);
                    // alacritty's PTY sends the shell's PID a SIGHUP as it drops, even
                    // after it reaped a shell that exited by itself, then waits for it.
                    drop(stopped);
                }
                ending.reaped.store(true, Ordering::SeqCst);
            });
        }
        stopping
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        if self.io.is_some() {
            let _ = self.sender.send(Msg::Shutdown);
        }
    }
}

/// A shell told to exit, until it and what it started have gone. Clones share it, so
/// a quit can wait for a shell a closed pane is still ending.
#[derive(Clone)]
pub(crate) struct Stopping(Arc<Ending>);

struct Ending {
    targets: Mutex<Option<Targets>>,
    reaped: AtomicBool,
}

/// What ending a shell signals.
enum Targets {
    /// The shell, the terminal's foreground job and what they started, each pinned.
    Pinned(Tree),
    /// The shell and its process group by PID, when pinning failed, such as on a
    /// Linux kernel without `pidfd_open`. Signalled only while the shell is
    /// unreaped, so its PID and group are its own, except in the microseconds
    /// between that check and the signal, when the PTY's drop may reap it.
    Unpinned(u32),
}

impl Targets {
    fn signal(&self, signal: libc::c_int) {
        match self {
            Self::Pinned(tree) => tree.signal(signal),
            Self::Unpinned(pid) if unreaped(*pid) => {
                let pid = *pid as libc::pid_t;
                // SAFETY: kill takes a PID or a negated group ID and a signal.
                unsafe {
                    libc::kill(-pid, signal);
                    libc::kill(pid, signal);
                }
            }
            Self::Unpinned(_) => {}
        }
    }

    /// Whether anything found is still running. Unpinned, only the shell is known,
    /// and the PTY's drop reaps it.
    fn is_alive(&self) -> bool {
        match self {
            Self::Pinned(tree) => tree.is_alive(),
            Self::Unpinned(_) => false,
        }
    }

    fn refresh(&mut self) {
        if let Self::Pinned(tree) = self {
            tree.refresh();
        }
    }
}

impl Stopping {
    pub(crate) fn is_done(&self) -> bool {
        self.0.reaped.load(Ordering::SeqCst)
            && self
                .0
                .targets
                .lock()
                .unwrap()
                .as_ref()
                .is_none_or(|targets| !targets.is_alive())
    }

    /// Waits until `deadline` for everything to go, then kills what is left.
    pub(crate) fn finish_by(&self, deadline: Instant) {
        if self.wait_until(deadline) {
            return;
        }
        if let Some(targets) = self.0.targets.lock().unwrap().as_mut() {
            targets.refresh();
            targets.signal(libc::SIGKILL);
        }
        self.wait_until(Instant::now() + Duration::from_millis(200));
    }

    fn wait_until(&self, deadline: Instant) -> bool {
        loop {
            if self.is_done() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Lets the shell exit in the background, with SIGKILL after [`GRACE`].
    pub(crate) fn detach(&self) {
        let stopping = self.clone();
        std::thread::spawn(move || stopping.finish_by(Instant::now() + GRACE));
    }
}

/// Hangs up the PTY once its IO thread has stopped. A process exiting on macOS waits
/// for its terminal output to drain, and with nothing reading the controller it would
/// wait forever, so the PTY's drop, which waits for the shell before it closes the
/// controller, would never return. `/dev/null` takes the descriptor's place, so the
/// PTY's own close later closes that instead.
fn close_controller(master: RawFd) {
    // SAFETY: the IO thread has stopped and nothing else reads or writes `master`;
    // dup2 replaces it in one step, so the number never names another file.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if null < 0 {
            return;
        }
        // dup2 clears close-on-exec, which keeps a shell started meanwhile from
        // inheriting the descriptor.
        #[cfg(target_os = "linux")]
        libc::dup3(null, master, libc::O_CLOEXEC);
        #[cfg(not(target_os = "linux"))]
        if libc::dup2(null, master) >= 0 {
            libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        libc::close(null);
    }
}

fn pin(pid: u32) -> Option<Root> {
    #[cfg(test)]
    if PIN_FAILS.get() {
        return None;
    }
    Root::pin(pid)
}

#[cfg(test)]
thread_local! {
    /// Makes pinning fail for shells this thread starts, as it does without
    /// `pidfd_open`.
    pub(crate) static PIN_FAILS: Cell<bool> = const { Cell::new(false) };
}

/// Shells Keystone has sent a signal, so a test can tell that one that exited by
/// itself got none.
#[cfg(test)]
pub(crate) static SIGNALLED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn signalled(_pid: u32) {
    #[cfg(test)]
    SIGNALLED.lock().unwrap().push(_pid);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flood queues one wakeup until the UI applies it, and nothing for events
    /// Keystone ignores. Replies and the exit are always queued.
    #[test]
    fn wakeups_coalesce_and_replies_always_queue() {
        let (events, mut received) = futures::channel::mpsc::unbounded();
        let woken = Arc::new(AtomicBool::new(false));
        let listener = Listener {
            pane: 7,
            events,
            woken: woken.clone(),
        };
        let mut queued = || {
            std::iter::from_fn(|| received.try_recv().ok())
                .map(|(pane, event)| {
                    assert_eq!(pane, 7);
                    format!("{event:?}")
                })
                .collect::<Vec<_>>()
        };
        for _ in 0..1000 {
            listener.send_event(Event::Wakeup);
            listener.send_event(Event::Title("busy".into()));
            listener.send_event(Event::Bell);
        }
        listener.send_event(Event::PtyWrite("\x1b[0n".into()));
        listener.send_event(Event::PtyWrite("\x1b[0n".into()));
        assert_eq!(
            queued(),
            ["Wakeup", "PtyWrite(\u{1b}[0n)", "PtyWrite(\u{1b}[0n)"]
        );
        woken.store(false, Ordering::Release);
        listener.send_event(Event::Wakeup);
        listener.send_event(Event::Wakeup);
        listener.send_event(Event::Exit);
        assert_eq!(queued(), ["Wakeup", "Exit"]);
    }

    #[test]
    fn a_closed_controller_reads_nothing_and_closes_on_exec() {
        let mut fds = [0; 2];
        // SAFETY: pipe writes two descriptors into the array it is given.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: both descriptors were just opened by this test.
        unsafe { libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC) };
        close_controller(fds[0]);
        let mut byte = 0u8;
        // SAFETY: read writes at most one byte into `byte`; close takes descriptors
        // this test owns.
        unsafe {
            assert_eq!(
                libc::fcntl(fds[0], libc::F_GETFD) & libc::FD_CLOEXEC,
                libc::FD_CLOEXEC
            );
            assert_eq!(
                libc::read(fds[0], (&raw mut byte).cast(), 1),
                0,
                "/dev/null took its place"
            );
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}
