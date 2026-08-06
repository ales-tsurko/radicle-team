//! Team member filesystem locations.
//!
//! One member working in one repository owns one directory, created where `setup` was run rather
//! than hidden under the user's home. Everything that member needs lives inside it: its signing
//! identity, its durable state, one working copy per adapter, and disposable caches. There is no
//! global layer, so a directory can be moved, backed up, or deleted as a unit.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
};

use directories::BaseDirs;
use radicle::identity::RepoId;
use serde::{Deserialize, Serialize};

use crate::{agent::Adapter, member::Member};

/// Environment variable naming the member directory.
pub const ENVIRONMENT_VARIABLE: &str = "RADICLE_TEAM_HOME";

/// File identifying a directory as one member's home.
pub const MARKER: &str = ".radicle-team";

/// On-disk layout this build understands.
pub const LAYOUT: u32 = 2;

/// Filesystem locations owned by one team member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberHome {
    path: PathBuf,
}

impl MemberHome {
    /// Locates the member directory for a command invoked without an explicit path.
    ///
    /// Uses [`ENVIRONMENT_VARIABLE`] when set. Otherwise it searches the current directory and
    /// each parent for a [`MARKER`] file, the way `git` finds a repository.
    ///
    /// # Errors
    ///
    /// Returns an error when no member directory is found, or when the only thing found is a
    /// directory written by an earlier layout.
    pub fn discover() -> Result<Self, Error> {
        if let Some(path) = env::var_os(ENVIRONMENT_VARIABLE).filter(|value| !value.is_empty()) {
            return Ok(Self::at(PathBuf::from(path)));
        }
        let start = env::current_dir().map_err(Error::WorkingDirectory)?;
        for directory in start.ancestors() {
            if directory.join(MARKER).is_file() {
                return Ok(Self::at(directory));
            }
        }

        Err(Self::legacy_layout().map_or(Error::NotFound { start }, Error::LegacyLayout))
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
            self.caches(),
            self.harnesses(),
        ]
        .into_iter()
        .chain(
            Adapter::ALL
                .into_iter()
                .flat_map(|adapter| [self.cache(adapter), self.harness(adapter)]),
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
        self.write_marker(member, repository)
    }

    /// Reads the marker recorded when this directory was created.
    ///
    /// # Errors
    ///
    /// Returns an error when the marker is missing, unreadable, malformed, or written by a layout
    /// this build does not understand.
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
        if marker.layout != LAYOUT {
            return Err(Error::LayoutVersion {
                path,
                found: marker.layout,
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

    /// Returns one adapter's working copy.
    #[must_use]
    pub fn worktree(&self, adapter: Adapter) -> PathBuf {
        self.code().join(adapter.id())
    }

    /// Returns the root for disposable build and dependency caches.
    #[must_use]
    pub fn caches(&self) -> PathBuf {
        self.path.join("cache")
    }

    /// Returns one adapter's cache directory.
    ///
    /// Caches are persistent writable state and therefore weaken execution isolation. Nothing that
    /// must survive belongs here, and removing the directory must always be safe.
    #[must_use]
    pub fn cache(&self, adapter: Adapter) -> PathBuf {
        self.caches().join(adapter.id())
    }

    /// Returns the root for agent harness state.
    #[must_use]
    pub fn harnesses(&self) -> PathBuf {
        self.path.join("harness")
    }

    /// Returns one adapter's harness home.
    ///
    /// An agent harness keeps its settings and logs in a user-level directory of its own by
    /// default, which every member on the machine would then share. This is given to it instead,
    /// so removing the member directory removes that state rather than orphaning it.
    #[must_use]
    pub fn harness(&self, adapter: Adapter) -> PathBuf {
        self.harnesses().join(adapter.id())
    }

    fn marker_path(&self) -> PathBuf {
        self.path.join(MARKER)
    }

    fn write_marker(&self, member: Member, repository: RepoId) -> Result<(), Error> {
        let marker = Marker {
            layout: LAYOUT,
            member: member.id().to_owned(),
            repository: repository.urn(),
        };
        let body = toml_edit::ser::to_string_pretty(&marker)
            .map_err(|source| Error::MarkerUnwritable(source.to_string()))?;

        fs::write(self.marker_path(), body).map_err(Error::Create)
    }

    fn legacy_layout() -> Option<PathBuf> {
        let root = env::var_os(ENVIRONMENT_VARIABLE)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                BaseDirs::new().map(|directories| directories.home_dir().join(".radicle-team"))
            })?;

        root.join("repositories").is_dir().then_some(root)
    }
}

/// Identity recorded in a member directory when it was created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// On-disk layout the directory was written with.
    pub layout: u32,
    /// Fixed name of the member owning the directory.
    pub member: String,
    /// Repository the member works in.
    pub repository: String,
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

    /// Returns the repository this directory belongs to.
    ///
    /// # Errors
    ///
    /// Returns an error when the recorded identifier is not a valid repository.
    pub fn repository(&self) -> Result<RepoId, radicle::identity::IdError> {
        RepoId::from_urn(&self.repository)
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
        "no member directory found in '{start}' or its parents; run `radicle-team setup` to \
         create one, or pass --home PATH"
    )]
    NotFound {
        /// Directory the search started from.
        start: PathBuf,
    },
    /// A directory written by an earlier layout was found instead.
    #[error(
        "'{0}' was written by an earlier layout which is no longer read; its members must be set \
         up again in their own directories"
    )]
    LegacyLayout(PathBuf),
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
    /// The marker records a layout this build does not understand.
    #[error(
        "'{path}' records layout {found}, but this build reads layout {LAYOUT}; set the member up \
         again"
    )]
    LayoutVersion {
        /// Marker path.
        path: PathBuf,
        /// Layout found in the marker.
        found: u32,
    },
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    // Only the tests which touch the filesystem need these, and Miri runs none of them.
    #[cfg(not(miri))]
    use radicle::identity::RepoId;

    #[cfg(not(miri))]
    use crate::home::{LAYOUT, MARKER};
    use crate::{agent::Adapter, home::MemberHome, member::Member};

    #[cfg(not(miri))]
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
            home.worktree(Adapter::Talker),
            home.cache(Adapter::Coder),
            home.harness(Adapter::Chatter),
        ] {
            assert!(
                path.starts_with(root),
                "'{}' escapes the member directory",
                path.display()
            );
        }
    }

    #[test]
    fn gives_each_adapter_its_own_worktree_cache_and_harness_state() {
        let home = MemberHome::for_project("/work", "myproject", Member::Nico);

        for (left, right) in [
            (Adapter::Talker, Adapter::Chatter),
            (Adapter::Talker, Adapter::Coder),
            (Adapter::Chatter, Adapter::Coder),
        ] {
            assert_ne!(
                home.worktree(left),
                home.worktree(right),
                "adapters sharing a worktree would fight over the checked-out branch"
            );
            assert_ne!(
                home.cache(left),
                home.cache(right),
                "adapters sharing a cache would block on each other's build locks"
            );
            assert_ne!(home.harness(left), home.harness(right));
        }
    }

    #[cfg(not(miri))]
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
        assert_eq!(marker.layout, LAYOUT);
        assert_eq!(marker.member().expect("member should parse"), Member::Nico);
        assert_eq!(
            marker.repository().expect("repository should parse"),
            repository
        );
    }

    #[cfg(not(miri))]
    #[test]
    fn finds_the_member_directory_from_a_subdirectory() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository = RepoId::from_urn(REPOSITORY).expect("repository should parse");
        let home = MemberHome::for_project(root.path(), "myproject", Member::Nico);
        home.create().expect("member directory should be created");
        home.record(Member::Nico, repository)
            .expect("marker should be recorded");

        let mut found = None;
        for directory in home.worktree(Adapter::Coder).ancestors() {
            if directory.join(MARKER).is_file() {
                found = Some(directory.to_path_buf());
                break;
            }
        }

        assert_eq!(
            found.as_deref(),
            Some(home.path()),
            "a command run inside a working copy must find the member directory above it"
        );
    }

    #[cfg(not(miri))]
    #[test]
    fn refuses_a_directory_written_by_another_layout() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let repository = RepoId::from_urn(REPOSITORY).expect("repository should parse");
        let home = MemberHome::for_project(root.path(), "myproject", Member::Nico);
        home.create().expect("member directory should be created");
        home.record(Member::Nico, repository)
            .expect("marker should be recorded");
        let marker = home.path().join(MARKER);
        let body = std::fs::read_to_string(&marker).expect("marker should be readable");
        std::fs::write(
            &marker,
            body.replace(&format!("layout = {LAYOUT}"), "layout = 1"),
        )
        .expect("marker should be rewritten");

        assert!(
            matches!(
                home.marker(),
                Err(crate::home::Error::LayoutVersion { found: 1, .. })
            ),
            "an unreadable layout must be reported rather than acted on"
        );
    }
}
