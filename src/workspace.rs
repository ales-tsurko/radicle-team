//! Ephemeral working copies supplied to agents.

use std::{fs, path::PathBuf};

use radicle::{
    cob::{patch::Patches, store::access::ReadOnly},
    crypto::PublicKey,
    git::Url,
    identity::RepoId,
    storage::{ReadRepository as _, ReadStorage as _, git::Storage},
};

use crate::{agent::WorkId, watcher::Subject};

#[derive(Debug, Clone)]
pub(crate) struct Workspaces {
    root: PathBuf,
    storage: Storage,
    repository: RepoId,
    remote: PublicKey,
}

impl Workspaces {
    pub(crate) fn new(
        root: PathBuf,
        storage: Storage,
        repository: RepoId,
        remote: PublicKey,
    ) -> Self {
        Self {
            root,
            storage,
            repository,
            remote,
        }
    }

    pub(crate) const fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Checks out the canonical head for `work`, or the patch head when it addresses one.
    pub(crate) async fn prepare(
        &self,
        work: WorkId,
        subject: Subject,
    ) -> Result<WorkingCopy, Error> {
        let manager = self.clone();
        tokio::task::spawn_blocking(move || manager.prepare_blocking(work, subject))
            .await
            .map_err(|error| Error::Task(error.to_string()))?
    }

    fn prepare_blocking(&self, work: WorkId, subject: Subject) -> Result<WorkingCopy, Error> {
        fs::create_dir_all(&self.root)?;
        let path = self.root.join(work.directory());
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        let storage_repository = self.storage.repository(self.repository)?;
        let (canonical, head) = storage_repository.canonical_head()?;
        let branch = canonical
            .to_namespaced()
            .map(|name| name.strip_namespace())
            .unwrap_or(canonical);
        let checkout_remote = storage_repository
            .delegates()?
            .iter()
            .map(|delegate| *delegate.as_key())
            .find(|delegate| storage_repository.reference_oid(delegate, &branch).is_ok())
            .ok_or(Error::CanonicalBranchRemote)?;
        let repository = radicle::rad::checkout(
            self.repository,
            &checkout_remote,
            &path,
            &self.storage,
            false,
        )?;
        let head = git2::Oid::from_str(&head.to_string())?;
        if repository.find_object(head, None).is_err() {
            let source = storage_repository.path().to_string_lossy();
            repository
                .remote_anonymous(&source)?
                .fetch(&[&head.to_string()], None, None)?;
        }
        repository.set_head_detached(head)?;
        repository.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
        let push = Url::from(self.repository).with_namespace(self.remote);
        repository.remote_set_pushurl(&radicle::rad::REMOTE_NAME, Some(&push.to_string()))?;
        if let Subject::Patch(id) = subject {
            let patch = Patches::open(&storage_repository, ReadOnly)?
                .get(&id)?
                .ok_or(Error::PatchNotFound(id))?;
            let head = git2::Oid::from_str(&patch.head().to_string())?;
            if repository.find_object(head, None).is_err() {
                let source = storage_repository.path().to_string_lossy();
                repository
                    .remote_anonymous(&source)?
                    .fetch(&[&head.to_string()], None, None)?;
            }
            repository.set_head_detached(head)?;
            repository.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
        }
        Ok(WorkingCopy { path })
    }
}

#[derive(Debug)]
pub(crate) struct WorkingCopy {
    path: PathBuf,
}

impl WorkingCopy {
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub(crate) async fn cleanup(self) -> Result<(), Error> {
        match tokio::fs::remove_dir_all(self.path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::Io(error)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("workspace I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("workspace checkout failed: {0}")]
    Checkout(#[from] radicle::rad::CheckoutError),
    #[error("workspace Git operation failed: {0}")]
    Git(#[from] git2::Error),
    #[error("workspace repository failed: {0}")]
    Repository(#[from] radicle::storage::RepositoryError),
    #[error("canonical branch is not available from a delegate namespace")]
    CanonicalBranchRemote,
    #[error("workspace collaborative object failed: {0}")]
    Cob(#[from] radicle::cob::store::Error),
    #[error("patch {0} was not found")]
    PatchNotFound(radicle::cob::ObjectId),
    #[error("workspace task failed: {0}")]
    Task(String),
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::{fs, path::Path, str::FromStr as _};

    use radicle::{
        cob::{EntryId, Title, patch::MergeTarget},
        crypto::Seed,
        identity::doc::Visibility,
        node::Alias,
        profile::{Home, Profile},
        storage::ReadStorage as _,
    };

    use crate::{agent::DeliveryId, watcher::Subject, workspace::Workspaces};

    #[tokio::test]
    async fn prepares_clean_issue_and_patch_workspaces() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let home = Home::new(root.path().join("home")).expect("home should be created");
        let profile = Profile::init(home, Alias::new("workspace-test"), None, Seed::generate())
            .expect("profile should initialize");
        let working = root.path().join("working");
        let checkout = git2::Repository::init(&working).expect("repository should initialize");
        let signature = git2::Signature::now("Workspace Test", "workspace@example.com")
            .expect("signature should be valid");
        fs::write(working.join("README.md"), "initial\n").expect("README should be written");
        let base = commit(&checkout, &signature, "Initial", &[]);
        fs::write(working.join("README.md"), "changed\n").expect("README should be changed");
        let parent = checkout.find_commit(base).expect("base should exist");
        let head = commit(&checkout, &signature, "Change", &[&parent]);
        drop(parent);
        let signer = profile.signer().expect("signer should load");
        let (repository, _document, _refs) = radicle::rad::init(
            &checkout,
            "workspace-test"
                .try_into()
                .expect("project name should be valid"),
            "Workspace test",
            "main".try_into().expect("branch should be valid"),
            Visibility::default(),
            &signer,
            &profile.storage,
        )
        .expect("Radicle repository should initialize");
        let patch = {
            let repository = profile
                .storage
                .repository(repository)
                .expect("stored repository should open");
            let mut patches = profile
                .patches_mut(&repository, &signer)
                .expect("patch cache should open");
            let patch = patches
                .create(
                    Title::try_from("Change".to_owned()).expect("title should be valid"),
                    "Description",
                    MergeTarget::default(),
                    base,
                    head,
                    &[],
                )
                .expect("patch should be created");
            *patch.id()
        };
        let member = radicle::crypto::PublicKey::from_str(
            "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7",
        )
        .expect("member should be valid");
        let workspaces = Workspaces::new(
            root.path().join("workspaces"),
            profile.storage.clone(),
            repository,
            member,
        );
        let delivery = DeliveryId {
            operation: EntryId::from_str("1111111111111111111111111111111111111111")
                .expect("operation should be valid"),
            action: 0,
        };

        let issue = workspaces
            .prepare_blocking(delivery.into(), Subject::Issue(patch))
            .expect("issue workspace should prepare");
        fs::write(issue.path().join("marker"), "stale").expect("marker should be written");
        let refreshed = workspaces
            .prepare_blocking(delivery.into(), Subject::Issue(patch))
            .expect("workspace should refresh");
        assert!(!refreshed.path().join("marker").exists());
        refreshed
            .cleanup()
            .await
            .expect("workspace should be removed");

        let patch_workspace = workspaces
            .prepare(
                DeliveryId {
                    operation: delivery.operation,
                    action: 1,
                }
                .into(),
                Subject::Patch(patch),
            )
            .await
            .expect("patch workspace should prepare");
        let patch_repository =
            git2::Repository::open(patch_workspace.path()).expect("patch workspace should open");
        assert_eq!(
            patch_repository.head().expect("HEAD should exist").target(),
            Some(head)
        );
        assert!(
            patch_repository
                .find_remote(&radicle::rad::REMOTE_NAME)
                .expect("rad remote should exist")
                .pushurl()
                .expect("push URL should exist")
                .contains(&member.to_string())
        );
        drop(patch_repository);
        fs::remove_dir_all(patch_workspace.path()).expect("agent should remove its workspace");
        patch_workspace
            .cleanup()
            .await
            .expect("an absent workspace should be considered clean");
    }

    fn commit(
        repository: &git2::Repository,
        signature: &git2::Signature<'_>,
        message: &str,
        parents: &[&git2::Commit<'_>],
    ) -> git2::Oid {
        let mut index = repository.index().expect("index should open");
        index
            .add_path(Path::new("README.md"))
            .expect("README should be added");
        let tree = index.write_tree().expect("tree should be written");
        let tree = repository.find_tree(tree).expect("tree should exist");
        repository
            .commit(
                Some("refs/heads/main"),
                signature,
                signature,
                message,
                &tree,
                parents,
            )
            .expect("commit should be created")
    }
}
