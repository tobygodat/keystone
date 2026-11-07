//! What changed between two trees of a repository, read from `git diff-tree` as plain
//! data. Reading stops after [`MAX_LINES`] lines of the patch, so a huge change costs
//! no more memory or time than that. Every call blocks.

use crate::git::{self, git};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Stdio;

/// Patch lines read before the rest is left out.
pub const MAX_LINES: usize = 200_000;
/// Bytes of one line kept; the rest of a longer line is left out.
const MAX_LINE_BYTES: usize = 4096;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patch {
    pub files: Vec<FileDiff>,
    /// The patch was longer than [`MAX_LINES`], and the files after that are missing.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Change {
    Added,
    Deleted,
    #[default]
    Modified,
    Renamed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDiff {
    /// The path after the change, or before it for a deleted file.
    pub path: String,
    /// The path before a rename.
    pub old_path: Option<String>,
    pub change: Change,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// Lines added and removed.
    pub fn counts(&self) -> (usize, usize) {
        let lines = self.hunks.iter().flat_map(|hunk| &hunk.lines);
        lines.fold((0, 0), |(added, removed), line| match line.kind {
            Kind::Added => (added + 1, removed),
            Kind::Removed => (added, removed + 1),
            Kind::Context => (added, removed),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@` line, with any function name after it.
    pub header: String,
    pub lines: Vec<Line>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    /// 1-based line numbers before and after the change, where the line is on that side.
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

/// What changed from tree `from` to tree `to` in the repository at `root`, renames
/// found.
pub fn between(root: &Path, from: &str, to: &str) -> git::Result<Patch> {
    let mut command = git(root);
    command
        .args([
            "-c",
            "core.quotePath=false",
            "diff-tree",
            "-r",
            "-p",
            "--find-renames",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            from,
            to,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| git::Error::Spawn(error.to_string()))?;
    let errors = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        })
    });
    let stdout = child.stdout.take().expect("piped stdout");
    let patch = parse(BufReader::new(stdout), MAX_LINES);
    if patch.truncated {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|error| git::Error::Spawn(error.to_string()))?;
    let errors = errors
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default();
    if !status.success() && !patch.truncated {
        return Err(git::Error::Failed {
            command: "git diff-tree".into(),
            output: errors,
            code: status.code(),
        });
    }
    Ok(patch)
}

/// Parses a `git diff` patch, keeping no more than `max_lines` of it.
pub fn parse(mut reader: impl BufRead, max_lines: usize) -> Patch {
    let mut patch = Patch::default();
    // Lines still due in the open hunk, before and after.
    let (mut old_left, mut new_left) = (0u32, 0u32);
    let (mut old_at, mut new_at) = (0u32, 0u32);
    let mut bytes = Vec::new();
    let mut read = 0;
    loop {
        if !read_line(&mut reader, &mut bytes) {
            break;
        }
        read += 1;
        if read > max_lines {
            patch.truncated = true;
            break;
        }
        let line = String::from_utf8_lossy(&bytes);
        if old_left > 0 || new_left > 0 {
            let Some(hunk) = patch.files.last_mut().and_then(|f| f.hunks.last_mut()) else {
                (old_left, new_left) = (0, 0);
                continue;
            };
            let (kind, text) = match line.split_at_checked(1) {
                Some((" ", text)) => (Kind::Context, text),
                Some(("+", text)) => (Kind::Added, text),
                Some(("-", text)) => (Kind::Removed, text),
                // "\ No newline at end of file"
                Some(("\\", _)) => continue,
                _ => (Kind::Context, ""),
            };
            let (old, new) = match kind {
                Kind::Context => (Some(old_at), Some(new_at)),
                Kind::Added => (None, Some(new_at)),
                Kind::Removed => (Some(old_at), None),
            };
            if old.is_some() {
                old_at += 1;
                old_left = old_left.saturating_sub(1);
            }
            if new.is_some() {
                new_at += 1;
                new_left = new_left.saturating_sub(1);
            }
            hunk.lines.push(Line {
                kind,
                old,
                new,
                text: text.to_owned(),
            });
            continue;
        }
        if let Some(names) = line.strip_prefix("diff --git ") {
            patch.files.push(FileDiff {
                path: same_name(names).unwrap_or_default(),
                ..FileDiff::default()
            });
            continue;
        }
        let Some(file) = patch.files.last_mut() else {
            continue;
        };
        if line.starts_with("@@ ") {
            if let Some((old, old_count, new, new_count)) = hunk_range(&line) {
                (old_at, old_left, new_at, new_left) = (old, old_count, new, new_count);
                file.hunks.push(Hunk {
                    header: line.into_owned(),
                    lines: Vec::new(),
                });
            }
        } else if line.starts_with("new file mode") {
            file.change = Change::Added;
        } else if line.starts_with("deleted file mode") {
            file.change = Change::Deleted;
        } else if let Some(from) = line.strip_prefix("rename from ") {
            file.old_path = Some(unquote(from));
            file.change = Change::Renamed;
        } else if let Some(to) = line.strip_prefix("rename to ") {
            file.path = unquote(to);
        } else if let Some(old) = line.strip_prefix("--- ") {
            if file.change == Change::Deleted
                && let Some(old) = side(old, "a/")
            {
                file.path = old;
            }
        } else if let Some(new) = line.strip_prefix("+++ ") {
            if let Some(new) = side(new, "b/") {
                file.path = new;
            }
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        }
    }
    patch
}

/// Reads one line without its newline into `bytes`, keeping at most [`MAX_LINE_BYTES`]
/// of it, so one huge line costs no more memory than a short one. False at the end.
fn read_line(reader: &mut impl BufRead, bytes: &mut Vec<u8>) -> bool {
    bytes.clear();
    let mut read_any = false;
    loop {
        let Ok(buffer) = reader.fill_buf() else {
            return read_any;
        };
        if buffer.is_empty() {
            return read_any;
        }
        read_any = true;
        let (line, used, ended) = match buffer.iter().position(|byte| *byte == b'\n') {
            Some(end) => (&buffer[..end], end + 1, true),
            None => (buffer, buffer.len(), false),
        };
        let room = MAX_LINE_BYTES.saturating_sub(bytes.len());
        bytes.extend_from_slice(&line[..line.len().min(room)]);
        reader.consume(used);
        if ended {
            return true;
        }
    }
}

/// `a/P b/P` names one path twice; a rename's two names are read from its own lines.
fn same_name(names: &str) -> Option<String> {
    if let Some(rest) = names.strip_prefix('"') {
        let end = rest.find("\" ")? + 2;
        let (old, new) = (unquote(&names[..end]), unquote(&names[end + 1..]));
        let old = old.strip_prefix("a/")?;
        return (Some(old) == new.strip_prefix("b/")).then(|| old.to_owned());
    }
    let half = names.len().checked_sub(1)? / 2;
    let (old, new) = (names.get(..half)?, names.get(half + 1..)?);
    let old = old.strip_prefix("a/")?;
    (Some(old) == new.strip_prefix("b/")).then(|| old.to_owned())
}

/// The path on a `---` or `+++` line, without its prefix, unless it is `/dev/null`.
fn side(name: &str, prefix: &str) -> Option<String> {
    let name = unquote(name.trim_end_matches('\t'));
    name.strip_prefix(prefix).map(str::to_owned)
}

/// A path as git writes it: in double quotes with C escapes when it needs them.
fn unquote(name: &str) -> String {
    let Some(inner) = name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) else {
        return name.to_owned();
    };
    let mut bytes = Vec::new();
    let mut chars = inner.bytes().peekable();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'a') => bytes.push(0x07),
            Some(b'b') => bytes.push(0x08),
            Some(b'f') => bytes.push(0x0c),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'v') => bytes.push(0x0b),
            Some(digit @ b'0'..=b'7') => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    if let Some(next @ b'0'..=b'7') = chars.peek().copied() {
                        value = value * 8 + u32::from(next - b'0');
                        chars.next();
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `@@ -a,b +c,d @@`: where each side starts and how many lines it has.
fn hunk_range(line: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = line.strip_prefix("@@ ")?.split(' ');
    let range = |part: &str| -> Option<(u32, u32)> {
        let (start, count) = part.split_once(',').unwrap_or((part, "1"));
        Some((start.parse().ok()?, count.parse().ok()?))
    };
    let (old, old_count) = range(parts.next()?.strip_prefix('-')?)?;
    let (new, new_count) = range(parts.next()?.strip_prefix('+')?)?;
    Some((old, old_count, new, new_count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn parsed(text: &str) -> Patch {
        parse(text.as_bytes(), MAX_LINES)
    }

    fn line(kind: Kind, old: Option<u32>, new: Option<u32>, text: &str) -> Line {
        Line {
            kind,
            old,
            new,
            text: text.into(),
        }
    }

    #[test]
    fn hunks_number_each_side_of_every_line() {
        let patch = parsed(
            "diff --git a/src/lib.rs b/src/lib.rs\n\
             index 1..2 100644\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -2,3 +2,3 @@ fn main() {\n \
             a\n\
             -b\n\
             +c\n \
             d\n\
             \\ No newline at end of file\n",
        );
        assert_eq!(patch.files.len(), 1);
        let file = &patch.files[0];
        assert_eq!(
            (file.path.as_str(), file.change),
            ("src/lib.rs", Change::Modified)
        );
        assert_eq!(file.hunks[0].header, "@@ -2,3 +2,3 @@ fn main() {");
        assert_eq!(
            file.hunks[0].lines,
            [
                line(Kind::Context, Some(2), Some(2), "a"),
                line(Kind::Removed, Some(3), None, "b"),
                line(Kind::Added, None, Some(3), "c"),
                line(Kind::Context, Some(4), Some(4), "d"),
            ]
        );
        assert_eq!(file.counts(), (1, 1));
    }

    #[test]
    fn a_removed_line_that_looks_like_a_header_stays_a_line() {
        let patch = parsed(
            "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1 @@\n--- a/x\n-+++ b/y\n+z\n",
        );
        let lines = &patch.files[0].hunks[0].lines;
        assert_eq!(lines[0], line(Kind::Removed, Some(1), None, "-- a/x"));
        assert_eq!(lines[1], line(Kind::Removed, Some(2), None, "+++ b/y"));
        assert_eq!(lines[2], line(Kind::Added, None, Some(1), "z"));
        assert_eq!(patch.files[0].path, "a.md");
    }

    #[test]
    fn added_deleted_renamed_and_binary_files_say_so() {
        let patch = parsed(
            "diff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hi\n\
             diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n\
             diff --git a/old name.rs b/new name.rs\nsimilarity index 100%\nrename from old name.rs\nrename to new name.rs\n\
             diff --git a/logo.png b/logo.png\nnew file mode 100644\nBinary files /dev/null and b/logo.png differ\n",
        );
        let summary: Vec<_> = patch
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.old_path.as_deref(), f.change, f.binary))
            .collect();
        assert_eq!(
            summary,
            [
                ("new.txt", None, Change::Added, false),
                ("gone.txt", None, Change::Deleted, false),
                ("new name.rs", Some("old name.rs"), Change::Renamed, false),
                ("logo.png", None, Change::Added, true),
            ]
        );
        assert_eq!(patch.files[0].hunks[0].lines[0].new, Some(1));
        assert_eq!(patch.files[1].hunks[0].lines[0].old, Some(1));
    }

    #[test]
    fn a_huge_line_is_cut_as_it_is_read() {
        let text = format!(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -0,0 +1,2 @@\n+{}\n+next\n",
            "x".repeat(MAX_LINE_BYTES * 10)
        );
        let lines = &parsed(&text).files[0].hunks[0].lines;
        assert_eq!(lines[0].text.len(), MAX_LINE_BYTES - 1);
        assert_eq!(lines[1].text, "next");
    }

    #[test]
    fn quoted_paths_are_unescaped() {
        let patch = parsed(
            "diff --git \"a/tab\\there\" \"b/tab\\there\"\nnew file mode 100644\nBinary files /dev/null and \"b/tab\\there\" differ\n",
        );
        assert_eq!(patch.files[0].path, "tab\there");
        assert_eq!(unquote("\"caf\\303\\251\""), "café");
    }

    #[test]
    fn a_patch_over_the_limit_is_cut_and_says_so() {
        let mut text = String::from("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -0,0 +1,100 @@\n");
        for n in 0..100 {
            text.push_str(&format!("+{n}\n"));
        }
        let patch = parse(text.as_bytes(), 20);
        assert!(patch.truncated);
        assert_eq!(patch.files[0].hunks[0].lines.len(), 16);
        assert!(!parse(text.as_bytes(), 200).truncated);
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "maintenance.auto", "false"]);
        run(&["config", "gc.auto", "0"]);
        dir
    }

    fn tree(root: &Path) -> String {
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        run(&["add", "-A"]);
        run(&["write-tree"])
    }

    #[test]
    fn between_reads_the_change_from_one_tree_to_another() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "same\n").unwrap();
        let before = tree(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\n2\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "new\n").unwrap();
        let after = tree(dir.path());
        let patch = between(dir.path(), &before, &after).unwrap();
        let files: Vec<_> = patch
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.change))
            .collect();
        assert_eq!(
            files,
            [("a.txt", Change::Modified), ("c.txt", Change::Added)]
        );
        assert_eq!(patch.files[0].counts(), (1, 1));
        assert!(between(dir.path(), &before, "not-a-tree").is_err());
    }

    #[test]
    fn a_huge_change_stops_at_the_limit() {
        let dir = repo();
        let before = tree(dir.path());
        let big: String = (0..MAX_LINES + 10).map(|n| format!("{n}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), big).unwrap();
        std::fs::write(dir.path().join("z.txt"), "after\n").unwrap();
        let after = tree(dir.path());
        let patch = between(dir.path(), &before, &after).unwrap();
        assert!(patch.truncated);
        assert_eq!(patch.files.len(), 1, "z.txt comes after the limit");
    }
}
