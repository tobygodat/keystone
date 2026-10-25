use crate::{WorkingDirectory, WorktreeContext};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn git(repository: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    let output = command
        .current_dir(repository)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn text(repository: &Path, args: &[&str]) -> io::Result<String> {
    String::from_utf8(git(repository, args)?)
        .map(|text| text.strip_suffix('\n').unwrap_or(&text).to_owned())
        .map_err(|_| invalid("git returned a non-UTF-8 path or reference"))
}

fn path_text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| invalid("worktree paths must be UTF-8"))
}

fn common(repository: &Path) -> io::Result<PathBuf> {
    PathBuf::from(text(
        repository,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)
    .canonicalize()
}

fn registered(repository: &Path) -> io::Result<Vec<PathBuf>> {
    let output = git(repository, &["worktree", "list", "--porcelain", "-z"])?;
    output
        .split(|byte| *byte == 0)
        .filter_map(|field| field.strip_prefix(b"worktree "))
        .map(|path| {
            String::from_utf8(path.to_vec())
                .map(PathBuf::from)
                .map_err(|_| invalid("registered worktree path is not UTF-8"))
        })
        .collect()
}

fn main_repository(repository: &Path) -> io::Result<WorkingDirectory> {
    let paths = registered(repository)?;
    let main = paths
        .first()
        .ok_or_else(|| invalid("repository has no main worktree"))?;
    let main = WorkingDirectory::new(main)?;
    if common(main.path())? != common(repository)?
        || !main.path().join(".git").is_dir()
        || main.path().join(".git").canonicalize()? != common(repository)?
    {
        return Err(invalid("repository has no ordinary main worktree"));
    }
    Ok(main)
}

/// Creates a linked worktree below the main repository's private Keystone directory.
/// The default base is the given project's HEAD, including when it is a linked worktree.
pub fn create(project: &Path, slug: &str, base: Option<&str>) -> io::Result<WorktreeContext> {
    if slug.is_empty()
        || slug.len() > 100
        || !slug.as_bytes()[0].is_ascii_alphanumeric()
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid(
            "Use letters, digits, - or _, starting with a letter or digit.",
        ));
    }
    let base = base.unwrap_or("HEAD");
    if base.is_empty() || base.starts_with('-') || base.chars().any(char::is_control) {
        return Err(invalid("invalid worktree base reference"));
    }
    let project = WorkingDirectory::new(project)?;
    let repository = main_repository(project.path())?;
    let commit = text(
        project.path(),
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{base}^{{commit}}"),
        ],
    )?;
    let branch = format!("keystone/{slug}");
    if !git(
        repository.path(),
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/heads/{branch}"),
        ],
    )?
    .is_empty()
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "That branch already exists.",
        ));
    }
    let mut parent = repository.path().to_owned();
    for component in [".keystone", "worktrees"] {
        parent.push(component);
        match fs::create_dir(&parent) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&parent)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(invalid("worktree parent must be an ordinary directory"));
                }
            }
            Err(error) => return Err(error),
        }
    }
    let path = parent.join(slug);
    if fs::symlink_metadata(&path).is_ok()
        || registered(repository.path())?
            .iter()
            .any(|entry| entry == &path)
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "That name is already in use.",
        ));
    }
    let exclude = common(repository.path())?.join("info/exclude");
    fs::create_dir_all(exclude.parent().expect("info directory"))?;
    let previous = match fs::read(&exclude) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    if !previous
        .split(|byte| *byte == b'\n')
        .any(|line| line == b"/.keystone/worktrees/")
    {
        let mut file = OpenOptions::new().create(true).append(true).open(exclude)?;
        if !previous.is_empty() && !previous.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        file.write_all(b"/.keystone/worktrees/\n")?;
    }
    git(
        repository.path(),
        &["worktree", "add", "-b", &branch, path_text(&path)?, &commit],
    )?;
    inspect(repository.path(), &path)
}

/// Resolves a registered linked worktree root in the same common Git repository.
pub fn inspect(repository: &Path, path: &Path) -> io::Result<WorktreeContext> {
    let repository = main_repository(repository)?;
    let directory = WorkingDirectory::new(path)?;
    if directory == repository {
        return Err(invalid("the main repository is not a linked worktree"));
    }
    let root = WorkingDirectory::new(Path::new(&text(
        directory.path(),
        &["rev-parse", "--show-toplevel"],
    )?))?;
    if root != directory || common(directory.path())? != common(repository.path())? {
        return Err(invalid(
            "path is not a worktree root in the same repository",
        ));
    }
    if !registered(repository.path())?
        .iter()
        .any(|entry| entry.canonicalize().ok().as_deref() == Some(directory.path()))
    {
        return Err(invalid("path is not a registered worktree"));
    }
    let admin = PathBuf::from(text(
        directory.path(),
        &["rev-parse", "--absolute-git-dir"],
    )?)
    .canonicalize()?;
    if admin.parent() != Some(common(repository.path())?.join("worktrees").as_path())
        || !fs::symlink_metadata(directory.path().join(".git"))?.is_file()
    {
        return Err(invalid("path does not have a linked worktree identity"));
    }
    let backlink = fs::read_to_string(admin.join("gitdir"))?;
    if Path::new(backlink.strip_suffix('\n').unwrap_or(&backlink)).canonicalize()?
        != directory.path().join(".git")
    {
        return Err(invalid(
            "worktree registration belongs to another directory",
        ));
    }
    let reference = text(directory.path(), &["symbolic-ref", "--quiet", "HEAD"])?;
    let branch = reference
        .strip_prefix("refs/heads/")
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| invalid("worktree is not on a local branch"))?
        .to_owned();
    Ok(WorktreeContext {
        directory,
        repository,
        branch,
    })
}

/// Whether `context`'s folder is still that linked worktree, judged from files alone so
/// it is cheap enough to ask before every send: a real directory at the recorded path,
/// not a symlink, whose `.git` file points into the recorded repository's worktrees at
/// an entry that points back at it.
pub fn is_intact(context: &WorktreeContext) -> bool {
    let directory = context.directory.path();
    if directory.canonicalize().ok().as_deref() != Some(directory)
        || !fs::symlink_metadata(directory.join(".git")).is_ok_and(|m| m.is_file())
    {
        return false;
    }
    let Ok(link) = fs::read_to_string(directory.join(".git")) else {
        return false;
    };
    let Some(admin) = link.trim_end().strip_prefix("gitdir: ") else {
        return false;
    };
    let (Ok(admin), Ok(common)) = (
        directory.join(admin).canonicalize(),
        context.repository.path().join(".git").canonicalize(),
    ) else {
        return false;
    };
    let backlink = fs::read_to_string(admin.join("gitdir")).ok();
    admin.parent() == Some(common.join("worktrees").as_path())
        && backlink
            .and_then(|path| Path::new(path.trim_end()).canonicalize().ok())
            .is_some_and(|path| path == directory.join(".git"))
}

/// Removes a clean worktree without deleting its branch. Every HEAD commit must be
/// reachable from a local remote-tracking ref or another local branch, so removing the
/// folder cannot lose a commit; no remote is contacted to check this. The cleanliness
/// scan runs last, right before `git worktree remove`.
pub fn remove(context: &WorktreeContext) -> io::Result<()> {
    let current = inspect(context.repository.path(), context.directory.path())?;
    if current.directory != context.directory || current.repository != context.repository {
        return Err(invalid("The folder is no longer this session's worktree."));
    }
    if current.branch != context.branch {
        return Err(invalid(format!(
            "The worktree is now on {}, not {}. Switch it back to remove it here.",
            current.branch, context.branch
        )));
    }
    if !git(
        context.directory.path(),
        &[
            "rev-list",
            "--max-count=1",
            "HEAD",
            "--not",
            "--remotes",
            &format!("--exclude={}", current.branch),
            "--branches",
        ],
    )?
    .is_empty()
    {
        return Err(invalid(format!(
            "The branch {} has commits that are neither pushed nor on another local branch.",
            current.branch
        )));
    }
    if git(context.directory.path(), &["ls-files", "-v", "-z"])?
        .split(|byte| *byte == 0)
        .any(|entry| {
            entry
                .first()
                .is_some_and(|flag| flag.is_ascii_lowercase() || *flag == b'S')
        })
    {
        return Err(invalid(
            "The worktree has files whose changes Git is configured to hide.",
        ));
    }
    if inspect(context.repository.path(), context.directory.path())? != current {
        return Err(invalid("Worktree changed. Try again."));
    }
    let status = git(
        context.directory.path(),
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            "--ignore-submodules=none",
        ],
    )?;
    let mut changed = Vec::new();
    let mut fields = status.split(|byte| *byte == 0);
    while let Some(entry) = fields.next() {
        let Some(path) = entry.get(3..).filter(|path| !path.is_empty()) else {
            continue;
        };
        changed.push(String::from_utf8_lossy(path).into_owned());
        if matches!(entry[0], b'R' | b'C') || matches!(entry[1], b'R' | b'C') {
            fields.next();
        }
    }
    if !status.is_empty() {
        let shown = changed
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = match changed.len().saturating_sub(3) {
            0 => String::new(),
            n => format!(" and {n} more"),
        };
        return Err(invalid(format!(
            "Uncommitted, untracked or ignored files: {shown}{more}."
        )));
    }
    git(
        context.repository.path(),
        &["worktree", "remove", path_text(context.directory.path())?],
    )?;
    Ok(())
}
