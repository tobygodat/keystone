//! The programs a test gives `AppState::test_programs`. A test build starts no provider
//! connection without them, so no test runs the `claude` or `codex` on `PATH`, which on
//! a developer's machine are the user's own (TOB-363).

use std::path::PathBuf;

/// The scripted `keystone-fake-codex` and `keystone-fake-claude`, as `(codex, claude)`.
/// `scripts/check.sh ui` builds them first.
pub(crate) fn scripted() -> (PathBuf, PathBuf) {
    let built = keystone_core::test_support::built_binary;
    (built("keystone-fake-codex"), built("keystone-fake-claude"))
}

/// Programs that are not there, as `(codex, claude)`, for a test that shows sessions but
/// never talks to a provider. Every start fails the way it does when neither CLI is
/// installed, such as Claude's catalog when a new session shows.
pub(crate) fn no_providers() -> (PathBuf, PathBuf) {
    (
        PathBuf::from("/nonexistent/keystone-test-codex"),
        PathBuf::from("/nonexistent/keystone-test-claude"),
    )
}

/// `crate::install_app_state` for a test whose connections run `programs`, set before
/// any window opens and asks for a catalog.
pub(crate) fn install(
    store: keystone_core::Store,
    programs: (PathBuf, PathBuf),
    cx: &mut gpui::App,
) -> gpui::Entity<crate::AppState> {
    let state = crate::install_app_state(store, cx);
    state.update(cx, |state, _| state.test_programs = Some(programs));
    state
}
