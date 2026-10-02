//! Native provider sign-in shared by a member's roles.

use std::{env, process::ExitStatus, time::Duration};

use serde::Deserialize;
use tokio::process::Command;

use crate::{
    home::MemberHome,
    identity::permissions::Permissions,
    member::Member,
    provider::{Credentials, Provider, Way},
};

#[derive(Debug)]
pub(crate) struct Authentication<'a> {
    provider: &'a Provider,
    member: Member,
    home: &'a MemberHome,
}

impl<'a> Authentication<'a> {
    pub(crate) const fn new(provider: &'a Provider, member: Member, home: &'a MemberHome) -> Self {
        Self {
            provider,
            member,
            home,
        }
    }

    pub(crate) async fn authorized(&self, held: Option<&Credentials>) -> Result<bool, Error> {
        if !self.provider.needs_authorizing() {
            return Ok(true);
        }
        match held {
            Some(held) if !self.provider.ways().contains(&held.way()) => Ok(false),
            Some(held) if held.way() == Way::Key => {
                Ok(self.provider.key_variables().iter().all(|name| {
                    held.variables()
                        .get(name)
                        .is_some_and(|value| !value.trim().is_empty())
                }))
            }
            Some(held) if held.way() == Way::Subscription => self.signed_in().await,
            _ => Ok(false),
        }
    }

    pub(crate) async fn login(&self) -> Result<(), Error> {
        if self.signed_in().await? {
            return Ok(());
        }
        let mut command = self.command()?;
        match self.provider.adapter() {
            "opencode" => {
                command.args(["auth", "login", &self.integration()?, "--standalone"]);
            }
            "claude-code" => {
                command.args(["auth", "login", "--claudeai"]);
            }
            _ => return Err(Error::Unsupported(self.provider.adapter().into())),
        }
        let status = command.status().await?;
        if !status.success() {
            return Err(Error::Command {
                program: self.program()?,
                status,
            });
        }
        if !self.signed_in().await? {
            return Err(Error::Incomplete);
        }
        Ok(())
    }

    async fn signed_in(&self) -> Result<bool, Error> {
        let mut command = self.command()?;
        match self.provider.adapter() {
            "opencode" => {
                command.args(["auth", "list", "--standalone", "--format", "json"]);
            }
            "claude-code" => {
                command.args(["auth", "status", "--json"]);
            }
            _ => return Err(Error::Unsupported(self.provider.adapter().into())),
        }
        let output = tokio::time::timeout(Duration::from_secs(30), command.output())
            .await
            .map_err(Error::Timeout)??;
        self.status(&output)
    }

    fn status(&self, output: &std::process::Output) -> Result<bool, Error> {
        // Claude reports a missing sign-in with status 1 and a valid JSON response.
        if self.provider.adapter() == "claude-code" {
            let status: ClaudeStatus = serde_json::from_slice(&output.stdout)?;
            return Ok(status.logged_in && status.auth_method.as_deref() == Some("claude.ai"));
        }
        if !output.status.success() {
            return Err(Error::Command {
                program: self.program()?,
                status: output.status,
            });
        }
        let integrations: Vec<Integration> = serde_json::from_slice(&output.stdout)?;
        let id = self.integration()?;
        Ok(integrations.iter().any(|integration| {
            integration.id == id
                && integration.connections.iter().any(|connection| {
                    connection.kind == "credential"
                        && connection.method.as_deref() == Some("oauth")
                        && connection.status.is_none()
                })
        }))
    }

    fn integration(&self) -> Result<String, Error> {
        let model = self
            .provider
            .settings(self.member.profile().tuning)
            .into_iter()
            .find(|(name, _)| name == "model")
            .and_then(|(_, value)| value.as_str().map(str::to_owned))
            .ok_or(Error::MissingModel)?;
        model
            .split_once('/')
            .filter(|(id, model)| !id.is_empty() && !model.is_empty())
            .map(|(id, _)| id.to_owned())
            .ok_or(Error::MissingModel)
    }

    fn program(&self) -> Result<&'static str, Error> {
        match self.provider.adapter() {
            "opencode" => Ok("opencode"),
            "claude-code" => Ok("claude"),
            other => Err(Error::Unsupported(other.into())),
        }
    }

    fn command(&self) -> Result<Command, Error> {
        let program = self.program()?;
        let root = self.home.provider_state(self.provider.adapter());
        for path in [
            &root,
            &root.join("config"),
            &root.join("data"),
            &root.join("state"),
            &self.home.cache(),
        ] {
            Permissions::secure_directory(path)?;
        }
        let mut command = Command::new(program);
        command.env_clear().kill_on_drop(true).current_dir(&root);
        for name in ["PATH", "USER", "SHELL", "TERM", "LANG", "LC_ALL", "TMPDIR"] {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", self.home.cache());
        match self.provider.adapter() {
            "opencode" => {
                command
                    .env("OPENCODE_CONFIG_PROJECT_DISABLE", "1")
                    .env("OPENCODE_DISABLE_AUTOUPDATE", "1");
            }
            "claude-code" => {
                command
                    .env("CLAUDE_CONFIG_DIR", &root)
                    .env("DISABLE_AUTOUPDATER", "1")
                    .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
            }
            _ => {}
        }
        Ok(command)
    }
}

#[derive(Deserialize)]
struct Integration {
    id: String,
    connections: Vec<Connection>,
}

#[derive(Deserialize)]
struct Connection {
    #[serde(rename = "type")]
    kind: String,
    method: Option<String>,
    status: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeStatus {
    logged_in: bool,
    auth_method: Option<String>,
}

/// Error checking or establishing native provider authorization.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Starting or reading a native command failed.
    #[error("native provider authorization failed: {0}")]
    Io(#[from] std::io::Error),
    /// The private authorization directory could not be secured.
    #[error(transparent)]
    Permissions(#[from] crate::identity::permissions::Error),
    /// Native status output is invalid.
    #[error("could not read native authorization status: {0}")]
    Json(#[from] serde_json::Error),
    /// No native login implementation exists for this adapter.
    #[error("adapter '{0}' does not support native login")]
    Unsupported(String),
    /// OpenCode cannot derive the integration from the configured model.
    #[error("OpenCode subscription login requires a model in integration/model format")]
    MissingModel,
    /// The native command exited unsuccessfully.
    #[error("{program} authorization command exited with {status}")]
    Command {
        /// Native executable.
        program: &'static str,
        /// Process exit status.
        status: ExitStatus,
    },
    /// The native status command exceeded its time limit.
    #[error("native authorization status timed out: {0}")]
    Timeout(#[source] tokio::time::error::Elapsed),
    /// The selected subscription is still unauthorized after login.
    #[error("login finished without authorizing the configured subscription")]
    Incomplete,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        os::unix::process::ExitStatusExt as _,
        path::PathBuf,
        process::{ExitStatus, Output},
    };

    use crate::{
        agent::{
            Layout, Role,
            authentication::{Authentication, Error},
        },
        home::MemberHome,
        member::Member,
        provider::{Credentials, Provider},
    };

    fn provider(adapter: &str) -> Provider {
        toml_edit::de::from_str(&format!(
            r#"adapter = "{adapter}"
[auth]
ways = ["subscription", "key"]
[auth.key]
env = ["OPENAI_API_KEY"]
[INT]
param = "model"
values = ["openai/test", "openai/test", "openai/test", "openai/test", "openai/test"]
"#
        ))
        .expect("test provider should parse")
    }

    fn output(json: &str, code: i32) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: json.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn accepts_only_the_configured_native_subscription() {
        let home = MemberHome::at("/tmp/member");
        let selected = provider("opencode");
        let auth = Authentication::new(&selected, Member::Maya, &home);
        assert!(
            auth.status(&output(
                r#"[{"id":"openai","connections":[{"type":"credential","method":"oauth"}]}]"#,
                0
            ))
            .expect("status should parse")
        );
        for json in [
            "[]",
            r#"[{"id":"anthropic","connections":[{"type":"credential","method":"oauth"}]}]"#,
            r#"[{"id":"openai","connections":[{"type":"credential","method":"key"}]}]"#,
            r#"[{"id":"openai","connections":[{"type":"credential","method":"oauth","status":{"status":"needs_auth"}}]}]"#,
        ] {
            assert!(!auth.status(&output(json, 0)).expect("status should parse"));
        }
        assert!(matches!(
            auth.status(&output("unavailable", 1)),
            Err(Error::Command { .. })
        ));
        assert!(matches!(
            auth.status(&output("broken", 0)),
            Err(Error::Json(_))
        ));
        let selected = provider("claude-code");
        let auth = Authentication::new(&selected, Member::Maya, &home);
        assert!(
            auth.status(&output(r#"{"loggedIn":true,"authMethod":"claude.ai"}"#, 0))
                .expect("status should parse")
        );
        assert!(
            !auth
                .status(&output(r#"{"loggedIn":false,"authMethod":"none"}"#, 1))
                .expect("signed-out status should parse")
        );
        assert!(
            !auth
                .status(&output(r#"{"loggedIn":true,"authMethod":"api_key"}"#, 0))
                .expect("status should parse")
        );
    }

    #[tokio::test]
    async fn validates_keys_without_reading_a_personal_native_sign_in() {
        let home = MemberHome::at("/does/not/exist");
        let selected = provider("opencode");
        let auth = Authentication::new(&selected, Member::Maya, &home);
        assert!(!auth.authorized(None).await.expect("no credentials"));
        assert!(
            !auth
                .authorized(Some(&Credentials::key(BTreeMap::new())))
                .await
                .expect("empty credentials")
        );
        let credentials = Credentials::key(BTreeMap::from([(
            "OPENAI_API_KEY".into(),
            "test-only".into(),
        )]));
        assert!(
            auth.authorized(Some(&credentials))
                .await
                .expect("valid local key")
        );
    }

    #[test]
    fn native_commands_and_roles_share_member_state_and_ignore_personal_environment() {
        let root = tempfile::tempdir().expect("member root");
        let home = MemberHome::at(root.path());
        home.create().expect("member directories");
        for adapter in ["opencode", "claude-code"] {
            let selected = provider(adapter);
            let auth = Authentication::new(&selected, Member::Maya, &home);
            let command = auth.command().expect("native command");
            let variables = command
                .as_std()
                .get_envs()
                .map(|(name, value)| {
                    (
                        name.to_string_lossy().into_owned(),
                        value.map(|value| value.to_string_lossy().into_owned()),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            assert_eq!(
                variables.get("HOME"),
                Some(&Some(home.provider_state(adapter).display().to_string()))
            );
            assert!(!variables.contains_key("OPENAI_API_KEY"));
            assert!(!variables.contains_key("OPENCODE_CONFIG"));
            assert!(!variables.contains_key("RAD_HOME"));
            for role in Role::ALL {
                let layout = Layout::new(role, home.cache(), home.harness(role), Vec::new());
                assert_eq!(layout.provider_state(adapter), home.provider_state(adapter));
            }
        }
    }
    #[tokio::test]
    async fn performs_one_native_login_for_all_roles() {
        const FLAG: &str = "RAD_TEAM_TEST_NATIVE_LOGIN";
        if let Some(path) = std::env::var_os(FLAG) {
            let home = MemberHome::at(path);
            home.create().expect("member home");
            for adapter in ["opencode", "claude-code"] {
                let selected = provider(adapter);
                let auth = Authentication::new(&selected, Member::Maya, &home);
                assert!(
                    !auth
                        .authorized(Some(&Credentials::subscription()))
                        .await
                        .expect("signed out")
                );
                crate::provider::credentials::store(
                    &crate::provider::credentials::directory(home.path()),
                    adapter,
                    &Credentials::subscription(),
                )
                .expect("existing subscription choice");
                crate::cli::provider::Authorization::new(
                    &selected,
                    home.path(),
                    adapter,
                    Member::Maya,
                    &home,
                )
                .authorize()
                .await
                .expect("native login through member authorization");
                auth.login().await.expect("reuse the existing login");
                assert!(
                    auth.authorized(Some(&Credentials::subscription()))
                        .await
                        .expect("signed in")
                );
                let root = home.provider_state(adapter);
                assert_eq!(
                    std::fs::read_to_string(root.join("login-count"))
                        .expect("login count")
                        .lines()
                        .count(),
                    1
                );
            }
            return;
        }
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("test root");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).expect("fake native binaries");
        let script = r#"#!/bin/sh
set -eu
test -z "${OPENAI_API_KEY-}"
test -z "${RAD_HOME-}"
case "$1 $2" in
  'auth login')
    case "$(basename "$0")" in
      opencode) test "$3" = openai; test "$4" = --standalone ;;
      claude) test "$3" = --claudeai ;;
    esac
    touch "$HOME/signed-in"
    echo login >> "$HOME/login-count"
    ;;
  'auth list')
    if test -f "$HOME/signed-in"; then
      echo '[{"id":"openai","connections":[{"type":"credential","method":"oauth"}]}]'
    else echo '[]'; fi
    ;;
  'auth status')
    if test -f "$HOME/signed-in"; then echo '{"loggedIn":true,"authMethod":"claude.ai"}';
    else echo '{"loggedIn":false,"authMethod":"none"}'; exit 1; fi
    ;;
  *) exit 2 ;;
esac
"#;
        for name in ["opencode", "claude"] {
            let file = bin.join(name);
            std::fs::write(&file, script).expect("fake native adapter");
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700))
                .expect("executable");
        }
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "agent::authentication::tests::performs_one_native_login_for_all_roles",
                "--nocapture",
            ])
            .env(FLAG, root.path().join("member"))
            .env(
                "PATH",
                std::env::join_paths([bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
                    .expect("test PATH"),
            )
            .env("OPENAI_API_KEY", "must-not-be-inherited")
            .env("RAD_HOME", "/must/not/be/read")
            .output()
            .expect("isolated native login test");
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
