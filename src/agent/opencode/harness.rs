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
    provider::{Credentials, Provider, Way},
    queue::Job,
    sandbox::{AgentCommand, AgentSandbox},
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
        let (harness, started) = self.prepare(profile, sandbox, &layout)?;
        // Asked before the file is written, because what OpenCode offers decides both whether this
        // member can run at all and which effort its model can be asked for. Boxed because the
        // future holds the whole started role, and the runtime starts three of them.
        settings.fit(&Box::pin(self.catalogue(&started, settings.model())).await?);
        settings.write(&harness.join(config::FILE), profile, &layout, mcp)?;
        tracing::info!(
            role = %self.role,
            member = profile.member.id(),
            model = settings.model(),
            harness = %harness.display(),
            "OpenCode role started"
        );
        self.started = Some(started);

        Ok(())
    }

    // Makes the role's harness home ready to run in, and reports it with what runs there.
    //
    // The plugin goes in before anything runs. A run without it looks silent from out here,
    // whatever the agent is doing inside.
    fn prepare(
        &self,
        profile: Profile,
        sandbox: AgentSandbox,
        layout: &Layout,
    ) -> Result<(PathBuf, Started), Error> {
        // A role given another role's directories would read another role's instructions and write
        // into a working copy which is not its own.
        if layout.role() != self.role {
            return Err(Error::Role {
                expected: self.role,
                given: layout.role(),
            });
        }
        let harness = config::prepare(layout.harness())?;
        config::install_plugin(&harness)?;
        self.session.reports_in(config::report(&harness));
        let started = Started {
            agent: Settings::agent(profile),
            environment: self.environment(&harness, &harness.join(config::FILE), layout)?,
            sandbox,
        };

        Ok((harness, started))
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
            self.arranged(started, started.sandbox.command(PROGRAM))
                .current_dir(workspace)
                .args(["run", "--auto", "--format", "json"])
                .args(["--agent", &started.agent])
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

    // Asks a fresh run to commit whatever the stopped job left in the working copy.
    async fn preserve(&self, job: &Job, workspace: &Path) -> Result<(), Error> {
        tracing::info!(
            job = job.id,
            "asking the stopped job to commit what it reached"
        );
        match self
            .deliver(prompt::stopping(job), workspace, &title(job))
            .await?
        {
            Outcome::Finished => Ok(()),
            // The runtime ran out of patience with the second run too. What is in the tree is still
            // there, and preparing the next job commits it to a salvage branch.
            Outcome::Stopped => {
                tracing::warn!(
                    job = job.id,
                    "the stopped job was ended before it committed; its work stays in the tree"
                );

                Ok(())
            }
        }
    }

    // Asks OpenCode what it is offering, and refuses a model it would refuse at the first delivery.
    //
    // Model names are strings in a file an operator may edit, and the answer depends on more than
    // the spelling: OpenCode offers a provider's models only once it has been authorized for it.
    async fn catalogue(&self, started: &Started, model: &str) -> Result<Catalogue, Error> {
        let listed = Box::pin(
            self.arranged(
                started,
                started
                    .sandbox
                    .command(PROGRAM)
                    .args(["models", "--verbose"]),
            )
            .output(),
        )
        .await?;
        if !listed.status.success() {
            return Err(Error::Unavailable {
                status: listed.status,
                detail: listed.stderr.trim().to_owned(),
            });
        }
        let catalogue = Catalogue::read(&listed.stdout);
        // A listing this adapter could not make sense of says nothing about the model, so it must
        // not be read as the model being absent. The delivery reports it if it really is.
        if catalogue.is_empty() {
            tracing::warn!(
                role = %self.role,
                model,
                "could not read back what OpenCode offers, so the model was not checked"
            );

            return Ok(catalogue);
        }
        if !catalogue.offers(model) {
            return Err(Error::Model(model.to_owned()));
        }

        Ok(catalogue)
    }

    // Gives a command the environment this role's OpenCode runs in, credentials included.
    fn arranged(&self, started: &Started, command: AgentCommand) -> AgentCommand {
        started
            .environment
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
        let mut environment = BTreeMap::from([
            (HOME_VAR.to_owned(), path(harness)),
            (CONFIG_VAR.to_owned(), path(configuration)),
            (CACHE_VAR.to_owned(), path(&cache)),
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
            if credentials.way() == Way::Subscription {
                tracing::warn!(
                    role = %self.role,
                    harness = %harness.display(),
                    "this provider was authorized as a subscription, so OpenCode must already be \
                     signed in under this member's own harness home"
                );
            }
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

    async fn work(&self, job: Job, workspace: PathBuf) -> Result<(), agent::Error> {
        let carried = self
            .deliver(prompt::job(&job), &workspace, &title(&job))
            .await
            .map_err(agent::Error::new)?;
        match carried {
            Outcome::Finished => Ok(()),
            // An agent turn cannot be interrupted, so a job which was asked to stop is ended and
            // then asked, in a run of its own, to commit what it had reached.
            Outcome::Stopped if self.session.was_interrupted() => self
                .preserve(&job, &workspace)
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
         is authorized, with `radicle-team login`"
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

// Returns the name a job's runs are recorded under, so a transcript can be found again.
fn title(job: &Job) -> String {
    format!("job:{}", job.id)
}

fn path(path: &Path) -> String {
    path.display().to_string()
}
