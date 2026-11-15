//! Whether this process may search `PATH` for a provider CLI. On a developer's machine
//! the `claude` and `codex` found there are the user's own, with their logins and
//! config, so test setup turns the search off (TOB-363). Only test code turns it off:
//! `test_support::built_binary`, which finds the scripted providers, and the desktop's
//! test builds. The app never does, so the app always searches, whatever its binary is
//! named and whichever features it was built with.

use std::sync::atomic::{AtomicBool, Ordering};

static REFUSED: AtomicBool = AtomicBool::new(false);

/// Turns the search off for the rest of this process.
pub fn refuse() {
    REFUSED.store(true, Ordering::Relaxed);
}

/// Whether test setup turned the search off.
pub fn refused() -> bool {
    REFUSED.load(Ordering::Relaxed)
}
