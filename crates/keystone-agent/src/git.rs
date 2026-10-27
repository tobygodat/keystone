//! The user's own `git`, run as a plain child process in a repository's top folder,
//! with the user's config, hooks and signing. Keystone holds no credentials. Every
//! call blocks, so callers run them on a [`crate::worker::Worker`].

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Bytes of a failed command's output kept for display, from its end.
const OUTPUT_TAIL: usize = 64 * 1024;
/// How long output is still read after git exits. A process a hook left running in
/// the background can hold the pipes open for as long as it lives.
const OUTPUT_GRACE: Duration = Duration::from_secs(1);

/// Variables that point git at a repository other than the folder it runs in. A
/// Keystone started from inside a git hook would otherwise act on that repository.
/// From `git rev-parse --local-env-vars`.
const LOCAL_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_INTERNAL_SUPER_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// `git` could not be started at all.
    Spawn(String),
    /// git ran and failed. `output` is its stdout and stderr as they arrived,
    /// including any hook's output. `code` is its exit code, if it exited.
    Failed {
        command: String,
        output: String,
        code: Option<i32>,
    },
    /// Its [`Cancel`] fired: it was stopped, or never started.
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "Couldn't run git: {error}"),
            Self::Cancelled => f.write_str("Cancelled."),
            Self::Failed {
                command, output, ..
            } if output.trim().is_empty() => {
                write!(f, "{command} failed")
            }
            Self::Failed { output, .. } => f.write_str(output.trim_end()),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// The branch line of `git status --branch`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Branch {
    /// The checked-out branch, or `None` on a detached HEAD.
    pub head: Option<String>,
    /// The commit HEAD names, or `None` before the first commit.
    pub oid: Option<String>,
    pub upstream: Option<String>,
    /// The upstream is set but its branch is gone from the remote.
    pub gone: bool,
    /// Commits ahead of and behind the upstream, when there is one.
    pub ahead: u32,
    pub behind: u32,
    /// The branch is on the remote it pushes to, where a pull request comes from:
    /// with a push remote configured, that remote has a branch of the same name;
    /// otherwise its upstream is a branch of the same name, as after a first push.
    /// A branch started from another, such as `origin/main`, is not. A push remote
    /// whose fetch refspecs keep no copy of the branch can't be checked, so the branch
    /// counts as published and gh says if it is not.
    pub published: bool,
    /// Commits not yet on the published branch.
    pub unpushed: u32,
}

/// One-letter states of porcelain v2: `.` unchanged, `M` modified, `T` type changed,
/// `A` added, `D` deleted, `R` renamed, `C` copied, `U` unmerged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Tracked {
        path: PathBuf,
        /// A rename's or copy's source.
        original: Option<PathBuf>,
        staged: char,
        unstaged: char,
    },
    Conflicted {
        path: PathBuf,
        /// Two letters such as `UU` or `AA`, as `git status --short` shows them.
        state: String,
    },
    /// Untracked, with a folder of untracked files listed once, ending in `/`.
    Untracked { path: PathBuf },
}

impl Entry {
    pub fn path(&self) -> &Path {
        match self {
            Self::Tracked { path, .. }
            | Self::Conflicted { path, .. }
            | Self::Untracked { path } => path,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// The repository's top folder. Entry paths are relative to it.
    pub root: PathBuf,
    pub branch: Branch,
    pub entries: Vec<Entry>,
    /// The user's `status.showUntrackedFiles` is `no`, so untracked files are left out.
    pub untracked_hidden: bool,
}

/// `git` in `folder`, in a session of its own with no controlling terminal, so a hook,
/// a signing program or a credential helper that asks on the terminal fails at once
/// instead of waiting for an answer nobody can type.
pub(crate) fn git(folder: &Path) -> Command {
    program(Path::new("git"), folder)
}

/// `program` in `folder`, set up as [`git`] describes. Programs that run git
/// themselves, such as `gh`, need the same.
pub(crate) fn program(program: &Path, folder: &Path) -> Command {
    let mut command = Command::new(program);
    for name in LOCAL_ENV {
        command.env_remove(name);
    }
    command
        .current_dir(folder)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

fn describe(command: &Command) -> String {
    let program = Path::new(command.get_program())
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned());
    std::iter::once(program)
        .chain(args)
        .collect::<Vec<_>>()
        .join(" ")
}

fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(OUTPUT_TAIL);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// How a command's output is collected.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    /// Stdout alone is returned, so warnings on stderr never reach a parser. A
    /// failure reports stderr.
    Separate,
    /// Stdout and stderr in one stream, in the order they were written, as a
    /// terminal shows hook output.
    Merged,
}

/// Stops the commands run with it, from any thread: the one running is killed with
/// whatever it started, and later ones fail before they start. Clones share it.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// When a running command is given up.
#[derive(Clone, Copy)]
pub(crate) enum Until<'a> {
    Exit,
    /// Fails as having had no answer.
    Deadline(Duration),
    /// Fails with [`Error::Cancelled`].
    Cancelled(&'a Cancel),
}

/// Runs `command` with `input` on stdin.
pub(crate) fn run(command: Command, input: Option<Vec<u8>>, output: Output) -> Result<Vec<u8>> {
    run_until(command, input, output, Until::Exit)
}

/// [`run`], but a command is killed, with whatever it started, and fails once
/// `until` gives it up.
pub(crate) fn run_until(
    mut command: Command,
    input: Option<Vec<u8>>,
    output: Output,
    until: Until,
) -> Result<Vec<u8>> {
    if let Until::Cancelled(cancel) = until
        && cancel.is_cancelled()
    {
        return Err(Error::Cancelled);
    }
    let name = describe(&command);
    let spawn = |error: io::Error| Error::Spawn(error.to_string());
    let (reader, writer) = io::pipe().map_err(spawn)?;
    match output {
        Output::Merged => command
            .stdout(writer.try_clone().map_err(spawn)?)
            .stderr(writer),
        Output::Separate => command.stdout(Stdio::piped()).stderr(writer),
    };
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().map_err(spawn)?;
    // The pipe ends only once every writer is closed, so the command's copies go first.
    drop(command);
    let feeder = child.stdin.take().zip(input).map(|(mut stdin, input)| {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        })
    });
    let stdout = child.stdout.take().map(drain);
    let written = drain(reader);
    let given_up = |started: Instant| match until {
        Until::Exit => None,
        Until::Deadline(limit) => (started.elapsed() >= limit)
            .then(|| Error::Spawn(format!("no answer after {} seconds", limit.as_secs()))),
        Until::Cancelled(cancel) => cancel.is_cancelled().then_some(Error::Cancelled),
    };
    let status = match until {
        Until::Exit => child.wait().map_err(spawn)?,
        _ => {
            let started = Instant::now();
            // Short at first under a cancel, so each quick command of a restore is not
            // held up by the polling.
            let mut pause = match until {
                Until::Cancelled(_) => Duration::from_millis(1),
                _ => Duration::from_millis(50),
            };
            loop {
                if let Some(status) = child.try_wait().map_err(spawn)? {
                    break status;
                }
                if let Some(error) = given_up(started) {
                    // A command from `program` leads its own process group, so whatever it
                    // started goes with it and lets go of the output pipes.
                    // SAFETY: kill has no memory effects; a pid that leads no group fails.
                    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(50));
            }
        }
    };
    if let Some(feeder) = feeder {
        let _ = feeder.join();
    }
    let deadline = Instant::now() + OUTPUT_GRACE;
    let written = collect(written, deadline).map_err(spawn)?;
    if let Some(stdout) = stdout {
        let stdout = collect(stdout, deadline).map_err(spawn)?;
        if status.success() {
            return Ok(stdout);
        }
    }
    if !status.success() {
        return Err(Error::Failed {
            command: name,
            output: tail(&written),
            code: status.code(),
        });
    }
    Ok(written)
}

/// Reads `pipe` on a thread of its own until every writer has closed it.
fn drain(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = vec![0; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    if send.send(Ok(chunk[..read].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    let _ = send.send(Err(error));
                    break;
                }
            }
        }
    });
    receive
}

/// What [`drain`] read, until the pipe closed or `deadline` passed.
fn collect(pipe: mpsc::Receiver<io::Result<Vec<u8>>>, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Ok(chunk) = pipe.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        bytes.extend(chunk?);
    }
    Ok(bytes)
}

/// The top folder of the repository or linked worktree holding `folder`.
pub fn top_folder(folder: &Path) -> Result<PathBuf> {
    let mut command = git(folder);
    command.args(["rev-parse", "--show-toplevel"]);
    let output = run(command, None, Output::Separate)?;
    let line = output.strip_suffix(b"\n").unwrap_or(&output);
    Ok(PathBuf::from(OsStr::from_bytes(line)))
}

/// The status of the repository holding `folder`. It takes no index lock, so a
/// refresh never gets in the way of git commands the user or an agent runs.
///
/// Untracked files are listed one by one, so each can be staged on its own, unless
/// the user's `status.showUntrackedFiles` is `no`, as some home-folder repositories
/// set to keep status cheap.
pub fn status(folder: &Path) -> Result<Status> {
    let root = top_folder(folder)?;
    let mut setting = git(&root);
    setting.args(["config", "--get", "status.showUntrackedFiles"]);
    let untracked_hidden = match run(setting, None, Output::Separate) {
        Ok(value)
            if matches!(
                value.trim_ascii().to_ascii_lowercase().as_slice(),
                b"no" | b"false" | b"off" | b"0"
            ) =>
        {
            true
        }
        _ => false,
    };
    let untracked = if untracked_hidden { "no" } else { "all" };
    let mut command = git(&root);
    command.args([
        "--no-optional-locks",
        "status",
        "--porcelain=v2",
        "-z",
        "--branch",
        &format!("--untracked-files={untracked}"),
    ]);
    let output = run(command, None, Output::Separate)?;
    let (mut branch, entries) = parse_status(&output);
    if let Some(head) = &branch.head {
        (branch.published, branch.unpushed) = publication(&root, head, &branch);
    }
    Ok(Status {
        root,
        branch,
        entries,
        untracked_hidden,
    })
}

/// Whether `head` is published and how many commits it has that are not, as
/// [`Branch::published`] describes. A config or ref that can't be read counts as
/// not published, so it never hides the list.
fn publication(root: &Path, head: &str, branch: &Branch) -> (bool, u32) {
    if let Ok(Some(remote)) = push_remote(root, head) {
        let Some(copy) = remote_copy(root, &remote, head) else {
            return (true, 0);
        };
        let mut command = git(root);
        command.args([
            "rev-list",
            "--count",
            &format!("{copy}..refs/heads/{head}"),
            "--",
        ]);
        return match run(command, None, Output::Separate) {
            Ok(count) => (true, text(&count).trim().parse().unwrap_or(0)),
            Err(_) => (false, 0),
        };
    }
    let own = branch.upstream.is_some()
        && !branch.gone
        && config(root, &format!("branch.{head}.merge"))
            .ok()
            .flatten()
            .is_some_and(|merge| merge == format!("refs/heads/{head}"));
    (own, if own { branch.ahead } else { 0 })
}

/// Where `remote`'s fetch refspecs keep their copy of its branch `head`, such as
/// `refs/remotes/origin/feature`. `None` when none of them fetches it.
fn remote_copy(root: &Path, remote: &str, head: &str) -> Option<String> {
    let mut command = git(root);
    command.args(["config", "--get-all", &format!("remote.{remote}.fetch")]);
    let refspecs = text(&run(command, None, Output::Separate).ok()?);
    let source = format!("refs/heads/{head}");
    refspecs.lines().find_map(|refspec| {
        let (from, to) = refspec.trim().trim_start_matches('+').split_once(':')?;
        match (from.split_once('*'), to.split_once('*')) {
            (None, None) => (from == source).then(|| to.to_owned()),
            (Some((prefix, suffix)), Some((into, after))) => {
                let name = source.strip_prefix(prefix)?.strip_suffix(suffix)?;
                Some(format!("{into}{name}{after}"))
            }
            _ => None,
        }
    })
}

/// The branch checked out in `root`, or `None` on a detached HEAD.
pub fn current_branch(root: &Path) -> Result<Option<String>> {
    let mut command = git(root);
    command.args(["symbolic-ref", "--quiet", "HEAD"]);
    match run(command, None, Output::Separate) {
        Ok(name) => {
            let name = text(&name);
            let name = name.trim_end();
            Ok(Some(
                name.strip_prefix("refs/heads/").unwrap_or(name).to_owned(),
            ))
        }
        Err(Error::Failed { output, .. }) if output.trim().is_empty() => Ok(None),
        Err(error) => Err(error),
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub(crate) fn path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// Parses `git status --porcelain=v2 -z --branch`. Lines it does not know are skipped.
pub fn parse_status(output: &[u8]) -> (Branch, Vec<Entry>) {
    let mut branch = Branch::default();
    let mut entries = Vec::new();
    let mut fields = output.split(|byte| *byte == 0);
    while let Some(line) = fields.next() {
        let Some((&kind, rest)) = line.split_first() else {
            continue;
        };
        let rest = rest.strip_prefix(b" ").unwrap_or(rest);
        match kind {
            b'#' => {
                let header = text(rest);
                let (key, value) = header.split_once(' ').unwrap_or((&header, ""));
                match key {
                    "branch.oid" if value != "(initial)" => branch.oid = Some(value.into()),
                    "branch.head" if value != "(detached)" => branch.head = Some(value.into()),
                    "branch.upstream" => {
                        branch.upstream = Some(value.into());
                        branch.gone = true;
                    }
                    "branch.ab" => {
                        branch.gone = false;
                        for count in value.split(' ') {
                            if let Some(n) = count.strip_prefix('+') {
                                branch.ahead = n.parse().unwrap_or(0);
                            } else if let Some(n) = count.strip_prefix('-') {
                                branch.behind = n.parse().unwrap_or(0);
                            }
                        }
                    }
                    _ => {}
                }
            }
            // `1 XY sub mH mI mW hH hI path`
            b'1' => {
                let mut parts = rest.splitn(8, |byte| *byte == b' ');
                let xy = parts.next().unwrap_or_default();
                if let (Some(&[staged, unstaged]), Some(file)) = (Some(xy), parts.nth(6)) {
                    entries.push(Entry::Tracked {
                        path: path(file),
                        original: None,
                        staged: staged.into(),
                        unstaged: unstaged.into(),
                    });
                }
            }
            // `2 XY sub mH mI mW hH hI Xscore path`, then the source as its own field.
            b'2' => {
                let mut parts = rest.splitn(9, |byte| *byte == b' ');
                let xy = parts.next().unwrap_or_default();
                let file = parts.nth(7);
                let original = fields.next();
                if let (&[staged, unstaged], Some(file)) = (xy, file) {
                    entries.push(Entry::Tracked {
                        path: path(file),
                        original: original.map(path),
                        staged: staged.into(),
                        unstaged: unstaged.into(),
                    });
                }
            }
            // `u XY sub m1 m2 m3 mW h1 h2 h3 path`
            b'u' => {
                let mut parts = rest.splitn(10, |byte| *byte == b' ');
                let xy = parts.next().unwrap_or_default();
                if let Some(file) = parts.nth(8) {
                    entries.push(Entry::Conflicted {
                        path: path(file),
                        state: text(xy),
                    });
                }
            }
            b'?' => entries.push(Entry::Untracked { path: path(rest) }),
            _ => {}
        }
    }
    (branch, entries)
}

/// Each path as a literal pathspec, NUL-terminated. Marking each one literal, rather
/// than setting `GIT_LITERAL_PATHSPECS`, keeps the user's hooks reading globs as globs.
fn pathspecs(paths: &[PathBuf]) -> Vec<u8> {
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(b":(literal)");
        input.extend_from_slice(path.as_os_str().as_bytes());
        input.push(0);
    }
    input
}

/// Runs `args` on `paths`, given on stdin so any name and any count work.
fn with_paths(root: &Path, args: &[&str], paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut command = git(root);
    command
        .args(args)
        .args(["--pathspec-from-file=-", "--pathspec-file-nul"]);
    run(command, Some(pathspecs(paths)), Output::Merged).map(drop)
}

/// Whether HEAD names a commit, which it does not before the first one.
fn born(root: &Path) -> Result<bool> {
    let mut command = git(root);
    command.args(["rev-parse", "--quiet", "--verify", "HEAD^{commit}"]);
    match run(command, None, Output::Separate) {
        Ok(_) => Ok(true),
        Err(Error::Failed { output, .. }) if output.trim().is_empty() => Ok(false),
        Err(error) => Err(error),
    }
}

/// `git add --all` for `paths`, relative to `root`: new, changed and deleted files.
/// Staging a conflicted file marks it resolved.
pub fn stage(root: &Path, paths: &[PathBuf]) -> Result<()> {
    with_paths(root, &["add", "--all"], paths)
}

/// `git restore --staged` for `paths`, relative to `root`. Before the first commit
/// there is no HEAD to restore from, so the paths leave the index instead, whatever
/// the working files hold now; the working files are never touched.
pub fn unstage(root: &Path, paths: &[PathBuf]) -> Result<()> {
    if born(root)? {
        with_paths(root, &["restore", "--staged"], paths)
    } else {
        with_paths(root, &["rm", "--cached", "-r", "-q", "-f"], paths)
    }
}

/// `git commit` of what is staged, with `message` as given. The user's hooks and
/// signing settings apply; a failure carries their output.
pub fn commit(root: &Path, message: &str) -> Result<()> {
    let mut command = git(root);
    command.args(["commit", "--file=-"]);
    run(command, Some(message.as_bytes().to_vec()), Output::Merged)?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushError {
    /// The branch needs a remote and the repository has none.
    NoRemote,
    /// The branch needs a remote, and none of several is `origin` or configured as
    /// the one to push to.
    UnclearRemote(Vec<String>),
    /// git ran and failed; a rejected push says so in its output.
    Failed(Error),
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRemote => f.write_str("This repository has no remote."),
            Self::UnclearRemote(remotes) => write!(
                f,
                "This repository has several remotes ({}) and none is set to push to.",
                remotes.join(", ")
            ),
            Self::Failed(error) => error.fmt(f),
        }
    }
}

impl From<Error> for PushError {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

/// `git config --get key`, or `None` when it is not set.
fn config(root: &Path, key: &str) -> Result<Option<String>> {
    let mut command = git(root);
    command.args(["config", "--get", key]);
    match run(command, None, Output::Separate) {
        Ok(value) => Ok(Some(text(&value).trim_end().to_owned())),
        Err(Error::Failed { output, .. }) if output.trim().is_empty() => Ok(None),
        Err(error) => Err(error),
    }
}

/// The default branch of the checked-out branch's remote, such as `main`, as the
/// remote's `HEAD` was last fetched. `None` when it is not known.
pub fn default_branch(root: &Path) -> Option<String> {
    let branch = current_branch(root).ok()??;
    let remote = config(root, &format!("branch.{branch}.remote"))
        .ok()
        .flatten()
        .filter(|remote| remote != ".")
        .unwrap_or_else(|| "origin".into());
    let mut command = git(root);
    command.args([
        "symbolic-ref",
        "--quiet",
        &format!("refs/remotes/{remote}/HEAD"),
    ]);
    let target = text(&run(command, None, Output::Separate).ok()?);
    let name = target
        .trim_end()
        .strip_prefix(&format!("refs/remotes/{remote}/"))?;
    Some(name.to_owned())
}

/// The remote the user set for pushing `branch`: its `pushRemote`, else
/// `remote.pushDefault`.
fn push_remote(root: &Path, branch: &str) -> Result<Option<String>> {
    match config(root, &format!("branch.{branch}.pushRemote"))? {
        Some(remote) => Ok(Some(remote)),
        None => config(root, "remote.pushDefault"),
    }
}

/// Pushes local `branch` to the branch of the same name on its push remote, and never
/// forces. The refspec is spelled out in full, so a configured push refspec, mirror
/// or matching setting never picks other branches, and no branch name can turn into
/// refspec syntax. The remote is the branch's `pushRemote`, then
/// `remote.pushDefault`, its upstream's remote, `origin`, or the only remote.
///
/// The pushed branch becomes the upstream when there is none, or when the upstream
/// is another branch, such as `origin/main` it was started from, and no push remote
/// is configured. A configured push remote keeps the upstream as it is. Returns git's
/// own output, which a rejected push's error carries too.
pub fn push(root: &Path, branch: &str) -> std::result::Result<String, PushError> {
    let upstream = config(root, &format!("branch.{branch}.remote"))?.filter(|r| r != ".");
    let own = config(root, &format!("branch.{branch}.merge"))?.as_deref()
        == Some(&format!("refs/heads/{branch}"));
    let configured = push_remote(root, branch)?;
    let set_upstream = upstream.is_none() || (!own && configured.is_none());
    let remote = match configured.or_else(|| upstream.clone()) {
        Some(remote) => remote,
        None => {
            let mut list = git(root);
            list.arg("remote");
            let names: Vec<String> = text(&run(list, None, Output::Separate)?)
                .lines()
                .map(str::to_owned)
                .collect();
            match names.as_slice() {
                [] => return Err(PushError::NoRemote),
                [only] => only.clone(),
                _ if names.iter().any(|name| name == "origin") => "origin".into(),
                _ => return Err(PushError::UnclearRemote(names)),
            }
        }
    };
    let mut command = git(root);
    command.args(["push", "--no-follow-tags", "--recurse-submodules=no"]);
    if set_upstream {
        command.arg("--set-upstream");
    }
    command.args([
        "--",
        &remote,
        &format!("refs/heads/{branch}:refs/heads/{branch}"),
    ]);
    Ok(text(&run(command, None, Output::Merged)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn sh(folder: &Path, args: &[&str]) -> String {
        let output = git(folder).args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        sh(dir.path(), &["init", "-q", "-b", "main"]);
        sh(dir.path(), &["config", "user.name", "Test"]);
        sh(dir.path(), &["config", "user.email", "test@example.com"]);
        sh(dir.path(), &["config", "commit.gpgsign", "false"]);
        sh(dir.path(), &["config", "core.hooksPath", ".git/hooks"]);
        dir
    }

    fn paths(status: &Status) -> Vec<String> {
        status
            .entries
            .iter()
            .map(|entry| match entry {
                Entry::Tracked {
                    path,
                    original,
                    staged,
                    unstaged,
                } => format!(
                    "{staged}{unstaged} {}{}",
                    path.display(),
                    original
                        .as_ref()
                        .map(|o| format!(" <- {}", o.display()))
                        .unwrap_or_default()
                ),
                Entry::Conflicted { path, state } => format!("{state} {}", path.display()),
                Entry::Untracked { path } => format!("?? {}", path.display()),
            })
            .collect()
    }

    #[test]
    fn parses_branch_headers_renames_conflicts_and_odd_names() {
        let output = b"# branch.oid 1234\0# branch.head feature/x\0# branch.upstream origin/feature/x\0# branch.ab +2 -3\0\
1 .M N... 100644 100644 100644 aaaa aaaa src/a file.rs\0\
2 R. N... 100644 100644 100644 aaaa aaaa R100 new name.rs\0old name.rs\0\
u UU N... 100644 100644 100644 100644 aaaa bbbb cccc both.rs\0\
? notes/\0! ignored\0";
        let (branch, entries) = parse_status(output);
        assert_eq!(
            branch,
            Branch {
                head: Some("feature/x".into()),
                oid: Some("1234".into()),
                upstream: Some("origin/feature/x".into()),
                gone: false,
                ahead: 2,
                behind: 3,
                published: false,
                unpushed: 0,
            }
        );
        assert_eq!(
            entries,
            [
                Entry::Tracked {
                    path: "src/a file.rs".into(),
                    original: None,
                    staged: '.',
                    unstaged: 'M'
                },
                Entry::Tracked {
                    path: "new name.rs".into(),
                    original: Some("old name.rs".into()),
                    staged: 'R',
                    unstaged: '.'
                },
                Entry::Conflicted {
                    path: "both.rs".into(),
                    state: "UU".into()
                },
                Entry::Untracked {
                    path: "notes/".into()
                },
            ]
        );
        let (branch, _) = parse_status(b"# branch.oid (initial)\0# branch.head (detached)\0");
        assert_eq!(branch, Branch::default());
    }

    #[test]
    fn stages_unstages_and_commits_in_the_top_folder_from_a_subfolder() {
        let dir = repository();
        let root = dir.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("keep.txt"), "one\n").unwrap();
        fs::write(root.join("gone.txt"), "bye\n").unwrap();
        fs::write(root.join("sub/edit.txt"), "a\n").unwrap();

        let status = status(&root.join("sub")).unwrap();
        assert_eq!(status.root, root.canonicalize().unwrap());
        assert_eq!(status.branch.head.as_deref(), Some("main"));
        assert_eq!(status.branch.oid, None, "unborn");
        assert_eq!(
            paths(&status),
            ["?? gone.txt", "?? keep.txt", "?? sub/edit.txt"],
            "untracked files are listed one by one, even in a new folder"
        );

        let top = status.root;
        stage(&top, &["gone.txt".into(), "sub/edit.txt".into()]).unwrap();
        fs::write(top.join("gone.txt"), "edited after staging\n").unwrap();
        unstage(&top, &["gone.txt".into()]).unwrap();
        assert_eq!(
            paths(&super::status(&top).unwrap()),
            ["A. sub/edit.txt", "?? gone.txt", "?? keep.txt"],
            "before the first commit, unstaging an edited file leaves the index"
        );
        assert_eq!(
            fs::read_to_string(top.join("gone.txt")).unwrap(),
            "edited after staging\n"
        );
        stage(&top, &["gone.txt".into(), "keep.txt".into()]).unwrap();
        commit(&top, "first\n\nbody").unwrap();
        assert_eq!(sh(&top, &["log", "-1", "--format=%B"]), "first\n\nbody\n\n");

        fs::remove_file(top.join("gone.txt")).unwrap();
        fs::write(top.join("keep.txt"), "two\n").unwrap();
        fs::write(top.join("[odd]*.txt"), "x\n").unwrap();
        sh(&top, &["mv", "sub/edit.txt", "sub/moved.txt"]);
        let now = super::status(&top).unwrap();
        assert!(now.branch.oid.is_some());
        assert_eq!(
            paths(&now),
            [
                ".D gone.txt",
                ".M keep.txt",
                "R. sub/moved.txt <- sub/edit.txt",
                "?? [odd]*.txt"
            ]
        );
        stage(&top, &["gone.txt".into(), "[odd]*.txt".into()]).unwrap();
        unstage(&top, &["sub/moved.txt".into(), "sub/edit.txt".into()]).unwrap();
        assert_eq!(
            paths(&super::status(&top).unwrap()),
            [
                "A. [odd]*.txt",
                "D. gone.txt",
                ".M keep.txt",
                ".D sub/edit.txt",
                "?? sub/moved.txt"
            ],
            "the odd name is staged literally and the rename is unstaged on both sides"
        );
    }

    #[test]
    fn a_failing_hook_keeps_its_output_and_commits_nothing() {
        let dir = repository();
        let root = dir.path();
        hook(
            root,
            "echo checking\necho 'lint failed: x.rs' >&2\nexit 1\n",
        );
        fs::write(root.join("x.rs"), "x").unwrap();
        stage(root, &["x.rs".into()]).unwrap();
        let error = commit(root, "verify commit").unwrap_err();
        assert_eq!(error.to_string(), "checking\nlint failed: x.rs");
        assert!(super::status(root).unwrap().branch.oid.is_none());
    }

    #[test]
    fn a_process_a_hook_leaves_running_does_not_hold_the_commit() {
        let dir = repository();
        let root = dir.path();
        hook(root, "echo indexing\nsleep 30 &\n");
        fs::write(root.join("x.rs"), "x").unwrap();
        stage(root, &["x.rs".into()]).unwrap();
        let started = Instant::now();
        commit(root, "background hook").unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(sh(root, &["log", "-1", "--format=%s"]), "background hook\n");
    }

    #[test]
    fn a_folder_outside_any_repository_reports_gits_message() {
        let dir = tempfile::tempdir().unwrap();
        let error = status(dir.path()).unwrap_err();
        assert!(
            matches!(&error, Error::Failed { output, .. } if output.starts_with("fatal:")),
            "{error:?}"
        );
    }

    fn hook(root: &Path, script: &str) {
        let hook = root.join(".git/hooks/pre-commit");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(&hook, format!("#!/bin/sh\n{script}")).unwrap();
        fs::set_permissions(&hook, PermissionsExt::from_mode(0o755)).unwrap();
    }

    #[test]
    fn warnings_on_stderr_never_reach_the_status_parser() {
        let dir = repository();
        let root = dir.path();
        fs::write(root.join("a.txt"), "a\n").unwrap();
        stage(root, &["a.txt".into()]).unwrap();
        commit(root, "first").unwrap();
        fs::create_dir(root.join("locked")).unwrap();
        fs::write(root.join("locked/inside"), "x").unwrap();
        fs::set_permissions(root.join("locked"), PermissionsExt::from_mode(0o000)).unwrap();
        fs::write(root.join("a.txt"), "b\n").unwrap();
        stage(root, &["a.txt".into()]).unwrap();
        let status = status(root);
        fs::set_permissions(root.join("locked"), PermissionsExt::from_mode(0o755)).unwrap();
        let status = status.unwrap();
        assert!(status.branch.oid.is_some(), "{status:?}");
        assert_eq!(paths(&status), ["M. a.txt"]);

        unstage(root, &["a.txt".into()]).unwrap();
        assert_eq!(
            paths(&super::status(root).unwrap()),
            [".M a.txt", "?? locked/inside"],
            "a repository with commits unstages with restore, never by removing the file"
        );
    }

    #[test]
    fn hooks_see_globs_as_globs_and_no_terminal() {
        let dir = repository();
        let root = dir.path();
        hook(
            root,
            "echo \"rust: $(git diff --cached --name-only -- '*.rs')\"\n\
             if (exec 3</dev/tty) 2>/dev/null; then echo terminal; else echo no terminal; fi\n\
             exit 1\n",
        );
        fs::write(root.join("x.rs"), "x").unwrap();
        stage(root, &["x.rs".into()]).unwrap();
        let error = commit(root, "verify commit").unwrap_err();
        assert_eq!(error.to_string(), "rust: x.rs\nno terminal");
    }

    fn commit_file(root: &Path, name: &str, message: &str) {
        fs::write(root.join(name), message).unwrap();
        stage(root, &[name.into()]).unwrap();
        commit(root, message).unwrap();
    }

    #[test]
    fn push_sets_an_upstream_then_pushes_and_a_rejection_forces_nothing() {
        let dir = repository();
        let root = dir.path();
        let remote = tempfile::tempdir().unwrap();
        sh(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        commit_file(root, "a.txt", "first");
        assert_eq!(push(root, "main"), Err(PushError::NoRemote));
        sh(
            root,
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        sh(root, &["switch", "-q", "-c", "feature"]);

        let output = push(root, "feature").unwrap();
        assert!(output.contains("[new branch]"), "{output}");
        let pushed = |name: &str| sh(remote.path(), &["rev-parse", name]);
        assert_eq!(pushed("feature"), sh(root, &["rev-parse", "HEAD"]));
        let branch = super::status(root).unwrap().branch;
        assert_eq!(branch.upstream.as_deref(), Some("origin/feature"));
        assert_eq!((branch.ahead, branch.behind), (0, 0));
        assert!(branch.published);

        commit_file(root, "b.txt", "second");
        assert_eq!(super::status(root).unwrap().branch.ahead, 1);
        push(root, "feature").unwrap();
        assert_eq!(pushed("feature"), sh(root, &["rev-parse", "HEAD"]));
        assert_eq!(super::status(root).unwrap().branch.ahead, 0);

        let other = tempfile::tempdir().unwrap();
        sh(
            other.path(),
            &[
                "clone",
                "-q",
                "-b",
                "feature",
                remote.path().to_str().unwrap(),
                ".",
            ],
        );
        for (key, value) in [
            ("user.name", "Other"),
            ("user.email", "o@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", ".git/hooks"),
        ] {
            sh(other.path(), &["config", key, value]);
        }
        sh(
            other.path(),
            &["commit", "-q", "--allow-empty", "-m", "theirs"],
        );
        sh(other.path(), &["push", "-q"]);
        let theirs = pushed("feature");
        commit_file(root, "c.txt", "ours");
        let Err(PushError::Failed(error)) = push(root, "feature") else {
            panic!("a diverged push must fail");
        };
        assert!(error.to_string().contains("[rejected]"), "{error}");
        assert_eq!(pushed("feature"), theirs, "nothing was forced");
    }

    #[test]
    fn a_push_sends_only_the_branch_and_no_tags() {
        let dir = repository();
        let root = dir.path();
        let origin = with_origin(root);
        sh(root, &["config", "push.followTags", "true"]);
        sh(root, &["switch", "-q", "-c", "feature"]);
        commit_file(root, "f.txt", "feature");
        sh(root, &["tag", "-a", "-m", "v1", "v1"]);
        push(root, "feature").unwrap();
        assert_eq!(
            sh(origin.path(), &["for-each-ref", "--format=%(refname)"]),
            "refs/heads/feature\nrefs/heads/main\n"
        );
    }

    #[test]
    fn a_branch_whose_upstream_is_gone_is_not_published() {
        let dir = repository();
        let root = dir.path();
        let origin = with_origin(root);
        sh(root, &["switch", "-q", "-c", "feature"]);
        push(root, "feature").unwrap();
        assert!(super::status(root).unwrap().branch.published);
        sh(origin.path(), &["branch", "-q", "-D", "feature"]);
        sh(root, &["fetch", "-q", "--prune", "origin"]);
        let branch = super::status(root).unwrap().branch;
        assert_eq!(branch.upstream.as_deref(), Some("origin/feature"));
        assert!(branch.gone && !branch.published);
    }

    #[test]
    fn a_branch_started_from_another_gets_its_own_upstream() {
        let dir = repository();
        let root = dir.path();
        let remote = tempfile::tempdir().unwrap();
        sh(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        commit_file(root, "a.txt", "first");
        sh(
            root,
            &["remote", "add", "upstream", remote.path().to_str().unwrap()],
        );
        sh(root, &["push", "-q", "-u", "upstream", "main"]);
        sh(
            root,
            &["switch", "-q", "-c", "topic", "--track", "upstream/main"],
        );
        commit_file(root, "b.txt", "topic work");
        assert!(!super::status(root).unwrap().branch.published);
        push(root, "topic").unwrap();
        assert_eq!(
            sh(remote.path(), &["rev-parse", "main"]),
            sh(root, &["rev-parse", "HEAD~1"]),
            "main is untouched"
        );
        assert_eq!(
            super::status(root).unwrap().branch.upstream.as_deref(),
            Some("upstream/topic")
        );
        assert_eq!(
            default_branch(root),
            None,
            "the remote's HEAD was never read"
        );
        sh(root, &["remote", "set-head", "upstream", "main"]);
        assert_eq!(default_branch(root).as_deref(), Some("main"));

        sh(root, &["switch", "-q", "-c", "loose"]);
        sh(root, &["remote", "add", "fork", "/nowhere"]);
        assert_eq!(
            push(root, "loose"),
            Err(PushError::UnclearRemote(vec![
                "fork".into(),
                "upstream".into()
            ]))
        );
        sh(root, &["config", "remote.pushDefault", "upstream"]);
        push(root, "loose").unwrap();
        assert_eq!(
            sh(remote.path(), &["rev-parse", "loose"]),
            sh(root, &["rev-parse", "HEAD"])
        );
    }

    /// A repository with `main` committed and pushed to a bare `origin`, which is
    /// returned.
    fn with_origin(root: &Path) -> tempfile::TempDir {
        let remote = tempfile::tempdir().unwrap();
        sh(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        commit_file(root, "a.txt", "first");
        sh(
            root,
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        sh(root, &["push", "-q", "-u", "origin", "main"]);
        remote
    }

    #[test]
    fn push_names_its_branch_so_push_settings_and_odd_names_never_reach_other_branches() {
        let dir = repository();
        let root = dir.path();
        let remote = with_origin(root);
        let main = sh(remote.path(), &["rev-parse", "main"]);
        sh(root, &["switch", "-q", "-c", "feature"]);
        commit_file(root, "b.txt", "feature work");
        push(root, "feature").unwrap();
        sh(
            root,
            &[
                "config",
                "remote.origin.push",
                "+refs/heads/feature:refs/heads/main",
            ],
        );
        sh(root, &["config", "push.default", "matching"]);
        commit_file(root, "c.txt", "more feature work");
        push(root, "feature").unwrap();
        assert_eq!(
            sh(remote.path(), &["rev-parse", "main"]),
            main,
            "a forcing push refspec in the config never moves main"
        );
        assert_eq!(
            sh(remote.path(), &["rev-parse", "feature"]),
            sh(root, &["rev-parse", "HEAD"])
        );
        sh(root, &["config", "--unset", "remote.origin.push"]);

        let feature = sh(remote.path(), &["rev-parse", "feature"]);
        sh(root, &["switch", "-q", "-c", "+feature", "main"]);
        commit_file(root, "d.txt", "plus work");
        push(root, "+feature").unwrap();
        assert_eq!(
            sh(remote.path(), &["rev-parse", "feature"]),
            feature,
            "a branch named +feature is not a forced push of feature"
        );
        assert_eq!(
            sh(remote.path(), &["rev-parse", "refs/heads/+feature"]),
            sh(root, &["rev-parse", "HEAD"])
        );

        sh(root, &["switch", "-q", "main"]);
        commit_file(root, "e.txt", "on main meanwhile");
        let before = sh(remote.path(), &["rev-parse", "main"]);
        sh(root, &["switch", "-q", "feature"]);
        commit_file(root, "f.txt", "feature again");
        sh(root, &["switch", "-q", "main"]);
        push(root, "feature").unwrap();
        assert_eq!(
            sh(remote.path(), &["rev-parse", "main"]),
            before,
            "the branch asked for is pushed, not whatever HEAD became"
        );
        assert_eq!(
            sh(remote.path(), &["rev-parse", "feature"]),
            sh(root, &["rev-parse", "feature"])
        );
    }

    #[test]
    fn a_push_remote_gets_the_branch_and_the_upstream_stays() {
        let dir = repository();
        let root = dir.path();
        let _origin = with_origin(root);
        let fork = tempfile::tempdir().unwrap();
        sh(fork.path(), &["init", "-q", "--bare", "-b", "main"]);
        sh(
            root,
            &["remote", "add", "fork", fork.path().to_str().unwrap()],
        );
        sh(root, &["switch", "-q", "-c", "feature"]);
        push(root, "feature").unwrap();
        sh(root, &["config", "remote.pushDefault", "fork"]);
        let status = super::status(root).unwrap();
        assert_eq!(
            (status.branch.ahead, status.branch.published),
            (0, false),
            "up to date with origin, but not yet on the push remote"
        );
        push(root, "feature").unwrap();
        assert_eq!(
            sh(fork.path(), &["rev-parse", "feature"]),
            sh(root, &["rev-parse", "HEAD"]),
            "up to date with origin, the push still reaches the push remote"
        );
        assert_eq!(
            super::status(root).unwrap().branch.upstream.as_deref(),
            Some("origin/feature"),
            "a triangular setup keeps its upstream"
        );

        sh(
            root,
            &["switch", "-q", "-c", "topic", "--track", "origin/main"],
        );
        commit_file(root, "t.txt", "topic work");
        assert!(!super::status(root).unwrap().branch.published);
        push(root, "topic").unwrap();
        let after = super::status(root).unwrap().branch;
        assert_eq!(after.upstream.as_deref(), Some("origin/main"));
        assert_eq!(
            (after.published, after.unpushed, after.ahead),
            (true, 0, 1),
            "nothing is left to push to the fork, though it is ahead of origin/main"
        );
        assert_eq!(
            sh(fork.path(), &["rev-parse", "topic"]),
            sh(root, &["rev-parse", "HEAD"])
        );
        commit_file(root, "u.txt", "more topic work");
        assert_eq!(super::status(root).unwrap().branch.unpushed, 1);

        sh(root, &["config", "--unset-all", "remote.fork.fetch"]);
        sh(
            root,
            &[
                "config",
                "remote.fork.fetch",
                "+refs/heads/*:refs/remotes/mine/*",
            ],
        );
        sh(root, &["fetch", "-q", "fork"]);
        assert_eq!(
            super::status(root).unwrap().branch.unpushed,
            1,
            "a fetch refspec with its own folder is followed"
        );
        sh(root, &["config", "--unset-all", "remote.fork.fetch"]);
        sh(
            root,
            &[
                "config",
                "remote.fork.fetch",
                "+refs/heads/main:refs/remotes/fork/main",
            ],
        );
        let unfetched = super::status(root).unwrap().branch;
        assert_eq!(
            (unfetched.published, unfetched.unpushed),
            (true, 0),
            "a push remote that never fetches the branch can't be checked, so gh decides"
        );
    }

    #[test]
    fn a_branch_named_like_a_tag_is_still_the_current_branch() {
        let dir = repository();
        let root = dir.path();
        let _origin = with_origin(root);
        sh(root, &["switch", "-q", "-c", "release"]);
        sh(root, &["tag", "release"]);
        sh(root, &["remote", "set-head", "origin", "main"]);
        assert_eq!(current_branch(root).unwrap().as_deref(), Some("release"));
        sh(root, &["switch", "-q", "--detach"]);
        assert_eq!(current_branch(root).unwrap(), None);
    }

    #[test]
    fn an_upstream_whose_name_only_ends_like_the_branch_is_not_its_own() {
        let dir = repository();
        let root = dir.path();
        let _origin = with_origin(root);
        sh(root, &["push", "-q", "origin", "main:team/leaf"]);
        sh(root, &["fetch", "-q", "origin"]);
        sh(
            root,
            &["switch", "-q", "-c", "leaf", "--track", "origin/team/leaf"],
        );
        let branch = super::status(root).unwrap().branch;
        assert_eq!(branch.upstream.as_deref(), Some("origin/team/leaf"));
        assert!(!branch.published);
    }

    #[test]
    fn a_repository_that_hides_untracked_files_keeps_them_hidden() {
        let dir = repository();
        let root = dir.path();
        sh(root, &["config", "status.showUntrackedFiles", "No"]);
        fs::write(root.join("loose.txt"), "x").unwrap();
        let status = status(root).unwrap();
        assert!(status.entries.is_empty() && status.untracked_hidden);
    }

    #[test]
    fn a_command_past_its_limit_is_stopped_with_what_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut command = program(Path::new("sh"), dir.path());
        command.args([
            "-c",
            &format!("sleep 30 & echo $! > '{}'; wait", pid_file.display()),
        ]);
        let start = Instant::now();
        let result = run_until(
            command,
            None,
            Output::Separate,
            Until::Deadline(Duration::from_millis(200)),
        );
        assert!(
            matches!(&result, Err(Error::Spawn(message)) if message.contains("no answer")),
            "{result:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_gone(&pid_file);
    }

    /// Waits for the process whose pid `pid_file` holds to be gone.
    fn assert_gone(pid_file: &Path) {
        let child: libc::pid_t = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = Instant::now() + Duration::from_secs(5);
        // SAFETY: signal 0 only asks whether the process exists.
        while unsafe { libc::kill(child, 0) } == 0 {
            assert!(
                Instant::now() < gone,
                "the command's own child was left running"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_cancelled_command_is_stopped_with_what_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut command = program(Path::new("sh"), dir.path());
        command.args([
            "-c",
            &format!("sleep 30 & echo $! > '{}'; wait", pid_file.display()),
        ]);
        let cancel = Cancel::default();
        let firing = cancel.clone();
        let fire = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            firing.cancel();
        });
        let start = Instant::now();
        let result = run_until(command, None, Output::Separate, Until::Cancelled(&cancel));
        fire.join().unwrap();
        assert_eq!(result, Err(Error::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_gone(&pid_file);
    }

    #[test]
    fn a_command_cancelled_before_it_starts_never_runs() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let mut command = program(Path::new("sh"), dir.path());
        command.args(["-c", &format!("touch '{}'", marker.display())]);
        let cancel = Cancel::default();
        cancel.cancel();
        let result = run_until(command, None, Output::Separate, Until::Cancelled(&cancel));
        assert_eq!(result, Err(Error::Cancelled));
        assert!(!marker.exists());
    }

    #[test]
    fn a_quick_command_under_a_cancel_is_not_slowed_by_polling() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = Cancel::default();
        let timed = |until: Until| {
            let start = Instant::now();
            for _ in 0..20 {
                let mut command = program(Path::new("sleep"), dir.path());
                command.arg("0.01");
                run_until(command, None, Output::Separate, until).unwrap();
            }
            start.elapsed()
        };
        let (cancellable, deadline) = (
            timed(Until::Cancelled(&cancel)),
            timed(Until::Deadline(Duration::from_secs(60))),
        );
        assert!(cancellable < deadline, "{cancellable:?} vs {deadline:?}");
    }
}
