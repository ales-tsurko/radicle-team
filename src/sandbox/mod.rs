//! Host-local process sandboxing for agent adapters.
//!
//! [`AgentSandbox`] combines Zerobox's platform defaults with the selected member's local
//! [`Config`]. Adapters may select and parameterize a command, but cannot change its filesystem or
//! network permissions.

#[cfg(target_os = "linux")]
pub use crate::sandbox::linux::Error as LinuxHelperError;
pub use crate::sandbox::{
    command::{AgentChild, AgentCommand, AgentSandbox, Error},
    config::{Config, ConfigError, NetworkAccess},
};

mod command;
mod config;
#[cfg(target_os = "linux")]
mod linux;
mod network;
mod policy;

/// Dispatches Zerobox's embedded Linux helper before argument parsing or thread creation.
///
/// Every executable that calls [`Application::run_with_agent`](crate::Application::run_with_agent)
/// must call this as the first statement in `main`. It is a no-op on other platforms.
pub fn dispatch_linux_helper() {
    #[cfg(target_os = "linux")]
    linux::dispatch();
}
