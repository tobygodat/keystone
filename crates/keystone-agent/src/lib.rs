//! Provider integration for Keystone, independent of the UI.
//!
//! Drivers translate a provider's wire protocol into [`Event`]s and never do IO. The
//! [`host`] module owns child processes, and [`log`] stores completed items. See
//! `docs/design/providers.md`.

pub mod codex;
mod connection;
pub mod find;
pub mod host;
mod item;
pub mod log;

pub use connection::{CodexConnection, Wake};
pub use item::{
    Approval, Event, Item, ItemKind, ItemStatus, OUTPUT_CAP, RequestId, TurnStatus, cap_output,
};
