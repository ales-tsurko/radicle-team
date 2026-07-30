//! Administrator-owned sandbox configuration.

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{home::MemberHome, identity::permissions::Permissions};

const SCHEMA_VERSION: u32 = 1;

/// Host-local additions to the built-in sandbox policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    allow_read: Vec<PathBuf>,
    deny_read: Vec<PathBuf>,
    allow_write: Vec<PathBuf>,
    deny_write: Vec<PathBuf>,
    external_network: ExternalNetwork,
}

impl Config {
    /// Loads the selected member's sandbox configuration or returns the default policy.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing configuration is unsafe, unreadable, or invalid.
    pub fn load(home: &MemberHome) -> Result<Self, ConfigError> {
        Self::load_optional(&home.sandbox_config()).map(Option::unwrap_or_default)
    }

    /// Loads a sandbox configuration when the file exists.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing configuration is unsafe, unreadable, or invalid.
    pub fn load_optional(path: &Path) -> Result<Option<Self>, ConfigError> {
        match fs::symlink_metadata(path) {
            Ok(_) => Self::load_file(path).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ConfigError::Io {
                path: path.into(),
                source,
            }),
        }
    }

    /// Writes the configuration as a private administrator-owned file.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization, validation, or writing fails.
    pub fn write(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let contents = toml_edit::ser::to_string_pretty(&Document::from(self))?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.into(),
            source,
        })?;

        match fs::symlink_metadata(path) {
            Ok(_) => Self::replace(path, &contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::create(path, &contents),
            Err(source) => Err(ConfigError::Io {
                path: path.into(),
                source,
            }),
        }
    }

    /// Returns additional readable paths.
    #[must_use]
    pub fn allow_read(&self) -> &[PathBuf] {
        &self.allow_read
    }

    /// Returns paths that must not be read.
    #[must_use]
    pub fn deny_read(&self) -> &[PathBuf] {
        &self.deny_read
    }

    /// Returns additional writable paths.
    #[must_use]
    pub fn allow_write(&self) -> &[PathBuf] {
        &self.allow_write
    }

    /// Returns paths that must not be written.
    #[must_use]
    pub fn deny_write(&self) -> &[PathBuf] {
        &self.deny_write
    }

    /// Returns the external network policy.
    #[must_use]
    pub const fn external_network(&self) -> &ExternalNetwork {
        &self.external_network
    }

    /// Replaces the additional readable paths.
    pub fn set_allow_read(&mut self, paths: Vec<PathBuf>) {
        self.allow_read = paths;
    }

    /// Replaces the denied readable paths.
    pub fn set_deny_read(&mut self, paths: Vec<PathBuf>) {
        self.deny_read = paths;
    }

    /// Replaces the additional writable paths.
    pub fn set_allow_write(&mut self, paths: Vec<PathBuf>) {
        self.allow_write = paths;
    }

    /// Replaces the denied writable paths.
    pub fn set_deny_write(&mut self, paths: Vec<PathBuf>) {
        self.deny_write = paths;
    }

    /// Replaces the external network policy.
    pub fn set_external_network(&mut self, external_network: ExternalNetwork) {
        self.external_network = external_network;
    }

    fn load_file(path: &Path) -> Result<Self, ConfigError> {
        Permissions::administrator_file(path)?;
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.into(),
            source,
        })?;
        let document: Document = toml_edit::de::from_str(&contents)?;
        if document.version != SCHEMA_VERSION {
            return Err(ConfigError::Schema {
                actual: document.version,
            });
        }
        let config = Self::from(document);
        config.validate()?;

        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for path in self
            .allow_read
            .iter()
            .chain(&self.deny_read)
            .chain(&self.allow_write)
            .chain(&self.deny_write)
        {
            if !path.is_absolute() {
                return Err(ConfigError::RelativePath(path.clone()));
            }
        }
        if let ExternalNetwork::Domains(domains) = &self.external_network {
            if domains.is_empty() {
                return Err(ConfigError::EmptyDomains);
            }
            if let Some(domain) = domains.iter().find(|domain| domain.trim().is_empty()) {
                return Err(ConfigError::InvalidDomain(domain.clone()));
            }
        }

        Ok(())
    }

    fn create(path: &Path, contents: &str) -> Result<(), ConfigError> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(path).map_err(|source| ConfigError::Io {
            path: path.into(),
            source,
        })?;
        file.write_all(contents.as_bytes())
            .map_err(|source| ConfigError::Io {
                path: path.into(),
                source,
            })
    }

    fn replace(path: &Path, contents: &str) -> Result<(), ConfigError> {
        Permissions::administrator_file(path)?;
        let metadata = fs::symlink_metadata(path).map_err(|source| ConfigError::Io {
            path: path.into(),
            source,
        })?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut replacement =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| ConfigError::Io {
                path: parent.into(),
                source,
            })?;
        replacement
            .as_file()
            .set_permissions(metadata.permissions())
            .map_err(|source| ConfigError::Io {
                path: path.into(),
                source,
            })?;
        replacement
            .write_all(contents.as_bytes())
            .and_then(|()| replacement.as_file().sync_all())
            .map_err(|source| ConfigError::Io {
                path: path.into(),
                source,
            })?;
        Permissions::administrator_file(path)?;
        replacement
            .persist(path)
            .map(|_file| ())
            .map_err(|error| ConfigError::Io {
                path: path.into(),
                source: error.error,
            })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            allow_read: Vec::new(),
            deny_read: Vec::new(),
            allow_write: Vec::new(),
            deny_write: Vec::new(),
            external_network: ExternalNetwork::All,
        }
    }
}

/// External network access available to sandboxed agents.
///
/// This governs traffic leaving the host only. Loopback stays reachable under every variant, so a
/// sandboxed agent can always connect to local services, including ones this application does not
/// operate. Isolate anything sensitive that listens on loopback by other means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalNetwork {
    /// Allow every external domain.
    All,
    /// Disable external networking, leaving only loopback reachable.
    Disabled,
    /// Allow only the listed external domains.
    Domains(Vec<String>),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    #[serde(default)]
    allow_read: Vec<PathBuf>,
    #[serde(default)]
    deny_read: Vec<PathBuf>,
    #[serde(default)]
    allow_write: Vec<PathBuf>,
    #[serde(default)]
    deny_write: Vec<PathBuf>,
    #[serde(default)]
    external_network: NetworkValue,
}

impl From<Document> for Config {
    fn from(document: Document) -> Self {
        Self {
            allow_read: document.allow_read,
            deny_read: document.deny_read,
            allow_write: document.allow_write,
            deny_write: document.deny_write,
            external_network: document.external_network.into(),
        }
    }
}

impl<'a> From<&'a Config> for Document {
    fn from(config: &'a Config) -> Self {
        Self {
            version: SCHEMA_VERSION,
            allow_read: config.allow_read.clone(),
            deny_read: config.deny_read.clone(),
            allow_write: config.allow_write.clone(),
            deny_write: config.deny_write.clone(),
            external_network: NetworkValue::from(&config.external_network),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum NetworkValue {
    Mode(NetworkMode),
    Domains(Vec<String>),
}

impl Default for NetworkValue {
    fn default() -> Self {
        Self::Mode(NetworkMode::All)
    }
}

impl From<NetworkValue> for ExternalNetwork {
    fn from(value: NetworkValue) -> Self {
        match value {
            NetworkValue::Mode(NetworkMode::All) => Self::All,
            NetworkValue::Mode(NetworkMode::Disabled) => Self::Disabled,
            NetworkValue::Domains(domains) => Self::Domains(domains),
        }
    }
}

impl<'a> From<&'a ExternalNetwork> for NetworkValue {
    fn from(value: &'a ExternalNetwork) -> Self {
        match value {
            ExternalNetwork::All => Self::Mode(NetworkMode::All),
            ExternalNetwork::Disabled => Self::Mode(NetworkMode::Disabled),
            ExternalNetwork::Domains(domains) => Self::Domains(domains.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum NetworkMode {
    All,
    Disabled,
}

/// Error loading or writing local sandbox configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Accessing the configuration failed.
    #[error("failed to access sandbox configuration '{path}': {source}")]
    Io {
        /// Configuration path.
        path: Box<Path>,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// The configuration has unsafe ownership or permissions.
    #[error("unsafe sandbox configuration: {0}")]
    Permissions(#[from] crate::identity::PermissionError),
    /// The configuration is not valid TOML.
    #[error("invalid sandbox configuration: {0}")]
    Parse(#[from] toml_edit::de::Error),
    /// The configuration could not be serialized.
    #[error("failed to serialize sandbox configuration: {0}")]
    Serialization(#[from] toml_edit::ser::Error),
    /// The schema version is unsupported.
    #[error("unsupported sandbox configuration schema {actual}; expected {SCHEMA_VERSION}")]
    Schema {
        /// Unsupported schema version.
        actual: u32,
    },
    /// User-configured paths must be absolute.
    #[error("sandbox path must be absolute: '{}'", .0.display())]
    RelativePath(PathBuf),
    /// A domain allowlist must contain at least one domain.
    #[error("sandbox external network domain list cannot be empty; use 'all' or 'disabled'")]
    EmptyDomains,
    /// A configured domain is empty.
    #[error("invalid empty sandbox external network domain: {0:?}")]
    InvalidDomain(String),
}

#[cfg(test)]
mod tests {
    #[cfg(all(unix, not(miri)))]
    use std::os::unix::fs::PermissionsExt as _;
    #[cfg(not(miri))]
    use std::{fs, path::Path};

    use crate::{
        home::MemberHome,
        member::Member,
        sandbox::{Config, ExternalNetwork},
    };
    #[cfg(not(miri))]
    use crate::sandbox::ConfigError;

    #[test]
    fn defaults_to_unrestricted_network_with_no_local_paths() {
        let config = Config::default();

        assert!(config.allow_read().is_empty());
        assert!(config.deny_read().is_empty());
        assert!(config.allow_write().is_empty());
        assert!(config.deny_write().is_empty());
        assert_eq!(config.external_network(), &ExternalNetwork::All);
    }

    #[test]
    #[cfg(not(miri))]
    fn round_trips_every_policy_value() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let home = MemberHome::at(directory.path(), Member::Nico);
        home.create().expect("member home should be created");
        let mut config = Config::default();
        config.set_allow_read(vec![directory.path().join("toolchain")]);
        config.set_deny_read(vec![directory.path().join("secret")]);
        config.set_allow_write(vec![directory.path().join("cache")]);
        config.set_deny_write(vec![directory.path().join("protected")]);
        config.set_external_network(ExternalNetwork::Domains(vec![
            "crates.io".to_owned(),
            "github.com".to_owned(),
        ]));

        config
            .write(&home.sandbox_config())
            .expect("configuration should be written");
        config.set_external_network(ExternalNetwork::Disabled);
        config
            .write(&home.sandbox_config())
            .expect("configuration should be replaced");

        assert_eq!(
            Config::load(&home).expect("configuration should load"),
            config
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(home.sandbox_config())
                .expect("configuration metadata should be readable")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn missing_configuration_uses_defaults() {
        let home = MemberHome::at("/missing", Member::Nico);

        assert_eq!(
            Config::load(&home).expect("missing configuration should use defaults"),
            Config::default()
        );
    }

    #[test]
    #[cfg(not(miri))]
    fn accepts_disabled_network_and_rejects_relative_paths() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let home = MemberHome::at(directory.path(), Member::Nico);
        home.create().expect("member home should be created");
        fs::write(
            home.sandbox_config(),
            "version = 1\nexternal_network = \"disabled\"\nallow_read = [\"relative\"]\n",
        )
        .expect("configuration should be written");

        assert!(matches!(
            Config::load(&home),
            Err(ConfigError::RelativePath(path)) if path == Path::new("relative")
        ));
    }
}
