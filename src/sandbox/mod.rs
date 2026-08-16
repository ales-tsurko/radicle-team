//! Host-local process sandboxing for agent adapters.
//!
//! [`AgentSandbox`] combines Zerobox's platform defaults with the selected member's local
//! [`Config`]. Adapters may select and parameterize a command, but cannot change its filesystem or
//! network permissions.
//!
//! Linux and macOS only, and the build refuses elsewhere rather than running without one. An agent
//! is given unrestricted permissions on purpose -- there is no mechanism for it to ask a person
//! before acting, so nothing would ever be confirmed and it would stall. The sandbox is what
//! confines it instead, which makes a build with no sandbox a build with no confinement at all.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "radicle-team supports Linux and macOS only: it runs every agent inside a sandbox (bubblewrap \
     on Linux, Seatbelt on macOS), and agents are given unrestricted permissions because that \
     sandbox is the only thing confining them. There is no sandbox on this platform, so a build \
     here would run agents unconfined."
);

#[cfg(target_os = "linux")]
pub use crate::sandbox::linux::Error as LinuxHelperError;
pub use crate::sandbox::{
    command::{AgentChild, AgentCommand, AgentSandbox, Error, Output},
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
/// Every executable that calls `Application::run_with_agent` must call this as the first statement
/// in `main`. It is a no-op on other platforms.
pub fn dispatch_linux_helper() {
    #[cfg(target_os = "linux")]
    linux::dispatch();
}
