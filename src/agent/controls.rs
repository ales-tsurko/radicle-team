//! The model controls a provider file tunes a member with.
//!
//! A provider file names settings and gives each one a value per score. Reading it produces this,
//! and every adapter starts from it: the vocabulary is shared, and only the translation into a
//! particular agent application's configuration is not.
//!
//! The names are checked here, by serde, when a member starts. A file naming something no adapter
//! carries is reported against the file with the names it could have used instead.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{member::ModelTuning, provider::Provider};

/// What a member runs on, once its scores have been read against a provider file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Controls {
    model: String,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
}

impl Controls {
    /// Reads what `provider` makes of a member scored `tuning`.
    ///
    /// # Errors
    ///
    /// Returns an error when the file names no model, names a setting no adapter carries, or gives
    /// one a value of the wrong kind.
    pub(crate) fn read(provider: &Provider, tuning: ModelTuning) -> Result<Self, Error> {
        let named = provider.settings(tuning).into_iter().collect::<Map<_, _>>();
        // The settings are listed with the failure, because serde names the value it choked on but
        // not always the setting it belongs to, and the operator has to know where to look.
        let listed = named.keys().cloned().collect::<Vec<_>>().join(", ");

        serde_json::from_value(Value::Object(named)).map_err(|error| Error {
            reason: error.to_string(),
            settings: listed,
        })
    }

    /// Returns the model to run, as the `provider/model` the agent application knows it by.
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    /// Returns how hard the model should think before answering.
    ///
    /// Absent for a provider whose models take no such control, which is how a file says a score
    /// moves nothing here.
    pub(crate) fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    /// Returns how far the model may range while answering.
    pub(crate) const fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    /// Returns the share of the distribution the model may sample from.
    pub(crate) const fn top_p(&self) -> Option<f64> {
        self.top_p
    }
}

/// A provider file does not describe a member this crate can run.
///
/// Carries serde's own message, which names the setting it could not use and lists the ones it
/// knows. The file is the operator's, so the message has to be about the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the provider cannot tune this member: {reason}; it sets {settings}")]
pub(crate) struct Error {
    /// What serde could not do with the settings.
    reason: String,
    /// The settings the provider file gave, so the operator knows which to look at.
    settings: String,
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::path::PathBuf;

    use crate::{agent::controls::Controls, member::Member, provider};

    #[test]
    fn reads_the_controls_a_member_runs_on() {
        let controls = read(
            "controls-full",
            r#"
adapter = "opencode"

[auth]
ways = ["none"]

[INT]
param = "model"
values = ["p/one", "p/two", "p/three", "p/four", "p/five"]

[ACC]
param = "reasoning_effort"
values = ["none", "low", "medium", "high", "max"]
"#,
        )
        .expect("controls should be read");

        // Nico is 3 sharp and 2 accurate, so each control is read at its own score.
        assert_eq!(controls.model(), "p/three");
        assert_eq!(controls.reasoning_effort(), Some("low"));
        assert_eq!(controls.temperature(), None);
        assert_eq!(controls.top_p(), None);
    }

    // A provider file is written by hand, so every way of getting it wrong belongs to the file.
    #[test]
    fn refuses_a_file_which_does_not_describe_a_member() {
        let unknown = read(
            "controls-unknown",
            r#"
adapter = "opencode"

[auth]
ways = ["none"]

[INT]
param = "model"
values = ["p/one", "p/two", "p/three", "p/four", "p/five"]

[ACC]
param = "presence_penalty"
values = [0.1, 0.2, 0.3, 0.4, 0.5]
"#,
        )
        .expect_err("a setting no adapter carries must be refused");
        assert!(
            unknown.to_string().contains("presence_penalty")
                && unknown.to_string().contains("reasoning_effort"),
            "the message must name the setting and what it could have been: {unknown}"
        );

        let mistyped = read(
            "controls-mistyped",
            r#"
adapter = "opencode"

[auth]
ways = ["none"]

[INT]
param = "model"
values = [1, 2, 3, 4, 5]
"#,
        )
        .expect_err("a model which is not a name must be refused");
        assert!(mistyped.to_string().contains("model"), "{mistyped}");

        let modelless = read(
            "controls-modelless",
            r#"
adapter = "opencode"

[auth]
ways = ["none"]

[ACC]
param = "temperature"
values = [0.9, 0.7, 0.5, 0.3, 0.1]
"#,
        )
        .expect_err("a file naming no model must be refused");
        assert!(modelless.to_string().contains("model"), "{modelless}");
    }

    fn read(name: &str, body: &str) -> Result<Controls, super::Error> {
        Controls::read(&written(name, body), Member::Nico.profile().tuning)
    }

    fn written(name: &str, body: &str) -> crate::provider::Provider {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(name);
        let _removed = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("test directory should be created");
        std::fs::write(provider::path(&directory, name), body).expect("provider should be written");

        provider::find(&directory, name).expect("provider should be read")
    }
}
