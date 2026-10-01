//! Model providers a member can be run on.
//!
//! A provider is a file in a directory, named after the provider. It says which adapter reaches
//! that provider's models, how a member's scores become that provider's settings, and which ways of
//! authorizing it accepts.
//!
//! Nothing here knows a model name or a setting name. Both are strings read out of the file and
//! handed to the adapter untouched, so covering a new model, a new setting, or a whole new provider
//! is an edited file rather than a release. What does live in code is the short, fixed list of ways
//! to authorize, because each is a different thing for the runtime to do.
//!
//! A setting name is a request to the adapter, not a key in the agent application's own
//! configuration. The file is read here and its values handed over as data; the application never
//! sees it. The names are whatever reach the adapter best -- one asks for `reasoning_effort`,
//! another for `effort` -- and how a name becomes a control is the adapter's business alone. Which
//! names an adapter carries is its answer too: one refusing to start over a name it cannot use
//! beats one silently dropping it.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use serde::Deserialize;

use crate::{
    identity::permissions::Permissions,
    member::{ModelTuning, Stat},
};
pub use crate::provider::credentials::{Credentials, Error as CredentialsError, Way};

/// Every provider file in a directory, each either read or the reason it could not be.
///
/// One unreadable file is one unusable provider, not an unusable directory.
pub type Installed = BTreeMap<String, Result<Provider, Error>>;

/// Directory holding one file per provider, under the user's configuration directory.
pub const DIRECTORY: &str = "providers";
/// Extension every provider file carries. The rest of the name is the provider's name.
pub const EXTENSION: &str = "toml";

// The providers the application ships with, written into the user's directory and replaced when the
// application is updated. A user who wants an edit to survive keeps it under another name.
const SHIPPED: &[(&str, &str)] = &[
    ("anthropic", include_str!("../../providers/anthropic.toml")),
    ("deepseek", include_str!("../../providers/deepseek.toml")),
    ("glm", include_str!("../../providers/glm.toml")),
    ("gpt", include_str!("../../providers/gpt.toml")),
    ("grok", include_str!("../../providers/grok.toml")),
    ("kimi", include_str!("../../providers/kimi.toml")),
    ("ollama", include_str!("../../providers/ollama.toml")),
    ("qwen", include_str!("../../providers/qwen.toml")),
];

/// One provider, as read from its file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Provider {
    adapter: String,
    auth: Auth,
    #[serde(flatten)]
    tune: BTreeMap<Stat, Setting>,
}

impl Provider {
    /// Returns the agent application which reaches this provider's models.
    #[must_use]
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// Returns the ways this provider accepts being authorized.
    #[must_use]
    pub fn ways(&self) -> &[Way] {
        &self.auth.ways
    }

    /// Returns the settings to run `tuning` on, as names and values the adapter passes through.
    ///
    /// A score with no setting in the file contributes nothing. That is how a provider which
    /// rejects a setting expresses it: the entry is absent rather than special-cased here.
    #[must_use]
    pub fn settings(&self, tuning: ModelTuning) -> Vec<(String, serde_json::Value)> {
        self.tune
            .iter()
            .filter_map(|(stat, setting)| {
                setting
                    .at(tuning.score(*stat))
                    .map(|value| (setting.param.clone(), value.clone()))
            })
            .collect()
    }

    /// Returns whether reaching this provider takes anything the operator has to supply.
    #[must_use]
    pub fn needs_authorizing(&self) -> bool {
        self.auth.ways.iter().any(|way| *way != Way::None)
    }

    /// Returns the environment variables the `key` way expects, when the provider offers it.
    #[must_use]
    pub fn key_variables(&self) -> &[String] {
        self.auth.key.as_ref().map_or(&[], |key| &key.env)
    }

    // Rejects a file which is internally inconsistent, so a mistake is reported against the file
    // rather than turning into a wrong setting on a running member.
    fn validate(&self, name: &str) -> Result<(), Error> {
        if self.auth.ways.is_empty() {
            return Err(Error::NoWays(name.to_owned()));
        }
        // Both directions, because either mismatch is a mistake the operator cannot see: a way
        // declared without its settings fails at the first login, and settings for a way nobody
        // declared sit there looking active and are never read.
        for way in Way::ALL {
            match (self.auth.ways.contains(&way), self.auth.settings(way)) {
                (true, Settings::Missing) => {
                    return Err(Error::MissingWaySettings {
                        provider: name.to_owned(),
                        way,
                    });
                }
                (false, Settings::Present) => {
                    return Err(Error::UndeclaredWaySettings {
                        provider: name.to_owned(),
                        way,
                    });
                }
                _ => {}
            }
        }
        let mut named = BTreeMap::new();
        for (stat, setting) in &self.tune {
            if let Some(first) = named.insert(setting.param.clone(), *stat) {
                return Err(Error::RepeatedSetting {
                    provider: name.to_owned(),
                    param: setting.param.clone(),
                    first,
                    second: *stat,
                });
            }
        }
        let expected = usize::from(ModelTuning::HIGHEST);
        for (stat, setting) in &self.tune {
            if setting.values.len() != expected {
                return Err(Error::Scores {
                    provider: name.to_owned(),
                    stat: *stat,
                    found: setting.values.len(),
                    expected,
                });
            }
        }

        Ok(())
    }
}

/// How a provider accepts being authorized.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Auth {
    ways: Vec<Way>,
    #[serde(default)]
    key: Option<KeyAuth>,
    #[serde(default)]
    subscription: Option<SubscriptionAuth>,
}

impl Auth {
    // Reports what this file says about one way, without naming which way it is.
    fn settings(&self, way: Way) -> Settings {
        match way {
            // The only way carrying settings today. A way needing none is satisfied by its own
            // declaration, so it is never missing.
            Way::Key => match self.key.as_ref().is_some_and(|key| !key.env.is_empty()) {
                true => Settings::Present,
                false => Settings::Missing,
            },
            // Nothing to supply today, so an absent section is not a mistake. A section which is
            // there is still checked against the declared ways, or it would sit unread.
            Way::Subscription => match self.subscription.is_some() {
                true => Settings::Present,
                false => Settings::Satisfied,
            },
            Way::None => Settings::Satisfied,
        }
    }
}

// What a provider file says about one way of authorizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settings {
    /// Settings this way needs are there.
    Present,
    /// Settings this way needs are absent.
    Missing,
    /// This way needs none, so it is neither.
    Satisfied,
}

/// Settings for authorizing with a key the operator holds.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyAuth {
    env: Vec<String>,
}

/// Settings for authorizing with an account the operator signs into.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscriptionAuth {}

/// One setting, and the value it takes at each score.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Setting {
    param: String,
    values: Vec<serde_json::Value>,
}

impl Setting {
    // Returns the value for `score`, which the file has already been checked to carry.
    fn at(&self, score: u8) -> Option<&serde_json::Value> {
        self.values.get(usize::from(score).checked_sub(1)?)
    }
}

/// Reads every provider in `directory`, keyed by the name its file carries.
///
/// A file which cannot be read is reported in place of its provider rather than in place of the
/// whole directory. The application tells operators to add their own files here, so one typo in one
/// of them must not take away the providers which are fine -- nor stop setup, which only needs a
/// different one.
///
/// A missing directory is no providers rather than an error, because a machine which has not been
/// set up yet has none.
///
/// # Errors
///
/// Returns an error when the directory itself cannot be listed.
pub fn load(directory: &Path) -> Result<Installed, Error> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Installed::new()),
        Err(source) => {
            return Err(Error::Directory {
                path: directory.to_path_buf(),
                source,
            });
        }
    };
    let mut providers = Installed::new();
    for entry in entries {
        let path = entry
            .map_err(|source| Error::Directory {
                path: directory.to_path_buf(),
                source,
            })?
            .path();
        if path.extension().and_then(OsStr::to_str) != Some(EXTENSION) {
            continue;
        }
        let Some(name) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        let _replaced = providers.insert(name.to_owned(), read(&path, name));
    }

    Ok(providers)
}

/// Reads the provider named `name` from `directory`.
///
/// # Errors
///
/// Returns an error when the provider is not there, or its file cannot be read and validated.
pub fn find(directory: &Path, name: &str) -> Result<Provider, Error> {
    plain_name(name)?;

    read(&path(directory, name), name)
}

/// Returns the directory holding provider files and credentials.
///
/// # Errors
///
/// Returns an error when the platform has no home directory to put it in.
pub fn configuration() -> Result<PathBuf, Error> {
    let directories = BaseDirs::new().ok_or(Error::NoConfigurationDirectory)?;
    let configured = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);

    Ok(configuration_in(
        configured.as_deref(),
        directories.home_dir(),
    ))
}

// Kept apart from reading the environment, so the rule can be checked without setting a variable
// the rest of the suite shares.
//
// `~/.config`, not the platform's own answer. `BaseDirs::config_dir` follows Apple's guidance and
// returns `~/Library/Application Support` on macOS, which is where a windowed application belongs.
// This is a command-line tool, and its configuration belongs beside git's and ssh's.
pub(crate) fn configuration_in(configured: Option<&Path>, home: &Path) -> PathBuf {
    configured
        .map_or_else(|| home.join(".config"), Path::to_path_buf)
        .join(env!("CARGO_PKG_NAME"))
}

/// Writes the providers this build ships with into `directory`, replacing earlier copies.
///
/// Replacing rather than merging, so an update can correct a model name which has been renamed
/// away. Anything the user added under another name is left alone.
///
/// # Errors
///
/// Returns an error when the directory or a file cannot be written.
pub fn install(directory: &Path) -> Result<(), Error> {
    fs::create_dir_all(directory).map_err(|source| Error::Directory {
        path: directory.to_path_buf(),
        source,
    })?;
    for (name, body) in SHIPPED {
        let path = path(directory, name);
        // Compared before writing, so a directory nobody has edited keeps its modification times
        // and a file open in an editor is not rewritten under it on every listing.
        if fs::read_to_string(&path).is_ok_and(|existing| existing == *body) {
            continue;
        }
        // Renamed into place rather than written through. A plain write follows a symlink and
        // overwrites whatever it points at, and a truncating write interrupted part-way leaves a
        // file which parses on no later run and cannot repair itself.
        Permissions::replace(&path, body)?;
    }

    Ok(())
}

/// Returns the directory `root` keeps provider files in.
#[must_use]
pub fn directory(root: &Path) -> PathBuf {
    root.join(DIRECTORY)
}

/// Installs the providers this build ships with under `root` and reads back every one there.
///
/// # Errors
///
/// Returns an error when the directory cannot be written, or a provider cannot be read.
pub fn installed(root: &Path) -> Result<Installed, Error> {
    let directory = directory(root);
    install(&directory)?;

    load(&directory)
}

/// Installs the providers this build ships with under `root` and reads back the one called `name`.
///
/// The rest of the directory is left unparsed, which is the difference from [`installed`].
///
/// # Errors
///
/// Returns an error when the directory cannot be written, or the provider is unknown or unreadable.
pub fn installed_named(root: &Path, name: &str) -> Result<Provider, Error> {
    let directory = directory(root);
    install(&directory)?;

    find(&directory, name)
}

/// Returns where the provider named `name` is read from.
#[must_use]
pub fn path(directory: &Path, name: &str) -> PathBuf {
    directory.join(format!("{name}.{EXTENSION}"))
}

/// Returns whether `name` may be turned into a file name.
///
/// A provider name becomes a file name under the operator's configuration directory. Anything but a
/// plain name is a path, and a path reaches outside that directory -- to read as a provider, and to
/// write an API key into.
#[must_use]
pub fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

// Refuses a name before it is joined onto a directory. Every path which turns an outside string
// into a file under the configuration directory comes through here.
fn plain_name(name: &str) -> Result<(), Error> {
    match is_plain_name(name) {
        true => Ok(()),
        false => Err(Error::Name(name.to_owned())),
    }
}

fn read(path: &Path, name: &str) -> Result<Provider, Error> {
    let body = fs::read_to_string(path).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => Error::Unknown(name.to_owned()),
        _ => Error::Unreadable {
            path: path.to_path_buf(),
            source,
        },
    })?;
    let provider: Provider = toml_edit::de::from_str(&body).map_err(|error| Error::Malformed {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    provider.validate(name)?;

    Ok(provider)
}

/// Error reading a provider.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The provider directory could not be listed.
    #[error("could not read the provider directory '{path}': {source}")]
    Directory {
        /// Directory being listed.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// A provider file could not be read.
    #[error("could not read '{path}': {source}")]
    Unreadable {
        /// Provider file.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// A provider file could not be parsed.
    #[error("'{path}' is not a valid provider: {reason}")]
    Malformed {
        /// Provider file.
        path: PathBuf,
        /// Parse failure.
        reason: String,
    },
    /// The platform has no configuration directory.
    #[error("this platform has no configuration directory to read providers from")]
    NoConfigurationDirectory,
    /// No provider of that name is configured.
    #[error("unknown provider '{0}'; run `rad-team providers` to see which are configured")]
    Unknown(String),
    /// A provider file could not be written.
    #[error(transparent)]
    Permissions(#[from] crate::identity::permissions::Error),
    /// A provider is installed but its file cannot be used.
    #[error("provider '{provider}' cannot be used: {reason}")]
    Unusable {
        /// Provider named by the file.
        provider: String,
        /// Why its file could not be read.
        reason: String,
    },
    /// A provider name could not be turned into a file name.
    #[error(
        "'{0}' is not a usable provider name; use letters, digits, '-', '_', or '.', and do not \
         start with '.'"
    )]
    Name(String),
    /// A provider offers no way of authorizing.
    #[error("provider '{0}' lists no way of authorizing")]
    NoWays(String),
    /// A provider offers a way without the settings that way needs.
    #[error("provider '{provider}' offers '{way}' without the settings it needs")]
    MissingWaySettings {
        /// Provider named by the file.
        provider: String,
        /// Way missing its settings.
        way: Way,
    },
    /// A provider carries settings for a way it does not offer.
    #[error("provider '{provider}' carries settings for '{way}', which it does not offer")]
    UndeclaredWaySettings {
        /// Provider named by the file.
        provider: String,
        /// Way whose settings are never read.
        way: Way,
    },
    /// Two scores name the same setting.
    #[error(
        "provider '{provider}' has both {first} and {second} set '{param}', so one silently \
         replaces the other"
    )]
    RepeatedSetting {
        /// Provider named by the file.
        provider: String,
        /// Setting named twice.
        param: String,
        /// First score to name it.
        first: Stat,
        /// Second score to name it.
        second: Stat,
    },
    /// A setting does not carry one value per score.
    #[error(
        "provider '{provider}' gives {stat} {found} values, but every score from 1 to 5 needs \
         one, so {expected} are required"
    )]
    Scores {
        /// Provider named by the file.
        provider: String,
        /// Score whose setting is wrong.
        stat: Stat,
        /// Values the file carries.
        found: usize,
        /// Values a score needs.
        expected: usize,
    },
}

pub mod credentials;

#[cfg(test)]
mod tests;
