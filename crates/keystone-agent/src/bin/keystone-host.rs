//! `keystone-host`: runs Keystone sessions on this machine for an app on another one.
//! `attach` is what the app runs over SSH, `serve` is the daemon `attach` starts, and
//! `reap` is the watcher `serve` starts beside each provider process.
//! See `docs/design/remote.md`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let result = match std::env::args().nth(1).as_deref() {
        Some("serve") => keystone_agent::remote::serve(),
        Some("attach") => keystone_agent::remote::attach(),
        Some("reap") => keystone_agent::remote::reap(),
        Some("--version") => {
            println!(
                "keystone-host {} (protocol {})",
                env!("CARGO_PKG_VERSION"),
                keystone_agent::remote::PROTOCOL
            );
            Ok(())
        }
        _ => {
            eprintln!("usage: keystone-host serve | attach | --version");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("keystone-host: {reason}");
            ExitCode::FAILURE
        }
    }
}
