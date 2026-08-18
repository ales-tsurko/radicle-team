//! Restricted command builder for agent adapters.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::Stdio,
};
#[cfg(target_os = "linux")]
use std::sync::Arc;

use tokio::process::{Child, ChildStderr, ChildStdout};
use zerobox_network_proxy::NetworkProxy;
use zerobox_protocol::{
    config_types::WindowsSandboxLevel,
    models::PermissionProfile,
    permissions::{FileSystemSandboxPolicy, NetworkSandboxPolicy},
};
use zerobox_sandboxing::{
    SandboxCommand, SandboxManager, SandboxTransformRequest, get_platform_sandbox,
};
use zerobox_utils_absolute_path::AbsolutePathBuf;

use crate::{
    agent::{McpEndpoint, Role},
    home::MemberHome,
    member::Member,
    sandbox::{
        Config,
        network::{ManagedNetwork, ProxyRuntime},
        policy::{NetworkPolicy, Policy},
    },
};

const ENVIRONMENT: &[&str] = &["PATH", "HOME", "USER", "SHELL", "TERM", "LANG"];
// How much of a command's output is kept. Enough for any diagnostic worth reading, and bounded so
// a process which writes without stopping cannot exhaust this one's memory.
const OUTPUT_KEPT: usize = 1 << 20;

// Who a sandbox is built for.
//
// The runtime confines its own Git the same way it confines an agent's. It is the more trusted
// side, but it runs Git against working copies an agent can write, and Git derives what it does
// from what it finds there. Fencing off each surface as it turns up is guesswork; running the
// command inside a sandbox bounds what the guess costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    // One role's agent, held to its own working copy and the member's local configuration.
    Agent(Role),
    // The runtime's own Git, held to the working copies and nothing else.
    Runtime,
}

/// Zerobox launcher configured for one member's agent workspaces.
#[derive(Debug, Clone)]
pub struct AgentSandbox {
    home: MemberHome,
    purpose: Purpose,
    policy: Policy,
    workspaces: PathBuf,
    #[cfg(target_os = "linux")]
    linux_helper: Arc<crate::sandbox::linux::Helper>,
}

impl AgentSandbox {
    /// Loads the fixed policy and the member's local configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform has no enforcing backend, the workspace root cannot be
    /// prepared, or the local configuration is unsafe or invalid.
    pub fn load(
        home: &MemberHome,
        member: Member,
        role: Role,
        endpoint: &McpEndpoint,
    ) -> Result<Self, Error> {
        // Scoped to this role's own working copy rather than the directory holding all three.
        // Sharing that directory would let one role rewrite another's checked-out branch, which is
        // exactly what running them separately is meant to prevent.
        let workspaces = Self::rooted(&home.worktree(role))?;
        let store = Store::new(home.clone(), member, role);
        let policy = Policy::load(
            &workspaces,
            Some(endpoint),
            &[],
            &store.writable(),
            &store.protected(&workspaces),
        )?;
        // Validated once here so an unusable configuration fails startup rather than the first
        // command, even though `build` reloads it for every spawn.
        policy.validate(&Config::load(home)?)?;

        Self::ready(home, Purpose::Agent(role), policy, workspaces)
    }

    /// Loads the sandbox the runtime runs its own Git in.
    ///
    /// Held to the shared object store and, when the command is preparing one, a single working
    /// copy. The other roles' working copies stay read-only, so a command run for one of them
    /// reaches no further into the others than that role could itself. `repository` is the one
    /// repository in Radicle storage a delivery is fetched from, and it is readable rather than
    /// writable. Nothing else is reachable: not the member's identity, not the rest of storage, and
    /// not whatever the local configuration happens to permit an agent.
    ///
    /// External network access is denied. On macOS host loopback stays reachable even so, which is
    /// a platform difference described in [`NetworkAccess`](crate::sandbox::NetworkAccess).
    ///
    /// # Errors
    ///
    /// Returns an error when the platform has no enforcing backend or the working copies cannot be
    /// prepared.
    pub fn runtime(
        home: &MemberHome,
        role: Option<Role>,
        repository: &Path,
    ) -> Result<Self, Error> {
        let workspaces = Self::rooted(&home.code())?;
        let policy = Policy::load(
            &workspaces,
            None,
            std::slice::from_ref(&repository.to_path_buf()),
            &[],
            &Store::others(home, role),
        )?;

        Self::ready(home, Purpose::Runtime, policy, workspaces)
    }

    /// Creates the sandbox's root and reports where it really is on disk.
    fn rooted(workspaces: &Path) -> Result<PathBuf, Error> {
        Self::ensure_supported()?;
        fs::create_dir_all(workspaces).map_err(|source| Error::Io {
            path: workspaces.into(),
            source,
        })?;
        #[cfg(unix)]
        fs::set_permissions(workspaces, fs::Permissions::from_mode(0o700)).map_err(|source| {
            Error::Io {
                path: workspaces.into(),
                source,
            }
        })?;

        fs::canonicalize(workspaces).map_err(|source| Error::Io {
            path: workspaces.into(),
            source,
        })
    }

    fn ready(
        home: &MemberHome,
        purpose: Purpose,
        policy: Policy,
        workspaces: PathBuf,
    ) -> Result<Self, Error> {
        tracing::info!(
            ?purpose,
            workspaces = %workspaces.display(),
            configuration = %home.sandbox_config().display(),
            "sandbox ready"
        );

        Ok(Self {
            home: home.clone(),
            purpose,
            policy,
            workspaces,
            #[cfg(target_os = "linux")]
            linux_helper: Arc::new(crate::sandbox::linux::Helper::prepare(home)?),
        })
    }

    /// Starts building a sandboxed command without exposing permission controls.
    #[must_use]
    pub fn command(&self, program: impl Into<String>) -> AgentCommand {
        AgentCommand {
            sandbox: self.clone(),
            program: program.into(),
            arguments: Vec::new(),
            current_dir: self.workspaces.clone(),
            environment: BTreeMap::new(),
        }
    }

    /// Builds the launch policy for one command, re-reading the member's local configuration.
    ///
    /// Reloading here lets an operator widen a denied path and have the next command pick it up,
    /// without restarting the member. A configuration that has become unsafe or invalid fails the
    /// spawn instead of silently reusing the previous one.
    fn build(&self, command: &AgentCommand) -> Result<Launch, Error> {
        let current_dir = self.validate_current_dir(&command.current_dir)?;
        // The runtime's own Git ignores it. That file says what an agent may reach, and widening
        // it must not widen what code smuggled into a Git command can reach.
        let config = match self.purpose {
            Purpose::Agent(_role) => Config::load(&self.home)?,
            Purpose::Runtime => Config::confined(),
        };
        self.policy.validate(&config)?;
        let file_system =
            self.policy
                .file_system(&config, &self.workspaces, self.helper_executable())?;

        let network = self.policy.network(&config);
        tracing::debug!(
            program = command.program,
            working_directory = %current_dir.display(),
            network_restricted = network.is_restricted(),
            "building sandbox policy for a command"
        );

        Ok(Launch {
            current_dir,
            file_system,
            network,
            helper: self.helper_executable().map(Path::to_owned),
        })
    }

    fn validate_current_dir(&self, path: &Path) -> Result<PathBuf, Error> {
        let canonical = fs::canonicalize(path).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })?;
        if canonical.starts_with(&self.workspaces) {
            Ok(canonical)
        } else {
            Err(Error::WorkingDirectory {
                path: path.into(),
                workspaces: self.workspaces.clone().into(),
            })
        }
    }

    fn ensure_supported() -> Result<(), Error> {
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            Ok(())
        } else {
            Err(Error::UnsupportedPlatform(std::env::consts::OS))
        }
    }

    fn helper_executable(&self) -> Option<&Path> {
        #[cfg(target_os = "linux")]
        {
            Some(self.linux_helper.executable())
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }
}

// Which parts of the shared Git object store one role may write.
//
// The store has to be writable at all, or an agent could edit files but never commit them: a linked
// working copy keeps its objects, references and index there rather than beside itself. What it may
// not have is a say in what the runtime runs. Git executes whatever its hooks and configuration
// name, it finds both by following `commondir` out of the working copy's own directory, and the
// runtime runs Git against this store outside the sandbox.
#[derive(Debug)]
struct Store {
    home: MemberHome,
    member: Member,
    role: Role,
    common: PathBuf,
    mine: PathBuf,
}

impl Store {
    fn new(home: MemberHome, member: Member, role: Role) -> Self {
        let common = home.git_common();
        let mine = common.join("worktrees").join(role.id());

        Self {
            home,
            member,
            role,
            common,
            mine,
        }
    }

    // Returns everything this role may write.
    //
    // Its own working copy directory is granted separately from the store holding it, because
    // Zerobox protects a working copy's Git directory by default and that default covers the entry
    // this worktree keeps in the store.
    fn writable(&self) -> Vec<PathBuf> {
        let mut writable = vec![
            self.home.cache(),
            self.home.harness(self.role),
            self.common.clone(),
            self.mine.clone(),
        ];
        // Branches are written only under the runtime's own namespace, and only by the role which
        // stands on one. The others answer from a detached head and never move a reference.
        if self.role.owns_branches() {
            writable.push(self.branches());
        }

        writable
    }

    // Returns the paths inside those grants which must stay read-only.
    fn protected(&self, worktree: &Path) -> Vec<PathBuf> {
        [
            // The member's standing instructions. They live inside the harness home, which the
            // role must write, but an agent able to rewrite its own character and the
            // team's rules could reword them mid-run and every later turn would read
            // the rewrite.
            self.home.instructions(self.role),
            // The pointer to the store. Replaced, it would send the runtime's Git somewhere else.
            worktree.join(".git"),
            self.common.join("config"),
            self.common.join("hooks"),
            // References belong to whichever role created them, and `packed-refs` holds all of
            // them at once. The building role is given its own namespace back below.
            self.common.join("refs"),
            self.common.join("packed-refs"),
            // Where this working copy finds the store. Repointed at one the role controls, the
            // runtime's next Git command would read that one's configuration and hooks.
            self.mine.join("commondir"),
            self.mine.join("gitdir"),
        ]
        .into_iter()
        .chain(Self::others(&self.home, Some(self.role)))
        .collect()
    }

    // Returns the working copies belonging to every role but `role`.
    //
    // Both halves of one: the files, and the entry the shared store keeps for it. Writing the
    // second is writing the first, since that entry holds the copy's index and head.
    fn others(home: &MemberHome, role: Option<Role>) -> Vec<PathBuf> {
        let entries = home.git_common().join("worktrees");

        Role::ALL
            .into_iter()
            .filter(|other| Some(*other) != role)
            .flat_map(|other| [home.worktree(other), entries.join(other.id())])
            .collect()
    }

    fn branches(&self) -> PathBuf {
        self.common
            .join("refs")
            .join("heads")
            .join(crate::workspace::branches(self.member))
    }
}

/// A command whose security policy is fixed by [`AgentSandbox`].
pub struct AgentCommand {
    sandbox: AgentSandbox,
    program: String,
    arguments: Vec<String>,
    current_dir: PathBuf,
    environment: BTreeMap<String, String>,
}

// Names the variables it carries and never their values. The provider's API key is placed here, and
// a secret which can be rendered is a secret which eventually is.
impl fmt::Debug for AgentCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentCommand")
            .field("program", &self.program)
            .field("arguments", &self.arguments)
            .field("current_dir", &self.current_dir)
            .field("environment", &self.environment.keys())
            .finish()
    }
}

impl AgentCommand {
    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, argument: impl Into<String>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    /// Appends multiple arguments.
    #[must_use]
    pub fn args(mut self, arguments: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    /// Selects a delivery workspace as the command's working directory.
    #[must_use]
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = path.into();
        self
    }

    /// Sets one explicit environment value.
    ///
    /// This is how a provider's API key reaches the agent, so the values never appear in this
    /// type's [`Debug`] output or in anything the sandbox logs.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let _previous = self.environment.insert(key.into(), value.into());
        self
    }

    /// Starts the command and returns its piped output streams.
    ///
    /// # Errors
    ///
    /// Returns an error when the working directory is outside the member's workspace root or
    /// Zerobox cannot establish the sandbox.
    pub async fn spawn(self) -> Result<AgentChild, Error> {
        let launch = self.sandbox.build(&self)?;
        launch.spawn(self).await
    }

    /// Runs the command to completion and collects what it wrote.
    ///
    /// Both streams are drained at once. Reading one to its end first would stop the process the
    /// moment it filled the other's pipe, and neither side would ever move again.
    ///
    /// # Errors
    ///
    /// Returns an error when the command cannot be started or its output cannot be read.
    pub async fn output(self) -> Result<Output, Error> {
        let mut child = self.spawn().await?;
        let (out, errors) = (child.stdout(), child.stderr());
        let (stdout, stderr) = tokio::join!(Self::drain(out), Self::drain(errors));

        Ok(Output {
            status: child.wait().await?,
            stdout: stdout?,
            stderr: stderr?,
        })
    }

    /// Reads a stream to its end, keeping the first [`OUTPUT_KEPT`] bytes.
    ///
    /// Everything past that is read and dropped rather than left in the pipe, because a process
    /// blocked writing output nobody collects never exits.
    async fn drain(stream: Option<impl tokio::io::AsyncRead + Unpin>) -> Result<String, Error> {
        use tokio::io::AsyncReadExt as _;

        let Some(mut stream) = stream else {
            return Ok(String::new());
        };
        let mut kept = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = stream.read(&mut buffer).await.map_err(Self::io)?;
            if read == 0 {
                break;
            }
            let room = OUTPUT_KEPT.saturating_sub(kept.len());
            kept.extend_from_slice(buffer.get(..read.min(room)).unwrap_or_default());
        }

        Ok(String::from_utf8_lossy(&kept).into_owned())
    }

    fn io(source: io::Error) -> Error {
        Error::Io {
            path: Path::new("<sandboxed process>").into(),
            source,
        }
    }
}

/// What a sandboxed command wrote and how it ended.
#[derive(Debug)]
pub struct Output {
    /// How the process exited.
    pub status: std::process::ExitStatus,
    /// Everything the process wrote to its standard output.
    pub stdout: String,
    /// Everything the process wrote to its standard error.
    pub stderr: String,
}

/// A running sandboxed agent process.
pub struct AgentChild {
    process: Child,
    _proxy: Option<NetworkProxy>,
    _proxy_runtime: Option<ProxyRuntime>,
}

impl AgentChild {
    /// Takes the child's standard output stream.
    pub fn stdout(&mut self) -> Option<ChildStdout> {
        self.process.stdout.take()
    }

    /// Takes the child's standard error stream.
    pub fn stderr(&mut self) -> Option<ChildStderr> {
        self.process.stderr.take()
    }

    /// Waits for the child to exit.
    ///
    /// # Errors
    ///
    /// Returns an error when waiting fails.
    pub async fn wait(mut self) -> Result<std::process::ExitStatus, Error> {
        self.process.wait().await.map_err(|source| Error::Io {
            path: Path::new("<agent process>").into(),
            source,
        })
    }
}

impl fmt::Debug for AgentChild {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("AgentChild").finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Launch {
    current_dir: PathBuf,
    file_system: FileSystemSandboxPolicy,
    network: NetworkPolicy,
    helper: Option<PathBuf>,
}

impl Launch {
    async fn spawn(self, command: AgentCommand) -> Result<AgentChild, Error> {
        let managed = ManagedNetwork::start(self.network.clone()).await?;
        let request = self.transform(command, managed.as_ref())?;

        PreparedAgent::new(self.current_dir, request, managed)?.spawn()
    }

    fn transform(
        &self,
        command: AgentCommand,
        managed: Option<&ManagedNetwork>,
    ) -> Result<zerobox_sandboxing::SandboxExecRequest, Error> {
        let proxy = managed.map(|managed| &managed.proxy);
        let permissions = PermissionProfile::from_runtime_permissions(
            &self.file_system,
            NetworkSandboxPolicy::Enabled,
        );
        let sandbox =
            get_platform_sandbox(false).ok_or(Error::UnsupportedPlatform(std::env::consts::OS))?;
        let request = SandboxManager::new()
            .transform(SandboxTransformRequest {
                command: SandboxCommand {
                    program: OsString::from(command.program),
                    args: command.arguments,
                    cwd: AbsolutePathBuf::from_absolute_path_checked(&self.current_dir).map_err(
                        |source| Error::Io {
                            path: self.current_dir.clone().into(),
                            source,
                        },
                    )?,
                    env: Self::environment(command.environment),
                    additional_permissions: None,
                },
                permissions: &permissions,
                sandbox,
                enforce_managed_network: proxy.is_some(),
                network: proxy,
                sandbox_policy_cwd: &self.current_dir,
                zerobox_linux_sandbox_exe: self.helper.as_deref(),
                use_legacy_landlock: false,
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                windows_sandbox_private_desktop: false,
            })
            .map_err(Error::from)?;

        Ok(request)
    }

    // Returns whether a sandboxed command inherits `key` from this process.
    fn inherited_common(key: &str) -> bool {
        ENVIRONMENT.contains(&key)
    }

    #[cfg(not(test))]
    fn inherited(key: &str) -> bool {
        Self::inherited_common(key)
    }

    // Adds where a coverage build writes its profile, which is measurement rather than policy.
    //
    // These tests run instrumented binaries, and one told nothing writes its profile into the
    // working directory when it exits -- a working copy, where the runtime would find it as
    // uncommitted work and salvage it to a branch. Passing the path through sends it to the
    // directory the coverage run named, outside the sandbox, where the write is simply refused.
    #[cfg(test)]
    fn inherited(key: &str) -> bool {
        Self::inherited_common(key) || key == "LLVM_PROFILE_FILE"
    }

    // Builds the environment one sandboxed command runs with.
    //
    // Git is reset to its defaults because it refuses to run at all when it cannot read the
    // operating-system user's configuration, and the sandbox denies that file. Doing it here also
    // keeps a member behaving the same on every machine it is deployed to. Who the member is comes
    // from the shared object store's own configuration instead.
    fn environment(
        overrides: BTreeMap<String, String>,
    ) -> std::collections::HashMap<String, String> {
        let mut environment = std::env::vars()
            .filter(|(key, _value)| Self::inherited(key))
            .collect::<std::collections::HashMap<_, _>>();
        environment.extend(
            radicle::git::env::GIT_DEFAULT_CONFIG
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned())),
        );
        environment.extend(overrides);
        environment
    }
}

struct PreparedAgent {
    command: tokio::process::Command,
    managed: Option<ManagedNetwork>,
    program: PathBuf,
}

impl PreparedAgent {
    fn new(
        current_dir: PathBuf,
        request: zerobox_sandboxing::SandboxExecRequest,
        managed: Option<ManagedNetwork>,
    ) -> Result<Self, Error> {
        let (program, arguments) = request.command.split_first().ok_or(Error::InvalidCommand)?;
        let mut command = tokio::process::Command::new(program);
        command
            .args(arguments)
            .current_dir(current_dir)
            .env_clear()
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self::set_arg0(&mut command, request.arg0);
        command.envs(Self::environment(request.env, managed.as_ref()));

        Ok(Self {
            command,
            managed,
            program: PathBuf::from(program),
        })
    }

    fn environment(
        mut environment: std::collections::HashMap<String, String>,
        managed: Option<&ManagedNetwork>,
    ) -> std::collections::HashMap<String, String> {
        if let Some(managed) = managed {
            managed.apply_to_environment(&mut environment);
        }
        environment
    }

    #[cfg(unix)]
    fn set_arg0(command: &mut tokio::process::Command, arg0: Option<String>) {
        if let Some(arg0) = arg0 {
            command.as_std_mut().arg0(arg0);
        }
    }

    #[cfg(not(unix))]
    fn set_arg0(_command: &mut tokio::process::Command, _arg0: Option<String>) {}

    fn spawn(mut self) -> Result<AgentChild, Error> {
        let process = self.command.spawn().map_err(|source| Error::Io {
            path: self.program.into(),
            source,
        })?;
        let (proxy, proxy_runtime) = self
            .managed
            .map(ManagedNetwork::into_parts)
            .map_or((None, None), |(proxy, runtime)| {
                (Some(proxy), Some(runtime))
            });

        Ok(AgentChild {
            process,
            _proxy: proxy,
            _proxy_runtime: proxy_runtime,
        })
    }
}

/// Error configuring or running a sandboxed agent process.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Loading local sandbox configuration failed.
    #[error(transparent)]
    Configuration(#[from] crate::sandbox::ConfigError),
    /// The operating-system home directory is unavailable.
    #[error("could not determine the operating-system home directory for sandbox defaults")]
    HomeUnavailable,
    /// A fixed Zerobox profile could not be loaded safely.
    #[error("invalid embedded Zerobox profile '{profile}': {reason}")]
    BuiltInProfile {
        /// Profile name.
        profile: String,
        /// Validation failure.
        reason: String,
    },
    /// The run-scoped MCP endpoint is not a loopback HTTP endpoint.
    #[error("the run-scoped MCP endpoint is not a loopback HTTP endpoint")]
    McpEndpoint,
    /// A local allow would override a fixed sensitive-path deny.
    #[error(
        "sandbox allow path '{}' overrides fixed deny '{}'",
        allowed.display(),
        denied.display()
    )]
    FixedDenyOverlap {
        /// Rejected local allow.
        allowed: Box<Path>,
        /// Fixed sensitive path.
        denied: Box<Path>,
    },
    /// The current platform has no enforcing Zerobox backend.
    #[error("agent sandboxing is unsupported on {0}")]
    UnsupportedPlatform(&'static str),
    #[cfg(target_os = "linux")]
    /// Preparing the embedded Linux helper failed.
    #[error(transparent)]
    LinuxHelper(#[from] crate::sandbox::LinuxHelperError),
    /// Preparing a workspace path failed.
    #[error("failed to prepare sandbox path '{path}': {source}")]
    Io {
        /// Path being prepared.
        path: Box<Path>,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// The adapter selected a directory outside its managed workspaces.
    #[error(
        "agent working directory '{}' is outside '{}'",
        path.display(),
        workspaces.display()
    )]
    WorkingDirectory {
        /// Rejected working directory.
        path: Box<Path>,
        /// Allowed workspace root.
        workspaces: Box<Path>,
    },
    /// Zerobox generated an empty process command.
    #[error("sandbox generated an empty agent command")]
    InvalidCommand,
    /// Zerobox could not transform the command for the platform sandbox.
    #[error("failed to transform agent command: {0}")]
    SandboxTransform(#[from] zerobox_sandboxing::SandboxTransformError),
    /// Zerobox failed to establish or supervise the sandbox.
    #[error("agent sandbox failed: {0}")]
    Zerobox(#[from] anyhow::Error),
}

// Excluded from Miri: every test here loads a real sandbox, which writes a configuration file
// that Miri's synthetic user ID cannot own, and on Linux resolves bubblewrap through a foreign
// `access` call that Miri cannot execute.
#[cfg(all(test, not(miri), any(target_os = "macos", target_os = "linux")))]
mod tests {
    use url::Url;

    use crate::{agent::McpEndpoint, home::MemberHome, member::Member, sandbox::AgentSandbox};

    fn member_home(root: &std::path::Path) -> MemberHome {
        MemberHome::at(root)
    }

    #[test]
    fn rejects_working_directories_outside_member_workspaces() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let home = member_home(directory.path());
        let sandbox = sandbox(&home);
        let outside = directory.path().join("outside");
        std::fs::create_dir(&outside).expect("outside directory should be created");
        let outside =
            std::fs::canonicalize(outside).expect("outside directory should be canonicalized");
        let expected_workspaces = std::fs::canonicalize(home.worktree(crate::agent::Role::Talker))
            .expect("workspaces should be canonicalized");
        let error = sandbox
            .validate_current_dir(&outside)
            .expect_err("outside working directory should be rejected");

        assert!(matches!(
            error,
            super::Error::WorkingDirectory {
                path,
                workspaces: actual_workspaces
            } if path.as_ref() == outside && actual_workspaces.as_ref() == expected_workspaces
        ));
    }

    #[test]
    fn builds_every_configured_policy() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let home = member_home(directory.path());
        let sandbox = sandbox(&home);
        let path = directory.path().to_owned();

        for network in [
            crate::sandbox::NetworkAccess::All,
            crate::sandbox::NetworkAccess::Disabled,
            crate::sandbox::NetworkAccess::Domains(vec!["example.com".to_owned()]),
        ] {
            let mut config = crate::sandbox::Config::default();
            config.set_allow_read(vec![path.clone()]);
            config.set_deny_read(vec![path.clone()]);
            config.set_allow_write(vec![path.clone()]);
            config.set_deny_write(vec![path.clone()]);
            config.set_network(network);
            write_config(&home, &config);
            let command = sandbox.command("true");

            sandbox
                .build(&command)
                .expect("configured policy should build");
        }
    }

    // The runtime runs Git against the shared store outside the sandbox, and Git runs whatever its
    // hooks and configuration name. A role which could write either of those, or repoint its
    // working copy at a store it controls, would be choosing what the runtime executes.
    #[test]
    fn keeps_the_shared_store_from_choosing_what_the_runtime_runs() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let home = member_home(directory.path());
        let sandbox = sandbox(&home);
        let common = home.git_common();
        let worktrees = common.join("worktrees");
        let policy = built(&home, &sandbox);

        for denied in [
            sandbox.workspaces.join(".git"),
            common.join("config"),
            common.join("hooks"),
            common.join("refs"),
            common.join("packed-refs"),
            worktrees.join("talker").join("commondir"),
            worktrees.join("talker").join("gitdir"),
            worktrees.join("coder"),
            worktrees.join("chatter"),
        ] {
            assert!(
                !policy.can_write_path_with_cwd(&denied, &sandbox.workspaces),
                "{} must not be writable by the talking role",
                denied.display()
            );
        }
        for allowed in [common.join("objects"), worktrees.join("talker")] {
            assert!(
                policy.can_write_path_with_cwd(&allowed, &sandbox.workspaces),
                "{} must stay writable, or the role could never commit",
                allowed.display()
            );
        }
    }

    // Only the building role has branches, and only under the runtime's own namespace. A talking
    // role which could write references would be able to move or delete them.
    #[test]
    fn gives_branches_to_the_building_role_alone() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let home = member_home(directory.path());
        let branches = home
            .git_common()
            .join("refs")
            .join("heads")
            .join(crate::workspace::branches(Member::Nico));

        for (role, expected) in [
            (crate::agent::Role::Coder, true),
            (crate::agent::Role::Talker, false),
        ] {
            let sandbox = AgentSandbox::load(&home, Member::Nico, role, &endpoint())
                .expect("default sandbox should load");
            let policy = built(&home, &sandbox);

            assert_eq!(
                policy.can_write_path_with_cwd(&branches, &sandbox.workspaces),
                expected,
                "{role} should{} be able to write branches",
                if expected { "" } else { " not" }
            );
        }
    }

    // The agent reads its own character and the team's rules from here every turn. An agent able to
    // rewrite them could reword itself mid-run and every later turn would read the rewrite.
    #[test]
    fn never_lets_an_agent_rewrite_its_own_instructions() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let home = member_home(directory.path());

        for role in crate::agent::Role::ALL {
            let sandbox = AgentSandbox::load(&home, Member::Nico, role, &endpoint())
                .expect("default sandbox should load");
            let policy = built(&home, &sandbox);

            assert!(
                !policy.can_write_path_with_cwd(&home.instructions(role), &sandbox.workspaces),
                "{role} must not be able to rewrite the documents it is instructed by"
            );
        }
    }

    fn built(
        home: &MemberHome,
        sandbox: &AgentSandbox,
    ) -> zerobox_protocol::permissions::FileSystemSandboxPolicy {
        let config = crate::sandbox::Config::default();
        write_config(home, &config);

        sandbox
            .policy
            .file_system(&config, &sandbox.workspaces, None)
            .expect("policy should build")
    }

    // Replaces the member's configuration on disk, which `build` reloads for every command.
    fn write_config(home: &MemberHome, config: &crate::sandbox::Config) {
        let path = home.sandbox_config();
        let _removed = std::fs::remove_file(&path);
        config
            .write(&path)
            .expect("sandbox configuration should be written");
    }

    fn sandbox(home: &MemberHome) -> AgentSandbox {
        AgentSandbox::load(home, Member::Nico, crate::agent::Role::Talker, &endpoint())
            .expect("default sandbox should load")
    }

    fn endpoint() -> McpEndpoint {
        McpEndpoint::new(
            Url::parse("http://127.0.0.1:12345/mcp/capability")
                .expect("test MCP endpoint should be valid"),
        )
    }
}
