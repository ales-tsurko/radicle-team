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
    agent::McpEndpoint,
    home::MemberHome,
    sandbox::{
        Config,
        network::{ManagedNetwork, ProxyRuntime},
        policy::{NetworkPolicy, Policy},
    },
};

const ENVIRONMENT: &[&str] = &["PATH", "HOME", "USER", "SHELL", "TERM", "LANG"];

/// Zerobox launcher configured for one member's agent workspaces.
#[derive(Debug, Clone)]
pub struct AgentSandbox {
    config: Config,
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
    pub fn load(home: &MemberHome, endpoint: &McpEndpoint) -> Result<Self, Error> {
        Self::ensure_supported()?;
        let workspaces = home.workspaces();
        fs::create_dir_all(&workspaces).map_err(|source| Error::Io {
            path: workspaces.clone().into(),
            source,
        })?;
        #[cfg(unix)]
        fs::set_permissions(&workspaces, fs::Permissions::from_mode(0o700)).map_err(|source| {
            Error::Io {
                path: workspaces.clone().into(),
                source,
            }
        })?;
        let workspaces = fs::canonicalize(&workspaces).map_err(|source| Error::Io {
            path: workspaces.into(),
            source,
        })?;

        let config = Config::load(home)?;
        let policy = Policy::load(&workspaces, endpoint)?;
        policy.validate(&config)?;

        Ok(Self {
            config,
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

    fn build(&self, command: &AgentCommand) -> Result<Launch, Error> {
        let current_dir = self.validate_current_dir(&command.current_dir)?;
        let file_system =
            self.policy
                .file_system(&self.config, &self.workspaces, self.helper_executable())?;

        Ok(Launch {
            current_dir,
            file_system,
            network: self.policy.external_network(&self.config),
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

/// A command whose security policy is fixed by [`AgentSandbox`].
#[derive(Debug)]
pub struct AgentCommand {
    sandbox: AgentSandbox,
    program: String,
    arguments: Vec<String>,
    current_dir: PathBuf,
    environment: BTreeMap<String, String>,
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

    /// Sets one explicit non-secret environment value.
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

    fn environment(
        overrides: BTreeMap<String, String>,
    ) -> std::collections::HashMap<String, String> {
        let mut environment = std::env::vars()
            .filter(|(key, _value)| ENVIRONMENT.contains(&key.as_str()))
            .collect::<std::collections::HashMap<_, _>>();
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

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use url::Url;

    use crate::{agent::McpEndpoint, home::MemberHome, member::Member, sandbox::AgentSandbox};

    #[test]
    fn rejects_working_directories_outside_member_workspaces() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let home = MemberHome::at(directory.path(), Member::Nico);
        let sandbox = sandbox(&home);
        let outside = directory.path().join("outside");
        std::fs::create_dir(&outside).expect("outside directory should be created");
        let outside =
            std::fs::canonicalize(outside).expect("outside directory should be canonicalized");
        let expected_workspaces =
            std::fs::canonicalize(home.workspaces()).expect("workspaces should be canonicalized");
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
        let home = MemberHome::at(directory.path(), Member::Nico);
        let sandbox = sandbox(&home);
        let path = directory.path().to_owned();

        for network in [
            crate::sandbox::ExternalNetwork::All,
            crate::sandbox::ExternalNetwork::Disabled,
            crate::sandbox::ExternalNetwork::Domains(vec!["example.com".to_owned()]),
        ] {
            let mut configured = sandbox.clone();
            configured.config.set_allow_read(vec![path.clone()]);
            configured.config.set_deny_read(vec![path.clone()]);
            configured.config.set_allow_write(vec![path.clone()]);
            configured.config.set_deny_write(vec![path.clone()]);
            configured.config.set_external_network(network);
            let command = configured.command("true");

            configured
                .build(&command)
                .expect("configured policy should build");
        }
    }

    fn sandbox(home: &MemberHome) -> AgentSandbox {
        crate::sandbox::dispatch_linux_helper();
        AgentSandbox::load(home, &endpoint()).expect("default sandbox should load")
    }

    fn endpoint() -> McpEndpoint {
        McpEndpoint::new(
            Url::parse("http://127.0.0.1:12345/mcp/capability")
                .expect("test MCP endpoint should be valid"),
        )
    }
}
