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

#[test]
fn file_logging_appends_plain_records_and_the_final_error_without_hiding_console_output() {
    let root = tempfile::tempdir().expect("temporary directory");
    let home = MemberHome::at(root.path().join("maya"));
    home.create().expect("member directory");
    home.record(
        Member::Maya,
        "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc".parse().expect("RID"),
    )
    .expect("member binding");
    std::fs::create_dir_all(home.identity()).expect("identity directory");
    std::fs::write(home.identity().join("config.json"), "{}").expect("partial identity");
    let path = root.path().join("member.log");
    std::fs::write(&path, "previous session\n").expect("existing log");
    for level in ["info", "off"] {
        let output = Command::new(env!("CARGO_BIN_EXE_rad-team"))
            .args(["run", "--plain", "--log", level, "--log-file"])
            .arg(&path)
            .arg("--home")
            .arg(home.path())
            .env_remove("NO_COLOR")
            .env_remove("RADICLE_TEAM_HOME")
            .env_remove("RAD_PASSPHRASE")
            .env("PATH", root.path().join("no-executables"))
            .output()
            .expect("run with file logging");
        assert!(!output.status.success(), "partial identity stops startup");
        let console = String::from_utf8(output.stdout).expect("console logs");
        if level == "info" {
            assert!(console.contains("Unlocking identity"));
            assert!(
                console.contains('\x1b'),
                "normal console coloring is preserved"
            );
        } else {
            assert!(console.is_empty());
        }
        let contents = std::fs::read_to_string(&path).expect("saved log");
        let failure = String::from_utf8(output.stderr).expect("error output");
        assert!(contents.starts_with("previous session\n"));
        assert!(contents.contains("Unlocking identity"));
        assert!(
            contents.ends_with(&failure),
            "final errors must reach the log file"
        );
        assert!(!contents.contains('\x1b'), "files must contain plain text");
    }
    let invalid = root.path().join("missing/member.log");
    let output = Command::new(env!("CARGO_BIN_EXE_rad-team"))
        .args(["run", "--log-file"])
        .arg(&invalid)
        .output()
        .expect("invalid file destination");
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).expect("startup error");
    assert!(error.contains("could not write log file"));
    assert!(error.contains(invalid.to_str().expect("UTF-8 path")));
    assert!(!invalid.exists());
}

#[cfg(unix)]
#[test]
fn log_timestamps_follow_the_process_timezone_and_include_its_offset() {
    let root = tempfile::tempdir().expect("temporary directory");
    let home = MemberHome::at(root.path().join("maya"));
    home.create().expect("create member directory");
    home.record(
        Member::Maya,
        "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc".parse().expect("RID"),
    )
    .expect("record binding");
    std::fs::create_dir_all(home.identity()).expect("identity directory");
    std::fs::write(home.identity().join("config.json"), "{}").expect("partial identity fixture");
    for (timezone, offset) in [("Etc/UTC", "+00:00"), ("Etc/GMT-3", "+03:00")] {
        let output = Command::new(env!("CARGO_BIN_EXE_rad-team"))
            .args(["run", "--plain", "--log", "info", "--home"])
            .arg(home.path())
            .env("TZ", timezone)
            .env("NO_COLOR", "1")
            .env("PATH", root.path().join("no-executables"))
            .env_remove("RADICLE_TEAM_HOME")
            .env_remove("RAD_PASSPHRASE")
            .output()
            .expect("run fixture");
        assert!(
            !output.status.success(),
            "partial identity must stop startup"
        );
        let logs = String::from_utf8(output.stdout).expect("UTF-8 logs");
        let timestamp = logs.split_whitespace().next().expect("startup timestamp");
        assert!(
            timestamp.contains('T') && timestamp.ends_with(offset),
            "{timezone}: {logs}"
        );
        assert!(logs.contains("Unlocking identity"), "{logs}");
    }
}
