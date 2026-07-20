//! Repository checkout validation and manifest patch publication.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use git2::{
    ErrorCode, Oid, Repository, ResetType, StatusOptions, StatusShow, build::CheckoutBuilder,
};
use radicle::identity::RepoId;

use super::{Error, ManifestEntry};
use crate::{SetupStatus, manifest::FILE_NAME, member::Member};

#[derive(Debug, Clone)]
pub(super) struct RepositoryCheckout {
    pub(super) path: PathBuf,
}

impl RepositoryCheckout {
    pub(super) fn current(expected: RepoId) -> Result<Option<Self>, Error> {
        let path = std::env::current_dir()?;
        let output = Command::new("rad")
            .args(["inspect", "--rid", "."])
            .current_dir(&path)
            .output()?;
        if !output.status.success() {
            return Ok(None);
        }
        let actual = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<RepoId>()
            .map_err(Error::CheckoutIdentity)?;

        if actual != expected {
            return Ok(None);
        }
        let repository = Repository::discover(path)?;
        let path = repository
            .workdir()
            .ok_or(Error::BareCheckout)?
            .to_path_buf();

        Ok(Some(Self { path }))
    }

    pub(super) fn commands(&self, member: Member) -> Result<String, Error> {
        self.ensure_canonical_head()?;
        Ok(format!(
            "git add {FILE_NAME}\ngit commit -m \"Bind {member} to radicle-team\"\ngit push rad \
             HEAD:refs/patches"
        ))
    }

    pub(super) fn publish(&self, entry: ManifestEntry, status: SetupStatus) -> Result<bool, Error> {
        self.validate_manifest_change()?;
        let base = self.revision("HEAD")?;
        let manifest_existed = self.path.join(FILE_NAME).try_exists()?;
        let committed = match self.commit_manifest(entry, status) {
            Ok(committed) => committed,
            Err(operation) => {
                if let Err(recovery) = self.restore_checkout(&base, &base, manifest_existed) {
                    return Err(Error::PatchRecovery {
                        operation: Box::new(operation),
                        recovery: Box::new(recovery),
                    });
                }
                return Err(operation);
            }
        };
        if committed {
            let generated = self.revision("HEAD")?;
            if let Err(push) = self.push_patch() {
                if let Err(recovery) = self.restore_checkout(&base, &generated, manifest_existed) {
                    return Err(Error::PatchRecovery {
                        operation: Box::new(push),
                        recovery: Box::new(recovery),
                    });
                }
                return Err(push);
            }
        }

        Ok(committed)
    }

    fn restore_checkout(
        &self,
        base: &str,
        generated: &str,
        manifest_existed: bool,
    ) -> Result<(), Error> {
        if self.revision("HEAD")? != generated {
            return Err(Error::CheckoutChangedDuringPublish);
        }
        let repository = self.repository()?;
        let base = repository.revparse_single(base)?;
        repository.reset(&base, ResetType::Mixed, None)?;
        if manifest_existed {
            let mut checkout = CheckoutBuilder::new();
            checkout.force().path(FILE_NAME);
            repository.checkout_tree(&base, Some(&mut checkout))?;
        } else {
            match fs::remove_file(self.path.join(FILE_NAME)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }

        Ok(())
    }

    pub(super) fn validate_manifest_change(&self) -> Result<(), Error> {
        self.ensure_canonical_head()?;
        self.ensure_manifest_not_ignored()?;
        self.ensure_clean_manifest()?;
        self.ensure_clean_index()?;

        Ok(())
    }

    fn ensure_manifest_not_ignored(&self) -> Result<(), Error> {
        let repository = self.repository()?;
        let index = repository.index()?;
        let tracked = index.get_path(Path::new(FILE_NAME), 0).is_some();
        if !tracked && repository.status_should_ignore(Path::new(FILE_NAME))? {
            Err(Error::IgnoredManifest)
        } else {
            Ok(())
        }
    }

    pub(super) fn commit_manifest(
        &self,
        entry: ManifestEntry,
        status: SetupStatus,
    ) -> Result<bool, Error> {
        if !entry.prepare(&self.path.join(FILE_NAME), status)? {
            return Ok(false);
        }
        let repository = self.repository()?;
        let mut index = repository.index()?;
        index.add_path(Path::new(FILE_NAME))?;
        index.write()?;
        self.git_cli([
            "commit",
            "-m",
            &format!("Bind {} to radicle-team", entry.member),
        ])?;

        Ok(true)
    }

    fn push_patch(&self) -> Result<(), Error> {
        self.git_cli(["push", "rad", "HEAD:refs/patches"])
    }

    pub(super) fn ensure_canonical_head(&self) -> Result<(), Error> {
        let upstream = self.upstream()?;
        Self::validate_upstream(upstream.as_deref())?;
        let canonical = self.canonical_head()?;
        self.ensure_head_matches(canonical)
    }

    fn upstream(&self) -> Result<Option<String>, Error> {
        let repository = self.repository()?;
        let head = repository.head()?;
        if !head.is_branch() {
            return Ok(None);
        }
        let Some(name) = head.name() else {
            return Ok(None);
        };
        match repository.branch_upstream_name(name) {
            Ok(upstream) => Ok(upstream
                .as_str()
                .and_then(|name| name.strip_prefix("refs/remotes/"))
                .map(str::to_owned)),
            Err(error) if error.code() == ErrorCode::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn validate_upstream(upstream: Option<&str>) -> Result<(), Error> {
        match upstream {
            Some(upstream) if upstream.starts_with("rad/patches/") => Err(Error::ExistingPatch),
            _ => Ok(()),
        }
    }

    fn canonical_head(&self) -> Result<Oid, Error> {
        let output = Command::new("git")
            .args(["ls-remote", "--symref", "rad", "HEAD"])
            .current_dir(&self.path)
            .output()?;
        if !output.status.success() {
            return Err(Error::Command {
                command: "git ls-remote --symref rad HEAD".to_owned(),
                status: output.status,
            });
        }

        Self::parse_canonical_head(&String::from_utf8_lossy(&output.stdout))
            .ok_or(Error::NonCanonicalCheckout)
    }

    fn parse_canonical_head(output: &str) -> Option<Oid> {
        output.lines().find_map(|line| {
            line.strip_suffix("\tHEAD")
                .filter(|target| !target.starts_with("ref: "))
                .and_then(|target| target.parse().ok())
        })
    }

    fn ensure_head_matches(&self, canonical: Oid) -> Result<(), Error> {
        let head = self.repository()?.revparse_single("HEAD")?.id();
        if head == canonical {
            Ok(())
        } else {
            Err(Error::NonCanonicalCheckout)
        }
    }

    pub(super) fn revision(&self, revision: &str) -> Result<String, Error> {
        Ok(self
            .repository()?
            .revparse_single(revision)?
            .id()
            .to_string())
    }

    pub(super) fn ensure_clean_manifest(&self) -> Result<(), Error> {
        let repository = self.repository()?;
        let mut options = StatusOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .disable_pathspec_match(true)
            .pathspec(FILE_NAME);
        if repository.statuses(Some(&mut options))?.is_empty() {
            Ok(())
        } else {
            Err(Error::DirtyManifest)
        }
    }

    pub(super) fn ensure_clean_index(&self) -> Result<(), Error> {
        let repository = self.repository()?;
        let mut options = StatusOptions::new();
        options.show(StatusShow::Index);
        if repository.statuses(Some(&mut options))?.is_empty() {
            Ok(())
        } else {
            Err(Error::StagedChanges)
        }
    }

    fn repository(&self) -> Result<Repository, git2::Error> {
        Repository::open(&self.path)
    }

    fn git_cli<const N: usize>(&self, arguments: [&str; N]) -> Result<(), Error> {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(&self.path)
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Command {
                command: format!("git {}", arguments.join(" ")),
                status,
            })
        }
    }
}
