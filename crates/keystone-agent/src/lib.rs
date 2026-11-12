//! Provider integration for Keystone, independent of the UI.
//!
//! Drivers translate a provider's wire protocol into [`Event`]s and never do IO. The
//! [`host`] module owns child processes, [`log`] stores completed items, [`worker`]
//! keeps blocking work off the UI thread, and [`git`] and [`github`] run the user's
//! own git and gh, and [`remote`] runs sessions on other machines through
//! `keystone-host`. See `docs/design/providers.md`.

pub mod browser;
pub mod checkpoint;
pub mod claude;
pub mod codex;
mod commands;
mod connection;
pub mod diff;
mod driver;
pub mod find;
pub mod git;
pub mod github;
pub mod host;
pub mod idle;
pub mod import;
mod item;
pub mod log;
pub mod mcp;
mod profile;
mod question;
pub mod recovery;
pub mod remote;
#[cfg(unix)]
pub mod setup;
pub mod surface;
mod version;
pub mod worker;

/// Hand off's shortcut, which a failed fork names. The desktop binds it as
/// `secondary-shift-h`.
pub const HAND_OFF_KEYS: &str = if cfg!(target_os = "macos") {
    "⇧⌘H"
} else {
    "Ctrl+Shift+H"
};

pub use commands::{CommandSource, Commands, SlashCommand, terminal_only};
pub use connection::{ClaudeConnection, CodexConnection, Connection, SpawnHook, StartHook, Wake};
pub use driver::{ApprovalState, Command, Image, Outputs};

/// The speed value that runs `provider` at standard speed, whatever model it runs.
pub fn standard_speed(provider: keystone_core::Provider) -> &'static str {
    match provider {
        keystone_core::Provider::Claude => claude::STANDARD_SPEED,
        keystone_core::Provider::Codex => codex::STANDARD_TIER,
    }
}
pub use item::{
    Approval, Catalog, Event, Item, ItemKind, ItemStatus, ModelInfo, OUTPUT_CAP, RequestId, Speed,
    TurnStatus, cap_output,
};
pub use profile::{
    ConfigEnv, LaunchProfile, default_home, default_sign_in_command, run_config_cli,
    sign_in_command, terminal_command,
};
pub use question::{Answer, Bounds, Choice, Field, Input, Question};
