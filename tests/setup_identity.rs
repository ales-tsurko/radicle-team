//! Identity provisioning must succeed before any repository access is available.

#![cfg(not(miri))]

use std::{path::Path, process::Command};

use radicle::{identity::Did, storage::ReadStorage as _};
use radicle_team::{
    home::MemberHome,
    identity::{KeyProtection, Manager, MemberLock, Provider},
    member::Member,
};

const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";

fn provision(home: &Path, member: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_radicle-team"))
        .args([
            "setup",
            member,
            "--identity-only",
            "--unencrypted-identity",
            "--home",
        ])
        .arg(home)
        // No node, Radicle CLI, or agent executable is available to this process.
        .env("PATH", home.join("no-executables"))
        .env("RUST_LOG", "warn")
        .env_remove("RAD_PASSPHRASE")
        .output()
        .expect("identity command should run")
}

#[test]
fn identity_only_creates_a_default_folder_and_reuses_its_identity() {
    for (repository, folder) in [(None, "maya"), (Some(REPOSITORY), "maya-z3cyotNH")] {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let home = MemberHome::at(root.path().join(folder));
        let mut command = Command::new(env!("CARGO_BIN_EXE_radicle-team"));
        command
            .current_dir(root.path())
            .args(["setup", "maya", "--identity-only", "--unencrypted-identity"])
            .env("PATH", root.path().join("no-executables"))
            .env("RUST_LOG", "warn")
            .env_remove("RADICLE_TEAM_HOME")
            .env_remove("RAD_PASSPHRASE");
        if let Some(repository) = repository {
            command.args(["--repo", repository]);
        }

        let first = command.output().expect("identity command should run");
        assert!(first.status.success(), "{first:?}");
        assert!(
            home.identity().is_dir(),
            "setup should create its default member folder"
        );
        assert_eq!(
            home.marker()
                .expect("marker should exist")
                .repository()
                .expect("repository should parse"),
            repository.map(|repository| repository.parse().expect("RID should parse"))
        );

        let repeated = command.output().expect("identity command should rerun");
        assert!(repeated.status.success(), "{repeated:?}");
        assert_eq!(repeated.stdout, first.stdout);
        let explicit = provision(home.path(), "maya");
        assert!(explicit.status.success(), "{explicit:?}");
        assert_eq!(explicit.stdout, first.stdout);
        assert!(!home.node_log().exists());
    }
}

#[test]
fn provisions_a_stable_identity_without_repository_access_or_node_tools() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    let home = MemberHome::at(root.path().join("maya"));
    let first = provision(home.path(), "maya");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let output = String::from_utf8(first.stdout).expect("output should be UTF-8");
    let did: Did = output
        .trim()
        .strip_prefix("maya identity: ")
        .expect("successful provisioning should print the member DID")
        .parse()
        .expect("printed DID should be valid");
    let marker = home
        .marker()
        .expect("identity should have an owned member directory");
    assert_eq!(marker.member().expect("member should parse"), Member::Maya);
    assert_eq!(marker.repository().expect("repository should parse"), None);
    assert!(
        !home.node_log().exists(),
        "identity provisioning must not start a node"
    );
    assert!(
        !home.sandbox_config().exists(),
        "identity provisioning must not run the setup wizard"
    );

    let identity = Manager::default()
        .load(
            Member::Maya,
            &home,
            &Provider::Managed {
                protection: KeyProtection::Unencrypted,
            },
        )
        .expect("full setup's identity loader should reuse the saved key");
    assert_eq!(identity.did(), did);
    assert!(
        !identity
            .profile()
            .storage
            .contains(&REPOSITORY.parse().expect("RID should parse"))
            .expect("storage should be readable")
    );

    let repeated = provision(home.path(), "maya");
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert_eq!(
        String::from_utf8(repeated.stdout).expect("output should be UTF-8"),
        output
    );

    let wrong_member = provision(home.path(), "nico");
    assert!(!wrong_member.status.success());
    assert!(
        String::from_utf8_lossy(&wrong_member.stdout).contains("belongs to maya"),
        "{wrong_member:?}"
    );

    // Selecting a repository later must preserve the key rather than provisioning another one.
    let bound = Command::new(env!("CARGO_BIN_EXE_radicle-team"))
        .args([
            "setup",
            "maya",
            "--identity-only",
            "--repo",
            REPOSITORY,
            "--home",
        ])
        .arg(home.path())
        .env("PATH", home.path().join("no-executables"))
        .env("RUST_LOG", "warn")
        .output()
        .expect("binding command should run");
    assert!(bound.status.success(), "{bound:?}");
    assert_eq!(
        String::from_utf8(bound.stdout).expect("output should be UTF-8"),
        output
    );
    assert_eq!(
        home.marker()
            .expect("marker should exist")
            .repository()
            .expect("repository should parse"),
        Some(REPOSITORY.parse().expect("RID should parse"))
    );
    let repeated_bound = provision(home.path(), "maya");
    assert!(repeated_bound.status.success(), "{repeated_bound:?}");
    assert_eq!(
        String::from_utf8(repeated_bound.stdout).expect("output should be UTF-8"),
        output
    );
    assert!(
        home.marker()
            .expect("marker should exist")
            .repository()
            .expect("repository should parse")
            .is_some(),
        "identity-only setup without --repo must preserve an existing binding"
    );

    let _lock =
        MemberLock::acquire(&home).expect("member lock should be available after provisioning");
    let busy = provision(home.path(), "maya");
    assert!(!busy.status.success());
    assert!(
        String::from_utf8_lossy(&busy.stdout).contains("already in use"),
        "{busy:?}"
    );
}
