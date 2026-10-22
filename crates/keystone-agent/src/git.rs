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
    /// including any hook's output.
    Failed { command: String, output: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "Couldn't run git: {error}"),
            Self::Failed { command, output } if output.trim().is_empty() => {
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
    /// Commits ahead of and behind the upstream, when there is one.
    pub ahead: u32,
    pub behind: u32,
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

/// The commit a successful `git commit` made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    /// The abbreviated commit ID and the subject, such as `1a2b3c4 Fix the build`.
    pub summary: String,
}

/// `git` in `folder`, in a session of its own with no controlling terminal, so a hook,
/// a signing program or a credential helper that asks on the terminal fails at once
/// instead of waiting for an answer nobody can type.
fn git(folder: &Path) -> Command {
    let mut command = Command::new("git");
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
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    format!("git {}", args.join(" "))
}

fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(OUTPUT_TAIL);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// How a command's output is collected.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Output {
    /// Stdout alone is returned, so warnings on stderr never reach a parser. A
    /// failure reports stderr.
    Separate,
    /// Stdout and stderr in one stream, in the order they were written, as a
    /// terminal shows hook output.
    Merged,
}

/// Runs `command` with `input` on stdin.
fn run(mut command: Command, input: Option<Vec<u8>>, output: Output) -> Result<Vec<u8>> {
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
    let status = child.wait().map_err(spawn)?;
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
    let (branch, entries) = parse_status(&output);
    Ok(Status {
        root,
        branch,
        entries,
        untracked_hidden,
    })
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn path(bytes: &[u8]) -> PathBuf {
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
                    "branch.upstream" => branch.upstream = Some(value.into()),
                    "branch.ab" => {
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
pub fn commit(root: &Path, message: &str) -> Result<Committed> {
    let mut command = git(root);
    command.args(["commit", "--file=-"]);
    run(command, Some(message.as_bytes().to_vec()), Output::Merged)?;
    // The commit is made; a summary that cannot be read must not look like a failure.
    let mut command = git(root);
    command.args(["log", "-1", "--no-show-signature", "--format=%h %s"]);
    let summary = run(command, None, Output::Separate)
        .map(|summary| text(&summary).trim_end().to_owned())
        .unwrap_or_else(|_| "Committed.".into());
    Ok(Committed { summary })
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
                ahead: 2,
                behind: 3,
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
        let committed = commit(&top, "first\n\nbody").unwrap();
        let head = sh(&top, &["rev-parse", "--short", "HEAD"]);
        assert_eq!(committed.summary, format!("{} first", head.trim()));
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
        let committed = commit(root, "background hook").unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert!(
            committed.summary.ends_with(" background hook"),
            "{committed:?}"
        );
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

    #[test]
    fn a_repository_that_hides_untracked_files_keeps_them_hidden() {
        let dir = repository();
        let root = dir.path();
        sh(root, &["config", "status.showUntrackedFiles", "No"]);
        fs::write(root.join("loose.txt"), "x").unwrap();
        let status = status(root).unwrap();
        assert!(status.entries.is_empty() && status.untracked_hidden);
    }
}
