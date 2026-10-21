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
    profile::Signer,
    storage::{ReadRepository as _, ReadStorage as _, SignRepository as _, WriteRepository as _},
};

use crate::{
    Application,
    agent::Role,
    cli::setup::{Error, ManifestUpdate},
    identity::{Identity, Manager, MemberLock},
    manifest::{FILE_NAME, Manifest},
    member::Member,
    queue::TaskQueue,
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
    pub(crate) async fn open(application: &Application) -> Result<Self, Error> {
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
        let queue = TaskQueue::open(&home.state_database(), checkout.repository, checkout.member)
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
        // Replicated storage can lack a local Git identity. COB commits must use the member,
        // regardless of whether the operator has a global Git identity configured.
        stored.set_user(&self.identity.profile().info())?;
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
        if repository.find_remote("rad")?.url()? != Url::from(self.repository).to_string().as_str()
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

    pub(crate) fn prepare(&self, update: ManifestUpdate) -> Result<(), Error> {
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

    pub(crate) fn preview(&self, generation: u64) -> Result<Option<(String, bool)>, Error> {
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

    pub(crate) fn proposed_provider(&self, generation: u64) -> Result<Option<String>, Error> {
        self.validated_proposal(generation)?
            .map(|proposal| {
                Manifest::validate_candidate(&proposal.manifest, self.member, self.identity.did())
                    .map(|binding| binding.provider().map(str::to_owned))
            })
            .transpose()
            .map(Option::flatten)
            .map_err(Error::from)
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

    pub(crate) fn publish(
        &self,
        generation: u64,
        title: &str,
        description: &str,
    ) -> Result<PatchId, Error> {
        let proposal = self
            .validated_proposal(generation)?
            .ok_or(Error::MissingProposal)?;
        let repository = Repository::open(&self.path)?;
        let head = proposal.commit(&repository, title)?;
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
            Some(signer) => Signer::Key(signer),
            None => self.identity.profile().signer()?,
        };
        let id = match self.find_patch(proposal.base, head)? {
            Some(id) => id,
            None => {
                let mut patches = self.identity.profile().patches_mut(&stored, &signer)?;
                let patch = patches.create(
                    Title::try_from(title.to_owned())?,
                    description,
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
    fn commit(&self, repository: &Repository, title: &str) -> Result<git2::Oid, Error> {
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
                    title,
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

    use clap::Parser as _;
    use radicle::{
        crypto::Seed,
        identity::Visibility,
        node::{Alias, Handle as _, config::PeerConfig, policy::Scope},
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
        node::NodeManager,
        queue::TaskQueue,
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
            let home = MemberHome::at(root.path().join("maya"));
            home.create().expect("member home should create");
            home.record_identity(Member::Maya)
                .expect("identity owner should be recorded");
            let provider = Provider::Managed {
                protection: KeyProtection::Unencrypted,
            };
            let identity = Manager::default()
                .load(Member::Maya, &home, &provider)
                .expect("member identity should create");
            let signer = operator.signer().expect("operator signer should load");
            let (repository, _, _) = radicle::rad::init(
                &git,
                "setup-test".try_into().expect("name should parse"),
                "Setup test",
                "main".try_into().expect("branch should parse"),
                Visibility::private([identity.did()]),
                &signer,
                &operator.storage,
            )
            .expect("Radicle repository should create");
            home.record(Member::Maya, repository)
                .expect("member should bind to repository");
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
    #[ignore = "requires the rad and radicle-node binaries"]
    async fn retains_the_patch_when_no_seed_can_replicate_it() {
        let fixture = Fixture::new();
        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("member checkout should open");
        let mut config = checkout.identity.profile().config.clone();
        config.preferred_seeds.clear();
        config.node.peers = PeerConfig::Static;
        config
            .write(&checkout.identity.profile().home().config())
            .expect("member configuration should write");
        checkout
            .prepare(fixture.proposal(&checkout))
            .expect("manifest should prepare");
        let id = checkout
            .publish(1, "Bind maya to radicle-team", "Set up Maya")
            .expect("patch should publish locally");
        let socket = checkout.identity.profile().home().socket_default();
        drop(checkout);

        assert!(matches!(
            fixture.application.announce_manifest_patch().await,
            Err(crate::application::Error::ReplicationUnconfirmed { .. })
        ));
        assert!(!socket.exists(), "failed setup must also stop its node");
        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("failed setup should release the member lock");
        assert_eq!(
            checkout
                .publish(1, "Bind maya to radicle-team", "Set up Maya")
                .expect("retry should reuse the patch"),
            id
        );
    }

    #[tokio::test]
    #[ignore = "requires the rad and radicle-node binaries"]
    async fn replicates_the_patch_before_stopping_the_member_node() {
        let fixture = Fixture::new();
        let repository = fixture.application.repository();
        let mut config = fixture.operator.config.clone();
        config.preferred_seeds.clear();
        config.node.peers = PeerConfig::Static;
        config.node.listen = vec!["127.0.0.1:0".parse().expect("address should parse")];
        config
            .write(&fixture.operator.home().config())
            .expect("seed configuration should write");
        let seed_home = MemberHome::at(fixture._root.path().join("seed"));
        seed_home.create().expect("seed home should create");
        seed_home
            .record(Member::Nico, repository)
            .expect("seed home should create");
        let seed_identity = Manager::default()
            .load(
                Member::Nico,
                &seed_home,
                &Provider::RadicleHome {
                    home: fixture.operator.home().path().to_owned(),
                    expected_did: Some(fixture.operator.did()),
                },
            )
            .expect("seed identity should load");
        let seed = NodeManager::start(&seed_identity, &seed_home)
            .await
            .expect("seed node should start");
        let mut node = seed.node().clone();
        fixture
            .operator
            .seed(repository, Scope::All, &mut node)
            .expect("operator should seed all member namespaces");
        let address = node
            .listen_addrs()
            .expect("seed addresses should read")
            .into_iter()
            .next()
            .expect("seed should listen");
        let application = fixture
            .application
            .clone()
            .with_seed(*fixture.operator.id());
        let checkout = RepositoryCheckout::open(&application)
            .await
            .expect("member checkout should open");
        let mut config = checkout.identity.profile().config.clone();
        config.preferred_seeds = vec![
            (
                *fixture.operator.id(),
                address
                    .to_string()
                    .parse()
                    .expect("seed address should parse"),
            )
                .into(),
        ];
        config.node.peers = PeerConfig::Static;
        config
            .write(&checkout.identity.profile().home().config())
            .expect("member configuration should write");
        checkout
            .prepare(fixture.proposal(&checkout))
            .expect("manifest should prepare");
        let id = checkout
            .publish(1, "Bind maya to radicle-team", "Set up Maya")
            .expect("patch should publish locally");
        let did = checkout.identity.did();
        let socket = checkout.identity.profile().home().socket_default();
        drop(checkout);

        let stored = fixture
            .operator
            .storage
            .repository(repository)
            .expect("seed storage should open");
        let patches =
            radicle::cob::patch::Patches::open(&stored, radicle::cob::store::access::ReadOnly)
                .expect("seed patches should open");
        assert!(
            patches
                .get(&id)
                .expect("seed patches should read")
                .is_none()
        );
        application
            .announce_manifest_patch()
            .await
            .expect("setup should replicate the patch before returning");
        let patch = patches
            .get(&id)
            .expect("replicated patch should read")
            .expect("patch must already exist in seed storage");
        assert_eq!(patch.author().id(), &did);
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
        assert!(!socket.exists(), "setup must stop and reap its member node");

        assert_eq!(patches.counts().expect("patch count should read").open, 1);
        seed.stop().await.expect("seed should stop cleanly");
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
            .publish(1, "Bind maya to radicle-team", "Set up Maya")
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
        let commit = stored
            .raw()
            .find_commit((*id).into())
            .expect("patch COB commit should read");
        let info = checkout.identity.profile().info();
        assert_eq!(
            commit.author().name().expect("member name"),
            info.name().as_ref()
        );
        assert_eq!(commit.author().email().expect("member email"), info.email());
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
        let id = checkout
            .publish(1, "Bind maya to radicle-team", "Set up Maya")
            .expect("committed draft should publish");
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
            checkout
                .publish(1, "Bind maya to radicle-team", "Set up Maya")
                .expect("retry should reuse the patch"),
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
        let patch = checkout
            .publish(1, "Bind maya to radicle-team", "Set up Maya")
            .expect("patch should publish");
        let home = fixture
            .application
            .member_home()
            .expect("home should resolve");
        let queue = TaskQueue::open(
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
            .push(crate::queue::TaskRequest::new(
                "Update README".to_owned(),
                Source::Subject(Subject::Issue(fixture.base.into())),
                QueuePosition::End,
            ))
            .await
            .expect("task should enqueue");
        let task = queue.next().await.expect("task should start");
        workspaces
            .prepare_task(&task)
            .await
            .expect("coder should move to its task branch");
        assert!(!home.worktree(Role::Coder).join(FILE_NAME).exists());
        queue.finish().await.expect("task should finish");
        workspaces
            .initialize()
            .await
            .expect("finished task branch should be cleaned up");

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
            .publish(1, "Bind maya to radicle-team", "Set up Maya")
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
            .publish(1, "Bind maya to radicle-team", "Set up Maya")
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
        let id = checkout
            .publish(1, "Bind maya to radicle-team", "Set up Maya")
            .expect("binding patch should publish");
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
    #[tokio::test]
    async fn provider_patch_changes_only_provider_and_activates_after_canonical_merge() {
        let fixture = Fixture::new();
        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("checkout");
        checkout
            .prepare(fixture.proposal(&checkout))
            .expect("setup draft");
        let context = crate::cli::provider::Context::load(
            fixture.application.home().map(std::path::Path::to_path_buf),
        )
        .expect("pending context");
        assert!(!context.canonical);
        assert_eq!(context.provider().expect("initial provider"), "gpt");
        checkout
            .publish(1, "Bind maya to radicle-team", "Setup")
            .expect("setup patch");
        let git = git2::Repository::open(&checkout.path).expect("coder repository");
        let setup_head = git
            .head()
            .expect("head")
            .peel_to_commit()
            .expect("setup commit")
            .id();
        let stored = checkout
            .identity
            .profile()
            .storage
            .repository(fixture.application.repository())
            .expect("stored repository");
        let delegate = format!("refs/namespaces/{}/refs/heads/main", fixture.operator.id());
        stored
            .raw()
            .reference(&delegate, setup_head, true, "Approve setup")
            .expect("approve setup");
        stored
            .sign_refs(&fixture.operator.signer().expect("delegate signer"))
            .expect("signed delegate refs");
        drop(checkout);

        let checkout = RepositoryCheckout::open(&fixture.application)
            .await
            .expect("approved checkout");
        let before = fs::read_to_string(checkout.path.join(FILE_NAME)).expect("canonical manifest");
        let before: toml_edit::DocumentMut = before.parse().expect("manifest document");
        checkout
            .prepare(ManifestUpdate::Provider(ManifestEntry {
                member: Member::Maya,
                did: checkout.identity.did(),
                generation: 1,
                provider: Some("anthropic".into()),
            }))
            .expect("provider draft");
        assert!(
            git.statuses(None)
                .expect("draft status")
                .iter()
                .any(|entry| entry.path().is_ok_and(|path| path == FILE_NAME))
        );
        let mut changed: toml_edit::DocumentMut = fs::read_to_string(checkout.path.join(FILE_NAME))
            .expect("draft manifest")
            .parse()
            .expect("draft document");
        assert_eq!(
            changed["members"]["maya"]["provider"].as_str(),
            Some("anthropic")
        );
        changed["members"]["maya"]["provider"] = before["members"]["maya"]["provider"].clone();
        assert_eq!(
            changed.to_string(),
            before.to_string(),
            "all identity and repository policy fields remain unchanged"
        );
        assert_eq!(
            checkout
                .proposed_provider(1)
                .expect("pending provider")
                .as_deref(),
            Some("anthropic")
        );
        let patch = checkout
            .publish(1, "Use anthropic for maya", "Change Maya's provider")
            .expect("provider patch");
        assert_eq!(
            checkout
                .publish(1, "Use anthropic for maya", "Change Maya's provider")
                .expect("retry same patch"),
            patch
        );
        let patches =
            radicle::cob::patch::Patches::open(&stored, radicle::cob::store::access::ReadOnly)
                .expect("patch store");
        let patch = patches
            .get(&patch)
            .expect("patch read")
            .expect("patch exists");
        assert_eq!(patch.author().id(), &checkout.identity.did());
        assert_eq!(patch.title(), "Use anthropic for maya");
        assert_eq!(
            stored.canonical_head().expect("canonical head").1,
            setup_head
        );
        let home = fixture.application.home().map(std::path::Path::to_path_buf);
        let context =
            crate::cli::provider::Context::load(home.clone()).expect("canonical provider context");
        assert!(context.canonical);
        assert_eq!(
            context.provider().expect("current provider"),
            "gpt",
            "an unmerged patch never selects the new provider"
        );
        let proposed = patch.latest().1.head();
        stored
            .raw()
            .reference(&delegate, proposed.into(), true, "Approve provider")
            .expect("merge provider");
        stored
            .sign_refs(&fixture.operator.signer().expect("delegate signer"))
            .expect("sign approved provider");
        assert_eq!(
            crate::cli::provider::Context::load(home)
                .expect("updated context")
                .provider()
                .expect("approved provider"),
            "anthropic"
        );
        assert_eq!(
            git2::Repository::open(&fixture.source)
                .expect("operator checkout")
                .head()
                .expect("operator head")
                .target(),
            Some(fixture.base.into())
        );
    }
    #[tokio::test]
    async fn provider_commands_keep_manual_changes_and_publish_only_after_confirmation() {
        const FLAG: &str = "RAD_TEAM_TEST_PROVIDER_COMMANDS";
        if std::env::var_os(FLAG).is_some() {
            let fixture = Fixture::new();
            let checkout = RepositoryCheckout::open(&fixture.application)
                .await
                .expect("checkout");
            let mut update = fixture.proposal(&checkout);
            if let ManifestUpdate::Create { entry, .. } = &mut update {
                entry.provider = Some("ollama".into());
            }
            checkout.prepare(update).expect("setup draft");
            let home = fixture.application.home().map(std::path::Path::to_path_buf);
            let pending =
                crate::cli::provider::Context::load(home.clone()).expect("pending context");
            assert!(matches!(
                pending.select().await,
                Err(crate::cli::provider::Error::PendingSetup)
            ));
            checkout
                .publish(1, "Set up maya", "Setup")
                .expect("setup patch");
            let git = git2::Repository::open(&checkout.path).expect("coder Git");
            let head = git
                .head()
                .expect("head")
                .peel_to_commit()
                .expect("setup commit")
                .id();
            let stored = checkout
                .identity
                .profile()
                .storage
                .repository(fixture.application.repository())
                .expect("stored repository");
            let delegate = format!("refs/namespaces/{}/refs/heads/main", fixture.operator.id());
            stored
                .raw()
                .reference(&delegate, head, true, "Approve setup")
                .expect("approve setup");
            stored
                .sign_refs(&fixture.operator.signer().expect("delegate signer"))
                .expect("approve refs");
            let configuration = crate::provider::configuration().expect("isolated configuration");
            let directory = crate::provider::directory(&configuration);
            fs::create_dir_all(&directory).expect("provider directory");
            fs::copy(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers/ollama.toml"),
                crate::provider::path(&directory, "ollama"),
            )
            .expect("install the fixture's configured provider");
            fs::write(
                crate::provider::path(&directory, "aaa-test"),
                "adapter = \"opencode\"\n[auth]\nways = [\"none\"]\n",
            )
            .expect("test provider");
            fs::write(crate::provider::path(&directory, "bad-test"), "invalid = [")
                .expect("unreadable provider");
            drop(checkout);
            let context =
                crate::cli::provider::Context::load(home.clone()).expect("approved context");
            context
                .list()
                .await
                .expect("list providers including an unreadable file");
            crate::cli::Cli::try_parse_from([
                "rad-team",
                "provider",
                "--home",
                context.home.path().to_str().expect("member home path"),
            ])
            .expect("current provider command")
            .execute(None)
            .await
            .expect("print the configured provider without a subcommand");
            context
                .login()
                .await
                .expect("login infers the configured provider needing no auth");
            context
                .select()
                .await
                .expect("accept the initially highlighted current provider");
            assert_eq!(git.head().expect("unchanged head").target(), Some(head));
            assert!(git.statuses(None).expect("unchanged status").is_empty());
            context.select().await.expect("manual publication choice");
            assert_eq!(
                git.head().expect("head after manual choice").target(),
                Some(head)
            );
            assert!(
                git.statuses(None)
                    .expect("manual draft status")
                    .iter()
                    .any(|entry| entry.path().is_ok_and(|path| path == FILE_NAME))
            );
            assert_eq!(
                crate::cli::provider::Context::load(home.clone())
                    .expect("current context")
                    .provider()
                    .expect("current provider"),
                "ollama"
            );
            let error = context
                .select()
                .await
                .expect_err("fake node prevents announcement");
            assert!(
                matches!(error, crate::cli::provider::Error::Application(_)),
                "{error}"
            );
            assert_ne!(git.head().expect("published head").target(), Some(head));
            assert!(git.statuses(None).expect("committed status").is_empty());
            assert_eq!(stored.canonical_head().expect("canonical head").1, head);
            assert_eq!(
                crate::cli::provider::Context::load(home)
                    .expect("canonical selection")
                    .provider()
                    .expect("current provider"),
                "ollama"
            );
            return;
        }
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("isolated environment");
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).expect("fake node binaries");
        for name in ["rad", "radicle-node"] {
            let file = bin.join(name);
            fs::write(&file, "#!/bin/sh\nexit 1\n").expect("offline node executable");
            fs::set_permissions(file, fs::Permissions::from_mode(0o700)).expect("executable");
        }
        let driver = r#"
import os,pty,select,sys,time,signal
pid,fd=pty.fork()
if pid==0:
    os.execv(sys.argv[1],[sys.argv[1],'--exact','cli::setup::checkout::tests::provider_commands_keep_manual_changes_and_publish_only_after_confirmation','--nocapture'])
answers=[
    (b'Which provider should this member use?',b'\r'),
    (b'Which provider should this member use?',b'k'*20+b'\r'),
    (b'Publish this provider change as a Radicle patch?',b'n'),
    (b'Which provider should this member use?',b'k'*20+b'\r'),
    (b'Publish this provider change as a Radicle patch?',b'y'),
]
buffer=b''; transcript=b''; deadline=time.monotonic()+45
while time.monotonic()<deadline:
    ready,_,_=select.select([fd],[],[],0.1)
    if ready:
        try: data=os.read(fd,65536)
        except OSError: data=b''
        buffer+=data; transcript+=data
        if answers and answers[0][0] in buffer:
            _,answer=answers.pop(0); os.write(fd,answer); buffer=b''
    done,status=os.waitpid(pid,os.WNOHANG)
    if done:
        if (status or answers or b'\r\nollama\r\n' not in transcript
                or b'ollama (current)' not in transcript):
            print(transcript.decode(errors='replace')); sys.exit(1)
        sys.exit(0)
os.killpg(pid,signal.SIGKILL); os.waitpid(pid,0)
print(transcript.decode(errors='replace')); raise RuntimeError('provider prompts timed out')
"#;
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH"),
        )))
        .expect("isolated PATH");
        let output = std::process::Command::new("python3")
            .args(["-c", driver])
            .arg(std::env::current_exe().expect("test binary"))
            .env(FLAG, "1")
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("PATH", path)
            .env_remove(crate::home::ENVIRONMENT_VARIABLE)
            .output()
            .expect("terminal prompt driver");
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
