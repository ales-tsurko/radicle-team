//! Setup workflow tests.

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::{collections::BTreeSet, fs, path::PathBuf, str::FromStr as _};

use radicle::identity::{Did, RepoId};

use crate::{
    SetupStatus,
    cli::setup::{Error, ManifestEntry, ManifestUpdate, SecurityManifest, SetupWizard},
    manifest::{FILE_NAME, Manifest, TriggerAccess},
    member::Member,
};

const NICO: &str = "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
const MAYA: &str = "did:key:z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C";
const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
const RECIPIENT: &str = "age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa";
const SECURITY: &str = "version = 1\n\n[security]\nrepository = \
                        \"rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc\"\nrecipients = \
                        [\"age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa\"]\n\n";

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = PathBuf::from("target/tests/setup-wizard").join(name);
        let _result = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("fixture directory should be created");
        Self(path)
    }

    fn manifest(&self) -> PathBuf {
        self.0.join(FILE_NAME)
    }

    fn entry(member: Member, did: &str) -> ManifestEntry {
        ManifestEntry {
            member,
            did: did.parse::<Did>().expect("DID should parse"),
            generation: 1,
            provider: None,
        }
    }

    fn create(entry: ManifestEntry) -> ManifestUpdate {
        ManifestUpdate::create(
            entry,
            SecurityManifest {
                repository: RepoId::from_str(REPOSITORY).expect("repository ID should parse"),
                recipient: RECIPIENT.to_owned(),
            },
            TriggerAccess::Delegates,
        )
    }

    const fn extend(entry: ManifestEntry) -> ManifestUpdate {
        ManifestUpdate::extend(entry)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _result = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn reports_the_explicit_manifest_path_noninteractively() {
    let path = PathBuf::from("config/team-policy.toml");
    let entry = Fixture::entry(Member::Nico, NICO);
    let missing = SetupWizard::explicit_manifest_output(
        &path,
        entry.clone(),
        SetupStatus::ManifestMissing { generation: 1 },
    );

    assert!(missing.contains("config/team-policy.toml"));
    assert!(missing.contains("create explicit manifest"));
    assert!(missing.contains("version = 1"));
    assert!(missing.contains(
        "mcp_allow = [\"read\", \"chat\", \"comments\", \"patches\", \"reactions\", \"reviews\"]"
    ));
    assert!(!missing.contains("canonical branch"));

    let binding = SetupWizard::explicit_manifest_output(
        &path,
        entry,
        SetupStatus::BindingMissing { generation: 1 },
    );
    assert!(binding.contains("add this binding to explicit manifest"));
    assert!(binding.contains("config/team-policy.toml"));
}

#[test]
fn includes_member_permissions_in_repository_output() {
    let output = SetupWizard::repository_manifest_output(
        Fixture::entry(Member::Nico, NICO),
        SetupStatus::ManifestMissing { generation: 1 },
    );

    assert!(output.contains("canonical branch"));
    assert!(output.contains("version = 1"));
    assert!(output.contains(
        "mcp_allow = [\"read\", \"chat\", \"comments\", \"patches\", \"reactions\", \"reviews\"]"
    ));
}

#[test]
fn emits_each_members_preconfigured_mcp_groups() {
    let nico = Fixture::entry(Member::Nico, NICO).to_string();
    let maya = Fixture::entry(Member::Maya, MAYA).to_string();

    assert!(nico.contains(
        "mcp_allow = [\"read\", \"chat\", \"comments\", \"patches\", \"reactions\", \"reviews\"]"
    ));
    assert!(maya.contains(
        "mcp_allow = [\"read\", \"chat\", \"comments\", \"issues\", \"patches\", \"reactions\", \
         \"reviews\"]"
    ));
}

// What the operator is told to paste has to be what setup would have written. A provider chosen in
// the wizard and then missing from the shown binding is a choice silently thrown away, and nothing
// downstream can tell it was ever made.
#[test]
fn shows_the_chosen_provider_in_the_binding_it_tells_you_to_paste() {
    let shown = Fixture::entry(Member::Nico, NICO)
        .with_provider(Some("anthropic".to_owned()))
        .to_string();

    assert!(
        shown.contains("provider = \"anthropic\""),
        "the shown binding dropped the provider: {shown}"
    );
    assert!(
        !Fixture::entry(Member::Nico, NICO)
            .to_string()
            .contains("provider"),
        "a member which chose no provider must not be given one"
    );
}

// The binding a non-interactive run prints is pasted into the shared manifest by hand, so the name
// in it gets the same check the interactive run applies. A typo accepted here is accepted by every
// reader forever, while the member it names has no credentials and no models.
#[test]
fn refuses_a_provider_name_nobody_installed_without_a_terminal() {
    let wizard = SetupWizard::new(
        crate::Application::new(
            Member::Nico,
            RepoId::from_str(REPOSITORY).expect("repository should parse"),
        )
        .with_provider("anthropc"),
    );

    let error = wizard
        .named_provider()
        .expect_err("a name nobody installed must be refused with no terminal to ask at");

    assert!(
        matches!(
            error,
            Error::Provider(boxed)
                if matches!(*boxed, crate::cli::provider::Error::Provider(
                    crate::provider::Error::Unknown(_)
                ))
        ),
        "the operator has to be told the name is not one they have"
    );
}

#[test]
fn writes_every_trigger_access_form_above_the_security_table() {
    for (access, expected) in [
        (TriggerAccess::Delegates, "triggered_by = \"delegates\""),
        (TriggerAccess::Anyone, "triggered_by = \"anyone\""),
        (
            TriggerAccess::Dids(BTreeSet::from([
                NICO.parse().expect("DID should parse"),
                MAYA.parse().expect("DID should parse"),
            ])),
            "triggered_by = [",
        ),
    ] {
        let fixture = Fixture::new("trigger-access");
        let manifest = fixture.manifest();
        let _removed = fs::remove_file(&manifest);
        let entry = Fixture::entry(Member::Nico, NICO);
        let update = ManifestUpdate::create(
            entry.clone(),
            SecurityManifest {
                repository: RepoId::from_str(REPOSITORY).expect("repository ID should parse"),
                recipient: RECIPIENT.to_owned(),
            },
            access.clone(),
        );

        assert!(matches!(update.prepare(&manifest), Ok(true)));

        let contents = fs::read_to_string(&manifest).expect("manifest should be readable");
        assert!(
            contents.contains(expected),
            "{access:?} should serialize as '{expected}', got:\n{contents}"
        );
        let triggered_by = contents
            .find("triggered_by")
            .expect("the generated manifest should set 'triggered_by'");
        let security = contents
            .find("[security]")
            .expect("the generated manifest should contain the security table");
        assert!(
            triggered_by < security,
            "'triggered_by' must precede '[security]', or TOML makes it a key of that \
             table:\n{contents}"
        );
        Manifest::validate_candidate(&contents, Member::Nico, entry.did)
            .expect("the generated manifest should parse back");
    }
}

#[test]
fn prepares_and_extends_a_manifest_safely() {
    let fixture = Fixture::new("prepare");
    let manifest = fixture.manifest();
    let nico = Fixture::entry(Member::Nico, NICO);
    let mut maya = Fixture::entry(Member::Maya, MAYA);
    maya.generation = 4;

    assert!(matches!(
        Fixture::create(nico.clone()).prepare(&manifest),
        Ok(true)
    ));
    #[cfg(unix)]
    let original_inode = fs::metadata(&manifest)
        .expect("manifest metadata should be readable")
        .ino();
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&manifest)
            .expect("manifest metadata should be readable")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(matches!(
        Fixture::extend(nico).prepare(&manifest),
        Ok(false)
    ));
    assert!(matches!(Fixture::extend(maya).prepare(&manifest), Ok(true)));
    #[cfg(unix)]
    assert_ne!(
        fs::metadata(&manifest)
            .expect("manifest metadata should be readable")
            .ino(),
        original_inode,
        "an existing manifest should be replaced atomically"
    );
    let contents = fs::read_to_string(&manifest).expect("manifest should be readable");
    assert!(contents.starts_with("version = 1"));
    assert!(contents.contains("[members.nico]"));
    assert!(contents.contains("[members.maya]"));
    assert!(contents.contains("generation = 4"));
    let nico = Manifest::validate_candidate(
        &contents,
        Member::Nico,
        NICO.parse().expect("Nico DID should parse"),
    )
    .expect("Nico binding should validate");
    let maya = Manifest::validate_candidate(
        &contents,
        Member::Maya,
        MAYA.parse().expect("Maya DID should parse"),
    )
    .expect("Maya binding should validate");
    assert!(!nico.mcp_allow().allows("create_issue"));
    assert!(maya.mcp_allow().allows("create_issue"));
}

#[test]
fn rejects_manifest_state_changes_during_preparation() {
    let fixture = Fixture::new("manifest-state-change");
    let manifest = fixture.manifest();
    let entry = Fixture::entry(Member::Nico, NICO);

    assert!(matches!(
        Fixture::extend(entry.clone()).prepare(&manifest),
        Err(Error::MissingManifest)
    ));
    fs::write(&manifest, SECURITY).expect("manifest should be written");
    assert!(matches!(
        Fixture::create(entry).prepare(&manifest),
        Err(Error::ExistingManifest)
    ));
}

#[test]
fn rejects_a_conflicting_local_binding() {
    let fixture = Fixture::new("conflict");
    let manifest = fixture.manifest();
    fs::write(
        &manifest,
        format!(
            "{SECURITY}[members.nico]\nsigning_did = \"did:key:different\"\ngeneration = 1\nstate \
             = \"active\"\nmcp_allow = []\n"
        ),
    )
    .expect("manifest should be written");

    assert!(matches!(
        Fixture::extend(Fixture::entry(Member::Nico, NICO)).prepare(&manifest),
        Err(Error::ConflictingBinding(Member::Nico))
    ));

    fs::write(
        &manifest,
        format!(
            "version = 1\n\n[security]\nrepository = \
             \"rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc\"\nrecipients = \
             [\"age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa\"]\n\n# \
             [members.nico]\n[members.maya]\nsigning_did = \"{MAYA}\"\ngeneration = 1\nstate = \
             \"active\"\nmcp_allow = []\n"
        ),
    )
    .expect("commented manifest should be written");
    assert!(matches!(
        Fixture::extend(Fixture::entry(Member::Nico, NICO)).prepare(&manifest),
        Ok(true)
    ));
}

#[test]
fn extends_inline_and_dotted_member_maps() {
    let fixture = Fixture::new("alternate-member-maps");
    let manifest = fixture.manifest();
    let nico = Fixture::entry(Member::Nico, NICO);
    let inline = format!(
        "version = 1\nsecurity = {{ repository = \"rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc\", \
         recipients = [\"age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa\"] \
         }}\nmembers = {{ maya = {{ signing_did = \"{MAYA}\", generation = 1, state = \"active\", \
         mcp_allow = [] }} }}\n"
    );
    fs::write(&manifest, inline).expect("inline manifest should be written");

    assert!(matches!(
        Fixture::extend(nico.clone()).prepare(&manifest),
        Ok(true)
    ));
    let contents = fs::read_to_string(&manifest).expect("manifest should be readable");
    assert!(contents.contains("members = {"));
    Manifest::validate_candidate(&contents, Member::Nico, nico.did)
        .expect("inline manifest should validate");

    let dotted = format!(
        "version = 1\nsecurity.repository = \
         \"rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc\"\nsecurity.recipients = \
         [\"age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa\"]\nmembers.maya.\
         signing_did = \"{MAYA}\"\nmembers.maya.generation = 1\nmembers.maya.state = \
         \"active\"\nmembers.maya.mcp_allow = []\n"
    );
    fs::write(&manifest, dotted).expect("dotted manifest should be written");
    assert!(matches!(
        Fixture::extend(nico.clone()).prepare(&manifest),
        Ok(true)
    ));
    let contents = fs::read_to_string(&manifest).expect("manifest should be readable");
    Manifest::validate_candidate(&contents, Member::Nico, nico.did)
        .expect("dotted manifest should validate");
}

#[test]
fn validates_a_checkout_manifest_before_writing_it() {
    let fixture = Fixture::new("invalid-checkout-manifest");
    let manifest = fixture.manifest();
    let unsupported = format!(
        "version = 2\n\n[security]\nrepository = \"{REPOSITORY}\"\nrecipients = \
         [\"{RECIPIENT}\"]\n\n[members]\n"
    );
    fs::write(&manifest, &unsupported).expect("manifest should be written");

    assert!(matches!(
        Fixture::extend(Fixture::entry(Member::Nico, NICO)).prepare(&manifest),
        Err(Error::Manifest(crate::manifest::Error::Schema {
            actual: 2
        }))
    ));
    assert_eq!(
        fs::read_to_string(&manifest).expect("manifest should be readable"),
        unsupported
    );

    let duplicate = format!(
        "{SECURITY}[members.maya]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
         \"active\"\nmcp_allow = []\n"
    );
    fs::write(&manifest, &duplicate).expect("manifest should be written");
    assert!(matches!(
        Fixture::extend(Fixture::entry(Member::Nico, NICO)).prepare(&manifest),
        Err(Error::Manifest(crate::manifest::Error::DuplicateDid { .. }))
    ));
    assert_eq!(
        fs::read_to_string(&manifest).expect("manifest should be readable"),
        duplicate
    );
}

#[cfg(unix)]
#[test]
fn rejects_a_symlink_when_writing_an_explicit_manifest() {
    let fixture = Fixture::new("symlink-manifest");
    let target = fixture.0.join("target.toml");
    let manifest = fixture.manifest();
    let original = format!(
        "{SECURITY}[members.maya]\nsigning_did = \"{MAYA}\"\ngeneration = 1\nstate = \
         \"active\"\nmcp_allow = []\n"
    );
    fs::write(&target, &original).expect("target manifest should be written");
    symlink("target.toml", &manifest).expect("manifest symlink should be created");

    assert!(matches!(
        Fixture::extend(Fixture::entry(Member::Nico, NICO)).prepare(&manifest),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput
    ));
    assert_eq!(
        fs::read_to_string(target).expect("target manifest should be readable"),
        original
    );
}

// Setup finishing without credentials would leave a member which fails at its first delivery, so
// the wizard authorizes as part of choosing. A provider needing nothing must not be asked about.
#[tokio::test]
async fn authorizes_only_a_chosen_provider_which_needs_it() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("setup-provider-auth");
    let _removed = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("test directory should be created");
    let providers = crate::provider::installed(&root).expect("providers should install");
    let wizard = SetupWizard::new(
        crate::Application::new(
            Member::Nico,
            RepoId::from_str(REPOSITORY).expect("repository should parse"),
        )
        .with_home(root.join("member")),
    );

    let (local, provider) = providers
        .iter()
        .filter_map(|(name, provider)| provider.as_ref().ok().map(|provider| (name, provider)))
        .find(|(_name, provider)| provider.ways() == [crate::provider::Way::None])
        .expect("a provider needing no authorization should ship");

    wizard
        .authorize(&root, local, provider)
        .await
        .expect("a provider needing nothing must not prompt");

    let (keyed, provider) = providers
        .iter()
        .filter_map(|(name, provider)| provider.as_ref().ok().map(|provider| (name, provider)))
        .find(|(_name, provider)| provider.ways().contains(&crate::provider::Way::Key))
        .expect("a provider needing a key should ship");
    crate::provider::credentials::store(
        &root.join(crate::provider::credentials::DIRECTORY),
        keyed,
        &crate::provider::Credentials::key(
            provider
                .key_variables()
                .iter()
                .map(|name| (name.clone(), "test-key".into()))
                .collect(),
        ),
    )
    .expect("credentials should be written");

    wizard
        .authorize(&root, keyed, provider)
        .await
        .expect("an already authorized provider must not prompt again");
}

// A name nobody installed would be written into the shared manifest and accepted there forever,
// while the member it names has no credentials and no models.
#[test]
fn refuses_a_provider_name_nobody_installed() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("setup-provider-unknown");
    let _removed = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("test directory should be created");
    let providers = crate::provider::installed(&root).expect("providers should install");
    let wizard = SetupWizard::new(
        crate::Application::new(
            Member::Nico,
            RepoId::from_str(REPOSITORY).expect("repository should parse"),
        )
        .with_provider("anthropc"),
    );

    let error = wizard
        .choice(&providers)
        .expect_err("a provider nobody installed must be refused");

    assert!(
        matches!(
            error,
            Error::Provider(boxed)
                if matches!(*boxed, crate::cli::provider::Error::Provider(
                    crate::provider::Error::Unknown(_)
                ))
        ),
        "the operator has to be told the name is not one they have"
    );
}
