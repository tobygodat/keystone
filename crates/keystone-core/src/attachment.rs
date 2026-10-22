//! Images attached to drafts and messages. Each is copied into Keystone's data folder,
//! named by the SHA-256 of its bytes, so it survives the original moving and the same
//! image is stored once.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// The largest image Keystone copies, unless `KEYSTONE_ATTACHMENT_MAX_MB` sets another
/// limit in MiB. Both tested CLIs resize large images themselves before they reach the
/// model, so this bounds storage and each image Keystone sends, not the model's limit.
pub const DEFAULT_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// How long a copy is kept after it was last attached or sent, unless
/// `KEYSTONE_ATTACHMENT_KEEP_DAYS` sets another number of days (0 keeps copies forever).
/// A copy a draft still holds is always kept.
pub const DEFAULT_KEEP_DAYS: u64 = 30;

/// The image types both providers accept, by media type and the extension of the copy.
const TYPES: [(&str, &str); 4] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
];

/// An image in a draft or a sent message. The bytes stay in the attachments folder;
/// drafts and logs hold only this reference.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Attachment {
    /// Lowercase hex SHA-256 of the image's bytes.
    pub sha256: String,
    pub media_type: String,
    /// The file name it was attached as, shown on its chip.
    pub name: String,
}

impl Attachment {
    /// Where its copy is in the attachments folder `dir`. Only the hash's hex digits
    /// name it, so a damaged record can never point outside `dir`.
    pub fn path(&self, dir: &Path) -> PathBuf {
        let extension = TYPES
            .iter()
            .find(|(media_type, _)| *media_type == self.media_type)
            .map_or("bin", |(_, extension)| extension);
        let hash: String = self
            .sha256
            .chars()
            .filter(char::is_ascii_hexdigit)
            .collect();
        dir.join(format!("{hash}.{extension}"))
    }
}

/// The media type of an image of a type both providers accept, from its first bytes.
pub fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The attachments folder, with its size limit. Clones share one lock, so a copy is
/// never written twice at once or deleted by [`Attachments::sweep`] while it is being
/// attached.
#[derive(Clone, Debug)]
pub struct Attachments {
    dir: PathBuf,
    max_bytes: u64,
    lock: Arc<Mutex<()>>,
}

impl Attachments {
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self {
            dir,
            max_bytes,
            lock: Arc::default(),
        }
    }

    /// The `attachments` folder in `data_dir`, made absolute, since Codex reads the
    /// copies from its own working directory. The limit is `KEYSTONE_ATTACHMENT_MAX_MB`
    /// when that is a positive number of MiB, else [`DEFAULT_MAX_BYTES`].
    pub fn in_data_dir(data_dir: &Path) -> Self {
        let max_bytes = std::env::var("KEYSTONE_ATTACHMENT_MAX_MB")
            .ok()
            .and_then(|mb| mb.trim().parse::<u64>().ok())
            .filter(|mb| *mb > 0)
            .map_or(DEFAULT_MAX_BYTES, |mb| mb.saturating_mul(1024 * 1024));
        let dir = data_dir.join("attachments");
        Self::new(std::path::absolute(&dir).unwrap_or(dir), max_bytes)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Copies the file at `path` when it is an image of an accepted type. `Ok(None)`
    /// means it is some other file, which is attached by mentioning its path instead.
    /// An image over the limit, a folder or an unreadable file is refused with a reason.
    pub fn import_file(&self, path: &Path) -> Result<Option<Attachment>, String> {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into(),
        );
        let unreadable = |error: io::Error| format!("{name} could not be read: {error}.");
        let metadata = fs::metadata(path).map_err(unreadable)?;
        if metadata.is_dir() {
            return Err(format!("{name} is a folder. Attach files, not folders."));
        }
        let mut file = fs::File::open(path).map_err(unreadable)?;
        let mut head = [0; 12];
        let read = read_up_to(&mut file, &mut head).map_err(unreadable)?;
        if image_type(&head[..read]).is_none() {
            return Ok(None);
        }
        self.check_size(&name, metadata.len())?;
        let mut bytes = head[..read].to_vec();
        // Read one byte past the limit, so a file that grew since is still refused.
        file.take((self.max_bytes + 1).saturating_sub(read as u64))
            .read_to_end(&mut bytes)
            .map_err(unreadable)?;
        self.import_bytes(&bytes, name).map(Some)
    }

    /// Copies an image held in memory, such as one pasted from the clipboard.
    pub fn import_bytes(&self, bytes: &[u8], name: String) -> Result<Attachment, String> {
        let Some(media_type) = image_type(bytes) else {
            return Err(format!(
                "{name} is not a png, jpeg, gif or webp image, the types Keystone attaches."
            ));
        };
        self.check_size(&name, bytes.len() as u64)?;
        let attachment = Attachment {
            sha256: hex(&Sha256::digest(bytes)),
            media_type: media_type.into(),
            name,
        };
        let copied = |error: io::Error| {
            format!(
                "{} could not be copied into Keystone's data folder: {error}.",
                attachment.name
            )
        };
        let target = attachment.path(&self.dir);
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if target.exists() {
            self.touch(&attachment);
            return Ok(attachment);
        }
        fs::create_dir_all(&self.dir).map_err(copied)?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temporary = self.dir.join(format!(
            ".{}.{}.{}.tmp",
            attachment.sha256,
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let written = fs::File::create_new(&temporary)
            .and_then(|mut file| {
                io::Write::write_all(&mut file, bytes)?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(&temporary, &target))
            .and_then(|()| fs::File::open(&self.dir)?.sync_all());
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary);
            return Err(copied(error));
        }
        Ok(attachment)
    }

    fn check_size(&self, name: &str, size: u64) -> Result<(), String> {
        if size <= self.max_bytes {
            return Ok(());
        }
        Err(format!(
            "{name} is {}, over the {} limit for an attached image.",
            megabytes(size),
            megabytes(self.max_bytes)
        ))
    }

    /// Marks the copy as used now, so retention counts from this use.
    pub fn touch(&self, attachment: &Attachment) {
        if let Ok(file) = fs::File::options()
            .write(true)
            .open(attachment.path(&self.dir))
        {
            let _ = file.set_modified(SystemTime::now());
        }
    }

    /// Deletes copies last used more than `keep_for` before `now`, except those named
    /// in `keep` (by SHA-256), and copies a crash left half written. Returns how many it
    /// deleted. A missing folder has nothing to delete.
    pub fn sweep(
        &self,
        keep: &HashSet<String>,
        keep_for: Duration,
        now: SystemTime,
    ) -> io::Result<usize> {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut deleted = 0;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let partial = name.starts_with('.') && name.ends_with(".tmp");
            let hash = name.split('.').next().unwrap_or_default();
            if !partial && keep.contains(hash) {
                continue;
            }
            let age = |limit: Duration| {
                entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|used| now.duration_since(used).is_ok_and(|age| age > limit))
            };
            let expired = if partial {
                age(Duration::from_secs(24 * 60 * 60))
            } else {
                age(keep_for)
            };
            if expired && entry.file_type().is_ok_and(|t| t.is_file()) {
                fs::remove_file(entry.path())?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }
}

/// How long copies are kept after their last use: `KEYSTONE_ATTACHMENT_KEEP_DAYS` days,
/// else [`DEFAULT_KEEP_DAYS`]. `None` keeps them forever.
pub fn keep_for() -> Option<Duration> {
    let days = std::env::var("KEYSTONE_ATTACHMENT_KEEP_DAYS")
        .ok()
        .and_then(|days| days.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_KEEP_DAYS);
    (days > 0).then(|| Duration::from_secs(days.saturating_mul(24 * 60 * 60)))
}

fn read_up_to(file: &mut fs::File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut read = 0;
    while read < buffer.len() {
        match file.read(&mut buffer[read..])? {
            0 => break,
            n => read += n,
        }
    }
    Ok(read)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn folder() -> (tempfile::TempDir, Attachments) {
        let dir = tempfile::tempdir().unwrap();
        let attachments = Attachments::new(dir.path().join("attachments"), 64);
        (dir, attachments)
    }

    #[test]
    fn image_types_are_read_from_the_bytes_not_the_name() {
        assert_eq!(image_type(PNG), Some("image/png"));
        assert_eq!(image_type(b"\xff\xd8\xff\xe0rest"), Some("image/jpeg"));
        assert_eq!(image_type(b"GIF89a.."), Some("image/gif"));
        assert_eq!(image_type(b"RIFF\x10\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_type(b"RIFF\x10\0\0\0WAVEfmt "), None);
        assert_eq!(image_type(b"BM\x36\0"), None, "bmp is not accepted");
        assert_eq!(image_type(b"<svg"), None);
        assert_eq!(image_type(b""), None);
    }

    #[test]
    fn an_image_is_copied_once_by_its_hash_and_survives_the_original_moving() {
        let (dir, attachments) = folder();
        let original = dir.path().join("shot.PNG.renamed");
        fs::write(&original, PNG).unwrap();
        let first = attachments.import_file(&original).unwrap().unwrap();
        assert_eq!(first.media_type, "image/png");
        assert_eq!(first.name, "shot.PNG.renamed");
        assert_eq!(first.sha256, hex(&Sha256::digest(PNG)));
        let copy = first.path(attachments.dir());
        assert_eq!(
            copy,
            attachments.dir().join(format!("{}.png", first.sha256))
        );
        fs::remove_file(&original).unwrap();
        assert_eq!(
            fs::read(&copy).unwrap(),
            PNG,
            "the copy outlives the original"
        );
        let again = attachments
            .import_bytes(PNG, "Pasted image.png".into())
            .unwrap();
        assert_eq!(again.sha256, first.sha256);
        assert_eq!(fs::read_dir(attachments.dir()).unwrap().count(), 1);
    }

    #[test]
    fn other_files_are_not_copied_but_left_to_be_mentioned() {
        let (dir, attachments) = folder();
        let notes = dir.path().join("notes.txt");
        fs::write(&notes, "x".repeat(1000)).unwrap();
        assert_eq!(
            attachments.import_file(&notes),
            Ok(None),
            "even over the limit"
        );
        assert!(!attachments.dir().exists());
    }

    #[test]
    fn an_oversized_image_a_folder_and_a_missing_file_are_refused_with_a_reason() {
        let (dir, attachments) = folder();
        let big = dir.path().join("big.png");
        fs::write(&big, [PNG, &[0; 64]].concat()).unwrap();
        let refused = attachments.import_file(&big).unwrap_err();
        assert!(
            refused.starts_with("big.png is ") && refused.contains(" MB, over the "),
            "{refused}"
        );
        let refused = attachments
            .import_bytes(&[PNG, &[0; 64]].concat(), "Pasted image.png".into())
            .unwrap_err();
        assert!(refused.contains("over the"), "{refused}");
        let at_limit = [PNG, &[0; 48]].concat();
        assert_eq!(at_limit.len(), 64);
        assert!(attachments.import_bytes(&at_limit, "ok.png".into()).is_ok());
        assert!(
            attachments
                .import_file(dir.path())
                .unwrap_err()
                .ends_with("is a folder. Attach files, not folders.")
        );
        assert!(
            attachments
                .import_file(&dir.path().join("gone.png"))
                .unwrap_err()
                .starts_with("gone.png could not be read:")
        );
        let refused = attachments
            .import_bytes(b"BM\x36\0", "Pasted image.bmp".into())
            .unwrap_err();
        assert!(
            refused.contains("not a png, jpeg, gif or webp image"),
            "{refused}"
        );
    }

    /// The same image attached many times at once is copied once, whole, and every
    /// attach succeeds.
    #[test]
    fn concurrent_imports_of_one_image_all_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let attachments = Attachments::new(dir.path().join("attachments"), 1 << 20);
        let bytes = [b"\x89PNG\r\n\x1a\n".as_slice(), &[7; 200_000]].concat();
        let results: Vec<_> = std::thread::scope(|scope| {
            (0..16)
                .map(|_| scope.spawn(|| attachments.import_bytes(&bytes, "a.png".into())))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        let names: Vec<_> = fs::read_dir(attachments.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert_eq!(
            fs::read(results[0].as_ref().unwrap().path(attachments.dir())).unwrap(),
            bytes
        );
    }

    #[test]
    fn a_damaged_hash_never_names_a_file_outside_the_folder() {
        let attachment = Attachment {
            sha256: "../../etc/passwd".into(),
            media_type: "image/png".into(),
            name: "x.png".into(),
        };
        assert_eq!(
            attachment.path(Path::new("/data/attachments")),
            Path::new("/data/attachments/ecad.png")
        );
    }

    #[test]
    fn sweeping_keeps_recent_copies_and_those_a_draft_holds() {
        let (_dir, attachments) = folder();
        let old = attachments.import_bytes(PNG, "old.png".into()).unwrap();
        let held = attachments
            .import_bytes(b"GIF89a held", "held.gif".into())
            .unwrap();
        let recent = attachments
            .import_bytes(b"\xff\xd8\xff recent", "recent.jpg".into())
            .unwrap();
        let partial = attachments.dir().join(".abc.1.tmp");
        fs::write(&partial, "half").unwrap();
        let day = Duration::from_secs(24 * 60 * 60);
        let now = SystemTime::now() + 10 * day;
        fs::File::options()
            .write(true)
            .open(recent.path(attachments.dir()))
            .unwrap()
            .set_modified(now - day)
            .unwrap();
        let keep = HashSet::from([held.sha256.clone()]);
        assert_eq!(attachments.sweep(&keep, 7 * day, now).unwrap(), 2);
        assert!(!old.path(attachments.dir()).exists());
        assert!(!partial.exists());
        assert!(held.path(attachments.dir()).exists());
        assert!(recent.path(attachments.dir()).exists());
        attachments.touch(&held);
        assert_eq!(
            Attachments::new(attachments.dir().join("none"), 1)
                .sweep(&keep, day, now)
                .unwrap(),
            0
        );
    }
}
