//! The configuration one role runs Claude Code under.
//!
//! [`Controls`] is what every adapter is given: the provider file's settings, under the names it
//! wrote, valued as it wrote them. Claude Code takes its tuning on the command line rather than in
//! a file of its own, so this adapter's work is mostly refusal -- a file naming anything but what
//! this harness carries stops the role here -- and assembling the two small files a delivery reads:
//! which MCP server the member reaches, and everything instructing it as one standing prompt.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    agent::{Controls, McpEndpoint, claude_code::Error},
    member::ModelTuning,
    provider::Provider,
};

/// Name Claude Code is told to call the run-scoped MCP server.
///
/// Claude Code prefixes the tools it takes from a server with that server's name, so this is the
/// word the agent sees inside every Radicle tool's name.
pub(super) const SERVER: &str = "radicle";
/// File the MCP servers of one role are written to.
pub(super) const FILE: &str = "mcp.json";
/// File the documents instructing the role are combined into.
///
/// It lives beside them in the instructions directory, which the sandbox keeps read-only to the
/// agent: an agent able to rewrite its own character could reword itself mid-run.
pub(super) const INSTRUCTIONS: &str = "ALL.md";

// The settings Claude Code carries. A reasoning effort reaches it under its own name, `effort`;
// OpenCode calls the same control `reasoning_effort`, which is why the two adapters refuse each
// other's files rather than guessing. Temperature and top-p have no control here at all.
const CARRIED: &[&str] = &["model", "effort"];

/// The controls one role runs with, as Claude Code takes them.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Settings {
    // The model Claude Code is asked to run.
    model: String,
    // How much work the model puts into a response, at this member's own score.
    effort: Option<String>,
}

impl Settings {
    /// Reads what `provider` makes of a member scored `tuning`.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider file does not describe a member this adapter can run.
    pub(super) fn read(provider: &Provider, tuning: ModelTuning) -> Result<Self, Error> {
        let controls = Controls::read(provider, tuning);
        controls.carried(CARRIED).map_err(Error::from)?;
        let given = || controls.names().map(str::to_owned).collect::<Vec<_>>();
        let model = controls
            .text("model")
            .map_err(Error::from)?
            .ok_or_else(|| Error::from(crate::agent::controls::Error::missing("model", given())))?;
        let effort = controls.text("effort").map_err(Error::from)?;

        Ok(Self { model, effort })
    }

    /// Returns the model Claude Code is asked to run.
    pub(super) fn model(&self) -> &str {
        &self.model
    }

    /// Returns how hard the model is asked to think before answering.
    ///
    /// Sent exactly as asked. Where a model does not take an effort, Claude Code itself falls back
    /// to the nearest one below it -- there is no listing to fit against beforehand, and no need:
    /// the fitting happens on the far side of the flag.
    pub(super) fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// Returns the arguments one delivery runs under.
    ///
    /// Everything here is fixed by this adapter rather than open to configuration, because each
    /// argument is a way a headless run stops being headless or stops being confined to what the
    /// member paid for. The prompt is the last argument, one value of its own: it may not open with
    /// a dash, which the harness would read as a flag, and every prompt this runtime writes opens
    /// with prose instead.
    pub(super) fn delivery(&self, mcp: &Path, instructions: &Path, prompt: &str) -> Vec<String> {
        let mut arguments = Vec::from([
            "--output-format".to_owned(),
            // One JSON event per line, as it happens: both how the runtime sees progress and where
            // a failure explains itself.
            "stream-json".to_owned(),
            "--verbose".to_owned(),
            "--model".to_owned(),
            self.model.clone(),
        ]);
        if let Some(effort) = &self.effort {
            arguments.push("--effort".to_owned());
            arguments.push(effort.clone());
        }
        arguments.extend([
            // Only the settings this harness home holds are read. A repository's own `.claude`
            // would otherwise run its hooks and impose its settings in a session nobody agreed to
            // trust, and a member works on repositories somebody else controls.
            "--setting-sources".to_owned(),
            "user".to_owned(),
            // The MCP server above is the only one, whatever else the machine or the repository
            // offers.
            "--strict-mcp-config".to_owned(),
            "--mcp-config".to_owned(),
            path(mcp),
            // Nothing can stop to ask a person before acting; the sandbox is what confines the
            // run, so the permission system would never receive an answer anyway.
            "--dangerously-skip-permissions".to_owned(),
            // Nothing is resumed between deliveries, so nothing is even kept to resume.
            "--no-session-persistence".to_owned(),
            "--append-system-prompt-file".to_owned(),
            path(instructions),
            "-p".to_owned(),
            prompt.to_owned(),
        ]);

        arguments
    }
}

// The MCP configuration file itself, in Claude Code's own schema.
#[derive(Debug, Serialize)]
struct File {
    #[serde(rename = "mcpServers")]
    servers: BTreeMap<&'static str, Server>,
}

// One MCP server Claude Code connects the agent to.
#[derive(Debug, Serialize)]
struct Server {
    #[serde(rename = "type")]
    kind: &'static str,
    url: String,
}

/// Writes the MCP servers this role connects to, and reports the file it wrote.
///
/// Rewritten every time the role starts, so a capability token from an earlier run cannot outlive
/// the server it belonged to.
///
/// # Errors
///
/// Returns an error when the file cannot be built or written.
pub(super) fn write_mcp(harness: &Path, mcp: &McpEndpoint) -> Result<PathBuf, Error> {
    let path = harness.join(FILE);
    let file = File {
        servers: BTreeMap::from([(
            SERVER,
            Server {
                kind: "http",
                url: mcp.url().to_string(),
            },
        )]),
    };
    let body = serde_json::to_string_pretty(&file).map_err(|error| Error::Configuration {
        path: path.clone(),
        reason: error.to_string(),
    })?;

    fs::write(&path, body).map_err(|source| Error::Harness {
        path: path.clone(),
        source,
    })?;

    Ok(path)
}

/// Combines the documents instructing the role into the one file a delivery reads them from.
///
/// A delivery takes a single standing prompt, so the documents travel together, in the order they
/// establish themselves: shared rules first, then who is speaking. The combination is rewritten on
/// every start and lives in the instructions directory beside its sources, where the runtime's
/// protection already covers it.
///
/// # Errors
///
/// Returns an error when a document cannot be read or the combination cannot be written.
pub(super) fn combine(documents: &[PathBuf]) -> Result<PathBuf, Error> {
    let Some(first) = documents.first() else {
        return Err(Error::Configuration {
            path: PathBuf::new(),
            reason: "no documents were given to instruct the role with".to_owned(),
        });
    };
    let Some(directory) = first.parent() else {
        return Err(Error::Configuration {
            path: first.clone(),
            reason: "the first document is nowhere to write the combination beside".to_owned(),
        });
    };
    let mut combined = String::new();
    for document in documents {
        let body = fs::read_to_string(document).map_err(|source| Error::Harness {
            path: document.clone(),
            source,
        })?;
        if !combined.is_empty() {
            combined.push_str("\n\n");
        }
        combined.push_str(body.trim_end());
    }
    combined.push('\n');

    let path = directory.join(INSTRUCTIONS);
    fs::write(&path, combined).map_err(|source| Error::Harness {
        path: path.clone(),
        source,
    })?;

    Ok(path)
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

fn path(path: &Path) -> String {
    path.display().to_string()
}
