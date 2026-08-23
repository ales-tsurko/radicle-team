use std::path::PathBuf;

use url::Url;

use crate::{
    agent::{McpEndpoint, Role, claude_code::config::Settings, documents},
    member::Member,
    provider::{self, Provider},
};

const ENDPOINT: &str = "http://127.0.0.1:4321/mcp/capability";

// A provider setting a control for each score, on the ladder this build ships for Anthropic.
const EFFORT: &str = r#"
adapter = "claude-code"

[auth]
ways = ["none"]

[INT]
param = "model"
values = [
  "claude-sonnet-5",
  "claude-sonnet-5",
  "claude-sonnet-5",
  "claude-opus-5",
  "claude-fable-5",
]

[ACC]
param = "effort"
values = ["low", "medium", "high", "xhigh", "max"]

"#;

// The name OpenCode carries the same control under. A file written for one adapter must be refused
// by the other rather than silently dropped, or a member would run untuned.
const OPENCODE_EFFORT: &str = r#"
adapter = "claude-code"

[auth]
ways = ["none"]

[INT]
param = "model"
values = [
  "claude-sonnet-5",
  "claude-sonnet-5",
  "claude-sonnet-5",
  "claude-opus-5",
  "claude-fable-5",
]

[ACC]
param = "reasoning_effort"
values = ["low", "medium", "high", "xhigh", "max"]

"#;

#[test]
fn turns_a_members_scores_into_the_flags_claude_code_takes() {
    // Nico is 3 sharp and 2 accurate, so each control is read at its own score rather than both at
    // one.
    let nico = read("scores-nico", EFFORT);
    assert_eq!(nico.model(), "claude-sonnet-5");
    assert_eq!(nico.effort(), Some("medium"));

    // Vera is 5 and 5, so she takes the sharpest model at its hardest effort.
    let vera_provider = provider("scores-vera", EFFORT);
    let vera = Settings::read(&vera_provider, Member::Vera.profile().tuning)
        .expect("the same ladder should fit another member");
    assert_eq!(vera.model(), "claude-fable-5");
    assert_eq!(vera.effort(), Some("max"));
}

#[test]
fn refuses_a_file_naming_what_it_has_no_control_for() {
    let temperature = read_err(
        "temperature",
        r#"
adapter = "claude-code"

[auth]
ways = ["none"]

[INT]
param = "model"
values = ["m", "m", "m", "m", "m"]

[ACC]
param = "temperature"
values = [0.9, 0.7, 0.5, 0.3, 0.1]
"#,
    );

    assert!(
        temperature.contains("`temperature`") && temperature.contains("`effort`"),
        "the message must name the setting and what this adapter could have carried instead: \
         {temperature}"
    );
}

#[test]
fn refuses_opencodes_own_name_for_its_effort_control() {
    let refused = read_err("reasoning-effort", OPENCODE_EFFORT);

    assert!(
        refused.contains("`reasoning_effort`") && refused.contains("`effort`"),
        "an operator moving a file between adapters has to be told the names differ: {refused}"
    );
}

#[test]
fn refuses_to_run_without_a_model() {
    let modelless = read_err(
        "modelless",
        r#"
adapter = "claude-code"

[auth]
ways = ["none"]

[ACC]
param = "effort"
values = ["low", "medium", "high", "xhigh", "max"]
"#,
    );

    assert!(modelless.contains("`model`"), "{modelless}");
}

// Every argument here is a way a headless run stops being headless or stops being confined, so the
// delivery has to carry all of them on every run whatever the provider file says.
#[test]
fn keeps_a_headless_run_headless() {
    let arguments = read("headless", EFFORT).delivery(&mcp(), &instructions(), "-dash-first");

    let pinned = |flag: &str| {
        arguments
            .iter()
            .position(|argument| argument == flag)
            .unwrap_or_else(|| panic!("{flag} must pin every delivery"))
    };
    assert_eq!(
        arguments[pinned("--setting-sources") + 1],
        "user",
        "a repository's own .claude would otherwise run its hooks in a session nobody trusted"
    );
    pinned("--strict-mcp-config");
    pinned("--dangerously-skip-permissions");
    pinned("--no-session-persistence");

    assert_eq!(
        arguments[pinned("-p") + 1],
        "-dash-first",
        "the prompt is the last argument and one value of its own, handed over untouched"
    );
}

#[test]
fn asks_for_the_model_and_effort_the_member_runs_on() {
    let arguments = read("flags", EFFORT).delivery(&mcp(), &instructions(), "hello");

    let value = |flag: &str| {
        arguments[arguments
            .iter()
            .position(|argument| argument == flag)
            .expect("the flag should be there")
            + 1]
        .clone()
    };
    assert_eq!(value("--model"), "claude-sonnet-5");
    assert_eq!(value("--effort"), "medium");
}

#[test]
fn points_the_agent_at_the_run_scoped_mcp_server_alone() {
    let file = mcp_written();

    let servers = file["mcpServers"]
        .as_object()
        .expect("MCP servers should be an object");
    assert_eq!(
        servers.len(),
        1,
        "a member reaches its own server and no other"
    );
    let server = &servers["radicle"];
    assert_eq!(server["type"], "http");
    assert_eq!(server["url"], ENDPOINT);
}

// The documents are what make a member that member rather than a competent stranger. One delivery
// takes one standing prompt, so they travel combined, in the order that settles disagreements
// between them.
#[test]
fn combines_the_documents_in_the_order_they_establish() {
    let directory = clear("claude-combine");
    let instructions = instructions_in(&directory);
    // All six are written, as `documents::install` would have before the role started.
    for (index, document) in instructions.iter().enumerate() {
        std::fs::write(document, format!("document {index}")).expect("document should be written");
    }

    let combined = crate::agent::claude_code::config::combine(&instructions)
        .expect("documents should be combined");
    assert_eq!(
        combined.parent(),
        instructions[0].parent(),
        "the combination lives beside its sources, where their protection covers it"
    );
    let expected = (0..instructions.len())
        .map(|index| format!("document {index}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    assert_eq!(
        std::fs::read_to_string(&combined).expect("combination should be readable"),
        format!("{expected}\n")
    );

    // Written fresh every start, so a document removed from a later build stops instructing.
    let fewer = &instructions[..1];
    let combined = crate::agent::claude_code::config::combine(fewer)
        .expect("fewer documents should still combine");
    assert_eq!(
        std::fs::read_to_string(&combined).expect("combination should be readable"),
        "document 0\n",
        "what the last start wrote must not survive into this one"
    );
}

// Every provider this build ships for Claude Code has to map onto controls it carries, for every
// member. A file which does not is one an operator meets as a failed run, on a file they did not
// write.
#[cfg(not(miri))]
#[test]
fn runs_every_provider_this_build_ships_for_it() {
    let root = clear("claude-shipped");
    let shipped = provider::installed(&root).expect("shipped providers should read back");
    let mut ours = 0;

    for (name, shipped) in &shipped {
        let shipped = shipped
            .as_ref()
            .unwrap_or_else(|error| panic!("shipped provider '{name}' must read back: {error}"));
        if shipped.adapter() != crate::agent::claude_code::ADAPTER {
            continue;
        }
        ours += 1;
        for member in Member::ALL {
            let settings = Settings::read(shipped, member.profile().tuning)
                .unwrap_or_else(|error| panic!("'{name}' must run {member}: {error}"));
            assert!(
                !settings.model().is_empty(),
                "'{name}' must name {member}'s model"
            );
            assert!(
                settings.effort().is_some(),
                "'{name}' tunes accuracy through effort, so {member} must have one"
            );
        }
    }
    assert!(
        ours > 0,
        "this build ships no provider for Claude Code at all"
    );
}

fn read(name: &str, body: &str) -> Settings {
    Settings::read(&provider(name, body), Member::Nico.profile().tuning)
        .expect("settings should be read")
}

fn read_err(name: &str, body: &str) -> String {
    Settings::read(&provider(name, body), Member::Nico.profile().tuning)
        .expect_err("a file this adapter cannot run must be refused")
        .to_string()
}

fn mcp_written() -> serde_json::Value {
    let directory = clear("claude-mcp");
    let path = crate::agent::claude_code::config::write_mcp(
        &directory,
        &McpEndpoint::new(Url::parse(ENDPOINT).expect("endpoint should be valid")),
    )
    .expect("configuration should be written");

    serde_json::from_str(&std::fs::read_to_string(&path).expect("configuration should be read"))
        .expect("configuration should be JSON")
}

fn mcp() -> PathBuf {
    PathBuf::from("/member/harness/talker/mcp.json")
}

fn instructions() -> PathBuf {
    PathBuf::from("/member/harness/talker/instructions/ALL.md")
}

// Six placeholder documents, as the runtime hands them over once `documents::install` has written
// the real ones.
fn instructions_in(directory: &std::path::Path) -> Vec<PathBuf> {
    documents::for_member(Member::Nico, Role::Talker)
        .into_iter()
        .map(|document| directory.join(document.name()))
        .collect()
}

fn provider(name: &str, body: &str) -> Provider {
    let directory = clear(&format!("claude-provider-{name}"));
    std::fs::write(provider::path(&directory, name), body).expect("provider should be written");

    provider::find(&directory, name).expect("provider should be read")
}

/// The provider file this build ships for Anthropic, read the way the application reads it.
///
/// One directory per process, because every test calling this runs at once -- nextest gives each
/// its own process -- and they would otherwise clear a shared one out from under whichever reached
/// it second.
pub(crate) fn shipped_anthropic() -> Provider {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("claude-shipped-anthropic-{}", std::process::id()));

    provider::installed_named(&root, "anthropic").expect("the shipped provider should be read")
}

fn clear(name: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(name);
    let _removed = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("test directory should be created");

    directory
}
