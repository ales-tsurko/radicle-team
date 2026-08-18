//! The configuration file one role runs OpenCode under.
//!
//! [`Controls`] is what every adapter is given. This turns one into OpenCode's own shape: a
//! reasoning effort becomes a variant, and the whole set becomes an agent OpenCode runs the member
//! as.
//!
//! The file also carries everything about the run which is not tuning: which MCP server the member
//! reaches, which documents instruct it, and the settings which keep a headless agent headless.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    agent::{
        Controls, Layout, McpEndpoint,
        opencode::{Error, catalogue::Catalogue},
    },
    member::{ModelTuning, Profile},
    provider::Provider,
};

/// Name OpenCode is told to call the run-scoped MCP server.
///
/// OpenCode prefixes the tools it takes from a server with that server's name, so this is the word
/// the agent sees in front of every Radicle tool.
pub(super) const SERVER: &str = "radicle";
/// File OpenCode reads a role's configuration from.
pub(super) const FILE: &str = "opencode.json";

const SCHEMA: &str = "https://opencode.ai/config.json";
// What the plugin writes and the adapter reads, inside the role's own harness home.
const REPORT: &str = "progress.jsonl";
// Where OpenCode looks for plugins under the home it is given, and the name this one is written as.
// Anything in that directory is loaded; the `plugin` setting is for packages instead.
const PLUGINS: [&str; 3] = [".config", "opencode", "plugins"];
const PLUGIN: &str = "progress.js";
// The path is written into the plugin when it is installed, because a plugin is handed no
// configuration of its own.
const PLUGIN_SOURCE: &str = include_str!("progress.js");
const PLUGIN_REPORT: &str = "\"__REPORT__\"";

/// The controls one role runs with, as OpenCode takes them.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Settings {
    controls: Controls,
    // The effort actually sent, once it has been fitted to what this model offers.
    variant: Option<String>,
}

impl Settings {
    /// Reads what `provider` makes of a member scored `tuning`.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider file does not describe a member this crate can run.
    pub(super) fn read(provider: &Provider, tuning: ModelTuning) -> Result<Self, Error> {
        let controls = Controls::read(provider, tuning)?;

        Ok(Self {
            variant: controls.reasoning_effort().map(str::to_owned),
            controls,
        })
    }

    /// Returns the `provider/model` OpenCode is asked to run.
    pub(super) fn model(&self) -> &str {
        self.controls.model()
    }

    /// Fits the member's reasoning effort to what its own model takes.
    ///
    /// A ladder can climb from a model which does not reason to one which does, and two models of
    /// one provider rarely offer the same efforts. Sending one an effort it does not know is an
    /// error from the provider on the first delivery, so what is sent is what [`Catalogue::effort`]
    /// settles on, and a model with no efforts is sent none.
    pub(super) fn fit(&mut self, catalogue: &Catalogue) {
        let Some(wanted) = self.controls.reasoning_effort() else {
            return;
        };
        let fitted = catalogue.effort(self.model(), wanted);
        match &fitted {
            Some(effort) if effort == wanted => {}
            Some(effort) => tracing::info!(
                model = self.model(),
                wanted,
                effort,
                "the model does not take this member's reasoning effort; using the nearest it does"
            ),
            None => tracing::info!(
                model = self.model(),
                wanted,
                "the model takes no reasoning effort; it will run at its own default"
            ),
        }
        self.variant = fitted;
    }

    /// Writes the configuration this role runs under to `path`.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written.
    pub(super) fn write(
        &self,
        path: &Path,
        profile: Profile,
        layout: &Layout,
        mcp: &McpEndpoint,
    ) -> Result<(), Error> {
        let body =
            serde_json::to_string_pretty(&self.file(profile, layout, mcp)).map_err(|error| {
                Error::Configuration {
                    path: path.to_path_buf(),
                    reason: error.to_string(),
                }
            })?;

        fs::write(path, body).map_err(|source| Error::Harness {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Returns the name of the agent OpenCode is asked to run this member as.
    ///
    /// OpenCode carries a model's effort and temperature on an agent rather than on the run, so a
    /// member with tuning at all needs one of its own.
    pub(super) fn agent(profile: Profile) -> String {
        profile.member.id().to_owned()
    }

    fn file(&self, profile: Profile, layout: &Layout, mcp: &McpEndpoint) -> File {
        File {
            schema: SCHEMA,
            // The same model for the errands OpenCode runs a cheaper one on. A member is authorized
            // for the provider it was given and no other, and a title generated on whatever
            // OpenCode would otherwise reach for is a request to a provider nobody paid.
            small_model: self.model().to_owned(),
            model: self.model().to_owned(),
            instructions: layout
                .documents()
                .iter()
                .map(|document| document.display().to_string())
                .collect(),
            mcp: BTreeMap::from([(
                SERVER,
                Server {
                    kind: "remote",
                    url: mcp.url().to_string(),
                    enabled: true,
                },
            )]),
            agent: BTreeMap::from([(Self::agent(profile), self.described(profile))]),
            permission: BTreeMap::from([
                // Nobody is on the other end of a delivery to answer, so a question would hold the
                // run open until the runtime stopped it for silence.
                ("question", "deny"),
                // The build cache and the harness are outside the working copy on purpose, and
                // reaching them is not the escape it would be anywhere else: the sandbox, not
                // OpenCode, is what says where this process may write.
                ("external_directory", "allow"),
            ]),
            // A member's conversations are the repository's, and sharing publishes them to a third
            // party's web page.
            share: "disabled",
            // Updating itself would rewrite the binary the operator installed, from inside a
            // sandbox which denies exactly that.
            autoupdate: false,
            // OpenCode's undo history is a second thing writing Git state into a working copy the
            // runtime resets before every delivery, and whose references it owns.
            snapshot: false,
        }
    }

    // Returns the agent OpenCode runs this member as: who it is, and what it runs with.
    fn described(&self, profile: Profile) -> Agent {
        Agent {
            description: format!("{}, {}", profile.name, profile.title),
            mode: "primary",
            model: self.model().to_owned(),
            variant: self.variant.clone(),
            temperature: self.controls.temperature(),
            top_p: self.controls.top_p(),
        }
    }
}

// The configuration file itself, in OpenCode's own schema.
#[derive(Debug, Serialize)]
struct File {
    #[serde(rename = "$schema")]
    schema: &'static str,
    model: String,
    small_model: String,
    instructions: Vec<String>,
    mcp: BTreeMap<&'static str, Server>,
    agent: BTreeMap<String, Agent>,
    permission: BTreeMap<&'static str, &'static str>,
    share: &'static str,
    autoupdate: bool,
    snapshot: bool,
}

// The agent OpenCode runs the member as, in OpenCode's own schema.
#[derive(Debug, Serialize)]
struct Agent {
    description: String,
    mode: &'static str,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
}

// One MCP server OpenCode connects the agent to.
#[derive(Debug, Serialize)]
struct Server {
    #[serde(rename = "type")]
    kind: &'static str,
    url: String,
    enabled: bool,
}

/// Returns the file the plugin reports this role's progress in.
pub(super) fn report(harness: &Path) -> PathBuf {
    harness.join(REPORT)
}

/// Installs the plugin which reports what the agent is doing.
///
/// Written on every start, so a role runs the plugin this build ships rather than one an older
/// build left behind.
///
/// # Errors
///
/// Returns an error when the plugin directory or the plugin itself cannot be written.
pub(super) fn install_plugin(harness: &Path) -> Result<(), Error> {
    let directory = prepare(
        &PLUGINS
            .iter()
            .fold(harness.to_path_buf(), |path, part| path.join(part)),
    )?;
    let source = PLUGIN_SOURCE.replace(
        PLUGIN_REPORT,
        &serde_json::to_string(&report(harness).display().to_string()).unwrap_or_default(),
    );

    fs::write(directory.join(PLUGIN), source).map_err(|source| Error::Harness {
        path: directory.join(PLUGIN),
        source,
    })
}

/// Creates `directory` and reports where it really is.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or resolved.
pub(super) fn prepare(directory: &Path) -> Result<PathBuf, Error> {
    fs::create_dir_all(directory)
        .and_then(|()| fs::canonicalize(directory))
        .map_err(|source: io::Error| Error::Harness {
            path: directory.to_path_buf(),
            source,
        })
}
