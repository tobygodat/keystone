//! Reading and saving the file the Files editor holds, so that a save never
//! overwrites a change made outside Keystone without the user choosing to.

use sha2::{Digest as _, Sha256};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Bigger files open only in the user's own editor.
pub(crate) const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// A file's size and modification time, which change on almost every write.
pub(crate) type Stamp = Option<(u64, SystemTime)>;

pub(crate) fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// What the file held when Keystone last read or wrote it. The digest decides whether
/// the file changed, since an edit can keep the size and modification time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Version {
    pub(crate) digest: [u8; 32],
    pub(crate) stamp: Stamp,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Read {
    Text(String, Version),
    Unreadable(&'static str),
}

/// The file's text, or why it cannot be edited here. Only regular files are read, and
/// no more than [`MAX_BYTES`] of them, so a pipe or a device never blocks or fills memory.
pub(crate) fn read(path: &Path) -> Read {
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_file() => return Read::Unreadable("Not a text file."),
        Ok(_) => {}
        Err(_) => return Read::Unreadable("Couldn't read this file."),
    }
    let stamp = stamp(path);
    let Ok(file) = std::fs::File::open(path) else {
        return Read::Unreadable("Couldn't read this file.");
    };
    let mut bytes = Vec::new();
    if file.take(MAX_BYTES + 1).read_to_end(&mut bytes).is_err() {
        return Read::Unreadable("Couldn't read this file.");
    }
    if bytes.len() as u64 > MAX_BYTES {
        return Read::Unreadable("Too large to edit here.");
    }
    let digest = Sha256::digest(&bytes).into();
    let Ok(text) = String::from_utf8(bytes) else {
        return Read::Unreadable("Not a text file.");
    };
    if text.contains('\0') {
        return Read::Unreadable("Not a text file.");
    }
    Read::Text(text, Version { digest, stamp })
}

/// What the file on disk holds now, compared with `known`.
#[derive(Debug, PartialEq)]
pub(crate) enum Disk {
    Unchanged(Version),
    Changed(Read),
    Deleted,
}

pub(crate) fn check(path: &Path, known: &Version) -> Disk {
    if !path.exists() {
        return Disk::Deleted;
    }
    match read(path) {
        Read::Text(_, version) if version.digest == known.digest => Disk::Unchanged(version),
        read => Disk::Changed(read),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Saved {
    Written(Version),
    /// The file changed on disk since `known`, and nothing was written.
    Conflict(Disk),
    Failed(String),
}

/// Writes `text` unless the file no longer holds `known`. With `known` as `None` it
/// writes whatever the file holds, which is how the user overwrites an outside change.
///
/// The new contents go to a temporary file in the same folder first, and only then is
/// the file checked and the temporary renamed over it. That leaves the slow part out
/// of the gap between the check and the replace, and a crash mid-write leaves the old
/// file whole. A symlink is followed, even to a target that's gone, so the link stays
/// a link, and the file keeps its permissions.
pub(crate) fn save(path: &Path, text: &str, known: Option<&Version>) -> Saved {
    let conflict = || {
        known.and_then(|known| match check(path, known) {
            Disk::Unchanged(_) => None,
            other => Some(Saved::Conflict(other)),
        })
    };
    let prepared = resolve(path).and_then(|target| {
        let (temporary, stamp) = prepare(&target, text.as_bytes())?;
        Ok((target, temporary, stamp))
    });
    let (target, temporary, stamp) = match prepared {
        Ok(prepared) => prepared,
        // A change on disk explains more than the error it may have caused.
        Err(error) => {
            return conflict().unwrap_or_else(|| Saved::Failed(format!("Couldn't save: {error}")));
        }
    };
    if let Some(conflict) = conflict() {
        return conflict;
    }
    match temporary.persist(&target) {
        Ok(_) => Saved::Written(Version {
            digest: Sha256::digest(text.as_bytes()).into(),
            stamp,
        }),
        Err(error) => {
            let error = error.error;
            Saved::Failed(format!("Couldn't save: {error}"))
        }
    }
}

/// The new contents in a temporary file beside `target`, and the stamp the file will
/// have once renamed into place, read from the file itself rather than the path, which
/// another writer may replace.
fn prepare(target: &Path, bytes: &[u8]) -> std::io::Result<(tempfile::NamedTempFile, Stamp)> {
    // A rename needs only the folder to be writable, so it would replace a file the user
    // can't write. Opening it for writing, without truncating, asks the system first.
    if target.exists() {
        std::fs::OpenOptions::new().write(true).open(target)?;
    }
    let folder = target.parent().unwrap_or(Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".keystone-save-")
        .tempfile_in(folder)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    if let Ok(meta) = std::fs::metadata(target) {
        temporary.as_file().set_permissions(meta.permissions())?;
    }
    let meta = temporary.as_file().metadata()?;
    let stamp = meta.modified().ok().map(|modified| (meta.len(), modified));
    Ok((temporary, stamp))
}

/// Where a write to `path` lands: the end of its chain of symlinks, which may not exist
/// yet. A chain that loops has no end, and writing anywhere in it would replace a link,
/// so a chain longer than the system allows is refused.
fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Ok(resolved);
    }
    let mut path = path.to_owned();
    for hops in 0..=40 {
        let Ok(next) = std::fs::read_link(&path) else {
            return Ok(path);
        };
        if hops == 40 {
            break;
        }
        path = path.parent().unwrap_or(Path::new("")).join(next);
    }
    Err(std::io::Error::other("too many symlinks"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn version(path: &Path) -> Version {
        match read(path) {
            Read::Text(_, version) => version,
            other => panic!("not text: {other:?}"),
        }
    }

    #[test]
    fn text_reads_whole_and_binary_large_missing_and_special_files_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\r\n\ttwo\n").unwrap();
        assert!(matches!(read(&file), Read::Text(text, _) if text == "one\r\n\ttwo\n"));
        std::fs::write(&file, b"a\0b").unwrap();
        assert_eq!(read(&file), Read::Unreadable("Not a text file."));
        std::fs::write(&file, [0xff, 0xfe, b'a']).unwrap();
        assert_eq!(read(&file), Read::Unreadable("Not a text file."));
        std::fs::write(&file, vec![b'a'; MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(read(&file), Read::Unreadable("Too large to edit here."));
        assert_eq!(
            read(&dir.path().join("missing")),
            Read::Unreadable("Couldn't read this file.")
        );
        assert_eq!(read(dir.path()), Read::Unreadable("Not a text file."));
        let fifo = dir.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || done.send(read(&fifo)).unwrap());
        assert_eq!(
            wait.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(Read::Unreadable("Not a text file.")),
            "a pipe is refused without waiting for a writer"
        );
    }

    #[test]
    fn a_save_writes_when_the_file_is_as_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "old\n").unwrap();
        let known = version(&file);
        let Saved::Written(written) = save(&file, "new\n", Some(&known)) else {
            panic!("not written");
        };
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");
        assert_eq!(written, version(&file));
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary file is left behind");
    }

    #[test]
    fn an_outside_change_is_never_overwritten_unless_asked() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "aaaa\n").unwrap();
        let known = version(&file);
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, "bbbb\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(stamp(&file), known.stamp, "same size and time");
        let Saved::Conflict(Disk::Changed(Read::Text(outside, _))) =
            save(&file, "mine\n", Some(&known))
        else {
            panic!("not a conflict");
        };
        assert_eq!(outside, "bbbb\n");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "bbbb\n");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "a refused save leaves no temporary file"
        );
        assert!(matches!(save(&file, "mine\n", None), Saved::Written(_)));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine\n");
    }

    #[test]
    fn a_deleted_file_is_a_conflict_and_can_be_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "a\n").unwrap();
        let known = version(&file);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            save(&file, "b\n", Some(&known)),
            Saved::Conflict(Disk::Deleted)
        );
        assert!(matches!(save(&file, "b\n", None), Saved::Written(_)));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "b\n");
    }

    #[test]
    fn a_save_keeps_permissions_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("run.sh");
        keystone_core::test_support::write_executable(&target, "#!/bin/sh\n");
        let link = dir.path().join("link.sh");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let known = version(&link);
        assert!(matches!(
            save(&link, "#!/bin/sh\necho hi\n", Some(&known)),
            Saved::Written(_)
        ));
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "#!/bin/sh\necho hi\n"
        );
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn overwriting_through_a_dangling_symlink_recreates_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let (target, link) = (dir.path().join("target.rs"), dir.path().join("link.rs"));
        std::os::unix::fs::symlink("target.rs", &link).unwrap();
        let Saved::Written(written) = save(&link, "back\n", None) else {
            panic!("not written");
        };
        assert_eq!(written.stamp, stamp(&link));
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "back\n");
    }

    #[test]
    fn a_symlink_loop_is_refused_and_every_link_kept() {
        let dir = tempfile::tempdir().unwrap();
        let [a, b, c] = ["a", "b", "c"].map(|name| dir.path().join(name));
        std::os::unix::fs::symlink("b", &a).unwrap();
        std::os::unix::fs::symlink("c", &b).unwrap();
        std::os::unix::fs::symlink("a", &c).unwrap();
        assert!(
            matches!(save(&a, "x\n", None), Saved::Failed(ref why) if why == "Couldn't save: too many symlinks")
        );
        for link in [&a, &b, &c] {
            assert!(
                std::fs::symlink_metadata(link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[test]
    fn a_read_only_file_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "a\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
        let known = version(&file);
        for known in [Some(&known), None] {
            let saved = save(&file, "b\n", known);
            assert!(
                matches!(saved, Saved::Failed(ref why) if why.starts_with("Couldn't save: ")),
                "{saved:?}"
            );
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\n");
        }
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o444);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary file is left behind"
        );
    }

    #[test]
    fn a_save_that_cannot_write_says_why_and_leaves_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "a\n").unwrap();
        let known = version(&file);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let saved = save(&file, "b\n", Some(&known));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(saved, Saved::Failed(ref why) if why.starts_with("Couldn't save: ")));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\n");
    }
}
