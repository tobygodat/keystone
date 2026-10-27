//! Checkpoints of a session's repository. As each turn ends, the working tree, tracked
//! and untracked files without ignored ones, is saved as a commit that only
//! `refs/keystone/checkpoints/<store>/<session>/<turn>` names, and a restore puts the
//! working tree back the way one left it. `<store>` tells apart the Keystone workspaces
//! that use one repository, since session numbers repeat between them. Every tree is
//! built in an index file of its own inside the repository's git folder, so the user's
//! index, HEAD and stash are never touched. Every call blocks.

use crate::git::{self, Output, git, path, run};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Checkpoints kept per session. Older ones are pruned as each new one is saved.
pub const KEEP: usize = 50;
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

#[derive(Debug)]
pub enum Error {
    Git(git::Error),
    Io(io::Error),
    /// The checkpoint's ref is gone, pruned or deleted.
    Gone,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
            Self::Gone => f.write_str("This checkpoint is no longer saved."),
        }
    }
}

impl std::error::Error for Error {}

impl From<git::Error> for Error {
    fn from(error: git::Error) -> Self {
        Self::Git(error)
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

/// Where `name` in the git folder of the repository or worktree at `root` is.
fn git_path(root: &Path, name: &str) -> Result<PathBuf> {
    let mut command = git(root);
    command.args(["rev-parse", "--git-path", name]);
    let output = run(command, None, Output::Separate)?;
    Ok(root.join(path(output.strip_suffix(b"\n").unwrap_or(&output))))
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

/// An index file in the repository's git folder for one chain of commands, removed
/// when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(root: &Path) -> Result<Self> {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "keystone-checkpoint-{}-{}.index",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        );
        let file = git_path(root, &name)?;
        let _ = fs::remove_file(&file);
        Ok(Self(file))
    }

    /// A copy of the index of the repository or worktree at `root`, so `git add` reads
    /// again only the files whose stat changed. Files the user marked assume-unchanged,
    /// or skip-worktree while they are on disk, are read again too. The copy keeps the
    /// index's modified time, so git still reads again the files changed in the same
    /// second the index was written.
    fn copy_of(root: &Path) -> Result<Self> {
        let scratch = Self::new(root)?;
        let index = git_path(root, "index")?;
        match fs::copy(&index, &scratch.0) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(scratch),
            Err(error) => return Err(error.into()),
        }
        fs::File::options()
            .write(true)
            .open(&scratch.0)?
            .set_modified(fs::metadata(&index)?.modified()?)?;
        let mut listed = scratch.git(root);
        listed.args(["ls-files", "-v", "-z"]);
        let listed = run(listed, None, Output::Separate)?;
        let (mut assumed, mut skipped) = (Vec::new(), Vec::new());
        for entry in nul_separated(&listed) {
            let (Some(&tag), Some(file)) = (entry.first(), entry.get(2..)) else {
                continue;
            };
            if tag.is_ascii_lowercase() {
                assumed.push(file);
            }
            if tag.to_ascii_uppercase() == b'S' && root.join(path(file)).symlink_metadata().is_ok()
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
            let mut clear = scratch.git(root);
            clear.args(["update-index", flag, "-z", "--stdin"]);
            run(clear, Some(nul_joined(files)), Output::Separate)?;
        }
        Ok(scratch)
    }

    fn git(&self, root: &Path) -> Command {
        let mut command = git(root);
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
fn snapshot_pathspecs(root: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut others = git(root);
    others.args(["ls-files", "--others", "--exclude-standard", "-z"]);
    let others = run(others, None, Output::Separate)?;
    let mut specs = b".\0".to_vec();
    let mut left_out = Vec::new();
    for file in nul_separated(&others) {
        let nested = file.ends_with(b"/");
        let large = !nested
            && root
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
    let scratch = Scratch::copy_of(root)?;
    let (specs, left_out) = snapshot_pathspecs(root)?;
    let mut add = scratch.git(root);
    add.args([
        "add",
        "--all",
        "--pathspec-from-file=-",
        "--pathspec-file-nul",
    ]);
    run(add, Some(specs), Output::Separate)?;
    let mut write = scratch.git(root);
    write.arg("write-tree");
    Ok(Snapshot {
        tree: text(&run(write, None, Output::Separate)?),
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
fn save(root: &Path, name: &str, tree: &str, message: String, create: bool) -> Result<()> {
    let commit = commit(root, tree, None, message)?;
    point(root, name, &commit, create)
}

/// Commits `tree` with `message`, on `parent` when there is one.
fn commit(root: &Path, tree: &str, parent: Option<&str>, message: String) -> Result<String> {
    let mut commit = git(root);
    commit.args(["commit-tree", "--no-gpg-sign", tree, "-F", "-"]);
    if let Some(parent) = parent {
        commit.args(["-p", parent]);
    }
    commit
        .env("GIT_AUTHOR_NAME", "Keystone")
        .env("GIT_AUTHOR_EMAIL", "keystone@localhost")
        .env("GIT_COMMITTER_NAME", "Keystone")
        .env("GIT_COMMITTER_EMAIL", "keystone@localhost");
    Ok(text(&run(
        commit,
        Some(message.into_bytes()),
        Output::Separate,
    )?))
}

/// Points `name` at `commit`. With `create`, an existing ref is left as it is and this
/// fails.
fn point(root: &Path, name: &str, commit: &str, create: bool) -> Result<()> {
    let mut update = git(root);
    update.args(["update-ref", name, &commit]);
    if create {
        update.arg("");
    }
    run(update, None, Output::Separate)?;
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
    Ok(Checkpoints {
        root: root.to_owned(),
        turns,
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
    let mut update = git(root);
    update.args(["update-ref", "--stdin"]);
    run(update, Some(doomed.into_bytes()), Output::Separate)?;
    Ok(())
}

/// Saves `tree`, the working tree as `turn` left it, as the session's checkpoint for
/// that turn, with the count of files changed since `start` when the turn's start was
/// taken, then keeps only the newest `keep`. An existing checkpoint of that turn is
/// left as it is and the save fails.
pub fn record(
    root: &Path,
    session: &Session,
    turn: u64,
    snapshot: &Snapshot,
    start: Option<&str>,
    keep: usize,
) -> Result<Checkpoints> {
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
        parent = Some(commit(root, &list, None, LEFT_OUT.to_owned())?);
        message.push_str(&format!("{LEFT_OUT}{blob}\n"));
    }
    let checkpoint = commit(root, tree, parent.as_deref(), message)?;
    point(root, &session.name(turn), &checkpoint, true)?;
    prune(root, session, keep)?;
    list(root, session)
}

/// Puts the working tree at `root` back the way the session's checkpoint for `turn`
/// saved it: files changed since are written back, and files created since are
/// deleted, with folders they leave empty. The working tree as it was just before is
/// saved first as `<session>/before-restore`.
///
/// Ignored files are never read or written, by the rules now or by the checkpoint's
/// own .gitignore files, which are put back first. Nested repositories are left alone,
/// and so is a file of the checkpoint where a folder, a nested repository, or a file
/// the snapshot left out now stands; those are returned.
pub fn restore(root: &Path, session: &Session, turn: u64) -> Result<Restored> {
    let mut verify = git(root);
    verify.args([
        "rev-parse",
        "--quiet",
        "--verify",
        &format!("{}^{{tree}}", session.name(turn)),
    ]);
    let target = match run(verify, None, Output::Separate) {
        Ok(target) => text(&target),
        Err(git::Error::Failed { .. }) => return Err(Error::Gone),
        Err(error) => return Err(error.into()),
    };
    let left_out = left_out(root, &session.name(turn))?;
    let now = snapshot(root)?.tree;
    save(
        root,
        &session.name(BEFORE_RESTORE),
        &now,
        format!("Keystone: before restoring turn {turn}\n"),
        false,
    )?;
    let mut diff = git(root);
    diff.args(["diff-tree", "-r", "-z", "--no-renames", &now, &target]);
    let output = run(diff, None, Output::Separate)?;
    // Each change is `:<mode> <mode> <oid> <oid> <status>` and its path.
    let mut deletes = Vec::new();
    let mut writes = Vec::new();
    let mut skipped = Vec::new();
    let mut fields = output.split(|byte| *byte == 0);
    while let (Some(meta), Some(file)) = (fields.next(), fields.next()) {
        let meta = text(meta);
        let meta: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old, new, _, _, status] = meta[..] else {
            continue;
        };
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
    let ignore_rules = |file: &[u8]| path(file).file_name() == Some(".gitignore".as_ref());
    // The checkpoint's ignore rules decide what else may be deleted.
    let target_index = Scratch::new(root)?;
    let mut read = target_index.git(root);
    read.args(["read-tree", &target]);
    run(read, None, Output::Separate)?;
    let rules: Vec<&[u8]> = deletes
        .iter()
        .copied()
        .filter(|f| ignore_rules(f))
        .collect();
    for file in &rules {
        remove(root, &root.join(path(file)))?;
    }
    let (rule_writes, writes): (Vec<_>, Vec<_>) =
        writes.into_iter().partition(|(_, file)| ignore_rules(file));
    checkout(root, &target_index, rule_writes, &mut skipped)?;
    // What the checkpoint left out was there, so it is not deleted as if made later.
    let (kept, candidates): (Vec<&[u8]>, Vec<&[u8]>) = deletes
        .into_iter()
        .filter(|f| !ignore_rules(f))
        .partition(|file| {
            nul_separated(&left_out)
                .any(|out| *file == out || (out.ends_with(b"/") && file.starts_with(out)))
        });
    skipped.extend(kept.into_iter().map(path));
    let ignored = ignored_by_rules(root, &candidates)?;
    for file in candidates.into_iter().filter(|f| !ignored.contains(*f)) {
        remove(root, &root.join(path(file)))?;
    }
    checkout(root, &target_index, writes, &mut skipped)?;
    Ok(Restored { skipped })
}

/// The untracked paths the checkpoint `name` left out, NUL-separated.
fn left_out(root: &Path, name: &str) -> Result<Vec<u8>> {
    let mut message = git(root);
    message.args(["cat-file", "commit", name]);
    let message = run(message, None, Output::Separate)?;
    let Some(blob) = String::from_utf8_lossy(&message)
        .lines()
        .find_map(|line| line.strip_prefix(LEFT_OUT).map(str::to_owned))
    else {
        return Ok(Vec::new());
    };
    let mut read = git(root);
    read.args(["cat-file", "blob", &blob]);
    Ok(run(read, None, Output::Separate)?)
}

/// Which of `files` the ignore rules now on disk leave out, tracked or not.
fn ignored_by_rules(root: &Path, files: &[&[u8]]) -> Result<BTreeSet<Vec<u8>>> {
    if files.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut check = git(root);
    check.args(["check-ignore", "--no-index", "-z", "--stdin"]);
    match run(
        check,
        Some(nul_joined(files.iter().copied())),
        Output::Separate,
    ) {
        Ok(output) => Ok(nul_separated(&output).map(<[u8]>::to_vec).collect()),
        // None of them is ignored.
        Err(git::Error::Failed { code: Some(1), .. }) => Ok(BTreeSet::new()),
        Err(error) => Err(error.into()),
    }
}

/// Writes `writes` from `index` into the working tree at `root`, each an added flag and
/// a path. Nothing is written, and the path goes to `skipped`, where a file the snapshot
/// left out (ignored or too large) stands, where a folder stands, or where one of its
/// folders is a file or a nested repository.
fn checkout(
    root: &Path,
    index: &Scratch,
    writes: Vec<(bool, &[u8])>,
    skipped: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut listed = git(root);
    listed.args(["ls-files", "-v", "-z"]);
    let listed = run(listed, None, Output::Separate)?;
    // Outside a sparse checkout's cone: the user keeps these off disk.
    let sparse: BTreeSet<&[u8]> = nul_separated(&listed)
        .filter(|entry| {
            entry
                .first()
                .is_some_and(|tag| tag.to_ascii_uppercase() == b'S')
        })
        .filter_map(|entry| entry.get(2..))
        .collect();
    let mut paths = Vec::new();
    for (added, file) in writes {
        let full = root.join(path(file));
        let here = full.symlink_metadata();
        if sparse.contains(file) && here.is_err() {
            continue;
        }
        // A file the snapshot left out is ignored or too large, so it stays.
        let blocked = here.as_ref().is_ok_and(|meta| meta.is_dir() || added)
            || full
                .ancestors()
                .skip(1)
                .take_while(|folder| *folder != root)
                .any(|folder| {
                    folder.symlink_metadata().is_ok_and(|meta| !meta.is_dir())
                        || folder.join(".git").symlink_metadata().is_ok()
                });
        if blocked {
            skipped.push(path(file));
        } else {
            paths.push(file);
        }
    }
    if paths.is_empty() {
        return Ok(());
    }
    let mut checkout = index.git(root);
    checkout.args(["checkout-index", "--force", "-z", "--stdin"]);
    run(checkout, Some(nul_joined(paths)), Output::Separate)?;
    Ok(())
}

/// Deletes `file`, then each folder above it up to `root` that it leaves empty.
fn remove(root: &Path, file: &Path) -> io::Result<()> {
    match fs::remove_file(file) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut folder = file.parent();
    while let Some(dir) = folder.filter(|dir| *dir != root && dir.starts_with(root)) {
        if fs::remove_dir(dir).is_err() {
            break;
        }
        folder = dir.parent();
    }
    Ok(())
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

        assert_eq!(restore(root, &session(3), 1).unwrap(), Restored::default());

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

        restore(root, &session(1), 1).unwrap();

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

        restore(root, &session(1), 1).unwrap();

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

        let restored = restore(root, &session(1), 1).unwrap();

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

        restore(root, &session(1), 1).unwrap();

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

        let restored = restore(root, &session(1), 1).unwrap();

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

        let restored = restore(root, &session(1), 2).unwrap();
        assert!(restored.skipped.is_empty());
        let restored = restore(root, &session(1), 1).unwrap();

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

        let restored = restore(root, &session(1), 2).unwrap();

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

        let restored = restore(root, &session(1), 1).unwrap();

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

        let restored = restore(root, &session(1), 1).unwrap();

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

        restore(root, &session(1), 2).unwrap();

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

        let restored = restore(root, &session(1), 1).unwrap();

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

        restore(root, &session(1), 1).unwrap();

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
        let restored = restore(root, &session(1), 3).unwrap();
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

        restore(root, &session(1), 1).unwrap();

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
        assert!(matches!(restore(root, &session(5), 1), Err(Error::Gone)));
        restore(root, &session(5), 3).unwrap();
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

        restore(root, &session(1), 1).unwrap();

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
        restore(&saved.root, &session(9), 1).unwrap();
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
        restore(root, &session(1), 1).unwrap();
        assert_eq!(read(root, "first.txt").as_deref(), Some("first\n"));
        assert_eq!(read(root, "second.txt"), None);
        assert!(
            !root.join(".git/index").exists(),
            "the user's index is never written"
        );
    }
}
