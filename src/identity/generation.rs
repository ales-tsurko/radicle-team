//! Local rollback protection for repository member bindings.
//!
//! [`GenerationStore`] records the highest accepted DID and generation for each repository and
//! member in the application's SQLite state database. When a validated manifest is observed
//! again, it rejects an older generation and rejects a changed DID unless the generation increased.
//!
//! Repository governance decides which manifest is canonical. This module only prevents stale
//! local or external data from silently reactivating an identity the process has already replaced.
//! The application records the generation after binding validation and before starting the watcher.

use std::{num::TryFromIntError, path::Path};

use radicle::identity::RepoId;
use sqlite::State;

use crate::{database::Database, manifest::ValidatedBinding, member::Member};

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
    database: Database,
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
    pub async fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Self::from_database(Database::open(path).await?).await
    }

    /// Rejects rollback and records the validated binding as the latest seen.
    ///
    /// # Errors
    ///
    /// Returns an error for rollback, replacement without a generation
    /// increase, an unrepresentable generation, or a database failure.
    pub async fn record(&self, repository: RepoId, binding: ValidatedBinding) -> Result<(), Error> {
        let generation = i64::try_from(binding.generation())?;
        let repository_id = repository.to_string();
        let member_id = binding.member().id().to_owned();
        let did = binding.did().to_string();
        let outcome = self
            .database
            .call(move |connection| {
                connection.execute("BEGIN IMMEDIATE")?;
                let outcome = Self::record_transaction(
                    connection,
                    &repository_id,
                    &member_id,
                    &did,
                    generation,
                );
                outcome
                    .and_then(|outcome| {
                        connection.execute("COMMIT")?;
                        Ok(outcome)
                    })
                    .inspect_err(|_| {
                        let _rollback = connection.execute("ROLLBACK");
                    })
            })
            .await?;
        match outcome {
            RecordOutcome::Recorded => Ok(()),
            RecordOutcome::Rollback { highest } => Err(Error::Rollback {
                repository,
                member: binding.member(),
                observed: binding.generation(),
                highest: u64::try_from(highest)?,
            }),
            RecordOutcome::Replacement => Err(Error::ReplacementGeneration {
                repository,
                member: binding.member(),
                generation: binding.generation(),
            }),
        }
    }

    /// Returns a generation safe for adding or replacing a member binding.
    ///
    /// # Errors
    ///
    /// Returns an error when prior state cannot be read or its generation is exhausted.
    pub async fn next_generation(&self, repository: RepoId, member: Member) -> Result<u64, Error> {
        match self.previous(repository, member).await? {
            Some(previous) if previous.generation < MAX_GENERATION => Ok(previous.generation + 1),
            Some(_) => Err(Error::GenerationExhausted { repository, member }),
            None => Ok(1),
        }
    }

    async fn from_database(database: Database) -> Result<Self, Error> {
        database.execute(SCHEMA).await?;
        Ok(Self { database })
    }

    fn record_transaction(
        connection: &sqlite::ConnectionThreadSafe,
        repository: &str,
        member: &str,
        did: &str,
        generation: i64,
    ) -> sqlite::Result<RecordOutcome> {
        let mut previous = connection.prepare(
            "SELECT did, generation FROM member_binding_generations
             WHERE repository = ?1 AND member = ?2",
        )?;
        previous.bind((1, repository))?;
        previous.bind((2, member))?;
        if previous.next()? == State::Row {
            let previous_did = previous.read::<String, _>("did")?;
            let previous_generation = previous.read::<i64, _>("generation")?;
            if generation < previous_generation {
                return Ok(RecordOutcome::Rollback {
                    highest: previous_generation,
                });
            }
            if did != previous_did && generation <= previous_generation {
                return Ok(RecordOutcome::Replacement);
            }
        }
        drop(previous);

        let mut update = connection.prepare(
            "INSERT INTO member_binding_generations (repository, member, did, generation)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (repository, member) DO UPDATE SET
                 did = excluded.did,
                 generation = excluded.generation",
        )?;
        update.bind((1, repository))?;
        update.bind((2, member))?;
        update.bind((3, did))?;
        update.bind((4, generation))?;
        update.next()?;
        Ok(RecordOutcome::Recorded)
    }

    async fn previous(
        &self,
        repository: RepoId,
        member: Member,
    ) -> Result<Option<Previous>, Error> {
        let repository = repository.to_string();
        let member = member.id().to_owned();
        let previous = self
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT did, generation FROM member_binding_generations
                     WHERE repository = ?1 AND member = ?2",
                )?;
                statement.bind((1, repository.as_str()))?;
                statement.bind((2, member.as_str()))?;
                if statement.next()? != State::Row {
                    return Ok(None);
                }
                Ok(Some((
                    statement.read::<String, _>("did")?,
                    statement.read::<i64, _>("generation")?,
                )))
            })
            .await?;
        previous
            .map(|(did, generation)| {
                Ok(Previous {
                    did,
                    generation: u64::try_from(generation)?,
                })
            })
            .transpose()
    }

    #[cfg(all(test, not(miri)))]
    async fn memory() -> Result<Self, Error> {
        Self::from_database(Database::memory().await?).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Previous {
    did: String,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordOutcome {
    Recorded,
    Rollback { highest: i64 },
    Replacement,
}

/// Error validating or recording an observed binding generation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The state database parent could not be created.
    #[error("failed to create identity state directory: {0}")]
    Io(#[from] std::io::Error),
    /// The state database could not be accessed.
    #[error("identity state database error: {0}")]
    Database(String),
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

impl From<crate::database::Error> for Error {
    fn from(error: crate::database::Error) -> Self {
        Self::Database(error.to_string())
    }
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
                &crate::manifest::Binding::test(
                    Did::from_str(did).expect("DID should parse"),
                    generation,
                ),
                Revision::Repository(Oid::from_sha1([0; 20])),
                crate::manifest::SecurityPolicy::test(Self::repository()),
                crate::manifest::RepositoryPolicy::new(
                    crate::manifest::TriggerAccess::Delegates,
                    vec![Did::from_str(did).expect("DID should parse")],
                    crate::manifest::PriorityLabels::default(),
                    crate::manifest::Limits::default(),
                ),
            )
        }
    }

    #[tokio::test]
    async fn records_and_advances_generation() {
        let store = GenerationStore::memory().await.expect("store should open");

        store
            .record(Fixture::repository(), Fixture::binding(NICO, 1))
            .await
            .expect("first binding should be recorded");
        store
            .record(Fixture::repository(), Fixture::binding(MAYA, 2))
            .await
            .expect("replacement should be recorded");

        assert_eq!(
            store
                .previous(Fixture::repository(), Member::Nico)
                .await
                .expect("binding should be read"),
            Some(super::Previous {
                did: MAYA.to_owned(),
                generation: 2,
            })
        );
    }

    #[tokio::test]
    async fn rejects_generation_rollback() {
        let store = GenerationStore::memory().await.expect("store should open");
        store
            .record(Fixture::repository(), Fixture::binding(NICO, 2))
            .await
            .expect("first binding should be recorded");

        assert!(matches!(
            store
                .record(Fixture::repository(), Fixture::binding(NICO, 1))
                .await,
            Err(Error::Rollback { .. })
        ));
    }

    #[tokio::test]
    async fn rejects_replacement_without_generation_change() {
        let store = GenerationStore::memory().await.expect("store should open");
        store
            .record(Fixture::repository(), Fixture::binding(NICO, 1))
            .await
            .expect("first binding should be recorded");

        assert!(matches!(
            store
                .record(Fixture::repository(), Fixture::binding(MAYA, 1))
                .await,
            Err(Error::ReplacementGeneration { .. })
        ));
    }

    #[tokio::test]
    async fn advances_the_suggested_generation() {
        let store = GenerationStore::memory().await.expect("store should open");

        assert!(matches!(
            store
                .next_generation(Fixture::repository(), Member::Nico)
                .await,
            Ok(1)
        ));
        store
            .record(Fixture::repository(), Fixture::binding(NICO, 3))
            .await
            .expect("binding should be recorded");
        assert!(matches!(
            store
                .next_generation(Fixture::repository(), Member::Nico)
                .await,
            Ok(4)
        ));
    }

    #[tokio::test]
    async fn rejects_an_exhausted_suggested_generation() {
        let store = GenerationStore::memory().await.expect("store should open");
        store
            .record(
                Fixture::repository(),
                Fixture::binding(NICO, super::MAX_GENERATION),
            )
            .await
            .expect("maximum generation should be recorded");

        assert!(matches!(
            store
                .next_generation(Fixture::repository(), Member::Nico)
                .await,
            Err(Error::GenerationExhausted { .. })
        ));
    }

    #[tokio::test]
    async fn concurrent_records_cannot_overwrite_a_newer_generation() {
        let store = GenerationStore::memory().await.expect("store should open");
        let newer = store.record(Fixture::repository(), Fixture::binding(NICO, 2));
        let older = store.record(Fixture::repository(), Fixture::binding(NICO, 1));

        let (newer, older) = tokio::join!(newer, older);

        newer.expect("newer binding should be recorded");
        assert!(older.is_ok() || matches!(older, Err(Error::Rollback { .. })));
        assert_eq!(
            store
                .previous(Fixture::repository(), Member::Nico)
                .await
                .expect("binding should be read")
                .expect("binding should exist")
                .generation,
            2
        );
    }
}
