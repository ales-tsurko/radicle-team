//! What the operator supplies so a provider will answer.
//!
//! Kept apart from provider definitions so installing or updating model configuration cannot change
//! the operator's saved key or authorization choice.
//! Global by default, because one person's account is one account however many members use it,
//! with a per-member file for when two projects must be billed apart.

use std::{
    collections::BTreeMap,
    fmt, io,
    io::Read as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    identity::permissions::{self, Permissions},
    provider,
};

/// Directory holding one file per authorized provider.
pub const DIRECTORY: &str = "credentials";
/// Extension every credentials file carries. The rest of the name is the provider's name.
pub const EXTENSION: &str = "toml";

/// A way of authorizing with a provider.
///
/// Short and fixed, unlike models and settings, because each is a different thing for the runtime
/// to do rather than a different string to pass along.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Way {
    /// A key the operator holds, placed in the agent's environment.
    Key,
    /// An account the operator signs into, which the agent application keeps signed in.
    Subscription,
    /// Nothing to authorize, as for a model served locally.
    None,
}

impl Way {
    /// Every way, in the order they are offered.
    pub const ALL: [Self; 3] = [Self::Key, Self::Subscription, Self::None];

    /// Returns the name this way is written as.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Subscription => "subscription",
            Self::None => "none",
        }
    }

    /// Returns whether authorizing a provider this way is something to record.
    ///
    /// A key is asked for and stored. A subscription is a note that the agent application holds
    /// the sign-in -- the operator signs in there, and this records which account pays. `none` is
    /// nothing to record.
    #[must_use]
    pub const fn is_performable(self) -> bool {
        matches!(self, Self::Key | Self::Subscription)
    }
}

impl fmt::Display for Way {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

/// What one provider was authorized with.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Credentials {
    way: Way,
    /// Variables to place in the agent's environment, for the `key` way.
    #[serde(flatten, default)]
    variables: BTreeMap<String, String>,
}

// Names the variables it holds and never their values. A secret which can be rendered is a secret
// which eventually is: one `tracing::debug!(?credentials, ..)` is all it takes.
impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("way", &self.way)
            .field("variables", &self.variables.keys())
            .finish()
    }
}

impl Credentials {
    /// Records a key the operator supplied.
    #[must_use]
    pub fn key(variables: BTreeMap<String, String>) -> Self {
        Self {
            way: Way::Key,
            variables,
        }
    }

    /// Records that the agent application holds the sign-in itself.
    #[must_use]
    pub fn subscription() -> Self {
        Self {
            way: Way::Subscription,
            variables: BTreeMap::new(),
        }
    }

    /// Returns which way this provider was authorized with.
    #[must_use]
    pub const fn way(&self) -> Way {
        self.way
    }

    /// Returns the variables to place in the agent's environment.
    ///
    /// Empty for a way which puts nothing there, which is every way but `key`.
    #[must_use]
    pub const fn variables(&self) -> &BTreeMap<String, String> {
        &self.variables
    }
}

/// Reads the credentials for `provider`, preferring `member` over `global`.
///
/// A member file wins outright rather than merging, so a project billed to its own account cannot
/// half-inherit another's.
///
/// # Errors
///
/// Returns an error when a file exists but cannot be read or parsed.
pub fn load(
    global: &Path,
    member: Option<&Path>,
    provider: &str,
) -> Result<Option<Credentials>, Error> {
    if let Some(member) = member
        && let Some(credentials) = read(&path(member, provider))?
    {
        return Ok(Some(credentials));
    }

    read(&path(global, provider))
}

/// Writes the credentials for `provider` into `directory` with private permissions.
///
/// # Errors
///
/// Returns an error when the directory or file cannot be written or secured.
pub fn store(
    directory: &Path,
    provider: &str,
    credentials: &Credentials,
) -> Result<PathBuf, Error> {
    // The name becomes a file name under a directory this call is about to create and chmod. An
    // unchecked one writes the operator's API key outside the tree that was secured.
    if !provider::is_plain_name(provider) {
        return Err(Error::Name(provider.to_owned()));
    }
    Permissions::secure_directory(directory)?;
    let path = path(directory, provider);
    // Never the serializer's own message: it quotes the value it choked on, and here every value
    // is a secret.
    let body = toml_edit::ser::to_string_pretty(credentials)
        .map_err(|_error| Error::Malformed { path: path.clone() })?;
    Permissions::write_private(&path, &body)?;

    Ok(path)
}

/// Returns the directory `root` keeps credentials in.
#[must_use]
pub fn directory(root: &Path) -> PathBuf {
    root.join(DIRECTORY)
}

/// Returns where `provider`'s credentials are read from inside `directory`.
///
/// Its own extension rather than the provider file's: the two name different kinds of file, and
/// changing one must not silently rename every stored key out from under the other.
#[must_use]
pub fn path(directory: &Path, provider: &str) -> PathBuf {
    directory.join(format!("{provider}.{EXTENSION}"))
}

fn read(path: &Path) -> Result<Option<Credentials>, Error> {
    // Every other secret this application reads is gated the same way, and this is the one holding
    // API keys. A file loosened by hand or restored from a backup is refused rather than used.
    // Checked through the open handle, so the file read is the file that was checked.
    let mut file = match Permissions::open_private(path) {
        Ok(file) => file,
        Err(permissions::Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let mut body = String::new();
    let _read = file
        .read_to_string(&mut body)
        .map_err(|source| Error::Unreadable {
            path: path.to_path_buf(),
            source,
        })?;

    // The parse error is dropped, not reported. Its `Display` quotes the line it failed on, and in
    // this file that line is an API key -- which `providers` would then print to a terminal, a log,
    // or a pasted bug report.
    toml_edit::de::from_str(&body)
        .map(Some)
        .map_err(|_error| Error::Malformed {
            path: path.to_path_buf(),
        })
}

/// Error reading or writing credentials.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A credentials file could not be read.
    #[error("could not read '{path}': {source}")]
    Unreadable {
        /// Credentials file.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// A credentials file has unsafe permissions.
    #[error(transparent)]
    Permissions(#[from] crate::identity::permissions::Error),
    /// A provider name could not be turned into a file name.
    #[error("'{0}' is not a usable provider name")]
    Name(String),
    /// A credentials file could not be parsed.
    ///
    /// Deliberately says nothing about why: a TOML parse error quotes the line it failed on, and
    /// in this file that line holds an API key.
    #[error("'{path}' is not a valid credentials file; remove it and authorize again")]
    Malformed {
        /// Credentials file.
        path: PathBuf,
    },
}
