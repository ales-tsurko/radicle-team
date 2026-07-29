//! Private security-report repository provisioning.

use std::{process::Command, str::FromStr as _};

use age::{secrecy::ExposeSecret as _, x25519};
use radicle::identity::{Did, RepoId};

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
        let repository = Self::create_repository()?;
        Self::grant(repository, did)?;
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
        Self::command(
            Command::new("rad")
                .args(["id", "update", "--repo"])
                .arg(repository.to_string())
                .args(["--allow"])
                .arg(did.to_string())
                .arg("--no-confirm"),
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
