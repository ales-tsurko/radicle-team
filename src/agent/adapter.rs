//! Choosing which agent application runs a member's roles.
//!
//! A provider file names an adapter, and the name is matched here against the adapters this build
//! has. Adding one is a module beside `opencode` and an arm in [`select`]; nothing else in the
//! crate knows which agent applications exist, because the roles reach the runtime as [`Talker`],
//! [`Chatter`] and [`Coder`] and it has never needed to know which harness is behind one.

use crate::{
    agent::{Chatter, Coder, Role, Talker, claude_code, opencode},
    provider::{Credentials, Provider},
};

/// Every agent application this build can run a member on.
pub const ADAPTERS: &[&str] = &[claude_code::ADAPTER, opencode::ADAPTER];

/// One member's three roles, on whichever agent application its provider named.
pub(crate) struct Roles {
    /// Answers repository activity.
    pub(crate) talker: Box<dyn Talker>,
    /// Answers authenticated chat.
    pub(crate) chatter: Box<dyn Chatter>,
    /// Carries out queued work.
    pub(crate) coder: Box<dyn Coder>,
}

impl std::fmt::Debug for Roles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Roles").finish_non_exhaustive()
    }
}

/// Builds one role of each kind on the agent application called `adapter`.
///
/// Three separate agents rather than one addressed three ways: each has its own harness state, its
/// own working copy and its own MCP server, and the runtime stops them one at a time.
///
/// # Errors
///
/// Returns an error when no adapter of that name is in this build.
pub(crate) fn select(
    adapter: &str,
    provider: &Provider,
    credentials: Option<&Credentials>,
) -> Result<Roles, Unknown> {
    match adapter {
        opencode::ADAPTER => Ok(roles(|role| {
            opencode::OpenCode::new(role, provider.clone(), credentials.cloned())
        })),
        claude_code::ADAPTER => Ok(roles(|role| {
            claude_code::ClaudeCode::new(role, provider.clone(), credentials.cloned())
        })),
        _ => Err(Unknown {
            adapter: adapter.to_owned(),
        }),
    }
}

// Builds one of each role on the agent application the given constructor makes them for.
fn roles<T>(build: impl Fn(Role) -> T) -> Roles
where
    T: Talker + Chatter + Coder + 'static,
{
    Roles {
        talker: Box::new(build(Role::Talker)),
        chatter: Box::new(build(Role::Chatter)),
        coder: Box::new(build(Role::Coder)),
    }
}

/// A provider names an agent application this build has no adapter for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no adapter for '{adapter}' in this build; it has {}", ADAPTERS.join(", "))]
pub struct Unknown {
    /// Agent application the provider named.
    adapter: String,
}

#[cfg(test)]
mod tests {
    use crate::{
        agent::adapter::{ADAPTERS, select},
        provider,
    };

    // The name in a provider file is matched against what this build has, so the two lists cannot
    // drift apart without a member failing to start on a provider which reads perfectly well.
    #[cfg(not(miri))]
    #[test]
    fn builds_roles_for_every_adapter_it_lists() {
        let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("adapter-select");
        let _removed = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("test directory should be created");

        for adapter in ADAPTERS {
            std::fs::write(
                provider::path(&directory, adapter),
                format!(
                    "adapter = \"{adapter}\"\n\n[auth]\nways = [\"none\"]\n\n[INT]\nparam = \
                     \"model\"\nvalues = [\"p/a\", \"p/a\", \"p/a\", \"p/a\", \"p/a\"]\n"
                ),
            )
            .expect("provider should be written");
            let read = provider::find(&directory, adapter).expect("provider should be read");

            assert!(
                select(read.adapter(), &read, None).is_ok(),
                "'{adapter}' is offered but cannot be built"
            );
        }
    }

    #[test]
    fn names_what_it_has_when_it_has_no_adapter_for_a_provider() {
        let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("adapter-unknown");
        let _removed = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("test directory should be created");
        std::fs::write(
            provider::path(&directory, "elsewhere"),
            "adapter = \"elsewhere\"\n\n[auth]\nways = [\"none\"]\n",
        )
        .expect("provider should be written");
        let read = provider::find(&directory, "elsewhere").expect("provider should be read");

        let error = select(read.adapter(), &read, None)
            .expect_err("an adapter this build does not have must be refused");

        assert!(
            ADAPTERS
                .iter()
                .all(|adapter| error.to_string().contains(adapter)),
            "the operator has to be told which agent applications they can pick from: {error}"
        );
    }
}
