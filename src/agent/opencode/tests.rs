use std::path::PathBuf;

use url::Url;

use crate::{
    agent::{
        Layout, McpEndpoint, Role, documents,
        opencode::{catalogue::Catalogue, config::Settings},
    },
    member::Member,
    provider::{self, Provider},
};

const ENDPOINT: &str = "http://127.0.0.1:4321/mcp/capability";

// A provider setting a control for each score, on the ladders a real one uses.
pub(super) const REASONING: &str = r#"
adapter = "opencode"

[auth]
ways = ["none"]

[INT]
param = "model"
values = ["p/one", "p/two", "p/three", "p/four", "p/five"]

[ACC]
param = "reasoning_effort"
values = ["minimal", "low", "medium", "high", "max"]

"#;

#[test]
fn turns_a_members_scores_into_opencode_controls() {
    // Nico is 3 sharp and 2 accurate, so each control is read at its own score rather than both
    // at one.
    let settings = read("scores", REASONING);

    assert_eq!(settings.model(), "p/three");
    let file = written("scores", Member::Nico, &settings);
    let agent = &file["agents"][Member::Nico.id()];
    assert_eq!(agent["model"], "p/three#low");
    assert!(
        file["providers"]["p"]["models"]["three"]["body"]["temperature"].is_null(),
        "a provider which sets no temperature must not have one invented for it, or a reasoning \
         model would be sent a control it rejects"
    );
}

#[test]
fn takes_the_controls_a_model_without_reasoning_uses_instead() {
    let settings = read(
        "temperature",
        r#"
adapter = "opencode"

[auth]
ways = ["none"]

[INT]
param = "model"
values = ["p/one", "p/two", "p/three", "p/four", "p/five"]

[ACC]
param = "temperature"
values = [0.9, 0.7, 0.5, 0.3, 0.1]
"#,
    );
    let narrowed = read(
        "top-p",
        r#"
adapter = "opencode"

[auth]
ways = ["none"]

[INT]
param = "model"
values = ["p/one", "p/two", "p/three", "p/four", "p/five"]

[ACC]
param = "top_p"
values = [0.9, 0.7, 0.5, 0.3, 0.1]
"#,
    );

    let file = written("temperature", Member::Nico, &settings);
    let agent = &file["agents"][Member::Nico.id()];
    assert_eq!(
        file["providers"]["p"]["models"]["three"]["body"]["temperature"],
        0.7
    );
    assert_eq!(agent["model"], "p/three");

    let file = written("top-p", Member::Nico, &narrowed);
    assert_eq!(
        file["providers"]["p"]["models"]["three"]["body"]["top_p"],
        0.7
    );
}

// The documents are what make a member that member rather than a competent stranger, and their
// order is what settles a disagreement between them.
#[test]
fn instructs_the_agent_with_this_members_documents_in_order() {
    let file = written("documents", Member::Nico, &read("documents", REASONING));
    let expected = layout()
        .documents()
        .iter()
        .map(|document| document.display().to_string())
        .collect::<Vec<_>>();

    assert_eq!(
        file["instructions"]
            .as_array()
            .expect("instructions should be a list")
            .iter()
            .map(|path| path.as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn points_the_agent_at_the_run_scoped_mcp_server_alone() {
    let file = written("mcp", Member::Nico, &read("mcp", REASONING));
    let servers = file["mcp"]["servers"]
        .as_object()
        .expect("MCP servers should be an object");

    assert_eq!(
        servers.len(),
        1,
        "a member reaches its own server and no other"
    );
    let server = &servers["radicle"];
    assert_eq!(server["type"], "remote");
    assert_eq!(server["url"], ENDPOINT);
    assert_eq!(server["disabled"], false);
}

// Every one of these is a way a headless agent stops being headless, or stops being confined to
// what the member paid for.
#[test]
fn keeps_a_headless_run_headless() {
    let file = written("headless", Member::Nico, &read("headless", REASONING));

    assert_eq!(
        file["permissions"],
        serde_json::json!([
            {"action":"question","resource":"*","effect":"deny"},
            {"action":"external_directory","resource":"*","effect":"allow"}
        ]),
        "nobody is there to answer a question, so asking one would hold the run open until the \
         runtime stopped it for silence"
    );
    assert_eq!(
        file["share"], "disabled",
        "a member's conversations are the repository's, not a third party's web page"
    );
    assert_eq!(file["update"], "disable");
    assert_eq!(
        file["snapshots"], false,
        "the runtime owns this working copy's Git state"
    );
    assert_eq!(
        file["agents"]["title"]["model"], file["model"],
        "a member is authorized for one provider, so the errands must not reach another"
    );
}

// The runtime cannot see what an agent is doing, so the plugin is what tells it. A plugin which was
// never installed, or which still carries the placeholder path, reports nothing at all.
#[test]
fn installs_a_plugin_which_reports_where_the_adapter_reads() {
    let harness = clear("opencode-plugin");

    crate::agent::opencode::config::install_plugin(&harness).expect("plugin should be installed");

    let report = crate::agent::opencode::config::report(&harness);
    let installed = std::fs::read_to_string(
        harness
            .join(".config")
            .join("opencode")
            .join("plugins")
            .join("progress.js"),
    )
    .expect("plugin should be readable");

    assert!(
        installed.contains(&report.display().to_string()),
        "the plugin must write where the adapter reads: {installed}"
    );
    assert!(
        !installed.contains("__REPORT__"),
        "the placeholder must be replaced, or the plugin writes to a file nobody reads"
    );
    assert!(
        installed.contains("event.subscribe"),
        "live events must continue after the plugin starts"
    );
    assert!(
        installed.contains("rpc.register"),
        "the model query must wait for provider activation"
    );
}

// This exercises version detection, the real sandbox, plugin activation, catalogue RPC and
// configuration together. The fake provider lists a model but no model request is made.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires OpenCode 2 on PATH and permission to start sandboxed processes"]
async fn starts_with_installed_opencode_v2() {
    use crate::{
        agent::{
            Talker as _,
            opencode::{OpenCode, config},
        },
        home::MemberHome,
        sandbox::AgentSandbox,
    };

    // System temporary directories are already readable in the sandbox, hiding failures when
    // OpenCode walks above a role's checkout to discover project configuration.
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("temporary home");
    let home = MemberHome::at(temporary.path().canonicalize().expect("canonical home"));
    home.create().expect("member directories");
    let harness = home.harness(Role::Talker);
    let configuration = harness.join(".config/opencode");
    std::fs::create_dir_all(&configuration).expect("OpenCode configuration directory");
    std::fs::write(
        configuration.join("opencode.json"),
        serde_json::json!({
            "providers": {"p": {
                "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"apiKey": "test", "baseURL": "http://127.0.0.1:9/v1"},
                "models": {"three": {"variants": [{"id": "medium"}]}}
            }}
        })
        .to_string(),
    )
    .expect("mock provider configuration");
    let endpoint = McpEndpoint::new(Url::parse(ENDPOINT).expect("endpoint"));
    let sandbox =
        AgentSandbox::load(&home, Member::Nico, Role::Talker, &endpoint).expect("agent sandbox");
    let layout = Layout::new(Role::Talker, home.cache(), harness.clone(), vec![]);
    let mut agent = OpenCode::new(Role::Talker, provider("v2-smoke", REASONING), None);
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        agent.start(Member::Nico.profile(), &endpoint, sandbox, layout),
    )
    .await
    .expect("startup must finish")
    .expect("OpenCode 2 must start");

    let configured: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(harness.join(config::FILE)).expect("generated configuration"),
    )
    .expect("valid JSON");
    assert_eq!(configured["agents"]["nico"]["model"], "p/three#medium");
    assert!(
        std::fs::read_to_string(config::report(&harness))
            .expect("v2 plugin progress")
            .contains("started")
    );
    agent.shutdown().await.expect("shutdown");
}

// A ladder may climb from a model which does not reason to one which does, so what reaches the
// agent is the score fitted to its own model rather than what the file says flatly.
#[test]
fn asks_each_model_for_the_effort_that_model_takes() {
    let catalogue = Catalogue::read(
        r#"{"output":[
        {"providerID":"p","id":"plain","enabled":true,"variants":[]},
        {"providerID":"p","id":"thinking","enabled":true,
         "variants":[{"id":"high"},{"id":"max"}]}
    ]}"#,
    )
    .expect("model catalogue");
    let ladder = r#"
adapter = "opencode"

[auth]
ways = ["none"]

[INT]
param = "model"
values = ["p/plain", "p/plain", "p/plain", "p/thinking", "p/thinking"]

[ACC]
param = "reasoning_effort"
values = ["low", "low", "medium", "high", "max"]
"#;
    let provider = provider("fitted", ladder);
    let fitted = |member: Member, name: &str| {
        let mut settings =
            Settings::read(&provider, member.profile().tuning).expect("settings should be read");
        settings.fit(&catalogue);

        written(name, member, &settings)["agents"][member.id()]["model"].clone()
    };

    assert_eq!(
        // Nico is 3 sharp, so the model which does not reason at all.
        fitted(Member::Nico, "plain"),
        "p/plain",
        "a model which does not reason must be sent no effort at all, or it fails the delivery"
    );
    assert_eq!(
        // Nora is 4 sharp and 3 accurate: the thinking model, at an effort it does not offer.
        fitted(Member::Nora, "nearest"),
        "p/thinking#high",
        "an effort the model does not take must become the nearest it does"
    );
    assert_eq!(
        // Vera is 5 and 5, and this model does go that far.
        fitted(Member::Vera, "highest"),
        "p/thinking#max"
    );
}

// Every provider this build ships for OpenCode has to map onto controls OpenCode has, for every
// member. A file which does not is one an operator meets as a failed run, on a file they did not
// write.
#[test]
fn runs_every_provider_this_build_ships_for_it() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("providers");
    let shipped = provider::load(&directory).expect("distributed providers should read back");
    let mut ours = 0;

    for (name, shipped) in &shipped {
        let shipped = shipped
            .as_ref()
            .unwrap_or_else(|error| panic!("shipped provider '{name}' must read back: {error}"));
        if shipped.adapter() != crate::agent::opencode::ADAPTER {
            continue;
        }
        ours += 1;
        for member in Member::ALL {
            let settings = Settings::read(shipped, member.profile().tuning)
                .unwrap_or_else(|error| panic!("'{name}' must run {member}: {error}"));
            assert!(
                settings.model().contains('/'),
                "'{name}' must name {member}'s model as provider/model, which is how OpenCode \
                 reads one"
            );
        }
    }
    assert!(ours > 0, "this build ships no provider for OpenCode at all");
}

fn read(name: &str, body: &str) -> Settings {
    Settings::read(&provider(name, body), Member::Nico.profile().tuning)
        .expect("settings should be read")
}

// Returns the configuration file this member's talking role would run under.
//
// Written under a directory of this test's own, because the suite runs its tests at once and a
// shared one would be cleared out from under whichever test reached it second.
fn written(name: &str, member: Member, settings: &Settings) -> serde_json::Value {
    let directory = clear(&format!("opencode-configuration-{name}"));
    let path = directory.join("opencode.json");
    settings
        .write(
            &path,
            member.profile(),
            &layout(),
            &McpEndpoint::new(Url::parse(ENDPOINT).expect("endpoint should be valid")),
        )
        .expect("configuration should be written");

    serde_json::from_str(&std::fs::read_to_string(&path).expect("configuration should be read"))
        .expect("configuration should be JSON")
}

// The role's documents as the runtime hands them over: the files `documents::install` wrote, in the
// order the documents themselves establish.
fn layout() -> Layout {
    let harness = PathBuf::from("/member/harness/talker");
    let instructions = harness.join("instructions");

    Layout::new(
        Role::Talker,
        PathBuf::from("/member/cache"),
        harness,
        documents::for_member(Member::Nico, Role::Talker)
            .into_iter()
            .map(|document| instructions.join(document.name()))
            .collect(),
    )
}

pub(super) fn provider(name: &str, body: &str) -> Provider {
    let directory = clear(&format!("opencode-provider-{name}"));
    std::fs::write(provider::path(&directory, name), body).expect("provider should be written");

    provider::find(&directory, name).expect("provider should be read")
}

fn clear(name: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(name);
    let _removed = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("test directory should be created");

    directory
}
