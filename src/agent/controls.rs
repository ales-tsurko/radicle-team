//! The settings a provider file tunes a member with.
//!
//! A provider file names settings and gives each one a value per score. Reading it produces this,
//! and every adapter starts from it. The names are the file's own, handed over untouched: they are
//! requests to whichever adapter the file reaches, in whatever words reach it best, and which of
//! them carry meaning is that adapter's business alone. Nothing here knows a setting name.
//!
//! What an adapter does with them is refuse what it cannot carry. A name its harness has no control
//! for would otherwise sit in the file looking active while doing nothing, so each adapter checks
//! the whole set against the names it takes and reports the rest against the file, together with
//! the names it could have used instead.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::{member::ModelTuning, provider::Provider};

/// What a member runs on, once its scores have been read against a provider file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Controls {
    // Keyed by the name the file wrote, valued exactly as the file gave it.
    settings: BTreeMap<String, Value>,
}

impl Controls {
    /// Reads what `provider` makes of a member scored `tuning`.
    ///
    /// A score with no setting in the file contributes nothing, which is how a provider says a
    /// score moves nothing here.
    #[must_use]
    pub(crate) fn read(provider: &Provider, tuning: ModelTuning) -> Self {
        Self {
            settings: provider.settings(tuning).into_iter().collect(),
        }
    }

    /// Returns the names the file gave, in sorted order.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.settings.keys().map(String::as_str)
    }

    /// Refuses every setting outside `carried`.
    ///
    /// # Errors
    ///
    /// Returns an error naming the first setting no adapter of this kind carries, the names it
    /// could have used, and everything the file gave.
    pub(crate) fn carried(&self, carried: &[&str]) -> Result<(), Error> {
        let unknown = self
            .names()
            .find(|named| !carried.contains(named))
            .map(str::to_owned);
        match unknown {
            None => Ok(()),
            Some(setting) => Err(Error::unknown(setting, carried, self.listed())),
        }
    }

    /// Returns the text the file gives `setting`, when it names it at all.
    ///
    /// # Errors
    ///
    /// Returns an error when the file gives the setting something other than text.
    pub(crate) fn text(&self, setting: &str) -> Result<Option<String>, Error> {
        match self.settings.get(setting) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(other) => Err(Error::kind(setting, "a string", other, self.listed())),
        }
    }

    /// Returns the number the file gives `setting`, when it names it at all.
    ///
    /// # Errors
    ///
    /// Returns an error when the file gives the setting something other than a number.
    pub(crate) fn number(&self, setting: &str) -> Result<Option<f64>, Error> {
        match self.settings.get(setting) {
            None => Ok(None),
            Some(Value::Number(number)) => Ok(number.as_f64()),
            Some(other) => Err(Error::kind(setting, "a number", other, self.listed())),
        }
    }

    // Everything the file gave, for telling the operator where to look.
    fn listed(&self) -> Vec<String> {
        self.settings.keys().cloned().collect()
    }
}

/// A provider file does not describe a member the asking adapter can run.
///
/// Carries what is wrong with the file's settings and lists the ones it gave. The file is the
/// operator's, so the message has to be about the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the provider cannot tune this member: {reason}; it sets {settings}")]
pub(crate) struct Error {
    /// What the settings could not do.
    reason: String,
    /// The settings the provider file gave, so the operator knows which to look at.
    settings: String,
}

impl Error {
    fn unknown(setting: String, carried: &[&str], listed: Vec<String>) -> Self {
        // Names the file already used are not suggestions; whatever else its adapter's vocabulary
        // holds is. There is always something left: a file sets at most one control per score,
        // which no adapter carries fewer names than.
        let expected = carried
            .iter()
            .filter(|named| !listed.iter().any(|given| given.as_str() == **named))
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");

        Self::failing(
            format!(
                "it names `{setting}`, which this adapter carries no control for; it could have \
                 named {expected}"
            ),
            listed,
        )
    }

    fn kind(setting: &str, expected: &str, given: &Value, listed: Vec<String>) -> Self {
        Self::failing(
            format!("`{setting}` must be {expected}, but is {given}"),
            listed,
        )
    }

    /// The error for a file which leaves out a setting every member needs.
    ///
    /// Reached from the adapters rather than from [`Controls`] itself, because which settings are
    /// required is the adapter's answer too.
    pub(crate) fn missing(setting: &str, listed: Vec<String>) -> Self {
        Self::failing(format!("it names no `{setting}`"), listed)
    }

    fn failing(reason: String, listed: Vec<String>) -> Self {
        let settings = listed.join(", ");

        Self { reason, settings }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{agent::controls::Controls, member::Member, provider};

    #[test]
    fn hands_over_the_settings_a_member_runs_on_untouched() {
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
        );

        // Nico is 3 sharp and 2 accurate, so each setting is read at its own score, under the name
        // the file wrote.
        assert_eq!(
            controls.names().collect::<Vec<_>>(),
            ["model", "reasoning_effort"]
        );
        assert_eq!(
            controls.text("model").expect("model should read"),
            Some("p/three".to_owned())
        );
        assert_eq!(
            controls
                .text("reasoning_effort")
                .expect("effort should read"),
            Some("low".to_owned())
        );
        assert_eq!(
            controls.number("temperature").expect("none was named"),
            None
        );
    }

    #[test]
    fn tells_an_adapter_which_names_it_cannot_have_meant() {
        let controls = read(
            "controls-carried",
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
        );

        let error = controls
            .carried(&["model", "reasoning_effort", "temperature", "top_p"])
            .expect_err("a setting the adapter does not carry must be refused");

        let said = error.to_string();
        assert!(
            said.contains("`presence_penalty`") && said.contains("`reasoning_effort`"),
            "the message must name the setting and what it could have been: {said}"
        );
        // The file already named `model`, so it is not among what it could have named; read past
        // that phrase, since `model` appears earlier in the list of what the file set.
        let suggested = said.split("could have named").nth(1).unwrap_or_default();
        assert!(
            suggested.contains("`reasoning_effort`"),
            "what remains of the vocabulary is still suggested: {suggested}"
        );
        assert!(
            !suggested.contains("`model`"),
            "a name the file already used is not a suggestion: {said}"
        );
        assert!(
            controls.carried(&["model", "presence_penalty"]).is_ok(),
            "another adapter may well carry what this one does not"
        );
    }

    #[test]
    fn refuses_a_value_of_the_wrong_kind_by_name() {
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
        );

        let error = mistyped
            .text("model")
            .expect_err("a model which is not a name must be refused");
        assert!(error.to_string().contains("`model`"), "{error}");
    }

    #[test]
    fn lets_the_adapter_say_what_it_cannot_run_without() {
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
        );

        let missing = modelless.text("model").expect("nothing named to fail on");
        let error = crate::agent::controls::Error::missing(
            "model",
            modelless.names().map(str::to_owned).collect(),
        );

        assert_eq!(missing, None);
        assert!(
            error.to_string().contains("`model`"),
            "a file naming no model must be refused by name: {error}"
        );
    }

    fn read(name: &str, body: &str) -> Controls {
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
