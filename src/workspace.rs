//! Reusable working copies supplied to agents.
//!
//! Each role owns one working copy for the lifetime of the member directory, and they share a
//! single Git object store. Moving one to a different issue, patch, or branch costs a checkout
//! rather than a clone, and nothing done in one role's copy is visible in another's.
//!
//! Everything an agent can write is untrusted, and Git decides what to do from what it finds in the
//! repository it is pointed at. The runtime is pointed at repositories an agent can write, so it
//! runs Git inside a sandbox of its own: the working copy the command is for, the shared store, and
//! the one repository it fetches from. Code which reaches a Git command from a role's files has got
//! no further than that role's own files, and cannot reach another role's, the member's identity,
//! or the rest of Radicle storage. External network access is denied, subject to the platform
//! difference in [`NetworkAccess`](crate::sandbox::NetworkAccess).
//!
//! Two rules narrow it further, so the sandbox is the last line rather than the only one. Nothing
//! an agent writes may say where Git looks: the sandbox holds hooks, configuration and every file
//! naming a working copy's Git directory read-only. And nothing it writes may say which reference
//! the runtime updates: every reference written here is one the runtime named itself, addressed in
//! full and set with `--no-deref`, because a name in that namespace can be made to stand for
//! another reference entirely.

use std::{collections::BTreeSet, ffi::OsStr, path::PathBuf, str::FromStr as _, sync::Arc};

use radicle::{
    cob::{
        patch::{self, Patches},
        store::access::ReadOnly,
    },
    git::{Url, UserInfo, Version},
    identity::RepoId,
    storage::{ReadRepository as _, ReadStorage as _, git::Storage},
};
use tokio::sync::{Mutex, OnceCell};

use crate::{
    agent::{Role, Source},
    home::MemberHome,
    member::Member,
    queue::{Task, TaskQueue},
    sandbox::AgentSandbox,
    watcher::Subject,
};

/// First Git which records a linked working copy's location relatively, keeping the member
/// directory movable. Without it Git stores where the directory was created, and renaming it
/// breaks every working copy.
const RELATIVE_WORKTREES: Version = Version {
    major: 2,
    minor: 48,
    patch: 0,
};

/// Returns the branch namespace `member` owns in its own working copies.
///
/// Carries the member's name so branches stay apart in a mirror, and read clearly by hand.
pub(crate) fn branches(member: Member) -> String {
    format!("{}-rt", member.id())
}

// Where a Git command runs, and whose working copy it may write.
//
// The two are not the same. Creating a working copy runs in the shared store but writes the copy,
// so each command says which sandbox it belongs in rather than having it inferred from a path.
#[derive(Debug, Clone, Copy)]
enum In {
    // The shared store, writing no working copy.
    Store,
    // The shared store, creating one role's working copy.
    Creating(Role),
    // One role's working copy.
    Copy(Role),
}

impl In {
    const fn role(self) -> Option<Role> {
        match self {
            Self::Store => None,
            Self::Creating(role) | Self::Copy(role) => Some(role),
        }
    }
}

/// Working copies for one member, one per role, sharing one object store.
#[derive(Debug, Clone)]
pub(crate) struct Workspaces {
    member: Member,
    home: MemberHome,
    storage: Storage,
    repository: RepoId,
    info: UserInfo,
    // Which branches still have work behind them. A branch belongs to one task and lives exactly
    // as long as that task's row does, so the queue is the only thing that decides what may be
    // removed.
    queue: TaskQueue,
    // Serialises fetches and reference updates, which every role performs against the one shared
    // store. Git would refuse a concurrent reference update rather than corrupt anything, so this
    // exists to avoid spurious failures rather than to protect the repository.
    shared: Arc<Mutex<()>>,
    // One per working copy, plus one for the shared store alone. Loaded on first use rather than
    // at construction, because loading reads embedded profiles and creates directories, and a
    // member which never prepares a working copy needs none of them.
    sandboxes: Arc<[OnceCell<AgentSandbox>; Role::ALL.len() + 1]>,
}

impl Workspaces {
    pub(crate) fn new(
        member: Member,
        home: MemberHome,
        storage: Storage,
        repository: RepoId,
        info: UserInfo,
        queue: TaskQueue,
    ) -> Self {
        Self {
            member,
            home,
            storage,
            repository,
            info,
            queue,
            shared: Arc::default(),
            sandboxes: Arc::default(),
        }
    }

    /// Returns the shared object store every working copy reads from.
    #[cfg(test)]
    pub(crate) fn store(&self) -> PathBuf {
        self.home.git_common()
    }

    /// Returns working copies rooted under `target/`, over `storage`.
    ///
    /// Not initialized: a test which needs the copies to exist calls [`Self::initialize`] itself.
    /// `name` separates one test's directory from another's, because the harness runs them at the
    /// same time and one clearing another's would fail whichever lost the race.
    #[cfg(test)]
    pub(crate) fn memory(
        name: &str,
        storage: Storage,
        repository: RepoId,
        info: UserInfo,
        queue: TaskQueue,
    ) -> Result<Self, Error> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(name);
        let _removed = std::fs::remove_dir_all(&root);
        let home = MemberHome::at(root.join("member"));
        home.create()
            .map_err(|error| Error::Sandbox(error.to_string()))?;

        Ok(Self::new(
            Member::Nico,
            home,
            storage,
            repository,
            info,
            queue,
        ))
    }

    /// Creates the working copies and releases branches held by the previous run.
    pub(crate) async fn initialize(&self) -> Result<(), Error> {
        self.create().await?;
        self.detach_copies().await?;

        self.sweep().await
    }

    /// Creates the shared store and role copies without moving existing working copies.
    ///
    /// Setup uses this before preparing the manifest in the coder's copy.
    pub(crate) async fn create(&self) -> Result<(), Error> {
        self.require_relative_worktrees().await?;
        let common = self.home.git_common();
        if !common.join("HEAD").is_file() {
            let manager = self.clone();
            tokio::task::spawn_blocking(move || manager.clone_common())
                .await
                .map_err(|error| Error::Task(error.to_string()))??;
        }
        self.prepare_common().await?;
        for role in Role::ALL {
            let worktree = self.home.worktree(role);
            if !worktree.join(".git").exists() {
                // Relative paths keep the member directory movable. Without them Git records where
                // the directory was created, and renaming it breaks every working copy.
                self.git(
                    In::Creating(role),
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
        Ok(())
    }

    // Takes every working copy off the branch it was left standing on.
    //
    // Nothing is running yet, so holding one would only keep a finished task's branch alive. The
    // tree is untouched, so a crashed run's uncommitted work is still there to salvage.
    async fn detach_copies(&self) -> Result<(), Error> {
        for role in Role::ALL {
            let place = In::Copy(role);
            let head = self.capture(place, ["rev-parse", "HEAD"]).await?;
            self.git(place, ["update-ref", "--no-deref", "HEAD", &head])
                .await?;
        }

        Ok(())
    }

    /// Moves an answering role's working copy onto whatever `source` addresses.
    ///
    /// Left on a detached head: two copies of one repository cannot hold the same branch, and what
    /// an answering role produces is the answer it publishes, not commits.
    pub(crate) async fn prepare(&self, role: Role, source: Source) -> Result<PathBuf, Error> {
        self.settle(role, source, None).await
    }

    /// Moves the building role's working copy onto the branch belonging to `task`.
    ///
    /// Named after the task alone, which is the only identifier both known before the work starts
    /// and never shared with another piece of work.
    pub(crate) async fn prepare_task(&self, task: &Task) -> Result<PathBuf, Error> {
        self.settle(Role::Coder, task.target(), Some(task.id)).await
    }

    /// Verifies the task's published result instead of trusting a successful agent exit.
    pub(crate) async fn verify_completion(&self, task: &Task) -> Result<(), Error> {
        if task.context.cancelled {
            return Ok(());
        }
        if task.kind() == crate::queue::TaskKind::Review {
            let manager = self.clone();
            let task = task.clone();
            return tokio::task::spawn_blocking(move || manager.verify_review(&task))
                .await
                .map_err(|error| Error::Task(error.to_string()))?;
        }
        let dirty = self
            .capture(In::Copy(Role::Coder), ["status", "--porcelain"])
            .await?;
        if !dirty.trim().is_empty() {
            return Err(Error::Unpublished(task.id));
        }
        let head = self
            .capture(In::Copy(Role::Coder), ["rev-parse", "HEAD"])
            .await?;
        let target = task.target();
        let manager = self.clone();
        let ready = tokio::task::spawn_blocking(move || {
            let repository = manager.storage.repository(manager.repository)?;
            let head = radicle::git::Oid::from_str(head.trim()).map_err(Error::Commit)?;
            if let Source::Subject(Subject::Patch(id)) = target {
                let patch = Patches::open(&repository, ReadOnly)?
                    .get(&id)?
                    .ok_or(Error::PatchNotFound(id))?;
                Ok::<_, Error>(
                    *patch.head() == head
                        && matches!(
                            patch.state(),
                            radicle::cob::patch::State::Open { .. }
                                | radicle::cob::patch::State::Merged { .. }
                        ),
                )
            } else {
                let (_, canonical) = repository.canonical_head()?;
                if head == canonical {
                    return Ok(true);
                }
                if !repository.backend.odb()?.exists(head.into()) {
                    return Ok(false);
                }
                Ok(repository
                    .backend
                    .graph_descendant_of(canonical.into(), head.into())?)
            }
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))??;
        if ready {
            Ok(())
        } else {
            Err(Error::Unpublished(task.id))
        }
    }

    fn verify_review(&self, task: &Task) -> Result<(), Error> {
        let Source::Subject(subject @ Subject::Patch(id)) = task.target() else {
            return Err(Error::Unreviewed(task.id));
        };
        let repository = self.storage.repository(self.repository)?;
        let patch = Patches::open(&repository, ReadOnly)?
            .get(&id)?
            .ok_or(Error::PatchNotFound(id))?;
        let activity = task
            .context
            .activity
            .as_ref()
            .filter(|activity| activity.subject == subject);
        let revision = activity
            .and_then(crate::watcher::Activity::review_revision)
            .unwrap_or_else(|| patch.latest().0);
        let review = patch
            .revision(&revision)
            .and_then(|revision| revision.review_by(&self.info.key))
            .ok_or(Error::Unreviewed(task.id))?;
        let Some(activity) = activity else {
            return Ok(());
        };
        if repository
            .backend
            .graph_descendant_of((*review.id()).into(), activity.operation.into())?
        {
            return Ok(());
        }
        // Updating an existing own review also completes a repeated request for that revision.
        for operation in radicle::cob::store::ops(&id, &patch::TYPENAME, &repository)? {
            if operation.author != self.info.key {
                continue;
            }
            for encoded in operation.actions {
                let action: patch::Action =
                    serde_json::from_slice(&encoded).map_err(Error::ReviewAction)?;
                if matches!(action, patch::Action::ReviewEdit(edit) if *edit.review_id() == review.id())
                    && repository
                        .backend
                        .graph_descendant_of(operation.id.into(), activity.operation.into())?
                {
                    return Ok(());
                }
            }
        }
        Err(Error::Unreviewed(task.id))
    }

    async fn settle(
        &self,
        role: Role,
        source: Source,
        task: Option<i64>,
    ) -> Result<PathBuf, Error> {
        let worktree = self.home.worktree(role);
        let head = self.head(source).await?;
        // Held across the whole preparation, not only the fetch. Salvaging and placing a branch
        // both write references into the one shared store, and two roles preparing at once would
        // otherwise collide there.
        let _shared = self.shared.lock().await;
        let place = In::Copy(role);
        self.fetch(place, &head).await?;
        // Placing the branch comes second because resetting while standing on one moves it, which
        // would throw away commits the agent made but had not published yet. It is also what moves
        // the copy off the branch salvaging left it on.
        let head = match task {
            Some(task) => {
                self.salvage(place, task).await?;
                self.place_branch(place, task, &head).await?
            }
            None => head,
        };
        self.git(place, ["reset", "--hard", &head]).await?;
        // Ignored files are removed as well, so nothing a previous delivery left behind can reach
        // the next one. Build output is kept out of the working copy instead, in the member's
        // cache directory.
        self.git(place, ["clean", "-ffdx"]).await?;
        // Last, because moving this working copy is what frees the branch it was standing on.
        // Tidy-up, so a failure is reported rather than allowed to fail a prepared delivery.
        if let Err(error) = self.sweep().await {
            tracing::warn!(%error, "could not remove the branches of finished tasks");
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
    async fn salvage(&self, place: In, task: i64) -> Result<(), Error> {
        if self.status(place).await?.is_empty() {
            return Ok(());
        }
        let branch = self.salvage_branch(place, task).await?;
        let head = self.capture(place, ["rev-parse", "HEAD"]).await?;
        self.stand_on(place, &branch, &head).await?;
        self.git(place, ["add", "--all"]).await?;
        self.git(
            place,
            ["commit", "--no-verify", "-m", "Salvage interrupted work"],
        )
        .await?;
        tracing::warn!(
            role = ?place.role(),
            task,
            %branch,
            "previous run left uncommitted work; it was committed rather than discarded"
        );

        Ok(())
    }

    // Puts the building role on the branch for `task` without losing commits.
    //
    // The branch may already be ahead of the published patch, because the agent commits before it
    // publishes. Forcing the branch back to the patch head would destroy exactly that work.
    async fn place_branch(&self, place: In, task: i64, head: &str) -> Result<String, Error> {
        let branch = self.branch(task);
        let tip = self.tip(place, &branch).await?;
        let target = match tip {
            Some(tip) if self.contains(place, &branch, head).await? => tip,
            Some(tip) => {
                self.copy_diverged(place, task, &tip).await?;
                head.to_owned()
            }
            None => head.to_owned(),
        };
        self.stand_on(place, &branch, &target).await?;

        Ok(target)
    }

    // Keeps a copy of a branch which has drifted away from the published patch.
    async fn copy_diverged(&self, place: In, task: i64, tip: &str) -> Result<(), Error> {
        let salvage = self.salvage_branch(place, task).await?;
        self.set_reference(place, &salvage, tip).await?;
        tracing::warn!(
            task,
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
    // Every reference the runtime writes is one it named itself and set directly. A role can write
    // inside its own namespace, so a name there may stand for another reference entirely, and
    // updating through it would move whatever it stands for rather than the branch.
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

    /// Removes every branch whose task is over.
    ///
    /// The queue decides: a task which finished has had its row removed, one which failed has not.
    /// A branch a working copy still stands on waits for the next preparation.
    async fn sweep(&self) -> Result<(), Error> {
        let present = self.namespace().await?;
        if present.is_empty() {
            return Ok(());
        }
        let live = self
            .queue
            .live()
            .await
            .map_err(|error| Error::Queue(error.to_string()))?;
        let held = self.checked_out().await?;
        for branch in present {
            // Only names this runtime gave a task. A salvage branch is what a failed run left
            // behind and is the one copy of work nobody published, and anything else under this
            // namespace was not put there by the runtime at all.
            let Some(task) = self.task_of(&branch) else {
                continue;
            };
            if !live.contains(&task) && !held.contains(&branch) {
                self.remove_branch(&branch).await?;
            }
        }

        Ok(())
    }

    // Reads back the task a branch was named after, and nothing for any other name.
    //
    // The inverse of [`Self::branch`].
    fn task_of(&self, branch: &str) -> Option<i64> {
        branch
            .strip_prefix(&self.branches())?
            .strip_prefix("/task-")?
            .parse()
            .ok()
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
                    &Self::reference(&self.branches()),
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
    // One name per task would let a second crash overwrite what the first one preserved, which is
    // the failure this whole path exists to prevent.
    async fn salvage_branch(&self, place: In, task: i64) -> Result<String, Error> {
        let stem = format!("{}/salvage/{task}", self.branches());
        let taken = self
            .capture(
                place,
                [
                    "for-each-ref",
                    "--format=%(refname:lstrip=5)",
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

    fn branches(&self) -> String {
        branches(self.member)
    }

    fn branch(&self, task: i64) -> String {
        format!("{}/task-{task}", self.branches())
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
        let namespace = self.branches();
        tokio::task::spawn_blocking(move || {
            let mut config = git2::Config::open(&common.join("config"))?;
            config.set_str("user.name", info.name().as_ref())?;
            config.set_str("user.email", &info.email())?;
            config.set_i32("gc.auto", 0)?;
            std::fs::create_dir_all(common.join("refs").join("heads").join(&namespace))
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
            In::Copy(role) => self.home.worktree(role),
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
    // One per working copy, plus one for commands which touch no working copy at all, so a command
    // run for one role cannot write another's files. Each is loaded once.
    async fn sandbox(&self, place: In) -> Result<&AgentSandbox, Error> {
        let role = place.role();
        let cell = self
            .sandboxes
            .get(role.map_or(Role::ALL.len(), |role| role as usize))
            .ok_or_else(|| Error::Sandbox("no sandbox for this working copy".to_owned()))?;

        cell.get_or_try_init(|| async {
            AgentSandbox::runtime(&self.home, role, &self.repository_path())
                .map_err(|error| Error::Sandbox(error.to_string()))
        })
        .await
    }

    /// Returns where this member's Radicle storage keeps the repository being worked on.
    fn repository_path(&self) -> PathBuf {
        self.storage.path().join(self.repository.canonical())
    }
}

/// Error preparing or using a member's working copies.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A review task cannot complete with only a reaction or an internal summary.
    #[error(
        "task {0} still needs your published review for the requested revision. Use create_review \
         or edit_review before finishing"
    )]
    Unreviewed(i64),
    /// A stored review operation could not be decoded.
    #[error("invalid stored review action: {0}")]
    ReviewAction(#[source] serde_json::Error),
    /// Durable edits must be committed and published before implementation is completed.
    #[error(
        "task {0} still has unpublished changes or a draft patch; commit and publish with \
         update_patch, then open the patch before claiming completion"
    )]
    Unpublished(i64),
    /// A checked-out commit identifier could not be read.
    #[error("invalid workspace commit: {0}")]
    Commit(#[source] radicle::git::ParseOidError),
    /// A workspace filesystem or subprocess operation failed.
    #[error("workspace I/O failed: {0}")]
    Io(#[source] std::io::Error),
    /// Preparing a sandboxed Git command failed.
    #[error("workspace sandbox failed: {0}")]
    Sandbox(String),
    /// A Git subprocess failed.
    #[error("git {command} failed: {message}")]
    Git {
        /// Git arguments for the failed command.
        command: String,
        /// Diagnostic output from Git.
        message: String,
    },
    /// Creating a Radicle checkout failed.
    #[error("workspace checkout failed: {0}")]
    Checkout(#[from] radicle::rad::CheckoutError),
    /// A Git library operation failed.
    #[error("workspace Git library call failed: {0}")]
    LibGit(#[from] git2::Error),
    /// Opening or validating the stored Radicle repository failed.
    #[error("workspace repository failed: {0}")]
    Repository(#[from] radicle::storage::RepositoryError),
    /// No delegate namespace provides the canonical branch.
    #[error("canonical branch is not available from a delegate namespace")]
    CanonicalBranchRemote,
    /// Reading a collaborative object failed.
    #[error("workspace collaborative object failed: {0}")]
    Cob(#[from] radicle::cob::store::Error),
    /// The requested patch does not exist.
    #[error("patch {0} was not found")]
    PatchNotFound(radicle::cob::ObjectId),
    /// A blocking workspace task failed.
    #[error("workspace task failed: {0}")]
    Task(String),
    /// Reading the member's task queue failed.
    #[error("workspace could not read the task queue: {0}")]
    Queue(String),
    /// Git is too old to create relocatable working copies.
    #[error(
        "Git 2.48 or newer is required so working copies survive moving the member directory, \
         found '{0}'"
    )]
    GitTooOld(String),
    /// No unused branch was available for preserving interrupted work.
    #[error("could not find an unused salvage branch under '{0}'")]
    SalvageExhausted(String),
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        str::FromStr as _,
    };

    use radicle::{
        cob::{
            Title,
            patch::{self, MergeTarget},
        },
        crypto::Seed,
        identity::doc::Visibility,
        node::Alias,
        profile::{Home, Profile},
        storage::ReadStorage as _,
    };

    use crate::{
        agent::{Role, Source},
        home::MemberHome,
        manifest::QueuePosition,
        member::Member,
        queue::{Task, TaskQueue},
        watcher::Subject,
        workspace::Workspaces,
    };

    // The namespace every branch in these tests lives under, named after the member the fixture
    // builds. Spelled out rather than derived, so a change to the naming has to be seen here.
    const NAMESPACE: &str = "nico-rt";

    struct Fixture {
        _root: Option<tempfile::TempDir>,
        profile: Profile,
        repository: radicle::identity::RepoId,
        workspaces: Workspaces,
        queue: TaskQueue,
        home: MemberHome,
        patch: radicle::cob::ObjectId,
        issue: radicle::cob::ObjectId,
        head: git2::Oid,
    }

    impl Fixture {
        // Records a task for `source` and starts it, the way the builder would.
        async fn start(&self, source: impl Into<Source>) -> Task {
            let _id = self
                .queue
                .push(crate::queue::TaskRequest::new(
                    "do it".to_owned(),
                    source.into(),
                    QueuePosition::End,
                ))
                .await
                .expect("task should be recorded")
                .task;

            self.queue.next().await.expect("task should be taken")
        }
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
        let queue = TaskQueue::memory(repository, Member::Nico)
            .await
            .expect("queue should open");
        let workspaces = Workspaces::new(
            Member::Nico,
            home.clone(),
            profile.storage.clone(),
            repository,
            info,
            queue.clone(),
        );

        Fixture {
            _root: temporary,
            profile,
            repository,
            workspaces,
            queue,
            home,
            patch,
            issue,
            head,
        }
    }

    #[tokio::test]
    async fn moves_one_role_without_disturbing_the_others() {
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
        for role in Role::ALL {
            assert!(
                home.worktree(role).join("README.md").is_file(),
                "{role} should have real files, not a pointer to somebody else's"
            );
        }
        assert!(
            home.git_common().join("HEAD").is_file(),
            "the shared object store must hold the history once"
        );

        let talker = workspaces
            .prepare(Role::Talker, Subject::Patch(patch).into())
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
            home.worktree(Role::Chatter).join("untouched.txt"),
            "chatter's own file",
        )
        .expect("chatter file should be written");
        workspaces
            .prepare(Role::Talker, Subject::Issue(patch).into())
            .await
            .expect("talker should move again");
        assert!(
            home.worktree(Role::Chatter).join("untouched.txt").is_file(),
            "moving one role must not disturb another's working copy"
        );

        fs::write(talker.join("ignored.log"), "left over").expect("leftover should be written");
        fs::write(talker.join("README.md"), "dirty").expect("README should be dirtied");
        let talker = workspaces
            .prepare(Role::Talker, Subject::Issue(patch).into())
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
        let (workspaces, home, head) = (&fixture.workspaces, &fixture.home, fixture.head);
        workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let coder = home.worktree(Role::Coder);
        let task = fixture.start(Subject::Patch(fixture.patch)).await;

        workspaces
            .prepare_task(&task)
            .await
            .expect("coder should take the patch");
        fs::write(coder.join("half-done.rs"), "unfinished").expect("file should be written");

        workspaces
            .prepare_task(&task)
            .await
            .expect("coder should take it again");

        assert!(
            !coder.join("half-done.rs").exists(),
            "the next run must start from a clean tree"
        );
        let salvaged = git(
            &coder,
            &["branch", "--list", &format!("{NAMESPACE}/salvage/*")],
        );
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
        let talker = home.worktree(Role::Talker);

        workspaces
            .prepare(Role::Talker, Subject::Patch(patch).into())
            .await
            .expect("talker should take the patch");
        fs::write(talker.join("scratch.md"), "checking something").expect("file should be written");
        fs::write(talker.join("README.md"), "edited while reading").expect("README should change");

        workspaces
            .prepare(Role::Talker, Subject::Patch(patch).into())
            .await
            .expect("talker should take it again");

        assert!(
            !talker.join("scratch.md").exists(),
            "the next answer must start from a clean tree"
        );
        assert!(
            git(
                &talker,
                &["branch", "--list", &format!("{NAMESPACE}/salvage/*")]
            )
            .is_empty(),
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
            common.join("refs").join("heads").join(NAMESPACE),
            common.join("worktrees").join("talker").join("commondir"),
            common.join("worktrees").join("talker").join("gitdir"),
            fixture.home.worktree(Role::Talker).join(".git"),
        ] {
            assert!(
                path.exists(),
                "{} must exist before any agent starts",
                path.display()
            );
        }
    }

    // Fencing off each surface a role could use to smuggle code into a Git command is guesswork,
    // and one missed surface would be a way out of the sandbox entirely. This is the bound on what
    // a missed one costs: the runtime's own Git can reach the working copies and nothing else, so
    // code arriving through it gets no further than its role already was.
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
        // The command below is run for the talking copy, so the building one is another role's
        // private state and out of reach even though both live side by side. Its files and the
        // entry the shared store keeps for it are the same thing: that entry holds its index.
        let others = fixture.home.worktree(Role::Coder).join("others.txt");
        let metadata = fixture
            .home
            .git_common()
            .join("worktrees")
            .join(Role::Coder.id())
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
            .prepare(Role::Talker, Subject::Patch(fixture.patch).into())
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
            "nor another role's working copy, which the command it rode in on was not for"
        );
        assert_ne!(
            fs::read_to_string(&metadata).unwrap_or_default(),
            "x\n",
            "nor that copy's index, which is the same thing said another way"
        );
    }

    // A working copy belongs to its role, so a name in the runtime's own branch namespace can be
    // made to stand for another reference entirely. Every reference the runtime writes is therefore
    // set directly: updating through this one would move the canonical branch instead.
    #[tokio::test]
    async fn never_writes_through_a_reference_a_role_left_behind() {
        let fixture = repository().await;
        let coder = fixture.home.worktree(Role::Coder);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let task = fixture.start(Subject::Patch(fixture.patch)).await;
        // Somewhere the runtime is about to move its own branch away from: the reference below
        // stands for this one, so following it would carry this one along.
        let base = git(&coder, &["rev-parse", &format!("{}~1", fixture.head)]);
        git(&coder, &["update-ref", "refs/heads/victim", &base]);
        git(
            &coder,
            &[
                "symbolic-ref",
                &format!("refs/heads/{NAMESPACE}/task-{}", task.id),
                "refs/heads/victim",
            ],
        );

        fixture
            .workspaces
            .prepare_task(&task)
            .await
            .expect("coder should take the patch");

        assert_eq!(
            git(&coder, &["rev-parse", "refs/heads/victim"]),
            base,
            "a reference outside the runtime's namespace must not move because a role pointed a \
             branch inside it at that reference"
        );
    }

    // Two tasks from one issue, or one chat channel, are two separate pieces of work. Naming a
    // branch after where the request came from would put both on the same branch, where the second
    // silently continues from whatever the first left.
    #[tokio::test]
    async fn gives_two_tasks_from_one_source_a_branch_each() {
        let fixture = repository().await;
        let coder = fixture.home.worktree(Role::Coder);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let first = fixture.start(Subject::Issue(fixture.issue)).await;
        fixture
            .workspaces
            .prepare_task(&first)
            .await
            .expect("coder should take the first task");
        fixture
            .queue
            .finish()
            .await
            .expect("the first task should finish");
        let second = fixture.start(Subject::Issue(fixture.issue)).await;

        fixture
            .workspaces
            .prepare_task(&second)
            .await
            .expect("coder should take the second task");

        assert_ne!(first.id, second.id);
        assert_eq!(
            git(&coder, &["rev-parse", "--abbrev-ref", "HEAD"]),
            format!("{NAMESPACE}/task-{}", second.id),
            "the second task must stand on a branch of its own"
        );
    }

    // The patch a task produces does not exist when the task starts, so a branch named after the
    // patch could only ever be reached by renaming one. Naming it after the task means the name is
    // right from the first commit and never has to move.
    #[tokio::test]
    async fn keeps_one_branch_name_from_the_first_commit_to_the_patch() {
        let fixture = repository().await;
        let coder = fixture.home.worktree(Role::Coder);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let task = fixture.start(Subject::Issue(fixture.issue)).await;
        fixture
            .workspaces
            .prepare_task(&task)
            .await
            .expect("coder should take the issue");
        let branch = git(&coder, &["rev-parse", "--abbrev-ref", "HEAD"]);

        // What the agent does when it publishes: the task is now answering a patch instead of the
        // issue it came from.
        fixture
            .queue
            .produced(task.id, Subject::Patch(fixture.patch))
            .await
            .expect("the task should retarget");
        let task = fixture
            .queue
            .running()
            .await
            .expect("the task should still be running");
        fixture
            .workspaces
            .prepare_task(&task)
            .await
            .expect("coder should carry on");

        assert_eq!(
            git(&coder, &["rev-parse", "--abbrev-ref", "HEAD"]),
            branch,
            "producing a patch must not move the task onto another branch"
        );
        assert_eq!(branch, format!("{NAMESPACE}/task-{}", task.id));
    }

    // A branch is the working copy of one task and means nothing once that task is over. The queue
    // is what says so: a task which finished has had its row removed, and one which failed has
    // not.
    #[tokio::test]
    async fn removes_the_branch_of_a_task_which_finished() {
        let fixture = repository().await;
        let coder = fixture.home.worktree(Role::Coder);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let task = fixture.start(Subject::Issue(fixture.issue)).await;
        let branch = format!("{NAMESPACE}/task-{}", task.id);
        fixture
            .workspaces
            .prepare_task(&task)
            .await
            .expect("coder should take the issue");
        fixture
            .queue
            .finish()
            .await
            .expect("the task should finish");
        assert!(!git(&coder, &["branch", "--list", &branch]).is_empty());

        let next = fixture.start(Subject::Patch(fixture.patch)).await;
        fixture
            .workspaces
            .prepare_task(&next)
            .await
            .expect("coder should move to other work");

        assert!(
            git(&coder, &["branch", "--list", &branch]).is_empty(),
            "the branch of a task which is over must be removed"
        );
    }

    // A task which stalled, failed, or was interrupted keeps its row, and its branch holds commits
    // nobody published. Removing it would throw that work away with nothing to recover it from.
    #[tokio::test]
    async fn keeps_the_branch_of_a_task_which_did_not_finish() {
        let fixture = repository().await;
        let coder = fixture.home.worktree(Role::Coder);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let failed = fixture.start(Subject::Issue(fixture.issue)).await;
        let branch = format!("{NAMESPACE}/task-{}", failed.id);
        fixture
            .workspaces
            .prepare_task(&failed)
            .await
            .expect("coder should take the issue");

        // No `finish`, which is what the builder skips when a task fails.
        let next = fixture.start(Subject::Patch(fixture.patch)).await;
        fixture
            .workspaces
            .prepare_task(&next)
            .await
            .expect("coder should move to other work");

        assert!(
            !git(&coder, &["branch", "--list", &branch]).is_empty(),
            "work a failed task left behind must survive until that task is actually carried out"
        );
    }

    // A task finishing while its branch is checked out is noticed nowhere but in memory, so a
    // restart would otherwise leave that branch behind forever.
    #[tokio::test]
    async fn removes_the_branch_of_a_finished_task_after_a_restart() {
        let fixture = repository().await;
        let coder = fixture.home.worktree(Role::Coder);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let task = fixture.start(Subject::Patch(fixture.patch)).await;
        let branch = format!("{NAMESPACE}/task-{}", task.id);
        fixture
            .workspaces
            .prepare_task(&task)
            .await
            .expect("coder should take the patch");
        fixture
            .queue
            .finish()
            .await
            .expect("the task should finish");
        assert!(!git(&coder, &["branch", "--list", &branch]).is_empty());

        // A different value, holding nothing of what the run before it was waiting to remove.
        let restarted = Workspaces::new(
            Member::Nico,
            fixture.home.clone(),
            fixture.profile.storage.clone(),
            fixture.repository,
            fixture.profile.info(),
            fixture.queue.clone(),
        );
        restarted
            .initialize()
            .await
            .expect("the next start should reuse the working copies");

        assert!(
            git(&coder, &["branch", "--list", &branch]).is_empty(),
            "the branch of a task which is over must be removed at the next start"
        );
    }

    #[tokio::test]
    async fn keeps_a_finished_branch_until_the_copy_holding_it_moves_on() {
        let fixture = repository().await;
        let coder = fixture.home.worktree(Role::Coder);
        fixture
            .workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let task = fixture.start(Subject::Patch(fixture.patch)).await;
        let branch = format!("{NAMESPACE}/task-{}", task.id);
        fixture
            .workspaces
            .prepare_task(&task)
            .await
            .expect("coder should take the patch");
        assert_eq!(git(&coder, &["rev-parse", "--abbrev-ref", "HEAD"]), branch);

        // Finishing does not move the copy off the branch, and the sweep runs anyway.
        fixture
            .queue
            .finish()
            .await
            .expect("the task should finish");
        fixture
            .workspaces
            .prepare(Role::Talker, Subject::Patch(fixture.patch).into())
            .await
            .expect("another role should still be able to prepare");

        assert!(
            !git(&coder, &["branch", "--list", &branch]).is_empty(),
            "tidying up must not pull the branch out from under a copy still standing on it"
        );

        let next = fixture.start(Subject::Issue(fixture.issue)).await;
        fixture
            .workspaces
            .prepare_task(&next)
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
        let (workspaces, home) = (&fixture.workspaces, &fixture.home);
        workspaces
            .initialize()
            .await
            .expect("working copies should be created");
        let coder = home.worktree(Role::Coder);
        let task = fixture.start(Subject::Patch(fixture.patch)).await;

        workspaces
            .prepare_task(&task)
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
            .prepare_task(&task)
            .await
            .expect("coder should take it again");

        assert_eq!(
            git(&coder, &["rev-parse", "HEAD"]),
            committed,
            "commits the agent made but had not published must survive the next run"
        );
    }

    #[tokio::test]
    async fn implementation_finishes_only_with_clean_published_ready_changes() {
        use crate::workspace::Error;
        let fixture = repository().await;
        let workspaces = &fixture.workspaces;
        workspaces.initialize().await.expect("workspaces");
        let mut task = fixture.start(Subject::Issue(fixture.issue)).await;
        let coder = workspaces.prepare_task(&task).await.expect("checkout");
        workspaces
            .verify_completion(&task)
            .await
            .expect("unchanged task");
        fs::write(coder.join("README.md"), "unpublished fix\n").expect("local edit");
        assert!(matches!(
            workspaces.verify_completion(&task).await,
            Err(Error::Unpublished(_))
        ));
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
                "Local fix",
            ],
        );
        assert!(matches!(
            workspaces.verify_completion(&task).await,
            Err(Error::Unpublished(_))
        ));
        fixture
            .queue
            .produced(task.id, Subject::Patch(fixture.patch))
            .await
            .expect("patch relation");
        task = fixture.queue.get(task.id).await.expect("current context");
        assert!(
            matches!(
                workspaces.verify_completion(&task).await,
                Err(Error::Unpublished(_))
            ),
            "a patch link cannot make a local unpublished commit complete"
        );
        git(&coder, &["reset", "--hard", &fixture.head.to_string()]);
        workspaces
            .verify_completion(&task)
            .await
            .expect("published open patch");
        let stored = fixture
            .profile
            .storage
            .repository(fixture.repository)
            .expect("storage");
        let signer = fixture.profile.signer().expect("signer");
        let mut patches = fixture
            .profile
            .patches_mut(&stored, &signer)
            .expect("patches");
        let mut patch = patches.get_mut(&fixture.patch).expect("patch");
        patch.unready().expect("draft");
        assert!(
            matches!(
                workspaces.verify_completion(&task).await,
                Err(Error::Unpublished(_))
            ),
            "a draft must remain unfinished"
        );
        patch.ready().expect("ready");
        workspaces
            .verify_completion(&task)
            .await
            .expect("ready publication");
        fs::write(coder.join("README.md"), "disposable review experiment\n").expect("experiment");
        task.context.kind = crate::queue::TaskKind::Review;
        assert!(matches!(
            workspaces.verify_completion(&task).await,
            Err(Error::Unreviewed(_))
        ));
    }

    #[tokio::test]
    async fn review_assignment_finishes_only_after_own_publication_for_the_requested_revision() {
        use std::collections::BTreeSet;

        use crate::{
            queue::TaskKind,
            watcher::{Activity, Kind},
            workspace::Error,
        };
        let fixture = repository().await;
        let reviewer = Profile::init(
            Home::new(fixture.profile.home.path().join("reviewer")).expect("reviewer home"),
            Alias::new("reviewer"),
            None,
            Seed::generate(),
        )
        .expect("reviewer identity");
        let mut workspaces = fixture.workspaces.clone();
        workspaces.info = reviewer.info();
        workspaces.initialize().await.expect("workspaces");
        let mut task = fixture.start(Subject::Patch(fixture.patch)).await;
        task.context.kind = TaskKind::Review;
        let checkout = workspaces
            .prepare_task(&task)
            .await
            .expect("review checkout");
        let stored = fixture
            .profile
            .storage
            .repository(fixture.repository)
            .expect("storage");
        let owner_signer = fixture.profile.signer().expect("owner signer");
        let reviewer_signer = reviewer.signer().expect("reviewer signer");
        let assignment = {
            let mut patches = patch::Cache::no_cache(&stored, &owner_signer).expect("patches");
            let mut object = patches.get_mut(&fixture.patch).expect("patch");
            let assignment = object
                .assign(BTreeSet::from([reviewer.did()]))
                .expect("assignment");
            object
                .review(
                    object.latest().0,
                    Some(patch::Verdict::Accept),
                    None,
                    vec![],
                )
                .expect("another author's review");
            assignment
        };
        task.context.activity = Some(Activity {
            repository: fixture.repository,
            subject: Subject::Patch(fixture.patch),
            operation: assignment,
            action: 0,
            author: fixture.profile.did(),
            kind: Kind::Assigned,
        });
        assert!(
            matches!(
                workspaces.verify_completion(&task).await,
                Err(Error::Unreviewed(_))
            ),
            "another author's review cannot finish this assignment"
        );
        {
            let mut patches =
                patch::Cache::no_cache(&stored, &reviewer_signer).expect("reviewer patches");
            let mut object = patches.get_mut(&fixture.patch).expect("patch");
            object
                .react(
                    object.latest().0,
                    "👀".parse().expect("reaction"),
                    None,
                    true,
                )
                .expect("queue receipt");
        }
        assert!(
            matches!(
                workspaces.verify_completion(&task).await,
                Err(Error::Unreviewed(_))
            ),
            "a receipt does not complete the review"
        );
        {
            let mut patches =
                patch::Cache::no_cache(&stored, &reviewer_signer).expect("reviewer patches");
            let mut object = patches.get_mut(&fixture.patch).expect("patch");
            object
                .review(
                    object.latest().0,
                    Some(patch::Verdict::Accept),
                    Some("Behavior checked".into()),
                    vec![],
                )
                .expect("own review");
        }
        fs::write(checkout.join("README.md"), "disposable verification edit\n")
            .expect("experiment");
        workspaces
            .verify_completion(&task)
            .await
            .expect("published review completes without publishing local experiments");
        let revision = {
            let mut patches =
                patch::Cache::no_cache(&stored, &owner_signer).expect("owner patches");
            let mut object = patches.get_mut(&fixture.patch).expect("patch");
            object
                .update("New revision", *object.base(), fixture.head)
                .expect("revision")
        };
        assert!(
            matches!(
                workspaces.verify_completion(&task).await,
                Err(Error::Unreviewed(_))
            ),
            "an older revision's review cannot finish a current assignment"
        );
        let own = {
            let mut patches =
                patch::Cache::no_cache(&stored, &reviewer_signer).expect("reviewer patches");
            let mut object = patches.get_mut(&fixture.patch).expect("patch");
            object
                .review(
                    revision,
                    Some(patch::Verdict::Accept),
                    Some("New revision checked".into()),
                    vec![],
                )
                .expect("current own review")
        };
        workspaces
            .verify_completion(&task)
            .await
            .expect("current review");
        let reassigned = {
            let mut patches =
                patch::Cache::no_cache(&stored, &owner_signer).expect("owner patches");
            let mut object = patches.get_mut(&fixture.patch).expect("patch");
            object
                .assign(BTreeSet::from([reviewer.did()]))
                .expect("repeated assignment")
        };
        task.context
            .activity
            .as_mut()
            .expect("assignment")
            .operation = reassigned;
        assert!(
            matches!(
                workspaces.verify_completion(&task).await,
                Err(Error::Unreviewed(_))
            ),
            "an old review cannot silently finish a renewed request"
        );
        {
            let mut patches =
                patch::Cache::no_cache(&stored, &reviewer_signer).expect("reviewer patches");
            let mut object = patches.get_mut(&fixture.patch).expect("patch");
            object
                .review_edit(
                    own,
                    Some(patch::Verdict::Accept),
                    "Rechecked on request".into(),
                    vec![],
                    [],
                )
                .expect("refresh own review");
        }
        workspaces
            .verify_completion(&task)
            .await
            .expect("published edit also completes renewed review work");
        task.context.activity = None;
        workspaces
            .verify_completion(&task)
            .await
            .expect("an authenticated chat review can use its published review");
        task.context.cancelled = true;
        workspaces
            .verify_completion(&task)
            .await
            .expect("explicit cancellation");
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
