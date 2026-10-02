//! Team member filesystem locations.
//!
//! One member working in one repository owns one directory, created where `setup` was run rather
//! than hidden under the user's home. Everything that member needs lives inside it: its signing
//! identity, its durable state, one working copy per role, and a disposable build cache. There is
//! no global layer, so a directory can be moved, backed up, or deleted as a unit.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
};

use radicle::identity::RepoId;
use serde::{Deserialize, Serialize};

use crate::{agent::Role, member::Member};

/// Environment variable naming the member directory.
pub const ENVIRONMENT_VARIABLE: &str = "RADICLE_TEAM_HOME";

/// File identifying a directory as one member's home.
///
/// Distinct from [`crate::manifest::FILE_NAME`], and named distinctly on purpose:
/// [`MemberHome::discover`] walks up past repository content, which must never shadow it.
pub const MARKER: &str = "radicle-team-member.toml";

/// Filesystem locations owned by one team member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberHome {
    path: PathBuf,
}

impl MemberHome {
    /// Locates the member directory for a command invoked without an explicit path.
    ///
    /// Uses [`ENVIRONMENT_VARIABLE`] when set, otherwise searches upwards for a readable
    /// [`MARKER`], the way `git` finds a repository.
    ///
    /// # Errors
    ///
    /// Returns an error when no member directory is found.
    pub fn discover() -> Result<Self, Error> {
        if let Some(path) = env::var_os(ENVIRONMENT_VARIABLE).filter(|value| !value.is_empty()) {
            return Ok(Self::at(PathBuf::from(path)));
        }
        let start = env::current_dir().map_err(Error::WorkingDirectory)?;

        Self::find(&start)
    }

    /// Returns the member directory at `start` or above it.
    ///
    /// # Errors
    ///
    /// Returns an error when neither `start` nor any of its parents is a member directory.
    pub fn find(start: &Path) -> Result<Self, Error> {
        for directory in start.ancestors() {
            let home = Self::at(directory);
            if home.marker().is_ok() {
                return Ok(home);
            }
        }

        Err(Error::NotFound {
            start: start.to_path_buf(),
        })
    }

    /// Returns whether this directory already holds a member's Radicle identity.
    ///
    /// The marker is written first, so an identity without one means the marker was lost.
    #[must_use]
    pub fn holds_identity(&self) -> bool {
        self.identity().exists()
    }

    /// Uses `path` as the member directory.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the directory named after a project and a member.
    ///
    /// Not yet what `setup` creates: the project's name is not known until the repository has been
    /// fetched, so `Application::default_setup_home` names the directory after the repository
    /// instead. This is the naming to move to once that name is available.
    #[must_use]
    pub fn for_project(parent: impl AsRef<Path>, project: &str, member: Member) -> Self {
        Self::at(parent.as_ref().join(format!("{project}-{}", member.id())))
    }

    /// Returns the member directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates the member directory and its fixed subdirectories with private permissions.
    ///
    /// # Errors
    ///
    /// Returns an error when a directory cannot be created or secured.
    pub fn create(&self) -> Result<(), Error> {
        let owned = [
            self.path.clone(),
            self.code(),
            self.cache(),
            self.harnesses(),
            self.harnesses().join(".providers"),
        ]
        .into_iter()
        .chain(
            Role::ALL
                .into_iter()
                .flat_map(|role| [self.harness(role), self.instructions(role)]),
        );
        for directory in owned {
            fs::create_dir_all(&directory).map_err(Error::Create)?;

            #[cfg(unix)]
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .map_err(Error::Create)?;
        }

        Ok(())
    }

    /// Records which member and repository this directory belongs to.
    ///
    /// # Errors
    ///
    /// Returns an error when the marker cannot be written.
    pub fn record(&self, member: Member, repository: RepoId) -> Result<(), Error> {
        self.write_marker(member, Some(repository))
    }

    /// Records an identity's owner before a repository is selected.
    ///
    /// # Errors
    ///
    /// Returns an error when the marker cannot be written.
    pub fn record_identity(&self, member: Member) -> Result<(), Error> {
        self.write_marker(member, None)
    }

    /// Reads the marker recorded when this directory was created.
    ///
    /// # Errors
    ///
    /// Returns an error when the marker is missing, unreadable, or malformed.
    pub fn marker(&self) -> Result<Marker, Error> {
        let path = self.marker_path();
        let body = fs::read_to_string(&path).map_err(|source| Error::MarkerUnreadable {
            path: path.clone(),
            source,
        })?;

        let marker: Marker =
            toml_edit::de::from_str(&body).map_err(|error| Error::MarkerMalformed {
                path: path.clone(),
                reason: error.to_string(),
            })?;
        if marker.repository.is_none() && !marker.identity_only {
            return Err(Error::MarkerMalformed {
                path,
                reason: "an unbound member marker must declare identity_only = true".to_owned(),
            });
        }
        Ok(marker)
    }

    /// Returns the member's Radicle home directory.
    #[must_use]
    pub fn identity(&self) -> PathBuf {
        self.path.join("identity")
    }

    /// Returns the member's durable state database path.
    #[must_use]
    pub fn state_database(&self) -> PathBuf {
        self.path.join("state.db")
    }

    /// Returns the member's IRC client identity path.
    #[must_use]
    pub fn chat_identity(&self) -> PathBuf {
        self.path.join("irc-identity.pem")
    }

    /// Returns the member process lock path.
    #[must_use]
    pub fn process_lock(&self) -> PathBuf {
        self.path.join("process.lock")
    }

    /// Returns the managed Radicle node log path.
    #[must_use]
    pub fn node_log(&self) -> PathBuf {
        self.path.join("node.log")
    }

    /// Returns the local agent sandbox configuration path.
    #[must_use]
    pub fn sandbox_config(&self) -> PathBuf {
        self.path.join("sandbox.toml")
    }

    /// Returns the directory holding every working copy.
    #[must_use]
    pub fn code(&self) -> PathBuf {
        self.path.join("code")
    }

    /// Returns the shared Git object store backing every working copy.
    #[must_use]
    pub fn git_common(&self) -> PathBuf {
        self.code().join(".git-common")
    }

    /// Returns one role's working copy.
    #[must_use]
    pub fn worktree(&self, role: Role) -> PathBuf {
        self.code().join(role.id())
    }

    /// Returns the disposable build cache shared by this member's roles.
    ///
    /// Shared because build output is too large to keep three copies of, and safe to share because
    /// the three roles are one person under one key. Never shared between members: a build cache
    /// holds compiled build scripts which the next build runs. Removing it must always be safe.
    #[must_use]
    pub fn cache(&self) -> PathBuf {
        self.path.join("cache")
    }

    /// Returns the root for agent harness state.
    #[must_use]
    pub fn harnesses(&self) -> PathBuf {
        self.path.join("harness")
    }

    /// Returns native provider state shared by this member's roles.
    #[must_use]
    pub fn provider_state(&self, adapter: &str) -> PathBuf {
        self.harnesses().join(".providers").join(adapter)
    }

    /// Returns where one role's standing instructions are written.
    #[must_use]
    pub fn instructions(&self, role: Role) -> PathBuf {
        self.harness(role).join("instructions")
    }

    /// Returns one role's harness home.
    ///
    /// An agent harness keeps its settings and logs in a user-level directory of its own by
    /// default, which every member on the machine would then share. This is given to it instead, so
    /// removing the member directory removes that state rather than orphaning it.
    #[must_use]
    pub fn harness(&self, role: Role) -> PathBuf {
        self.harnesses().join(role.id())
    }

    fn marker_path(&self) -> PathBuf {
        self.path.join(MARKER)
    }

    fn write_marker(&self, member: Member, repository: Option<RepoId>) -> Result<(), Error> {
        let marker = Marker {
            member: member.id().to_owned(),
            identity_only: repository.is_none(),
            repository: repository.map(|repository| repository.urn()),
        };
        let body = toml_edit::ser::to_string_pretty(&marker)
            .map_err(|source| Error::MarkerUnwritable(source.to_string()))?;

        fs::write(self.marker_path(), body).map_err(Error::Create)
    }
}

/// Identity recorded in a member directory when it was created.
///
/// This is local state, not the team's manifest. The manifest lists every member of a repository
/// and lives in that repository; this says which one of them owns the directory it is in, which
/// nothing in the repository can know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// Fixed name of the member owning the directory.
    pub member: String,
    /// Repository the member works in, absent until full setup binds the identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Marks an intentionally prepared identity whose repository has not yet been selected.
    #[serde(default)]
    pub identity_only: bool,
}

impl Marker {
    /// Returns the member this directory belongs to.
    ///
    /// # Errors
    ///
    /// Returns an error when the recorded name is not a known team member.
    pub fn member(&self) -> Result<Member, crate::member::UnknownMember> {
        Member::from_str(&self.member)
    }

    /// Returns the repository this directory belongs to, if full setup has selected one.
    ///
    /// # Errors
    ///
    /// Returns an error when the recorded identifier is not a valid repository.
    pub fn repository(&self) -> Result<Option<RepoId>, radicle::identity::IdError> {
        self.repository.as_deref().map(RepoId::from_urn).transpose()
    }
}

/// Error locating or preparing a member directory.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The current directory could not be read.
    #[error("could not read the current directory: {0}")]
    WorkingDirectory(#[source] io::Error),
    /// No member directory was found.
    #[error(
        "no member directory found in '{start}' or its parents; run `rad-team setup` to create \
         one, or pass --home PATH"
    )]
    NotFound {
        /// Directory the search started from.
        start: PathBuf,
    },
    /// The member directory could not be created.
    #[error("could not create the member directory: {0}")]
    Create(#[source] io::Error),
    /// The marker could not be read.
    #[error("could not read '{path}': {source}")]
    MarkerUnreadable {
        /// Marker path.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// The marker could not be parsed.
    #[error("'{path}' is not a valid member marker: {reason}")]
    MarkerMalformed {
        /// Marker path.
        path: PathBuf,
        /// Parse failure.
        reason: String,
    },
    /// The marker could not be written.
    #[error("could not write the member marker: {0}")]
    MarkerUnwritable(String),
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use radicle::identity::RepoId;

    use crate::{
        agent::Role,
        home::{MARKER, MemberHome},
        member::Member,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";

    #[test]
    fn keeps_every_member_location_inside_one_directory() {
        let home = MemberHome::for_project("/work", "myproject", Member::Nico);
        let root = Path::new("/work/myproject-nico");

        assert_eq!(home.path(), root);
        for path in [
            home.identity(),
            home.state_database(),
            home.chat_identity(),
            home.process_lock(),
            home.node_log(),
            home.sandbox_config(),
            home.git_common(),
            home.worktree(Role::Talker),
            home.cache(),
            home.harness(Role::Chatter),
        ] {
            assert!(
                path.starts_with(root),
                "'{}' escapes the member directory",
                path.display()
            );
        }
    }

    #[test]
    fn gives_each_role_its_own_worktree_and_harness_state() {
        let home = MemberHome::for_project("/work", "myproject", Member::Nico);

        for (left, right) in [
            (Role::Talker, Role::Chatter),
            (Role::Talker, Role::Coder),
            (Role::Chatter, Role::Coder),
        ] {
            assert_ne!(
                home.worktree(left),
                home.worktree(right),
                "roles sharing a worktree would fight over the checked-out branch"
            );
            assert_ne!(home.harness(left), home.harness(right));
        }
    }

    // Build output is the largest thing a member writes, and three copies of it per member would
    // multiply the disk a team costs by three for nothing.
    #[test]
    fn gives_every_role_of_one_member_the_same_build_cache() {
        let home = MemberHome::for_project("/work", "myproject", Member::Nico);

        assert_eq!(home.cache(), Path::new("/work/myproject-nico/cache"));
    }

    // A build cache holds compiled build scripts and plugins which the next build runs. Two
    // members sharing one would let either run code as the other.
    #[test]
    fn never_shares_a_build_cache_between_members() {
        let nico = MemberHome::for_project("/work", "myproject", Member::Nico);
        let maya = MemberHome::for_project("/work", "myproject", Member::Maya);

        assert_ne!(nico.cache(), maya.cache());
    }

    #[test]
    fn records_and_reads_back_the_directory_it_created() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository = RepoId::from_urn(REPOSITORY).expect("repository should parse");
        let home = MemberHome::for_project(root.path(), "myproject", Member::Nico);

        home.create().expect("member directory should be created");
        home.record(Member::Nico, repository)
            .expect("marker should be recorded");
        assert!(home.path().join(MARKER).is_file());

        let marker = home.marker().expect("marker should be readable");
        assert_eq!(marker.member().expect("member should parse"), Member::Nico);
        assert_eq!(
            marker.repository().expect("repository should parse"),
            Some(repository)
        );
    }

    #[test]
    fn finds_the_member_directory_from_a_subdirectory() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository = RepoId::from_urn(REPOSITORY).expect("repository should parse");
        let home = MemberHome::for_project(root.path(), "myproject", Member::Nico);
        home.create().expect("member directory should be created");
        home.record(Member::Nico, repository)
            .expect("marker should be recorded");

        let found =
            MemberHome::find(&home.worktree(Role::Coder)).expect("the home should be found");

        assert_eq!(
            found.path(),
            home.path(),
            "a command run inside a working copy must find the member directory above it"
        );
    }

    // A member's working copies live inside its home, so everything below it is repository
    // content. A file which happens to carry the marker's name there would otherwise end the
    // search one directory short of the home it was looking for.
    #[test]
    fn walks_past_a_repository_file_wearing_the_marker_name() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository = RepoId::from_urn(REPOSITORY).expect("repository should parse");
        let home = MemberHome::for_project(root.path(), "myproject", Member::Nico);
        home.create().expect("member directory should be created");
        home.record(Member::Nico, repository)
            .expect("marker should be recorded");
        let planted = home.worktree(Role::Coder);
        std::fs::create_dir_all(&planted).expect("working copy should be created");
        std::fs::write(planted.join(MARKER), "member = \"maya\"\n")
            .expect("planted file should be written");

        let found = MemberHome::find(&planted).expect("the member home should be found");

        assert_eq!(
            found.path(),
            home.path(),
            "discovery must reach the real member home rather than stop at repository content"
        );
    }

    #[test]
    fn refuses_a_marker_it_cannot_read() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository = RepoId::from_urn(REPOSITORY).expect("repository should parse");
        let home = MemberHome::for_project(root.path(), "myproject", Member::Nico);
        home.create().expect("member directory should be created");
        home.record(Member::Nico, repository)
            .expect("marker should be recorded");
        std::fs::write(home.path().join(MARKER), "member = ").expect("marker should be rewritten");

        assert!(
            matches!(
                home.marker(),
                Err(crate::home::Error::MarkerMalformed { .. })
            ),
            "a marker which cannot be read must be reported rather than guessed at"
        );
    }
}
