//! The host's cache of images the app sent, kept by the SHA-256 of their bytes, so an
//! image a session sends again isn't uploaded again. `put_attachment` writes one in
//! parts, and `send` names the images it carries by hash.

use super::frame::{Attached, CHUNK};
use super::paths::private_dir;
use crate::Image;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// The largest image the cache takes, past Keystone's own attachment limit.
const MAX_IMAGE: u64 = 64 * 1024 * 1024;
/// How long an image stays once no message has sent it.
const KEEP: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// How long a cut-off upload's part stays.
const KEEP_PART: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) const GONE: &str = "An image in this message isn't in the machine's cache any more, so the message wasn't sent. Send it again";

pub(crate) struct Cache {
    dir: PathBuf,
}

/// The file extension the CLIs read an image's type from.
fn extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn check_hash(sha256: &str) -> Result<(), String> {
    if sha256.len() == 64
        && sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(format!("{sha256:?} isn't a SHA-256"))
    }
}

impl Cache {
    /// The cache in `dir`, private to this user, with images no message sent for
    /// [`KEEP`] removed.
    pub fn open(dir: PathBuf) -> Result<Self, String> {
        private_dir(&dir)?;
        let cache = Self { dir };
        cache.prune();
        Ok(cache)
    }

    fn file(&self, attached: &Attached) -> Result<PathBuf, String> {
        check_hash(&attached.sha256)?;
        let extension = extension(&attached.media_type)
            .ok_or_else(|| format!("{} isn't an image type Keystone sends", attached.media_type))?;
        Ok(self.dir.join(format!("{}.{extension}", attached.sha256)))
    }

    /// Writes part of an image at `offset`. The part that ends the image is checked
    /// against its hash before the image is kept. An image already kept takes no
    /// part again.
    pub fn put(
        &self,
        attached: &Attached,
        size: u64,
        offset: u64,
        data: &str,
    ) -> Result<(), String> {
        let file = self.file(attached)?;
        if size > MAX_IMAGE {
            return Err(format!("A {size} byte image is over the machine's limit"));
        }
        if file.exists() {
            return Ok(());
        }
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
            .map_err(|e| format!("An image part isn't base64: {e}"))?;
        let end = offset + bytes.len() as u64;
        if bytes.len() > CHUNK || end > size || (bytes.is_empty() && size > 0) {
            return Err("An image part doesn't fit its image".into());
        }
        let part = self.dir.join(format!("{}.part", attached.sha256));
        let mut options = File::options();
        options.write(true).create(true).mode(0o600);
        if offset == 0 {
            options.truncate(true);
        } else {
            options.append(true);
        }
        let mut writing = options
            .open(&part)
            .map_err(|e| format!("Could not save an image: {e}"))?;
        let have = writing.metadata().map_err(|e| e.to_string())?.len();
        if have != offset {
            let _ = fs::remove_file(&part);
            return Err(
                "An image's upload was cut off. Send the message again to send it whole".into(),
            );
        }
        writing
            .write_all(&bytes)
            .and_then(|()| writing.sync_all())
            .map_err(|e| format!("Could not save an image: {e}"))?;
        if end < size {
            return Ok(());
        }
        let mut hasher = Sha256::new();
        let mut reading = File::open(&part).map_err(|e| e.to_string())?;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = reading.read(&mut buffer).map_err(|e| e.to_string())?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        if hex(&hasher.finalize()) != attached.sha256 {
            let _ = fs::remove_file(&part);
            return Err("An image's bytes don't match its hash, so it wasn't kept".into());
        }
        fs::rename(&part, &file).map_err(|e| format!("Could not keep an image: {e}"))
    }

    /// The kept image `attached` names, for a provider to read, marked as used now.
    pub fn image(&self, attached: &Attached) -> Result<Image, String> {
        let path = self.file(attached)?;
        let file = File::options()
            .write(true)
            .open(&path)
            .map_err(|_| GONE.to_owned())?;
        let _ = file.set_modified(SystemTime::now());
        Ok(Image {
            path,
            media_type: attached.media_type.clone(),
            base64: None,
        })
    }

    /// Removes images no message sent within [`KEEP`], and parts of uploads cut off
    /// more than [`KEEP_PART`] ago.
    fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            let keep = if path.extension().is_some_and(|e| e == "part") {
                KEEP_PART
            } else {
                KEEP
            };
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > keep);
            if old {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attached(bytes: &[u8]) -> Attached {
        Attached {
            media_type: "image/png".into(),
            sha256: hex(&Sha256::digest(bytes)),
        }
    }

    fn b64(bytes: &[u8]) -> String {
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
    }

    #[test]
    fn an_image_sent_in_parts_is_kept_once_its_bytes_match_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("attachments")).unwrap();
        let bytes = b"0123456789";
        let image = attached(bytes);
        assert_eq!(cache.image(&image).unwrap_err(), GONE);
        cache.put(&image, 10, 0, &b64(&bytes[..4])).unwrap();
        assert!(cache.image(&image).is_err(), "not kept until whole");
        cache.put(&image, 10, 4, &b64(&bytes[4..])).unwrap();
        let kept = cache.image(&image).unwrap();
        assert_eq!(std::fs::read(&kept.path).unwrap(), bytes);
        assert!(kept.path.to_string_lossy().ends_with(".png"));
        cache.put(&image, 10, 0, &b64(b"x")).unwrap();
        assert_eq!(std::fs::read(&kept.path).unwrap(), bytes, "kept once");
    }

    #[test]
    fn a_part_out_of_place_or_bytes_off_their_hash_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path().join("attachments")).unwrap();
        let image = attached(b"abcd");
        assert!(
            cache.put(&image, 4, 2, &b64(b"cd")).is_err(),
            "no first part"
        );
        assert!(
            cache.put(&image, 4, 0, &b64(b"abcx")).is_err(),
            "wrong hash"
        );
        assert!(cache.image(&image).is_err());
        let bad = Attached {
            media_type: "image/png".into(),
            sha256: "../../etc/passwd".into(),
        };
        assert!(cache.put(&bad, 1, 0, &b64(b"a")).is_err());
        let svg = Attached {
            media_type: "image/svg+xml".into(),
            ..attached(b"a")
        };
        assert!(cache.put(&svg, 1, 0, &b64(b"a")).is_err());
    }
}
