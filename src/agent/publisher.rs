//! Authorized Radicle writes issued through MCP.

use std::{fmt, path::Path, str::FromStr as _, sync::Arc};

use radicle::{
    cob::{
        EntryId, Op,
        issue::{IssueStream, Issues},
        patch::{PatchStream, Patches},
        store::access::ReadOnly,
        stream::CobStream as _,
    },
    crypto::{PublicKey, ssh::keystore::MemorySigner},
    identity::RepoId,
    node::{Handle as _, Node, device::Device},
    profile::Profile,
    storage::{ReadRepository as _, ReadStorage as _, SignRepository as _, refs::SignedRefs},
};
use sqlite::State;

use crate::{agent::DeliveryId, database::Database, manifest::SecurityPolicy};

const JOURNAL_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS mcp_mutations (
        repository TEXT NOT NULL,
        delivery_repository TEXT NOT NULL,
        operation TEXT NOT NULL,
        action TEXT NOT NULL,
        invocation TEXT NOT NULL,
        method TEXT NOT NULL,
        author TEXT NOT NULL,
        result TEXT,
        PRIMARY KEY (
            repository,
            delivery_repository,
            operation,
            action,
            invocation
        )
    ) STRICT;
";

#[derive(Clone)]
pub(crate) struct Publisher {
    profile: Profile,
    repository: RepoId,
    signer: Option<MemorySigner>,
    announcer: Arc<dyn Announcer>,
    authorizer: Arc<dyn Authorizer>,
    journal: Option<MutationJournal>,
    mutation_lock: Arc<tokio::sync::Mutex<()>>,
}

impl fmt::Debug for Publisher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Publisher")
            .field("repository", &self.repository)
            .field("signer_loaded", &self.signer.is_some())
            .finish_non_exhaustive()
    }
}

impl Publisher {
    pub(crate) fn new(
        profile: Profile,
        repository: RepoId,
        signer: Option<MemorySigner>,
        announcer: impl Announcer,
        authorizer: impl Authorizer,
    ) -> Self {
        Self {
            profile,
            repository,
            signer,
            announcer: Arc::new(announcer),
            authorizer: Arc::new(authorizer),
            journal: None,
            mutation_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub(crate) async fn with_journal(mut self, path: &Path) -> Result<Self, Error> {
        self.journal = Some(MutationJournal::open(path).await?);
        Ok(self)
    }

    pub(crate) fn for_repository(&self, repository: RepoId) -> Self {
        let mut publisher = self.clone();
        publisher.repository = repository;
        publisher
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) async fn with_memory_journal(mut self) -> Result<Self, Error> {
        self.journal = Some(MutationJournal::memory().await?);
        Ok(self)
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) async fn forget_completion(&self, key: &MutationKey) -> Result<(), Error> {
        self.journal
            .as_ref()
            .ok_or(Error::JournalDisabled)?
            .forget_completion(self.repository, key)
            .await
    }

    pub(crate) fn author(&self) -> PublicKey {
        *self.profile.id()
    }

    pub(crate) async fn authorize(&self, method: &str) -> Result<(), Error> {
        self.authorizer.authorize(method).await
    }

    pub(crate) async fn security_policy(&self) -> Result<SecurityPolicy, Error> {
        self.authorizer.security_policy().await
    }

    pub(crate) async fn reconcile(&self, delivery: DeliveryId) -> Result<(), Error> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        for repository in journal.repositories(self.repository, delivery).await? {
            self.for_repository(repository)
                .reconcile_repository(self.repository, delivery)
                .await?;
        }
        Ok(())
    }

    async fn reconcile_repository(
        &self,
        delivery_repository: RepoId,
        delivery: DeliveryId,
    ) -> Result<(), Error> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        let _mutation_guard = self.mutation_lock.lock().await;
        for entry in journal
            .entries(self.repository, delivery_repository, delivery)
            .await?
        {
            if entry.result.is_some() {
                self.finish_recovery(entry.author).await?;
                continue;
            }
            let publisher = self.clone();
            let marker = Self::marker(&entry.key);
            let author = entry.author;
            let operation = tokio::task::spawn_blocking(move || {
                let repository = publisher.profile.storage.repository(publisher.repository)?;
                let operation = publisher.find_marker(&repository, &marker, author)?;
                if let Some(operation) = operation
                    && author != publisher.author()
                    && !Self::is_signed(&repository, author, operation)?
                {
                    return Err(Error::UnsignedRecovery);
                }
                Ok(operation)
            })
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
            let Some(operation) = operation else {
                journal.discard(self.repository, &entry.key).await?;
                continue;
            };
            journal
                .complete(
                    self.repository,
                    &entry.key,
                    &entry.method,
                    author,
                    &operation.to_string(),
                )
                .await?;
            self.finish_recovery(author).await?;
        }
        Ok(())
    }

    pub(crate) async fn mutate<T, F, V>(
        &self,
        method: &'static str,
        mutation: Option<MutationKey>,
        operation: F,
        validate: V,
    ) -> Result<String, Error>
    where
        T: Send + ToString + 'static,
        F: FnOnce(
                &radicle::storage::git::Repository,
                &radicle::node::device::BoxedDevice,
                &str,
            ) -> Result<T, Error>
            + Send
            + 'static,
        V: Fn(
                &radicle::storage::git::Repository,
                PublicKey,
                EntryId,
                MutationValidation,
            ) -> Result<bool, Error>
            + Send
            + 'static,
    {
        if let Err(error) = self.authorizer.authorize(method).await {
            tracing::warn!(method, %error, "the manifest no longer allows this MCP method");
            return Err(error);
        }
        let _mutation_guard = if mutation.is_some() {
            Some(self.mutation_lock.lock().await)
        } else {
            None
        };
        let marker = mutation.as_ref().map(Self::marker);
        let pending = if let (Some(journal), Some(key)) = (&self.journal, mutation.as_ref()) {
            Some(
                journal
                    .begin(self.repository, key, method, self.author())
                    .await?,
            )
        } else {
            None
        };
        if let Some(JournalEntry {
            result: Some(result),
            author,
        }) = pending.as_ref()
        {
            self.finish_recovery(*author).await?;
            return Ok(result.clone());
        }
        let pending_author = pending.map(|entry| entry.author);
        let publisher = self.clone();
        let recovery_marker = marker.clone();
        let (value, author) = tokio::task::spawn_blocking(move || {
            let repository = publisher.profile.storage.repository(publisher.repository)?;
            let signer = match publisher.signer.clone() {
                Some(signer) => Device::from(signer).boxed(),
                None => publisher
                    .profile
                    .signer()
                    .map_err(|error| Error::Signer(error.to_string()))?,
            };
            let current = publisher.author();
            let recovered = match (recovery_marker.as_deref(), pending_author) {
                (Some(marker), Some(author)) => {
                    match publisher.find_marker(&repository, marker, author)? {
                        Some(operation)
                            if validate(
                                &repository,
                                author,
                                operation,
                                MutationValidation::Recovery,
                            )? && (author == current
                                || Self::is_signed(&repository, author, operation)?) =>
                        {
                            Some((operation.to_string(), author))
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            let (value, author) = match recovered {
                Some(recovered) => {
                    tracing::info!(
                        method,
                        author = %recovered.1,
                        operation = recovered.0,
                        "recovered an interrupted mutation instead of repeating it"
                    );
                    recovered
                }
                None => {
                    let value = if let Some(marker) = recovery_marker.as_deref() {
                        operation(&repository, &signer, marker)?.to_string()
                    } else {
                        operation(&repository, &signer, method)?.to_string()
                    };
                    let operation =
                        EntryId::from_str(&value).map_err(|source| Error::InvalidId {
                            value: value.clone(),
                            source: Box::new(source),
                        })?;
                    if !validate(&repository, current, operation, MutationValidation::Fresh)? {
                        return Err(Error::NotApplied);
                    }
                    (value, current)
                }
            };
            if author == current {
                repository
                    .sign_refs(&signer)
                    .map_err(|error| Error::Signing(error.to_string()))?;
            }
            Ok::<_, Error>((value, author))
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))??;
        if let (Some(journal), Some(key)) = (&self.journal, mutation.as_ref()) {
            journal
                .complete(self.repository, key, method, author, &value)
                .await?;
        }
        tracing::info!(method, author = %author, operation = value, "published Radicle mutation");
        self.announce(author).await?;
        Ok(value)
    }

    fn marker(key: &MutationKey) -> String {
        if key.invocation == 0 {
            format!("radicle-team mcp {}:{}", key.delivery, key.request)
        } else {
            format!(
                "radicle-team mcp {}:{}:{}",
                key.delivery, key.invocation, key.request
            )
        }
    }

    async fn finish_recovery(&self, author: PublicKey) -> Result<(), Error> {
        if author != self.author() {
            return self.announce(author).await;
        }
        let publisher = self.clone();
        tokio::task::spawn_blocking(move || {
            let repository = publisher.profile.storage.repository(publisher.repository)?;
            let signer = match publisher.signer.clone() {
                Some(signer) => Device::from(signer).boxed(),
                None => publisher
                    .profile
                    .signer()
                    .map_err(|error| Error::Signer(error.to_string()))?,
            };
            repository
                .sign_refs(&signer)
                .map_err(|error| Error::Signing(error.to_string()))
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))??;
        self.announce(author).await
    }

    fn find_marker(
        &self,
        repository: &radicle::storage::git::Repository,
        marker: &str,
        author: PublicKey,
    ) -> Result<Option<EntryId>, Error> {
        let issues = Issues::open(repository, ReadOnly)?;
        for issue in issues.all()? {
            let (id, _issue) = issue?;
            if let Some(operation) = self.find_in(
                repository,
                IssueStream::init(id, repository).all(),
                marker,
                author,
            )? {
                return Ok(Some(operation));
            }
        }
        let patches = Patches::open(repository, ReadOnly)?;
        for patch in patches.all()? {
            let (id, _patch) = patch?;
            if let Some(operation) = self.find_in(
                repository,
                PatchStream::init(id, repository).all(),
                marker,
                author,
            )? {
                return Ok(Some(operation));
            }
        }
        Ok(None)
    }

    fn find_in<A>(
        &self,
        repository: &radicle::storage::git::Repository,
        operations: Result<
            impl Iterator<Item = Result<Op<A>, radicle::cob::stream::error::Ops>>,
            radicle::cob::stream::error::Stream,
        >,
        marker: &str,
        author: PublicKey,
    ) -> Result<Option<EntryId>, Error> {
        self.find_matching_in(repository, operations, marker, author, |_| true)
    }

    fn find_matching_in<A>(
        &self,
        repository: &radicle::storage::git::Repository,
        operations: Result<
            impl Iterator<Item = Result<Op<A>, radicle::cob::stream::error::Ops>>,
            radicle::cob::stream::error::Stream,
        >,
        marker: &str,
        author: PublicKey,
        matches: impl Fn(&Op<A>) -> bool,
    ) -> Result<Option<EntryId>, Error> {
        for operation in operations? {
            let operation = operation?;
            if operation.author != author || !matches(&operation) {
                continue;
            }
            let commit = repository.backend.find_commit(operation.id.into())?;
            if commit
                .message()
                .is_some_and(|message| message.lines().next() == Some(marker))
            {
                return Ok(Some(operation.id));
            }
        }
        Ok(None)
    }

    fn is_signed(
        repository: &radicle::storage::git::Repository,
        author: PublicKey,
        operation: EntryId,
    ) -> Result<bool, Error> {
        let Some(signed) = SignedRefs::load(author, repository).map_err(Error::from)? else {
            return Ok(false);
        };
        for (_name, tip) in signed.iter() {
            if signed_tip_reaches(repository, operation, *tip)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn announce(&self, namespace: PublicKey) -> Result<(), Error> {
        let announcer = Arc::clone(&self.announcer);
        let repository = self.repository;
        tokio::task::spawn_blocking(move || announcer.announce(repository, namespace))
            .await
            .map_err(|error| Error::Task(error.to_string()))?
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MutationKey {
    pub(crate) repository: RepoId,
    pub(crate) delivery: DeliveryId,
    pub(crate) invocation: usize,
    pub(crate) request: String,
}

#[derive(Clone, Copy)]
pub(crate) enum MutationValidation {
    Fresh,
    Recovery,
}

pub(crate) trait Announcer: fmt::Debug + Send + Sync + 'static {
    fn announce(&self, repository: RepoId, namespace: PublicKey) -> Result<(), Error>;
}

impl Announcer for Node {
    fn announce(&self, repository: RepoId, namespace: PublicKey) -> Result<(), Error> {
        let mut node = self.clone();
        let _announced = node.announce_refs_for(repository, [namespace])?;
        Ok(())
    }
}

#[async_trait::async_trait]
pub(crate) trait Authorizer: fmt::Debug + Send + Sync + 'static {
    async fn authorize(&self, method: &str) -> Result<(), Error>;

    async fn security_policy(&self) -> Result<SecurityPolicy, Error>;
}

#[derive(Clone)]
struct MutationJournal {
    database: Database,
}

impl MutationJournal {
    async fn open(path: &Path) -> Result<Self, Error> {
        Self::from_database(Database::open(path).await?).await
    }

    #[cfg(all(test, not(miri)))]
    async fn memory() -> Result<Self, Error> {
        Self::from_database(Database::memory().await?).await
    }

    async fn from_database(database: Database) -> Result<Self, Error> {
        database.execute(JOURNAL_SCHEMA).await?;
        Ok(Self { database })
    }

    async fn begin(
        &self,
        repository: RepoId,
        key: &MutationKey,
        method: &'static str,
        author: PublicKey,
    ) -> Result<JournalEntry, Error> {
        let values = mutation_values(repository, key);
        let author = author.to_string();
        self.database
            .call(move |connection| {
                let mut insert = connection.prepare(
                    "INSERT OR IGNORE INTO mcp_mutations
                         (repository, delivery_repository, operation, action, invocation,
                          method, author)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                bind_mutation(&mut insert, &values)?;
                insert.bind((6, method))?;
                insert.bind((7, author.as_str()))?;
                insert.next()?;
                drop(insert);

                let mut select = connection.prepare(
                    "SELECT method, author, result FROM mcp_mutations
                     WHERE repository = ?1 AND delivery_repository = ?2
                       AND operation = ?3 AND action = ?4 AND invocation = ?5",
                )?;
                bind_mutation(&mut select, &values)?;
                if select.next()? != State::Row {
                    return Ok(None);
                }

                Ok(Some((
                    select.read::<String, _>(0)?,
                    select.read::<String, _>(1)?,
                    select.read::<Option<String>, _>(2)?,
                )))
            })
            .await?
            .ok_or(Error::MutationMissing)
            .and_then(|(recorded, author, result)| {
                if recorded != method {
                    return Err(Error::MutationChanged {
                        recorded,
                        requested: method.to_owned(),
                    });
                }
                let author = PublicKey::from_str(&author).map_err(|source| Error::InvalidId {
                    value: author.clone(),
                    source: Box::new(source),
                })?;

                Ok(JournalEntry { author, result })
            })
    }

    async fn complete(
        &self,
        repository: RepoId,
        key: &MutationKey,
        method: &str,
        author: PublicKey,
        result: &str,
    ) -> Result<(), Error> {
        let values = mutation_values(repository, key);
        let method = method.to_owned();
        let result = result.to_owned();
        let author = author.to_string();
        self.database
            .call(move |connection| {
                complete_mutation(connection, &values, &method, &author, &result)
            })
            .await
            .map_err(Error::from)
    }

    async fn entries(
        &self,
        repository: RepoId,
        delivery_repository: RepoId,
        delivery: DeliveryId,
    ) -> Result<Vec<DeliveryJournalEntry>, Error> {
        let values = [
            repository.to_string(),
            delivery_repository.to_string(),
            delivery.operation.to_string(),
            delivery.action.to_string(),
        ];
        self.database
            .call(move |connection| {
                let mut select = connection.prepare(
                    "SELECT invocation, method, author, result FROM mcp_mutations
                     WHERE repository = ?1 AND delivery_repository = ?2
                       AND operation = ?3 AND action = ?4",
                )?;
                for (index, value) in values.iter().enumerate() {
                    select.bind((index + 1, value.as_str()))?;
                }
                let mut entries = Vec::new();
                while select.next()? == State::Row {
                    let encoded = select.read::<String, _>(0)?;
                    let (invocation, request) = decode_invocation(encoded)?;
                    let method = select.read::<String, _>(1)?;
                    let value = select.read::<String, _>(2)?;
                    let author = PublicKey::from_str(&value).map_err(|error| sqlite::Error {
                        code: None,
                        message: Some(error.to_string()),
                    })?;
                    entries.push(DeliveryJournalEntry {
                        key: MutationKey {
                            repository: delivery_repository,
                            delivery,
                            invocation,
                            request,
                        },
                        method,
                        author,
                        result: select.read::<Option<String>, _>(3)?,
                    });
                }
                Ok(entries)
            })
            .await
            .map_err(Error::from)
    }

    async fn repositories(
        &self,
        delivery_repository: RepoId,
        delivery: DeliveryId,
    ) -> Result<Vec<RepoId>, Error> {
        let delivery_repository = delivery_repository.to_string();
        let operation = delivery.operation.to_string();
        let action = delivery.action.to_string();
        self.database
            .call(move |connection| {
                let mut select = connection.prepare(
                    "SELECT DISTINCT repository FROM mcp_mutations
                     WHERE delivery_repository = ?1
                       AND operation = ?2 AND action = ?3",
                )?;
                select.bind((1, delivery_repository.as_str()))?;
                select.bind((2, operation.as_str()))?;
                select.bind((3, action.as_str()))?;
                let mut repositories = Vec::new();
                while select.next()? == State::Row {
                    let value = select.read::<String, _>(0)?;
                    repositories.push(RepoId::from_str(&value).map_err(|error| sqlite::Error {
                        code: None,
                        message: Some(error.to_string()),
                    })?);
                }
                Ok(repositories)
            })
            .await
            .map_err(Error::from)
    }

    async fn discard(&self, repository: RepoId, key: &MutationKey) -> Result<(), Error> {
        self.update_entry(
            repository,
            key,
            "DELETE FROM mcp_mutations
             WHERE repository = ?1 AND delivery_repository = ?2
               AND operation = ?3 AND action = ?4
               AND invocation = ?5 AND result IS NULL",
        )
        .await
    }

    #[cfg(all(test, not(miri)))]
    async fn forget_completion(&self, repository: RepoId, key: &MutationKey) -> Result<(), Error> {
        self.update_entry(
            repository,
            key,
            "UPDATE mcp_mutations SET result = NULL
             WHERE repository = ?1 AND delivery_repository = ?2
               AND operation = ?3 AND action = ?4 AND invocation = ?5",
        )
        .await
    }

    async fn update_entry(
        &self,
        repository: RepoId,
        key: &MutationKey,
        sql: &'static str,
    ) -> Result<(), Error> {
        let values = mutation_values(repository, key);
        self.database
            .call(move |connection| {
                let mut update = connection.prepare(sql)?;
                bind_mutation(&mut update, &values)?;
                update.next()?;
                Ok(())
            })
            .await
            .map_err(Error::from)
    }
}

#[derive(Clone)]
struct JournalEntry {
    author: PublicKey,
    result: Option<String>,
}

struct DeliveryJournalEntry {
    key: MutationKey,
    method: String,
    author: PublicKey,
    result: Option<String>,
}

fn signed_tip_reaches(
    repository: &radicle::storage::git::Repository,
    operation: EntryId,
    tip: radicle::git::Oid,
) -> Result<bool, Error> {
    if tip == operation {
        Ok(true)
    } else {
        repository
            .is_ancestor_of(operation, tip)
            .map_err(Error::from)
    }
}

fn complete_mutation(
    connection: &sqlite::ConnectionThreadSafe,
    values: &[String; 5],
    method: &str,
    author: &str,
    result: &str,
) -> sqlite::Result<()> {
    let mut update = connection.prepare(
        "UPDATE mcp_mutations SET author = ?7, result = ?8
         WHERE repository = ?1 AND delivery_repository = ?2
           AND operation = ?3 AND action = ?4
           AND invocation = ?5 AND method = ?6",
    )?;
    bind_mutation(&mut update, values)?;
    update.bind((6, method))?;
    update.bind((7, author))?;
    update.bind((8, result))?;
    update.next()?;
    Ok(())
}

fn mutation_values(repository: RepoId, key: &MutationKey) -> [String; 5] {
    [
        repository.to_string(),
        key.repository.to_string(),
        key.delivery.operation.to_string(),
        key.delivery.action.to_string(),
        encode_invocation(key),
    ]
}

fn encode_invocation(key: &MutationKey) -> String {
    format!("{:020}:{}", key.invocation, key.request)
}

fn decode_invocation(value: String) -> sqlite::Result<(usize, String)> {
    let Some((invocation, request)) = value.split_once(':') else {
        return Ok((0, value));
    };
    let invocation = invocation
        .parse()
        .map_err(|error: std::num::ParseIntError| sqlite::Error {
            code: None,
            message: Some(error.to_string()),
        })?;
    Ok((invocation, request.to_owned()))
}

fn bind_mutation(
    statement: &mut sqlite::Statement<'_>,
    values: &[String; 5],
) -> sqlite::Result<()> {
    for (index, value) in values.iter().enumerate() {
        statement.bind((index + 1, value.as_str()))?;
    }
    Ok(())
}

/// Error publishing an authorized Radicle write.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// The manifest no longer authorizes the requested method.
    #[error("MCP authorization failed: {0}")]
    Authorization(String),
    /// A recovered mutation carries no signature from the author that produced it.
    #[error("unfinished mutation was not signed by its author")]
    UnsignedRecovery,
    /// The operation completed without producing the expected Radicle change.
    #[error("mutation was not applied")]
    NotApplied,
    /// The requested change cannot be applied to the Radicle object as given.
    #[error("{0}")]
    Rejected(String),
    /// The same delivery previously recorded a different method.
    #[error("delivery mutation changed from '{recorded}' to '{requested}'")]
    MutationChanged {
        /// Method stored by the earlier attempt.
        recorded: String,
        /// Method requested now.
        requested: String,
    },
    /// A journal row vanished between insertion and lookup.
    #[error("mutation disappeared from the journal")]
    MutationMissing,
    /// A journal operation was requested while journalling is disabled.
    #[cfg(all(test, not(miri)))]
    #[error("mutation journal is disabled")]
    JournalDisabled,
    /// Reading or writing the mutation journal failed.
    #[error("mutation journal failed: {0}")]
    Journal(#[from] crate::database::Error),
    /// A stored identifier could not be parsed.
    #[error("invalid stored identifier '{value}': {source}")]
    InvalidId {
        /// Malformed value.
        value: String,
        /// Underlying parse failure.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Radicle node communication failed.
    #[error("Radicle node error: {0}")]
    Node(#[from] radicle::node::Error),
    /// Opening the Radicle repository failed.
    #[error("failed to open Radicle repository: {0}")]
    Repository(#[from] radicle::storage::RepositoryError),
    /// Reading or writing Radicle collaborative objects failed.
    #[error("failed to access Radicle collaborative objects: {0}")]
    Store(#[from] radicle::cob::store::Error),
    /// An issue operation failed.
    #[error("Radicle issue operation failed: {0}")]
    Issue(#[from] radicle::issue::Error),
    /// A patch operation failed.
    #[error("Radicle patch operation failed: {0}")]
    Patch(#[from] radicle::patch::Error),
    /// Opening a collaborative object operation stream failed.
    #[error("failed to open collaborative object operation stream: {0}")]
    Stream(#[from] radicle::cob::stream::error::Stream),
    /// Reading one collaborative object operation failed.
    #[error("failed to read collaborative object operation: {0}")]
    Operation(#[from] radicle::cob::stream::error::Ops),
    /// A Git operation failed.
    #[error("Git operation failed: {0}")]
    Git(#[from] radicle::git::raw::Error),
    /// Loading the signing device failed.
    #[error("failed to load the signing key: {0}")]
    Signer(String),
    /// Signing Radicle references failed.
    #[error("failed to sign Radicle references: {0}")]
    Signing(String),
    /// Reading an author's signed references failed.
    #[error("failed to read signed Radicle references: {0}")]
    SignedRefs(#[from] radicle::storage::refs::sigrefs::read::error::Read),
    /// A blocking publisher task failed.
    #[error("publisher task failed: {0}")]
    Task(String),
}
