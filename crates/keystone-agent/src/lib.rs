//! Provider integration for Keystone, independent of the UI.
//!
//! Drivers translate a provider's wire protocol into [`Event`]s and never do IO. The
//! [`host`] module owns child processes, [`log`] stores completed items, and [`worker`]
//! keeps blocking work off the UI thread. See
//! `docs/design/providers.md`.

pub mod codex;
mod connection;
pub mod find;
pub mod host;
mod item;
pub mod log;
pub mod worker;

pub use connection::{CodexConnection, Wake};
pub use item::{
    Approval, Event, Item, ItemKind, ItemStatus, OUTPUT_CAP, RequestId, TurnStatus, cap_output,
};
