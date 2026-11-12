//! Sessions on another machine, end to end: the real `keystone-host` binary, with
//! `keystone-host attach` run directly as the transport in place of `ssh`, and the
//! scripted Claude as the provider. Each test has its own host folder. A few tests of
//! the link alone use a shell script as the host.

use keystone_agent::remote::frame::{self, Call, Frame, Op, Outcome, Routed, Welcome};
use keystone_agent::remote::{Link, RemoteConnection, Status};
use keystone_agent::{ApprovalState, Command, Connection, Event, ItemKind, TurnStatus};
use keystone_core::{ModelChoice, Provider};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(30);
const SESSION: u64 = 12;

/// A machine: its own host folder, and the scripted providers in place of real ones.
struct Machine {
    dir: tempfile::TempDir,
    env: Vec<(String, String)>,
    host: OsString,
}

impl Machine {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = |p: &Path| p.to_string_lossy().into_owned();
        let env = vec![
            ("KEYSTONE_HOST_DIR".into(), path(dir.path())),
            ("HOME".into(), path(dir.path())),
            (
                "KEYSTONE_CLAUDE_PATH".into(),
                env!("CARGO_BIN_EXE_keystone-fake-claude").into(),
            ),
            (
                "KEYSTONE_CODEX_PATH".into(),
                env!("CARGO_BIN_EXE_keystone-fake-codex").into(),
            ),
        ];
        Self {
            dir,
            env,
            host: env!("CARGO_BIN_EXE_keystone-host").into(),
        }
    }

    fn with(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    /// `keystone-host attach` with this machine's environment, as SSH would run it.
    fn argv(&self) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec!["env".into()];
        argv.extend(self.env.iter().map(|(k, v)| format!("{k}={v}").into()));
        argv.push(self.host.clone());
        argv.push("attach".into());
        argv
    }

    /// The daemon's process ID while it holds its lock.
    fn daemon(&self) -> Option<i32> {
        let path = self.dir.path().join("run/host.lock");
        let file = std::fs::File::open(&path).ok()?;
        // SAFETY: flock only reads the descriptor, which `file` keeps open.
        let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if free {
            return None;
        }
        std::fs::read_to_string(&path).ok()?.trim().parse().ok()
    }

    /// The names of the watcher files in the run folder.
    fn watcher_files(&self) -> Vec<String> {
        std::fs::read_dir(self.dir.path().join("run"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with("reap-"))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns once a watcher has pinned every live child in `dirs`. A provider that
    /// exits on end of input before the next scan orphans a child the watcher never
    /// saw, which docs/design/remote.md names as out of its reach, so a test that
    /// kills the daemon comes here first. Linux shows the pins in the watcher's fdinfo.
    #[cfg(target_os = "linux")]
    fn wait_watched(&self, dirs: &[&Path]) {
        let live: Vec<i32> = dirs
            .iter()
            .flat_map(|dir| numbers(dir, "children"))
            .filter(|pid| !gone(*pid))
            .collect();
        let daemon = self.daemon().expect("a daemon runs");
        wait_for("the watcher to pin every live child", || {
            let pinned: Vec<i32> = children(daemon, "keystone-host reap")
                .iter()
                .flat_map(|pid| {
                    std::fs::read_dir(format!("/proc/{pid}/fdinfo"))
                        .into_iter()
                        .flatten()
                        .flatten()
                })
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .flat_map(|info| {
                    info.lines()
                        .filter_map(|line| line.strip_prefix("Pid:")?.trim().parse().ok())
                        .collect::<Vec<i32>>()
                })
                .collect();
            live.iter().all(|pid| pinned.contains(pid))
        });
    }

    /// Elsewhere the pins can't be read, so this allows two of the watcher's scans.
    #[cfg(not(target_os = "linux"))]
    fn wait_watched(&self, _dirs: &[&Path]) {
        std::thread::sleep(Duration::from_millis(600));
    }

    /// Kills the daemon and checks that every provider generation in `dirs`, its child
    /// and its watcher exit, before the test's folders disappear.
    fn end_children(&self, dirs: &[&Path]) {
        for dir in dirs {
            assert_eq!(
                pids(dir).len(),
                numbers(dir, "children").len(),
                "each provider generation started its child"
            );
        }
        let started: Vec<i32> = dirs
            .iter()
            .flat_map(|dir| [pids(dir), numbers(dir, "children")].concat())
            .collect();
        let daemon = self.daemon().expect("a daemon runs");
        let watchers = children(daemon, "keystone-host reap");
        self.kill_daemon();
        wait_for(
            "all provider generations, children and watchers to exit",
            || started.iter().chain(&watchers).all(|pid| gone(*pid)),
        );
        assert!(self.watcher_files().is_empty(), "no watcher holds a tree");
    }

    fn kill_daemon(&self) {
        let pid = self.daemon().expect("a daemon runs");
        // SAFETY: kill only signals; the pid is this test's daemon, which holds the lock.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        wait_for("the daemon to die", || self.daemon().is_none());
    }

    fn client(&self, id: &str) -> Client {
        let link = Link::new(self.argv(), id.into());
        let (tx, wake) = channel();
        let conn = RemoteConnection::new(
            &link,
            Provider::Claude,
            None,
            Arc::new(move || {
                let _ = tx.send(());
            }),
        );
        Client {
            link,
            conn,
            wake,
            events: Vec::new(),
        }
    }

    fn raw(&self) -> Raw {
        let argv = self.argv();
        let mut child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
        let stdout = child.stdout.take().unwrap();
        let (tx, frames) = channel();
        let pong = stdin.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            while let Ok(Some(frame)) = frame::read(&mut reader, &mut line) {
                if frame == Frame::Ping {
                    let line = frame::encode(&Frame::Pong).unwrap();
                    let _ = pong.lock().unwrap().write_all(&line);
                } else if tx.send(frame).is_err() {
                    return;
                }
            }
        });
        Raw {
            child,
            stdin,
            frames,
        }
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon() {
            // SAFETY: kill only signals; the pid is this test's daemon, which holds the lock.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A session folder with the scripted Claude's settings as `.fake-claude-<name>` files.
fn folder(settings: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for file in ["pids", "messages"] {
        std::fs::write(dir.path().join(format!(".fake-claude-{file}")), "").unwrap();
    }
    for (name, value) in settings {
        std::fs::write(dir.path().join(format!(".fake-claude-{name}")), value).unwrap();
    }
    dir
}

/// The finished lines of a file the scripted Claude appends to, leaving out one it is
/// still writing.
fn lines(dir: &Path, name: &str) -> Vec<String> {
    let text =
        std::fs::read_to_string(dir.join(format!(".fake-claude-{name}"))).unwrap_or_default();
    let finished = text.rfind('\n').map_or("", |end| &text[..end]);
    finished.lines().map(str::to_owned).collect()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn pids(dir: &Path) -> Vec<i32> {
    numbers(dir, "pids")
}

fn numbers(dir: &Path, name: &str) -> Vec<i32> {
    lines(dir, name)
        .iter()
        .filter_map(|p| p.parse().ok())
        .collect()
}

/// Exited, a zombie included.
fn gone(pid: i32) -> bool {
    matches!(
        keystone_agent::host::process_state(pid as u32),
        None | Some('Z' | 'X')
    )
}

/// The app's side: a link and the Claude connection on it.
struct Client {
    link: Link,
    conn: RemoteConnection,
    wake: Receiver<()>,
    events: Vec<(u64, Event)>,
}

impl Client {
    fn send(&self, dir: &Path, text: &str) -> Result<(), &'static str> {
        self.conn.send(
            SESSION,
            &dir.to_string_lossy(),
            None,
            &ModelChoice::default(),
            ApprovalState::default(),
            text.into(),
            Vec::new(),
        )
    }

    fn drain(&mut self) {
        self.events.extend(self.conn.drain());
    }

    /// Drains on every wake until `done` holds for the events so far and the status.
    fn wait(&mut self, what: &str, mut done: impl FnMut(&[(u64, Event)], &Status) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            self.drain();
            if done(&self.events, &self.link.status()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; status {:?}, events {:#?}",
                self.link.status(),
                self.events
            );
            let _ = self.wake.recv_timeout(Duration::from_millis(50));
        }
    }

    fn connected(&mut self) {
        self.wait("the link to connect", |_, status| {
            *status == Status::Connected
        });
    }

    fn turn_ended(&mut self) -> TurnStatus {
        let mut ended = None;
        self.wait("the turn to end", |events, _| {
            ended = events.iter().find_map(|(_, event)| match event {
                Event::TurnCompleted { status, .. } => Some(*status),
                _ => None,
            });
            ended.is_some()
        });
        ended.unwrap()
    }

    fn approval(&mut self) -> keystone_agent::Approval {
        let mut found = None;
        self.wait("an approval request", |events, _| {
            found = events.iter().find_map(|(_, event)| match event {
                Event::ApprovalRequested(approval) => Some(approval.clone()),
                _ => None,
            });
            found.is_some()
        });
        found.unwrap()
    }

    fn kill_transport(&mut self) {
        let pid = self.link.transport_pid().expect("a transport runs") as i32;
        // SAFETY: kill only signals; the pid is this link's own unreaped child.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        self.wait("the link to drop", |_, status| {
            *status == Status::Reconnecting
        });
    }
}

fn count(events: &[(u64, Event)], matches: impl Fn(&Event) -> bool) -> usize {
    events.iter().filter(|(_, event)| matches(event)).count()
}

/// Every event of the turn came once and in order: one start and one end, and each
/// agent message's streamed text, joined, is exactly its completed text.
fn assert_whole_turn(events: &[(u64, Event)]) {
    assert_eq!(count(events, |e| matches!(e, Event::TurnStarted)), 1);
    assert_eq!(
        count(events, |e| matches!(e, Event::TurnCompleted { .. })),
        1
    );
    let mut streamed: HashMap<&str, String> = HashMap::new();
    let mut completed = 0;
    for (_, event) in events {
        match event {
            Event::ItemDelta { item_id, text } => {
                streamed.entry(item_id).or_default().push_str(text)
            }
            Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage => {
                completed += 1;
                assert_eq!(
                    streamed.get(item.id.as_str()).map(String::as_str),
                    Some(item.text.as_str()),
                    "streamed text of {} matches its completed text",
                    item.id
                );
            }
            _ => {}
        }
    }
    assert!(completed > 0, "the turn has an agent message");
}

/// The app's end without a `Link`, to send exactly the frames a test needs.
struct Raw {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    frames: Receiver<Frame>,
}

impl Raw {
    fn send(&mut self, frame: &Frame) {
        self.stdin
            .lock()
            .unwrap()
            .write_all(&frame::encode(frame).unwrap())
            .unwrap();
    }

    fn next(&mut self) -> Frame {
        self.frames.recv_timeout(WAIT).expect("a frame")
    }

    fn hello(&mut self, client: &str, after: Option<u64>) -> Welcome {
        self.send(&Frame::Hello {
            protocol: frame::PROTOCOL,
            client: client.into(),
            after,
        });
        match self.next() {
            Frame::Welcome(welcome) => welcome,
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    /// Frames up to `synced`, then the `sessions` frame when one follows.
    fn replay(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        loop {
            match self.next() {
                Frame::Synced { .. } => return frames,
                frame => frames.push(frame),
            }
        }
    }

    fn until(&mut self, mut done: impl FnMut(&Frame) -> bool) -> Vec<Frame> {
        let mut frames = Vec::new();
        loop {
            let frame = self.next();
            let stop = done(&frame);
            frames.push(frame);
            if stop {
                return frames;
            }
        }
    }

    /// Every frame until the host ends the attachment.
    fn rest(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Ok(frame) = self.frames.recv_timeout(WAIT) {
            frames.push(frame);
        }
        frames
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        self.kill();
    }
}

fn send_call(id: u64, dir: &Path, text: &str) -> Frame {
    send_call_to(id, SESSION, dir, text)
}

fn send_call_to(id: u64, session: u64, dir: &Path, text: &str) -> Frame {
    Frame::Call(Call {
        id,
        provider: Provider::Claude,
        account: None,
        op: Op::Send {
            session,
            cwd: dir.to_string_lossy().into_owned(),
            resume: None,
            choice: ModelChoice::default(),
            approval: ApprovalState::default(),
            text: text.into(),
        },
    })
}

fn turn_completed(frame: &Frame) -> bool {
    matches!(
        frame,
        Frame::Event(routed) if matches!(routed.event, Event::TurnCompleted { .. })
    )
}

fn events(frames: &[Frame]) -> Vec<(u64, Event)> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event(routed) => Some((routed.session, routed.event.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_send_streams_its_events_in_order() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "1")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "hello long").unwrap();
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
    assert_whole_turn(&client.events);
    assert!(!client.conn.is_running(SESSION));
    assert_eq!(lines(dir.path(), "messages").len(), 1);
}

#[test]
fn killing_the_transport_mid_turn_loses_and_repeats_nothing() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "4")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "hello long").unwrap();
    client.wait("streaming", |events, _| {
        events
            .iter()
            .any(|(_, e)| matches!(e, Event::ItemDelta { .. }))
    });
    client.kill_transport();
    assert_eq!(
        client.send(dir.path(), "not now"),
        Err("The machine isn't connected, so the message wasn't sent")
    );
    client.connected();
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
    assert_whole_turn(&client.events);
    assert_eq!(lines(dir.path(), "messages").len(), 1);
}

#[test]
fn events_appended_during_a_replay_are_neither_lost_nor_doubled() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "40")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "hello long").unwrap();
    client.wait("streaming", |events, _| {
        events
            .iter()
            .any(|(_, e)| matches!(e, Event::ItemDelta { .. }))
    });
    client.kill_transport();
    client.connected();
    let at_reconnect = client.events.len();
    assert!(
        count(&client.events, |e| matches!(e, Event::TurnCompleted { .. })) == 0,
        "the turn still streams after the reconnect"
    );
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
    assert!(
        count(&client.events[at_reconnect..], |e| matches!(
            e,
            Event::ItemDelta { .. }
        )) > 0,
        "some text arrived live after the replay"
    );
    assert_whole_turn(&client.events);
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "faults exist only in debug builds")]
fn an_append_forced_during_a_replay_is_neither_lost_nor_doubled() {
    let faults = tempfile::tempdir().unwrap();
    let machine = Machine::new().with("KEYSTONE_HOST_FAULTS", &faults.path().to_string_lossy());
    let dir = folder(&[("delay-ms", "10")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "hello long").unwrap();
    client.wait("streaming", |events, _| {
        events
            .iter()
            .any(|(_, e)| matches!(e, Event::ItemDelta { .. }))
    });
    let during = faults.path().join("append-during-replay");
    std::fs::write(&during, "").unwrap();
    client.kill_transport();
    client.connected();
    std::fs::remove_file(&during).unwrap();
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
    assert_whole_turn(&client.events);
}

#[test]
fn a_send_whose_reply_was_lost_reaches_the_provider_once() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "1")]);
    let mut first = machine.raw();
    first.hello("app-1", None);
    first.replay();
    first.send(&send_call(1, dir.path(), "hello"));
    wait_for("the provider to get the message", || {
        lines(dir.path(), "messages").len() == 1
    });
    first.kill();

    let mut second = machine.raw();
    second.hello("app-1", Some(0));
    let mut frames = second.replay();
    second.send(&send_call(1, dir.path(), "hello"));
    frames.extend(second.until(|frame| matches!(frame, Frame::Reply { id: 1, .. })));
    assert_eq!(
        frames.last(),
        Some(&Frame::Reply {
            id: 1,
            outcome: Outcome::Ok(())
        })
    );
    if !frames.iter().any(turn_completed) {
        second.until(turn_completed);
    }
    second.send(&send_call(2, dir.path(), "then this"));
    wait_for("the next message", || {
        lines(dir.path(), "messages").len() >= 2
    });
    assert_eq!(
        lines(dir.path(), "messages"),
        ["\"hello\"", "\"then this\""],
        "a second run of call 1 would reach the provider before call 2"
    );
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "faults exist only in debug builds")]
fn a_call_sent_again_after_a_crash_before_its_reply_does_not_run_again() {
    let faults = tempfile::tempdir().unwrap();
    let machine = Machine::new().with("KEYSTONE_HOST_FAULTS", &faults.path().to_string_lossy());
    let dir = folder(&[("delay-ms", "1")]);
    let crash = faults.path().join("crash-before-reply");
    std::fs::write(&crash, "").unwrap();
    let mut first = machine.raw();
    first.hello("app-1", None);
    first.replay();
    first.send(&send_call(1, dir.path(), "hello"));
    wait_for("the daemon to die before replying", || {
        first.child.try_wait().unwrap().is_some()
    });
    assert_eq!(lines(dir.path(), "messages"), ["\"hello\""]);
    std::fs::remove_file(&crash).unwrap();

    let mut second = machine.raw();
    second.hello("app-1", Some(0));
    second.replay();
    second.send(&send_call(1, dir.path(), "hello"));
    let frames = second.until(|frame| matches!(frame, Frame::Reply { id: 1, .. }));
    assert!(
        matches!(
            frames.last(),
            Some(Frame::Reply { outcome: Outcome::Err(reason), .. })
                if reason.contains("may or may not have run")
        ),
        "{frames:?}"
    );
    second.send(&send_call(2, dir.path(), "then this"));
    wait_for("the next message", || {
        lines(dir.path(), "messages").len() >= 2
    });
    assert_eq!(
        lines(dir.path(), "messages"),
        ["\"hello\"", "\"then this\""],
        "call 1 ran once"
    );
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "faults exist only in debug builds")]
fn events_the_journal_fails_to_take_wait_in_order_with_a_warning() {
    let faults = tempfile::tempdir().unwrap();
    let machine = Machine::new().with("KEYSTONE_HOST_FAULTS", &faults.path().to_string_lossy());
    let dir = folder(&[("delay-ms", "4")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "hello long").unwrap();
    client.wait("streaming", |events, _| {
        events
            .iter()
            .any(|(_, e)| matches!(e, Event::ItemDelta { .. }))
    });
    let failing = faults.path().join("append");
    std::fs::write(&failing, "").unwrap();
    client.wait("a warning that events wait", |events, _| {
        events.iter().any(|(session, e)| {
            *session == SESSION
                && matches!(e, Event::Warning { message } if message.contains("couldn't save"))
        })
    });
    std::fs::remove_file(&failing).unwrap();
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
    assert_whole_turn(&client.events);
}

#[test]
fn a_consumer_that_falls_behind_is_disconnected_and_catches_up_by_replay() {
    let limit = 131_072;
    let machine = Machine::new().with("KEYSTONE_HOST_QUEUE_LIMIT", &limit.to_string());
    let dir = folder(&[("delay-ms", "1")]);
    let mut slow = machine.raw();
    slow.hello("app-1", None);
    slow.replay();
    // Paced, so no burst fills the queue, while the whole reply is several times more
    // than the limit and a socket's buffers hold.
    let text = format!("hello {}", "word ".repeat(5_000));
    slow.send(&send_call(1, dir.path(), &text));
    let mut frames = slow.until(|frame| {
        matches!(frame, Frame::Event(routed) if matches!(routed.event, Event::ItemDelta { .. }))
    });
    let provider = pids(dir.path());
    let attach = slow.child.id() as i32;
    // SAFETY: kill only signals; the pid is this test's own unreaped child.
    unsafe { libc::kill(attach, libc::SIGSTOP) };
    let log = machine.dir.path().join("host.log");
    wait_for("the host to disconnect the stalled reader", || {
        std::fs::read_to_string(&log).is_ok_and(|text| text.contains("fell"))
    });
    // SAFETY: as above.
    unsafe { libc::kill(attach, libc::SIGCONT) };
    frames.extend(slow.rest());
    wait_for("the host to end the attachment", || {
        slow.child.try_wait().unwrap().is_some()
    });
    assert!(
        !frames.iter().any(turn_completed),
        "the reader missed the end"
    );
    // With no app attached, the host ends the provider once its turn has ended, so the
    // rest of the turn is all in the journal.
    wait_for("the turn to end", || provider.iter().all(|pid| gone(*pid)));
    let received = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event(routed) => Some(routed.seq),
            _ => None,
        })
        .max()
        .unwrap();

    let mut again = machine.raw();
    again.hello("app-1", Some(received));
    let replayed = again.replay();
    let size: usize = replayed
        .iter()
        .map(|frame| frame::encode(frame).unwrap().len())
        .sum();
    assert!(
        size > 3 * limit,
        "a replay of {size} bytes is past the limit"
    );
    frames.extend(replayed);
    assert_whole_turn(&events(&frames));
}

#[test]
fn a_reader_that_hangs_up_mid_replay_is_sent_nothing_more() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "0")]);
    let mut first = machine.raw();
    first.hello("app-1", None);
    first.replay();
    let text = format!("hello {}", "word ".repeat(5_000));
    first.send(&send_call(1, dir.path(), &text));
    first.until(turn_completed);
    drop(first);
    let replay: usize = {
        let mut probe = machine.raw();
        probe.hello("app-1", Some(0));
        probe
            .replay()
            .iter()
            .map(|frame| frame::encode(frame).unwrap().len())
            .sum()
    };

    let mut stream =
        std::os::unix::net::UnixStream::connect(machine.dir.path().join("run/host.sock")).unwrap();
    stream
        .write_all(
            &frame::encode(&Frame::Hello {
                protocol: frame::PROTOCOL,
                client: "app-1".into(),
                after: Some(0),
            })
            .unwrap(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = Vec::new();
    assert!(matches!(
        frame::read(&mut reader, &mut line),
        Ok(Some(Frame::Welcome(_)))
    ));
    // The replay is queued now, far more than the socket holds, and nothing reads it.
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let mut rest = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut rest).unwrap();
    assert!(
        rest.len() < replay / 2,
        "{} of a {replay} byte replay was still written",
        rest.len()
    );
}

#[test]
fn an_answer_given_while_reconnecting_arrives_once() {
    let logs = tempfile::tempdir().unwrap();
    let machine = Machine::new().with(
        "KEYSTONE_FAKE_CLAUDE_LOG_DIR",
        &logs.path().to_string_lossy(),
    );
    let dir = folder(&[("delay-ms", "1")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "please run it").unwrap();
    let approval = client.approval();
    assert_eq!(client.conn.waiting(SESSION), vec![approval.clone()]);
    client.kill_transport();
    client.conn.command(Command::Answer {
        session: SESSION,
        request: approval.request.clone(),
        allow: true,
    });
    client.connected();
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
    assert!(client.conn.waiting(SESSION).is_empty());
    let applied: Vec<String> = std::fs::read_dir(logs.path())
        .unwrap()
        .filter_map(|entry| std::fs::read_to_string(entry.ok()?.path()).ok())
        .flat_map(|text| text.lines().map(str::to_owned).collect::<Vec<_>>())
        .filter(|line| line.starts_with("applied"))
        .collect();
    assert_eq!(applied.len(), 1, "answers applied: {applied:?}");
}

#[test]
fn a_second_attach_replaces_the_first() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "1")]);
    let mut first = machine.raw();
    first.hello("app-1", None);
    first.replay();
    let mut second = machine.raw();
    second.hello("app-1", Some(0));
    second.replay();
    wait_for("the first attach to end", || {
        first.child.try_wait().unwrap().is_some()
    });
    second.send(&send_call(1, dir.path(), "hello"));
    let frames = second.until(turn_completed);
    assert!(frames.contains(&Frame::Reply {
        id: 1,
        outcome: Outcome::Ok(())
    }));
    assert_whole_turn(&events(&frames));
}

#[test]
fn a_daemon_crash_ends_the_running_turn_and_its_approvals() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "1")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "please run it").unwrap();
    let approval = client.approval();
    assert!(client.conn.is_running(SESSION));
    let providers = pids(dir.path());
    assert!(!providers.is_empty());

    machine.kill_daemon();
    wait_for("the provider processes to exit", || {
        providers.iter().all(|pid| !alive(*pid))
    });
    let before = client.events.len();
    client.wait("the next daemon to end the turn", |events, status| {
        *status == Status::Connected
            && events[before..]
                .iter()
                .any(|(_, e)| matches!(e, Event::TurnCompleted { .. }))
    });
    let after = &client.events[before..];
    assert!(after.contains(&(
        SESSION,
        Event::ApprovalResolved {
            request: approval.request
        }
    )));
    assert!(after.iter().any(|(_, e)| matches!(
        e,
        Event::TurnCompleted { status: TurnStatus::Failed, error: Some(error), .. }
            if error.contains("restarted")
    )));
    assert!(!client.conn.is_running(SESSION));
    assert!(client.conn.waiting(SESSION).is_empty());
}

/// Sends `text` to `session` in `dir`.
fn send_to(client: &Client, session: u64, dir: &Path, text: &str) {
    client
        .conn
        .send(
            session,
            &dir.to_string_lossy(),
            None,
            &ModelChoice::default(),
            ApprovalState::default(),
            text.into(),
            Vec::new(),
        )
        .unwrap();
}

#[test]
fn a_daemon_crash_ends_what_the_providers_started_before_their_turns_end() {
    let machine = Machine::new();
    // Two providers exit when their input closes: one leaves its child behind in its
    // group, the other in a session of its own, as Claude Code 2.1.317 runs a Bash
    // tool command. The third waits on a tool call, as Claude Code waits for a shell
    // it put in a group of its own.
    let exits = folder(&[("delay-ms", "20"), ("child", "1")]);
    let alone = folder(&[("delay-ms", "20"), ("child", "session")]);
    let holds = folder(&[("delay-ms", "1"), ("child", "group"), ("idle", "1")]);
    let mut client = machine.client("app-1");
    client.connected();
    send_to(&client, SESSION, exits.path(), "hello long");
    send_to(&client, SESSION + 1, holds.path(), "hold tool");
    send_to(&client, SESSION + 2, alone.path(), "hello long");
    client.wait("the turns to run", |events, _| {
        let streams = |session| {
            events
                .iter()
                .any(|(s, e)| *s == session && matches!(e, Event::ItemDelta { .. }))
        };
        streams(SESSION)
            && streams(SESSION + 2)
            && events.iter().any(|(s, e)| {
                *s == SESSION + 1
                    && matches!(e, Event::ItemStarted(item) if matches!(item.kind, ItemKind::Command { .. }))
            })
    });
    let started: Vec<i32> = [exits.path(), holds.path(), alone.path()]
        .iter()
        .flat_map(|dir| [pids(dir), numbers(dir, "children")].concat())
        .collect();
    assert_eq!(started.len(), 6, "three providers and a child of each");
    assert!(started.iter().all(|pid| !gone(*pid)));

    let before = client.events.len();
    machine.wait_watched(&[exits.path(), holds.path(), alone.path()]);
    machine.kill_daemon();
    client.wait("the next daemon to end the turns", |events, status| {
        let ended = events[before..]
            .iter()
            .filter(|(_, e)| matches!(e, Event::TurnCompleted { .. }))
            .count();
        if ended > 0 {
            let left: Vec<&i32> = started.iter().filter(|pid| !gone(**pid)).collect();
            assert!(left.is_empty(), "{left:?} outlived the turns' end");
        }
        *status == Status::Connected && ended == 3
    });

    client.send(exits.path(), "hello again").unwrap();
    client.wait("the resumed turn to end", |events, _| {
        events[before..]
            .iter()
            .filter(|(_, e)| matches!(e, Event::TurnCompleted { .. }))
            .count()
            == 4
    });
    assert!(started.iter().all(|pid| gone(*pid)));
    assert_eq!(
        pids(exits.path()).len(),
        2,
        "the resumed turn has its own process"
    );
    machine.wait_watched(&[exits.path(), holds.path(), alone.path()]);
    drop(client);
    machine.end_children(&[exits.path(), holds.path(), alone.path()]);
}

fn reply(frames: &[Frame], id: u64) -> Option<&Outcome> {
    frames.iter().find_map(|frame| match frame {
        Frame::Reply { id: got, outcome } if *got == id => Some(outcome),
        _ => None,
    })
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "faults exist only in debug builds")]
fn a_session_whose_last_run_wont_stop_is_held_and_says_why() {
    let faults = tempfile::tempdir().unwrap();
    let machine = Machine::new()
        .with("KEYSTONE_HOST_FAULTS", &faults.path().to_string_lossy())
        .with("KEYSTONE_HOST_HOLD_MS", "500");
    let dir = folder(&[("delay-ms", "20"), ("child", "session")]);
    let unkillable = faults.path().join("reap-hold");
    std::fs::write(&unkillable, "").unwrap();
    let mut first = machine.raw();
    first.hello("app-1", None);
    first.replay();
    first.send(&send_call(1, dir.path(), "hello long"));
    first.until(|frame| {
        matches!(frame, Frame::Event(routed) if matches!(routed.event, Event::ItemDelta { .. }))
    });
    let child = numbers(dir.path(), "children")[0];
    machine.wait_watched(&[dir.path()]);
    machine.kill_daemon();
    drop(first);

    let mut second = machine.raw();
    second.hello("app-1", None);
    let mut seen = second.replay();
    second.send(&send_call(1, dir.path(), "hello again"));
    seen.extend(second.until(|frame| matches!(frame, Frame::Reply { id: 1, .. })));
    assert!(
        matches!(reply(&seen, 1), Some(Outcome::Err(reason)) if reason.contains("still stopping")),
        "{seen:?}"
    );
    let warning = |frame: &Frame| match frame {
        Frame::Event(routed) => match &routed.event {
            Event::Warning { message } if message.contains("can't stop") => {
                Some((routed.session, message.clone()))
            }
            _ => None,
        },
        _ => None,
    };
    if !seen.iter().any(|frame| warning(frame).is_some()) {
        seen.extend(second.until(|frame| warning(frame).is_some()));
    }
    let (session, message) = seen.iter().find_map(warning).unwrap();
    assert_eq!(session, SESSION);
    assert!(
        message.contains(&child.to_string()),
        "{message} names the child"
    );
    assert!(!gone(child));
    assert!(!seen.iter().any(turn_completed), "held, the turn goes on");
    second.send(&send_call(2, dir.path(), "hello again"));
    let frames = second.until(|frame| matches!(frame, Frame::Reply { id: 2, .. }));
    assert!(matches!(reply(&frames, 2), Some(Outcome::Err(_))));
    let commands = [
        Command::Interrupt { session: SESSION },
        Command::Answer {
            session: SESSION,
            request: keystone_agent::RequestId("tool-1".into()),
            allow: true,
        },
        Command::Act {
            session: SESSION,
            call: "show-1".into(),
            text: "ok".into(),
        },
    ];
    for (id, command) in (3..).zip(commands) {
        second.send(&Frame::Call(Call {
            id,
            provider: Provider::Claude,
            account: None,
            op: Op::Command(command),
        }));
        let frames =
            second.until(|frame| matches!(frame, Frame::Reply { id: got, .. } if *got == id));
        assert!(
            matches!(reply(&frames, id), Some(Outcome::Err(reason)) if reason.contains("still stopping")),
            "call {id} reaches no process: {frames:?}"
        );
    }

    std::fs::remove_file(&unkillable).unwrap();
    let frames = second.until(turn_completed);
    assert!(gone(child), "the turn ends only once the child is gone");
    assert!(events(&frames).iter().any(|(_, e)| matches!(
        e,
        Event::TurnCompleted { status: TurnStatus::Failed, error: Some(error), .. }
            if error.contains("restarted")
    )));
    second.send(&send_call(6, dir.path(), "hello again"));
    let frames = second.until(turn_completed);
    assert_eq!(reply(&frames, 6), Some(&Outcome::Ok(())));
    assert_eq!(
        pids(dir.path()).len(),
        2,
        "a second provider generation ran"
    );
    machine.wait_watched(&[dir.path()]);
    drop(second);
    machine.end_children(&[dir.path()]);
}

#[test]
fn a_watcher_file_nobody_holds_for_a_provider_gone_warns_what_may_still_run() {
    let machine = Machine::new();
    let run = machine.dir.path().join("run");
    std::fs::DirBuilder::new().mode(0o700).create(&run).unwrap();
    // No process is this one started at time 1, so the provider it names is gone.
    let provider = std::process::id();
    let stale = run.join(format!("reap-{provider}-1.lock"));
    std::fs::write(
        &stale,
        format!("{{\"client\":\"app-1\",\"session\":{SESSION},\"provider\":\"Claude\",\"account\":null}}\n"),
    )
    .unwrap();
    let dir = folder(&[("delay-ms", "1")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.wait("a warning naming the provider", |events, _| {
        events.iter().any(|(session, e)| {
            *session == SESSION
                && matches!(e, Event::Warning { message }
                    if message.contains("stopped unexpectedly")
                        && message.contains(&format!("process {provider}")))
        })
    });
    assert!(!stale.exists(), "the daemon removed it");
    client.send(dir.path(), "hello").unwrap();
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
}

/// The processes `daemon` started whose command lines hold `pattern`.
fn children(daemon: i32, pattern: &str) -> Vec<i32> {
    let output = std::process::Command::new("pgrep")
        .args(["-P", &daemon.to_string(), "-f", pattern])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

#[test]
fn a_watcher_killed_before_its_daemon_is_replaced_while_its_provider_runs() {
    let machine = Machine::new();
    // The provider waits on a tool call after its input closes, as Claude Code waits
    // for a shell it started.
    let dir = folder(&[("delay-ms", "1"), ("child", "group"), ("idle", "1")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "hold tool").unwrap();
    client.wait("the tool call", |events, _| {
        events.iter().any(|(_, e)| {
            matches!(e, Event::ItemStarted(item) if matches!(item.kind, ItemKind::Command { .. }))
        })
    });
    let started = [pids(dir.path()), numbers(dir.path(), "children")].concat();
    assert_eq!(started.len(), 2, "a provider and its child");
    let daemon = machine.daemon().expect("a daemon runs");
    let watcher = children(daemon, "keystone-host reap");
    assert_eq!(watcher.len(), 1, "one watcher");
    // SAFETY: kill only signals; the pid is a watcher this test's daemon started.
    unsafe { libc::kill(watcher[0], libc::SIGKILL) };
    wait_for("the watcher to die", || gone(watcher[0]));
    assert_eq!(machine.watcher_files().len(), 1, "it left its file");

    let before = client.events.len();
    machine.kill_daemon();
    client.wait("the next daemon to end the turn", |events, status| {
        let ended = events[before..]
            .iter()
            .any(|(_, e)| matches!(e, Event::TurnCompleted { .. }));
        if ended {
            let left: Vec<&i32> = started.iter().filter(|pid| !gone(**pid)).collect();
            assert!(left.is_empty(), "{left:?} outlived the turn's end");
        }
        *status == Status::Connected && ended
    });
    assert!(
        !client.events[before..].iter().any(
            |(_, e)| matches!(e, Event::Warning { message } if message.contains("stopped unexpectedly"))
        ),
        "the provider still ran, so a new watcher found its tree"
    );
    wait_for("the new watcher to finish", || {
        machine.watcher_files().is_empty()
    });
}

#[test]
fn a_provider_that_exits_while_the_daemon_lives_holds_its_session_until_its_tree_is_gone() {
    let machine = Machine::new();
    let dir = folder(&[
        ("delay-ms", "20"),
        ("child", "session"),
        ("child-term", "ignore"),
    ]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "hello long").unwrap();
    client.wait("streaming", |events, _| {
        events
            .iter()
            .any(|(_, e)| matches!(e, Event::ItemDelta { .. }))
    });
    let provider = pids(dir.path())[0];
    let child = numbers(dir.path(), "children")[0];
    machine.wait_watched(&[dir.path()]);
    let daemon = machine.daemon().expect("a daemon runs");
    // Teardown sends SIGTERM and SIGKILL 2 s later. A frozen daemon dies between the
    // two: the provider exits, and its child ignores the SIGTERM.
    // SAFETY: kill only signals this test's daemon and what its provider started.
    unsafe {
        libc::kill(daemon, libc::SIGSTOP);
        libc::kill(provider, libc::SIGTERM);
        libc::kill(child, libc::SIGTERM);
    }
    wait_for("the provider to exit", || gone(provider));
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        !gone(child),
        "the watcher ends nothing while the daemon lives"
    );
    assert_eq!(
        machine.watcher_files().len(),
        1,
        "it holds on for the child"
    );

    let before = client.events.len();
    machine.kill_daemon();
    client.wait("the next daemon to end the turn", |events, status| {
        let ended = events[before..]
            .iter()
            .any(|(_, e)| matches!(e, Event::TurnCompleted { .. }));
        if ended {
            assert!(gone(child), "the turn ended while the child ran");
        }
        *status == Status::Connected && ended
    });
    assert!(machine.watcher_files().is_empty());
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "faults exist only in debug builds")]
fn a_watcher_that_cant_take_over_stops_its_provider_before_any_input() {
    let faults = tempfile::tempdir().unwrap();
    let machine = Machine::new().with("KEYSTONE_HOST_FAULTS", &faults.path().to_string_lossy());
    let refuse = faults.path().join("reap-refuse");
    std::fs::write(&refuse, "").unwrap();
    let dir = folder(&[("delay-ms", "1")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "hello").unwrap();
    client.wait("the start to fail", |events, _| {
        events.iter().any(
            |(_, e)| matches!(e, Event::Failed { message } if message.contains("couldn't watch")),
        )
    });
    // Stopped at once, it may not have written its PID.
    let daemon = machine.daemon().expect("a daemon runs");
    wait_for("the provider to be stopped", || {
        children(daemon, "keystone-fake-claude")
            .into_iter()
            .chain(pids(dir.path()))
            .all(gone)
    });
    assert!(lines(dir.path(), "messages").is_empty(), "it got no input");
    assert!(
        machine.watcher_files().is_empty(),
        "no file says it's watched"
    );

    std::fs::remove_file(&refuse).unwrap();
    client.send(dir.path(), "hello").unwrap();
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
}

#[test]
fn a_daemon_whose_binary_was_replaced_still_watches_its_providers() {
    let bin = tempfile::tempdir().unwrap();
    let host = bin.path().join("keystone-host");
    keystone_core::test_support::write_executable(
        &host,
        std::fs::read(env!("CARGO_BIN_EXE_keystone-host")).unwrap(),
    );
    let mut machine = Machine::new();
    machine.host = host.clone().into();
    let mut client = machine.client("app-1");
    client.connected();
    // As `cargo install` replaces it: a new copy renamed over the old.
    let new = bin.path().join("keystone-host.new");
    keystone_core::test_support::write_executable(
        &new,
        std::fs::read(env!("CARGO_BIN_EXE_keystone-host")).unwrap(),
    );
    std::fs::rename(&new, &host).unwrap();
    let dir = folder(&[("delay-ms", "1")]);
    client.send(dir.path(), "hello").unwrap();
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
    let provider = pids(dir.path())[0];
    assert!(
        machine
            .watcher_files()
            .iter()
            .any(|name| name.starts_with(&format!("reap-{provider}-"))),
        "{:?}",
        machine.watcher_files()
    );
}

#[test]
fn provider_processes_exit_when_the_daemon_dies() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "20")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "hello long").unwrap();
    client.wait("streaming", |events, _| {
        events
            .iter()
            .any(|(_, e)| matches!(e, Event::ItemDelta { .. }))
    });
    let providers = pids(dir.path());
    assert!(providers.iter().any(|pid| alive(*pid)));
    machine.kill_daemon();
    drop(client);
    wait_for("the provider processes to exit", || {
        providers.iter().all(|pid| !alive(*pid))
    });
}

/// Runs a long reply in two sessions without acknowledging anything, then reads back
/// what the journal kept.
fn overflow(machine: &Machine) -> (Vec<Frame>, Vec<Frame>) {
    let dirs = [folder(&[("delay-ms", "0")]), folder(&[("delay-ms", "0")])];
    let mut raw = machine.raw();
    raw.hello("app-1", None);
    raw.replay();
    let mut live = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        let id = index as u64 + 1;
        let session = SESSION + index as u64;
        let mut call = send_call(id, dir.path(), "hello long");
        if let Frame::Call(Call {
            op: Op::Send { session: s, .. },
            ..
        }) = &mut call
        {
            *s = session;
        }
        raw.send(&call);
        live.extend(raw.until(turn_completed));
    }
    let mut again = machine.raw();
    again.hello("app-1", Some(0));
    (live, again.replay())
}

fn lifecycle(frames: &[Frame]) -> Vec<(u64, Event)> {
    events(frames)
        .into_iter()
        .filter(|(_, e)| {
            matches!(
                e,
                Event::ThreadReady { .. }
                    | Event::TurnStarted
                    | Event::TurnCompleted { .. }
                    | Event::ApprovalRequested(_)
                    | Event::ApprovalResolved { .. }
                    | Event::Failed { .. }
            )
        })
        .collect()
}

fn size(frames: &[Frame], matches: impl Fn(&Event) -> bool) -> u64 {
    frames
        .iter()
        .filter(|frame| matches!(frame, Frame::Event(routed) if matches(&routed.event)))
        .map(|frame| frame::encode(frame).unwrap().len() as u64)
        .sum()
}

fn is_delta(event: &Event) -> bool {
    matches!(event, Event::ItemDelta { .. })
}

fn is_item(event: &Event) -> bool {
    matches!(event, Event::ItemStarted(_) | Event::ItemCompleted(_))
}

#[test]
fn a_full_journal_drops_deltas_then_items_and_never_lifecycle_events() {
    let tiny = Machine::new().with("KEYSTONE_HOST_JOURNAL_CAP", "1");
    let (live, kept) = overflow(&tiny);
    assert_eq!(lifecycle(&kept), lifecycle(&live));
    assert_eq!(lifecycle(&live).len(), 6, "two threads, starts and ends");
    assert!(!events(&kept).iter().any(|(_, e)| is_delta(e) || is_item(e)));
    let gaps: Vec<u64> = kept
        .iter()
        .flat_map(|frame| match frame {
            Frame::Gap { sessions, .. } => sessions.clone(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(gaps, vec![SESSION, SESSION + 1]);

    let deltas = size(&live, is_delta);
    let room = size(&live, |e| !is_delta(e)) + deltas / 2;
    let roomy = Machine::new().with("KEYSTONE_HOST_JOURNAL_CAP", &room.to_string());
    let (live, kept) = overflow(&roomy);
    assert_eq!(lifecycle(&kept), lifecycle(&live));
    assert_eq!(
        events(&kept).iter().filter(|(_, e)| is_item(e)).count(),
        events(&live).iter().filter(|(_, e)| is_item(e)).count(),
        "every item event is kept while dropping deltas makes room"
    );
    let kept_deltas = events(&kept).iter().filter(|(_, e)| is_delta(e)).count();
    let live_deltas = events(&live).iter().filter(|(_, e)| is_delta(e)).count();
    assert!(kept_deltas > 0 && kept_deltas < live_deltas);
    assert!(!kept.iter().any(|frame| matches!(frame, Frame::Gap { .. })));
}

#[test]
fn a_queue_limit_too_small_for_any_reader_is_refused() {
    let machine = Machine::new().with("KEYSTONE_HOST_QUEUE_LIMIT", "0");
    let serve = std::process::Command::new(env!("CARGO_BIN_EXE_keystone-host"))
        .arg("serve")
        .envs(machine.env.iter().map(|(k, v)| (k, v)))
        .output()
        .unwrap();
    assert!(!serve.status.success());
    assert!(
        String::from_utf8_lossy(&serve.stderr).contains("must be at least 65536 bytes"),
        "{}",
        String::from_utf8_lossy(&serve.stderr)
    );
    let unset = Machine::new()
        .with("KEYSTONE_HOST_QUEUE_LIMIT", "")
        .with("KEYSTONE_HOST_FAULTS", "");
    let dir = folder(&[("delay-ms", "1")]);
    let mut client = unset.client("app-1");
    client.connected();
    client.send(dir.path(), "hello").unwrap();
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
}

#[test]
fn a_host_speaking_another_protocol_is_refused_with_the_reason() {
    let machine = Machine::new();
    let mut raw = machine.raw();
    raw.send(&Frame::Hello {
        protocol: 99,
        client: "app-1".into(),
        after: None,
    });
    match raw.next() {
        Frame::Refused { reason } => {
            assert!(
                reason.contains("protocol 99 is needed, this host speaks 1"),
                "{reason}"
            )
        }
        other => panic!("expected refused, got {other:?}"),
    }
    wait_for("attach to end", || raw.child.try_wait().unwrap().is_some());
}

#[test]
fn a_missing_host_fails_without_retrying() {
    let link = Link::new(
        ["sh", "-c", "exit 127"].map(OsString::from).into(),
        "app-1".into(),
    );
    wait_for(
        "the link to fail",
        || matches!(link.status(), Status::Failed(reason) if reason.contains("isn't installed")),
    );
}

#[test]
fn a_socket_folder_others_can_open_is_refused() {
    let machine = Machine::new();
    let run = machine.dir.path().join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();

    let serve = std::process::Command::new(env!("CARGO_BIN_EXE_keystone-host"))
        .arg("serve")
        .envs(machine.env.iter().map(|(k, v)| (k, v)))
        .output()
        .unwrap();
    assert!(!serve.status.success());
    assert!(String::from_utf8_lossy(&serve.stderr).contains("must be 0700"));

    let link = machine.client("app-1").link;
    wait_for(
        "the link to fail",
        || matches!(link.status(), Status::Failed(reason) if reason.contains("must be 0700")),
    );
    assert!(machine.daemon().is_none());
}

#[test]
fn a_host_still_exiting_is_tried_again_until_it_answers() {
    let machine = Machine::new();
    let run = machine.dir.path().join("run");
    std::fs::DirBuilder::new().mode(0o700).create(&run).unwrap();
    // An old daemon that removed its socket and still holds the lock as it exits.
    let held = std::fs::File::create(run.join("host.lock")).unwrap();
    // SAFETY: flock only reads the descriptor, which `held` keeps open.
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
    let mut client = machine.client("app-1");
    client.wait("an attempt to end without a daemon", |_, status| {
        assert!(!matches!(status, Status::Failed(_)), "{status:?}");
        *status == Status::Reconnecting
    });
    drop(held);
    client.connected();
}

#[test]
fn a_journal_folder_others_can_open_is_refused() {
    let machine = Machine::new();
    std::fs::set_permissions(machine.dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = machine.client("app-1").link;
    wait_for(
        "the link to fail",
        || matches!(link.status(), Status::Failed(reason) if reason.contains("must be 0700")),
    );
    assert!(machine.daemon().is_none());
}

#[test]
fn only_this_user_can_read_the_journal() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "1")]);
    let mut client = machine.client("app-1");
    client.connected();
    client.send(dir.path(), "hello").unwrap();
    client.turn_ended();
    let mut checked = Vec::new();
    for entry in std::fs::read_dir(machine.dir.path()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("journal.sqlite3") {
            let mode = entry.metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{name} has mode {mode:o}");
            checked.push(name);
        }
    }
    checked.sort();
    assert_eq!(
        checked,
        [
            "journal.sqlite3",
            "journal.sqlite3-shm",
            "journal.sqlite3-wal"
        ]
    );
}

#[test]
fn quit_keeping_sessions_restores_the_turn_and_its_approval_next_launch() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "1")]);
    let mut first = machine.client("app-1");
    first.connected();
    first.send(dir.path(), "please run it").unwrap();
    let approval = first.approval();
    first.link.quit(true);
    drop(first);
    std::thread::sleep(Duration::from_millis(500));
    assert!(pids(dir.path()).iter().any(|pid| alive(*pid)));

    let mut next = machine.client("app-1");
    next.wait("the sessions frame", |_, status| {
        *status == Status::Connected
    });
    wait_for("the restored approval", || {
        next.conn.waiting(SESSION) == vec![approval.clone()]
    });
    assert!(next.conn.is_running(SESSION));
    next.conn.command(Command::Answer {
        session: SESSION,
        request: approval.request,
        allow: true,
    });
    assert_eq!(next.turn_ended(), TurnStatus::Completed);
}

#[test]
fn quit_ending_sessions_ends_the_provider_and_its_turn() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "1")]);
    let mut first = machine.client("app-1");
    first.connected();
    first.send(dir.path(), "please run it").unwrap();
    let approval = first.approval();
    let providers = pids(dir.path());
    first.link.quit(false);
    drop(first);
    wait_for("the provider processes to exit", || {
        providers.iter().all(|pid| !alive(*pid))
    });

    let mut next = machine.client("app-1");
    next.connected();
    assert_eq!(next.turn_ended(), TurnStatus::Interrupted);
    assert!(next.events.contains(&(
        SESSION,
        Event::ApprovalResolved {
            request: approval.request
        }
    )));
    assert!(!next.conn.is_running(SESSION));
    assert!(next.conn.waiting(SESSION).is_empty());
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "faults exist only in debug builds")]
fn quit_ending_sessions_while_the_journal_fails_ends_the_turn_it_has_not_saved() {
    let faults = tempfile::tempdir().unwrap();
    let machine = Machine::new().with("KEYSTONE_HOST_FAULTS", &faults.path().to_string_lossy());
    let failing = faults.path().join("append");
    std::fs::write(&failing, "").unwrap();
    let dir = folder(&[("delay-ms", "1")]);
    let mut first = machine.client("app-1");
    first.connected();
    first.send(dir.path(), "please run it").unwrap();
    first.wait("a warning that events wait", |events, _| {
        events.iter().any(
            |(_, e)| matches!(e, Event::Warning { message } if message.contains("couldn't save")),
        )
    });
    wait_for("the provider to get the message", || {
        lines(dir.path(), "messages").len() == 1
    });
    let providers = pids(dir.path());
    first.link.quit(false);
    drop(first);
    wait_for("the provider processes to exit", || {
        providers.iter().all(|pid| !alive(*pid))
    });
    std::thread::sleep(Duration::from_secs(1));
    let log = std::fs::read_to_string(machine.dir.path().join("host.log")).unwrap();
    assert_eq!(
        log.matches("wait for the journal").count(),
        1,
        "one failure, logged once: {log}"
    );

    std::fs::remove_file(&failing).unwrap();
    let mut next = machine.client("app-1");
    next.connected();
    assert_eq!(next.turn_ended(), TurnStatus::Interrupted);
    assert_eq!(count(&next.events, |e| matches!(e, Event::TurnStarted)), 1);
    wait_for("the sessions frame", || !next.conn.is_running(SESSION));
    assert!(next.conn.waiting(SESSION).is_empty());
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "faults exist only in debug builds")]
fn quit_ending_sessions_ends_a_held_sessions_turn_once_its_last_run_stops() {
    let faults = tempfile::tempdir().unwrap();
    let machine = Machine::new().with("KEYSTONE_HOST_FAULTS", &faults.path().to_string_lossy());
    let dir = folder(&[("delay-ms", "20"), ("child", "session")]);
    let unkillable = faults.path().join("reap-hold");
    std::fs::write(&unkillable, "").unwrap();
    let mut first = machine.raw();
    first.hello("app-1", None);
    first.replay();
    first.send(&send_call(1, dir.path(), "hello long"));
    first.until(|frame| {
        matches!(frame, Frame::Event(routed) if matches!(routed.event, Event::ItemDelta { .. }))
    });
    let child = numbers(dir.path(), "children")[0];
    machine.wait_watched(&[dir.path()]);
    machine.kill_daemon();
    drop(first);

    let mut second = machine.raw();
    second.hello("app-1", None);
    let mut seen = second.replay();
    second.send(&Frame::Quit {
        keep_running: false,
    });
    second.send(&send_call(1, dir.path(), "hello again"));
    seen.extend(second.until(|frame| matches!(frame, Frame::Reply { id: 1, .. })));
    assert!(
        matches!(reply(&seen, 1), Some(Outcome::Err(reason)) if reason.contains("still stopping")),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(turn_completed),
        "the turn waits for its last run"
    );

    std::fs::remove_file(&unkillable).unwrap();
    let frames = second.until(turn_completed);
    assert!(gone(child), "the turn ends only once the child is gone");
    assert!(
        events(&frames).iter().any(|(_, e)| matches!(
            e,
            Event::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }
        )),
        "it ends as Quit ends it: {frames:?}"
    );
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "faults exist only in debug builds")]
fn a_backlog_past_its_limit_refuses_new_turns_and_lets_the_running_one_end() {
    let faults = tempfile::tempdir().unwrap();
    let machine = Machine::new()
        .with("KEYSTONE_HOST_FAULTS", &faults.path().to_string_lossy())
        .with("KEYSTONE_HOST_JOURNAL_CAP", "1");
    let failing = faults.path().join("append");
    let running = folder(&[("delay-ms", "20")]);
    let refused = folder(&[("delay-ms", "1")]);
    let mut raw = machine.raw();
    raw.hello("app-1", None);
    raw.replay();
    std::fs::write(&failing, "").unwrap();
    raw.send(&send_call(1, running.path(), "hello long"));
    raw.until(|frame| matches!(frame, Frame::Warning { message, .. } if message.contains("couldn't save")));
    for id in [2, 3] {
        raw.send(&send_call_to(id, SESSION + 1, refused.path(), "hello"));
    }
    let frames = raw.until(|frame| matches!(frame, Frame::Reply { id: 3, .. }));
    for id in [2, 3] {
        assert!(
            matches!(reply(&frames, id), Some(Outcome::Err(reason))
                if reason.contains("starts no more turns") && reason.contains("test fault")),
            "call {id}: {frames:?}"
        );
    }
    assert!(pids(refused.path()).is_empty(), "no provider started");
    let log = std::fs::read_to_string(machine.dir.path().join("host.log")).unwrap();
    assert_eq!(
        log.matches("no turn starts").count(),
        1,
        "logged once: {log}"
    );

    std::fs::remove_file(&failing).unwrap();
    let frames = raw.until(turn_completed);
    assert!(
        events(&frames)
            .iter()
            .any(|(session, e)| *session == SESSION
                && matches!(
                    e,
                    Event::TurnCompleted {
                        status: TurnStatus::Completed,
                        ..
                    }
                ))
    );
    raw.send(&send_call_to(4, SESSION + 1, refused.path(), "hello"));
    let frames = raw.until(turn_completed);
    assert_eq!(reply(&frames, 4), Some(&Outcome::Ok(())));
}

#[test]
fn with_no_app_attached_a_finished_turn_ends_its_process_then_the_daemon() {
    let machine = Machine::new().with("KEYSTONE_HOST_IDLE_MS", "300");
    let dir = folder(&[("delay-ms", "10")]);
    let mut first = machine.client("app-1");
    first.connected();
    first.send(dir.path(), "hello long").unwrap();
    first.wait("streaming", |events, _| {
        events
            .iter()
            .any(|(_, e)| matches!(e, Event::ItemDelta { .. }))
    });
    let providers = pids(dir.path());
    assert!(providers.iter().any(|pid| alive(*pid)));
    let seen = std::mem::take(&mut first.events);
    drop(first);
    wait_for("the provider to exit after its turn", || {
        providers.iter().all(|pid| !alive(*pid))
    });
    wait_for("the daemon to exit", || machine.daemon().is_none());

    let mut next = machine.client("app-1");
    next.events = seen;
    next.connected();
    assert_eq!(next.turn_ended(), TurnStatus::Completed);
    assert_whole_turn(&next.events);
}

/// A host that is only a script: once `go` exists it writes `frames`, then keeps every
/// frame the link sends in `sent`.
struct Scripted {
    dir: tempfile::TempDir,
}

impl Scripted {
    fn new(frames: &[Frame]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let text: Vec<u8> = frames
            .iter()
            .flat_map(|frame| frame::encode(frame).unwrap())
            .collect();
        std::fs::write(dir.path().join("frames"), text).unwrap();
        Self { dir }
    }

    fn argv(&self) -> Vec<OsString> {
        let script =
            r#"while [ ! -e "$0/go" ]; do sleep 0.01; done; cat "$0/frames"; exec cat > "$0/sent""#;
        vec![
            "sh".into(),
            "-c".into(),
            script.into(),
            self.dir.path().into(),
        ]
    }

    fn go(&self) {
        std::fs::write(self.dir.path().join("go"), "").unwrap();
    }

    /// The event cursor of every `ack` the link sent so far.
    fn acks(&self) -> Vec<u64> {
        std::fs::read_to_string(self.dir.path().join("sent"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| match frame::parse(line.as_bytes()) {
                Ok(Frame::Ack { seq, .. }) => Some(seq),
                _ => None,
            })
            .collect()
    }
}

fn welcome(latest: u64) -> Frame {
    Frame::Welcome(Welcome {
        protocol: frame::PROTOCOL,
        version: "test".into(),
        os: "test".into(),
        arch: "test".into(),
        hostname: "test".into(),
        providers: Vec::new(),
        oldest: 1,
        latest,
    })
}

fn note(seq: u64, provider: Provider) -> Frame {
    Frame::Event(Routed {
        seq,
        provider,
        account: None,
        session: SESSION,
        event: Event::Warning {
            message: format!("event {seq}"),
        },
    })
}

fn notes(seqs: &[u64]) -> Vec<(u64, Event)> {
    seqs.iter()
        .map(|seq| {
            let message = format!("event {seq}");
            (SESSION, Event::Warning { message })
        })
        .collect()
}

fn connection(link: &Link, provider: Provider) -> (RemoteConnection, Receiver<()>) {
    let (tx, wake) = channel();
    let wake_up = Arc::new(move || {
        let _ = tx.send(());
    });
    (RemoteConnection::new(link, provider, None, wake_up), wake)
}

/// Drains `conn` until it has handed over `count` events.
fn drain_all(conn: &RemoteConnection, count: usize) -> Vec<(u64, Event)> {
    let mut events = Vec::new();
    wait_for("the connection's events", || {
        events.extend(conn.drain());
        events.len() >= count
    });
    events
}

#[test]
fn a_transport_that_stops_reading_blocks_no_caller_and_is_replaced() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "1")]);
    let mut client = machine.client("app-1");
    client.connected();
    let long = format!("hello long {}", "x".repeat(1 << 20));
    client.link.set_silence(Duration::from_secs(2));
    let stopped = client.link.transport_pid().unwrap() as i32;
    // SAFETY: kill only signals; the pid is this link's own unreaped child.
    unsafe { libc::kill(stopped, libc::SIGSTOP) };
    let started = Instant::now();
    client.send(dir.path(), &long).unwrap();
    client.conn.set_approval(SESSION, ApprovalState::default());
    client.drain();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "send, a report and drain returned in {:?}",
        started.elapsed()
    );
    client.wait("the watchdog to end the stopped transport", |_, status| {
        *status == Status::Reconnecting
    });
    client.link.set_silence(Duration::from_secs(15));
    client.connected();
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
    assert_whole_turn(&client.events);
    assert_eq!(lines(dir.path(), "messages").len(), 1);

    let stopped = client.link.transport_pid().unwrap() as i32;
    // SAFETY: as above.
    unsafe { libc::kill(stopped, libc::SIGSTOP) };
    client.send(dir.path(), &long).unwrap();
    let started = Instant::now();
    drop(client);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "dropping the link returned in {:?}",
        started.elapsed()
    );
    wait_for("the stopped transport to be ended", || !alive(stopped));
}

#[test]
fn a_call_the_host_refuses_shows_in_its_session_later() {
    let machine = Machine::new();
    let dir = folder(&[]);
    let mut client = machine.client("app-1");
    client.connected();
    let (codex, _wake) = connection(&client.link, Provider::Codex);
    let sent = codex.send(
        SESSION,
        &dir.path().to_string_lossy(),
        None,
        &ModelChoice::default(),
        ApprovalState::default(),
        "hello".into(),
        Vec::new(),
    );
    assert_eq!(sent, Ok(()), "send returns once the call is queued");
    let events = drain_all(&codex, 1);
    assert!(
        matches!(&events[..], [(SESSION, Event::Failed { message })] if message.contains("Codex")),
        "{events:?}"
    );
}

#[test]
fn the_acknowledgement_is_the_prefix_every_connection_drained() {
    let host = Scripted::new(&[
        welcome(3),
        note(1, Provider::Claude),
        note(2, Provider::Codex),
        note(3, Provider::Claude),
        Frame::Synced { seq: 3 },
    ]);
    let link = Link::new(host.argv(), "app-1".into());
    let (claude, _) = connection(&link, Provider::Claude);
    let (codex, _) = connection(&link, Provider::Codex);
    host.go();
    assert_eq!(drain_all(&claude, 2), notes(&[1, 3]));
    wait_for("the acknowledgement of 1", || {
        host.acks().last() == Some(&1)
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        host.acks().iter().max(),
        Some(&1),
        "2 waits in the Codex connection"
    );
    assert_eq!(drain_all(&codex, 1), notes(&[2]));
    wait_for("the acknowledgement of 3", || {
        host.acks().last() == Some(&3)
    });
}

#[test]
fn events_for_a_route_with_no_connection_wait_without_holding_the_ack() {
    let host = Scripted::new(&[
        welcome(3),
        note(1, Provider::Claude),
        note(2, Provider::Codex),
        note(3, Provider::Claude),
        Frame::Synced { seq: 3 },
    ]);
    let link = Link::new(host.argv(), "app-1".into());
    let (claude, _) = connection(&link, Provider::Claude);
    host.go();
    assert_eq!(drain_all(&claude, 2), notes(&[1, 3]));
    wait_for("the acknowledgement of 3", || {
        host.acks().last() == Some(&3)
    });
    let (codex, wake) = connection(&link, Provider::Codex);
    wake.recv_timeout(WAIT)
        .expect("a connection that registers is woken for what waited");
    assert_eq!(codex.drain(), notes(&[2]));
}

#[test]
fn a_replaced_connection_leaves_its_successor_registered() {
    let host = Scripted::new(&[
        welcome(1),
        note(1, Provider::Claude),
        Frame::Synced { seq: 1 },
    ]);
    let link = Link::new(host.argv(), "app-1".into());
    let (old, _) = connection(&link, Provider::Claude);
    let (new, wake) = connection(&link, Provider::Claude);
    drop(old);
    host.go();
    wake.recv_timeout(WAIT).expect("the successor is woken");
    assert_eq!(drain_all(&new, 1), notes(&[1]));
}

#[test]
fn model_effort_and_speed_reach_the_host_on_send_and_set_choice() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "1")]);
    let mut client = machine.client("choice-app");
    client.connected();
    let choice = ModelChoice {
        model: Some("fake-opus".into()),
        effort: Some("high".into()),
        speed: Some("fast".into()),
    };
    client
        .conn
        .send(
            SESSION,
            &dir.path().to_string_lossy(),
            None,
            &choice,
            ApprovalState::default(),
            "first".into(),
            Vec::new(),
        )
        .unwrap();
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
    let reply = |events: &[(u64, Event)]| {
        events
            .iter()
            .find_map(|(_, event)| match event {
                Event::ItemCompleted(item) if item.kind == ItemKind::AgentMessage => {
                    Some(item.text.clone())
                }
                _ => None,
            })
            .expect("a completed reply")
    };
    let first = reply(&client.events);
    assert!(first.contains("**Model:** `fake-opus`"), "{first}");
    assert!(first.contains("**Effort:** `high`"), "{first}");
    assert!(first.contains("**Fast:** `on`"), "{first}");
    let providers = pids(dir.path());
    client.events.clear();
    client.conn.set_choice(
        SESSION,
        ModelChoice {
            model: Some("fake-haiku".into()),
            effort: Some("low".into()),
            speed: Some("standard".into()),
        },
    );
    client.conn.command(Command::Send {
        session: SESSION,
        text: "second".into(),
        images: Vec::new(),
    });
    assert_eq!(client.turn_ended(), TurnStatus::Completed);
    let second = reply(&client.events);
    assert!(second.contains("**Model:** `fake-haiku`"), "{second}");
    assert!(second.contains("**Effort:** `low`"), "{second}");
    assert!(!second.contains("**Fast:** `on`"), "{second}");
    assert_eq!(
        pids(dir.path()),
        providers,
        "the choice changed in the existing process"
    );
    assert_eq!(lines(dir.path(), "messages"), ["\"first\"", "\"second\""]);
}

#[test]
fn a_real_host_keeps_received_events_until_drained_across_reconnect() {
    let machine = Machine::new();
    let dir = folder(&[("delay-ms", "5")]);
    let mut client = machine.client("held-ack-app");
    client.connected();
    client.send(dir.path(), "hello long").unwrap();
    wait_for("the turn start to be received", || {
        client.conn.is_running(SESSION)
    });
    wait_for("the turn end to be received without draining", || {
        !client.conn.is_running(SESSION)
    });
    let cursor = || {
        let db = rusqlite::Connection::open_with_flags(
            machine.dir.path().join("journal.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.query_row(
            "SELECT latest, acked FROM clients WHERE client = 'held-ack-app'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap()
    };
    let (received, acked) = cursor();
    assert!(received > 0);
    assert_eq!(acked, 0);
    let transport = client.link.transport_pid().unwrap() as i32;
    // SAFETY: the transport is this link's own unreaped child.
    unsafe { libc::kill(transport, libc::SIGKILL) };
    wait_for("the old transport to be replaced", || {
        client.link.status() == Status::Connected
            && client.link.transport_pid() != Some(transport as u32)
    });
    assert_eq!(
        cursor().1,
        0,
        "reconnect uses received, but acknowledges drained"
    );
    client.drain();
    assert_whole_turn(&client.events);
    wait_for("the drained cursor to reach the host", || {
        cursor().1 >= received
    });
    assert_eq!(lines(dir.path(), "messages").len(), 1);
}

#[test]
fn every_four_event_route_assignment_acknowledges_only_the_drained_prefix() {
    for mask in 0..16 {
        let routes: Vec<_> = (0..4)
            .map(|index| {
                if mask & (1 << index) == 0 {
                    Provider::Claude
                } else {
                    Provider::Codex
                }
            })
            .collect();
        for first in [Provider::Claude, Provider::Codex] {
            let mut frames = vec![welcome(4)];
            frames.extend(
                routes
                    .iter()
                    .enumerate()
                    .map(|(index, provider)| note(index as u64 + 1, *provider)),
            );
            frames.push(Frame::Synced { seq: 4 });
            let host = Scripted::new(&frames);
            let link = Link::new(host.argv(), "generated-ack-app".into());
            let (claude, _) = connection(&link, Provider::Claude);
            let (codex, _) = connection(&link, Provider::Codex);
            host.go();
            wait_for("all generated frames to be received", || {
                link.status() == Status::Connected
            });
            let (drained, held) = if first == Provider::Claude {
                (&claude, &codex)
            } else {
                (&codex, &claude)
            };
            let expected: Vec<_> = routes
                .iter()
                .enumerate()
                .filter_map(|(index, provider)| (*provider == first).then_some(index as u64 + 1))
                .collect();
            assert_eq!(drain_all(drained, expected.len()), notes(&expected));
            let prefix = routes
                .iter()
                .position(|provider| *provider != first)
                .unwrap_or(4) as u64;
            wait_for("the generated drained prefix acknowledgement", || {
                host.acks().last().copied().unwrap_or(0) == prefix
            });
            assert!(
                host.acks().iter().all(|seq| *seq <= prefix),
                "mask {mask}, first {first:?}"
            );
            let remaining: Vec<_> = routes
                .iter()
                .enumerate()
                .filter_map(|(index, provider)| (*provider != first).then_some(index as u64 + 1))
                .collect();
            assert_eq!(drain_all(held, remaining.len()), notes(&remaining));
            wait_for("all generated events to be acknowledged", || {
                host.acks().last() == Some(&4)
            });
        }
    }
}

#[test]
fn a_send_to_a_folder_the_machine_lacks_fails_in_its_session() {
    let machine = Machine::new();
    let mut client = machine.client("app-1");
    client.connected();
    let welcome = client.link.welcome().expect("the host said who it is");
    assert_eq!(welcome.protocol, keystone_agent::remote::PROTOCOL);
    client
        .send(Path::new("/nowhere/keystone-test"), "hello")
        .unwrap();
    client.wait("the host to refuse the folder", |events, _| {
        events.iter().any(|(session, event)| {
            *session == SESSION
                && matches!(event, Event::Failed { message }
                    if message == "/nowhere/keystone-test isn't a folder on this machine")
        })
    });
}

#[test]
fn a_first_attempt_that_ends_before_the_host_answers_says_why() {
    let argv = [
        "sh",
        "-c",
        "echo 'ssh: connect to host desk port 22: Connection refused' >&2; exit 255",
    ]
    .map(OsString::from)
    .to_vec();
    let link = Link::new(argv, "app-1".into());
    wait_for("the link to give a reason", || link.trouble().is_some());
    assert_eq!(
        link.trouble().as_deref(),
        Some("ssh: connect to host desk port 22: Connection refused")
    );
    assert_eq!(link.status(), Status::Reconnecting);
    assert!(link.welcome().is_none());

    let silent = Link::new(["sleep", "30"].map(OsString::from).to_vec(), "app-1".into());
    silent.set_silence(Duration::from_millis(300));
    wait_for("the silent link to give a reason", || {
        silent.trouble().is_some()
    });
    assert!(
        silent
            .trouble()
            .unwrap()
            .starts_with("The machine didn't answer")
    );
    let started = Instant::now();
    silent.close(Duration::from_secs(5));
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// `attach` hands each frame on at once. A `hello` that waited in the pipe until the
/// daemon next wrote, as when `attach` spliced its stdin into the socket, left a link
/// connecting for 5 to 15 s.
#[test]
fn attach_hands_a_hello_to_the_daemon_at_once() {
    let machine = Machine::new();
    drop(machine.raw().hello("warm", None));
    for n in 0..5 {
        let started = Instant::now();
        let mut raw = machine.raw();
        raw.hello(&format!("client-{n}"), None);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "hello {n} took {:?}",
            started.elapsed()
        );
        raw.kill();
    }
}

/// A transport that refuses and leaves a process holding its stderr, as a
/// `ProxyCommand` can, still fails at once with its reason.
#[test]
fn a_refusal_whose_stderr_stays_open_still_says_why_at_once() {
    let argv = [
        "sh",
        "-c",
        "read hello; echo '{\"t\":\"welcome\",\"protocol\":99,\"version\":\"9.0.0\",\"os\":\"linux\",\"arch\":\"x86_64\",\"hostname\":\"desk\",\"providers\":[],\"oldest\":0,\"latest\":0}'; (sleep 30 &); sleep 30",
    ]
    .map(OsString::from)
    .to_vec();
    let link = Link::new(argv, "app-1".into());
    let started = Instant::now();
    wait_for("the link to fail", || {
        matches!(link.status(), Status::Failed(_))
    });
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let Status::Failed(reason) = link.status() else {
        unreachable!()
    };
    assert!(reason.contains("speaks protocol 99"), "{reason}");
}
