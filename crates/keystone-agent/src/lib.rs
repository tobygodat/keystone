//! Provider integration for Keystone, independent of the UI.
//!
//! Drivers translate a provider's wire protocol into [`Event`]s and never do IO. The
//! [`host`] module owns child processes, [`log`] stores completed items, and [`worker`]
//! keeps blocking work off the UI thread. See
//! `docs/design/providers.md`.

pub mod claude;
pub mod codex;
mod connection;
mod driver;
pub mod find;
pub mod host;
mod item;
pub mod log;
pub mod mcp;
mod version;
pub mod worker;

pub use connection::{ClaudeConnection, CodexConnection, Connection, StartHook, Wake};
pub use driver::{ApprovalState, Command, Outputs};
pub use item::{
    Approval, Catalog, Event, Item, ItemKind, ItemStatus, ModelInfo, OUTPUT_CAP, RequestId,
    TurnStatus, cap_output,
};
