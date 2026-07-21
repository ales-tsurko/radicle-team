#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::{fs, path::PathBuf, process::Command};

use radicle::identity::Did;

use crate::{
    SetupStatus,
    cli::setup::{Error, ManifestEntry, RepositoryCheckout, SetupWizard},
    manifest::{FILE_NAME, Manifest},
    member::Member,
};

const NICO: &str = "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
const MAYA: &str = "did:key:z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C";

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = PathBuf::from("target/tests/setup-wizard").join(name);
        let _result = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("fixture directory should be created");
        Self(path)
    }

    fn manifest(&self) -> PathBuf {
        self.0.join(".radicle-team.toml")
    }

    fn initialize_repository(&self) {
        self.git(&["init", "-b", "main"]);
        self.git(&["config", "user.name", "Test"]);
        self.git(&["config", "user.email", "test@example.com"]);
        fs::write(self.0.join("README.md"), "# Test\n").expect("repository file should be written");
        self.git(&["add", "README.md"]);
        self.git(&["commit", "-m", "Initial commit"]);
        self.git(&["init", "--bare", "-b", "main", "canonical.git"]);
        self.git(&["remote", "add", "rad", "canonical.git"]);
        self.git(&["push", "rad", "main:main"]);
        self.git(&["update-ref", "refs/remotes/rad/main", "HEAD"]);
        self.git(&["config", "branch.main.remote", "rad"]);
        self.git(&["config", "branch.main.merge", "refs/heads/main"]);
    }

    fn git(&self, arguments: &[&str]) {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(&self.0)
            .output()
            .expect("Git should run");
        assert!(output.status.success(), "git {arguments:?} should succeed");
    }

    #[cfg(unix)]
    fn rejecting_hook(&self, name: &str) -> PathBuf {
        let hook = self.0.join(".git/hooks").join(name);
        fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("Git hook should be written");
        let mut permissions = fs::metadata(&hook)
            .expect("hook metadata should be readable")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&hook, permissions).expect("Git hook should be executable");

        hook
    }

    fn entry(member: Member, did: &str) -> ManifestEntry {
        ManifestEntry {
            member,
            did: did.parse::<Did>().expect("DID should parse"),
            generation: 1,
        }
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
        entry,
        SetupStatus::ManifestMissing { generation: 1 },
    );

    assert!(missing.contains("config/team-policy.toml"));
    assert!(missing.contains("create explicit manifest"));
    assert!(missing.contains("version = 1"));
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
fn prepares_and_extends_a_manifest_safely() {
    let fixture = Fixture::new("prepare");
    let manifest = fixture.manifest();
    let nico = Fixture::entry(Member::Nico, NICO);
    let mut maya = Fixture::entry(Member::Maya, MAYA);
    maya.generation = 4;

    assert!(matches!(
        nico.prepare(&manifest, SetupStatus::ManifestMissing { generation: 1 }),
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
        nico.prepare(&manifest, SetupStatus::BindingMissing { generation: 1 }),
        Ok(false)
    ));
    assert!(matches!(
        maya.prepare(&manifest, SetupStatus::BindingMissing { generation: 1 }),
        Ok(true)
    ));
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
}

#[test]
fn rejects_a_conflicting_local_binding() {
    let fixture = Fixture::new("conflict");
    let manifest = fixture.manifest();
    fs::write(
        &manifest,
        "version = 1\n\n[members.nico]\nsigning_did = \"did:key:different\"\ngeneration = \
         1\nstate = \"active\"\n",
    )
    .expect("manifest should be written");

    assert!(matches!(
        Fixture::entry(Member::Nico, NICO)
            .prepare(&manifest, SetupStatus::BindingMissing { generation: 1 }),
        Err(Error::ConflictingBinding(Member::Nico))
    ));

    fs::write(
        &manifest,
        format!(
            "version = 1\n\n# [members.nico]\n[members.maya]\nsigning_did = \
             \"{MAYA}\"\ngeneration = 1\nstate = \"active\"\n"
        ),
    )
    .expect("commented manifest should be written");
    assert!(matches!(
        Fixture::entry(Member::Nico, NICO)
            .prepare(&manifest, SetupStatus::BindingMissing { generation: 1 }),
        Ok(true)
    ));
}

#[test]
fn extends_inline_and_dotted_member_maps() {
    let fixture = Fixture::new("alternate-member-maps");
    let manifest = fixture.manifest();
    let nico = Fixture::entry(Member::Nico, NICO);
    let inline = format!(
        "version = 1\nmembers = {{ maya = {{ signing_did = \"{MAYA}\", generation = 1, state = \
         \"active\" }} }}\n"
    );
    fs::write(&manifest, inline).expect("inline manifest should be written");

    assert!(matches!(
        nico.prepare(&manifest, SetupStatus::BindingMissing { generation: 1 }),
        Ok(true)
    ));
    let contents = fs::read_to_string(&manifest).expect("manifest should be readable");
    assert!(contents.contains("members = {"));
    Manifest::validate_candidate(&contents, Member::Nico, nico.did)
        .expect("inline manifest should validate");

    let dotted = format!(
        "version = 1\nmembers.maya.signing_did = \"{MAYA}\"\nmembers.maya.generation = \
         1\nmembers.maya.state = \"active\"\n"
    );
    fs::write(&manifest, dotted).expect("dotted manifest should be written");
    assert!(matches!(
        nico.prepare(&manifest, SetupStatus::BindingMissing { generation: 1 }),
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
    let unsupported = "version = 2\n[members]\n";
    fs::write(&manifest, unsupported).expect("manifest should be written");

    assert!(matches!(
        Fixture::entry(Member::Nico, NICO)
            .prepare(&manifest, SetupStatus::BindingMissing { generation: 1 }),
        Err(Error::Manifest(crate::manifest::Error::Schema {
            actual: 2
        }))
    ));
    assert_eq!(
        fs::read_to_string(&manifest).expect("manifest should be readable"),
        unsupported
    );

    let duplicate = format!(
        "version = 1\n\n[members.maya]\nsigning_did = \"{NICO}\"\ngeneration = 1\nstate = \
         \"active\"\n"
    );
    fs::write(&manifest, &duplicate).expect("manifest should be written");
    assert!(matches!(
        Fixture::entry(Member::Nico, NICO)
            .prepare(&manifest, SetupStatus::BindingMissing { generation: 1 }),
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
        "version = 1\n\n[members.maya]\nsigning_did = \"{MAYA}\"\ngeneration = 1\nstate = \
         \"active\"\n"
    );
    fs::write(&target, &original).expect("target manifest should be written");
    symlink("target.toml", &manifest).expect("manifest symlink should be created");

    assert!(matches!(
        Fixture::entry(Member::Nico, NICO)
            .prepare(&manifest, SetupStatus::BindingMissing { generation: 1 }),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput
    ));
    assert_eq!(
        fs::read_to_string(target).expect("target manifest should be readable"),
        original
    );
}

#[test]
fn skips_a_commit_when_the_manifest_entry_exists() {
    let fixture = Fixture::new("unchanged-manifest");
    fixture.initialize_repository();
    let entry = Fixture::entry(Member::Nico, NICO);
    entry
        .prepare(
            &fixture.manifest(),
            SetupStatus::ManifestMissing { generation: 1 },
        )
        .expect("manifest should be created");
    fixture.git(&["add", FILE_NAME]);
    fixture.git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "Add manifest",
    ]);
    fixture.git(&["push", "rad", "main:main"]);
    let checkout = RepositoryCheckout {
        path: fixture.0.clone(),
    };

    assert!(matches!(
        checkout.publish(entry, SetupStatus::BindingMissing { generation: 1 }),
        Ok(false)
    ));
}

#[test]
fn rejects_an_ignored_repository_manifest() {
    let fixture = Fixture::new("ignored-manifest");
    fixture.initialize_repository();
    fs::write(fixture.0.join(".gitignore"), format!("{FILE_NAME}\n"))
        .expect("ignore rule should be written");
    let entry = Fixture::entry(Member::Nico, NICO);
    entry
        .prepare(
            &fixture.manifest(),
            SetupStatus::ManifestMissing { generation: 1 },
        )
        .expect("ignored manifest should be created locally");
    let checkout = RepositoryCheckout {
        path: fixture.0.clone(),
    };

    assert!(matches!(
        checkout.publish(entry, SetupStatus::BindingMissing { generation: 1 }),
        Err(Error::IgnoredManifest)
    ));
    assert!(fixture.manifest().exists());
}

#[cfg(unix)]
#[test]
fn restores_the_checkout_after_a_failed_patch_push() {
    let fixture = Fixture::new("failed-patch-push");
    fixture.initialize_repository();
    let checkout = RepositoryCheckout {
        path: fixture.0.clone(),
    };
    let base = checkout.revision("HEAD").expect("HEAD should resolve");
    let note = fixture.0.join("local-notes.txt");
    fs::write(&note, "preserve me\n").expect("unrelated work should be written");
    let hook = fixture.rejecting_hook("pre-push");
    let entry = Fixture::entry(Member::Nico, NICO);

    assert!(matches!(
        checkout.publish(entry, SetupStatus::ManifestMissing { generation: 1 }),
        Err(Error::Command { .. })
    ));
    assert_eq!(
        checkout.revision("HEAD").expect("HEAD should resolve"),
        base
    );
    assert!(!fixture.manifest().exists());
    assert_eq!(
        fs::read_to_string(&note).expect("unrelated work should remain"),
        "preserve me\n"
    );
    checkout
        .ensure_clean_manifest()
        .expect("manifest should be clean after recovery");

    fs::remove_file(hook).expect("push hook should be removed");
    checkout
        .validate_manifest_change()
        .expect("retry should pass checkout validation");
    assert!(matches!(
        checkout.commit_manifest(entry, SetupStatus::ManifestMissing { generation: 1 }),
        Ok(true)
    ));
}

#[cfg(unix)]
#[test]
fn restores_the_checkout_after_a_failed_commit() {
    let fixture = Fixture::new("failed-manifest-commit");
    fixture.initialize_repository();
    let checkout = RepositoryCheckout {
        path: fixture.0.clone(),
    };
    let base = checkout.revision("HEAD").expect("HEAD should resolve");
    let hook = fixture.rejecting_hook("pre-commit");
    let entry = Fixture::entry(Member::Nico, NICO);

    assert!(matches!(
        checkout.publish(entry, SetupStatus::ManifestMissing { generation: 1 }),
        Err(Error::Command { .. })
    ));
    assert_eq!(
        checkout.revision("HEAD").expect("HEAD should resolve"),
        base
    );
    assert!(!fixture.manifest().exists());
    checkout
        .ensure_clean_manifest()
        .expect("manifest should be clean after recovery");
    checkout
        .ensure_clean_index()
        .expect("index should be clean after recovery");

    fs::remove_file(hook).expect("commit hook should be removed");
    checkout
        .validate_manifest_change()
        .expect("retry should pass checkout validation");
    assert!(matches!(
        checkout.commit_manifest(entry, SetupStatus::ManifestMissing { generation: 1 }),
        Ok(true)
    ));
}

#[test]
fn opens_a_manifest_patch_only_from_the_canonical_head() {
    let fixture = Fixture::new("new-patch");
    fixture.initialize_repository();
    let checkout = RepositoryCheckout {
        path: fixture.0.clone(),
    };
    let commands = checkout
        .commands(Member::Nico)
        .expect("commands should be generated");

    assert!(commands.ends_with("git push rad HEAD:refs/patches"));
    assert!(matches!(
        RepositoryCheckout::validate_upstream(Some("rad/patches/0123456789abcdef")),
        Err(Error::ExistingPatch)
    ));
    RepositoryCheckout::validate_upstream(Some("origin/main"))
        .expect("a non-Radicle upstream should be allowed");
    RepositoryCheckout::validate_upstream(None).expect("a missing upstream should be allowed");

    fs::write(fixture.0.join("canonical.txt"), "new canonical work\n")
        .expect("canonical file should be written");
    fixture.git(&["add", "canonical.txt"]);
    fixture.git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "Advance canonical branch",
    ]);
    fixture.git(&["push", "rad", "main:main"]);
    fixture.git(&["reset", "--hard", "HEAD~1"]);
    fixture.git(&["update-ref", "refs/remotes/rad/main", "HEAD"]);
    assert!(matches!(
        checkout.commands(Member::Nico),
        Err(Error::NonCanonicalCheckout)
    ));
}

#[test]
fn accepts_any_checkout_at_the_live_canonical_head() {
    let origin = Fixture::new("origin-upstream");
    origin.initialize_repository();
    origin.git(&["remote", "add", "origin", "canonical.git"]);
    origin.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    origin.git(&["config", "branch.main.remote", "origin"]);
    let checkout = RepositoryCheckout {
        path: origin.0.clone(),
    };
    checkout
        .ensure_canonical_head()
        .expect("an origin-tracking canonical checkout should be accepted");

    let untracked = Fixture::new("no-upstream");
    untracked.initialize_repository();
    untracked.git(&["branch", "--unset-upstream"]);
    let checkout = RepositoryCheckout {
        path: untracked.0.clone(),
    };
    checkout
        .ensure_canonical_head()
        .expect("a canonical checkout without an upstream should be accepted");

    let detached = Fixture::new("detached-head");
    detached.initialize_repository();
    detached.git(&["checkout", "--detach", "HEAD"]);
    let checkout = RepositoryCheckout {
        path: detached.0.clone(),
    };
    checkout
        .ensure_canonical_head()
        .expect("a detached canonical checkout should be accepted");
}
