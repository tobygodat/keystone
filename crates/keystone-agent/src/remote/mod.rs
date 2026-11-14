//! Sessions on other machines. `keystone-host` runs there: [`attach`] joins an SSH
//! session to its daemon, and [`serve`] is the daemon, which runs the provider
//! processes and journals their events. Here, a [`Link`] reaches one machine and a
//! [`RemoteConnection`] is the [`crate::Connection`] for each provider and account on
//! it. See `docs/design/remote.md`.

mod attach;
mod cache;
mod connection;
pub mod frame;
mod journal;
mod link;
mod paths;
mod reap;
mod serve;

pub use attach::attach;
pub use connection::RemoteConnection;
pub use frame::{Found, PROTOCOL, Welcome};
pub use journal::SYNCHRONOUS as JOURNAL_SYNCHRONOUS;
pub use link::{Link, NOT_CONNECTED, Status, ssh};
pub use reap::reap;
pub use serve::serve;

use keystone_core::Provider;

/// A new random ID for a workspace to give the hosts it links, as 32 hex digits.
pub fn new_client_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("the system's random source");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// An event's place: its number in the sequence of the host journal `journal` names.
/// A number from one journal says nothing about another, so a place is never compared
/// without its journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub journal: std::sync::Arc<str>,
    pub seq: u64,
}

/// Which connection on a machine an event or call belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Route {
    pub provider: Provider,
    pub account: Option<String>,
}

/// Whether the test-only fault `name` is on: `KEYSTONE_HOST_FAULTS` names a folder, and
/// a file of that name in it turns the fault on. A release build, such as one `cargo
/// install` makes, has no faults.
#[cfg(debug_assertions)]
pub(crate) fn fault(name: &str) -> bool {
    std::env::var_os("KEYSTONE_HOST_FAULTS")
        .filter(|dir| !dir.is_empty())
        .is_some_and(|dir| std::path::Path::new(&dir).join(name).exists())
}

#[cfg(not(debug_assertions))]
pub(crate) fn fault(_name: &str) -> bool {
    false
}
