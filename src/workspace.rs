//! Reusable working copies supplied to agents.
//!
//! Each adapter owns one working copy for the lifetime of the member directory, and they share a
//! single Git object store. Moving one to a different issue, patch, or branch costs a checkout
//! rather than a clone, and nothing an adapter does to its own copy is visible in another's.
//!
//! Everything an adapter can write is untrusted, and Git decides what to do from what it finds in
//! the repository it is pointed at. The runtime is pointed at repositories an adapter can write,
//! so it runs Git inside a sandbox of its own: the working copy the command is for, the shared
//! store, and the one repository it fetches from. Code which reaches a Git command from an
//! adapter's files has got no further than that adapter's own files, and cannot reach another
//! adapter's, the member's identity, or the rest of Radicle storage. External network access is
//! denied, subject to the platform difference in
//! [`NetworkAccess`](crate::sandbox::NetworkAccess).
//!
//! Two rules narrow it further, so the sandbox is the last line rather than the only one. Nothing
//! an adapter writes may say where Git looks: the sandbox holds hooks, configuration and every
//! file naming a working copy's Git directory read-only. And nothing it writes may say which
//! reference the runtime updates: every reference written here is one the runtime named itself,
//! addressed in full and set with `--no-deref`, because a name in that namespace can be made to
//! stand for another reference entirely.

use std::{collections::BTreeSet, ffi::OsStr, path::PathBuf, sync::Arc};

use radicle::{
    cob::{
        ObjectId,
        issue::{Issues, State as IssueState},
        patch::{Patches, State},
        store::access::ReadOnly,
    },
    git::{Url, UserInfo, Version},
    identity::RepoId,
    storage::{ReadRepository as _, ReadStorage as _, git::Storage},
};
use tokio::sync::{Mutex, OnceCell};

use crate::{
    agent::{Adapter, Source},
    home::MemberHome,
    sandbox::AgentSandbox,
    watcher::Subject,
};

/// Namespace holding every branch this runtime creates in a member's working copies.
pub(crate) const BRANCHES: &str = "radicle-team";
/// First Git which records a linked working copy's location relatively, keeping the member
/// directory movable. Without it Git stores where the directory was created, and renaming it
/// breaks every working copy.
const RELATIVE_WORKTREES: Version = Version {
    major: 2,
    minor: 48,
    patch: 0,
};

// Where a Git command runs, and whose working copy it may write.
//
// The two are not the same. Creating a working copy runs in the shared store but writes the copy,
// so each command says which sandbox it belongs in rather than having it inferred from a path.
#[derive(Debug, Clone, Copy)]
enum In {
    // The shared store, writing no working copy.
    Store,
    // The shared store, creating one adapter's working copy.
    Creating(Adapter),
    // One adapter's working copy.
    Copy(Adapter),
}

impl In {
    const fn adapter(self) -> Option<Adapter> {
        match self {
            Self::Store => None,
            Self::Creating(adapter) | Self::Copy(adapter) => Some(adapter),
        }
    }
}

/// Working copies for one member, one per adapter, sharing one object store.
#[derive(Debug, Clone)]
pub(crate) struct Workspaces {
    home: MemberHome,
    storage: Storage,
    repository: RepoId,
    info: UserInfo,
    // Serialises fetches and reference updates, which every adapter performs against the one
    // shared store. Git would refuse a concurrent reference update rather than corrupt anything,
    // so this exists to avoid spurious failures rather than to protect the repository.
    shared: Arc<Mutex<()>>,
    // Branches whose work has landed but which were still checked out when that was noticed.
    // Removed at the next preparation, by which time the adapter holding one has usually moved on.
    retiring: Arc<Mutex<BTreeSet<String>>>,
    // One per working copy, plus one for the shared store alone. Loaded on first use rather than
    // at construction, because loading reads embedded profiles and creates directories, and a
    // member which never prepares a working copy needs none of them.
    sandboxes: Arc<[OnceCell<AgentSandbox>; Adapter::ALL.len() + 1]>,
}

impl Workspaces {
    pub(crate) fn new(
        home: MemberHome,
        storage: Storage,
        repository: RepoId,
        info: UserInfo,
    ) -> Self {
        Self {
            home,
            storage,
            repository,
            info,
            shared: Arc::default(),
            retiring: Arc::default(),
            sandboxes: Arc::default(),
        }
    }

    /// Returns the shared object store every working copy reads from.
    #[cfg(all(test, not(miri)))]
    pub(crate) fn store(&self) -> PathBuf {
        self.home.git_common()
    }

    /// Returns working copies rooted under `target/`, over `storage`.
    ///
    /// Not initialized: a test which needs the copies to exist calls [`Self::initialize`] itself.
    /// `name` separates one test's directory from another's, because the harness runs them at the
    /// same time and one clearing another's would fail whichever lost the race.
    #[cfg(all(test, not(miri)))]
    pub(crate) fn memory(
        name: &str,
        storage: Storage,
        repository: RepoId,
        info: UserInfo,
    ) -> Result<Self, Error> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(name);
        let _removed = std::fs::remove_dir_all(&root);
        let home = MemberHome::at(root.join("member"));
        home.create()
            .map_err(|error| Error::Sandbox(error.to_string()))?;

        Ok(Self::new(home, storage, repository, info))
    }

    /// Creates the shared object store and every adapter's working copy.
    ///
    /// Safe to call again on an existing member directory, which is what happens on every start.
    pub(crate) async fn initialize(&self) -> Result<(), Error> {
        self.require_relative_worktrees().await?;
        let common = self.home.git_common();
        if !common.join("HEAD").is_file() {
            let manager = self.clone();
            tokio::task::spawn_blocking(move || manager.clone_common())
                .await
                .map_err(|error| Error::Task(error.to_string()))??;
        }
        self.prepare_common().await?;
        for adapter in Adapter::ALL {
            let worktree = self.home.worktree(adapter);
            if !worktree.join(".git").exists() {
                // Relative paths keep the member directory movable. Without them Git records
                // where the directory was created, and renaming it breaks every working copy.
                self.git(
                    In::Creating(adapter),
                    [
                        OsStr::new("worktree"),
                        OsStr::new("add"),
                        OsStr::new("--relative-paths"),
                        OsStr::new("--detach"),
                        worktree.as_os_str(),
                        OsStr::new("HEAD"),
                    ],
                )
                .await?;
            }
        }

        self.retire_finished().await
    }

    /// Moves `adapter`'s working copy onto whatever `source` addresses and reports its path.
    ///
    /// A talking or chatting adapter is left on a detached head, because two working copies of one
    /// repository cannot hold the same branch. A building adapter is given a real branch, since it
    /// has commits to make.
    pub(crate) async fn prepare(&self, adapter: Adapter, source: Source) -> Result<PathBuf, Error> {
        let worktree = self.home.worktree(adapter);
        let head = self.head(source.clone()).await?;
        // Held across the whole preparation, not only the fetch. Salvaging and placing a branch
        // both write references into the one shared store, and two adapters preparing at once
        // would otherwise collide there.
        let _shared = self.shared.lock().await;
        let place = In::Copy(adapter);
        self.fetch(place, &head).await?;
        // Salvaging and placing a branch belong together to the adapter which owns branches. It
        // is the only one whose working copy holds work: what the others produce is the answer
        // they publish, and their copy holds only what they looked at on the way to it.
        //
        // Placing the branch comes second because resetting while standing on one moves it, which
        // would throw away commits the agent made but had not published yet. It is also what moves
        // the copy off the branch salvaging left it on.
        let head = if adapter.owns_branches() {
            self.salvage(place, &source).await?;
            self.place_branch(place, &source, &head).await?
        } else {
            head
        };
        self.git(place, ["reset", "--hard", &head]).await?;
        // Ignored files are removed as well, so nothing a previous delivery left behind can reach
        // the next one. Build output is kept out of the working copy instead, in the adapter's own
        // cache directory.
        self.git(place, ["clean", "-ffdx"]).await?;
        // Last, because moving this working copy is what frees the branch it was standing on.
        // Tidy-up, so a failure is reported rather than allowed to fail a prepared delivery.
        if let Err(error) = self.sweep().await {
            tracing::warn!(%error, "could not remove the branch of a landed patch");
        }

        Ok(worktree)
    }

    // Refuses a Git too old to record worktree paths relatively.
    async fn require_relative_worktrees(&self) -> Result<(), Error> {
        let found = tokio::task::spawn_blocking(radicle::git::version)
            .await
            .map_err(|error| Error::Task(error.to_string()))?
            .map_err(|error| Error::GitTooOld(error.to_string()))?;
        if found >= RELATIVE_WORKTREES {
            return Ok(());
        }

        Err(Error::GitTooOld(found.to_string()))
    }

    // Preserves anything the previous run left uncommitted before the tree is reset.
    async fn salvage(&self, place: In, source: &Source) -> Result<(), Error> {
        if self.status(place).await?.is_empty() {
            return Ok(());
        }
        let branch = self.salvage_branch(place, source).await?;
        let head = self.capture(place, ["rev-parse", "HEAD"]).await?;
        self.stand_on(place, &branch, &head).await?;
        self.git(place, ["add", "--all"]).await?;
        self.git(
            place,
            ["commit", "--no-verify", "-m", "Salvage interrupted work"],
        )
        .await?;
        tracing::warn!(
            adapter = ?place.adapter(),
            %source,
            %branch,
            "previous run left uncommitted work; it was committed rather than discarded"
        );

        Ok(())
    }

    // Puts the building adapter on the branch for `source` without losing commits.
    //
    // The branch may already be ahead of the published patch, because the agent commits before it
    // publishes. Forcing the branch back to the patch head would destroy exactly that work.
    async fn place_branch(&self, place: In, source: &Source, head: &str) -> Result<String, Error> {
        let branch = Self::branch(source);
        let tip = self.tip(place, &branch).await?;
        let target = match tip {
            Some(tip) if self.contains(place, &branch, head).await? => tip,
            Some(tip) => {
                self.copy_diverged(place, source, &tip).await?;
                head.to_owned()
            }
            None => head.to_owned(),
        };
        self.stand_on(place, &branch, &target).await?;

        Ok(target)
    }

    // Keeps a copy of a branch which has drifted away from the published patch.
    async fn copy_diverged(&self, place: In, source: &Source, tip: &str) -> Result<(), Error> {
        let salvage = self.salvage_branch(place, source).await?;
        self.set_reference(place, &salvage, tip).await?;
        tracing::warn!(
            %source,
            %salvage,
            "branch had diverged from the published patch; kept a copy before resetting"
        );

        Ok(())
    }

    // Points `branch` at `commit` and stands the working copy on it.
    async fn stand_on(&self, place: In, branch: &str, commit: &str) -> Result<(), Error> {
        self.set_reference(place, branch, commit).await?;
        self.git(place, ["symbolic-ref", "HEAD", &Self::reference(branch)])
            .await
    }

    // Points `branch` at `commit` without following it.
    //
    // Every reference the runtime writes is one it named itself and set directly. An adapter can
    // write inside its own namespace, so a name there may stand for another reference entirely,
    // and updating through it would move whatever it stands for rather than the branch.
    async fn set_reference(&self, place: In, branch: &str, commit: &str) -> Result<(), Error> {
        self.git(
            place,
            ["update-ref", "--no-deref", &Self::reference(branch), commit],
        )
        .await
    }

    // Returns the full name of a branch, which is never ambiguous with a tag or a remote.
    fn reference(branch: &str) -> String {
        format!("refs/heads/{branch}")
    }

    /// Moves the branch a job worked on onto the identity of the patch it produced.
    ///
    /// A job taken from an issue names its branch after that issue, because the patch it will
    /// become does not exist yet. Left there, nothing would ever settle that name: the issue can
    /// stay open long after the patch lands. Renaming it means one rule retires every branch, and
    /// somebody reading the working copy sees the patch they are looking at.
    pub(crate) async fn adopt(&self, source: &Source, patch: ObjectId) -> Result<(), Error> {
        let worked = Self::branch(source);
        let adopted = Self::branch(&Source::Subject(Subject::Patch(patch)));
        if worked == adopted {
            return Ok(());
        }
        let _shared = self.shared.lock().await;
        let Some(tip) = self.tip(In::Store, &worked).await? else {
            return Ok(());
        };
        self.set_reference(In::Store, &adopted, &tip).await?;
        self.follow(&worked, &adopted).await?;
        self.remove_branch(&worked).await?;
        tracing::info!(%worked, %adopted, "branch moved onto the patch it produced");

        Ok(())
    }

    // Points whichever working copy stands on `worked` at `adopted` instead.
    async fn follow(&self, worked: &str, adopted: &str) -> Result<(), Error> {
        for adapter in Adapter::ALL {
            let place = In::Copy(adapter);
            let standing = self
                .capture(place, ["symbolic-ref", "--quiet", "HEAD"])
                .await
                .unwrap_or_default();
            if standing == Self::reference(worked) {
                self.git(place, ["symbolic-ref", "HEAD", &Self::reference(adopted)])
                    .await?;
            }
        }

        Ok(())
    }

    /// Removes the branch belonging to a patch or issue whose work has landed.
    ///
    /// A branch a working copy is standing on is left alone and removed at the next preparation
    /// instead. A landed patch is tidy-up, never a reason to pull the ground out from under a job
    /// which is still running. Restarting the member forgets what is still waiting, which leaves a
    /// stale branch behind and nothing worse.
    pub(crate) async fn retire(&self, subject: Subject) -> Result<(), Error> {
        let branch = Self::branch(&Source::Subject(subject));
        let _shared = self.shared.lock().await;
        let _inserted = self.retiring.lock().await.insert(branch);

        self.sweep().await
    }

    ///Removes every branch waiting to be retired which no working copy is standing on.
    async fn sweep(&self) -> Result<(), Error> {
        if self.retiring.lock().await.is_empty() {
            return Ok(());
        }
        let held = self.checked_out().await?;
        let present = self.namespace().await?;
        let mut retiring = self.retiring.lock().await;
        let mut waiting = BTreeSet::new();
        for branch in retiring.iter() {
            if held.contains(branch) {
                let _waiting = waiting.insert(branch.clone());
                continue;
            }
            if present.contains(branch) {
                self.remove_branch(branch).await?;
            }
        }
        *retiring = waiting;

        Ok(())
    }

    async fn remove_branch(&self, branch: &str) -> Result<(), Error> {
        self.git(
            In::Store,
            ["update-ref", "--no-deref", "-d", &Self::reference(branch)],
        )
        .await?;
        tracing::info!(%branch, "removed the branch of finished work");

        Ok(())
    }

    // Returns every branch the runtime has made in the shared store.
    async fn namespace(&self) -> Result<BTreeSet<String>, Error> {
        let listed = self
            .capture(
                In::Store,
                [
                    "for-each-ref",
                    "--format=%(refname:lstrip=2)",
                    &Self::reference(BRANCHES),
                ],
            )
            .await?;

        Ok(listed.lines().map(str::to_owned).collect())
    }

    // Returns the branches the working copies are standing on.
    async fn checked_out(&self) -> Result<BTreeSet<String>, Error> {
        let listed = self
            .capture(In::Store, ["worktree", "list", "--porcelain"])
            .await?;

        Ok(listed
            .lines()
            .filter_map(|line| line.strip_prefix("branch refs/heads/"))
            .map(str::to_owned)
            .collect())
    }

    // Picks a salvage branch name which is not already in use.
    //
    // One name per subject would let a second crash overwrite what the first one preserved, which
    // is the failure this whole path exists to prevent.
    async fn salvage_branch(&self, place: In, source: &Source) -> Result<String, Error> {
        let stem = format!("{BRANCHES}/salvage/{}", Self::slug(source));
        let taken = self
            .capture(
                place,
                [
                    "for-each-ref",
                    "--format=%(refname:lstrip=4)",
                    &Self::reference(&stem),
                ],
            )
            .await?;
        let highest = taken
            .lines()
            .filter_map(|name| name.parse::<u32>().ok())
            .max()
            .unwrap_or(0);

        highest
            .checked_add(1)
            .map(|attempt| format!("{stem}/{attempt}"))
            .ok_or(Error::SalvageExhausted(stem))
    }

    fn slug(source: &Source) -> String {
        source.to_string().replace(':', "/")
    }

    async fn status(&self, place: In) -> Result<String, Error> {
        self.capture(place, ["status", "--porcelain"]).await
    }

    // Returns the commit `branch` points at, or nothing when there is no such branch.
    //
    // One command answers both questions, because the one which reports whether a branch exists
    // prints the commit while it is at it.
    async fn tip(&self, place: In, branch: &str) -> Result<Option<String>, Error> {
        Ok(self
            .capture(
                place,
                [
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{}^{{commit}}", Self::reference(branch)),
                ],
            )
            .await
            .ok())
    }

    async fn contains(&self, place: In, branch: &str, commit: &str) -> Result<bool, Error> {
        if self.tip(place, branch).await?.is_none() {
            return Ok(false);
        }
        Ok(self
            .git(
                place,
                [
                    "merge-base",
                    "--is-ancestor",
                    commit,
                    &Self::reference(branch),
                ],
            )
            .await
            .is_ok())
    }

    fn branch(source: &Source) -> String {
        match source {
            Source::Subject(Subject::Patch(id)) => format!("{BRANCHES}/patch/{id}"),
            Source::Subject(Subject::Issue(id)) => format!("{BRANCHES}/issue/{id}"),
            Source::Chat(channel) => format!("{BRANCHES}/chat/{channel}"),
        }
    }

    async fn head(&self, source: Source) -> Result<String, Error> {
        let manager = self.clone();
        tokio::task::spawn_blocking(move || manager.head_blocking(&source))
            .await
            .map_err(|error| Error::Task(error.to_string()))?
    }

    fn head_blocking(&self, source: &Source) -> Result<String, Error> {
        let storage = self.storage.repository(self.repository)?;
        if let Source::Subject(Subject::Patch(id)) = source {
            let patch = Patches::open(&storage, ReadOnly)?
                .get(id)?
                .ok_or(Error::PatchNotFound(*id))?;

            return Ok(patch.head().to_string());
        }
        let (_canonical, head) = storage.canonical_head()?;

        Ok(head.to_string())
    }

    async fn fetch(&self, place: In, head: &str) -> Result<(), Error> {
        let source = self.repository_path().to_string_lossy().into_owned();
        self.git(place, ["fetch", "--force", &source, head]).await
    }

    fn clone_common(&self) -> Result<(), Error> {
        let common = self.home.git_common();
        let storage = self.storage.repository(self.repository)?;
        let (canonical, _head) = storage.canonical_head()?;
        let branch = canonical
            .to_namespaced()
            .map(|name| name.strip_namespace())
            .unwrap_or(canonical);
        let remote = storage
            .delegates()?
            .iter()
            .map(|delegate| *delegate.as_key())
            .find(|delegate| storage.reference_oid(delegate, &branch).is_ok())
            .ok_or(Error::CanonicalBranchRemote)?;
        radicle::rad::checkout(self.repository, &remote, &common, &self.storage, true)?;
        let repository = git2::Repository::open_bare(&common)?;
        let push = Url::from(self.repository).with_namespace(self.info.key);
        repository.remote_set_pushurl(&radicle::rad::REMOTE_NAME, Some(&push.to_string()))?;

        Ok(())
    }

    // Prepares the parts of the shared store the sandbox depends on.
    //
    // The member's identity is recorded here because the store's configuration is read-only
    // inside the sandbox: an agent cannot set it, and without it Git stops at the first commit to
    // ask who is making it. Housekeeping is turned off for the same reason, since it would try to
    // rewrite `packed-refs`, which no agent may touch. That file and the branch directory are
    // created empty because a path which does not exist can be neither granted nor fenced off.
    async fn prepare_common(&self) -> Result<(), Error> {
        let common = self.home.git_common();
        let info = self.info.clone();
        tokio::task::spawn_blocking(move || {
            let mut config = git2::Config::open(&common.join("config"))?;
            config.set_str("user.name", info.name().as_ref())?;
            config.set_str("user.email", &info.email())?;
            config.set_i32("gc.auto", 0)?;
            std::fs::create_dir_all(common.join("refs").join("heads").join(BRANCHES))
                .map_err(Error::Io)?;
            let packed = common.join("packed-refs");
            if !packed.is_file() {
                std::fs::write(&packed, "").map_err(Error::Io)?;
            }

            Ok(())
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?
    }

    // Queues the branches of every subject whose work is over.
    //
    // Run at every start rather than only when a merge is delivered, because a branch which was
    // checked out at that moment is remembered nowhere but in memory. Reading the subjects back
    // covers a restart, a merge nobody was running to see, and the branch a job took its name
    // from, which is the issue it answered rather than the patch that answer became.
    async fn retire_finished(&self) -> Result<(), Error> {
        let branches = self
            .capture(
                In::Store,
                [
                    "for-each-ref",
                    "--format=%(refname:lstrip=2)",
                    &format!("refs/heads/{BRANCHES}"),
                ],
            )
            .await?;
        let finished = self.finished(branches).await?;
        let _shared = self.shared.lock().await;
        self.retiring.lock().await.extend(finished);

        self.sweep().await
    }

    async fn finished(&self, branches: String) -> Result<Vec<String>, Error> {
        let manager = self.clone();
        tokio::task::spawn_blocking(move || manager.finished_blocking(&branches))
            .await
            .map_err(|error| Error::Task(error.to_string()))?
    }

    // Returns the branches among `branches` whose subject has been settled.
    //
    // A salvage branch is what a failed run left behind and is never removed here. A chat branch
    // belongs to a conversation, which does not end.
    fn finished_blocking(&self, branches: &str) -> Result<Vec<String>, Error> {
        let storage = self.storage.repository(self.repository)?;
        let patches = Patches::open(&storage, ReadOnly)?;
        let issues = Issues::open(&storage, ReadOnly)?;
        let mut finished = Vec::new();
        for branch in branches.lines() {
            let settled = match Self::subject_of(branch) {
                Some(Subject::Patch(id)) => patches
                    .get(&id)?
                    .is_some_and(|patch| matches!(patch.state(), State::Merged { .. })),
                Some(Subject::Issue(id)) => issues
                    .get(&id)?
                    .is_some_and(|issue| matches!(issue.state(), IssueState::Closed { .. })),
                None => false,
            };
            if settled {
                finished.push(branch.to_owned());
            }
        }

        Ok(finished)
    }

    // Reads back the subject a branch was named after.
    //
    // The inverse of [`Self::branch`], and `None` for a name it never produced or for one whose
    // fate no subject decides, such as a salvage or a chat branch.
    fn subject_of(branch: &str) -> Option<Subject> {
        let rest = branch.strip_prefix(BRANCHES)?.strip_prefix('/')?;
        let (kind, id) = rest.split_once('/')?;
        let id = id.parse::<ObjectId>().ok()?;
        match kind {
            "patch" => Some(Subject::Patch(id)),
            "issue" => Some(Subject::Issue(id)),
            _other => None,
        }
    }

    async fn git<I, S>(&self, place: In, arguments: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.capture(place, arguments).await.map(|_output| ())
    }

    fn directory(&self, place: In) -> PathBuf {
        match place {
            In::Store | In::Creating(_) => self.home.git_common(),
            In::Copy(adapter) => self.home.worktree(adapter),
        }
    }

    // Runs one Git command inside the runtime's sandbox and returns what it printed.
    async fn capture<I, S>(&self, place: In, arguments: I) -> Result<String, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let directory = self.directory(place);
        let arguments = arguments
            .into_iter()
            .map(|argument| argument.as_ref().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        // Boxed because establishing a sandbox is a large frame, and every caller of this would
        // otherwise carry it inline all the way up.
        let output = Box::pin(
            self.sandbox(place)
                .await?
                .command("git")
                .args(arguments.clone())
                .current_dir(&directory)
                .output(),
        )
        .await
        .map_err(|error| Error::Sandbox(error.to_string()))?;
        if !output.status.success() {
            return Err(Error::Git {
                command: arguments.join(" "),
                message: output.stderr.trim().to_owned(),
            });
        }

        Ok(output.stdout.trim().to_owned())
    }

    // Returns the sandbox a command belongs in.
    //
    // One per working copy, plus one for commands which touch no working copy at all, so a
    // command run for one adapter cannot write another's files. Each is loaded once.
    async fn sandbox(&self, place: In) -> Result<&AgentSandbox, Error> {
        let adapter = place.adapter();
        let cell = self
            .sandboxes
            .get(adapter.map_or(Adapter::ALL.len(), |adapter| adapter as usize))
            .ok_or_else(|| Error::Sandbox("no sandbox for this working copy".to_owned()))?;

        cell.get_or_try_init(|| async {
            AgentSandbox::runtime(&self.home, adapter, &self.repository_path())
                .map_err(|error| Error::Sandbox(error.to_string()))
        })
        .await
    }

    /// Returns where this member's Radicle storage keeps the repository being worked on.
    fn repository_path(&self) -> PathBuf {
        self.storage.path().join(self.repository.canonical())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("workspace I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("workspace sandbox failed: {0}")]
    Sandbox(String),
    #[error("git {command} failed: {message}")]
    Git { command: String, message: String },
    #[error("workspace checkout failed: {0}")]
    Checkout(#[from] radicle::rad::CheckoutError),
    #[error("workspace Git library call failed: {0}")]
    LibGit(#[from] git2::Error),
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
    #[error(
        "Git 2.48 or newer is required so working copies survive moving the member directory, \
         found '{0}'"
    )]
    GitTooOld(String),
    #[error("could not find an unused salvage branch under '{0}'")]
    SalvageExhausted(String),
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        str::FromStr as _,
    };

    use radicle::{
        cob::{Title, patch::MergeTarget},
        crypto::Seed,
        identity::doc::Visibility,
        node::Alias,
        profile::{Home, Profile},
        storage::ReadStorage as _,
    };

    use crate::{
        agent::Adapter,
        home::MemberHome,
        watcher::Subject,
        workspace::{BRANCHES, Workspaces},
    };

    struct Fixture {
        _root: Option<tempfile::TempDir>,
        profile: Profile,
        repository: radicle::identity::RepoId,
        workspaces: Workspaces,
        home: MemberHome,
        patch: radicle::cob::ObjectId,
        issue: radicle::cob::ObjectId,
        head: git2::Oid,
    }

    async fn repository() -> Fixture {
        let temporary = tempfile::tempdir().expect("temporary directory should be created");

        build(temporary.path().to_owned(), Some(temporary)).await
    }

    // Builds the fixture under `target/`, which no platform default makes writable.
    //
    // A temporary directory is inside one on both platforms, so nothing rooted there can show
    // that the sandbox stopped a write rather than the write being permitted anyway.
    async fn repository_beyond_reach(name: &str) -> Fixture {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(name);
        let _removed = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("test directory should be created");

        build(root, None).await
    }

    async fn build(root: PathBuf, temporary: Option<tempfile::TempDir>) -> Fixture {
        let profile_home = Home::new(root.join("radicle")).expect("home should be created");
        let profile = Profile::init(
            profile_home,
            Alias::new("workspace-test"),
            None,
            Seed::generate(),
        )
        .expect("profile should initialize");
        let working = root.join("working");
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
            let stored = profile
                .storage
                .repository(repository)
                .expect("stored repository should open");
            let mut patches = profile
                .patches_mut(&stored, &signer)
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
        let issue = {
            let stored = profile
                .storage
                .repository(repository)
                .expect("stored repository should open");
            let mut issues = profile
                .issues_mut(&stored, &signer)
                .expect("issue cache should open");
            let issue = issues
                .create(
                    Title::try_from("Ask".to_owned()).expect("title should be valid"),
                    "Description",
                    &[],
                    &[],
                    [],
                )
                .expect("issue should be created");
            *issue.id()
        };
        let info = radicle::git::UserInfo {
            alias: Alias::new("workspace-test"),
            key: radicle::crypto::PublicKey::from_str(
                "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7",
            )
            .expect("member should be valid"),
        };
        let home = MemberHome::at(root.join("member"));
        home.create().expect("member directory should be created");
        let workspaces = Workspaces::new(home.clone(), profile.storage.clone(), repository, info);

        Fixture {
            _root: temporary,
            profile,
            repository,
            workspaces,
            home,
            patch,
            issue,
            head,
        }
    }

    // Settles the issue, the way somebody closing it would.
    fn close(fixture: &Fixture) {
        let stored = fixture
            .profile
            .storage
            .repository(fixture.repository)
            .expect("stored repository should open");
        let signer = fixture.profile.signer().expect("signer should load");
        let mut issues = fixture
            .profile
            .issues_mut(&stored, &signer)
            .expect("issue cache should open");
        let mut issue = issues.get_mut(&fixture.issue).expect("issue should exist");
        issue
            .lifecycle(radicle::cob::issue::State::Closed {
                reason: radicle::cob::issue::CloseReason::Solved,
            })
            .expect("issue should close");
    }

    // Records that the patch landed, the way a merge delivered by the watcher would.
    fn merge(fixture: &Fixture) {
        let stored = fixture
            .profile
            .storage
            .repository(fixture.repository)
            .expect("stored repository should open");
        let signer = fixture.profile.signer().expect("signer should load");
        let mut patches = fixture
            .profile
            .patches_mut(&stored, &signer)
            .expect("patch cache should open");
        let mut patch = patches.get_mut(&fixture.patch).expect("patch should exist");
        let revision = patch.latest().0;
        let _merged = patch
            .merge(revision, fixture.head.into())
            .expect("patch should merge");
    }

    #[tokio::test]
    async fn moves_one_adapter_without_disturbing_the_others() {
        let fixture = repository().await;
        let (workspaces, home, patch, head) = (
            &fixture.workspaces,
            &fixture.home,
            fixture.patch,
            fixture.head,
        );
        workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        for adapter in Adapter::ALL {
            assert!(
                home.worktree(adapter).join("README.md").is_file(),
                "{adapter} should have real files, not a pointer to somebody else's"
            );
        }
        assert!(
            home.git_common().join("HEAD").is_file(),
            "the shared object store must hold the history once"
        );

        let talker = workspaces
            .prepare(Adapter::Talker, Subject::Patch(patch).into())
            .await
            .expect("talker should move to the patch");
        let opened = git2::Repository::open(&talker).expect("worktree should open");
        assert_eq!(
            opened.head().expect("HEAD should exist").target(),
            Some(head),
            "the talker must see the patch's code"
        );
        drop(opened);

        fs::write(
            home.worktree(Adapter::Chatter).join("untouched.txt"),
            "chatter's own file",
        )
        .expect("chatter file should be written");
        workspaces
            .prepare(Adapter::Talker, Subject::Issue(patch).into())
            .await
            .expect("talker should move again");
        assert!(
            home.worktree(Adapter::Chatter)
                .join("untouched.txt")
                .is_file(),
            "moving one adapter must not disturb another's working copy"
        );

        fs::write(talker.join("ignored.log"), "left over").expect("leftover should be written");
        fs::write(talker.join("README.md"), "dirty").expect("README should be dirtied");
        let talker = workspaces
            .prepare(Adapter::Talker, Subject::Issue(patch).into())
            .await
            .expect("talker should move back to the canonical branch");
        assert!(
            !talker.join("ignored.log").exists(),
            "an untracked file from a previous delivery must not reach the next one"
        );
        assert_eq!(
            fs::read_to_string(talker.join("README.md")).expect("README should be readable"),
            "changed\n",
            "a modified tracked file must be restored"
        );
    }

    #[tokio::test]
    async fn keeps_work_a_crashed_run_left_behind() {
        let fixture = repository().await;
        let (workspaces, home, patch, head) = (
            &fixture.workspaces,
            &fixture.home,
            fixture.patch,
            fixture.head,
        );
        workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let coder = home.worktree(Adapter::Coder);

        workspaces
            .prepare(Adapter::Coder, Subject::Patch(patch).into())
            .await
            .expect("coder should take the patch");
        fs::write(coder.join("half-done.rs"), "unfinished").expect("file should be written");

        workspaces
            .prepare(Adapter::Coder, Subject::Patch(patch).into())
            .await
            .expect("coder should take it again");

        assert!(
            !coder.join("half-done.rs").exists(),
            "the next run must start from a clean tree"
        );
        let salvaged = git(&coder, &["branch", "--list", "radicle-team/salvage/*"]);
        assert!(
            !salvaged.is_empty(),
            "work left behind by a crashed run must be kept, not silently discarded"
        );
        assert_eq!(
            git(
                &coder,
                &[
                    "log",
                    "-1",
                    "--format=%s",
                    salvaged.trim_start_matches("* ").trim()
                ]
            ),
            "Salvage interrupted work"
        );
        let _unused = head;
    }

    // What a talking or chatting copy holds is what it looked at on the way to an answer, and
    // the answer itself is already in the issue or the channel. Keeping a branch of that would
    // preserve scratch for ever, under a name nothing ever settles.
    #[tokio::test]
    async fn discards_what_a_crashed_answering_run_left_behind() {
        let fixture = repository().await;
        let (workspaces, home, patch) = (&fixture.workspaces, &fixture.home, fixture.patch);
        workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let talker = home.worktree(Adapter::Talker);

        workspaces
            .prepare(Adapter::Talker, Subject::Patch(patch).into())
            .await
            .expect("talker should take the patch");
        fs::write(talker.join("scratch.md"), "checking something").expect("file should be written");
        fs::write(talker.join("README.md"), "edited while reading").expect("README should change");

        workspaces
            .prepare(Adapter::Talker, Subject::Patch(patch).into())
            .await
            .expect("talker should take it again");

        assert!(
            !talker.join("scratch.md").exists(),
            "the next answer must start from a clean tree"
        );
        assert!(
            git(&talker, &["branch", "--list", "radicle-team/salvage/*"]).is_empty(),
            "and nothing it touched on the way to an answer becomes a branch"
        );
    }

    // The sandbox fences these paths off individually, and a path which is not there when an
    // agent starts cannot be fenced off at all.
    #[tokio::test]
    async fn creates_everything_the_sandbox_has_to_fence_off() {
        let fixture = repository().await;
        let common = fixture.home.git_common();
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");

        let configured = fs::read_to_string(common.join("config"))
            .expect("shared store configuration should be readable");
        assert!(
            configured.contains("workspace-test@z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"),
            "the member's identity must be recorded where an agent cannot write it, or no commit \
             could be made at all: {configured}"
        );
        for path in [
            common.join("config"),
            common.join("hooks"),
            common.join("packed-refs"),
            common.join("refs").join("heads").join(BRANCHES),
            common.join("worktrees").join("talker").join("commondir"),
            common.join("worktrees").join("talker").join("gitdir"),
            fixture.home.worktree(Adapter::Talker).join(".git"),
        ] {
            assert!(
                path.exists(),
                "{} must exist before any agent starts",
                path.display()
            );
        }
    }

    // Fencing off each surface an adapter could use to smuggle code into a Git command is
    // guesswork, and one missed surface would be a way out of the sandbox entirely. This is the
    // bound on what a missed one costs: the runtime's own Git can reach the working copies and
    // nothing else, so code arriving through it gets no further than its adapter already was.
    #[tokio::test]
    async fn confines_a_hook_smuggled_into_the_shared_store() {
        let fixture = repository_beyond_reach("workspace-hook").await;
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let reached = fixture.home.git_common().join("reached.txt");
        let escaped = fixture.home.identity().join("escaped.txt");
        // The command below is run for the talking copy, so the building one is another adapter's
        // private state and out of reach even though both live side by side. Its files and the
        // entry the shared store keeps for it are the same thing: that entry holds its index.
        let others = fixture.home.worktree(Adapter::Coder).join("others.txt");
        let metadata = fixture
            .home
            .git_common()
            .join("worktrees")
            .join(Adapter::Coder.id())
            .join("index");
        fs::create_dir_all(fixture.home.identity()).expect("identity should be created");
        let hook = fixture
            .home
            .git_common()
            .join("hooks/reference-transaction");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nfor target in '{}' '{}' '{}' '{}'; do echo x > \"$target\" || true; \
                 done\nexit 0\n",
                reached.display(),
                escaped.display(),
                others.display(),
                metadata.display()
            ),
        )
        .expect("hook should be written");
        fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("hook should be executable");

        fixture
            .workspaces
            .prepare(Adapter::Talker, Subject::Patch(fixture.patch).into())
            .await
            .expect("talker should take the patch");

        assert!(
            reached.is_file(),
            "the hook must actually run, or the checks below prove nothing"
        );
        assert!(
            !escaped.is_file(),
            "a hook running inside a runtime Git command must not reach the member's identity"
        );
        assert!(
            !others.is_file(),
            "nor another adapter's working copy, which the command it rode in on was not for"
        );
        assert_ne!(
            fs::read_to_string(&metadata).unwrap_or_default(),
            "x\n",
            "nor that copy's index, which is the same thing said another way"
        );
    }

    // A working copy belongs to its adapter, so a name in the runtime's own branch namespace can
    // be made to stand for another reference entirely. Every reference the runtime writes is
    // therefore set directly: updating through this one would move the canonical branch instead.
    #[tokio::test]
    async fn never_writes_through_a_reference_an_adapter_left_behind() {
        let fixture = repository().await;
        let coder = fixture.home.worktree(Adapter::Coder);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        // Somewhere the runtime is about to move its own branch away from: the reference below
        // stands for this one, so following it would carry this one along.
        let base = git(&coder, &["rev-parse", &format!("{}~1", fixture.head)]);
        git(&coder, &["update-ref", "refs/heads/victim", &base]);
        git(
            &coder,
            &[
                "symbolic-ref",
                &format!("refs/heads/{BRANCHES}/patch/{}", fixture.patch),
                "refs/heads/victim",
            ],
        );

        fixture
            .workspaces
            .prepare(Adapter::Coder, Subject::Patch(fixture.patch).into())
            .await
            .expect("coder should take the patch");

        assert_eq!(
            git(&coder, &["rev-parse", "refs/heads/victim"]),
            base,
            "a reference outside the runtime's namespace must not move because an adapter pointed \
             a branch inside it at that reference"
        );
    }

    // Almost every job arrives as an issue, and the patch it produces is what eventually lands.
    // The branch has to end up on that patch, or the work is filed under a name whose fate says
    // nothing about it: an issue can stay open long after the patch answering it merges.
    #[tokio::test]
    async fn moves_the_branch_of_a_job_onto_the_patch_it_produced() {
        let fixture = repository().await;
        let coder = fixture.home.worktree(Adapter::Coder);
        let worked = format!("{BRANCHES}/issue/{}", fixture.issue);
        let adopted = format!("{BRANCHES}/patch/{}", fixture.patch);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        fixture
            .workspaces
            .prepare(Adapter::Coder, Subject::Issue(fixture.issue).into())
            .await
            .expect("coder should take the issue");
        let tip = git(&coder, &["rev-parse", "HEAD"]);

        fixture
            .workspaces
            .adopt(&Subject::Issue(fixture.issue).into(), fixture.patch)
            .await
            .expect("the branch should move onto the patch");

        assert!(
            git(&coder, &["branch", "--list", &worked]).is_empty(),
            "the name the job started under must not be left behind"
        );
        assert_eq!(
            git(&coder, &["rev-parse", &format!("refs/heads/{adopted}")]),
            tip,
            "the patch's branch must hold the work the job did"
        );
        assert_eq!(
            git(&coder, &["symbolic-ref", "HEAD"]),
            format!("refs/heads/{adopted}"),
            "the working copy must follow its branch rather than be left on a name which is gone"
        );
    }

    // A job which produced no patch keeps the name it started under, so something still has to
    // settle that name. Reading only patch branches back would leave it there for good.
    #[tokio::test]
    async fn removes_the_branch_of_an_issue_which_was_settled() {
        let fixture = repository().await;
        let branch = format!("{BRANCHES}/issue/{}", fixture.issue);
        let coder = fixture.home.worktree(Adapter::Coder);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        fixture
            .workspaces
            .prepare(Adapter::Coder, Subject::Issue(fixture.issue).into())
            .await
            .expect("coder should take the issue");
        fixture
            .workspaces
            .prepare(Adapter::Coder, Subject::Patch(fixture.patch).into())
            .await
            .expect("coder should move to other work");
        assert!(!git(&coder, &["branch", "--list", &branch]).is_empty());

        close(&fixture);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("the next start should reuse the working copies");

        assert!(
            git(&coder, &["branch", "--list", &branch]).is_empty(),
            "the branch a job took its name from must be removed once its issue is settled"
        );
    }

    // A merge noticed while the branch was checked out is remembered nowhere but in memory, so a
    // restart would otherwise leave that branch behind forever.
    #[tokio::test]
    async fn removes_a_landed_branch_after_a_restart() {
        let fixture = repository().await;
        let branch = format!("{BRANCHES}/patch/{}", fixture.patch);
        let coder = fixture.home.worktree(Adapter::Coder);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        fixture
            .workspaces
            .prepare(Adapter::Coder, Subject::Patch(fixture.patch).into())
            .await
            .expect("coder should take the patch");
        fixture
            .workspaces
            .prepare(Adapter::Coder, Subject::Issue(fixture.patch).into())
            .await
            .expect("coder should move to other work");
        assert!(!git(&coder, &["branch", "--list", &branch]).is_empty());

        merge(&fixture);
        // A different value, holding nothing of what the run before it was waiting to remove.
        let restarted = Workspaces::new(
            fixture.home.clone(),
            fixture.profile.storage.clone(),
            fixture.repository,
            fixture.profile.info(),
        );
        restarted
            .initialize()
            .await
            .expect("the next start should reuse the working copies");

        assert!(
            git(&coder, &["branch", "--list", &branch]).is_empty(),
            "a branch whose patch has landed must be removed at the next start"
        );
    }

    #[tokio::test]
    async fn keeps_a_landed_branch_until_the_copy_holding_it_moves_on() {
        let fixture = repository().await;
        let (workspaces, home, patch) = (&fixture.workspaces, &fixture.home, fixture.patch);
        workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let coder = home.worktree(Adapter::Coder);
        let branch = format!("radicle-team/patch/{patch}");

        workspaces
            .prepare(Adapter::Coder, Subject::Patch(patch).into())
            .await
            .expect("coder should take the patch");
        assert_eq!(git(&coder, &["rev-parse", "--abbrev-ref", "HEAD"]), branch);

        workspaces
            .retire(Subject::Patch(patch))
            .await
            .expect("retiring a branch which is checked out must not fail");
        assert!(
            !git(&coder, &["branch", "--list", &branch]).is_empty(),
            "a landed patch must not pull the branch out from under a job which is still running"
        );

        workspaces
            .prepare(Adapter::Coder, Subject::Issue(patch).into())
            .await
            .expect("coder should move to other work");

        assert!(
            git(&coder, &["branch", "--list", &branch]).is_empty(),
            "a branch left behind because it was checked out must be removed once it is free"
        );
    }

    #[tokio::test]
    async fn never_moves_the_building_branch_backwards() {
        let fixture = repository().await;
        let (workspaces, home, patch) = (&fixture.workspaces, &fixture.home, fixture.patch);
        workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let coder = home.worktree(Adapter::Coder);

        workspaces
            .prepare(Adapter::Coder, Subject::Patch(patch).into())
            .await
            .expect("coder should take the patch");
        fs::write(coder.join("README.md"), "agent's work\n").expect("README should be written");
        git(&coder, &["add", "--all"]);
        git(
            &coder,
            &[
                "-c",
                "user.email=a@b.c",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "Agent work",
            ],
        );
        let committed = git(&coder, &["rev-parse", "HEAD"]);

        workspaces
            .prepare(Adapter::Coder, Subject::Patch(patch).into())
            .await
            .expect("coder should take it again");

        assert_eq!(
            git(&coder, &["rev-parse", "HEAD"]),
            committed,
            "commits the agent made but had not published must survive the next run"
        );
    }

    fn git(directory: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
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
