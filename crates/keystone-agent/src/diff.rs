//! What changed between two trees of a repository, read from `git diff-tree` as plain
//! data, and what one agent edit changed, built from what its provider sent. Reading
//! stops after [`MAX_LINES`] lines of the patch, so a huge change costs no more memory
//! or time than that. Every git call blocks.
//!
//! A patch serializes compactly: each hunk as its header and its lines in unified diff
//! form (` a`, `-b`, `+c`), with line numbers worked out again when it is read.

use crate::git::{self, git};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Stdio;

/// Patch lines read before the rest is left out.
pub const MAX_LINES: usize = 200_000;
/// Bytes of one line kept; the rest of a longer line is left out.
const MAX_LINE_BYTES: usize = 4096;
/// Bytes of an edit's lines kept: the log's cap on a tool's output, both ends together.
pub const EDIT_BYTES: usize = crate::item::OUTPUT_CAP * 2;
/// Lines of an edit kept in the log. A transcript row draws only the first few of them;
/// the diff view, which lays out only the lines in view, shows the rest.
pub const EDIT_LINES: usize = 1000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    #[serde(default)]
    pub files: Vec<FileDiff>,
    /// The patch was cut at a limit ([`MAX_LINES`], or [`EDIT_LINES`] and
    /// [`EDIT_BYTES`] for an edit), and the lines after that are missing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl Patch {
    /// An agent edit's files, kept to [`EDIT_LINES`] lines and [`EDIT_BYTES`] bytes of
    /// line text. Hunks and files after the limit are dropped, and `truncated` says so.
    pub fn edit(mut files: Vec<FileDiff>) -> Patch {
        let (mut lines, mut bytes) = (0, 0);
        let mut cut = None;
        'files: for (f, file) in files.iter().enumerate() {
            for (h, hunk) in file.hunks.iter().enumerate() {
                for (l, line) in hunk.lines.iter().enumerate() {
                    lines += 1;
                    bytes += line.text.len() + 1;
                    if lines > EDIT_LINES || bytes > EDIT_BYTES {
                        cut = Some((f, h, l));
                        break 'files;
                    }
                }
            }
        }
        if let Some((f, h, l)) = cut {
            files.truncate(f + 1);
            let file = &mut files[f];
            file.hunks.truncate(h + 1);
            file.hunks[h].lines.truncate(l);
            if l == 0 {
                file.hunks.pop();
            } else {
                file.hunks[h].recount();
            }
            if file.hunks.is_empty() && f > 0 {
                files.pop();
            }
        }
        Patch {
            files,
            truncated: cut.is_some(),
        }
    }

    /// The same patch with each path as a user should see it from `folder`: relative to
    /// it when inside it ([`shown_path`]).
    pub fn shown_from(&self, folder: &str) -> Patch {
        let mut patch = self.clone();
        for file in &mut patch.files {
            file.path = shown_path(&file.path, folder).into_owned();
            if let Some(old) = &mut file.old_path {
                *old = shown_path(old, folder).into_owned();
            }
        }
        patch
    }

    /// The patch as unified diff text: each file's `---` and `+++` lines, then its hunks.
    /// A relative path takes git's `a/` and `b/` prefixes; an absolute one is written as
    /// it is. A cut patch starts with a line saying so, which patch tools skip as text
    /// before the first file.
    pub fn text(&self) -> String {
        let mut text = String::new();
        let partial = "so this diff is partial and will not apply as it is.";
        if self.truncated {
            let kept: usize = self
                .files
                .iter()
                .flat_map(|f| &f.hunks)
                .map(|h| h.lines.len())
                .sum();
            text.push_str(&format!(
                "Keystone kept only the first {kept} lines of this edit, {partial}\n"
            ));
        }
        if self
            .files
            .iter()
            .flat_map(|f| &f.hunks)
            .any(|h| h.shortened)
        {
            text.push_str(&format!(
                "Keystone shortened lines longer than {} KiB, {partial}\n",
                MAX_LINE_BYTES / 1024
            ));
        }
        let side = |prefix: &str, path: &str| match path.starts_with('/') {
            true => path.to_owned(),
            false => format!("{prefix}{path}"),
        };
        for file in &self.files {
            let old = file.old_path.as_deref().unwrap_or(&file.path);
            let (from, to) = match file.change {
                Change::Added => ("/dev/null".to_owned(), side("b/", &file.path)),
                Change::Deleted => (side("a/", old), "/dev/null".to_owned()),
                Change::Modified | Change::Renamed => (side("a/", old), side("b/", &file.path)),
            };
            text.push_str(&format!("--- {from}\n+++ {to}\n"));
            for hunk in &file.hunks {
                let wire = WireHunk::from(hunk.clone());
                text.push_str(&wire.header);
                text.push('\n');
                for line in wire.lines {
                    text.push_str(&line);
                    text.push('\n');
                }
            }
        }
        text
    }
}

/// `path` as a user should see it from `folder`, the session's folder here or on
/// another machine: relative to it when it lies inside it, else as given. Paths are
/// compared as text, a whole component at a time, since a remote folder names nothing
/// on this machine; repeated separators count as one.
pub fn shown_path<'a>(path: &'a str, folder: &str) -> std::borrow::Cow<'a, str> {
    let path = single_slashes(path);
    let folder = single_slashes(folder);
    let folder = folder.trim_end_matches('/');
    let inside = path
        .strip_prefix(folder)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|inside| !inside.is_empty())
        .map(str::to_owned);
    match inside {
        Some(inside) => inside.into(),
        None => path,
    }
}

/// `text` with each run of `/` made one.
fn single_slashes(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("//") {
        return text.into();
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if !(c == '/' && out.ends_with('/')) {
            out.push(c);
        }
    }
    out.into()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Added,
    Deleted,
    #[default]
    Modified,
    Renamed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// The path after the change, or before it for a deleted file.
    pub path: String,
    /// The path before a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(default)]
    pub change: Change,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    #[serde(default)]
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// A new file holding `text`: every line added.
    pub fn created(path: String, text: &str) -> FileDiff {
        Self::whole(path, Change::Added, text, '+')
    }

    /// A deleted file that held `text`: every line removed.
    pub fn deleted(path: String, text: &str) -> FileDiff {
        Self::whole(path, Change::Deleted, text, '-')
    }

    fn whole(path: String, change: Change, text: &str, mark: char) -> FileDiff {
        let mut lines: Vec<String> = text.lines().map(|line| format!("{mark}{line}")).collect();
        if !lines.is_empty() && !text.ends_with('\n') {
            lines.push(NO_NEWLINE.to_owned());
        }
        let hunks = if lines.is_empty() {
            Vec::new()
        } else {
            let start = |side| u32::from(mark == side);
            Hunk::new(start('-'), start('+'), lines)
                .into_iter()
                .collect()
        };
        FileDiff {
            path,
            change,
            hunks,
            ..FileDiff::default()
        }
    }

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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireHunk", into = "WireHunk")]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@` line, with any function name after it.
    pub header: String,
    pub lines: Vec<Line>,
    /// A line was longer than [`MAX_LINE_BYTES`] and its end was left out.
    pub shortened: bool,
}

impl Hunk {
    /// A hunk whose sides start at `old` and `new`, from lines marked as in a unified
    /// diff: ` ` for context, `-` removed and `+` added. A line with no mark is empty
    /// context, and [`NO_NEWLINE`] marks the line before it ([`Line::no_newline`]).
    /// Lines are numbered from the starts and cut to [`MAX_LINE_BYTES`]. `None` when a
    /// line number would pass `u32::MAX`, as only a malformed patch numbers its lines.
    pub fn new<S: AsRef<str>>(
        old: u32,
        new: u32,
        marked: impl IntoIterator<Item = S>,
    ) -> Option<Hunk> {
        let (mut old_at, mut new_at) = (old, new);
        let mut lines: Vec<Line> = Vec::new();
        let mut shortened = false;
        for marked in marked {
            let marked = marked.as_ref();
            let (kind, text) = match marked.split_at_checked(1) {
                Some(("+", text)) => (Kind::Added, text),
                Some(("-", text)) => (Kind::Removed, text),
                Some(("\\", _)) => {
                    if let Some(line) = lines.last_mut() {
                        line.no_newline = true;
                    }
                    continue;
                }
                Some((_, text)) => (Kind::Context, text),
                None => (Kind::Context, ""),
            };
            let (old, new) = match kind {
                Kind::Context => (Some(old_at), Some(new_at)),
                Kind::Added => (None, Some(new_at)),
                Kind::Removed => (Some(old_at), None),
            };
            old_at = old_at.checked_add(u32::from(old.is_some()))?;
            new_at = new_at.checked_add(u32::from(new.is_some()))?;
            let kept = &text[..floor_char_boundary(text, MAX_LINE_BYTES - 1)];
            shortened |= kept.len() < text.len();
            lines.push(Line {
                kind,
                old,
                new,
                text: kept.to_owned(),
                no_newline: false,
            });
        }
        let count = |side: fn(&Line) -> Option<u32>| lines.iter().filter_map(side).count();
        let header = format!(
            "@@ -{old},{} +{new},{} @@",
            count(|line| line.old),
            count(|line| line.new)
        );
        Some(Hunk {
            header,
            lines,
            shortened,
        })
    }
}

impl Hunk {
    /// Makes the header's line counts match the lines the hunk holds, as after a cut.
    fn recount(&mut self) {
        let (old, _, new, _) = hunk_range(&self.header).unwrap_or_default();
        let old = self.lines.iter().find_map(|line| line.old).unwrap_or(old);
        let new = self.lines.iter().find_map(|line| line.new).unwrap_or(new);
        let count = |side: fn(&Line) -> Option<u32>| self.lines.iter().filter_map(side).count();
        self.header = format!(
            "@@ -{old},{} +{new},{} @@",
            count(|line| line.old),
            count(|line| line.new)
        );
    }
}

/// A hunk as it is stored: its header and its marked lines.
#[derive(Serialize, Deserialize)]
struct WireHunk {
    header: String,
    lines: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    shortened: bool,
}

impl From<Hunk> for WireHunk {
    fn from(hunk: Hunk) -> WireHunk {
        let mut lines = Vec::with_capacity(hunk.lines.len());
        for line in hunk.lines {
            let mark = match line.kind {
                Kind::Context => ' ',
                Kind::Added => '+',
                Kind::Removed => '-',
            };
            lines.push(format!("{mark}{}", line.text));
            if line.no_newline {
                lines.push(NO_NEWLINE.to_owned());
            }
        }
        WireHunk {
            header: hunk.header,
            lines,
            shortened: hunk.shortened,
        }
    }
}

impl TryFrom<WireHunk> for Hunk {
    type Error = String;

    fn try_from(wire: WireHunk) -> Result<Hunk, String> {
        let (old, _, new, _) = hunk_range(&wire.header).unwrap_or_default();
        let hunk = Hunk::new(old, new, wire.lines)
            .ok_or_else(|| format!("line numbers past the end in {}", wire.header))?;
        Ok(Hunk {
            header: wire.header,
            shortened: hunk.shortened || wire.shortened,
            ..hunk
        })
    }
}

/// The hunks of a unified diff that has no file headers, as Codex sends an update:
/// each `@@` line starts a hunk, and lines before the first are skipped.
pub fn hunks(text: &str) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut open: Option<(String, Vec<&str>)> = None;
    let mut close = |open: Option<(String, Vec<&str>)>| {
        let Some((header, lines)) = open else {
            return;
        };
        let wire = WireHunk {
            header,
            lines: lines.into_iter().map(str::to_owned).collect(),
            shortened: false,
        };
        hunks.extend(Hunk::try_from(wire).ok());
    };
    for line in text.lines() {
        if line.starts_with("@@ ") && hunk_range(line).is_some() {
            close(open.replace((line.to_owned(), Vec::new())));
        } else if let Some((_, lines)) = &mut open {
            lines.push(line);
        }
    }
    close(open);
    hunks
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
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
    /// The file ends after this line with no newline: `\ No newline at end of file`
    /// followed it. A context line can carry it for both sides.
    pub no_newline: bool,
}

/// The marker a unified diff puts after a file's last line when it has no newline.
pub const NO_NEWLINE: &str = "\\ No newline at end of file";

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
                no_newline: false,
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
                    shortened: false,
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
            no_newline: false,
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

    /// A small seeded generator, so a failing case can be replayed from its seed.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn text(&mut self) -> String {
            const CHARS: &[char] = &[
                'a', 'Z', ' ', '\t', '@', '+', '-', '\\', 'é', '漢', '🦀', '"',
            ];
            if self.below(40) == 0 {
                return "é".repeat(MAX_LINE_BYTES);
            }
            (0..self.below(12))
                .map(|_| CHARS[self.below(CHARS.len())])
                .collect()
        }
    }

    /// What a generated line should read as once parsed: its text, cut as the parser
    /// cuts a long line.
    fn kept(text: &str) -> String {
        text[..floor_char_boundary(text, MAX_LINE_BYTES - 1)].to_owned()
    }

    #[test]
    fn generated_hunks_parse_number_and_round_trip() {
        let mut cut = [0, 0];
        for seed in 1..=500u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut text = String::new();
            for _ in 0..rng.below(3) {
                text.push_str("diff header the parser skips\n");
            }
            let mut expected = Vec::new();
            let count = if rng.below(8) == 0 {
                20 + rng.below(40)
            } else {
                1 + rng.below(4)
            };
            for _ in 0..count {
                let (old, new) = (rng.below(500) as u32, rng.below(500) as u32);
                let lines: Vec<(Kind, String, bool)> = (0..rng.below(30))
                    .map(|_| {
                        let kind = [Kind::Context, Kind::Added, Kind::Removed][rng.below(3)];
                        (kind, rng.text(), rng.below(10) == 0)
                    })
                    .collect();
                let count = |side: Kind| lines.iter().filter(|(k, _, _)| *k != side).count();
                text.push_str(&format!(
                    "@@ -{old},{} +{new},{} @@ fn f()\n",
                    count(Kind::Added),
                    count(Kind::Removed)
                ));
                for (kind, line, no_newline) in &lines {
                    let mark = match kind {
                        Kind::Context => ' ',
                        Kind::Added => '+',
                        Kind::Removed => '-',
                    };
                    text.push_str(&format!("{mark}{line}\n"));
                    if *no_newline {
                        text.push_str(&format!("{NO_NEWLINE}\n"));
                    }
                }
                expected.push((old, new, lines));
            }

            let parsed = hunks(&text);
            assert_eq!(parsed.len(), expected.len(), "seed {seed}");
            for (hunk, (old, new, lines)) in parsed.iter().zip(&expected) {
                let read: Vec<_> = hunk
                    .lines
                    .iter()
                    .map(|l| (l.kind, l.text.clone(), l.no_newline))
                    .collect();
                let want: Vec<_> = lines.iter().map(|(k, t, n)| (*k, kept(t), *n)).collect();
                assert_eq!(read, want, "seed {seed}");
                let olds: Vec<u32> = hunk.lines.iter().filter_map(|l| l.old).collect();
                let news: Vec<u32> = hunk.lines.iter().filter_map(|l| l.new).collect();
                assert!(
                    olds.iter().copied().eq(*old..*old + olds.len() as u32),
                    "seed {seed}"
                );
                assert!(
                    news.iter().copied().eq(*new..*new + news.len() as u32),
                    "seed {seed}"
                );
                assert!(hunk.header.ends_with(" @@ fn f()"), "seed {seed}");
            }

            let file = FileDiff {
                path: format!("file {seed}.rs"),
                hunks: parsed,
                ..FileDiff::default()
            };
            let all: Vec<Line> = file.hunks.iter().flat_map(|h| h.lines.clone()).collect();
            let patch = Patch::edit(vec![file.clone(), file]);
            let json = serde_json::to_string(&patch).unwrap();
            assert_eq!(
                serde_json::from_str::<Patch>(&json).unwrap(),
                patch,
                "seed {seed}"
            );

            let kept: Vec<&Line> = patch
                .files
                .iter()
                .flat_map(|f| &f.hunks)
                .flat_map(|h| &h.lines)
                .collect();
            let bytes: usize = kept.iter().map(|l| l.text.len() + 1).sum();
            assert!(
                kept.len() <= EDIT_LINES && bytes <= EDIT_BYTES,
                "seed {seed}"
            );
            let whole: Vec<&Line> = all.iter().chain(&all).collect();
            assert_eq!(
                kept[..],
                whole[..kept.len()],
                "seed {seed}: kept lines are a prefix"
            );
            let whole_bytes: usize = whole.iter().map(|l| l.text.len() + 1).sum();
            let over = whole.len() > EDIT_LINES || whole_bytes > EDIT_BYTES;
            assert_eq!(patch.truncated, over, "seed {seed}");
            cut[usize::from(over)] += 1;
        }
        assert!(
            cut[0] > 0 && cut[1] > 0,
            "both whole and cut edits generated: {cut:?}"
        );
    }

    #[test]
    fn any_text_reads_as_hunks_without_panicking() {
        for seed in 1..=500u64 {
            let mut rng = Rng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) | 1);
            let text: String = (0..rng.below(20))
                .map(|_| match rng.below(4) {
                    0 => format!("@@ -{} +{} @@\n", rng.text(), rng.text()),
                    1 => "@@ -1,2 +3 @@\n".to_owned(),
                    _ => format!("{}\n", rng.text()),
                })
                .collect();
            for hunk in hunks(&text) {
                let round = Hunk::try_from(WireHunk::from(hunk.clone())).unwrap();
                assert_eq!(round, hunk, "seed {seed}");
            }
        }
    }

    #[test]
    fn an_edit_over_the_cap_keeps_its_start_and_says_it_was_cut() {
        let long: String = (0..EDIT_LINES + 50)
            .map(|n| format!("line {n}\n"))
            .collect();
        let patch = Patch::edit(vec![
            FileDiff::created("big.txt".into(), &long),
            FileDiff::created("after.txt".into(), "x\n"),
        ]);
        assert!(patch.truncated);
        assert_eq!(patch.files.len(), 1, "after.txt comes after the cut");
        let lines = &patch.files[0].hunks[0].lines;
        assert_eq!(lines.len(), EDIT_LINES);
        assert_eq!(lines[0].text, "line 0");
        assert_eq!(lines[EDIT_LINES - 1].new, Some(EDIT_LINES as u32));

        let wide = "w".repeat(1000);
        let wide: String = (0..100).map(|_| format!("{wide}\n")).collect();
        let patch = Patch::edit(vec![FileDiff::deleted("wide.txt".into(), &wide)]);
        assert!(patch.truncated);
        assert_eq!(patch.files[0].hunks[0].lines.len(), EDIT_BYTES / 1001);
        assert!(!Patch::edit(vec![FileDiff::created("small".into(), "a\nb\n")]).truncated);
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
