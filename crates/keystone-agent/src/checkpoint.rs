//! Checkpoints of a session's repository. As each turn ends, the working tree, tracked
//! and untracked files without ignored ones, is saved as a commit that only
//! `refs/keystone/checkpoints/<store>/<session>/<turn>` names, and a restore puts the
//! working tree back the way one left it. `<store>` tells apart the Keystone workspaces
//! that use one repository, since session numbers repeat between them. Every tree is
//! built in an index file of its own inside the repository's git folder, so the user's
//! index, HEAD and stash are never touched. Checkpoints older than [`KEEP_DAYS`] go as
//! each new one is saved, in every workspace's store. Every call blocks.

use crate::git::{self, Cancel, Output, Until, git, path, run, run_until};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Checkpoints kept per session. Older ones are pruned as each new one is saved.
pub const KEEP: usize = 50;
/// How many days any checkpoint is kept, before-restore included, in every
/// workspace's store, so the checkpoints of sessions nobody uses any more, and of
/// workspaces that are gone, do not stay forever.
pub const KEEP_DAYS: u64 = 30;
/// Untracked files larger than this stay out of checkpoints, so a build artifact
/// nobody ignored does not copy itself into the repository on every turn. A restore
/// leaves them as they are.
pub const LARGE: u64 = 20 * 1024 * 1024;

const REFS: &str = "refs/keystone/checkpoints";
/// The ref holding the working tree as it was just before the last restore, so what a
/// restore overwrote can still be found with git.
const BEFORE_RESTORE: &str = "before-restore";
/// The line of a checkpoint's message naming the blob that lists what it left out.
const LEFT_OUT: &str = "Left out: ";
/// A submodule's entry in a tree. Its files belong to its own repository.
const GITLINK: &str = "160000";
/// How the files Keystone keeps in the git folder while it works are named.
const SCRATCH_INDEX: &str = "keystone-checkpoint-";
const RESTORE_FOLDER: &str = "keystone-restore-";
/// The file of a restore's folder recording each change made to the working tree.
const JOURNAL: &str = "journal";
/// The file in the git folder a restore or a put back locks while it runs.
const RESTORING: &str = "keystone-restoring";

#[derive(Debug)]
pub enum Error {
    Git(git::Error),
    Io(io::Error),
    /// The checkpoint's ref is gone, pruned or deleted.
    Gone,
    /// A restore was cancelled, or its caller did not keep it. No file changed.
    Cancelled,
    /// `cause` stopped a restore, and some of the files it had replaced could not be
    /// put back. They are in `kept`.
    NotPutBack {
        cause: Box<Error>,
        kept: PathBuf,
    },
    /// Putting back a restore Keystone did not finish left some files where they are.
    /// The ones they replace are in this folder.
    Kept(PathBuf),
    /// Another Keystone is restoring the working tree.
    Busy,
    /// A restore Keystone did not finish is still to be put back.
    Unfinished,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
            Self::Gone => f.write_str("This checkpoint is no longer saved."),
            Self::Cancelled => f.write_str("Restore cancelled."),
            Self::NotPutBack { cause, kept } => write!(f, "{cause} {}", Self::Kept(kept.clone())),
            Self::Busy => f.write_str("Another restore is running here."),
            Self::Unfinished => f.write_str("Put files back first."),
            Self::Kept(kept) => write!(
                f,
                "Some files could not be put back. They are in {}.",
                kept.display()
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<git::Error> for Error {
    fn from(error: git::Error) -> Self {
        match error {
            git::Error::Cancelled => Self::Cancelled,
            error => Self::Git(error),
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// One session's checkpoints: the Keystone workspace and the session in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub store: String,
    pub id: u64,
}

impl Session {
    fn refs(&self) -> String {
        format!("{REFS}/{}/{}", self.store, self.id)
    }

    fn name(&self, turn: impl fmt::Display) -> String {
        format!("{}/{turn}", self.refs())
    }
}

/// A new random name for a Keystone workspace's checkpoints, for [`Session::store`].
pub fn new_store() -> io::Result<String> {
    let mut random = [0u8; 8];
    getrandom::getrandom(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// A repository's saved checkpoints for one session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checkpoints {
    /// The top folder of the repository or worktree they were taken in.
    pub root: PathBuf,
    /// By turn: how many files the turn changed, when the tree it started from was
    /// taken.
    pub turns: BTreeMap<u64, Option<usize>>,
    /// A restore of the session's ended without finishing, with Keystone, so its
    /// files are half in place until [`put_back`].
    pub unfinished: bool,
}

/// What a restore left as it is: files the checkpoint has where a folder, a nested
/// repository or an ignored file now stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restored {
    pub skipped: Vec<PathBuf>,
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end().to_owned()
}

/// The repository or worktree a chain of commands runs in, and when they are given
/// up. Every command of a restore goes through one, so each can be cancelled.
#[derive(Clone, Copy)]
struct Repo<'a> {
    root: &'a Path,
    until: Until<'a>,
}

impl<'a> Repo<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            until: Until::Exit,
        }
    }

    fn git(&self) -> Command {
        git(self.root)
    }

    fn run(&self, command: Command, input: Option<Vec<u8>>) -> git::Result<Vec<u8>> {
        run_until(command, input, Output::Separate, self.until)
    }

    /// Runs a ref update to its end even when cancelled, since a killed `update-ref`
    /// leaves a lock that fails every later update of the ref. It runs no hook (see
    /// [`update_refs`]), so it is quick.
    fn run_whole(&self, command: Command, input: Option<Vec<u8>>) -> git::Result<Vec<u8>> {
        run(command, input, Output::Separate)
    }
}

/// `git update-ref` for Keystone's own refs, without the user's hooks: a
/// reference-transaction hook that hangs would otherwise hold a restore that can't be
/// cancelled there.
fn update_refs(root: &Path) -> Command {
    let mut command = git(root);
    command.args(["-c", "core.hooksPath=/dev/null", "update-ref"]);
    command
}

/// Where `name` in the git folder of the repository or worktree at `root` is.
fn git_path(repo: Repo, name: &str) -> Result<PathBuf> {
    let mut command = repo.git();
    command.args(["rev-parse", "--git-path", name]);
    let output = repo.run(command, None)?;
    Ok(repo
        .root
        .join(path(output.strip_suffix(b"\n").unwrap_or(&output))))
}

fn nul_separated(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
}

fn nul_joined<'a>(paths: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    paths
        .into_iter()
        .flat_map(|path| path.iter().copied().chain([0]))
        .collect()
}

/// A name in the git folder no other chain of commands uses, starting with `prefix`.
static COUNT: AtomicU64 = AtomicU64::new(0);

fn unique(repo: Repo, prefix: &str, suffix: &str) -> Result<PathBuf> {
    let name = format!(
        "{prefix}{}-{}{suffix}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    );
    git_path(repo, &name)
}

/// An index file in the repository's git folder for one chain of commands, removed
/// when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(repo: Repo) -> Result<Self> {
        let file = unique(repo, SCRATCH_INDEX, ".index")?;
        let _ = fs::remove_file(&file);
        Ok(Self(file))
    }

    /// A copy of the index of the repository or worktree at `root`, so `git add` reads
    /// again only the files whose stat changed. Files the user marked assume-unchanged,
    /// or skip-worktree while they are on disk, are read again too. The copy keeps the
    /// index's modified time, so git still reads again the files changed in the same
    /// second the index was written.
    fn copy_of(repo: Repo) -> Result<Self> {
        let scratch = Self::new(repo)?;
        let index = git_path(repo, "index")?;
        match fs::copy(&index, &scratch.0) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(scratch),
            Err(error) => return Err(error.into()),
        }
        fs::File::options()
            .write(true)
            .open(&scratch.0)?
            .set_modified(fs::metadata(&index)?.modified()?)?;
        let mut listed = scratch.git(repo);
        listed.args(["ls-files", "-v", "-z"]);
        let listed = repo.run(listed, None)?;
        let (mut assumed, mut skipped) = (Vec::new(), Vec::new());
        for entry in nul_separated(&listed) {
            let (Some(&tag), Some(file)) = (entry.first(), entry.get(2..)) else {
                continue;
            };
            if tag.is_ascii_lowercase() {
                assumed.push(file);
            }
            if tag.to_ascii_uppercase() == b'S'
                && repo.root.join(path(file)).symlink_metadata().is_ok()
            {
                skipped.push(file);
            }
        }
        for (flag, files) in [
            ("--no-assume-unchanged", assumed),
            ("--no-skip-worktree", skipped),
        ] {
            if files.is_empty() {
                continue;
            }
            let mut clear = scratch.git(repo);
            clear.args(["update-index", flag, "-z", "--stdin"]);
            repo.run(clear, Some(nul_joined(files)))?;
        }
        Ok(scratch)
    }

    fn git(&self, repo: Repo) -> Command {
        let mut command = repo.git();
        command.env("GIT_INDEX_FILE", &self.0);
        command
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = fs::remove_file(lock);
    }
}

/// The top folder of the repository or worktree holding `folder`, or `None` when no
/// repository holds it.
pub fn root(folder: &Path) -> Result<Option<PathBuf>> {
    match git::top_folder(folder) {
        Ok(root) => Ok(Some(root)),
        Err(git::Error::Failed { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// What `git add` takes for a snapshot: everything, except nested repositories and
/// large files among the untracked ones.
fn snapshot_pathspecs(repo: Repo) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut others = repo.git();
    others.args(["ls-files", "--others", "--exclude-standard", "-z"]);
    let others = repo.run(others, None)?;
    let mut specs = b".\0".to_vec();
    let mut left_out = Vec::new();
    for file in nul_separated(&others) {
        let nested = file.ends_with(b"/");
        let large = !nested
            && repo
                .root
                .join(path(file))
                .symlink_metadata()
                .is_ok_and(|meta| meta.is_file() && meta.len() > LARGE);
        if nested || large {
            specs.extend_from_slice(b":(top,exclude,literal)");
            specs.extend_from_slice(file);
            specs.push(0);
            left_out.extend_from_slice(file);
            left_out.push(0);
        }
    }
    Ok((specs, left_out))
}

/// The working tree as a tree, and the untracked paths it left out: nested
/// repositories, ending in `/`, and large files, NUL-separated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub tree: String,
    left_out: Vec<u8>,
}

/// The tree of the working tree at `root`: tracked and untracked files, without the
/// ones .gitignore, info/exclude or the user's excludes file leave out, and without
/// untracked nested repositories and untracked files over [`LARGE`].
pub fn snapshot(root: &Path) -> Result<Snapshot> {
    take(Repo::new(root))
}

fn take(repo: Repo) -> Result<Snapshot> {
    let scratch = Scratch::copy_of(repo)?;
    let (specs, left_out) = snapshot_pathspecs(repo)?;
    let mut add = scratch.git(repo);
    add.args([
        "add",
        "--all",
        "--pathspec-from-file=-",
        "--pathspec-file-nul",
    ]);
    repo.run(add, Some(specs))?;
    let mut write = scratch.git(repo);
    write.arg("write-tree");
    Ok(Snapshot {
        tree: text(&repo.run(write, None)?),
        left_out,
    })
}

/// How many files differ between two trees, a rename counting once.
fn changed(root: &Path, from: &str, to: &str) -> Result<usize> {
    let mut diff = git(root);
    diff.args([
        "diff-tree",
        "-r",
        "-z",
        "--name-only",
        "--find-renames",
        from,
        to,
    ]);
    Ok(nul_separated(&run(diff, None, Output::Separate)?).count())
}

/// Commits `tree` with `message` and points `name` at it. With `create`, an existing
/// ref is left as it is and the save fails.
fn save(repo: Repo, name: &str, tree: &str, message: String, create: bool) -> Result<()> {
    let commit = commit(repo, tree, None, message)?;
    point(repo, name, &commit, create)
}

/// Commits `tree` with `message`, on `parent` when there is one, dated now, which
/// is how [`expire`] tells its age.
fn commit(repo: Repo, tree: &str, parent: Option<&str>, message: String) -> Result<String> {
    let mut commit = repo.git();
    commit.args(["commit-tree", "--no-gpg-sign", tree, "-F", "-"]);
    if let Some(parent) = parent {
        commit.args(["-p", parent]);
    }
    commit
        .env("GIT_AUTHOR_NAME", "Keystone")
        .env("GIT_AUTHOR_EMAIL", "keystone@localhost")
        .env("GIT_COMMITTER_NAME", "Keystone")
        .env("GIT_COMMITTER_EMAIL", "keystone@localhost")
        .env_remove("GIT_AUTHOR_DATE")
        .env_remove("GIT_COMMITTER_DATE");
    Ok(text(&repo.run(commit, Some(message.into_bytes()))?))
}

/// Points `name` at `commit`. With `create`, an existing ref is left as it is and this
/// fails.
fn point(repo: Repo, name: &str, commit: &str, create: bool) -> Result<()> {
    let mut update = update_refs(repo.root);
    update.args([name, commit]);
    if create {
        update.arg("");
    }
    repo.run_whole(update, None)?;
    Ok(())
}

/// The session's checkpoints in the repository at `root`.
pub fn list(root: &Path, session: &Session) -> Result<Checkpoints> {
    let prefix = format!("{}/", session.refs());
    let mut command = git(root);
    command.args([
        "for-each-ref",
        "--format=%(refname)%00%(contents:body)%00",
        &prefix,
    ]);
    let output = run(command, None, Output::Separate)?;
    let mut fields = output.split(|byte| *byte == 0);
    let mut turns = BTreeMap::new();
    while let (Some(name), Some(body)) = (fields.next(), fields.next()) {
        let Some(Ok(turn)) = text(name)
            .trim_start()
            .strip_prefix(&prefix)
            .map(str::parse::<u64>)
        else {
            continue;
        };
        let files = text(body)
            .lines()
            .find_map(|line| line.strip_prefix("Files: ")?.parse().ok());
        turns.insert(turn, files);
    }
    let unfinished = restore_folders(Repo::new(root))?
        .iter()
        .any(|folder| Work::unfinished(folder, false).is_some());
    Ok(Checkpoints {
        root: root.to_owned(),
        turns,
        unfinished,
    })
}

/// Deletes all but the newest `keep` of the session's checkpoints; with 0, every ref
/// of the session.
pub fn prune(root: &Path, session: &Session, keep: usize) -> Result<()> {
    let mut doomed: String = list(root, session)?
        .turns
        .keys()
        .rev()
        .skip(keep)
        .map(|turn| format!("delete {}\n", session.name(turn)))
        .collect();
    if keep == 0 {
        doomed.push_str(&format!("delete {}\n", session.name(BEFORE_RESTORE)));
    }
    let mut update = update_refs(root);
    update.arg("--stdin");
    run(update, Some(doomed.into_bytes()), Output::Separate)?;
    Ok(())
}

/// Deletes every checkpoint ref, of any store and session, before-restore included,
/// saved before `now` less [`KEEP_DAYS`], and what a save or restore that never finished
/// left in the git folder that long ago, except folders still holding a user's files.
/// A ref saved again meanwhile stays. Best effort: the next save tries again.
fn expire(root: &Path, now: SystemTime) {
    let Some(cutoff) = now.checked_sub(Duration::from_secs(KEEP_DAYS * 24 * 60 * 60)) else {
        return;
    };
    let seconds = cutoff
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let mut listed = git(root);
    listed.args([
        "for-each-ref",
        "--format=%(refname) %(objectname) %(committerdate:unix)",
        REFS,
    ]);
    if let Ok(listed) = run(listed, None, Output::Separate) {
        let doomed: String = text(&listed)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split(' ');
                let (name, oid) = (fields.next()?, fields.next()?);
                let saved: u64 = fields.next()?.parse().ok()?;
                (saved < seconds).then(|| format!("delete {name} {oid}\n"))
            })
            .collect();
        let mut update = update_refs(root);
        update.arg("--stdin");
        if !doomed.is_empty()
            && run(update, Some(doomed.clone().into_bytes()), Output::Separate).is_err()
        {
            // One ref saved again meanwhile fails the whole batch, so the rest go one by
            // one.
            for line in doomed.lines() {
                let mut update = update_refs(root);
                update.arg("--stdin");
                let _ = run(
                    update,
                    Some(
                        format!(
                            "{line}
"
                        )
                        .into_bytes(),
                    ),
                    Output::Separate,
                );
            }
        }
    }
    let Ok(folder) = git_path(Repo::new(root), "index") else {
        return;
    };
    let Some(Ok(entries)) = folder.parent().map(fs::read_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours = name.starts_with(SCRATCH_INDEX)
            || (name.starts_with(RESTORE_FOLDER) && !holds_files(&entry.path().join("aside")));
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified < cutoff);
        if ours && old {
            let _ = match entry.file_type() {
                Ok(kind) if kind.is_dir() => fs::remove_dir_all(entry.path()),
                _ => fs::remove_file(entry.path()),
            };
        }
    }
}

/// Whether `folder` or one inside it holds a file: a restore that could not put
/// back, or never finished, leaves the user's originals there, and those stay.
fn holds_files(folder: &Path) -> bool {
    fs::read_dir(folder).is_ok_and(|entries| {
        entries.flatten().any(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => holds_files(&entry.path()),
            _ => true,
        })
    })
}

/// Saves `tree`, the working tree as `turn` left it, as the session's checkpoint for that
/// turn, with the count of files changed since `start` when the turn's start was
/// taken, then keeps only the newest `keep` and expires checkpoints older than
/// [`KEEP_DAYS`]. An existing checkpoint of that turn is left as it is and the save fails.
pub fn record(
    root: &Path,
    session: &Session,
    turn: u64,
    snapshot: &Snapshot,
    start: Option<&str>,
    keep: usize,
) -> Result<Checkpoints> {
    let repo = Repo::new(root);
    let tree = &snapshot.tree;
    let files = start.map(|start| changed(root, start, tree)).transpose()?;
    let mut message = format!("Keystone checkpoint: session {}, turn {turn}\n", session.id);
    if let Some(files) = files {
        message.push_str(&format!("\nFiles: {files}\n"));
    }
    let mut parent = None;
    if !snapshot.left_out.is_empty() {
        let mut blob = git(root);
        blob.args(["hash-object", "-w", "--stdin"]);
        let blob = text(&run(
            blob,
            Some(snapshot.left_out.clone()),
            Output::Separate,
        )?);
        let mut list = git(root);
        list.args(["mktree"]);
        let list = text(&run(
            list,
            Some(format!("100644 blob {blob}\tleft-out\n").into_bytes()),
            Output::Separate,
        )?);
        parent = Some(commit(repo, &list, None, LEFT_OUT.to_owned())?);
        message.push_str(&format!("{LEFT_OUT}{blob}\n"));
    }
    let checkpoint = commit(repo, tree, parent.as_deref(), message)?;
    point(repo, &session.name(turn), &checkpoint, true)?;
    prune(root, session, keep)?;
    expire(root, SystemTime::now());
    list(root, session)
}

/// Puts the working tree at `root` back the way the session's checkpoint for `turn`
/// saved it, all or nothing: files changed since are written back, and files created
/// since are deleted, with folders they leave empty. The working tree as it was just
/// before is saved first as `<session>/before-restore`.
///
/// Ignored files are never read or written, by the rules now or by the checkpoint's
/// own .gitignore files. Nested repositories are left alone, and so is a file of the
/// checkpoint where a folder, a nested repository, or a file the snapshot left out now
/// stands; those are returned.
///
/// Every file is converted by git into a folder in the git folder before any file of
/// the working tree changes, so a filter that hangs or fails changes nothing there.
/// The files are then moved into place, each one they replace moved aside, and
/// `keep` is asked whether to keep them. When it says no, `cancel` fired, or anything
/// failed, every file is moved back and the working tree is as it was.
pub fn restore(
    root: &Path,
    session: &Session,
    turn: u64,
    cancel: &Cancel,
    keep: impl FnOnce() -> bool,
) -> Result<Restored> {
    let repo = Repo {
        root,
        until: Until::Cancelled(cancel),
    };
    let mut verify = repo.git();
    verify.args([
        "rev-parse",
        "--quiet",
        "--verify",
        &format!("{}^{{tree}}", session.name(turn)),
    ]);
    let target = match repo.run(verify, None) {
        Ok(target) => text(&target),
        Err(git::Error::Failed { .. }) => return Err(Error::Gone),
        Err(error) => return Err(error.into()),
    };
    let _held = hold(repo)?;
    if restore_folders(repo)?
        .iter()
        .any(|folder| Work::unfinished(folder, true).is_some())
    {
        return Err(Error::Unfinished);
    }
    let left_out = left_out(repo, &session.name(turn))?;
    let now = take(repo)?.tree;
    save(
        repo,
        &session.name(BEFORE_RESTORE),
        &now,
        format!("Keystone: before restoring turn {turn}\n"),
        false,
    )?;
    let mut diff = repo.git();
    diff.args(["diff-tree", "-r", "-z", "--no-renames", &now, &target]);
    let output = repo.run(diff, None)?;
    // Each change is `:<mode> <mode> <oid> <oid> <status>` and its path.
    let mut deletes = Vec::new();
    let mut writes = Vec::new();
    let mut skipped = Vec::new();
    // The mode and blob the tree read now has at each path it holds.
    let mut was = BTreeMap::new();
    let mut fields = output.split(|byte| *byte == 0);
    while let (Some(meta), Some(file)) = (fields.next(), fields.next()) {
        let meta = text(meta);
        let meta: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old, new, blob, _, status] = meta[..] else {
            continue;
        };
        if status != "A" {
            was.insert(file, (old.to_owned(), blob.to_owned()));
        }
        if old == GITLINK || new == GITLINK {
            // A submodule's own commit is its repository's; only a file the checkpoint
            // has in its place is worth saying.
            if new != GITLINK && status != "D" {
                skipped.push(path(file));
            }
        } else if status == "D" {
            deletes.push(file);
        } else {
            writes.push((status == "A", file));
        }
    }
    let target_index = Scratch::new(repo)?;
    let mut read = target_index.git(repo);
    read.args(["read-tree", &target]);
    repo.run(read, None)?;
    let rules = rule_files(repo, &target_index)?;
    let sparse = sparse(repo)?;
    let work = Work::new(repo)?;
    // Only what can be written now goes through git's filters before any file changes:
    // not a file kept off disk by a sparse checkout, nor one where something stands in
    // its way. A path the deletes free, or one that only looks taken, as a case-only
    // rename does on a case-insensitive file system, is converted as it is placed.
    let staged: BTreeSet<&[u8]> = writes
        .iter()
        .filter(|&&(added, file)| {
            let full = root.join(path(file));
            let here = full.symlink_metadata();
            !(sparse.contains(file) && here.is_err())
                && !(added && here.is_ok())
                && !not_ours(root, &full)
        })
        .map(|(_, file)| *file)
        .chain(rules.iter().map(Vec::as_slice))
        .collect();
    let stage = work.stage.clone();
    let convert = |paths: Vec<&[u8]>| -> Result<()> {
        let mut checkout = target_index.git(repo);
        let mut prefix = std::ffi::OsString::from("--prefix=");
        prefix.push(&stage);
        prefix.push("/");
        checkout
            .args(["checkout-index", "--force", "-z"])
            .arg(prefix)
            .arg("--stdin");
        repo.run(checkout, Some(nul_joined(paths)))?;
        Ok(())
    };
    convert(staged.into_iter().collect())?;
    // What the checkpoint left out was there, so it is not deleted as if made later.
    let (kept, candidates): (Vec<&[u8]>, Vec<&[u8]>) = deletes.into_iter().partition(|file| {
        nul_separated(&left_out)
            .any(|out| *file == out || (out.ends_with(b"/") && file.starts_with(out)))
    });
    skipped.extend(kept.into_iter().map(path));
    // The checkpoint's ignore rules decide what else may be deleted.
    let ignored = ignored_by_rules(repo, &work.stage, &candidates)?;
    let deletes: Vec<&[u8]> = candidates
        .into_iter()
        .filter(|file| is_rule_file(file) || !ignored.contains(*file))
        .collect();
    let replaced = writes
        .iter()
        .filter(|(added, _)| !added)
        .map(|(_, file)| *file);
    let unchanged = unchanged_since_read(repo, deletes.iter().copied().chain(replaced), &was)?;
    let mut ignorecase = repo.git();
    ignorecase.args(["config", "--type=bool", "core.ignorecase"]);
    let case_insensitive = match repo.run(ignorecase, None) {
        Ok(value) => text(&value) == "true",
        Err(git::Error::Failed { code: Some(1), .. }) => false,
        Err(error) => return Err(error.into()),
    };
    let mut journal = Journal {
        root,
        unchanged,
        case_insensitive,
        listings: HashMap::new(),
        work,
        steps: Vec::new(),
    };
    let moved = journal.apply(&deletes, &writes, &sparse, &mut skipped, &convert);
    let outcome = moved.and_then(|()| match !cancel.is_cancelled() && keep() {
        true => Ok(()),
        false => Err(Error::Cancelled),
    });
    match outcome {
        Ok(()) => {
            // Said before the folder goes, so a crash in between offers no put back.
            let _ = journal.work.write(&[KEPT.to_vec()]);
            Ok(Restored { skipped })
        }
        Err(cause) => match journal.undo() {
            Ok(()) => Err(cause),
            Err(kept) => Err(Error::NotPutBack {
                cause: Box::new(cause),
                kept,
            }),
        },
    }
}

/// Whether one of the folders above `full`, below `root`, is a nested repository.
fn in_nested_repository(root: &Path, full: &Path) -> bool {
    full.ancestors()
        .skip(1)
        .take_while(|folder| *folder != root)
        .any(|folder| folder.join(".git").symlink_metadata().is_ok())
}

/// Whether `full` is not a file of this working tree: a folder stands there, or one of
/// the folders above it is a file, a link or a nested repository.
fn not_ours(root: &Path, full: &Path) -> bool {
    full.symlink_metadata().is_ok_and(|meta| meta.is_dir())
        || full
            .ancestors()
            .skip(1)
            .take_while(|folder| *folder != root)
            .any(|folder| folder.symlink_metadata().is_ok_and(|meta| !meta.is_dir()))
        || in_nested_repository(root, full)
}

fn is_rule_file(file: &[u8]) -> bool {
    path(file).file_name() == Some(".gitignore".as_ref())
}

/// The .gitignore files of the tree in `index`.
fn rule_files(repo: Repo, index: &Scratch) -> Result<Vec<Vec<u8>>> {
    let mut listed = index.git(repo);
    listed.args(["ls-files", "-z"]);
    let listed = repo.run(listed, None)?;
    Ok(nul_separated(&listed)
        .filter(|file| is_rule_file(file))
        .map(<[u8]>::to_vec)
        .collect())
}

/// The untracked paths the checkpoint `name` left out, NUL-separated.
fn left_out(repo: Repo, name: &str) -> Result<Vec<u8>> {
    let mut message = repo.git();
    message.args(["cat-file", "commit", name]);
    let message = repo.run(message, None)?;
    let Some(blob) = String::from_utf8_lossy(&message)
        .lines()
        .find_map(|line| line.strip_prefix(LEFT_OUT).map(str::to_owned))
    else {
        return Ok(Vec::new());
    };
    let mut read = repo.git();
    read.args(["cat-file", "blob", &blob]);
    Ok(repo.run(read, None)?)
}

/// Which of `files` the ignore rules in `rules`, a folder holding a tree's .gitignore
/// files, leave out, with the repository's own and the user's excludes, tracked or not.
fn ignored_by_rules(repo: Repo, rules: &Path, files: &[&[u8]]) -> Result<BTreeSet<Vec<u8>>> {
    if files.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut check = repo.git();
    check
        .arg("--work-tree")
        .arg(rules)
        .args(["check-ignore", "--no-index", "-z", "--stdin"]);
    match repo.run(check, Some(nul_joined(files.iter().copied()))) {
        Ok(output) => Ok(nul_separated(&output).map(<[u8]>::to_vec).collect()),
        // None of them is ignored.
        Err(git::Error::Failed { code: Some(1), .. }) => Ok(BTreeSet::new()),
        Err(error) => Err(error.into()),
    }
}

/// The files of the user's index outside a sparse checkout's cone: the user keeps these
/// off disk.
fn sparse(repo: Repo) -> Result<BTreeSet<Vec<u8>>> {
    let mut listed = repo.git();
    listed.args(["ls-files", "-v", "-z"]);
    let listed = repo.run(listed, None)?;
    Ok(nul_separated(&listed)
        .filter(|entry| {
            entry
                .first()
                .is_some_and(|tag| tag.to_ascii_uppercase() == b'S')
        })
        .filter_map(|entry| entry.get(2..))
        .map(<[u8]>::to_vec)
        .collect())
}

/// A restore's folder in the git folder: `stage` holds the checkpoint's files as git
/// converted them, `aside` the files of the working tree they replace, and the journal
/// each folder made or removed and each file placed in the working tree, so
/// [`put_back`] can undo a restore Keystone did not finish. The journal is locked
/// while its restore runs. Removed when dropped, the journal first, unless files could
/// not be put back from it.
struct Work {
    folder: PathBuf,
    stage: PathBuf,
    aside: PathBuf,
    journal: fs::File,
    keep: bool,
}

impl Work {
    /// A new folder, never one an earlier Keystone left, which may hold the only copy
    /// of a user's files.
    fn new(repo: Repo) -> Result<Self> {
        let folder = loop {
            let folder = unique(repo, RESTORE_FOLDER, "")?;
            match fs::create_dir(&folder) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                made => made?,
            }
            break folder;
        };
        let (stage, aside) = (folder.join("stage"), folder.join("aside"));
        fs::create_dir(&stage)?;
        fs::create_dir(&aside)?;
        let journal = fs::File::options()
            .append(true)
            .create_new(true)
            .open(folder.join(JOURNAL))?;
        // Waits out a reader that holds it a moment; the working tree's lock [`hold`]
        // keeps restores apart.
        let _ = journal.lock();
        Ok(Self {
            folder,
            stage,
            aside,
            journal,
            keep: false,
        })
    }

    /// The folder of a restore that ended without finishing, with the steps its journal
    /// records: `None` while the restore still runs, once it was kept, or when it
    /// changed nothing. `held` when the caller holds the working tree's lock [`hold`]:
    /// then no restore runs, a journal's lock is only another reader's for a moment,
    /// and a kept restore's folder goes.
    fn unfinished(folder: &Path, held: bool) -> Option<(Self, Vec<Step>)> {
        use std::io::Read;
        let mut journal = fs::File::options()
            .read(true)
            .append(true)
            .open(folder.join(JOURNAL))
            .ok()?;
        match held {
            true => {
                let _ = journal.lock();
            }
            false => {
                if let Err(fs::TryLockError::WouldBlock) = journal.try_lock() {
                    return None;
                }
            }
        }
        let mut records = Vec::new();
        journal.read_to_end(&mut records).ok()?;
        let Some(steps) = parse(&records) else {
            if held {
                let _ = fs::remove_dir_all(folder);
            }
            return None;
        };
        let aside = folder.join("aside");
        let changed = !steps.is_empty() || holds_files(&aside);
        changed.then(|| {
            let work = Self {
                folder: folder.to_owned(),
                stage: folder.join("stage"),
                aside,
                journal,
                // Only an undo removes it.
                keep: true,
            };
            (work, steps)
        })
    }

    /// Adds a record to the journal: its fields, each ended by a NUL.
    fn write(&mut self, fields: &[Vec<u8>]) -> io::Result<()> {
        use std::io::Write;
        let record: Vec<u8> = fields
            .iter()
            .flat_map(|field| field.iter().copied().chain([0]))
            .collect();
        self.journal.write_all(&record)
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        // A panic may leave originals aside; they stay, and so does the journal.
        if !self.keep && !std::thread::panicking() {
            let _ = fs::remove_file(self.folder.join(JOURNAL));
            let _ = fs::remove_dir_all(&self.folder);
        }
    }
}

/// The lock a restore and a put back of the working tree at `repo` hold while they
/// run, so two Keystones never move its files at once. Neither runs where the file
/// system cannot lock.
fn hold(repo: Repo) -> Result<fs::File> {
    let lock = fs::File::options()
        .create(true)
        .append(true)
        .open(git_path(repo, RESTORING)?)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(fs::TryLockError::WouldBlock) => Err(Error::Busy),
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// The folders restores left in the git folder of the repository or worktree.
fn restore_folders(repo: Repo) -> Result<Vec<PathBuf>> {
    let index = git_path(repo, "index")?;
    let Some(folder) = index.parent() else {
        return Ok(Vec::new());
    };
    Ok(fs::read_dir(folder)?
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(RESTORE_FOLDER)
        })
        .map(|entry| entry.path())
        .collect())
}

/// Puts back what each restore in the working tree at `root` changed and did not
/// finish, as a cancelled restore is put back. A file changed since stays, and the
/// one it replaced stays aside: [`Error::Kept`] says where.
pub fn put_back(root: &Path) -> Result<()> {
    let repo = Repo::new(root);
    let _held = hold(repo)?;
    let mut kept = None;
    for folder in restore_folders(repo)? {
        let Some((mut work, steps)) = Work::unfinished(&folder, true) else {
            continue;
        };
        work.keep = false;
        let journal = Journal {
            root,
            unchanged: BTreeMap::new(),
            case_insensitive: false,
            listings: HashMap::new(),
            work,
            steps,
        };
        if let Err(aside) = journal.undo() {
            kept = Some(aside);
        }
    }
    kept.map_or(Ok(()), |kept| Err(Error::Kept(kept)))
}

/// One change a restore made to the working tree, in the order made. Paths are
/// relative to the working tree's top folder. Files it moved aside are not recorded:
/// an undo puts back whatever `aside` holds.
#[derive(Debug, PartialEq)]
enum Step {
    /// A file about to move from `stage` into the working tree, how it was there (a
    /// rename keeps its inode, size and modified time) and its [`digest`].
    Placing(PathBuf, Stamp, String),
    /// A file moved from `stage` into the working tree, and how it was then.
    Placed(PathBuf, Stamp),
    /// A folder made for a file.
    Made(PathBuf),
    /// A folder about to be removed once a file left it empty, with its mode.
    Removing(PathBuf, u32),
    /// A folder removed.
    Removed(PathBuf),
}

impl Step {
    /// The step as a journal records it.
    fn fields(&self) -> Vec<Vec<u8>> {
        use std::os::unix::ffi::OsStrExt;
        let file = |file: &Path| file.as_os_str().as_bytes().to_vec();
        let stamped = |kind: &[u8], placed: &Path, stamp: &Stamp| {
            [kind.to_vec(), file(placed)]
                .into_iter()
                .chain(stamp.fields().map(String::into_bytes))
                .collect()
        };
        match self {
            Self::Placing(placing, stamp, digest) => {
                let mut fields: Vec<Vec<u8>> = stamped(b"placing", placing, stamp);
                fields.push(digest.clone().into_bytes());
                fields
            }
            Self::Placed(placed, stamp) => stamped(b"placed", placed, stamp),
            Self::Made(folder) => vec![b"made".to_vec(), file(folder)],
            Self::Removing(folder, mode) => vec![
                b"removing".to_vec(),
                file(folder),
                mode.to_string().into_bytes(),
            ],
            Self::Removed(folder) => vec![b"removed".to_vec(), file(folder)],
        }
    }
}

/// The record that a restore's files were kept.
const KEPT: &[u8] = b"kept";

fn number<T: std::str::FromStr>(field: Option<&[u8]>) -> Option<T> {
    std::str::from_utf8(field?).ok()?.parse().ok()
}

/// The steps a journal records, or `None` once its restore was kept. A record the end
/// of the journal cuts short, as a crash while writing it leaves, is not one.
fn parse(journal: &[u8]) -> Option<Vec<Step>> {
    let mut fields: Vec<&[u8]> = journal.split(|byte| *byte == 0).collect();
    // What follows the last NUL: nothing, or a field cut short.
    fields.pop();
    let mut fields = fields.into_iter();
    let mut steps = Vec::new();
    while let Some(kind) = fields.next() {
        if kind == KEPT {
            return None;
        }
        let file = fields.next().map(path);
        let step = match kind {
            b"made" => file.map(Step::Made),
            b"removing" => file
                .zip(number(fields.next()))
                .map(|(folder, mode)| Step::Removing(folder, mode)),
            b"removed" => file.map(Step::Removed),
            b"placing" => file
                .zip(Stamp::parse(&mut fields))
                .zip(
                    fields
                        .next()
                        .and_then(|digest| std::str::from_utf8(digest).ok()),
                )
                .map(|((placing, stamp), digest)| Step::Placing(placing, stamp, digest.into())),
            b"placed" => file
                .zip(Stamp::parse(&mut fields))
                .map(|(placed, stamp)| Step::Placed(placed, stamp)),
            _ => None,
        };
        let Some(step) = step else {
            break;
        };
        steps.push(step);
    }
    Some(steps)
}

/// The changes a restore made to the working tree, so each can be undone. Only files
/// are moved, by rename, so undoing runs no git; across file systems, as for a linked
/// worktree on another volume, a move is a copy, and then undoing needs space.
struct Journal<'a> {
    root: &'a Path,
    /// The stamp of each file the restore may replace that was still as the restore
    /// read it once its files were converted. Anything else there was written since.
    unchanged: BTreeMap<Vec<u8>, Stamp>,
    /// Whether git takes the file system to ignore case, as macOS's does by default.
    /// Then a file or folder moved aside is kept by the name its folder stores, so an
    /// undo gives it back that name and not the case the tree has.
    case_insensitive: bool,
    /// Each folder's names and inodes, listed once, for [`Journal::stored`].
    listings: HashMap<PathBuf, Vec<(std::ffi::OsString, u64)>>,
    work: Work,
    steps: Vec<Step>,
}

impl Journal<'_> {
    /// Deletes `deletes` with the folders they leave empty, then places `writes`, each an
    /// added flag and a path. A path where a folder stands, or one of whose folders is a
    /// file, a link or a nested repository, is left and goes to `skipped`, and so is a
    /// write where a file the snapshot left out (ignored or too large) stands, and a file
    /// written since the working tree was read; a file outside a sparse checkout's cone
    /// stays off disk.
    fn apply(
        &mut self,
        deletes: &[&[u8]],
        writes: &[(bool, &[u8])],
        sparse: &BTreeSet<Vec<u8>>,
        skipped: &mut Vec<PathBuf>,
        convert: &dyn Fn(Vec<&[u8]>) -> Result<()>,
    ) -> Result<()> {
        for &file in deletes {
            let full = self.root.join(path(file));
            if not_ours(self.root, &full) || self.changed_since_read(file, &full) {
                skipped.push(path(file));
            } else {
                self.delete(&path(file))?;
            }
        }
        for &(added, file) in writes {
            let full = self.root.join(path(file));
            let here = full.symlink_metadata();
            if sparse.contains(file) && here.is_err() {
                continue;
            }
            let taken = |journal: &Self| {
                (added && full.symlink_metadata().is_ok())
                    || not_ours(journal.root, &full)
                    || journal.changed_since_read(file, &full)
            };
            if taken(self) {
                skipped.push(path(file));
                continue;
            }
            if self.work.stage.join(path(file)).symlink_metadata().is_err() {
                convert(vec![file])?;
                // A filter may take its time: what stands there is looked at again.
                if taken(self) || full.symlink_metadata().is_ok() != here.is_ok() {
                    skipped.push(path(file));
                    continue;
                }
            }
            self.place(&path(file), here.is_ok())?;
        }
        Ok(())
    }

    /// Whether something stands at `file` other than what the restore read there.
    fn changed_since_read(&self, file: &[u8], full: &Path) -> bool {
        Stamp::of(full).is_some_and(|now| self.unchanged.get(file) != Some(&now))
    }

    /// Keeps `step`, made, for an undo now and in the journal for one after a crash.
    fn record(&mut self, step: Step) -> io::Result<()> {
        let fields = step.fields();
        self.steps.push(step);
        self.work.write(&fields)
    }

    /// `file` with its last name as its folder stores it, when the file system ignores
    /// case: the one name there for the same file that differs only in case. A hard
    /// link by another name, or two such names, leave `file` as it is.
    fn stored(&mut self, file: &Path) -> PathBuf {
        use std::os::unix::fs::{DirEntryExt, MetadataExt};
        let full = self.root.join(file);
        let (true, Some(folder), Some(name), Ok(meta)) = (
            self.case_insensitive,
            full.parent(),
            file.file_name(),
            full.symlink_metadata(),
        ) else {
            return file.to_owned();
        };
        let listing = self.listings.entry(folder.to_owned()).or_insert_with(|| {
            fs::read_dir(folder)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|entry| (entry.file_name(), entry.ino()))
                        .collect()
                })
                .unwrap_or_default()
        });
        let same: Vec<&std::ffi::OsString> = listing
            .iter()
            .filter(|(_, inode)| *inode == meta.ino())
            .map(|(entry, _)| entry)
            .collect();
        let folded = name.to_string_lossy().to_lowercase();
        let named: Vec<_> = same
            .iter()
            .copied()
            .filter(|entry| entry.to_string_lossy().to_lowercase() == folded)
            .collect();
        match (&same[..], &named[..]) {
            ([entry], _) | (_, [entry]) => file.with_file_name(entry),
            _ => file.to_owned(),
        }
    }

    fn set_aside(&mut self, file: &Path) -> io::Result<()> {
        let file = self.stored(file);
        let aside = self.work.aside.join(&file);
        if let Some(folder) = aside.parent() {
            fs::create_dir_all(folder)?;
        }
        match move_file(&self.root.join(&file), &aside) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            moved => moved,
        }
    }

    fn delete(&mut self, file: &Path) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        self.set_aside(file)?;
        let root = self.root;
        let mut folder = root.join(file).parent().map(Path::to_owned);
        while let Some(dir) = folder.filter(|dir| dir != root && dir.starts_with(root)) {
            let Ok(meta) = dir.symlink_metadata() else {
                break;
            };
            let relative = self.stored(dir.strip_prefix(root).unwrap_or(&dir));
            // Recorded first, so a crash after the removal still has its mode.
            self.record(Step::Removing(relative.clone(), meta.permissions().mode()))?;
            if fs::remove_dir(&dir).is_err() {
                break;
            }
            self.record(Step::Removed(relative))?;
            folder = dir.parent().map(Path::to_owned);
        }
        Ok(())
    }

    fn place(&mut self, file: &Path, exists: bool) -> io::Result<()> {
        if exists {
            self.set_aside(file)?;
        }
        let full = self.root.join(file);
        let missing: Vec<PathBuf> = full
            .ancestors()
            .skip(1)
            .take_while(|folder| *folder != self.root && folder.symlink_metadata().is_err())
            .map(Path::to_owned)
            .collect();
        for folder in missing.into_iter().rev() {
            fs::create_dir(&folder)?;
            let relative = folder.strip_prefix(self.root).unwrap_or(&folder).to_owned();
            self.record(Step::Made(relative))?;
        }
        let staged = self.work.stage.join(file);
        let placing = Stamp::of(&staged).ok_or(io::ErrorKind::NotFound)?;
        self.record(Step::Placing(file.to_owned(), placing, digest(&staged)?))?;
        move_file(&staged, &full)?;
        let stamp = Stamp::of(&full).ok_or(io::ErrorKind::NotFound)?;
        self.record(Step::Placed(file.to_owned(), stamp))
    }

    /// Undoes every step, the newest first, then moves each file still aside back to
    /// its path, unless something stands there. A placed file someone changed since
    /// stays, and so does its original, aside. When a file stays aside, or a step
    /// failed, the folder holding the originals is kept and returned. Undoing again,
    /// after a crash cut an undo short, undoes only what is left.
    fn undo(mut self) -> std::result::Result<(), PathBuf> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let root = self.root;
        let mut failed = false;
        let (mut placed, mut removed) = (BTreeSet::new(), BTreeSet::new());
        for step in std::mem::take(&mut self.steps).into_iter().rev() {
            let undone = match step {
                Step::Placed(file, stamp) => {
                    let full = root.join(&file);
                    placed.insert(file);
                    match Stamp::of(&full) {
                        Some(now) if now == stamp => fs::remove_file(full),
                        // Gone, changed since, or the original an undo put back.
                        _ => Ok(()),
                    }
                }
                Step::Placing(file, ..) if placed.contains(&file) => Ok(()),
                // A crash came as the file moved: there, it is the staged file, unless
                // its contents changed since.
                Step::Placing(file, stamp, staged) => {
                    let full = root.join(&file);
                    match Stamp::of(&full) {
                        Some(now)
                            if (now.inode, now.len, now.modified)
                                == (stamp.inode, stamp.len, stamp.modified)
                                && digest(&full).is_ok_and(|now| now == staged) =>
                        {
                            fs::remove_file(full)
                        }
                        _ => Ok(()),
                    }
                }
                // A file there is the original an undo put back; files in it are the
                // user's, written since.
                Step::Made(folder) => match fs::remove_dir(root.join(folder)) {
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound
                                | io::ErrorKind::NotADirectory
                                | io::ErrorKind::DirectoryNotEmpty
                        ) =>
                    {
                        Ok(())
                    }
                    removed => removed,
                },
                Step::Removed(folder) => {
                    removed.insert(folder);
                    Ok(())
                }
                // A folder there that was not removed is as it was.
                Step::Removing(folder, mode) => {
                    let full = root.join(&folder);
                    let made = fs::DirBuilder::new().mode(mode).create(&full);
                    match made {
                        Err(error)
                            if error.kind() != io::ErrorKind::AlreadyExists
                                || !full.symlink_metadata().is_ok_and(|meta| meta.is_dir()) =>
                        {
                            Err(error)
                        }
                        Err(_) if !removed.contains(&folder) => Ok(()),
                        _ => fs::set_permissions(&full, fs::Permissions::from_mode(mode)),
                    }
                }
            };
            failed |= undone.is_err();
        }
        failed |= put_aside_back(&self.work.aside, root, Path::new("")).is_err();
        failed |= holds_files(&self.work.aside);
        let _ = fs::remove_file(self.work.folder.join(JOURNAL));
        if failed {
            self.work.keep = true;
            let _ = fs::remove_dir_all(&self.work.stage);
            return Err(self.work.aside.clone());
        }
        Ok(())
    }
}

/// The SHA-256 of a file's contents, or of its target for a link.
fn digest(full: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    let mut hasher = Sha256::new();
    match fs::read_link(full) {
        Ok(target) => hasher.update(target.as_os_str().as_bytes()),
        Err(_) => {
            io::copy(&mut fs::File::open(full)?, &mut hasher)?;
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Moves each file under `aside`, at `relative`, back to the same path under `root`
/// where nothing stands, with the folders it needs.
fn put_aside_back(aside: &Path, root: &Path, relative: &Path) -> io::Result<()> {
    let mut result = Ok(());
    for entry in fs::read_dir(aside.join(relative))? {
        let relative = relative.join(entry?.file_name());
        let from = aside.join(&relative);
        let to = root.join(&relative);
        let moved = if from.symlink_metadata()?.is_dir() {
            put_aside_back(aside, root, &relative)
        } else if to.symlink_metadata().is_ok() {
            Ok(())
        } else {
            to.parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| move_file(&from, &to))
        };
        result = result.and(moved);
    }
    result
}

/// What tells a file apart from one written in its place since. A write changes the
/// file's change time even when it keeps the modified time, as `cp -p` and `rsync -t`
/// do. The device is left out: some file systems number theirs anew at each mount.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Stamp {
    inode: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Stamp {
    fn of(full: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = full.symlink_metadata().ok()?;
        Some(Self {
            inode: meta.ino(),
            len: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }

    fn fields(&self) -> [String; 6] {
        [
            self.inode.to_string(),
            self.len.to_string(),
            self.modified.0.to_string(),
            self.modified.1.to_string(),
            self.changed.0.to_string(),
            self.changed.1.to_string(),
        ]
    }

    fn parse<'a>(fields: &mut impl Iterator<Item = &'a [u8]>) -> Option<Self> {
        Some(Self {
            inode: number(fields.next())?,
            len: number(fields.next())?,
            modified: (number(fields.next())?, number(fields.next())?),
            changed: (number(fields.next())?, number(fields.next())?),
        })
    }
}

/// The stamp of each of `files` still as the tree the restore read holds it, `read`
/// giving its mode and blob there. Every stamp is taken first, then git hashes each
/// file again through the same filters as the snapshot, so a file written since the
/// read is left out even when it kept its modified time, and one written after its
/// stamp was taken no longer matches it.
fn unchanged_since_read<'a>(
    repo: Repo,
    files: impl IntoIterator<Item = &'a [u8]>,
    read: &BTreeMap<&[u8], (String, String)>,
) -> Result<BTreeMap<Vec<u8>, Stamp>> {
    let mut unchanged = BTreeMap::new();
    let mut entries = Vec::new();
    for file in files {
        let (Some((mode, blob)), Some(stamp)) =
            (read.get(file), Stamp::of(&repo.root.join(path(file))))
        else {
            continue;
        };
        unchanged.insert(file.to_vec(), stamp);
        entries.extend(format!("{mode} {blob}\t").bytes());
        entries.extend(file);
        entries.push(0);
    }
    if unchanged.is_empty() {
        return Ok(unchanged);
    }
    let index = Scratch::new(repo)?;
    let mut fill = index.git(repo);
    fill.args(["update-index", "-z", "--index-info"]);
    repo.run(fill, Some(entries))?;
    // Entries with no stat are hashed again; those whose contents match get one.
    let mut refresh = index.git(repo);
    refresh.args(["update-index", "-q", "--refresh"]);
    match repo.run(refresh, None) {
        Ok(_) | Err(git::Error::Failed { code: Some(1), .. }) => {}
        Err(error) => return Err(error.into()),
    }
    let mut differ = index.git(repo);
    differ.args(["diff-files", "-z", "--name-only"]);
    for file in nul_separated(&repo.run(differ, None)?) {
        unchanged.remove(file);
    }
    Ok(unchanged)
}

/// Moves the file or link at `from` to `to`; across file systems, copies it and
/// removes the original.
fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            let copied = if from.symlink_metadata()?.is_symlink() {
                fs::read_link(from).and_then(|target| std::os::unix::fs::symlink(target, to))
            } else {
                fs::copy(from, to).map(|_| ())
            };
            // Either the file is at `to` and gone from `from`, or `to` has nothing.
            match copied.and_then(|()| fs::remove_file(from)) {
                Err(error) => {
                    let _ = fs::remove_file(to);
                    Err(error)
                }
                done => done,
            }
        }
        moved => moved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(folder: &Path, args: &[&str]) -> String {
        let output = git(folder).args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn write(root: &Path, file: &str, contents: &str) {
        let file = root.join(file);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, contents).unwrap();
    }

    fn read(root: &Path, file: &str) -> Option<String> {
        fs::read_to_string(root.join(file)).ok()
    }

    fn session(id: u64) -> Session {
        Session {
            store: "test".into(),
            id,
        }
    }

    fn name(id: u64, turn: u64) -> String {
        session(id).name(turn)
    }

    /// Saves the working tree as the session's checkpoint for `turn`.
    fn save_turn(root: &Path, id: u64, turn: u64, start: Option<&str>, keep: usize) -> Checkpoints {
        let tree = snapshot(root).unwrap();
        record(root, &session(id), turn, &tree, start, keep).unwrap()
    }

    fn init(root: &Path) {
        sh(root, &["init", "-q", "-b", "main"]);
        sh(root, &["config", "user.name", "Test"]);
        sh(root, &["config", "user.email", "test@example.com"]);
        sh(root, &["config", "commit.gpgsign", "false"]);
    }

    /// A repository with a commit, a staged change, a stash and an ignored file, as a
    /// user's might be.
    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init(root);
        write(root, ".gitignore", "*.log\nbuild/\n");
        write(root, "kept.txt", "kept\n");
        write(root, "edited.txt", "first\n");
        write(root, "deleted.txt", "deleted\n");
        sh(root, &["add", "--all"]);
        sh(root, &["commit", "-q", "-m", "start"]);
        write(root, "edited.txt", "stashed\n");
        sh(root, &["stash", "-q"]);
        write(root, "staged.txt", "staged\n");
        sh(root, &["add", "staged.txt"]);
        write(root, "debug.log", "ignored\n");
        write(root, "build/out.bin", "ignored\n");
        dir
    }

    /// Everything a checkpoint must not touch: the index file, HEAD and the stash.
    fn untouched(root: &Path) -> (Vec<u8>, String, String) {
        (
            fs::read(root.join(".git/index")).unwrap(),
            sh(root, &["rev-parse", "HEAD"]),
            sh(root, &["stash", "list", "--format=%H"]),
        )
    }

    fn refs(root: &Path) -> Vec<String> {
        sh(root, &["for-each-ref", "--format=%(refname)", REFS])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn files(root: &Path, tree: &str) -> Vec<String> {
        sh(root, &["ls-tree", "-r", "-z", "--name-only", tree])
            .split('\0')
            .filter(|file| !file.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// Every file, link and folder of the working tree outside .git, with its kind,
    /// mode and contents.
    fn tree_state(root: &Path) -> BTreeMap<PathBuf, (&'static str, u32, Vec<u8>)> {
        use std::os::unix::fs::PermissionsExt;
        let mut state = BTreeMap::new();
        let mut folders = vec![root.to_owned()];
        while let Some(folder) = folders.pop() {
            for entry in fs::read_dir(&folder).unwrap() {
                let full = entry.unwrap().path();
                if full == root.join(".git") {
                    continue;
                }
                let meta = full.symlink_metadata().unwrap();
                let (kind, contents) = if meta.is_symlink() {
                    let target = fs::read_link(&full).unwrap();
                    ("link", target.into_os_string().into_encoded_bytes())
                } else if meta.is_dir() {
                    folders.push(full.clone());
                    ("folder", Vec::new())
                } else {
                    ("file", fs::read(&full).unwrap())
                };
                let relative = full.strip_prefix(root).unwrap().to_owned();
                state.insert(relative, (kind, meta.permissions().mode(), contents));
            }
        }
        state
    }

    fn no_work_left(root: &Path) {
        let index = git_path(Repo::new(root), "index").unwrap();
        let left: Vec<PathBuf> = fs::read_dir(index.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with(RESTORE_FOLDER) || name.starts_with(SCRATCH_INDEX)
            })
            .map(|entry| entry.path())
            .collect();
        assert!(left.is_empty(), "left in the git folder: {left:?}");
    }

    /// Restores the session's checkpoint for `turn` and keeps it, after checking that
    /// the same restore, not kept, leaves the working tree exactly as it was.
    fn restore_kept(root: &Path, session: &Session, turn: u64) -> Result<Restored> {
        let before = tree_state(root);
        match restore(root, session, turn, &Cancel::default(), || false) {
            Err(Error::Cancelled) => {}
            Err(Error::Gone) => return Err(Error::Gone),
            other => panic!("a restore not kept returned {other:?}"),
        }
        assert_eq!(
            tree_state(root),
            before,
            "a restore not kept changes nothing"
        );
        no_work_left(root);
        let restored = restore(root, session, turn, &Cancel::default(), || true);
        no_work_left(root);
        restored
    }

    /// Points `name` at a commit of the working tree saved `days` ago.
    fn saved_days_ago(root: &Path, name: &str, days: u64) {
        let tree = snapshot(root).unwrap().tree;
        let then = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
        let date = format!(
            "@{} +0000",
            then.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
        let output = git(root)
            .args(["commit-tree", &tree, "-m", "old"])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        sh(root, &["update-ref", name, text(&output.stdout).as_str()]);
    }

    #[test]
    fn checkpoints_older_than_keep_days_go_from_every_store() {
        let dir = repository();
        let root = dir.path();
        let old = KEEP_DAYS + 1;
        saved_days_ago(root, &format!("{REFS}/gone/1/1"), old);
        saved_days_ago(root, &format!("{REFS}/gone/1/{BEFORE_RESTORE}"), old);
        saved_days_ago(root, &name(2, 1), old);
        saved_days_ago(root, &format!("{REFS}/other/4/7"), KEEP_DAYS - 1);

        save_turn(root, 2, 2, None, KEEP);

        assert_eq!(refs(root), [format!("{REFS}/other/4/7"), name(2, 2)]);
    }

    #[test]
    fn a_ref_that_cannot_be_expired_keeps_no_other_from_going() {
        let dir = repository();
        let root = dir.path();
        let old = KEEP_DAYS + 1;
        saved_days_ago(root, &format!("{REFS}/gone/1/1"), old);
        saved_days_ago(root, &format!("{REFS}/gone/1/2"), old);
        write(root, &format!(".git/{REFS}/gone/1/2.lock"), "");

        save_turn(root, 2, 1, None, KEEP);

        assert_eq!(refs(root), [format!("{REFS}/gone/1/2"), name(2, 1)]);
    }

    #[test]
    fn a_file_dated_in_the_future_is_still_restored() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");
        fs::File::options()
            .write(true)
            .open(root.join("edited.txt"))
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(60 * 60))
            .unwrap();

        let restored = restore_kept(root, &session(1), 1).unwrap();

        assert!(restored.skipped.is_empty(), "{:?}", restored.skipped);
        assert_eq!(read(root, "edited.txt").as_deref(), Some("first\n"));
    }

    #[test]
    fn what_an_unfinished_save_or_restore_left_goes_after_keep_days() {
        let dir = repository();
        let root = dir.path();
        let git_folder = root.join(".git");
        let old = git_folder.join(format!("{RESTORE_FOLDER}1-0"));
        write(&old, "stage/kept.txt", "a checkpoint's copy\n");
        fs::create_dir_all(old.join("aside/empty")).unwrap();
        let originals = git_folder.join(format!("{RESTORE_FOLDER}1-1"));
        write(&originals, "aside/kept.txt", "an original\n");
        let old_index = git_folder.join(format!("{SCRATCH_INDEX}1-0.index"));
        fs::write(&old_index, "").unwrap();
        let fresh = git_folder.join(format!("{RESTORE_FOLDER}2-0"));
        fs::create_dir(&fresh).unwrap();
        let then = SystemTime::now() - Duration::from_secs((KEEP_DAYS + 1) * 24 * 60 * 60);
        for stale in [&old, &originals, &old_index] {
            fs::File::open(stale).unwrap().set_modified(then).unwrap();
        }

        save_turn(root, 1, 1, None, KEEP);

        assert!(!old.exists() && !old_index.exists());
        assert!(fresh.exists(), "a restore of the last days is kept");
        assert_eq!(
            read(&originals, "aside/kept.txt").as_deref(),
            Some("an original\n"),
            "a user's originals are never expired"
        );
    }

    #[test]
    fn a_restore_stuck_in_a_filter_is_cancelled_and_changes_nothing() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");
        write(root, "new.txt", "later\n");
        fs::remove_file(root.join("kept.txt")).unwrap();
        let pid_file = root.join(".git/filter-pid");
        sh(
            root,
            &[
                "config",
                "filter.slow.smudge",
                &format!("echo $$ > '{}'; sleep 30; cat", pid_file.display()),
            ],
        );
        write(root, ".git/info/attributes", "*.txt filter=slow\n");
        let before = tree_state(root);
        let cancel = Cancel::default();
        let firing = cancel.clone();
        let watched = root.to_owned();
        let fire = std::thread::spawn(move || {
            let until = std::time::Instant::now() + Duration::from_secs(10);
            let filter = loop {
                let written = read(&watched, ".git/filter-pid");
                if let Some(pid) = written.and_then(|pid| pid.trim().parse().ok()) {
                    break Some(pid);
                }
                if std::time::Instant::now() > until {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            firing.cancel();
            filter
        });
        let start = std::time::Instant::now();

        let result = restore(root, &session(1), 1, &cancel, || true);

        let filter: libc::pid_t = fire.join().unwrap().expect("the filter ran");
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(tree_state(root), before);
        no_work_left(root);
        let gone = std::time::Instant::now() + Duration::from_secs(5);
        // SAFETY: signal 0 only asks whether the process exists.
        while unsafe { libc::kill(filter, 0) } == 0 {
            assert!(
                std::time::Instant::now() < gone,
                "the filter was left running"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_failing_filter_changes_nothing() {
        let dir = repository();
        let root = dir.path();
        write(root, "a-first.txt", "at the checkpoint\n");
        write(root, "z-broken.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        write(root, ".gitignore", "*.log\n");
        write(root, "a-first.txt", "later\n");
        write(root, "z-broken.txt", "later\n");
        write(root, "made/later.txt", "later\n");
        fs::remove_file(root.join("kept.txt")).unwrap();
        sh(root, &["config", "filter.broken.clean", "cat"]);
        sh(root, &["config", "filter.broken.smudge", "false"]);
        sh(root, &["config", "filter.broken.required", "true"]);
        write(root, ".git/info/attributes", "z-broken.txt filter=broken\n");
        let before = tree_state(root);

        let result = restore(root, &session(1), 1, &Cancel::default(), || true);

        assert!(
            matches!(&result, Err(Error::Git(error)) if error.to_string().contains("smudge")),
            "{result:?}"
        );
        assert_eq!(tree_state(root), before);
        no_work_left(root);
    }

    #[test]
    fn a_folder_that_refuses_a_write_puts_everything_back() {
        // SAFETY: geteuid only reads the process's user id.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = repository();
        let root = dir.path();
        write(root, "z/locked.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");
        write(root, "made/later.txt", "later\n");
        fs::remove_file(root.join("kept.txt")).unwrap();
        write(root, "z/locked.txt", "later\n");
        let locked = root.join("z");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let before = tree_state(root);

        let result = restore(root, &session(1), 1, &Cancel::default(), || true);

        let after = tree_state(root);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
        assert_eq!(after, before);
        no_work_left(root);
    }

    #[test]
    fn a_restore_not_kept_gives_back_bytes_a_filter_would_change() {
        let dir = repository();
        let root = dir.path();
        sh(root, &["config", "filter.lf.clean", "tr -d '\\r'"]);
        write(root, ".git/info/attributes", "*.txt filter=lf\n");
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\r\nwith returns\r\n");

        restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(read(root, "edited.txt").as_deref(), Some("first\n"));
    }

    #[test]
    fn a_restore_not_kept_removes_a_file_the_checkpoints_rules_ignore() {
        let dir = repository();
        let root = dir.path();
        write(root, ".gitignore", "*.log\nbuild/\ngen/\n");
        write(root, "gen/out.txt", "generated\n");
        sh(root, &["add", "--force", "gen/out.txt"]);
        save_turn(root, 1, 1, None, KEEP);
        sh(root, &["rm", "-q", "--cached", "gen/out.txt"]);
        fs::remove_dir_all(root.join("gen")).unwrap();
        write(root, ".gitignore", "*.log\nbuild/\n");

        restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(read(root, "gen/out.txt").as_deref(), Some("generated\n"));
    }

    #[test]
    fn the_files_are_in_place_when_keep_is_asked() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");
        let mut seen = None;

        restore(root, &session(1), 1, &Cancel::default(), || {
            seen = read(root, "edited.txt");
            true
        })
        .unwrap();

        assert_eq!(seen.as_deref(), Some("first\n"));
        assert_eq!(read(root, "edited.txt").as_deref(), Some("first\n"));
    }

    #[test]
    fn a_file_kept_off_disk_by_a_sparse_checkout_goes_through_no_filter() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "kept.txt", "changed\n");
        save_turn(root, 1, 2, None, KEEP);
        sh(root, &["update-index", "--skip-worktree", "kept.txt"]);
        fs::remove_file(root.join("kept.txt")).unwrap();
        sh(root, &["config", "filter.broken.clean", "cat"]);
        sh(root, &["config", "filter.broken.smudge", "false"]);
        sh(root, &["config", "filter.broken.required", "true"]);
        write(root, ".git/info/attributes", "kept.txt filter=broken\n");

        restore_kept(root, &session(1), 2).unwrap();

        assert!(!root.join("kept.txt").exists());
    }

    #[test]
    fn a_restore_never_reuses_a_folder_an_earlier_keystone_left() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");
        let first = COUNT.load(Ordering::Relaxed);
        let left: Vec<PathBuf> = (first..first + 200)
            .map(|n| {
                let folder = root.join(format!(".git/{RESTORE_FOLDER}{}-{n}", std::process::id()));
                write(&folder, "aside/precious.txt", "the only copy\n");
                folder
            })
            .collect();

        let declined = restore(root, &session(1), 1, &Cancel::default(), || false);
        assert!(matches!(declined, Err(Error::Cancelled)), "{declined:?}");
        restore(root, &session(1), 1, &Cancel::default(), || true).unwrap();

        for folder in left {
            assert_eq!(
                read(&folder, "aside/precious.txt").as_deref(),
                Some("the only copy\n"),
                "{folder:?}"
            );
        }
    }

    #[test]
    fn a_folder_made_where_a_deleted_file_stood_is_left_and_said() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");
        write(root, "later", "made after the checkpoint\n");
        let (started, go) = (root.join(".git/started"), root.join(".git/go"));
        sh(
            root,
            &[
                "config",
                "filter.wait.smudge",
                &format!(
                    "touch '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; cat",
                    started.display(),
                    go.display()
                ),
            ],
        );
        write(root, ".git/info/attributes", "*.txt filter=wait\n");
        let editor = {
            let root = root.to_owned();
            std::thread::spawn(move || {
                while !root.join(".git/started").exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                fs::remove_file(root.join("later")).unwrap();
                write(&root, "later/precious.txt", "saved during the restore\n");
                fs::write(root.join(".git/go"), "").unwrap();
            })
        };

        let restored = restore(root, &session(1), 1, &Cancel::default(), || true).unwrap();

        editor.join().unwrap();
        assert_eq!(restored.skipped, [PathBuf::from("later")]);
        assert_eq!(
            read(root, "later/precious.txt").as_deref(),
            Some("saved during the restore\n")
        );
    }

    #[test]
    fn a_file_saved_while_the_restore_converts_is_left_and_said() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");
        let (started, go) = (root.join(".git/started"), root.join(".git/go"));
        sh(
            root,
            &[
                "config",
                "filter.wait.smudge",
                &format!(
                    "touch '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; cat",
                    started.display(),
                    go.display()
                ),
            ],
        );
        write(root, ".git/info/attributes", "*.txt filter=wait\n");
        let editor = {
            let root = root.to_owned();
            std::thread::spawn(move || {
                while !root.join(".git/started").exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                write(&root, "edited.txt", "typed during the restore\n");
                fs::write(root.join(".git/go"), "").unwrap();
            })
        };

        let restored = restore(root, &session(1), 1, &Cancel::default(), || true).unwrap();

        editor.join().unwrap();
        assert_eq!(restored.skipped, [PathBuf::from("edited.txt")]);
        assert_eq!(
            read(root, "edited.txt").as_deref(),
            Some("typed during the restore\n")
        );
    }

    #[test]
    fn a_file_of_a_nested_repository_the_outer_index_tracks_is_not_deleted() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "vendor/keep.rs", "theirs\n");
        sh(root, &["add", "vendor/keep.rs"]);
        init(&root.join("vendor"));

        let restored = restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(restored.skipped, [PathBuf::from("vendor/keep.rs")]);
        assert_eq!(read(root, "vendor/keep.rs").as_deref(), Some("theirs\n"));
    }

    #[test]
    fn a_private_folder_a_restore_removed_comes_back_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "private/later.txt", "made after the checkpoint\n");
        fs::set_permissions(root.join("private"), fs::Permissions::from_mode(0o700)).unwrap();

        restore_kept(root, &session(1), 1).unwrap();

        assert!(!root.join("private").exists());
    }

    #[test]
    fn an_undo_that_fails_keeps_the_originals_and_says_where() {
        // SAFETY: geteuid only reads the process's user id.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = repository();
        let root = dir.path();
        write(root, "y/a.txt", "first\n");
        save_turn(root, 1, 1, None, KEEP);
        write(root, "y/a.txt", "later\n");
        let locked = root.join("y");

        let result = restore(root, &session(1), 1, &Cancel::default(), || {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
            false
        });

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let Err(Error::NotPutBack { cause, kept }) = result else {
            panic!("{result:?}");
        };
        assert!(matches!(*cause, Error::Cancelled));
        assert!(kept.ends_with("aside"), "{kept:?}");
        assert_eq!(read(&kept, "y/a.txt").as_deref(), Some("later\n"));
        assert!(!kept.with_file_name("stage").exists());
    }

    #[test]
    fn a_file_edited_while_the_fork_opens_is_kept_over_its_original() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");

        let result = restore(root, &session(1), 1, &Cancel::default(), || {
            write(root, "edited.txt", "typed while the fork opened\n");
            false
        });

        let Err(Error::NotPutBack { kept, .. }) = result else {
            panic!("{result:?}");
        };
        assert_eq!(
            read(root, "edited.txt").as_deref(),
            Some("typed while the fork opened\n")
        );
        assert_eq!(read(&kept, "edited.txt").as_deref(), Some("later\n"));
    }

    #[test]
    fn a_restored_file_deleted_while_the_fork_opens_gets_its_original_back() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");

        let result = restore(root, &session(1), 1, &Cancel::default(), || {
            fs::remove_file(root.join("edited.txt")).unwrap();
            false
        });

        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert_eq!(read(root, "edited.txt").as_deref(), Some("later\n"));
    }

    #[test]
    fn a_file_made_while_the_fork_opens_where_one_was_deleted_is_kept() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "later.txt", "before the restore\n");

        let result = restore(root, &session(1), 1, &Cancel::default(), || {
            write(root, "later.txt", "made while the fork opened\n");
            false
        });

        let Err(Error::NotPutBack { kept, .. }) = result else {
            panic!("{result:?}");
        };
        assert_eq!(
            read(root, "later.txt").as_deref(),
            Some("made while the fork opened\n")
        );
        assert_eq!(
            read(&kept, "later.txt").as_deref(),
            Some("before the restore\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_became_a_link_is_restored() {
        let dir = repository();
        let root = dir.path();
        write(root, "x/y.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_dir_all(root.join("x")).unwrap();
        write(root, "real/y.txt", "elsewhere\n");
        std::os::unix::fs::symlink("real", root.join("x")).unwrap();

        restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(
            read(root, "x/y.txt").as_deref(),
            Some("at the checkpoint\n")
        );
        assert!(!root.join("x").symlink_metadata().unwrap().is_symlink());
    }

    #[test]
    fn a_path_a_folder_keeps_taken_goes_through_no_filter() {
        let dir = repository();
        let root = dir.path();
        write(root, "blocked.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_file(root.join("blocked.txt")).unwrap();
        write(root, "blocked.txt/trace.log", "ignored\n");
        write(root, "edited.txt", "later\n");
        sh(root, &["config", "filter.broken.clean", "cat"]);
        sh(root, &["config", "filter.broken.smudge", "false"]);
        sh(root, &["config", "filter.broken.required", "true"]);
        write(root, ".git/info/attributes", "blocked.txt filter=broken\n");

        let restored = restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(restored.skipped, [PathBuf::from("blocked.txt")]);
        assert_eq!(read(root, "edited.txt").as_deref(), Some("first\n"));
    }

    #[test]
    fn a_ref_hook_that_hangs_does_not_hold_a_restore() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");
        let hook = root.join(".git/hooks/reference-transaction");
        write(
            root,
            ".git/hooks/reference-transaction",
            "#!/bin/sh\nsleep 30\n",
        );
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let start = std::time::Instant::now();

        restore_kept(root, &session(1), 1).unwrap();

        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(read(root, "edited.txt").as_deref(), Some("first\n"));
    }

    #[test]
    fn a_checkpoint_saves_tracked_and_untracked_files_without_ignored_ones() {
        let dir = repository();
        let root = dir.path();
        let before = untouched(root);
        let start = snapshot(root).unwrap().tree;
        write(root, "new.txt", "new\n");
        write(root, "edited.txt", "second\n");
        let saved = save_turn(root, 7, 1, Some(&start), KEEP);
        assert_eq!(saved.root, root);
        assert_eq!(saved.turns, BTreeMap::from([(1, Some(2))]));
        assert_eq!(
            files(root, &name(7, 1)),
            [
                ".gitignore",
                "deleted.txt",
                "edited.txt",
                "kept.txt",
                "new.txt",
                "staged.txt"
            ]
        );
        assert_eq!(untouched(root), before);
        assert_eq!(
            sh(root, &["status", "--porcelain"]),
            " M edited.txt\nA  staged.txt\n?? new.txt\n"
        );
    }

    #[test]
    fn restore_brings_back_a_turn_and_deletes_what_came_after() {
        let dir = repository();
        let root = dir.path();
        let start = snapshot(root).unwrap().tree;
        write(root, "one.txt", "turn one\n");
        write(root, "edited.txt", "turn one\n");
        fs::remove_file(root.join("deleted.txt")).unwrap();
        save_turn(root, 3, 1, Some(&start), KEEP);
        let after_one = snapshot(root).unwrap().tree;
        let before = untouched(root);
        write(root, "edited.txt", "turn two\n");
        write(root, "deleted.txt", "back in turn two\n");
        write(root, "two/nested/file.txt", "turn two\n");
        fs::remove_file(root.join("one.txt")).unwrap();
        write(root, "debug.log", "changed while ignored\n");
        save_turn(root, 3, 2, None, KEEP);
        let after_two = snapshot(root).unwrap().tree;

        assert_eq!(
            restore_kept(root, &session(3), 1).unwrap(),
            Restored::default()
        );

        assert_eq!(snapshot(root).unwrap().tree, after_one);
        assert_eq!(read(root, "one.txt").as_deref(), Some("turn one\n"));
        assert_eq!(read(root, "edited.txt").as_deref(), Some("turn one\n"));
        assert_eq!(read(root, "deleted.txt"), None);
        assert!(!root.join("two").exists(), "folders left empty are removed");
        assert_eq!(
            read(root, "debug.log").as_deref(),
            Some("changed while ignored\n")
        );
        assert_eq!(read(root, "build/out.bin").as_deref(), Some("ignored\n"));
        assert_eq!(untouched(root), before);
        assert_eq!(
            list(root, &session(3)).unwrap().turns.len(),
            2,
            "restoring keeps every checkpoint"
        );
        assert_eq!(
            sh(
                root,
                &[
                    "rev-parse",
                    &format!("{}^{{tree}}", session(3).name(BEFORE_RESTORE))
                ]
            )
            .trim(),
            after_two,
            "what the restore replaced is kept"
        );
    }

    #[test]
    fn a_file_ignored_since_the_checkpoint_is_left_alone() {
        let dir = repository();
        let root = dir.path();
        write(root, "notes/todo.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        write(root, ".gitignore", "*.log\nbuild/\nnotes/\n");
        write(root, "notes/todo.txt", "ignored now\n");
        write(root, "notes/scratch.txt", "ignored now\n");

        restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(read(root, ".gitignore").as_deref(), Some("*.log\nbuild/\n"));
        assert_eq!(
            read(root, "notes/todo.txt").as_deref(),
            Some("ignored now\n")
        );
        assert_eq!(
            read(root, "notes/scratch.txt").as_deref(),
            Some("ignored now\n")
        );
    }

    #[test]
    fn a_file_ignored_at_the_checkpoint_survives_a_later_unignore() {
        let dir = repository();
        let root = dir.path();
        write(root, ".gitignore", "*.log\nbuild/\n.env\n");
        write(root, "secrets/.gitignore", "*.key\n");
        write(root, ".env", "TOKEN=1\n");
        write(root, "secrets/a.key", "key\n");
        save_turn(root, 1, 1, None, KEEP);
        write(root, ".gitignore", "*.log\nbuild/\n");
        fs::remove_file(root.join("secrets/.gitignore")).unwrap();
        save_turn(root, 1, 2, None, KEEP);

        restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(read(root, ".env").as_deref(), Some("TOKEN=1\n"));
        assert_eq!(read(root, "secrets/a.key").as_deref(), Some("key\n"));
        assert_eq!(read(root, "secrets/.gitignore").as_deref(), Some("*.key\n"));
    }

    #[test]
    fn an_ignored_file_where_a_folder_would_go_is_left_alone() {
        let dir = repository();
        let root = dir.path();
        write(root, "out/kept.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_dir_all(root.join("out")).unwrap();
        write(root, ".gitignore", "*.log\nbuild/\nout\n");
        write(root, "out", "an ignored file now\n");

        let restored = restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(read(root, "out").as_deref(), Some("an ignored file now\n"));
        assert_eq!(restored.skipped, [PathBuf::from("out/kept.txt")]);
    }

    #[test]
    fn a_folder_replaced_by_a_file_is_restored() {
        let dir = repository();
        let root = dir.path();
        write(root, "item", "a file\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_file(root.join("item")).unwrap();
        write(root, "item/inside.txt", "a folder now\n");

        restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(read(root, "item").as_deref(), Some("a file\n"));
    }

    #[test]
    fn a_folder_kept_by_an_ignored_file_is_reported() {
        let dir = repository();
        let root = dir.path();
        write(root, "item", "a file\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_file(root.join("item")).unwrap();
        write(root, "item/inside.txt", "a folder now\n");
        write(root, "item/trace.log", "ignored\n");

        let restored = restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(restored.skipped, [PathBuf::from("item")]);
        assert_eq!(read(root, "item/trace.log").as_deref(), Some("ignored\n"));
    }

    #[test]
    fn a_nested_repository_is_never_written_into() {
        let dir = repository();
        let root = dir.path();
        write(root, "vendor", "a file\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_file(root.join("vendor")).unwrap();
        let vendor = root.join("vendor");
        fs::create_dir(&vendor).unwrap();
        init(&vendor);
        write(&vendor, "work.rs", "committed\n");
        sh(&vendor, &["add", "--all"]);
        sh(&vendor, &["commit", "-q", "-m", "work"]);
        write(root, "linked", "a file\n");
        save_turn(root, 1, 2, None, KEEP);
        assert!(
            !files(root, &name(1, 2))
                .iter()
                .any(|f| f.starts_with("vendor"))
        );

        let restored = restore_kept(root, &session(1), 2).unwrap();
        assert!(restored.skipped.is_empty());
        let restored = restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(restored.skipped, [PathBuf::from("vendor")]);
        assert_eq!(read(&vendor, "work.rs").as_deref(), Some("committed\n"));
        assert!(vendor.join(".git").is_dir());
        assert_eq!(read(root, "linked"), None);
    }

    #[test]
    fn a_tracked_submodule_is_left_alone() {
        let dir = repository();
        let root = dir.path();
        let vendor = root.join("vendor");
        fs::create_dir(&vendor).unwrap();
        init(&vendor);
        write(&vendor, "work.rs", "committed\n");
        sh(&vendor, &["add", "--all"]);
        sh(&vendor, &["commit", "-q", "-m", "work"]);
        sh(root, &["add", "vendor"]);
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_dir_all(&vendor).unwrap();
        write(root, "vendor", "a file now\n");
        save_turn(root, 1, 2, None, KEEP);
        fs::remove_file(root.join("vendor")).unwrap();
        fs::create_dir(&vendor).unwrap();
        init(&vendor);
        write(&vendor, "other.rs", "committed\n");
        sh(&vendor, &["add", "--all"]);
        sh(&vendor, &["commit", "-q", "-m", "other"]);

        let restored = restore_kept(root, &session(1), 2).unwrap();

        assert_eq!(restored.skipped, [PathBuf::from("vendor")]);
        assert_eq!(read(&vendor, "other.rs").as_deref(), Some("committed\n"));
    }

    #[test]
    fn an_empty_nested_repository_does_not_stop_checkpoints() {
        let dir = repository();
        let root = dir.path();
        fs::create_dir(root.join("fresh")).unwrap();
        init(&root.join("fresh"));
        write(root, "fresh/draft.txt", "not committed\n");
        write(root, "new.txt", "new\n");

        let saved = save_turn(root, 1, 1, None, KEEP);

        assert_eq!(saved.turns, BTreeMap::from([(1, None)]));
        let saved = files(root, &name(1, 1));
        assert!(saved.contains(&"new.txt".to_owned()));
        assert!(!saved.iter().any(|f| f.starts_with("fresh")));
    }

    #[test]
    fn a_file_left_out_of_the_checkpoint_is_not_deleted() {
        let dir = repository();
        let root = dir.path();
        fs::File::create(root.join("data.db"))
            .unwrap()
            .set_len(LARGE + 1)
            .unwrap();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "data.db", "small now\n");

        let restored = restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(read(root, "data.db").as_deref(), Some("small now\n"));
        assert_eq!(restored.skipped, [PathBuf::from("data.db")]);
    }

    #[test]
    fn what_a_checkpoint_left_out_survives_git_gc() {
        let dir = repository();
        let root = dir.path();
        fs::File::create(root.join("data.db"))
            .unwrap()
            .set_len(LARGE + 1)
            .unwrap();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "data.db", "small now\n");
        sh(
            root,
            &["-c", "gc.reflogExpire=now", "gc", "--prune=now", "--quiet"],
        );

        let restored = restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(read(root, "data.db").as_deref(), Some("small now\n"));
        assert_eq!(restored.skipped, [PathBuf::from("data.db")]);
    }

    #[test]
    fn a_sparse_checkout_keeps_its_cone() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "kept.txt", "changed\n");
        save_turn(root, 1, 2, None, KEEP);
        sh(root, &["update-index", "--skip-worktree", "kept.txt"]);
        fs::remove_file(root.join("kept.txt")).unwrap();

        restore_kept(root, &session(1), 2).unwrap();

        assert!(
            !root.join("kept.txt").exists(),
            "outside the cone stays off disk"
        );
    }

    #[test]
    fn a_same_size_edit_in_the_second_the_index_was_written_is_saved() {
        let dir = repository();
        let root = dir.path();
        sh(root, &["config", "core.trustctime", "false"]);
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let stamp = |file: &str| {
            fs::File::options()
                .write(true)
                .open(root.join(file))
                .unwrap()
                .set_modified(then)
                .unwrap()
        };
        stamp("deleted.txt");
        sh(root, &["update-index", "--refresh"]);
        stamp(".git/index");
        write(
            root,
            "deleted.txt",
            "changed
",
        );
        stamp("deleted.txt");

        save_turn(root, 1, 1, None, KEEP);

        let blob = sh(root, &["rev-parse", &format!("{}:deleted.txt", name(1, 1))]);
        assert_eq!(
            sh(root, &["cat-file", "blob", blob.trim()]),
            "changed
"
        );
    }

    #[test]
    fn files_the_user_hid_from_git_status_are_still_saved() {
        let dir = repository();
        let root = dir.path();
        sh(root, &["update-index", "--assume-unchanged", "kept.txt"]);
        sh(root, &["update-index", "--skip-worktree", "edited.txt"]);
        sh(root, &["update-index", "--skip-worktree", "deleted.txt"]);
        sh(root, &["update-index", "--assume-unchanged", "deleted.txt"]);
        assert!(sh(root, &["ls-files", "-v", "deleted.txt"]).starts_with("s "));
        for file in ["kept.txt", "edited.txt", "deleted.txt"] {
            write(root, file, "changed\n");
        }
        save_turn(root, 1, 1, None, KEEP);
        for file in ["kept.txt", "edited.txt", "deleted.txt"] {
            let blob = sh(root, &["rev-parse", &format!("{}:{file}", name(1, 1))]);
            assert_eq!(
                sh(root, &["cat-file", "blob", blob.trim()]),
                "changed\n",
                "{file}"
            );
        }
    }

    #[test]
    fn a_file_under_a_nested_repository_is_left_and_reported() {
        let dir = repository();
        let root = dir.path();
        write(root, "vendor/old.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_dir_all(root.join("vendor")).unwrap();
        let vendor = root.join("vendor");
        fs::create_dir(&vendor).unwrap();
        init(&vendor);
        write(&vendor, "work.rs", "theirs\n");

        let restored = restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(restored.skipped, [PathBuf::from("vendor/old.txt")]);
        assert_eq!(read(&vendor, "old.txt"), None);
    }

    #[test]
    fn a_large_untracked_file_stays_out_and_is_kept() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        let large = fs::File::create(root.join("model.bin")).unwrap();
        large.set_len(LARGE + 1).unwrap();
        write(root, "small.txt", "small\n");
        save_turn(root, 1, 2, None, KEEP);
        assert!(!files(root, &name(1, 2)).contains(&"model.bin".to_owned()));

        restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(
            fs::metadata(root.join("model.bin")).unwrap().len(),
            LARGE + 1
        );
        assert_eq!(read(root, "small.txt"), None);
        write(root, "grown.bin", "small at the checkpoint\n");
        save_turn(root, 1, 3, None, KEEP);
        fs::File::create(root.join("grown.bin"))
            .unwrap()
            .set_len(LARGE + 1)
            .unwrap();
        let restored = restore_kept(root, &session(1), 3).unwrap();
        assert_eq!(restored.skipped, [PathBuf::from("grown.bin")]);
        assert_eq!(
            fs::metadata(root.join("grown.bin")).unwrap().len(),
            LARGE + 1
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_unusual_names_round_trip() {
        let dir = repository();
        let root = dir.path();
        std::os::unix::fs::symlink("kept.txt", root.join("link")).unwrap();
        write(root, "line\nbreak.txt", "newline\n");
        write(root, "spaced name ü.txt", "spaced\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_file(root.join("link")).unwrap();
        fs::remove_file(root.join("line\nbreak.txt")).unwrap();
        write(root, "link", "a file now\n");
        write(root, "spaced name ü.txt", "changed\n");
        std::os::unix::fs::symlink("edited.txt", root.join("later link")).unwrap();
        save_turn(root, 1, 2, None, KEEP);

        restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(
            fs::read_link(root.join("link")).unwrap(),
            Path::new("kept.txt")
        );
        assert_eq!(read(root, "line\nbreak.txt").as_deref(), Some("newline\n"));
        assert_eq!(read(root, "spaced name ü.txt").as_deref(), Some("spaced\n"));
        assert!(root.join("later link").symlink_metadata().is_err());
    }

    #[test]
    fn pruning_keeps_the_newest_and_removes_a_sessions_refs() {
        let dir = repository();
        let root = dir.path();
        for turn in 1..=4 {
            write(root, "count.txt", &turn.to_string());
            save_turn(root, 5, turn, None, 2);
        }
        save_turn(root, 6, 1, None, 2);
        assert_eq!(
            refs(root),
            [name(5, 3), name(5, 4), name(6, 1)],
            "each save keeps the newest two"
        );
        assert!(matches!(
            restore_kept(root, &session(5), 1),
            Err(Error::Gone)
        ));
        restore_kept(root, &session(5), 3).unwrap();
        assert!(refs(root).contains(&session(5).name(BEFORE_RESTORE)));
        prune(root, &session(5), 0).unwrap();
        assert_eq!(refs(root), [name(6, 1)]);
    }

    #[test]
    fn turn_ten_sorts_after_turn_nine() {
        let dir = repository();
        let root = dir.path();
        for turn in [9, 10] {
            save_turn(root, 2, turn, None, 1);
        }
        assert_eq!(
            list(root, &session(2)).unwrap().turns,
            BTreeMap::from([(10, None)])
        );
        assert!(
            list(root, &session(20)).unwrap().turns.is_empty(),
            "session 20 is not session 2"
        );
    }

    #[test]
    fn workspaces_sharing_a_repository_keep_their_own_checkpoints() {
        let dir = repository();
        let root = dir.path();
        let other = Session {
            store: "other".into(),
            id: 1,
        };
        write(root, "mine.txt", "mine\n");
        save_turn(root, 1, 1, None, KEEP);
        let theirs = snapshot(root).unwrap().tree;
        write(root, "mine.txt", "changed\n");
        let tree = snapshot(root).unwrap();
        record(root, &other, 1, &tree, None, KEEP).unwrap();
        assert!(
            record(root, &session(1), 1, &tree, None, KEEP).is_err(),
            "a saved checkpoint is never replaced"
        );

        restore_kept(root, &session(1), 1).unwrap();

        assert_eq!(snapshot(root).unwrap().tree, theirs);
        assert_eq!(list(root, &other).unwrap().turns.len(), 1);
    }

    #[test]
    fn a_worktree_checkpoints_its_own_files() {
        let dir = repository();
        let root = dir.path();
        let linked = tempfile::tempdir().unwrap();
        let tree = linked.path().join("tree");
        sh(
            root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                tree.to_str().unwrap(),
            ],
        );
        write(&tree, "side.txt", "in the worktree\n");
        let saved = save_turn(&tree, 9, 1, None, KEEP);
        assert_eq!(saved.root, tree);
        let saved_files = files(root, &name(9, 1));
        assert!(saved_files.iter().any(|f| f == "side.txt"));
        assert!(
            !saved_files.iter().any(|f| f == "staged.txt"),
            "the main checkout's own index stays out"
        );
        write(&tree, "side.txt", "changed\n");
        restore_kept(&saved.root, &session(9), 1).unwrap();
        assert_eq!(
            read(&tree, "side.txt").as_deref(),
            Some("in the worktree\n")
        );
        assert_eq!(read(root, "staged.txt").as_deref(), Some("staged\n"));
    }

    #[test]
    fn a_folder_outside_any_repository_has_no_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(root(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_repository_before_its_first_commit_has_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sh(root, &["init", "-q", "-b", "main"]);
        write(root, "first.txt", "first\n");
        save_turn(root, 1, 1, None, KEEP);
        write(root, "first.txt", "changed\n");
        write(root, "second.txt", "second\n");
        restore_kept(root, &session(1), 1).unwrap();
        assert_eq!(read(root, "first.txt").as_deref(), Some("first\n"));
        assert_eq!(read(root, "second.txt"), None);
        assert!(
            !root.join(".git/index").exists(),
            "the user's index is never written"
        );
    }

    /// Restores the session's checkpoint for `turn` and ends as a crash would once every
    /// file is in place, with nothing undone: a panic leaves the restore's folder.
    fn crash_mid_restore(root: &Path, session: &Session, turn: u64) {
        let crashed = std::panic::catch_unwind(|| {
            let _ = restore(root, session, turn, &Cancel::default(), || panic!("killed"));
        });
        assert!(crashed.is_err());
    }

    /// The restore folder a crash left, and the steps its journal records.
    fn left_by_crash(root: &Path) -> (PathBuf, Vec<Step>) {
        let folders = restore_folders(Repo::new(root)).unwrap();
        let [folder] = &folders[..] else {
            panic!("{folders:?}");
        };
        let journal = fs::read(folder.join(JOURNAL)).unwrap();
        (folder.clone(), parse(&journal).unwrap())
    }

    fn unfinished_in(root: &Path) -> bool {
        list(root, &session(1)).unwrap().unfinished
    }

    /// A working tree a restore to turn one changes in every way it can.
    fn edited_since_turn_one() -> tempfile::TempDir {
        let dir = repository();
        let root = dir.path();
        write(root, "gone/deep/file.txt", "at the checkpoint\n");
        write(root, "z-last.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");
        write(root, "z-last.txt", "later\n");
        write(root, "made/later.txt", "later\n");
        fs::remove_file(root.join("kept.txt")).unwrap();
        fs::remove_dir_all(root.join("gone")).unwrap();
        dir
    }

    #[test]
    fn a_restore_keystone_did_not_finish_is_put_back() {
        let dir = edited_since_turn_one();
        let root = dir.path();
        let before = tree_state(root);

        crash_mid_restore(root, &session(1), 1);

        assert_ne!(tree_state(root), before);
        assert!(unfinished_in(root));
        assert!(
            list(root, &session(2)).unwrap().unfinished,
            "any session sees it"
        );
        let again = restore(root, &session(1), 1, &Cancel::default(), || true);
        assert!(matches!(again, Err(Error::Unfinished)), "{again:?}");

        put_back(root).unwrap();

        assert_eq!(tree_state(root), before);
        no_work_left(root);
        assert!(!unfinished_in(root));
        put_back(root).unwrap();
        assert_eq!(tree_state(root), before);
    }

    #[test]
    fn a_restore_still_running_is_neither_offered_nor_put_back() {
        let dir = edited_since_turn_one();
        let root = dir.path();
        let mut seen = None;

        restore(root, &session(1), 1, &Cancel::default(), || {
            seen = Some((unfinished_in(root), put_back(root)));
            true
        })
        .unwrap();

        let Some((false, Err(Error::Busy))) = seen else {
            panic!("{seen:?}");
        };
        assert_eq!(read(root, "kept.txt").as_deref(), Some("kept\n"));
        assert_eq!(read(root, "edited.txt").as_deref(), Some("first\n"));
        no_work_left(root);
    }

    #[test]
    fn a_file_changed_after_the_crash_stays_and_its_original_is_kept() {
        let dir = edited_since_turn_one();
        let root = dir.path();
        crash_mid_restore(root, &session(1), 1);
        write(root, "edited.txt", "typed after the crash\n");

        let result = put_back(root);

        let Err(Error::Kept(kept)) = result else {
            panic!("{result:?}");
        };
        assert_eq!(
            read(root, "edited.txt").as_deref(),
            Some("typed after the crash\n")
        );
        assert_eq!(read(&kept, "edited.txt").as_deref(), Some("later\n"));
        assert_eq!(read(root, "made/later.txt").as_deref(), Some("later\n"));
        assert!(!root.join("kept.txt").exists());
        assert!(!unfinished_in(root));
    }

    #[test]
    fn a_replaced_file_placed_as_the_crash_came_gets_its_original_back() {
        let dir = edited_since_turn_one();
        let root = dir.path();
        let before = tree_state(root);
        crash_mid_restore(root, &session(1), 1);
        let (folder, steps) = left_by_crash(root);
        let Some(Step::Placed(last, _)) = steps.last() else {
            panic!("{steps:?}");
        };
        assert_eq!(last, Path::new("z-last.txt"));
        // The crash came as the last file was placed, before its record was whole.
        let journal = folder.join(JOURNAL);
        let length = fs::metadata(&journal).unwrap().len();
        fs::File::options()
            .write(true)
            .open(&journal)
            .unwrap()
            .set_len(length - 3)
            .unwrap();

        put_back(root).unwrap();

        assert_eq!(tree_state(root), before);
        no_work_left(root);
    }

    #[test]
    fn a_file_moved_aside_before_any_record_is_put_back() {
        let dir = repository();
        let root = dir.path();
        let before = tree_state(root);
        let mut work = Work::new(Repo::new(root)).unwrap();
        move_file(&root.join("edited.txt"), &work.aside.join("edited.txt")).unwrap();
        // Killed then: nothing recorded, the file aside.
        work.keep = true;
        drop(work);

        assert!(unfinished_in(root));
        put_back(root).unwrap();

        assert_eq!(tree_state(root), before);
        no_work_left(root);
    }

    #[test]
    fn putting_back_again_after_a_crash_cut_an_undo_short_finishes_it() {
        let dir = edited_since_turn_one();
        let root = dir.path();
        let before = tree_state(root);
        crash_mid_restore(root, &session(1), 1);
        let (folder, _) = left_by_crash(root);
        // The undo got as far as the newest file: placed copy gone, original back.
        fs::remove_file(root.join("z-last.txt")).unwrap();
        move_file(&folder.join("aside/z-last.txt"), &root.join("z-last.txt")).unwrap();

        put_back(root).unwrap();

        assert_eq!(tree_state(root), before);
        no_work_left(root);
    }

    #[test]
    fn a_kept_restore_is_not_offered_and_its_folder_goes() {
        let dir = edited_since_turn_one();
        let root = dir.path();
        let mut work = Work::new(Repo::new(root)).unwrap();
        move_file(&root.join("edited.txt"), &work.aside.join("edited.txt")).unwrap();
        work.write(&[KEPT.to_vec()]).unwrap();
        work.keep = true;
        drop(work);

        assert!(!unfinished_in(root));
        put_back(root).unwrap();
        assert!(restore_folders(Repo::new(root)).unwrap().is_empty());
    }

    #[test]
    fn a_reader_holding_a_journal_a_moment_neither_hides_it_nor_lets_a_restore_past() {
        let dir = edited_since_turn_one();
        let root = dir.path();
        let before = tree_state(root);
        crash_mid_restore(root, &session(1), 1);
        let (folder, _) = left_by_crash(root);
        let reading = || {
            let reader = fs::File::open(folder.join(JOURNAL)).unwrap();
            reader.lock().unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                drop(reader);
            })
        };

        let reader = reading();
        let again = restore(root, &session(1), 1, &Cancel::default(), || true);
        reader.join().unwrap();
        assert!(matches!(again, Err(Error::Unfinished)), "{again:?}");

        let reader = reading();
        put_back(root).unwrap();
        reader.join().unwrap();
        assert_eq!(tree_state(root), before);
        no_work_left(root);
    }

    #[test]
    fn a_cancel_leaves_a_folder_it_did_not_remove_as_it_is() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repository();
        let root = dir.path();
        write(root, "keep/stay.txt", "stays\n");
        save_turn(root, 1, 1, None, KEEP);
        write(root, "keep/later.txt", "later\n");

        let result = restore(root, &session(1), 1, &Cancel::default(), || {
            fs::set_permissions(root.join("keep"), fs::Permissions::from_mode(0o700)).unwrap();
            false
        });

        assert!(!matches!(result, Err(Error::Kept(_))), "{result:?}");
        let mode = fs::metadata(root.join("keep"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o700);
        assert_eq!(read(root, "keep/later.txt").as_deref(), Some("later\n"));
    }

    #[test]
    fn a_journal_cut_short_keeps_its_whole_records() {
        let stamp = Stamp {
            inode: 7,
            len: 3,
            modified: (-1, 999_999_999),
            changed: (1_700_000_000, 5),
        };
        let steps = vec![
            Step::Removing(PathBuf::from("dir"), 0o700),
            Step::Removed(PathBuf::from("dir")),
            Step::Made(PathBuf::from("a\nb")),
            Step::Placing(PathBuf::from("a\nb/c.txt"), stamp, "ab".repeat(32)),
            Step::Placed(PathBuf::from("a\nb/c.txt"), stamp),
        ];
        let mut journal = Vec::new();
        for step in &steps {
            for field in step.fields() {
                journal.extend(field);
                journal.push(0);
            }
        }
        assert_eq!(parse(&journal).unwrap(), steps);
        for cut in 1..20 {
            let parsed = parse(&journal[..journal.len() - cut]).unwrap();
            assert_eq!(parsed[..], steps[..4], "cut {cut}");
        }
    }

    #[test]
    fn files_written_while_a_filter_holds_the_restore_are_left_and_said() {
        let dir = repository();
        let root = dir.path();
        write(root, "slow", "at the checkpoint\n");
        write(root, "z.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_file(root.join("slow")).unwrap();
        write(root, "slow/inside.txt", "later\n");
        write(root, "z.txt", "later\n");
        // The path the delete frees is converted only as it is placed.
        let (started, go) = (root.join(".git/started"), root.join(".git/go"));
        sh(root, &["config", "filter.wait.clean", "cat"]);
        sh(
            root,
            &[
                "config",
                "filter.wait.smudge",
                &format!(
                    "touch '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; cat",
                    started.display(),
                    go.display()
                ),
            ],
        );
        write(root, ".git/info/attributes", "slow filter=wait\n");
        let editor = {
            let root = root.to_owned();
            std::thread::spawn(move || {
                while !root.join(".git/started").exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                write(&root, "slow", "made while the filter ran\n");
                let file = root.join("z.txt");
                let modified = fs::metadata(&file).unwrap().modified().unwrap();
                fs::write(&file, "LATER\n").unwrap();
                fs::File::options()
                    .write(true)
                    .open(&file)
                    .unwrap()
                    .set_modified(modified)
                    .unwrap();
                fs::write(root.join(".git/go"), "").unwrap();
            })
        };

        let restored = restore(root, &session(1), 1, &Cancel::default(), || true).unwrap();

        editor.join().unwrap();
        assert_eq!(
            restored.skipped,
            [PathBuf::from("slow"), PathBuf::from("z.txt")]
        );
        assert_eq!(
            read(root, "slow").as_deref(),
            Some("made while the filter ran\n")
        );
        assert_eq!(read(root, "z.txt").as_deref(), Some("LATER\n"));
    }

    #[test]
    fn a_file_rewritten_with_its_old_times_during_the_restore_is_left_and_said() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");
        let (started, go) = (root.join(".git/started"), root.join(".git/go"));
        sh(
            root,
            &[
                "config",
                "filter.wait.smudge",
                &format!(
                    "touch '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; cat",
                    started.display(),
                    go.display()
                ),
            ],
        );
        write(root, ".git/info/attributes", "*.txt filter=wait\n");
        let editor = {
            let root = root.to_owned();
            std::thread::spawn(move || {
                while !root.join(".git/started").exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                // As cp -p or rsync -t would: same size, same modified time.
                let file = root.join("edited.txt");
                let modified = fs::metadata(&file).unwrap().modified().unwrap();
                fs::write(&file, "LATER\n").unwrap();
                fs::File::options()
                    .write(true)
                    .open(&file)
                    .unwrap()
                    .set_modified(modified)
                    .unwrap();
                fs::write(root.join(".git/go"), "").unwrap();
            })
        };

        let restored = restore(root, &session(1), 1, &Cancel::default(), || true).unwrap();

        editor.join().unwrap();
        assert_eq!(restored.skipped, [PathBuf::from("edited.txt")]);
        assert_eq!(read(root, "edited.txt").as_deref(), Some("LATER\n"));
    }

    #[test]
    fn a_file_rewritten_in_place_with_its_old_time_while_the_fork_opens_is_kept() {
        let dir = repository();
        let root = dir.path();
        save_turn(root, 1, 1, None, KEEP);
        write(root, "edited.txt", "later\n");

        let result = restore(root, &session(1), 1, &Cancel::default(), || {
            let file = root.join("edited.txt");
            let modified = fs::metadata(&file).unwrap().modified().unwrap();
            fs::write(&file, "FIRST\n").unwrap();
            fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(modified)
                .unwrap();
            false
        });

        let Err(Error::NotPutBack { kept, .. }) = result else {
            panic!("{result:?}");
        };
        assert_eq!(read(root, "edited.txt").as_deref(), Some("FIRST\n"));
        assert_eq!(read(&kept, "edited.txt").as_deref(), Some("later\n"));
    }

    /// A repository in a case-insensitive folder, as macOS has by default (on Linux,
    /// scripts/case-insensitive-tests.sh mounts one), whose README.md and Docs folder
    /// are renamed by case only after turn one: with `git mv`, or only on disk, which
    /// git, ignoring case, does not see as a rename.
    fn case_insensitive_repository(git_mv: bool) -> tempfile::TempDir {
        let folder = std::env::var_os("KEYSTONE_CASE_INSENSITIVE_DIR")
            .expect("KEYSTONE_CASE_INSENSITIVE_DIR names a case-insensitive folder");
        let dir = tempfile::tempdir_in(folder).unwrap();
        let root = dir.path();
        write(root, "Probe", "");
        assert!(root.join("probe").exists(), "{root:?} is case-sensitive");
        fs::remove_file(root.join("Probe")).unwrap();
        init(root);
        assert_eq!(sh(root, &["config", "core.ignorecase"]).trim(), "true");
        write(root, ".gitignore", "copy.md\n");
        write(root, "README.md", "at the checkpoint\n");
        write(root, "Docs/guide.md", "at the checkpoint\n");
        sh(root, &["add", "--all"]);
        sh(root, &["commit", "-q", "-m", "start"]);
        save_turn(root, 1, 1, None, KEEP);
        // Through another name: a casefolded Linux folder keeps a name renamed by case
        // only, and git mv would move a folder into itself.
        for (from, to) in [("README.md", "Readme.md"), ("Docs", "docs")] {
            for (from, to) in [(from, "moving"), ("moving", to)] {
                if git_mv {
                    sh(root, &["mv", from, to]);
                } else {
                    fs::rename(root.join(from), root.join(to)).unwrap();
                }
            }
        }
        write(root, "Readme.md", "renamed later\n");
        dir
    }

    /// The names a folder lists, in the case they are stored.
    fn names(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != ".git")
            .collect();
        names.sort();
        names
    }

    #[test]
    #[ignore = "needs a case-insensitive folder: scripts/case-insensitive-tests.sh"]
    fn case_insensitive_a_case_only_rename_is_restored() {
        for git_mv in [false, true] {
            let dir = case_insensitive_repository(git_mv);
            let root = dir.path();

            let restored = restore_kept(root, &session(1), 1).unwrap();

            assert!(restored.skipped.is_empty(), "{:?}", restored.skipped);
            assert_eq!(
                read(root, "README.md").as_deref(),
                Some("at the checkpoint\n")
            );
            let expected: &[&str] = match git_mv {
                true => &[".gitignore", "Docs", "README.md"],
                // Git's tree keeps the folder's old name, and so does the restore.
                false => &[".gitignore", "README.md", "docs"],
            };
            assert_eq!(names(root), expected, "git mv: {git_mv}");
        }
    }

    #[test]
    #[ignore = "needs a case-insensitive folder: scripts/case-insensitive-tests.sh"]
    fn case_insensitive_a_case_only_rename_a_crash_cut_short_is_put_back() {
        for git_mv in [false, true] {
            let dir = case_insensitive_repository(git_mv);
            let root = dir.path();
            let before = tree_state(root);

            crash_mid_restore(root, &session(1), 1);
            assert_ne!(tree_state(root), before);
            put_back(root).unwrap();

            assert_eq!(
                names(root),
                [".gitignore", "Readme.md", "docs"],
                "git mv: {git_mv}"
            );
            assert_eq!(tree_state(root), before, "git mv: {git_mv}");
            no_work_left(root);
        }
    }

    #[test]
    #[ignore = "needs a case-insensitive folder: scripts/case-insensitive-tests.sh"]
    fn case_insensitive_a_hard_link_by_another_name_is_left_alone() {
        let dir = case_insensitive_repository(false);
        let root = dir.path();
        // Listed before the stored name, and after it, as folders may order them.
        for link in ["a-copy.md", "copy.md"] {
            fs::write(root.join(".gitignore"), format!("{link}\n")).unwrap();
            fs::hard_link(root.join("Readme.md"), root.join(link)).unwrap();

            restore_kept(root, &session(1), 1).unwrap();

            assert_eq!(
                read(root, link).as_deref(),
                Some("renamed later\n"),
                "{link}"
            );
            assert_eq!(
                read(root, "README.md").as_deref(),
                Some("at the checkpoint\n")
            );
            fs::remove_file(root.join(link)).unwrap();
            write(root, "Readme.md", "renamed later\n");
            fs::rename(root.join("README.md"), root.join("moving")).unwrap();
            fs::rename(root.join("moving"), root.join("Readme.md")).unwrap();
            fs::write(root.join(".gitignore"), "copy.md\n").unwrap();
        }
    }

    #[test]
    fn a_new_file_placed_as_the_crash_came_is_taken_away() {
        let dir = repository();
        let root = dir.path();
        write(root, "new.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_file(root.join("new.txt")).unwrap();
        let before = tree_state(root);
        crash_mid_restore(root, &session(1), 1);
        let (folder, steps) = left_by_crash(root);
        let [Step::Placing(..), Step::Placed(..)] = &steps[..] else {
            panic!("{steps:?}");
        };
        // The crash came as the file was placed, before its record was whole.
        let journal = folder.join(JOURNAL);
        let length = fs::metadata(&journal).unwrap().len();
        fs::File::options()
            .write(true)
            .open(&journal)
            .unwrap()
            .set_len(length - 3)
            .unwrap();

        put_back(root).unwrap();

        assert_eq!(tree_state(root), before);
        no_work_left(root);
    }

    #[test]
    fn a_file_placed_as_the_crash_came_and_written_since_keeping_its_times_stays() {
        let dir = repository();
        let root = dir.path();
        write(root, "new.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_file(root.join("new.txt")).unwrap();
        crash_mid_restore(root, &session(1), 1);
        let (folder, _) = left_by_crash(root);
        let journal = folder.join(JOURNAL);
        let length = fs::metadata(&journal).unwrap().len();
        fs::File::options()
            .write(true)
            .open(&journal)
            .unwrap()
            .set_len(length - 3)
            .unwrap();
        let file = root.join("new.txt");
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "AT THE CHECKPOINT\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();

        put_back(root).unwrap();

        assert_eq!(
            read(root, "new.txt").as_deref(),
            Some("AT THE CHECKPOINT\n")
        );
    }

    #[test]
    fn a_file_written_since_the_crash_in_a_folder_the_restore_made_stays_with_it() {
        let dir = repository();
        let root = dir.path();
        write(root, "made/inside.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_dir_all(root.join("made")).unwrap();
        crash_mid_restore(root, &session(1), 1);
        write(root, "made/mine.txt", "written since\n");

        put_back(root).unwrap();

        assert_eq!(
            read(root, "made/mine.txt").as_deref(),
            Some("written since\n")
        );
        assert_eq!(read(root, "made/inside.txt"), None);
        no_work_left(root);
    }

    #[test]
    fn putting_back_again_where_an_undo_put_a_file_back_in_place_of_a_folder_is_clean() {
        let dir = repository();
        let root = dir.path();
        write(root, "x/inside.txt", "at the checkpoint\n");
        save_turn(root, 1, 1, None, KEEP);
        fs::remove_dir_all(root.join("x")).unwrap();
        write(root, "x", "original file\n");
        let before = tree_state(root);
        crash_mid_restore(root, &session(1), 1);
        let (folder, _) = left_by_crash(root);
        // As an undo a crash cut short left it: every file back, the journal still there.
        fs::remove_file(root.join("x/inside.txt")).unwrap();
        fs::remove_dir(root.join("x")).unwrap();
        fs::rename(folder.join("aside/x"), root.join("x")).unwrap();

        put_back(root).unwrap();

        assert_eq!(tree_state(root), before);
        no_work_left(root);
    }

    #[test]
    #[ignore = "needs a case-insensitive folder: scripts/case-insensitive-tests.sh"]
    fn case_insensitive_a_name_folded_beyond_lowercase_keeps_its_spelling() {
        let folder = std::env::var_os("KEYSTONE_CASE_INSENSITIVE_DIR").unwrap();
        let dir = tempfile::tempdir_in(folder).unwrap();
        let root = dir.path();
        init(root);
        write(root, "ς.txt", "original\n");
        assert!(root.join("Σ.txt").exists());
        let mut journal = Journal {
            root,
            unchanged: BTreeMap::new(),
            case_insensitive: true,
            listings: HashMap::new(),
            work: Work::new(Repo::new(root)).unwrap(),
            steps: Vec::new(),
        };

        journal.set_aside(Path::new("Σ.txt")).unwrap();
        journal.undo().unwrap();

        assert_eq!(names(root), ["ς.txt"]);
    }
}
