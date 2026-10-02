//! The role an OpenCode process is started as, and the deliveries it is handed.

use std::{
    collections::BTreeMap,
    fmt, io,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
};

use async_trait::async_trait;
use radicle::cob::EntryId;

use crate::{
    agent::{
        self, Change, ChatContext, ChatMessage, Chatter, Coder, Comment, Context, Layout,
        McpEndpoint, Role, Supervised, Talker, Target,
        opencode::{
            catalogue::Catalogue,
            config::{self, Settings},
            run,
        },
        prompt,
        session::{Outcome, Session},
    },
    member::Profile,
    provider::{Credentials, Provider},
    queue::Task,
    sandbox::{AgentCommand, AgentSandbox, Output},
};

/// Name a provider file gives this adapter.
pub(crate) const ADAPTER: &str = "opencode";

// The program itself, found on the operator's `PATH` inside the sandbox.
const PROGRAM: &str = "opencode";
// Where OpenCode keeps its settings, sessions and logs. Pointed at the role's own harness
// directory, so one member's state is that member's, and so neither the operator's `AGENTS.md` nor
// their other agent applications' instructions are read as if they were the team's.
const HOME_VAR: &str = "HOME";
const CONFIG_VAR: &str = "OPENCODE_CONFIG";
// The runtime supplies this role's configuration. Project discovery would walk out of its
// checkout into directories the sandbox deliberately does not grant.
const PROJECT_CONFIG_VAR: &str = "OPENCODE_CONFIG_PROJECT_DISABLE";
// What anything run inside the sandbox downloads or builds, from OpenCode's own model registry to
// whatever a project's toolchain caches. Pointed at the member's cache directory, which its three
// roles share and which can be removed at any time.
const CACHE_VAR: &str = "XDG_CACHE_HOME";
// Belt and braces beside the configuration's own `autoupdate`, which a run reaches later than it
// reaches this.
const AUTOUPDATE_VAR: &str = "OPENCODE_DISABLE_AUTOUPDATE";

/// One role of one member, run through OpenCode.
pub(crate) struct OpenCode {
    role: Role,
    provider: Provider,
    credentials: Option<Credentials>,
    started: Option<Started>,
    session: Arc<Session>,
}

impl OpenCode {
    /// Prepares `role` to run on `provider`, reached with `credentials`.
    ///
    /// Nothing is started here. A role is configured when the runtime hands it the member it runs
    /// as, the MCP server it may use, and the directories it owns.
    pub(crate) fn new(role: Role, provider: Provider, credentials: Option<Credentials>) -> Self {
        Self {
            role,
            provider,
            credentials,
            started: None,
            session: Arc::new(Session::new(role)),
        }
    }

    // Configures the role and proves OpenCode can run it before any work arrives.
    async fn begin(
        &mut self,
        profile: Profile,
        mcp: &McpEndpoint,
        sandbox: AgentSandbox,
        layout: Layout,
    ) -> Result<(), Error> {
        let mut settings = Settings::read(&self.provider, profile.tuning)?;
        let (harness, environment) = self.prepare(&sandbox, &layout).await?;
        // Asked before the file is written, because what OpenCode offers decides both whether this
        // member can run at all and which effort its model can be asked for. Boxed because the
        // query owns its command environment, and the runtime starts three roles.
        settings.fit(
            &Box::pin(Catalogue::query(
                Self::arranged(&environment, sandbox.command(PROGRAM)),
                settings.model(),
            ))
            .await?,
        );
        settings.write(&harness.join(config::FILE), profile, &layout, mcp)?;
        tracing::info!(
            role = %self.role,
            member = profile.member.id(),
            model = settings.model(),
            harness = %harness.display(),
            "OpenCode role started"
        );
        self.started = Some(Started {
            agent: Settings::agent(profile),
            model: settings.selection(),
            environment,
            sandbox,
        });

        Ok(())
    }

    // Makes the role's harness home ready to run in, and reports it with what runs there.
    //
    // The plugin goes in before model discovery or deliveries. A run without it looks silent from
    // out here, whatever the agent is doing inside.
    async fn prepare(
        &self,
        sandbox: &AgentSandbox,
        layout: &Layout,
    ) -> Result<(PathBuf, BTreeMap<String, String>), Error> {
        let (harness, environment) = self.prepare_home(layout)?;
        Self::present(Self::arranged(&environment, sandbox.command(PROGRAM))).await?;
        config::install_plugin(&harness)?;
        self.session.reports_in(config::report(&harness));
        Ok((harness, environment))
    }

    // Resolves the role's private directories before starting any processes.
    fn prepare_home(&self, layout: &Layout) -> Result<(PathBuf, BTreeMap<String, String>), Error> {
        // A role given another role's directories would read another role's instructions and write
        // into a working copy which is not its own.
        if layout.role() != self.role {
            return Err(Error::Role {
                expected: self.role,
                given: layout.role(),
            });
        }
        let harness = config::prepare(layout.harness())?;
        let environment = self.environment(&harness, &harness.join(config::FILE), layout)?;

        Ok((harness, environment))
    }

    // Runs one delivery and reports how it ended.
    async fn deliver(
        &self,
        prompt: String,
        workspace: &Path,
        title: &str,
    ) -> Result<Outcome, Error> {
        let started = self.started.as_ref().ok_or(Error::NotStarted(self.role))?;

        run::supervise(
            &self.session,
            Self::arranged(&started.environment, started.sandbox.command(PROGRAM))
                .current_dir(workspace)
                .args(["run", "--standalone", "--auto", "--format", "json"])
                .args(["--agent", &started.agent])
                .args(["--model", &started.model])
                .args(["--title", title])
                .arg(prompt),
        )
        .await
    }

    // Runs one delivery which is expected to finish on its own.
    //
    // The talking and chatting roles have nothing to preserve when they are stopped: what they
    // produce is the answer they publish, and an answer half-written is not one.
    async fn answer(&self, prompt: String, workspace: &Path, title: &str) -> Result<(), Error> {
        match self.deliver(prompt, workspace, title).await? {
            Outcome::Finished => Ok(()),
            Outcome::Stopped => Err(Error::Stopped(self.role)),
        }
    }

    // Asks a fresh run to commit whatever the stopped task left in the working copy.
    async fn preserve(&self, task: &Task, workspace: &Path) -> Result<(), Error> {
        tracing::info!(
            task = task.id,
            "asking the stopped task to commit what it reached"
        );
        match self
            .deliver(prompt::stopping(task), workspace, &title(task))
            .await?
        {
            Outcome::Finished => Ok(()),
            // The runtime ran out of patience with the second run too. What is in the tree is still
            // there, and preparing the next task commits it to a salvage branch.
            Outcome::Stopped => {
                tracing::warn!(
                    task = task.id,
                    "the stopped task was ended before it committed; its work stays in the tree"
                );

                Ok(())
            }
        }
    }

    // Check the required CLI before installing a plugin or attempting model discovery.
    async fn present(command: AgentCommand) -> Result<(), Error> {
        let output = Box::pin(command.arg("--version").output()).await?;

        Self::check_version(&output)
    }

    fn check_version(output: &Output) -> Result<(), Error> {
        if !output.status.success() {
            return Err(Error::Unavailable {
                status: output.status,
                detail: output.stderr.trim().to_owned(),
            });
        }
        let version = output.stdout.trim();
        if !version.starts_with("opencode v2.") {
            return Err(Error::Version(version.to_owned()));
        }

        Ok(())
    }

    // Gives a command the environment this role's OpenCode runs in, credentials included.
    fn arranged(environment: &BTreeMap<String, String>, command: AgentCommand) -> AgentCommand {
        environment
            .iter()
            .fold(command, |command, (key, value)| command.env(key, value))
    }

    fn environment(
        &self,
        harness: &Path,
        configuration: &Path,
        layout: &Layout,
    ) -> Result<BTreeMap<String, String>, Error> {
        let cache = config::prepare(layout.cache())?;
        let data = config::prepare(&layout.provider_state("opencode").join("data"))?;
        let mut environment = BTreeMap::from([
            (HOME_VAR.to_owned(), path(harness)),
            (CONFIG_VAR.to_owned(), path(configuration)),
            (PROJECT_CONFIG_VAR.to_owned(), "1".to_owned()),
            (CACHE_VAR.to_owned(), path(&cache)),
            ("XDG_DATA_HOME".to_owned(), path(&data)),
            (AUTOUPDATE_VAR.to_owned(), "1".to_owned()),
        ]);
        if let Some(credentials) = &self.credentials {
            // Named, never rendered: these are the operator's API keys.
            tracing::debug!(
                role = %self.role,
                way = %credentials.way(),
                variables = ?credentials.variables().keys(),
                "authorizing OpenCode"
            );
            environment.extend(credentials.variables().clone());
        }

        Ok(environment)
    }

    // Stops whatever is running and forgets what the role was configured with.
    async fn end(&mut self) -> Result<(), agent::Error> {
        self.session.abort().await?;
        self.started = None;

        Ok(())
    }
}

impl fmt::Debug for OpenCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenCode")
            .field("role", &self.role)
            .field("started", &self.started.is_some())
            .finish_non_exhaustive()
    }
}

impl Supervised for OpenCode {
    fn supervisor(&self) -> Arc<Session> {
        Arc::clone(&self.session)
    }
}

#[async_trait]
impl Talker for OpenCode {
    async fn start(
        &mut self,
        profile: Profile,
        mcp: &McpEndpoint,
        sandbox: AgentSandbox,
        layout: Layout,
    ) -> Result<(), agent::Error> {
        Box::pin(self.begin(profile, mcp, sandbox, layout))
            .await
            .map_err(agent::Error::new)
    }

    async fn mentioned(&mut self, context: Context, target: Target) -> Result<(), agent::Error> {
        self.answer(
            prompt::mentioned(&context, target),
            &context.workspace,
            &context.subject.to_string(),
        )
        .await
        .map_err(agent::Error::new)
    }

    async fn replied(
        &mut self,
        context: Context,
        comment: Comment,
        reply_to: EntryId,
    ) -> Result<(), agent::Error> {
        self.answer(
            prompt::replied(&context, comment, reply_to),
            &context.workspace,
            &context.subject.to_string(),
        )
        .await
        .map_err(agent::Error::new)
    }

    async fn changed(&mut self, context: Context, change: Change) -> Result<(), agent::Error> {
        self.answer(
            prompt::changed(&context, change),
            &context.workspace,
            &context.subject.to_string(),
        )
        .await
        .map_err(agent::Error::new)
    }

    async fn shutdown(&mut self) -> Result<(), agent::Error> {
        self.end().await
    }
}

#[async_trait]
impl Chatter for OpenCode {
    async fn start(
        &mut self,
        profile: Profile,
        mcp: &McpEndpoint,
        sandbox: AgentSandbox,
        layout: Layout,
    ) -> Result<(), agent::Error> {
        Box::pin(self.begin(profile, mcp, sandbox, layout))
            .await
            .map_err(agent::Error::new)
    }

    async fn messaged(
        &mut self,
        context: ChatContext,
        message: ChatMessage,
    ) -> Result<(), agent::Error> {
        self.answer(
            prompt::messaged(&context, &message),
            context.workspace(),
            &format!("chat:{}", context.channel()),
        )
        .await
        .map_err(agent::Error::new)
    }

    async fn shutdown(&mut self) -> Result<(), agent::Error> {
        self.end().await
    }
}

#[async_trait]
impl Coder for OpenCode {
    async fn start(
        &mut self,
        profile: Profile,
        mcp: &McpEndpoint,
        sandbox: AgentSandbox,
        layout: Layout,
    ) -> Result<(), agent::Error> {
        Box::pin(self.begin(profile, mcp, sandbox, layout))
            .await
            .map_err(agent::Error::new)
    }

    async fn work(&self, task: Task, workspace: PathBuf) -> Result<(), agent::Error> {
        let carried = self
            .deliver(prompt::task(&task), &workspace, &title(&task))
            .await
            .map_err(agent::Error::new)?;
        match carried {
            Outcome::Finished => Ok(()),
            // End the CLI delivery, then ask a fresh run to commit what the stopped task reached.
            Outcome::Stopped if self.session.was_interrupted() => self
                .preserve(&task, &workspace)
                .await
                .map_err(agent::Error::new),
            Outcome::Stopped => Err(agent::Error::new(Error::Stopped(self.role))),
        }
    }

    async fn interrupt(&self) -> Result<(), agent::Error> {
        self.session.interrupt();

        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), agent::Error> {
        self.end().await
    }
}

/// What one role was configured with, once the runtime has started it.
struct Started {
    agent: String,
    model: String,
    environment: BTreeMap<String, String>,
    sandbox: AgentSandbox,
}

// Never renders the environment: it carries the provider's API key.
impl fmt::Debug for Started {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Started")
            .field("agent", &self.agent)
            .field("environment", &self.environment.keys())
            .finish_non_exhaustive()
    }
}

/// Error running a member's role through OpenCode.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// The provider file does not describe a member this crate can run.
    #[error(transparent)]
    Controls(#[from] crate::agent::controls::Error),
    /// OpenCode does not offer the configured model.
    #[error(
        "OpenCode does not offer '{0}'; check the name against models.dev and that this provider \
         is authorized, with `rad-team provider login`"
    )]
    Model(String),
    /// OpenCode itself could not be run.
    #[error(
        "could not run `opencode` ({status}): {detail}; install OpenCode \
         (https://opencode.ai/docs) so it is on this member's PATH"
    )]
    Unavailable {
        /// How the listing ended.
        status: ExitStatus,
        /// What it wrote about it.
        detail: String,
    },
    /// The CLI has changed beyond the interfaces supported here.
    #[error("unsupported OpenCode version '{0}'; install OpenCode 2")]
    Version(String),
    /// A model API response could not be decoded.
    #[error("could not read OpenCode's model catalogue: {0}")]
    Catalogue(#[from] serde_json::Error),
    /// The CLI could not obtain a usable model catalogue.
    #[error("could not query OpenCode's models ({status}): {detail}")]
    CatalogueUnavailable {
        /// How the model query exited.
        status: ExitStatus,
        /// The CLI or API's explanation.
        detail: String,
    },
    /// A role's harness or cache directory could not be prepared.
    #[error("could not prepare '{path}': {source}")]
    Harness {
        /// Directory or file being prepared.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// The configuration could not be written.
    #[error("could not build the OpenCode configuration '{path}': {reason}")]
    Configuration {
        /// Configuration file.
        path: PathBuf,
        /// Why it could not be built.
        reason: String,
    },
    /// A role was given the directories of another role.
    #[error("the {expected} role was started with the {given} role's directories")]
    Role {
        /// Role this adapter was created for.
        expected: Role,
        /// Role the runtime started it as.
        given: Role,
    },
    /// A role was given work before it was started.
    #[error("the {0} role was given work before it was started")]
    NotStarted(Role),
    /// Reading what a run wrote failed.
    #[error("could not read what the {role} role's OpenCode wrote: {source}")]
    Output {
        /// Role whose output could not be read.
        role: Role,
        /// Underlying stream error.
        #[source]
        source: io::Error,
    },
    /// A run was stopped before it was done.
    #[error("the {0} role was stopped before it finished")]
    Stopped(Role),
    /// A run ended in failure. Whatever was said about it decides before the status does, and an
    /// exit code alone would call some failed runs successes.
    #[error("opencode failed (ended with {status}): {detail}")]
    Failed {
        /// How the process exited.
        status: ExitStatus,
        /// What it reported about the failure.
        detail: String,
    },
    /// Starting or running a sandboxed process failed.
    #[error(transparent)]
    Sandbox(#[from] crate::sandbox::Error),
}

// Returns the name a task's runs are recorded under, so a transcript can be found again.
fn title(task: &Task) -> String {
    format!("task:{}", task.id)
}

fn path(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use crate::{
        agent::{
            Layout, Role,
            opencode::{
                config,
                harness::{Error, OpenCode, path},
                tests::{REASONING, provider},
            },
        },
        provider::Credentials,
        sandbox::Output,
    };

    #[test]
    fn requires_opencode_two() {
        let mut output = Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(0),
            stdout: "opencode v2.0.13\n".to_owned(),
            stderr: String::new(),
        };
        OpenCode::check_version(&output).expect("supported version");
        for version in ["1.18.32", "opencode v3.0.0", "unrecognized output"] {
            output.stdout = version.to_owned();
            assert!(
                matches!(OpenCode::check_version(&output), Err(Error::Version(given)) if given == version)
            );
        }
    }

    #[test]
    fn rejects_a_failed_version_probe() {
        let output = Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(1 << 8),
            stdout: "opencode v2.0.13".to_owned(),
            stderr: " startup failed\n".to_owned(),
        };
        let error = OpenCode::check_version(&output).expect_err("failed probe");
        assert!(error.to_string().contains("install OpenCode"));
        assert!(matches!(error, Error::Unavailable { status, detail }
            if status.code() == Some(1) && detail == "startup failed"));
    }

    #[test]
    fn prepares_private_directories_and_supplies_only_the_roles_credentials() {
        let temporary = tempfile::tempdir().expect("member home");
        let layout = layout(temporary.path(), Role::Talker);
        let credentials = Credentials::key(BTreeMap::from([(
            "PROVIDER_API_KEY".to_owned(),
            "test-secret".to_owned(),
        )]));
        let adapter = adapter("prepare-home", Some(credentials));

        let (harness, environment) = adapter.prepare_home(&layout).expect("private home");
        assert_eq!(
            harness,
            layout.harness().canonicalize().expect("resolved home")
        );
        assert_eq!(environment["HOME"], path(&harness));
        assert_eq!(
            environment["OPENCODE_CONFIG"],
            path(&harness.join(config::FILE))
        );
        assert_eq!(
            environment["XDG_CACHE_HOME"],
            path(&layout.cache().canonicalize().expect("cache"))
        );
        assert_eq!(environment["OPENCODE_DISABLE_AUTOUPDATE"], "1");
        assert_eq!(environment["OPENCODE_CONFIG_PROJECT_DISABLE"], "1");
        assert_eq!(environment["PROVIDER_API_KEY"], "test-secret");
        assert_eq!(environment.len(), 7);
        assert!(!format!("{adapter:?}").contains("test-secret"));
    }

    #[test]
    fn prepares_a_subscription_without_inventing_an_api_key() {
        let temporary = tempfile::tempdir().expect("member home");
        let (_, environment) = adapter("prepare-subscription", Some(Credentials::subscription()))
            .prepare_home(&layout(temporary.path(), Role::Talker))
            .expect("subscription home");

        assert_eq!(environment.len(), 6);
        assert!(!environment.contains_key("PROVIDER_API_KEY"));
    }

    #[test]
    fn rejects_another_role_before_creating_its_directories() {
        let temporary = tempfile::tempdir().expect("member home");
        let layout = layout(temporary.path(), Role::Coder);

        assert!(matches!(
            adapter("prepare-wrong-role", None).prepare_home(&layout),
            Err(Error::Role {
                expected: Role::Talker,
                given: Role::Coder
            })
        ));
        assert!(!layout.harness().exists());
        assert!(!layout.cache().exists());
    }

    #[test]
    fn reports_the_directory_that_cannot_be_prepared() {
        for directory in ["harness", "cache"] {
            let temporary = tempfile::tempdir().expect("member home");
            let blocked = temporary.path().join(directory);
            std::fs::write(&blocked, "occupied").expect("blocker");
            let error = adapter(&format!("prepare-blocked-{directory}"), None)
                .prepare_home(&layout(temporary.path(), Role::Talker))
                .expect_err("a file cannot serve as a directory");

            assert!(matches!(error, Error::Harness { path, .. } if path == blocked));
            assert_eq!(
                std::fs::read_to_string(&blocked).expect("preserved blocker"),
                "occupied"
            );
        }
    }

    fn adapter(name: &str, credentials: Option<Credentials>) -> OpenCode {
        OpenCode::new(Role::Talker, provider(name, REASONING), credentials)
    }

    fn layout(root: &Path, role: Role) -> Layout {
        Layout::new(role, root.join("cache"), root.join("harness"), Vec::new())
    }
}
