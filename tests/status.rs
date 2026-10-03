//! Inspection commands resolve only the selected member directory and never start its runtime.

use std::{
    path::Path,
    process::{Command, Output},
};

use radicle_team::{
    home::{ENVIRONMENT_VARIABLE, MemberHome},
    member::Member,
};

struct Fixture {
    directory: tempfile::TempDir,
    maya: MemberHome,
    nico: MemberHome,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("test directory should create");
        let repository = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc"
            .parse()
            .expect("RID should parse");
        let maya = MemberHome::at(directory.path().join("maya"));
        let nico = MemberHome::at(directory.path().join("nico"));
        for (home, member) in [(&maya, Member::Maya), (&nico, Member::Nico)] {
            home.create().expect("home should create");
            home.record(member, repository)
                .expect("marker should write");
        }
        Self {
            directory,
            maya,
            nico,
        }
    }

    fn run(&self, cwd: &Path, args: &[&str], explicit: Option<&Path>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rad-team"));
        command
            .current_dir(cwd)
            .args(args)
            .env(ENVIRONMENT_VARIABLE, self.maya.path())
            .env("PATH", self.directory.path().join("no-executables"))
            .env("RUST_LOG", "warn");
        if let Some(home) = explicit {
            command.arg("--home").arg(home);
        }
        command.output().expect("inspection command should execute")
    }

    fn assert_untouched(&self) {
        for home in [&self.maya, &self.nico] {
            for path in [
                home.state_database(),
                home.process_lock(),
                home.identity(),
                home.node_log(),
            ] {
                assert!(
                    !path.exists(),
                    "inspection must not create {}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn status_and_tasks_discover_current_home_and_honor_explicit_precedence() {
    let fixture = Fixture::new();
    let nested = fixture.maya.worktree(radicle_team::agent::Role::Coder);
    std::fs::create_dir_all(&nested).expect("nested directory should create");
    for (cwd, explicit, member) in [
        (fixture.maya.path(), None, "maya"),
        (nested.as_path(), None, "maya"),
        (fixture.directory.path(), Some(fixture.nico.path()), "nico"),
        (fixture.maya.path(), Some(fixture.nico.path()), "nico"),
    ] {
        let output = fixture.run(cwd, &["status"], explicit);
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{output:?}");
        assert!(text.contains(&format!("Member: {member}")), "{text}");
        assert!(text.contains("Status: stopped"), "{text}");
        assert!(text.contains("Waiting tasks: 0"), "{text}");
        let output = fixture.run(cwd, &["task", "ls"], explicit);
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("No unfinished tasks."));
        let output = fixture.run(cwd, &["task", "show", "1"], explicit);
        assert!(!output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("unfinished task 1 not found"));
    }
    fixture.assert_untouched();
}

#[test]
fn inspection_rejects_invalid_and_identity_only_directories_without_fallback() {
    let fixture = Fixture::new();
    let ordinary = fixture.directory.path().join("checkout");
    git2::Repository::init(&ordinary).expect("ordinary checkout should create");
    let unbound = MemberHome::at(fixture.directory.path().join("identity-only"));
    unbound
        .create()
        .expect("identity-only directory should create");
    unbound
        .record_identity(Member::Maya)
        .expect("identity marker should write");
    for args in [
        vec!["status"],
        vec!["task", "ls"],
        vec!["task", "show", "1"],
    ] {
        for (cwd, explicit, expected) in [
            (ordinary.as_path(), None, "no member directory found"),
            (
                fixture.maya.path(),
                Some(ordinary.as_path()),
                "could not read",
            ),
            (unbound.path(), None, "has no repository binding"),
        ] {
            let output = fixture.run(cwd, &args, explicit);
            assert!(!output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(expected),
                "{output:?}"
            );
        }
    }
    fixture.assert_untouched();
    assert!(!MemberHome::at(ordinary).code().exists());
    assert!(!unbound.state_database().exists());
}
