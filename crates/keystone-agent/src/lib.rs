//! Provider integration for Keystone, independent of the UI.
//!
//! Drivers translate a provider's wire protocol into [`Event`]s and never do IO. The
//! [`host`] module owns child processes, [`log`] stores completed items, and [`worker`]
//! keeps blocking work off the UI thread. See
//! `docs/design/providers.md`.

pub mod claude;
pub mod codex;
mod commands;
mod connection;
mod driver;
pub mod find;
pub mod host;
pub mod idle;
mod item;
pub mod log;
pub mod mcp;
mod profile;
pub mod recovery;
pub mod surface;
mod version;
pub mod worker;

pub use commands::{CommandSource, Commands, SlashCommand, terminal_only};
pub use connection::{ClaudeConnection, CodexConnection, Connection, StartHook, Wake};
pub use driver::{ApprovalState, Command, Outputs};
pub use item::{
    Approval, Catalog, Event, Item, ItemKind, ItemStatus, ModelInfo, OUTPUT_CAP, RequestId,
    TurnStatus, cap_output,
};
pub use profile::{
    LaunchProfile, default_home, default_sign_in_command, sign_in_command, terminal_command,
};
