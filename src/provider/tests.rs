use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    member::{Member, Stat},
    provider::{self, Way, credentials},
};

const OPENAI: &str = r#"
adapter = "opencode"

[auth]
ways = ["key", "subscription"]

[auth.key]
env = ["OPENAI_API_KEY"]

[auth.subscription]

[INT]
param  = "model"
values = ["gpt-5-nano", "gpt-5-mini", "gpt-5-mini", "gpt-5", "gpt-5"]

[ACC]
param  = "temperature"
values = [0.9, 0.7, 0.5, 0.3, 0.1]
"#;

// Everything a provider file can express, parsed from the shape a user actually writes.
#[test]
fn reads_a_provider_file() {
    let directory = write("provider-reads", &[("openai", OPENAI)]);

    let provider = provider::find(&directory, "openai").expect("provider should be read");

    assert_eq!(provider.adapter(), "opencode");
    assert_eq!(provider.ways(), [Way::Key, Way::Subscription]);
    assert_eq!(provider.key_variables(), ["OPENAI_API_KEY"]);
}

// The whole point of the file: a setting name and a value the runtime never interprets, so a
// provider gaining a setting is an edit rather than a release.
#[test]
fn resolves_settings_from_a_members_scores() {
    let directory = write("provider-settings", &[("openai", OPENAI)]);
    let provider = provider::find(&directory, "openai").expect("provider should be read");
    let nico = Member::Nico.profile().tuning;

    let settings = provider.settings(nico);

    assert_eq!(nico.score(Stat::Intelligence), 3);
    assert_eq!(nico.score(Stat::Accuracy), 2);
    assert_eq!(
        settings,
        [
            ("model".to_owned(), "gpt-5-mini".into()),
            ("temperature".to_owned(), 0.7.into()),
        ],
        "a score must select the value at its own position and nothing else"
    );
}

// A provider which rejects a setting says so by leaving it out. Nothing in the runtime knows which
// providers those are, so this is the only way it can be expressed.
#[test]
fn contributes_nothing_for_a_score_the_provider_has_no_setting_for() {
    let modelless = r#"
adapter = "opencode"

[auth]
ways = ["none"]

[INT]
param  = "model"
values = ["gpt-5-nano", "gpt-5-mini", "gpt-5-mini", "gpt-5", "gpt-5"]
"#;
    let directory = write("provider-absent", &[("openai", modelless)]);
    let provider = provider::find(&directory, "openai").expect("provider should be read");

    let named = provider
        .settings(Member::Vera.profile().tuning)
        .into_iter()
        .map(|(param, _value)| param)
        .collect::<Vec<_>>();

    assert_eq!(named, ["model"]);
    assert!(
        !named.contains(&"temperature".to_owned()),
        "a file with no ACC entry must produce no setting for it"
    );
}

// The list of scores is fixed at five, so a file with four has a typo rather than an intention.
// Caught against the file, because the alternative is a member silently running on the wrong value.
#[test]
fn refuses_a_setting_which_does_not_cover_every_score() {
    let short = r#"
adapter = "opencode"

[auth]
ways = ["none"]

[INT]
param  = "model"
values = ["a", "b", "c", "d"]
"#;
    let directory = write("provider-short", &[("openai", short)]);

    let error = provider::find(&directory, "openai").expect_err("a short list must be refused");

    assert!(
        matches!(
            error,
            provider::Error::Scores {
                stat: Stat::Intelligence,
                found: 4,
                expected: 5,
                ..
            }
        ),
        "{error}"
    );
}

// Offering a way without what that way needs would fail at the first delivery instead, long after
// whoever wrote the file could connect the two.
#[test]
fn refuses_a_key_way_with_no_variable_to_put_it_in() {
    let bare = r#"
adapter = "opencode"

[auth]
ways = ["key"]
"#;
    let directory = write("provider-bare-key", &[("openai", bare)]);

    let error = provider::find(&directory, "openai").expect_err("an unusable way must be refused");

    assert!(
        matches!(
            error,
            provider::Error::MissingWaySettings { way: Way::Key, .. }
        ),
        "{error}"
    );
}

// The list a user picks from is the directory, so the name has to come from the file rather than
// from anything inside it.
#[test]
fn names_every_provider_after_its_file() {
    let directory = write(
        "provider-listing",
        &[("openai", OPENAI), ("groq", OPENAI), ("notes.txt", OPENAI)],
    );

    let providers = provider::load(&directory).expect("providers should be listed");

    assert_eq!(
        providers.keys().collect::<Vec<_>>(),
        ["groq", "openai"],
        "only .toml files are providers, and each is named after its file"
    );
}

#[test]
fn reports_no_providers_before_any_are_installed() {
    let root = clear("provider-missing");
    let missing = root.join(provider::DIRECTORY);
    for directory in [&missing, &root] {
        let error = provider::load(directory).expect_err("no providers must be actionable");
        assert!(matches!(&error, provider::Error::NoProviders { path } if path == directory));
        let message = error.to_string();
        assert!(
            message.contains(&directory.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains("copy the provider TOML files"),
            "{message}"
        );
    }
    assert!(
        !missing.exists(),
        "reading must not create a provider directory"
    );
}

// One account per person and provider, until a project has to be billed separately. The member's
// file replaces the global one rather than merging, so a split bill cannot half-inherit.
#[test]
fn prefers_a_members_credentials_over_the_global_ones() {
    let root = clear("provider-credentials");
    let global = root.join("global");
    let mine = root.join("member");
    credentials::store(
        &global,
        "openai",
        &credentials::Credentials::key(BTreeMap::from([(
            "OPENAI_API_KEY".to_owned(),
            "shared".to_owned(),
        )])),
    )
    .expect("global credentials should be written");

    let shared = credentials::load(&global, Some(&mine), "openai")
        .expect("credentials should be read")
        .expect("the global credentials should be found");
    assert_eq!(shared.variables()["OPENAI_API_KEY"], "shared");

    credentials::store(
        &mine,
        "openai",
        &credentials::Credentials::key(BTreeMap::from([(
            "OPENAI_API_KEY".to_owned(),
            "mine".to_owned(),
        )])),
    )
    .expect("member credentials should be written");

    let owned = credentials::load(&global, Some(&mine), "openai")
        .expect("credentials should be read")
        .expect("the member credentials should be found");

    assert_eq!(owned.way(), Way::Key);
    assert_eq!(owned.variables()["OPENAI_API_KEY"], "mine");
}

// A key on disk that any process can read is a key that leaked, and the operator would never see it
// happen.
#[cfg(unix)]
#[test]
fn keeps_stored_credentials_private() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = clear("provider-credential-mode").join("global");

    let path = credentials::store(
        &directory,
        "openai",
        &credentials::Credentials::key(BTreeMap::from([(
            "OPENAI_API_KEY".to_owned(),
            "secret".to_owned(),
        )])),
    )
    .expect("credentials should be written");

    let mode = |path: &std::path::Path| {
        std::fs::metadata(path)
            .expect("path should exist")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&path), 0o600, "the file must be readable only by us");
    assert_eq!(mode(&directory), 0o700, "and so must the directory");
}

// The name a score is written as in a provider file and the name an error prints are the same
// string. Letting them drift would mean an error naming a section the file does not have.
#[test]
fn writes_every_score_under_the_name_it_reports() {
    for stat in Stat::ALL {
        let body = format!(
            "adapter = \"opencode\"\n\n[auth]\nways = [\"none\"]\n\n[{}]\nparam = \"p\"\nvalues = \
             [1, 2, 3, 4, 5]\n",
            stat.id()
        );
        let directory = write(&format!("provider-stat-{stat}"), &[("openai", &body)]);
        let provider = provider::find(&directory, "openai").expect("provider should be read");

        let settings = provider.settings(Member::Nico.profile().tuning);

        assert_eq!(
            settings.len(),
            1,
            "'{stat}' must name a section a provider file can carry"
        );
        assert_eq!(
            settings[0].1,
            serde_json::Value::from(Member::Nico.profile().tuning.score(stat)),
            "'{stat}' must select the value at this member's own score"
        );
    }
}

// Everything shipped has to survive its own loader, or the application arrives broken and the first
// thing a user sees is a parse error against a file they did not write.
#[test]
fn ships_providers_which_read_back() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("providers");
    let providers = provider::load(&directory).expect("distributed providers should read back");

    assert!(!providers.is_empty(), "the build must ship some providers");
    for (name, shipped) in &providers {
        // Every shipped file, not just the ones which happen to parse: a broken one is now reported
        // in place of its provider rather than failing the read, so it would otherwise slip past
        // this loop unexamined.
        let shipped = shipped
            .as_ref()
            .unwrap_or_else(|error| panic!("shipped provider '{name}' must read back: {error}"));
        assert!(
            !shipped.ways().is_empty(),
            "'{name}' must say how it is authorized"
        );
        assert!(
            shipped
                .settings(Member::Nico.profile().tuning)
                .iter()
                .any(|(param, _value)| param == "model"),
            "'{name}' must at least choose a model"
        );
        // A provider sets the same things for every member; only the values differ. A member
        // missing one would silently run without it, which nothing downstream could notice.
        let expected = shipped
            .settings(Member::Nico.profile().tuning)
            .into_iter()
            .map(|(param, _value)| param)
            .collect::<Vec<_>>();
        for member in Member::ALL {
            let named = shipped
                .settings(member.profile().tuning)
                .into_iter()
                .map(|(param, _value)| param)
                .collect::<Vec<_>>();

            assert_eq!(
                named, expected,
                "'{name}' sets different things for {member} than for another member"
            );
        }
    }
}

// Only installed files are choices, including custom providers and edits to distributed files.
#[test]
fn reads_only_installed_configs_and_preserves_edits() {
    let directory = write(
        "provider-read-only",
        &[("glm", OPENAI), ("my-provider", OPENAI)],
    );
    let glm = provider::path(&directory, "glm");
    let edited = OPENAI.replace("gpt-5", "my-new-model");
    std::fs::write(&glm, &edited).expect("an edited copy should be written");
    let expected = provider::find(&directory, "glm").expect("the edited provider should read");

    let listed = provider::load(&directory).expect("providers should be listed");
    let selected = provider::find(&directory, "glm").expect("provider should be selected");

    assert_eq!(
        listed.keys().map(String::as_str).collect::<Vec<_>>(),
        ["glm", "my-provider"]
    );
    assert_eq!(
        listed["glm"].as_ref().expect("edited provider should read"),
        &expected
    );
    assert_eq!(selected, expected);
    assert_eq!(
        std::fs::read_to_string(&glm).expect("edited file should remain readable"),
        edited
    );
}

#[test]
fn reports_broken_configs_and_never_recreates_removed_files() {
    let directory = write(
        "provider-broken-local",
        &[("custom", OPENAI), ("glm", "invalid toml")],
    );
    let glm = provider::path(&directory, "glm");

    let listed = provider::load(&directory).expect("a broken provider must not fail the listing");
    assert!(matches!(
        listed["glm"],
        Err(provider::Error::Malformed { .. })
    ));
    assert!(matches!(
        provider::find(&directory, "glm"),
        Err(provider::Error::Malformed { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(&glm).expect("file should remain"),
        "invalid toml"
    );

    std::fs::remove_file(&glm).expect("broken file should be removable");
    let remaining = provider::load(&directory).expect("the custom provider should remain");
    assert_eq!(
        remaining.keys().map(String::as_str).collect::<Vec<_>>(),
        ["custom"]
    );
    assert!(matches!(
        provider::find(&directory, "glm"),
        Err(provider::Error::Unknown(_))
    ));
    assert!(!glm.exists(), "a removed config must stay removed");
}

#[test]
fn refuses_a_directory_with_no_usable_providers() {
    let directory = write("provider-all-broken", &[("glm", "invalid toml")]);
    let error = provider::load(&directory).expect_err("broken files provide no choices");
    assert!(
        matches!(error, provider::Error::Malformed { path, .. } if path == provider::path(&directory, "glm"))
    );
}

// Settings for a way the file does not offer are never read, so they look configured and do
// nothing. Both directions of the agreement are checked, not just the missing one.
#[test]
fn refuses_settings_for_a_way_the_provider_does_not_offer() {
    let stray = r#"
adapter = "opencode"

[auth]
ways = ["key"]

[auth.key]
env = ["OPENAI_API_KEY"]

[auth.subscription]
"#;
    let directory = write("provider-stray-way", &[("openai", stray)]);

    let error = provider::find(&directory, "openai").expect_err("unread settings must be refused");

    assert!(
        matches!(
            error,
            provider::Error::UndeclaredWaySettings {
                way: Way::Subscription,
                ..
            }
        ),
        "{error}"
    );
}

// The mode argument to `open` only applies when `open` creates the file, so re-authorizing over a
// file somebody had already loosened would write a fresh key into a world-readable file.
#[cfg(unix)]
#[test]
fn narrows_a_credentials_file_which_was_already_loose() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = clear("provider-credential-loose").join("global");
    std::fs::create_dir_all(&directory).expect("directory should be created");
    let path = credentials::path(&directory, "openai");
    std::fs::write(&path, "way = \"key\"\n").expect("a loose file should be written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("the file should be loosened");

    let written = credentials::store(
        &directory,
        "openai",
        &credentials::Credentials::key(BTreeMap::from([(
            "OPENAI_API_KEY".to_owned(),
            "fresh".to_owned(),
        )])),
    )
    .expect("credentials should be written");

    let mode = std::fs::metadata(&written)
        .expect("the file should exist")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "re-authorizing must narrow a file which was already loose, not write a key into it"
    );
}

// A key loosened by hand or restored from a backup is refused rather than used, the same way every
// other secret this application reads is gated.
#[cfg(unix)]
#[test]
fn refuses_credentials_anybody_can_read() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = clear("provider-credential-refuse").join("global");
    let path = credentials::store(
        &directory,
        "openai",
        &credentials::Credentials::key(BTreeMap::from([(
            "OPENAI_API_KEY".to_owned(),
            "secret".to_owned(),
        )])),
    )
    .expect("credentials should be written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("the file should be loosened");

    let error =
        credentials::load(&directory, None, "openai").expect_err("a readable key must be refused");

    // The mode specifically, not any permissions failure: an owner or file-type error would satisfy
    // a looser assertion while saying nothing about the mode this test is here for.
    assert!(
        matches!(
            error,
            credentials::Error::Permissions(crate::identity::permissions::Error::Mode {
                mode: 0o644,
                ..
            })
        ),
        "{error}"
    );
}

// A provider name becomes a file name under the operator's configuration directory. A name which is
// a path reads a provider from outside that directory, and writes the operator's API key outside
// the tree `store` just secured.
#[test]
fn refuses_a_provider_name_which_is_a_path() {
    let root = clear("provider-traversal");
    let escaping = [
        "../../../tmp/evil",
        "/absolute/evil",
        ".hidden",
        "",
        "has space",
        "sub/dir",
    ];

    for name in escaping {
        assert!(
            matches!(provider::find(&root, name), Err(provider::Error::Name(_))),
            "reading '{name}' must be refused before it is joined onto a directory"
        );
        assert!(
            matches!(
                credentials::store(&root, name, &credentials::Credentials::subscription()),
                Err(credentials::Error::Name(_))
            ),
            "writing credentials for '{name}' must be refused"
        );
    }

    assert!(
        provider::is_plain_name("anthropic") && provider::is_plain_name("my-openai_2.1"),
        "an ordinary name must still be accepted"
    );
}

// Existing links are operator configuration too, including links whose target is not present yet.
#[cfg(unix)]
#[test]
fn preserves_linked_providers_and_their_targets() {
    let root = clear("provider-install-symlink");
    let directory = root.join(provider::DIRECTORY);
    std::fs::create_dir_all(&directory).expect("provider directory should be created");
    let victim = root.join("victim.txt");
    std::fs::write(&victim, OPENAI).expect("linked provider should be written");
    std::os::unix::fs::symlink(&victim, provider::path(&directory, "gpt"))
        .expect("symlink should be created");
    let missing = root.join("missing.toml");
    std::os::unix::fs::symlink(&missing, provider::path(&directory, "glm"))
        .expect("dangling symlink should be created");

    let listed = provider::load(&directory).expect("a readable linked provider should work");
    let _linked = listed["gpt"].as_ref().expect("linked provider should read");
    assert!(matches!(listed["glm"], Err(provider::Error::Unknown(_))));

    assert_eq!(
        std::fs::read_to_string(&victim).expect("victim should still be readable"),
        OPENAI,
        "loading must not write through a link"
    );
    assert_eq!(
        std::fs::read_link(provider::path(&directory, "gpt")).expect("link should remain"),
        victim
    );
    assert_eq!(
        std::fs::read_link(provider::path(&directory, "glm")).expect("dangling link should remain"),
        missing
    );
    assert!(
        !missing.exists(),
        "loading must not create the missing target"
    );
}

// A command-line tool's configuration belongs beside git's and ssh's. `BaseDirs::config_dir`
// follows Apple's guidance for windowed applications and answers `~/Library/Application Support` on
// macOS, which is not where anyone would look for this or edit a provider file.
#[test]
fn keeps_configuration_where_a_command_line_tool_is_looked_for() {
    let home = PathBuf::from("/home/someone");

    assert_eq!(
        provider::configuration_in(None, &home),
        home.join(".config").join("radicle-team"),
        "with nothing configured it is ~/.config, on every platform which has a ~"
    );
    assert_eq!(
        provider::configuration_in(Some(Path::new("/elsewhere/config")), &home),
        PathBuf::from("/elsewhere/config").join("radicle-team"),
        "and XDG_CONFIG_HOME wins when it is set"
    );
}

// The application tells operators to add their own files here, so one of them having a typo is a
// thing that happens. It must cost that one provider and nothing else -- neither the providers
// which are fine, nor the ability to set up a member on a different one.
#[test]
fn keeps_the_readable_providers_when_one_file_is_broken() {
    let directory = write(
        "provider-one-broken",
        &[("openai", OPENAI), ("mine", "this is not toml at all {{{")],
    );

    let providers = provider::load(&directory).expect("a broken file must not fail the directory");

    assert_eq!(
        providers.keys().collect::<Vec<_>>(),
        ["mine", "openai"],
        "both are listed, so the operator can see which one to go and fix"
    );
    assert!(
        providers["openai"].is_ok(),
        "the file which is fine stays usable"
    );
    assert!(
        matches!(providers["mine"], Err(provider::Error::Malformed { .. })),
        "and the broken one reports its own complaint in place of a provider"
    );
}

// Two scores naming one setting hands the adapter that setting twice, and whichever it applies last
// decides. A member would then run on a model chosen by its stubbornness score.
#[test]
fn refuses_two_scores_which_set_the_same_thing() {
    let repeated = r#"
adapter = "opencode"

[auth]
ways = ["none"]

[INT]
param  = "model"
values = ["a", "b", "c", "d", "e"]

[ACC]
param  = "model"
values = ["v", "w", "x", "y", "z"]
"#;
    let directory = write("provider-repeated", &[("openai", repeated)]);

    let error = provider::find(&directory, "openai").expect_err("a repeat must be refused");

    assert!(
        matches!(error, provider::Error::RepeatedSetting { ref param, .. } if param == "model"),
        "{error}"
    );
}

#[test]
fn reports_a_provider_which_is_not_installed() {
    let directory = write("provider-unknown", &[("openai", OPENAI)]);

    let error =
        provider::find(&directory, "groq").expect_err("an absent provider must be reported");

    assert!(matches!(error, provider::Error::Unknown(name) if name == "groq"));
}

fn clear(name: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(name);
    let _removed = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("test directory should be created");

    root
}

fn write(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let directory = clear(name).join(provider::DIRECTORY);
    std::fs::create_dir_all(&directory).expect("provider directory should be created");
    for (provider, body) in files {
        let path = match provider.contains('.') {
            true => directory.join(provider),
            false => directory.join(format!("{provider}.toml")),
        };
        std::fs::write(path, body).expect("provider file should be written");
    }

    directory
}
