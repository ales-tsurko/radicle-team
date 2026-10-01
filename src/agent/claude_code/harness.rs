//! The role a Claude Code process is started as, and the deliveries it is handed.

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
        claude_code::{
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
pub(crate) const ADAPTER: &str = "claude-code";

// The program itself, found on the operator's `PATH` inside the sandbox.
const PROGRAM: &str = "claude";
// Where Claude Code keeps its settings, sessions and credentials. Pointed at the role's own harness
// directory, so one member's state is that member's, and so neither the operator's `CLAUDE.md` nor
// their other agent applications' instructions are read as if they were the team's.
const HOME_VAR: &str = "HOME";
const CONFIG_VAR: &str = "CLAUDE_CONFIG_DIR";
// What anything run inside the sandbox downloads or builds. Pointed at the member's cache
// directory, which its three roles share and which can be removed at any time.
const CACHE_VAR: &str = "XDG_CACHE_HOME";
// Belt and braces beside the nonessential-traffic switch below, which a run reaches later than it
// reaches this.
const AUTOUPDATE_VAR: &str = "DISABLE_AUTOUPDATER";
// Turns off telemetry, error reporting, auto-updates and feature-flag fetching in one move. A
// member then needs nothing from Anthropic but its API, which is also what makes the network
// allowlist in the docs short enough to keep honest.
const TRAFFIC_VAR: &str = "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC";
// The errands Claude Code runs on a cheaper model -- naming things, choosing tools -- are paid for
// by the same account as everything else, so they run on this member's own model rather than on a
// Haiku nobody here authorized.
const ERRANDS_VAR: &str = "ANTHROPIC_DEFAULT_HAIKU_MODEL";
// Where the harness's own settings live under its home.
const CONFIG_SUBDIR: &str = ".claude";

/// One role of one member, run through Claude Code.
pub(crate) struct ClaudeCode {
    role: Role,
    provider: Provider,
    credentials: Option<Credentials>,
    started: Option<Started>,
    session: Arc<Session>,
}

impl ClaudeCode {
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

    // Configures the role and proves Claude Code can run it before any work arrives.
    async fn begin(
        &mut self,
        profile: Profile,
        mcp: &McpEndpoint,
        sandbox: AgentSandbox,
        layout: Layout,
    ) -> Result<(), Error> {
        let settings = Settings::read(&self.provider, profile.tuning)?;
        let started = self.prepare(sandbox, &layout, settings.clone(), mcp)?;
        self.present(&started).await?;
        tracing::info!(
            role = %self.role,
            member = profile.member.id(),
            model = settings.model(),
            effort = settings.effort(),
            harness = %started.harness.display(),
            "Claude Code role started"
        );
        self.started = Some(started);

        Ok(())
    }

    // Makes the role's harness home ready to run in, and reports what runs there.
    fn prepare(
        &self,
        sandbox: AgentSandbox,
        layout: &Layout,
        settings: Settings,
        mcp: &McpEndpoint,
    ) -> Result<Started, Error> {
        // A role given another role's directories would read another role's instructions and write
        // into a working copy which is not its own.
        if layout.role() != self.role {
            return Err(Error::Role {
                expected: self.role,
                given: layout.role(),
            });
        }
        let harness = config::prepare(layout.harness())?;
        let cache = config::prepare(layout.cache())?;
        let mcp_file = config::write_mcp(&harness, mcp)?;
        let instructions = config::combine(layout.documents())?;
        let environment = self.environment(&harness, &cache, settings.model());

        Ok(Started {
            harness,
            settings,
            mcp: mcp_file,
            instructions,
            environment,
            sandbox,
        })
    }

    // Proves Claude Code can be run, and is authorized, before any work arrives.
    //
    // There is no model listing to ask for afterwards, so these two probes are at most all a role
    // checks at startup; whether the model name is one Anthropic answers shows up on the first
    // delivery instead.
    async fn present(&self, started: &Started) -> Result<(), Error> {
        running_probe(&self.probed(started, &["--version"]).await?)?;
        if self.authorization_is_settled() {
            return Ok(());
        }

        signed_in_probe(&self.probed(started, &["auth", "status"]).await?)
    }

    // Returns whether this role's authorization is already settled, so Claude Code need not be
    // asked what it is signed in as.
    //
    // A provider which needs nothing to authorize has nothing to ask, and a role holding a key of
    // its own answers itself: the key goes into the run's environment and is what the first
    // delivery will present, so whether Claude Code would call an environment key a sign-in must
    // not decide startup. Only a sign-in Claude Code itself holds is worth asking about, since no
    // run of this role can place it there.
    fn authorization_is_settled(&self) -> bool {
        !self.provider.needs_authorizing()
            || self
                .credentials
                .as_ref()
                .is_some_and(|credentials| credentials.way() == Way::Key)
    }

    // Runs one read-only probe of the program and collects what it wrote.
    async fn probed(
        &self,
        started: &Started,
        arguments: &[&str],
    ) -> Result<crate::sandbox::Output, Error> {
        let mut command = self.arranged(started, started.sandbox.command(PROGRAM));
        for argument in arguments.iter().copied() {
            command = command.arg(argument);
        }

        Box::pin(command.output()).await.map_err(Error::from)
    }

    // Runs one delivery and reports how it ended.
    async fn deliver(&self, prompt: String, workspace: &Path) -> Result<Outcome, Error> {
        let started = self.started.as_ref().ok_or(Error::NotStarted(self.role))?;

        run::supervise(
            &self.session,
            self.arranged(started, started.sandbox.command(PROGRAM))
                .current_dir(workspace)
                .args(
                    started
                        .settings
                        .delivery(&started.mcp, &started.instructions, &prompt),
                ),
        )
        .await
    }

    // Runs one delivery which is expected to finish on its own.
    //
    // The talking and chatting roles have nothing to preserve when they are stopped: what they
    // produce is the answer they publish, and an answer half-written is not one.
    async fn answer(&self, prompt: String, workspace: &Path) -> Result<(), Error> {
        match self.deliver(prompt, workspace).await? {
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
        match self.deliver(prompt::stopping(job), workspace).await? {
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

    // Gives a command the environment this role's Claude Code runs in, credentials included.
    fn arranged(&self, started: &Started, command: AgentCommand) -> AgentCommand {
        started
            .environment
            .iter()
            .fold(command, |command, (key, value)| command.env(key, value))
    }

    // Gives the environment this role's Claude Code runs in, credentials included.
    fn environment(&self, harness: &Path, cache: &Path, model: &str) -> BTreeMap<String, String> {
        let mut environment = BTreeMap::from([
            (HOME_VAR.to_owned(), path(harness)),
            (CONFIG_VAR.to_owned(), path(&harness.join(CONFIG_SUBDIR))),
            (CACHE_VAR.to_owned(), path(cache)),
            (AUTOUPDATE_VAR.to_owned(), "1".to_owned()),
            (TRAFFIC_VAR.to_owned(), "1".to_owned()),
            (ERRANDS_VAR.to_owned(), model.to_owned()),
        ]);
        if let Some(credentials) = &self.credentials {
            // Named, never rendered: these are the operator's API keys.
            tracing::debug!(
                role = %self.role,
                way = %credentials.way(),
                variables = ?credentials.variables().keys(),
                "authorizing Claude Code"
            );
            if credentials.way() == Way::Subscription {
                tracing::warn!(
                    role = %self.role,
                    harness = %harness.display(),
                    "this provider was authorized as a subscription, so Claude Code must already \
                     hold that sign-in itself: sign in once under this member's own harness home"
                );
            }
            environment.extend(credentials.variables().clone());
        }

        environment
    }

    // Stops whatever is running and forgets what the role was configured with.
    async fn end(&mut self) -> Result<(), agent::Error> {
        self.session.abort().await?;
        self.started = None;

        Ok(())
    }
}

impl fmt::Debug for ClaudeCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeCode")
            .field("role", &self.role)
            .field("started", &self.started.is_some())
            .finish_non_exhaustive()
    }
}

impl Supervised for ClaudeCode {
    fn supervisor(&self) -> Arc<Session> {
        Arc::clone(&self.session)
    }
}

#[async_trait]
impl Talker for ClaudeCode {
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
        self.answer(prompt::mentioned(&context, target), &context.workspace)
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
        )
        .await
        .map_err(agent::Error::new)
    }

    async fn changed(&mut self, context: Context, change: Change) -> Result<(), agent::Error> {
        self.answer(prompt::changed(&context, change), &context.workspace)
            .await
            .map_err(agent::Error::new)
    }

    async fn shutdown(&mut self) -> Result<(), agent::Error> {
        self.end().await
    }
}

#[async_trait]
impl Chatter for ClaudeCode {
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
        self.answer(prompt::messaged(&context, &message), context.workspace())
            .await
            .map_err(agent::Error::new)
    }

    async fn shutdown(&mut self) -> Result<(), agent::Error> {
        self.end().await
    }
}

#[async_trait]
impl Coder for ClaudeCode {
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
            .deliver(prompt::job(&job), &workspace)
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
    // Where the harness home really is, for pointing the log reader at.
    harness: PathBuf,
    settings: Settings,
    mcp: PathBuf,
    instructions: PathBuf,
    environment: BTreeMap<String, String>,
    sandbox: AgentSandbox,
}

// Never renders the environment: it carries the provider's API key.
impl fmt::Debug for Started {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Started")
            .field("harness", &self.harness)
            .field("settings", &self.settings)
            .field("mcp", &self.mcp)
            .field("instructions", &self.instructions)
            .field("environment", &self.environment.keys())
            .finish_non_exhaustive()
    }
}

/// Error running a member's role through Claude Code.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// The provider file does not describe a member this crate can run.
    #[error(transparent)]
    Controls(#[from] crate::agent::controls::Error),
    /// Claude Code itself could not be run.
    #[error(
        "could not run `claude` ({status}): {detail}; install Claude Code \
         (https://code.claude.com/docs) so it is on this member's PATH"
    )]
    Unavailable {
        /// How the probe ended.
        status: ExitStatus,
        /// What it wrote about it.
        detail: String,
    },
    /// Claude Code holds no authorization this run could use.
    #[error(
        "this provider is not authorized ({status}); run `rad-team login anthropic` for a key, or \
         sign in to Claude Code yourself under this member's own harness home"
    )]
    Unauthorized {
        /// How the probe ended.
        status: ExitStatus,
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
    #[error("could not build the Claude Code configuration '{path}': {reason}")]
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
    #[error("could not read what the {role} role's Claude Code wrote: {source}")]
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
    /// A run ended in failure. Claude Code may report one and still exit zero, so the status is
    /// context rather than the verdict; the failure is in what was said.
    #[error("claude failed (ended with {status}): {detail}")]
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

// Decides what a probe of the program's own presence said.
fn running_probe(listed: &crate::sandbox::Output) -> Result<(), Error> {
    if listed.status.success() {
        return Ok(());
    }

    Err(Error::Unavailable {
        status: listed.status,
        detail: listed.stderr.trim().to_owned(),
    })
}

// Decides what a probe of this run's authorization said.
//
// The exit code decides, and what the answer wrote is never rendered: an authorization names the
// account it found, which is nobody else's business. The error says what to do instead.
fn signed_in_probe(listed: &crate::sandbox::Output) -> Result<(), Error> {
    if listed.status.success() {
        return Ok(());
    }

    Err(Error::Unauthorized {
        status: listed.status,
    })
}

fn path(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use super::ClaudeCode;
    use crate::{agent::Role, provider::Credentials};

    #[test]
    fn pins_the_environment_a_headless_run_needs() {
        let environment = keyed().environment(&harness(), &cache(), "claude-sonnet-5");

        assert_eq!(
            environment["CLAUDE_CONFIG_DIR"],
            path_of(&harness().join(".claude")),
            "settings, sessions and credentials belong to this role alone"
        );
        assert_eq!(environment["HOME"], path_of(&harness()));
        assert_eq!(environment["XDG_CACHE_HOME"], path_of(&cache()));
        // Both switches off, so a run neither phones home nor rewrites the binary under itself.
        assert_eq!(environment["DISABLE_AUTOUPDATER"], "1");
        assert_eq!(
            environment["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"], "1",
            "telemetry and feature flags are not worth a network allowlist entry"
        );
        assert_eq!(
            environment["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-sonnet-5",
            "the errands are paid for by the same account as everything else"
        );
        assert_eq!(environment["ANTHROPIC_API_KEY"], "secret-key");
    }

    #[test]
    fn authorizes_without_anything_to_place_in_the_environment() {
        let signed_in = ClaudeCode::new(
            Role::Talker,
            crate::agent::claude_code::tests::shipped_anthropic(),
            Some(Credentials::subscription()),
        );

        let environment = signed_in.environment(&harness(), &cache(), "claude-sonnet-5");

        assert!(
            !environment.contains_key("ANTHROPIC_API_KEY"),
            "the sign-in is the operator's own; nothing here carries it"
        );
    }

    // A secret which can be rendered is a secret which eventually is.
    #[test]
    fn never_renders_what_it_holds() {
        let claude = keyed();

        assert!(
            !format!("{claude:?}").contains("secret-key"),
            "the debug output names what is held, never its values"
        );
    }

    #[test]
    fn takes_a_version_which_ran_for_presence() {
        super::running_probe(&ended(0)).expect("a probe which ran proves presence");

        let mut crashed = ended(1);
        crashed.stderr = "claude: not found\n".to_owned();
        let error = super::running_probe(&crashed)
            .expect_err("a probe which could not run must be reported");

        assert!(
            error.to_string().contains("install Claude Code")
                && error.to_string().contains("not found"),
            "the operator has to be told what to install and what happened: {error}"
        );
    }

    #[test]
    fn takes_an_authorization_which_answered_well() {
        super::signed_in_probe(&ended(0))
            .expect("an authorization which answered is one this run can use");
    }

    // A key this role places in its own environment is the authorization itself, so whether Claude
    // Code would call an environment key a sign-in must not decide startup.
    #[test]
    fn takes_a_key_of_its_own_for_authorization() {
        assert!(
            keyed().authorization_is_settled(),
            "a key of one's own is the whole authorization; there is nothing to ask"
        );
        assert!(
            !ClaudeCode::new(
                Role::Talker,
                crate::agent::claude_code::tests::shipped_anthropic(),
                Some(Credentials::subscription()),
            )
            .authorization_is_settled(),
            "a sign-in is held by Claude Code, so that is the thing to ask"
        );
        assert!(
            !ClaudeCode::new(
                Role::Talker,
                crate::agent::claude_code::tests::shipped_anthropic(),
                None,
            )
            .authorization_is_settled(),
            "nothing recorded means the probe has to be the one to say so"
        );
    }

    #[test]
    fn refuses_a_run_nobody_authorized_and_says_what_to_do() {
        // What the check wrote when it found an account is never relayed; the exit code decides.
        let mut signed_in = ended(1);
        signed_in.stdout = "{\"email\":\"someone@example.com\"}".to_owned();

        let error = super::signed_in_probe(&signed_in)
            .expect_err("an unanswered authorization must stop the role at startup");

        let said = error.to_string();
        assert!(
            said.contains("login anthropic") && said.contains("harness home"),
            "both ways of fixing it have to be named: {said}"
        );
        assert!(
            !said.contains("example.com"),
            "the account an authorization found must stay out of the message"
        );
    }

    // A probe's ending, as the sandbox reports it.
    fn ended(code: i32) -> crate::sandbox::Output {
        crate::sandbox::Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(code << 8),
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn keyed() -> ClaudeCode {
        ClaudeCode::new(
            Role::Talker,
            crate::agent::claude_code::tests::shipped_anthropic(),
            Some(Credentials::key(BTreeMap::from([(
                "ANTHROPIC_API_KEY".to_owned(),
                "secret-key".to_owned(),
            )]))),
        )
    }

    fn harness() -> PathBuf {
        PathBuf::from("/member/harness/talker")
    }

    fn cache() -> PathBuf {
        PathBuf::from("/member/cache")
    }

    fn path_of(path: &std::path::Path) -> String {
        path.display().to_string()
    }

    mod preparing {
        use url::Url;

        use super::{ClaudeCode, path_of};
        use crate::{
            agent::{Layout, McpEndpoint, Role, claude_code::config::Settings, documents},
            home::MemberHome,
            member::Member,
            sandbox::AgentSandbox,
        };

        const ENDPOINT: &str = "http://127.0.0.1:4321/mcp/capability";

        #[test]
        fn prepares_what_a_delivery_will_read_and_run_under() {
            let (home, _kept) = member();
            let endpoint = endpoint();
            let claude = adapter();

            let started = claude
                .prepare(
                    AgentSandbox::load(&home, Member::Nico, Role::Talker, &endpoint)
                        .expect("sandbox should load"),
                    &layout(&home),
                    settings(),
                    &endpoint,
                )
                .expect("the role should be prepared");

            assert_eq!(
                started.harness,
                std::fs::canonicalize(home.harness(Role::Talker)).expect("harness should resolve"),
                "the role runs in the directory the runtime handed it"
            );
            assert!(
                started.mcp.is_file() && started.instructions.is_file(),
                "both files a delivery reads must exist before any run starts"
            );
            assert_eq!(started.settings.model(), "claude-sonnet-5");
            assert_eq!(
                started.environment["CLAUDE_CONFIG_DIR"],
                format!("{}/.claude", path_of(&started.harness)),
                "settings, sessions and credentials belong under the harness home it resolved"
            );
        }

        #[test]
        fn refuses_another_roles_directories() {
            let (home, _kept) = member();
            let endpoint = endpoint();
            let claude = adapter();

            let error = claude
                .prepare(
                    AgentSandbox::load(&home, Member::Nico, Role::Talker, &endpoint)
                        .expect("sandbox should load"),
                    &Layout::new(
                        Role::Coder,
                        home.cache(),
                        home.harness(Role::Coder),
                        documents::install(&home, Member::Nico, Role::Talker)
                            .expect("documents should be written"),
                    ),
                    settings(),
                    &endpoint,
                )
                .expect_err("a role given another's directories must refuse them");

            assert!(
                error.to_string().contains("talker") && error.to_string().contains("coder"),
                "{error}"
            );
        }

        #[test]
        fn reports_a_harness_directory_it_cannot_create() {
            let (home, _kept) = member();
            let endpoint = endpoint();
            let claude = adapter();
            // The documents are laid out while the directories still exist; what is blocked after
            // that is only the harness home itself.
            let prepared = layout(&home);
            let blocked = home.harness(Role::Talker);
            std::fs::remove_dir_all(&blocked).expect("harness should be removed");
            std::fs::write(&blocked, "in the way").expect("blocker should be written");

            let error = claude
                .prepare(
                    AgentSandbox::load(&home, Member::Nico, Role::Talker, &endpoint)
                        .expect("sandbox should load"),
                    &prepared,
                    settings(),
                    &endpoint,
                )
                .expect_err("an unusable harness home must be reported");

            assert!(error.to_string().contains("prepare"), "{error}");
        }

        #[test]
        fn reports_documents_it_cannot_read() {
            let (home, _kept) = member();
            let endpoint = endpoint();
            let claude = adapter();

            let error = claude
                .prepare(
                    AgentSandbox::load(&home, Member::Nico, Role::Talker, &endpoint)
                        .expect("sandbox should load"),
                    // One document named which was never written, where six readable ones belong.
                    &Layout::new(
                        Role::Talker,
                        home.cache(),
                        home.harness(Role::Talker),
                        vec![home.instructions(Role::Talker).join("MISSING.md")],
                    ),
                    settings(),
                    &endpoint,
                )
                .expect_err("instructions which cannot be read must stop the role");

            assert!(error.to_string().contains("MISSING.md"), "{error}");
        }

        fn adapter() -> ClaudeCode {
            ClaudeCode::new(
                Role::Talker,
                crate::agent::claude_code::tests::shipped_anthropic(),
                None,
            )
        }

        fn settings() -> Settings {
            Settings::read(
                &crate::agent::claude_code::tests::shipped_anthropic(),
                Member::Nico.profile().tuning,
            )
            .expect("settings should be read")
        }

        fn layout(home: &MemberHome) -> Layout {
            Layout::new(
                Role::Talker,
                home.cache(),
                home.harness(Role::Talker),
                documents::install(home, Member::Nico, Role::Talker)
                    .expect("documents should be written"),
            )
        }

        fn endpoint() -> McpEndpoint {
            McpEndpoint::new(Url::parse(ENDPOINT).expect("endpoint should be valid"))
        }

        // One member home per test, named after this process so tests running beside it never
        // share one.
        fn member() -> (MemberHome, tempfile::TempDir) {
            let kept = tempfile::tempdir().expect("temporary directory should be created");
            let home = MemberHome::at(kept.path());
            home.create().expect("member directory should be created");

            (home, kept)
        }
    }
}
