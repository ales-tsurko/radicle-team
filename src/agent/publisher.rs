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

use crate::{agent::DeliveryId, database::Database};

const JOURNAL_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS mcp_mutations (
        repository TEXT NOT NULL,
        operation TEXT NOT NULL,
        action TEXT NOT NULL,
        invocation TEXT NOT NULL,
        method TEXT NOT NULL,
        author TEXT NOT NULL,
        result TEXT,
        PRIMARY KEY (repository, operation, action, invocation)
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

    pub(crate) async fn with_journal(mut self, path: &Path) -> Result<Self, String> {
        self.journal = Some(MutationJournal::open(path).await?);
        Ok(self)
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) async fn with_memory_journal(mut self) -> Result<Self, String> {
        self.journal = Some(MutationJournal::memory().await?);
        Ok(self)
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) async fn forget_completion(&self, key: &MutationKey) -> Result<(), String> {
        self.journal
            .as_ref()
            .ok_or_else(|| "mutation journal is disabled".to_owned())?
            .forget_completion(self.repository, key)
            .await
    }

    pub(crate) fn author(&self) -> PublicKey {
        *self.profile.id()
    }

    pub(crate) async fn authorize(&self, method: &str) -> Result<(), String> {
        self.authorizer.authorize(method).await
    }

    pub(crate) async fn reconcile(&self, delivery: DeliveryId) -> Result<(), String> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        let _mutation_guard = self.mutation_lock.lock().await;
        for entry in journal.entries(self.repository, delivery).await? {
            if entry.result.is_some() {
                self.finish_recovery(entry.author).await?;
                continue;
            }
            let publisher = self.clone();
            let marker = Self::marker(&entry.key);
            let author = entry.author;
            let operation = tokio::task::spawn_blocking(move || {
                let repository = publisher
                    .profile
                    .storage
                    .repository(publisher.repository)
                    .map_err(stringify)?;
                let operation = publisher.find_marker(&repository, &marker, author)?;
                if let Some(operation) = operation
                    && author != publisher.author()
                    && !Self::is_signed(&repository, author, operation)?
                {
                    return Err("unfinished mutation was not signed by its author".to_owned());
                }
                Ok(operation)
            })
            .await
            .map_err(stringify)??;
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
    ) -> Result<String, String>
    where
        T: Send + ToString + 'static,
        F: FnOnce(
                &radicle::storage::git::Repository,
                &radicle::node::device::BoxedDevice,
                &str,
            ) -> Result<T, String>
            + Send
            + 'static,
        V: Fn(
                &radicle::storage::git::Repository,
                PublicKey,
                EntryId,
                MutationValidation,
            ) -> Result<bool, String>
            + Send
            + 'static,
    {
        self.authorizer.authorize(method).await?;
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
            let repository = publisher
                .profile
                .storage
                .repository(publisher.repository)
                .map_err(|error| error.to_string())?;
            let signer = match publisher.signer.clone() {
                Some(signer) => Device::from(signer).boxed(),
                None => publisher
                    .profile
                    .signer()
                    .map_err(|error| error.to_string())?,
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
                Some(recovered) => recovered,
                None => {
                    let value = if let Some(marker) = recovery_marker.as_deref() {
                        operation(&repository, &signer, marker)?.to_string()
                    } else {
                        operation(&repository, &signer, method)?.to_string()
                    };
                    let operation = EntryId::from_str(&value).map_err(stringify)?;
                    if !validate(&repository, current, operation, MutationValidation::Fresh)? {
                        return Err("mutation was not applied".to_owned());
                    }
                    (value, current)
                }
            };
            if author == current {
                repository
                    .sign_refs(&signer)
                    .map_err(|error| error.to_string())?;
            }
            Ok::<_, String>((value, author))
        })
        .await
        .map_err(|error| error.to_string())??;
        if let (Some(journal), Some(key)) = (&self.journal, mutation.as_ref()) {
            journal
                .complete(self.repository, key, method, author, &value)
                .await?;
        }
        self.announce(author)
            .await
            .map_err(|error| error.to_string())?;
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

    async fn finish_recovery(&self, author: PublicKey) -> Result<(), String> {
        if author != self.author() {
            return self
                .announce(author)
                .await
                .map_err(|error| error.to_string());
        }
        let publisher = self.clone();
        tokio::task::spawn_blocking(move || {
            let repository = publisher
                .profile
                .storage
                .repository(publisher.repository)
                .map_err(|error| error.to_string())?;
            let signer = match publisher.signer.clone() {
                Some(signer) => Device::from(signer).boxed(),
                None => publisher
                    .profile
                    .signer()
                    .map_err(|error| error.to_string())?,
            };
            repository
                .sign_refs(&signer)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())??;
        self.announce(author)
            .await
            .map_err(|error| error.to_string())
    }

    fn find_marker(
        &self,
        repository: &radicle::storage::git::Repository,
        marker: &str,
        author: PublicKey,
    ) -> Result<Option<EntryId>, String> {
        let issues = Issues::open(repository, ReadOnly).map_err(stringify)?;
        for issue in issues.all().map_err(stringify)? {
            let (id, _issue) = issue.map_err(stringify)?;
            if let Some(operation) = self.find_in(
                repository,
                IssueStream::init(id, repository).all(),
                marker,
                author,
            )? {
                return Ok(Some(operation));
            }
        }
        let patches = Patches::open(repository, ReadOnly).map_err(stringify)?;
        for patch in patches.all().map_err(stringify)? {
            let (id, _patch) = patch.map_err(stringify)?;
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
    ) -> Result<Option<EntryId>, String> {
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
    ) -> Result<Option<EntryId>, String> {
        for operation in operations.map_err(stringify)? {
            let operation = operation.map_err(stringify)?;
            if operation.author != author || !matches(&operation) {
                continue;
            }
            let commit = repository
                .backend
                .find_commit(operation.id.into())
                .map_err(stringify)?;
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
    ) -> Result<bool, String> {
        let Some(signed) = SignedRefs::load(author, repository).map_err(stringify)? else {
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
    async fn authorize(&self, method: &str) -> Result<(), String>;
}

#[derive(Clone)]
struct MutationJournal {
    database: Database,
}

impl MutationJournal {
    async fn open(path: &Path) -> Result<Self, String> {
        Self::from_database(Database::open(path).await.map_err(stringify)?).await
    }

    #[cfg(all(test, not(miri)))]
    async fn memory() -> Result<Self, String> {
        Self::from_database(Database::memory().await.map_err(stringify)?).await
    }

    async fn from_database(database: Database) -> Result<Self, String> {
        database.execute(JOURNAL_SCHEMA).await.map_err(stringify)?;
        Ok(Self { database })
    }

    async fn begin(
        &self,
        repository: RepoId,
        key: &MutationKey,
        method: &'static str,
        author: PublicKey,
    ) -> Result<JournalEntry, String> {
        let values = mutation_values(repository, key);
        let author = author.to_string();
        self.database
            .call(move |connection| {
                let mut insert = connection.prepare(
                    "INSERT OR IGNORE INTO mcp_mutations
                         (repository, operation, action, invocation, method, author)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                bind_mutation(&mut insert, &values)?;
                insert.bind((5, method))?;
                insert.bind((6, author.as_str()))?;
                insert.next()?;
                drop(insert);

                let mut select = connection.prepare(
                    "SELECT method, author, result FROM mcp_mutations
                     WHERE repository = ?1 AND operation = ?2
                       AND action = ?3 AND invocation = ?4",
                )?;
                bind_mutation(&mut select, &values)?;
                if select.next()? != State::Row {
                    return Err(sqlite::Error {
                        code: None,
                        message: Some("mutation disappeared".to_owned()),
                    });
                }
                let stored = select.read::<String, _>(0)?;
                if stored != method {
                    return Err(sqlite::Error {
                        code: None,
                        message: Some(format!(
                            "delivery mutation changed from '{stored}' to '{method}'"
                        )),
                    });
                }
                let author = select.read::<String, _>(1)?;
                let author = PublicKey::from_str(&author).map_err(|error| sqlite::Error {
                    code: None,
                    message: Some(error.to_string()),
                })?;
                Ok(JournalEntry {
                    author,
                    result: select.read::<Option<String>, _>(2)?,
                })
            })
            .await
            .map_err(stringify)
    }

    async fn complete(
        &self,
        repository: RepoId,
        key: &MutationKey,
        method: &str,
        author: PublicKey,
        result: &str,
    ) -> Result<(), String> {
        let values = mutation_values(repository, key);
        let method = method.to_owned();
        let result = result.to_owned();
        let author = author.to_string();
        self.database
            .call(move |connection| {
                complete_mutation(connection, &values, &method, &author, &result)
            })
            .await
            .map_err(stringify)
    }

    async fn entries(
        &self,
        repository: RepoId,
        delivery: DeliveryId,
    ) -> Result<Vec<DeliveryJournalEntry>, String> {
        let values = [
            repository.to_string(),
            delivery.operation.to_string(),
            delivery.action.to_string(),
        ];
        self.database
            .call(move |connection| {
                let mut select = connection.prepare(
                    "SELECT invocation, method, author, result FROM mcp_mutations
                     WHERE repository = ?1 AND operation = ?2
                       AND action = ?3",
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
            .map_err(stringify)
    }

    async fn discard(&self, repository: RepoId, key: &MutationKey) -> Result<(), String> {
        self.update_entry(
            repository,
            key,
            "DELETE FROM mcp_mutations
             WHERE repository = ?1 AND operation = ?2
               AND action = ?3 AND invocation = ?4 AND result IS NULL",
        )
        .await
    }

    #[cfg(all(test, not(miri)))]
    async fn forget_completion(&self, repository: RepoId, key: &MutationKey) -> Result<(), String> {
        self.update_entry(
            repository,
            key,
            "UPDATE mcp_mutations SET result = NULL
             WHERE repository = ?1 AND operation = ?2
               AND action = ?3 AND invocation = ?4",
        )
        .await
    }

    async fn update_entry(
        &self,
        repository: RepoId,
        key: &MutationKey,
        sql: &'static str,
    ) -> Result<(), String> {
        let values = mutation_values(repository, key);
        self.database
            .call(move |connection| {
                let mut update = connection.prepare(sql)?;
                bind_mutation(&mut update, &values)?;
                update.next()?;
                Ok(())
            })
            .await
            .map_err(stringify)
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
) -> Result<bool, String> {
    if tip == operation {
        Ok(true)
    } else {
        repository.is_ancestor_of(operation, tip).map_err(stringify)
    }
}

fn stringify(error: impl fmt::Display) -> String {
    error.to_string()
}

fn complete_mutation(
    connection: &sqlite::ConnectionThreadSafe,
    values: &[String; 4],
    method: &str,
    author: &str,
    result: &str,
) -> sqlite::Result<()> {
    let mut update = connection.prepare(
        "UPDATE mcp_mutations SET author = ?6, result = ?7
         WHERE repository = ?1 AND operation = ?2
           AND action = ?3 AND invocation = ?4 AND method = ?5",
    )?;
    bind_mutation(&mut update, values)?;
    update.bind((5, method))?;
    update.bind((6, author))?;
    update.bind((7, result))?;
    update.next()?;
    Ok(())
}

fn mutation_values(repository: RepoId, key: &MutationKey) -> [String; 4] {
    [
        repository.to_string(),
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
    values: &[String; 4],
) -> sqlite::Result<()> {
    for (index, value) in values.iter().enumerate() {
        statement.bind((index + 1, value.as_str()))?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("Radicle node error: {0}")]
    Node(#[from] radicle::node::Error),
    #[error("publisher task failed: {0}")]
    Task(String),
}
