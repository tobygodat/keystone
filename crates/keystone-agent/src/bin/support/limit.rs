//! A scripted usage limit, held by an account: a `.fake-usage-limit` file in the
//! account's config home holding a number of seconds. The account is at its limit
//! from the file's modification time until that many seconds later, and every turn
//! fails on the limit meanwhile. Nothing else changes, so another account's home
//! without the file works as usual.

use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

pub const FILE: &str = ".fake-usage-limit";

#[allow(dead_code)]
pub fn claude_home() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
}

#[allow(dead_code)]
pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
}

/// When the account's scripted limit resets, in unix seconds, while it has not.
pub fn limited_until(home: Option<PathBuf>) -> Option<u64> {
    let path = home?.join(FILE);
    let seconds: u64 = std::fs::read_to_string(&path).ok()?.trim().parse().ok()?;
    let set = std::fs::metadata(&path).ok()?.modified().ok()?;
    // Whole seconds, as providers report a reset, so the limit lifts at the time given.
    let reset = (set + Duration::from_secs(seconds))
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    (now < reset).then_some(reset)
}
