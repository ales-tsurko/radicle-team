//! A run selects its member and repository from a prepared member directory.

use std::process::Command;

use radicle_team::{
    home::{ENVIRONMENT_VARIABLE, MemberHome},
    member::Member,
};

#[test]
fn run_discovers_the_home_and_honors_explicit_path_precedence() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    let parent = root
        .path()
        .canonicalize()
        .expect("temporary path should resolve");
    let home = MemberHome::at(parent.join("maya"));
    home.create().expect("member directory should be created");
    home.record_identity(Member::Maya)
        .expect("identity-only marker should be recorded");
    let nested = home.code().join("coder");
    std::fs::create_dir_all(&nested).expect("nested directory should be created");
    let unrelated = parent.join("unrelated");

    // An unbound identity exercises CLI discovery without starting a node or accessing keys.
    for (working, environment, explicit) in [
        (home.path(), None, None),
        (nested.as_path(), None, None),
        (parent.as_path(), Some(home.path()), None),
        (
            parent.as_path(),
            Some(unrelated.as_path()),
            Some(home.path()),
        ),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rad-team"));
        command
            .current_dir(working)
            .arg("run")
            .env_remove(ENVIRONMENT_VARIABLE)
            .env("PATH", parent.join("no-executables"))
            .env("RUST_LOG", "warn");
        if let Some(environment) = environment {
            command.env(ENVIRONMENT_VARIABLE, environment);
        }
        if let Some(explicit) = explicit {
            command.arg("--home").arg(explicit);
        }
        let output = command.output().expect("run command should execute");
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "an unbound identity must be refused"
        );
        assert!(message.contains("has no repository binding"), "{output:?}");
        assert!(message.starts_with("Error: "), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(
            message.contains(&home.path().display().to_string()),
            "{output:?}"
        );
    }

    assert!(!home.process_lock().exists());
    assert!(!home.identity().exists());
    assert!(!home.node_log().exists());
    assert!(!unrelated.exists());
}

#[test]
fn run_refuses_an_ordinary_checkout_without_creating_member_data() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    let checkout = root.path().join("checkout");
    git2::Repository::init(&checkout).expect("ordinary checkout should be initialized");

    for (explicit, plain) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rad-team"));
        command
            .current_dir(&checkout)
            .arg("run")
            .env_remove(ENVIRONMENT_VARIABLE)
            .env("PATH", root.path().join("no-executables"))
            .env("RUST_LOG", "warn");
        if explicit {
            command.arg("--home").arg(&checkout);
        }
        if plain {
            command.arg("--plain");
        }
        let output = command.output().expect("run command should execute");
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "a checkout alone cannot run a member"
        );
        assert!(
            if explicit {
                message.contains("could not read") && message.contains("radicle-team-member.toml")
            } else {
                message.contains("no member directory found")
            },
            "{output:?}"
        );
        assert!(message.starts_with("Error: "), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
    }
    assert!(!MemberHome::at(checkout).code().exists());
}
