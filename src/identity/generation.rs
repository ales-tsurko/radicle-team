//! Local rollback protection for repository member bindings.
//!
//! [`GenerationStore`] records the highest accepted DID and generation for each repository and
//! member in the application's SQLite state database. When a validated manifest is observed
//! again, it rejects an older generation and rejects a changed DID unless the generation increased.
//!
//! Repository governance decides which manifest is canonical; this module only prevents stale
//! local or external data from silently reactivating an identity the process has already replaced.
//! The application records the generation after binding validation and before starting the watcher.

use std::{fs, num::TryFromIntError, path::Path};

use radicle::identity::RepoId;
use sqlite::{Connection, State};

use crate::{manifest::ValidatedBinding, member::Member};

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS member_binding_generations (
        repository TEXT NOT NULL,
        member TEXT NOT NULL,
        did TEXT NOT NULL,
        generation INTEGER NOT NULL,
        PRIMARY KEY (repository, member)
    ) STRICT;
";
const MAX_GENERATION: u64 = i64::MAX.unsigned_abs();

/// Persistent highest observed binding generations.
pub struct GenerationStore {
    connection: Connection,
}

impl std::fmt::Debug for GenerationStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationStore")
            .finish_non_exhaustive()
    }
}

impl GenerationStore {
    /// Opens generation state in the application SQLite database.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be opened or initialized.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// Rejects rollback and records the validated binding as the latest seen.
    ///
    /// # Errors
    ///
    /// Returns an error for rollback, replacement without a generation
    /// increase, an unrepresentable generation, or a database failure.
    pub fn record(&self, repository: RepoId, binding: ValidatedBinding) -> Result<(), Error> {
        if let Some(previous) = self.previous(repository, binding.member())? {
            if binding.generation() < previous.generation {
                return Err(Error::Rollback {
                    repository,
                    member: binding.member(),
                    observed: binding.generation(),
                    highest: previous.generation,
                });
            }
            if binding.did().to_string() != previous.did
                && binding.generation() <= previous.generation
            {
                return Err(Error::ReplacementGeneration {
                    repository,
                    member: binding.member(),
                    generation: binding.generation(),
                });
            }
        }

        let generation = i64::try_from(binding.generation())?;
        let mut statement = self.connection.prepare(
            "INSERT INTO member_binding_generations (repository, member, did, generation)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (repository, member) DO UPDATE SET
                 did = excluded.did,
                 generation = excluded.generation",
        )?;
        let repository = repository.to_string();
        let did = binding.did().to_string();
        statement.bind((1, repository.as_str()))?;
        statement.bind((2, binding.member().id()))?;
        statement.bind((3, did.as_str()))?;
        statement.bind((4, generation))?;
        statement.next()?;

        Ok(())
    }

    /// Returns a generation safe for adding or replacing a member binding.
    ///
    /// # Errors
    ///
    /// Returns an error when prior state cannot be read or its generation is exhausted.
    pub fn next_generation(&self, repository: RepoId, member: Member) -> Result<u64, Error> {
        match self.previous(repository, member)? {
            Some(previous) if previous.generation < MAX_GENERATION => Ok(previous.generation + 1),
            Some(_) => Err(Error::GenerationExhausted { repository, member }),
            None => Ok(1),
        }
    }

    fn from_connection(connection: Connection) -> Result<Self, Error> {
        connection.execute(SCHEMA)?;
        Ok(Self { connection })
    }

    fn previous(&self, repository: RepoId, member: Member) -> Result<Option<Previous>, Error> {
        let mut statement = self.connection.prepare(
            "SELECT did, generation FROM member_binding_generations
             WHERE repository = ?1 AND member = ?2",
        )?;
        let repository = repository.to_string();
        statement.bind((1, repository.as_str()))?;
        statement.bind((2, member.id()))?;
        if statement.next()? != State::Row {
            return Ok(None);
        }
        let generation = statement.read::<i64, _>("generation")?;

        Ok(Some(Previous {
            did: statement.read::<String, _>("did")?,
            generation: u64::try_from(generation)?,
        }))
    }

    #[cfg(test)]
    fn memory() -> Result<Self, Error> {
        Self::from_connection(Connection::open(":memory:")?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Previous {
    did: String,
    generation: u64,
}

/// Error validating or recording an observed binding generation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The state database parent could not be created.
    #[error("failed to create identity state directory: {0}")]
    Io(#[from] std::io::Error),
    /// The state database could not be accessed.
    #[error("identity state database error: {0}")]
    Database(#[from] sqlite::Error),
    /// A generation cannot be represented by SQLite.
    #[error("binding generation is too large: {0}")]
    Generation(#[from] TryFromIntError),
    /// An older generation was observed.
    #[error(
        "binding generation rollback for {member} in {repository}: observed {observed}, highest \
         {highest}"
    )]
    Rollback {
        /// Repository whose binding rolled back.
        repository: RepoId,
        /// Bound member.
        member: Member,
        /// Newly observed generation.
        observed: u64,
        /// Highest previously observed generation.
        highest: u64,
    },
    /// A DID changed without increasing its generation.
    #[error(
        "binding DID changed for {member} in {repository} without increasing generation \
         {generation}"
    )]
    ReplacementGeneration {
        /// Repository whose binding changed.
        repository: RepoId,
        /// Bound member.
        member: Member,
        /// Reused generation.
        generation: u64,
    },
    /// The last recorded generation cannot be incremented.
    #[error("binding generation is exhausted for {member} in {repository}")]
    GenerationExhausted {
        /// Repository whose generation is exhausted.
        repository: RepoId,
        /// Member whose generation is exhausted.
        member: Member,
    },
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::str::FromStr as _;

    use radicle::{
        git::Oid,
        identity::{Did, RepoId},
    };

    use crate::{
        identity::{GenerationStore, generation::Error},
        manifest::{Revision, ValidatedBinding},
        member::Member,
    };

    const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
    const NICO: &str = "did:key:z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
    const MAYA: &str = "did:key:z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C";

    struct Fixture;

    impl Fixture {
        fn repository() -> RepoId {
            REPOSITORY.parse().expect("repository should parse")
        }

        fn binding(did: &str, generation: u64) -> ValidatedBinding {
            ValidatedBinding::new(
                Member::Nico,
                Did::from_str(did).expect("DID should parse"),
                generation,
                Revision::Repository(Oid::from_sha1([0; 20])),
            )
        }
    }

    #[test]
    fn records_and_advances_generation() {
        let store = GenerationStore::memory().expect("store should open");

        store
            .record(Fixture::repository(), Fixture::binding(NICO, 1))
            .expect("first binding should be recorded");
        store
            .record(Fixture::repository(), Fixture::binding(MAYA, 2))
            .expect("replacement should be recorded");

        assert_eq!(
            store
                .previous(Fixture::repository(), Member::Nico)
                .expect("binding should be read"),
            Some(super::Previous {
                did: MAYA.to_owned(),
                generation: 2,
            })
        );
    }

    #[test]
    fn rejects_generation_rollback() {
        let store = GenerationStore::memory().expect("store should open");
        store
            .record(Fixture::repository(), Fixture::binding(NICO, 2))
            .expect("first binding should be recorded");

        assert!(matches!(
            store.record(Fixture::repository(), Fixture::binding(NICO, 1)),
            Err(Error::Rollback { .. })
        ));
    }

    #[test]
    fn rejects_replacement_without_generation_change() {
        let store = GenerationStore::memory().expect("store should open");
        store
            .record(Fixture::repository(), Fixture::binding(NICO, 1))
            .expect("first binding should be recorded");

        assert!(matches!(
            store.record(Fixture::repository(), Fixture::binding(MAYA, 1)),
            Err(Error::ReplacementGeneration { .. })
        ));
    }

    #[test]
    fn advances_the_suggested_generation() {
        let store = GenerationStore::memory().expect("store should open");

        assert!(matches!(
            store.next_generation(Fixture::repository(), Member::Nico),
            Ok(1)
        ));
        store
            .record(Fixture::repository(), Fixture::binding(NICO, 3))
            .expect("binding should be recorded");
        assert!(matches!(
            store.next_generation(Fixture::repository(), Member::Nico),
            Ok(4)
        ));
    }

    #[test]
    fn rejects_an_exhausted_suggested_generation() {
        let store = GenerationStore::memory().expect("store should open");
        store
            .record(
                Fixture::repository(),
                Fixture::binding(NICO, super::MAX_GENERATION),
            )
            .expect("maximum generation should be recorded");

        assert!(matches!(
            store.next_generation(Fixture::repository(), Member::Nico),
            Err(Error::GenerationExhausted { .. })
        ));
    }
}
