//! Immutable sandbox policy supplied by the application.

use std::{
    env,
    net::IpAddr,
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use zerobox_protocol::permissions::{
    FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxPolicy,
};
#[cfg(target_os = "linux")]
use zerobox_protocol::permissions::FileSystemSpecialPath;
use zerobox_utils_absolute_path::AbsolutePathBuf;
use url::Host;
use zerobox::profile_core::Profile;

use crate::{
    agent::McpEndpoint,
    sandbox::{Config, ExternalNetwork, command::Error},
};

const PROFILES: &[&str] = &[
    "system-read-macos",
    "system-read-linux",
    "deny-credentials",
    "deny-shell-history",
    "deny-shell-configs",
    "deny-keychains-macos",
    "deny-keychains-linux",
    "deny-browser-data-macos",
    "deny-browser-data-linux",
    "deny-macos-private",
    "system-write-macos",
    "system-write-linux",
    "user-tools",
    "homebrew-macos",
    "homebrew-linux",
    "rust-runtime",
    "node-runtime",
    "python-runtime",
    "go-runtime",
    "nix-runtime",
];
const CARGO_CREDENTIALS: &[&str] = &["credentials", "credentials.toml"];

/// Application-controlled policy independent of user Zerobox profiles.
#[derive(Debug, Clone, Default)]
pub(super) struct Policy {
    allow_read: Vec<PathBuf>,
    deny_read: Vec<PathBuf>,
    allow_write: Vec<PathBuf>,
    deny_write: Vec<PathBuf>,
    mcp_host: String,
    mcp_port: u16,
}

impl Policy {
    pub(super) fn load(cwd: &Path, endpoint: &McpEndpoint) -> Result<Self, Error> {
        let home = BaseDirs::new()
            .map(|directories| directories.home_dir().to_owned())
            .ok_or(Error::HomeUnavailable)?;
        let temporary = env::temp_dir();
        let (mcp_host, mcp_port) = Self::mcp_address(endpoint)?;
        let mut policy = Self {
            mcp_host,
            mcp_port,
            ..Self::default()
        };

        for name in PROFILES {
            let mut profile = Self::embedded_profile(name)?;
            if !Self::matches_platform(&profile) {
                continue;
            }
            zerobox::profile_core::expand_profile(&mut profile, &home, cwd, &temporary);
            policy.extend(name, profile)?;
        }
        policy.deny_cargo_credentials(&home);

        Ok(policy)
    }

    pub(super) fn validate(&self, config: &Config) -> Result<(), Error> {
        for allowed in config.allow_read() {
            self.validate_allow(allowed, self.deny_read.iter())?;
        }
        for allowed in config.allow_write() {
            self.validate_allow(allowed, self.deny_read.iter().chain(&self.deny_write))?;
        }

        Ok(())
    }

    fn validate_allow<'a>(
        &self,
        allowed: &Path,
        denied: impl Iterator<Item = &'a PathBuf>,
    ) -> Result<(), Error> {
        let allowed_absolute = Self::absolute(allowed)?;
        for denied in denied {
            let denied_absolute = Self::absolute(denied)?;
            if allowed_absolute
                .as_path()
                .starts_with(denied_absolute.as_path())
            {
                return Err(Error::FixedDenyOverlap {
                    allowed: allowed.into(),
                    denied: denied.clone().into(),
                });
            }
        }

        Ok(())
    }

    pub(super) fn file_system(
        &self,
        config: &Config,
        workspaces: &Path,
        helper: Option<&Path>,
    ) -> Result<FileSystemSandboxPolicy, Error> {
        let mut entries = Vec::new();
        #[cfg(target_os = "linux")]
        entries.push(FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Minimal,
            },
            access: FileSystemAccessMode::Read,
        });
        Self::extend_entries(
            &mut entries,
            self.allow_read
                .iter()
                .chain(config.allow_read())
                .map(PathBuf::as_path),
            FileSystemAccessMode::Read,
        )?;
        Self::extend_entries(
            &mut entries,
            self.deny_read
                .iter()
                .chain(config.deny_read())
                .map(PathBuf::as_path),
            FileSystemAccessMode::None,
        )?;
        Self::extend_entries(
            &mut entries,
            self.allow_write
                .iter()
                .chain(config.allow_write())
                .map(PathBuf::as_path)
                .chain(std::iter::once(workspaces)),
            FileSystemAccessMode::Write,
        )?;
        Self::extend_entries(
            &mut entries,
            self.deny_write
                .iter()
                .chain(config.deny_write())
                .map(PathBuf::as_path),
            FileSystemAccessMode::Read,
        )?;
        let policy = FileSystemSandboxPolicy::restricted(entries);

        Ok(match helper.and_then(Path::parent) {
            Some(root) => {
                policy.with_additional_readable_roots(workspaces, &[Self::absolute(root)?])
            }
            None => policy,
        })
    }

    pub(super) fn external_network(&self, config: &Config) -> NetworkPolicy {
        NetworkPolicy::new(
            config.external_network().clone(),
            self.mcp_host.clone(),
            self.mcp_port,
        )
    }

    fn extend_entries<'a>(
        entries: &mut Vec<FileSystemSandboxEntry>,
        paths: impl Iterator<Item = &'a Path>,
        access: FileSystemAccessMode,
    ) -> Result<(), Error> {
        for path in paths {
            entries.push(FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: Self::absolute(path)?,
                },
                access,
            });
        }

        Ok(())
    }

    fn absolute(path: &Path) -> Result<AbsolutePathBuf, Error> {
        AbsolutePathBuf::from_absolute_path_checked(path).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    }

    fn embedded_profile(name: &str) -> Result<Profile, Error> {
        let json = zerobox::profile_core::builtin_profiles()
            .iter()
            .find_map(|(candidate, json)| (*candidate == name).then_some(*json))
            .ok_or_else(|| Error::BuiltInProfile {
                profile: name.to_owned(),
                reason: "profile is absent".to_owned(),
            })?;
        serde_json::from_str(json).map_err(|error| Error::BuiltInProfile {
            profile: name.to_owned(),
            reason: error.to_string(),
        })
    }

    fn matches_platform(profile: &Profile) -> bool {
        match profile.platform.as_deref() {
            None => true,
            Some("macos") => cfg!(target_os = "macos"),
            Some("linux") => cfg!(target_os = "linux"),
            Some(_) => false,
        }
    }

    fn extend(&mut self, name: &str, profile: Profile) -> Result<(), Error> {
        if !profile.uses.is_empty()
            || profile.allow_all.unwrap_or(false)
            || profile.no_sandbox.unwrap_or(false)
            || profile.allow_net.is_some()
        {
            return Err(Error::BuiltInProfile {
                profile: name.to_owned(),
                reason: "profile can weaken the fixed policy".to_owned(),
            });
        }
        self.allow_read.extend(
            profile
                .allow_read
                .unwrap_or_default()
                .into_iter()
                .map(PathBuf::from),
        );
        self.deny_read.extend(
            profile
                .deny_read
                .unwrap_or_default()
                .into_iter()
                .map(PathBuf::from),
        );
        self.allow_write.extend(
            profile
                .allow_write
                .unwrap_or_default()
                .into_iter()
                .map(PathBuf::from),
        );
        self.deny_write.extend(
            profile
                .deny_write
                .unwrap_or_default()
                .into_iter()
                .map(PathBuf::from),
        );

        Ok(())
    }

    fn deny_cargo_credentials(&mut self, home: &Path) {
        for name in CARGO_CREDENTIALS {
            let path = home.join(".cargo").join(name);
            self.deny_read.push(path.clone());
            self.deny_write.push(path);
        }
    }

    fn mcp_address(endpoint: &McpEndpoint) -> Result<(String, u16), Error> {
        let url = endpoint.url();
        let host = match url.host() {
            Some(Host::Ipv4(host)) => Some(IpAddr::V4(host)),
            Some(Host::Ipv6(host)) => Some(IpAddr::V6(host)),
            Some(Host::Domain(_)) | None => None,
        }
        .filter(IpAddr::is_loopback);
        if url.scheme() == "http"
            && url.port().is_some()
            && let (Some(host), Some(port)) = (host, url.port())
        {
            return Ok((host.to_string(), port));
        }

        Err(Error::McpEndpoint)
    }
}

#[derive(Debug, Clone)]
pub(super) struct NetworkPolicy {
    access: ExternalNetwork,
    mcp_host: String,
    mcp_port: u16,
}

impl NetworkPolicy {
    pub(super) const fn new(access: ExternalNetwork, mcp_host: String, mcp_port: u16) -> Self {
        Self {
            access,
            mcp_host,
            mcp_port,
        }
    }

    pub(super) const fn is_restricted(&self) -> bool {
        !matches!(self.access, ExternalNetwork::All)
    }

    pub(super) fn mcp_address(&self) -> (&str, u16) {
        (&self.mcp_host, self.mcp_port)
    }

    pub(super) fn domains(&self) -> &[String] {
        match &self.access {
            ExternalNetwork::Domains(domains) => domains,
            ExternalNetwork::All | ExternalNetwork::Disabled => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use crate::{
        agent::McpEndpoint,
        sandbox::{Config, ExternalNetwork, policy::Policy},
    };

    #[test]
    fn network_policy_keeps_mcp_separate_from_configured_domains() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let policy =
            Policy::load(directory.path(), &endpoint()).expect("embedded policy should load");
        let mut config = Config::default();
        config.set_external_network(ExternalNetwork::Domains(vec!["example.com".to_owned()]));
        let network = policy.external_network(&config);

        assert!(network.is_restricted());
        assert_eq!(network.mcp_address(), ("127.0.0.1", 12345));
        assert_eq!(network.domains(), ["example.com"]);
    }

    #[test]
    fn cargo_credentials_are_always_denied() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let policy =
            Policy::load(directory.path(), &endpoint()).expect("embedded policy should load");
        let home = directories::BaseDirs::new()
            .expect("home directory should be available")
            .home_dir()
            .join(".cargo");

        for name in super::CARGO_CREDENTIALS {
            let credential = home.join(name);
            assert!(policy.deny_read.contains(&credential));
            assert!(policy.deny_write.contains(&credential));
        }
    }

    #[test]
    fn fixed_denies_reject_nested_local_allows() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let mut policy =
            Policy::load(directory.path(), &endpoint()).expect("embedded policy should load");
        let home = directories::BaseDirs::new()
            .expect("home directory should be available")
            .home_dir()
            .to_owned();
        let mut config = Config::default();
        config.set_allow_read(vec![home.join(".ssh/id_rsa")]);

        assert!(matches!(
            policy.validate(&config),
            Err(crate::sandbox::Error::FixedDenyOverlap { .. })
        ));

        config.set_allow_read(Vec::new());
        config.set_allow_write(vec![home.join(".ssh/id_rsa")]);

        assert!(matches!(
            policy.validate(&config),
            Err(crate::sandbox::Error::FixedDenyOverlap { .. })
        ));

        let fixed_write = home.join("fixed-write");
        policy.deny_write.push(fixed_write.clone());
        config.set_allow_write(vec![fixed_write.join("nested")]);

        assert!(matches!(
            policy.validate(&config),
            Err(crate::sandbox::Error::FixedDenyOverlap { .. })
        ));
    }

    #[test]
    fn fixed_denies_reject_dot_dot_traversal_around_local_allows() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let policy =
            Policy::load(directory.path(), &endpoint()).expect("embedded policy should load");
        let home = directories::BaseDirs::new()
            .expect("home directory should be available")
            .home_dir()
            .to_owned();
        let mut config = Config::default();
        config.set_allow_read(vec![home.join(".config/../.ssh/id_rsa")]);

        assert!(matches!(
            policy.validate(&config),
            Err(crate::sandbox::Error::FixedDenyOverlap { .. })
        ));
    }

    fn endpoint() -> McpEndpoint {
        McpEndpoint::new(
            Url::parse("http://127.0.0.1:12345/mcp/capability")
                .expect("test MCP endpoint should be valid"),
        )
    }
}
