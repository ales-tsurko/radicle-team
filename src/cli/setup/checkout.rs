//! Manifest patch publication through the member's coder worktree.

use std::{fs, path::PathBuf};

use git2::{Repository, ResetType, StatusOptions};
use radicle::{
    cob::{
        Title,
        patch::{MergeTarget, PatchId, Patches},
        store::access::ReadOnly,
    },
    git::Url,
    identity::RepoId,
    node::device::Device,
    storage::{ReadRepository as _, ReadStorage as _, SignRepository as _, WriteRepository as _},
};

use crate::{
    Application,
    agent::Role,
    cli::setup::{Error, ManifestUpdate},
    identity::{Identity, Manager, MemberLock},
    manifest::{FILE_NAME, Manifest},
    member::Member,
    queue::WorkQueue,
    workspace::Workspaces,
};

#[derive(Debug)]
pub(crate) struct RepositoryCheckout {
    path: PathBuf,
    member: Member,
    repository: RepoId,
    identity: Identity,
    _lock: MemberLock,
}

impl RepositoryCheckout {
    pub(super) async fn open(application: &Application) -> Result<Self, Error> {
        let application = application.clone();
        let (checkout, home) = tokio::task::spawn_blocking(move || {
            let home = application.member_home().map_err(crate::Error::from)?;
            let lock = MemberLock::acquire(&home).map_err(crate::Error::from)?;
            application.validate_member_home(&home)?;
            let identity = Manager::default()
                .load(application.member(), &home, application.identity_provider())
                .map_err(crate::Error::from)?;
            let checkout = Self {
                path: home.worktree(Role::Coder),
                member: application.member(),
                repository: application.repository(),
                identity,
                _lock: lock,
            };
            Ok::<_, Error>((checkout, home))
        })
        .await??;
        let queue = WorkQueue::open(&home.state_database(), checkout.repository, checkout.member)
            .await
            .map_err(crate::Error::from)?;
        let profile = checkout.identity.profile();
        Workspaces::new(
            checkout.member,
            home,
            profile.storage.clone(),
            checkout.repository,
            profile.info(),
            queue,
        )
        .create()
        .await?;
        tokio::task::spawn_blocking(move || {
            checkout.initialize()?;
            Ok(checkout)
        })
        .await?
    }

    fn initialize(&self) -> Result<(), Error> {
        let storage = &self.identity.profile().storage;
        let stored = storage.repository(self.repository)?;
        let (_, canonical) = stored.canonical_head()?;
        let repository = Repository::open(&self.path)?;
        self.validate_checkout(&repository)?;
        repository
            .remote_anonymous(
                stored
                    .path()
                    .to_str()
                    .ok_or_else(|| Error::NonUtf8Path(stored.path().to_path_buf()))?,
            )?
            .fetch(&[&canonical.to_string()], None, None)?;
        let proposal = self.proposal()?;
        let base = match &proposal {
            Some(proposal) => proposal.base,
            None => repository.head()?.peel_to_commit()?.id(),
        };
        if base != canonical.into() && !repository.graph_descendant_of(canonical.into(), base)? {
            return Err(Error::NonCanonicalCheckout(self.path.clone()));
        }
        if proposal.is_none() {
            self.ensure_clean(&repository)?;
            let commit = repository.find_commit(canonical.into())?;
            repository.set_head_detached(commit.id())?;
            repository.reset(commit.as_object(), ResetType::Hard, None)?;
        } else {
            // Setup owns the member lock; the coder copy stays here until publication finishes.
            // Detaching also keeps retries from moving any branch previously checked out here.
            let head = repository.head()?.peel_to_commit()?.id();
            repository.set_head_detached(head)?;
        }
        Ok(())
    }

    fn validate_checkout(&self, repository: &Repository) -> Result<(), Error> {
        if !repository.workdir().is_some_and(|path| path == self.path) {
            return Err(Error::CheckoutLocation);
        }
        let common = self
            .path
            .parent()
            .ok_or(Error::CheckoutLocation)?
            .join(".git-common");
        if !repository.is_worktree() || fs::canonicalize(repository.commondir())? != common {
            return Err(Error::CheckoutStorage);
        }
        if repository.find_remote("rad")?.url()
            != Some(Url::from(self.repository).to_string().as_str())
        {
            return Err(Error::CheckoutRepository);
        }
        Ok(())
    }

    fn ensure_clean(&self, repository: &Repository) -> Result<(), Error> {
        let mut options = StatusOptions::new();
        options.include_untracked(true).recurse_untracked_dirs(true);
        if !repository.statuses(Some(&mut options))?.is_empty() {
            return Err(Error::DirtyCheckout(self.path.clone()));
        }
        Ok(())
    }

    pub(super) fn prepare(&self, update: ManifestUpdate) -> Result<(), Error> {
        let repository = Repository::open(&self.path)?;
        self.ensure_clean(&repository)?;
        let base = repository.head()?.peel_to_commit()?.id();
        let stored = self
            .identity
            .profile()
            .storage
            .repository(self.repository)?;
        if radicle::git::Oid::from(base) != stored.canonical_head()?.1 {
            return Err(Error::CanonicalChanged);
        }
        update.prepare(&self.path.join(FILE_NAME))?;
        Ok(())
    }

    pub(super) fn preview(&self, generation: u64) -> Result<Option<(String, bool)>, Error> {
        let Some(proposal) = self.validated_proposal(generation)? else {
            return Ok(None);
        };
        let published = match proposal.head {
            Some(head) => self.find_patch(proposal.base, head)?.is_some(),
            None => false,
        };
        Ok(Some((
            format!("{}\n\n{}", self.path.display(), proposal.manifest),
            published,
        )))
    }

    fn validated_proposal(&self, generation: u64) -> Result<Option<Proposal>, Error> {
        let Some(proposal) = self.proposal()? else {
            return Ok(None);
        };
        let binding =
            Manifest::validate_candidate(&proposal.manifest, self.member, self.identity.did())?;
        if binding.generation() != generation {
            return Err(Error::BindingGeneration {
                expected: generation,
                actual: binding.generation(),
            });
        }
        Ok(Some(proposal))
    }

    fn manifest(&self) -> Result<Option<String>, Error> {
        let path = self.path.join(FILE_NAME);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    return Err(Error::IrregularManifest(path));
                }
                Ok(Some(fs::read_to_string(path)?))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn proposal(&self) -> Result<Option<Proposal>, Error> {
        let repository = Repository::open(&self.path)?;
        let head = repository.head()?.peel_to_commit()?;
        let manifest = self.manifest()?;
        let original = head
            .tree()?
            .get_name(FILE_NAME)
            .map(|entry| {
                repository
                    .find_blob(entry.id())
                    .map(|blob| blob.content().to_vec())
            })
            .transpose()?;
        if let Some(base) = self.committed_base(&repository, &head)? {
            if manifest.as_deref().map(str::as_bytes) != original.as_deref() {
                return Err(Error::ChangedManifest);
            }
            return Ok(Some(Proposal {
                base,
                head: Some(head.id()),
                manifest: manifest.ok_or(Error::MissingManifest)?,
            }));
        }
        if manifest.as_deref().map(str::as_bytes) == original.as_deref() {
            return Ok(None);
        }
        Ok(Some(Proposal {
            base: head.id(),
            head: None,
            manifest: manifest.ok_or(Error::RemovedManifest)?,
        }))
    }

    // Recover an interrupted publication from HEAD: one manifest commit above canonical history.
    fn committed_base(
        &self,
        repository: &Repository,
        head: &git2::Commit<'_>,
    ) -> Result<Option<git2::Oid>, Error> {
        let stored = self
            .identity
            .profile()
            .storage
            .repository(self.repository)?;
        let (_, canonical) = stored.canonical_head()?;
        if head.id() == canonical.into()
            || repository.graph_descendant_of(canonical.into(), head.id())?
        {
            return Ok(None);
        }
        if head.parent_count() != 1 {
            return Err(Error::InvalidProposalHistory);
        }
        let parent = head.parent(0)?;
        let diff =
            repository.diff_tree_to_tree(Some(&parent.tree()?), Some(&head.tree()?), None)?;
        if diff.deltas().len() != 1
            || !diff.deltas().all(|delta| {
                delta.new_file().path() == Some(std::path::Path::new(FILE_NAME))
                    && matches!(delta.status(), git2::Delta::Added | git2::Delta::Modified)
            })
        {
            return Err(Error::NonManifestChanges);
        }
        Ok(Some(parent.id()))
    }

    fn find_patch(&self, base: git2::Oid, head: git2::Oid) -> Result<Option<PatchId>, Error> {
        let stored = self
            .identity
            .profile()
            .storage
            .repository(self.repository)?;
        let patches = Patches::open(&stored, ReadOnly)?;
        for patch in patches.all()? {
            let (id, patch) = patch?;
            if patch.author().id() == &self.identity.did()
                && patch
                    .revisions()
                    .any(|(_, revision)| revision.head() == head && *revision.base() == base)
            {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    pub(super) fn publish(&self, generation: u64) -> Result<PatchId, Error> {
        let proposal = self
            .validated_proposal(generation)?
            .ok_or(Error::MissingProposal)?;
        let repository = Repository::open(&self.path)?;
        let head = proposal.commit(&repository, self.member)?;
        let stored = self
            .identity
            .profile()
            .storage
            .repository(self.repository)?;
        stored
            .raw()
            .remote_anonymous(
                self.path
                    .to_str()
                    .ok_or_else(|| Error::NonUtf8Path(self.path.clone()))?,
            )?
            .fetch(&[&head.to_string()], None, None)?;
        let signer = match self.identity.memory_signer() {
            Some(signer) => Device::from(signer).boxed(),
            None => self.identity.profile().signer()?,
        };
        let id = match self.find_patch(proposal.base, head)? {
            Some(id) => id,
            None => {
                let mut patches = self.identity.profile().patches_mut(&stored, &signer)?;
                let patch = patches.create(
                    Title::try_from(format!("Bind {} to radicle-team", self.member))?,
                    "Provision this member's identity and configuration. Activation requires \
                     merging this manifest into the canonical branch.",
                    MergeTarget::Delegates,
                    proposal.base,
                    head,
                    &[],
                )?;
                *patch.id()
            }
        };
        stored.sign_refs(&signer)?;
        Ok(id)
    }
}

/// Derived from the working manifest and HEAD; never persisted separately.
#[derive(Debug)]
struct Proposal {
    base: git2::Oid,
    head: Option<git2::Oid>,
    manifest: String,
}

impl Proposal {
    fn commit(&self, repository: &Repository, member: Member) -> Result<git2::Oid, Error> {
        let parent = repository.find_commit(self.base)?;
        let blob = repository.blob(self.manifest.as_bytes())?;
        let mut tree = repository.treebuilder(Some(&parent.tree()?))?;
        tree.insert(FILE_NAME, blob, 0o100644)?;
        let tree = repository.find_tree(tree.write()?)?;
        let mut index = repository.index()?;
        let index_tree = index.write_tree()?;
        if index_tree != parent.tree_id() && index_tree != tree.id() {
            return Err(Error::StagedChanges);
        }
        let head = match self.head {
            Some(head) => head,
            None => {
                let signature = repository.signature()?;
                repository.commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    &format!("Bind {member} to radicle-team"),
                    &tree,
                    &[&parent],
                )?
            }
        };
        // Repair an index write interrupted after the commit, without generating another commit.
        index.read_tree(&tree)?;
        index.write()?;
        Ok(head)
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, fs};

    use radicle::{
        crypto::Seed,
        identity::Visibility,
        node::Alias,
        profile::{Home, Profile},
        storage::{
            ReadRepository as _, ReadStorage as _, SignRepository as _, WriteRepository as _,
        },
    };

    use super::RepositoryCheckout;
    use crate::{
        Application,
        agent::{Role, Source},
        cli::setup::{Error, ManifestEntry, ManifestUpdate, SecurityManifest},
        home::MemberHome,
        identity::{KeyProtection, Manager, Provider},
        manifest::{FILE_NAME, Manifest, QueuePosition, TriggerAccess},
        member::Member,
        queue::WorkQueue,
        watcher::Subject,
        workspace::Workspaces,
    };

    struct Fixture {
        _root: tempfile::TempDir,
        source: std::path::PathBuf,
        application: Application,
        operator: Profile,
        base: radicle::git::Oid,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("fixture directory should exist");
            let source = root.path().join("operator-checkout");
            let git = git2::Repository::init(&source).expect("source should initialize");
            git.set_head("refs/heads/main")
                .expect("main should be selected");
            fs::write(source.join("README.md"), "# Operator repository\n")
                .expect("README should exist");
            let mut index = git.index().expect("index should open");
            index
                .add_path(std::path::Path::new("README.md"))
                .expect("README should stage");
            let tree = git
                .find_tree(index.write_tree().expect("tree should write"))
                .expect("tree should open");
            let signature = git2::Signature::now("Operator", "operator@example.com")
                .expect("signature should create");
            let base = git
                .commit(Some("HEAD"), &signature, &signature, "Initial", &tree, &[])
                .expect("initial commit should exist")
                .into();
            let operator = Profile::init(
                Home::new(root.path().join("operator-identity"))
                    .expect("operator home should create"),
                Alias::new("operator"),
                None,
                Seed::new([41; 32]),
            )
            .expect("operator identity should create");
            let signer = operator.signer().expect("operator signer should load");
            let (repository, _, _) = radicle::rad::init(
                &git,
                "setup-test".try_into().expect("name should parse"),
                "Setup test",
                "main".try_into().expect("branch should parse"),
                Visibility::default(),
                &signer,
                &operator.storage,
            )
            .expect("Radicle repository should create");
            let home = MemberHome::at(root.path().join("maya"));
            home.create().expect("member home should create");
            home.record(Member::Maya, repository)
                .expect("member should bind to repository");
            let provider = Provider::Managed {
                protection: KeyProtection::Unencrypted,
            };
            let identity = Manager::default()
                .load(Member::Maya, &home, &provider)
                .expect("member identity should create");
            // Replicate the operator's repository into isolated member storage without a network.
            let stored = operator
                .storage
                .repository(repository)
                .expect("operator storage should open");
            let target = identity.profile().storage.path().join(
                stored
                    .path()
                    .file_name()
                    .expect("repository directory should have a name"),
            );
            let replica = git2::Repository::init_bare(target).expect("replica should initialize");
            replica
                .remote_anonymous(stored.path().to_str().expect("path should be UTF-8"))
                .expect("remote should create")
                .fetch(&["+refs/*:refs/*"], None, None)
                .expect("repository should replicate");
            let application = Application::new(Member::Maya, repository)
                .with_home(home.path())
                .with_identity_provider(provider);
            Self {
                _root: root,
                source,
                application,
                operator,
                base,
            }
        }

        fn proposal(&self, checkout: &RepositoryCheckout) -> ManifestUpdate {
            ManifestUpdate::create(
                ManifestEntry {
                    member: Member::Maya,
                    did: checkout.identity.did(),
                    generation: 1,
                    provider: Some("gpt".to_owned()),
                },
                SecurityManifest {
                    repository: self.application.repository(),
                    recipient: "age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa"
                        .to_owned(),
                },
                TriggerAccess::Delegates,
            )
        }
    }

    #[tokio::test]
    async fn publishes_as_the_member_without_touching_the_operator_checkout_or_canonical_branch() {
        let fixture = Fixture::new();
        // Dirty operator files are irrelevant; setup operates exclusively on the member's copy.
        fs::write(fixture.source.join(FILE_NAME), "operator notes")
            .expect("operator notes should write");
        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("member checkout should open");
        assert!(
            checkout.path.starts_with(
                fs::canonicalize(fixture.application.home().expect("home should exist"))
                    .expect("member home should resolve")
            )
        );
        let coder = git2::Repository::open(&checkout.path).expect("coder should open");
        let branches = || {
            coder
                .references_glob("refs/heads/*")
                .expect("branches should read")
                .map(|reference| {
                    reference
                        .expect("branch should read")
                        .name()
                        .expect("branch name should be UTF-8")
                        .to_owned()
                })
                .collect::<std::collections::BTreeSet<_>>()
        };
        let original_branches = branches();
        checkout
            .prepare(fixture.proposal(&checkout))
            .expect("proposal should prepare");
        let home = fixture
            .application
            .member_home()
            .expect("home should resolve");
        assert_eq!(checkout.path, home.worktree(Role::Coder));
        let directories = fs::read_dir(home.code())
            .expect("code directory should read")
            .map(|entry| entry.expect("entry should read").file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            directories,
            [".git-common", "talker", "chatter", "coder"]
                .map(std::ffi::OsString::from)
                .into()
        );
        for role in Role::ALL {
            let git =
                git2::Repository::open(home.worktree(role)).expect("role worktree should open");
            assert!(git.is_worktree());
            assert_eq!(
                fs::canonicalize(git.commondir()).expect("store should resolve"),
                home.git_common()
            );
        }
        assert!(coder.head_detached().expect("HEAD state should read"));
        assert_eq!(
            coder.head().expect("HEAD should read").target(),
            Some(fixture.base.into()),
            "saving a preview must not make a commit before confirmation"
        );
        let id = checkout
            .publish(1)
            .expect("patch should publish without a merge");
        assert_eq!(
            branches(),
            original_branches,
            "setup must not create or move a branch"
        );
        let stored = checkout
            .identity
            .profile()
            .storage
            .repository(fixture.application.repository())
            .expect("member storage should open");
        assert_eq!(
            stored
                .raw()
                .references_glob(&format!(
                    "refs/namespaces/{}/refs/heads/*",
                    checkout.identity.profile().id()
                ))
                .expect("member branches should read")
                .count(),
            0,
            "patch publication must not create a seed-side setup branch"
        );
        let patches =
            radicle::cob::patch::Patches::open(&stored, radicle::cob::store::access::ReadOnly)
                .expect("patches should open");
        let patch = patches
            .get(&id)
            .expect("patch should read")
            .expect("patch should exist");
        assert_eq!(patch.author().id(), &checkout.identity.did());
        assert_ne!(patch.author().id(), &fixture.operator.did());
        assert_eq!(*patch.latest().1.base(), fixture.base);
        assert_eq!(
            stored
                .canonical_head()
                .expect("canonical head should read")
                .1,
            fixture.base
        );
        assert!(
            Manifest::load_optional_repository(&stored)
                .expect("canonical manifest should read")
                .is_none()
        );
        let head = stored
            .raw()
            .find_commit(patch.latest().1.head().into())
            .expect("patch commit should read");
        let parent = stored
            .raw()
            .find_commit(fixture.base.into())
            .expect("base should read");
        let diff = stored
            .raw()
            .diff_tree_to_tree(
                Some(&parent.tree().expect("base tree should read")),
                Some(&head.tree().expect("patch tree should read")),
                None,
            )
            .expect("diff should read");
        assert_eq!(diff.deltas().len(), 1);
        assert_eq!(
            diff.deltas()
                .next()
                .expect("manifest delta should exist")
                .new_file()
                .path(),
            Some(std::path::Path::new(FILE_NAME))
        );
        assert_eq!(
            fs::read_to_string(fixture.source.join(FILE_NAME))
                .expect("operator notes should remain"),
            "operator notes"
        );
        assert_eq!(
            git2::Repository::open(&fixture.source)
                .expect("operator checkout should open")
                .head()
                .expect("operator HEAD should read")
                .peel_to_commit()
                .expect("HEAD should be a commit")
                .id(),
            fixture.base.into()
        );
    }

    #[tokio::test]
    async fn resumes_the_draft_commit_and_patch_without_separate_state() {
        let fixture = Fixture::new();
        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("checkout should open");
        let home = fixture
            .application
            .home()
            .expect("member home should exist");
        let files = || {
            fs::read_dir(home)
                .expect("home should read")
                .map(|entry| entry.expect("entry should read").file_name())
                .collect::<std::collections::BTreeSet<_>>()
        };
        let before = files();
        checkout
            .prepare(fixture.proposal(&checkout))
            .expect("draft should prepare");
        assert_eq!(
            files(),
            before,
            "preparing a manifest must not create another state file"
        );
        let manifest = fs::read_to_string(checkout.path.join(FILE_NAME))
            .expect("normal manifest should exist before publication");
        Manifest::validate_candidate(&manifest, Member::Maya, checkout.identity.did())
            .expect("working manifest should be valid");
        let preview = checkout.preview(1).expect("proposal should preview");
        assert!(
            !preview.as_ref().expect("proposal should exist").1,
            "an uncommitted draft has no patch yet"
        );
        drop(checkout);

        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("draft should survive restart");
        assert_eq!(
            checkout.preview(1).expect("proposal should preview"),
            preview
        );
        // Stop at the actual Git commit boundary, before updating the index or creating a COB.
        let git = git2::Repository::open(&checkout.path).expect("checkout should open");
        let parent = git
            .head()
            .expect("HEAD should exist")
            .peel_to_commit()
            .expect("base should read");
        let mut builder = git
            .treebuilder(Some(&parent.tree().expect("base tree should read")))
            .expect("tree builder should open");
        let blob = git
            .blob(manifest.as_bytes())
            .expect("manifest blob should write");
        builder
            .insert(FILE_NAME, blob, 0o100644)
            .expect("manifest should enter tree");
        let tree = git
            .find_tree(builder.write().expect("tree should write"))
            .expect("tree should read");
        let signature = git.signature().expect("member signature should load");
        let head = git
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Configure Maya",
                &tree,
                &[&parent],
            )
            .expect("interrupted setup commit should create");
        drop(checkout);

        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("committed draft should survive restart");
        let id = checkout.publish(1).expect("committed draft should publish");
        assert_eq!(
            git.head()
                .expect("HEAD should exist")
                .peel_to_commit()
                .expect("commit should read")
                .id(),
            head,
            "resuming must reuse the existing commit"
        );
        checkout
            .ensure_clean(&git)
            .expect("interrupted index should be repaired");
        drop(checkout);

        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("published checkout should reopen");
        assert!(
            checkout
                .preview(1)
                .expect("published patch should be found")
                .expect("proposal should exist")
                .1
        );
        assert_eq!(
            checkout.publish(1).expect("retry should reuse the patch"),
            id
        );
        assert_eq!(
            files(),
            before,
            "publication must not create a separate state file"
        );
        let stored = checkout
            .identity
            .profile()
            .storage
            .repository(fixture.application.repository())
            .expect("storage should open");
        assert_eq!(
            radicle::cob::patch::Patches::open(&stored, radicle::cob::store::access::ReadOnly)
                .expect("patches should open")
                .all()
                .expect("patches should list")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn the_published_patch_preserves_the_commit_after_coder_resets() {
        let fixture = Fixture::new();
        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("checkout should open");
        checkout
            .prepare(fixture.proposal(&checkout))
            .expect("manifest should prepare");
        let patch = checkout.publish(1).expect("patch should publish");
        let home = fixture
            .application
            .member_home()
            .expect("home should resolve");
        let queue = WorkQueue::open(
            &home.state_database(),
            fixture.application.repository(),
            Member::Maya,
        )
        .await
        .expect("queue should open");
        let did = checkout.identity.did();
        let profile = checkout.identity.profile();
        let stored = profile
            .storage
            .repository(fixture.application.repository())
            .expect("member storage should open");
        let workspaces = Workspaces::new(
            Member::Maya,
            home.clone(),
            profile.storage.clone(),
            fixture.application.repository(),
            profile.info(),
            queue.clone(),
        );
        drop(checkout);

        workspaces
            .initialize()
            .await
            .expect("runtime should initialize");
        queue
            .push(
                "Update README".to_owned(),
                Source::Subject(Subject::Issue(fixture.base.into())),
                QueuePosition::End,
            )
            .await
            .expect("job should enqueue");
        let job = queue.next().await.expect("job should start");
        workspaces
            .prepare_job(&job)
            .await
            .expect("coder should move to its job branch");
        assert!(!home.worktree(Role::Coder).join(FILE_NAME).exists());
        queue.finish().await.expect("job should finish");
        workspaces
            .initialize()
            .await
            .expect("finished job branch should be cleaned up");

        let published =
            radicle::cob::patch::Patches::open(&stored, radicle::cob::store::access::ReadOnly)
                .expect("patches should open")
                .get(&patch)
                .expect("patch should read")
                .expect("published patch must remain");
        let commit = stored
            .raw()
            .find_commit(published.head().into())
            .expect("the patch must retain its commit without a setup branch");
        let tree = commit.tree().expect("patch tree should read");
        let entry = tree
            .get_name(FILE_NAME)
            .expect("manifest must remain in the patch");
        let blob = stored
            .raw()
            .find_blob(entry.id())
            .expect("manifest should read");
        let manifest = std::str::from_utf8(blob.content()).expect("manifest should be UTF-8");
        Manifest::validate_candidate(manifest, Member::Maya, did)
            .expect("patch manifest must remain valid");
    }

    #[tokio::test]
    async fn refuses_a_changed_setup_checkout_and_another_member_home() {
        let fixture = Fixture::new();
        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("checkout should open");
        fs::write(checkout.path.join("notes.txt"), "local work").expect("notes should write");
        let error = checkout
            .prepare(fixture.proposal(&checkout))
            .expect_err("dirty setup checkout should be refused");
        assert!(matches!(error, Error::DirtyCheckout(path) if path == checkout.path));
        assert_eq!(
            fs::read_to_string(checkout.path.join("notes.txt")).expect("notes should remain"),
            "local work"
        );
        drop(checkout);
        let wrong = Application::new(Member::Nico, fixture.application.repository())
            .with_home(fixture.application.home().expect("home should exist"));
        let error = RepositoryCheckout::open(&wrong)
            .await
            .expect_err("another member's home should be refused");
        assert!(matches!(
            error,
            Error::Application(error) if matches!(error.as_ref(), crate::Error::MemberDirectory { .. })
        ));
    }

    #[tokio::test]
    async fn rejects_stale_generation_and_unrelated_staged_changes_without_committing() {
        let fixture = Fixture::new();
        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("checkout should open");
        let error = checkout
            .publish(1)
            .expect_err("publication without a proposal should be refused");
        assert!(matches!(error, Error::MissingProposal));
        checkout
            .prepare(fixture.proposal(&checkout))
            .expect("manifest should prepare");
        let error = checkout
            .preview(2)
            .expect_err("a stale binding generation should be refused");
        assert!(matches!(
            error,
            Error::BindingGeneration {
                expected: 2,
                actual: 1
            }
        ));
        let repository = git2::Repository::open(&checkout.path).expect("checkout should open");
        let head = repository.head().expect("HEAD should read").target();
        fs::write(checkout.path.join("notes.txt"), "local work").expect("notes should write");
        let mut index = repository.index().expect("index should open");
        index
            .add_path(std::path::Path::new("notes.txt"))
            .expect("notes should stage");
        index.write().expect("index should write");
        let error = checkout
            .publish(1)
            .expect_err("unrelated staged work should be refused");
        assert!(matches!(error, Error::StagedChanges));
        assert_eq!(repository.head().expect("HEAD should read").target(), head);
        assert_eq!(
            fs::read_to_string(checkout.path.join("notes.txt")).expect("notes should remain"),
            "local work"
        );
    }

    #[tokio::test]
    async fn preserves_the_git_error_source_when_the_radicle_remote_is_missing() {
        let fixture = Fixture::new();
        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("checkout should open");
        git2::Repository::open(&checkout.path)
            .expect("checkout should open")
            .remote_delete("rad")
            .expect("remote should delete");
        let error = checkout
            .initialize()
            .expect_err("a missing Radicle remote should be refused");
        assert!(matches!(&error, Error::Git(source) if source.code() == git2::ErrorCode::NotFound));
        assert!(
            error
                .source()
                .expect("Git failure should retain its source")
                .downcast_ref::<git2::Error>()
                .is_some()
        );
    }

    #[tokio::test]
    async fn extends_existing_policy_and_preserves_proposal_when_canonical_advances() {
        let fixture = Fixture::new();
        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("checkout should open");
        let did = checkout.identity.did();
        let stored = checkout
            .identity
            .profile()
            .storage
            .repository(fixture.application.repository())
            .expect("storage should open");
        let policy = format!(
            r#"version = 1
# Preserve this repository policy.
triggered_by = "delegates"
[security]
repository = "{}"
recipients = ["age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa"]
[members.nico]
signing_did = "{}"
generation = 1
state = "active"
mcp_allow = ["read"]
"#,
            fixture.application.repository(),
            fixture.operator.did()
        );
        let parent = stored
            .raw()
            .find_commit(fixture.base.into())
            .expect("base should read");
        let mut builder = stored
            .raw()
            .treebuilder(Some(&parent.tree().expect("base tree should read")))
            .expect("tree builder should open");
        let blob = stored
            .raw()
            .blob(policy.as_bytes())
            .expect("policy blob should write");
        builder
            .insert(FILE_NAME, blob, 0o100644)
            .expect("policy should enter tree");
        let tree = stored
            .raw()
            .find_tree(builder.write().expect("tree should write"))
            .expect("tree should read");
        let signature = git2::Signature::now("Operator", "operator@example.com")
            .expect("signature should create");
        let reference = format!("refs/namespaces/{}/refs/heads/main", fixture.operator.id());
        let canonical = stored
            .raw()
            .commit(
                Some(&reference),
                &signature,
                &signature,
                "Add existing member",
                &tree,
                &[&parent],
            )
            .expect("canonical commit should create");
        stored
            .sign_refs(
                &fixture
                    .operator
                    .signer()
                    .expect("operator signer should load"),
            )
            .expect("delegate references should sign");
        drop(checkout);

        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("checkout should refresh from canonical storage");
        checkout
            .prepare(ManifestUpdate::extend(ManifestEntry {
                member: Member::Maya,
                did,
                generation: 1,
                provider: Some("gpt".to_owned()),
            }))
            .expect("existing manifest should extend");
        let draft = fs::read_to_string(checkout.path.join(FILE_NAME))
            .expect("draft should be saved in the checkout");
        let parent = stored
            .raw()
            .find_commit(canonical)
            .expect("canonical commit should read");
        let readme = stored
            .raw()
            .blob(b"# Updated operator repository\n")
            .expect("README blob should write");
        builder
            .insert("README.md", readme, 0o100644)
            .expect("README should enter tree");
        let tree = stored
            .raw()
            .find_tree(builder.write().expect("tree should write"))
            .expect("tree should read");
        let advanced = stored
            .raw()
            .commit(
                Some(&reference),
                &signature,
                &signature,
                "Update README while setup is pending",
                &tree,
                &[&parent],
            )
            .expect("canonical branch should advance");
        stored
            .sign_refs(
                &fixture
                    .operator
                    .signer()
                    .expect("operator signer should load"),
            )
            .expect("delegate references should sign");
        drop(checkout);

        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("pending draft should survive canonical changes");
        assert_eq!(
            fs::read_to_string(checkout.path.join(FILE_NAME)).expect("draft should remain"),
            draft
        );
        let id = checkout.publish(1).expect("binding patch should publish");
        let patch =
            radicle::cob::patch::Patches::open(&stored, radicle::cob::store::access::ReadOnly)
                .expect("patches should open")
                .get(&id)
                .expect("patch should read")
                .expect("patch should exist");
        assert_eq!(*patch.latest().1.base(), canonical);
        assert_eq!(
            stored
                .canonical_head()
                .expect("canonical head should read")
                .1,
            advanced
        );
        let proposed = fs::read_to_string(checkout.path.join(FILE_NAME))
            .expect("proposed manifest should read");
        assert!(proposed.contains("# Preserve this repository policy."));
        Manifest::validate_candidate(&proposed, Member::Nico, fixture.operator.did())
            .expect("existing member must remain valid");
        Manifest::validate_candidate(&proposed, Member::Maya, did)
            .expect("new member must be valid");
    }
}
