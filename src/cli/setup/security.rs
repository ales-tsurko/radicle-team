//! Private security-report repository provisioning.

use std::{process::Command, str::FromStr as _};

use age::{secrecy::ExposeSecret as _, x25519};
use radicle::{
    identity::{Did, RepoId},
    profile::Profile,
};

use crate::cli::setup::Error;

#[derive(Debug, Clone)]
pub(crate) struct SecurityManifest {
    pub(super) repository: RepoId,
    pub(super) recipient: String,
}

#[derive(Debug)]
pub(crate) struct SecuritySetup {
    manifest: SecurityManifest,
    identity: age::secrecy::SecretString,
}

impl SecuritySetup {
    pub(super) fn provision(did: Did) -> Result<Self, Error> {
        Self::authenticate()?;
        let repository = Self::create_repository()?;
        Self::grant_access(repository, did)?;
        Ok(Self::generate(repository))
    }

    pub(super) fn existing(repository: RepoId, did: Did) -> Result<Self, Error> {
        Self::ensure_private(repository)?;
        Self::grant(repository, did)?;
        Ok(Self::generate(repository))
    }

    fn create_repository() -> Result<RepoId, Error> {
        let directory = tempfile::tempdir()?;
        Self::initialize_git(directory.path())?;
        Self::command(
            Command::new("rad")
                .args([
                    "init",
                    "--name",
                    "radicle-team-security",
                    "--description",
                    "Encrypted radicle-team security reports",
                    "--private",
                    "--no-confirm",
                ])
                .arg(directory.path()),
        )?;
        let repository = Self::repository(directory.path())?;

        Ok(repository)
    }

    fn generate(repository: RepoId) -> Self {
        let identity = x25519::Identity::generate();
        let recipient = identity.to_public().to_string();

        Self {
            manifest: SecurityManifest {
                repository,
                recipient,
            },
            identity: identity.to_string(),
        }
    }

    pub(super) fn manifest(&self) -> SecurityManifest {
        self.manifest.clone()
    }

    pub(super) fn recovery_key(&self) -> &str {
        self.identity.expose_secret()
    }

    pub(super) fn grant(repository: RepoId, did: Did) -> Result<(), Error> {
        Self::authenticate()?;
        Self::grant_access(repository, did)
    }

    fn authenticate() -> Result<(), Error> {
        let profile = Profile::load().map_err(Error::OperatorProfile)?;
        match profile.signer() {
            Ok(_) => return Ok(()),
            Err(error) if error.prompt_for_passphrase() => {}
            Err(error) => return Err(Error::OperatorSigner(error)),
        }

        // `rad init` can unlock a key just for itself, whereas `rad id update` needs the key
        // registered with ssh-agent. Authenticate before creating anything, using the operator's
        // profile inherited by both commands. The member's separate key cannot grant access.
        Self::command(Command::new("rad").arg("auth"))?;
        // `rad auth` may succeed without a running ssh-agent. Verify that later commands can sign.
        profile.signer().map_err(Error::OperatorSigner)?;
        Ok(())
    }

    fn grant_access(repository: RepoId, did: Did) -> Result<(), Error> {
        Self::command(
            Command::new("rad")
                .args(["id", "update", "--repo"])
                .arg(repository.to_string())
                .args(["--allow"])
                .arg(did.to_string())
                .args([
                    "--title",
                    "Grant security-report access",
                    "--description",
                    "Allow a radicle-team member to submit encrypted security reports.",
                    "--no-confirm",
                ]),
        )?;
        Self::command(
            Command::new("rad")
                .args(["sync", "--announce", "--replicas", "1"])
                .arg(repository.to_string()),
        )
    }

    fn initialize_git(path: &std::path::Path) -> Result<(), Error> {
        let repository = git2::Repository::init(path)?;
        repository.set_head("refs/heads/main")?;
        let tree = {
            let oid = repository.treebuilder(None)?.write()?;
            repository.find_tree(oid)?
        };
        let signature = git2::Signature::now("radicle-team", "security@localhost")?;
        let _commit = repository.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Initialize security reports",
            &tree,
            &[],
        )?;

        Ok(())
    }

    fn repository(path: &std::path::Path) -> Result<RepoId, Error> {
        let output = Command::new("rad")
            .args(["inspect", "--rid"])
            .arg(path)
            .output()?;
        if !output.status.success() {
            return Err(Error::Command {
                command: "rad inspect --rid".to_owned(),
                status: output.status,
            });
        }
        let value = String::from_utf8_lossy(&output.stdout);
        RepoId::from_str(value.trim()).map_err(Error::CheckoutIdentity)
    }

    fn ensure_private(repository: RepoId) -> Result<(), Error> {
        let output = Command::new("rad")
            .args(["inspect", "--visibility"])
            .arg(repository.to_string())
            .output()?;
        if !output.status.success() {
            return Err(Error::Command {
                command: "rad inspect --visibility".to_owned(),
                status: output.status,
            });
        }
        if String::from_utf8_lossy(&output.stdout).trim() == "private" {
            Ok(())
        } else {
            Err(Error::PublicSecurityRepository(repository))
        }
    }

    fn command(command: &mut Command) -> Result<(), Error> {
        let rendered = format!("{command:?}");
        let status = command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Command {
                command: rendered,
                status,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::setup::SecuritySetup;

    #[test]
    #[cfg(not(miri))]
    fn initializes_the_security_repository_source() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");

        SecuritySetup::initialize_git(directory.path())
            .expect("security repository source should initialize");

        let repository =
            git2::Repository::open(directory.path()).expect("Git repository should open");
        let head = repository.head().expect("repository should have a head");
        assert_eq!(head.shorthand(), Some("main"));
        assert_eq!(
            head.peel_to_commit()
                .expect("head should resolve to a commit")
                .message(),
            Some("Initialize security reports")
        );
    }

    #[test]
    #[cfg(all(unix, not(miri)))]
    fn authenticates_before_creating_or_granting_access() {
        use std::{fs, os::unix::fs::PermissionsExt as _, process::Command};

        use radicle::{
            crypto::Seed,
            node::Alias,
            profile::{Home, Profile},
            storage::ReadStorage as _,
        };

        use crate::cli::setup::Error;

        const CASE: &str = "RADICLE_TEAM_SECURITY_AUTH_CASE";
        if let Ok(case) = std::env::var(CASE) {
            let profile = Profile::load().expect("isolated operator profile should load");
            if case == "unlocked" {
                SecuritySetup::authenticate().expect("an unlocked key needs no authentication");
                return;
            }
            let repository = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc"
                .parse()
                .expect("repository ID should parse");
            for result in [
                SecuritySetup::provision(profile.did()).map(|_| ()),
                SecuritySetup::grant(repository, profile.did()),
            ] {
                if case == "auth-fails" {
                    assert!(
                        matches!(result, Err(Error::Command { status, .. }) if status.code() == Some(7))
                    );
                } else {
                    assert!(matches!(result, Err(Error::OperatorSigner(_))));
                }
            }
            assert!(
                profile
                    .storage
                    .repositories()
                    .expect("storage should open")
                    .is_empty()
            );
            return;
        }

        for case in ["auth-fails", "agent-missing", "unlocked"] {
            let root = tempfile::tempdir().expect("temporary directory should be created");
            let home = Home::new(root.path().join("identity")).expect("profile home should exist");
            let passphrase = (case != "unlocked").then(|| "test-passphrase".to_owned().into());
            Profile::init(
                home,
                Alias::new("operator"),
                passphrase,
                Seed::new([41; 32]),
            )
            .expect("isolated operator identity should be created");
            let rad = root.path().join("rad");
            fs::write(
                &rad,
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$RADICLE_TEAM_SECURITY_AUTH_LOG\"\ntest \
                 \"$*\" = auth || exit 9\ntest \"$RADICLE_TEAM_SECURITY_AUTH_CASE\" != auth-fails \
                 || exit 7\n",
            )
            .expect("Radicle stub should be written");
            fs::set_permissions(&rad, fs::Permissions::from_mode(0o700))
                .expect("Radicle stub should be executable");
            let log = root.path().join("commands");
            let output = Command::new(std::env::current_exe().expect("test binary should exist"))
                .args([
                    "--exact",
                    "cli::setup::security::tests::authenticates_before_creating_or_granting_access",
                    "--nocapture",
                ])
                .env(CASE, case)
                .env("RAD_HOME", root.path().join("identity"))
                .env_remove("RAD_PASSPHRASE")
                .env_remove("RAD_SOCKET")
                .env("SSH_AUTH_SOCK", root.path().join("missing-agent.sock"))
                .env("PATH", root.path())
                .env("RADICLE_TEAM_SECURITY_AUTH_LOG", &log)
                .output()
                .expect("isolated authentication test should start");
            assert!(output.status.success(), "{case}: {output:?}");
            if case == "unlocked" {
                assert!(
                    !log.exists(),
                    "an available signing key must not prompt again"
                );
            } else {
                assert_eq!(
                    fs::read_to_string(log).expect("commands should be logged"),
                    "auth\nauth\n"
                );
            }
        }
    }

    #[test]
    fn generates_an_encryption_identity() {
        let repository = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc"
            .parse()
            .expect("repository ID should parse");

        let setup = SecuritySetup::generate(repository);

        assert_eq!(setup.manifest().repository, repository);
        assert!(setup.manifest().recipient.starts_with("age1"));
        assert!(setup.recovery_key().starts_with("AGE-SECRET-KEY-"));
    }
}
