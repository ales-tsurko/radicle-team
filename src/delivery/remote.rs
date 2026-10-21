//! Signed, member-owned delivery records replicated as ordinary Radicle COBs.

use std::{collections::BTreeMap, sync::LazyLock};

use radicle::{
    cob::{
        self, EntryId, Op, TypeName,
        store::{
            Cob, CobAction, CobWithType, Store,
            access::{ReadOnly, WriteAs},
        },
    },
    crypto::{PublicKey, Signer as _},
    storage::{ReadRepository, git::Repository},
};
use serde::{Deserialize, Serialize};

use crate::{
    agent::{MutationKey, Publisher},
    delivery::{Error, Inbox, Record, State},
    member::Member,
};

static TYPENAME: LazyLock<TypeName> =
    LazyLock::new(|| "xyz.radicle.team.delivery".parse().expect("valid COB type"));

#[derive(Debug, Clone)]
pub(crate) struct Deliveries {
    pub(crate) author: PublicKey,
    pub(crate) member: Member,
    pub(crate) records: BTreeMap<String, Record>,
    latest: EntryId,
}

impl Deliveries {
    fn apply(&mut self, op: Op<Update>) {
        // Replication may include operations from other namespaces. Only the root signer owns
        // this member's record; another contributor cannot forge its queue acknowledgements.
        if op.author != self.author {
            return;
        }
        for update in op.actions {
            if update.member != self.member {
                continue;
            }
            let id = update.record.id();
            if self.records.get(&id).is_none_or(|saved| {
                (saved.received, saved.revision) < (update.record.received, update.record.revision)
            }) {
                self.records.insert(id, update.record);
                self.latest = op.id;
            }
        }
    }

    pub(crate) fn read(repository: &Repository) -> Result<Vec<Self>, Error> {
        let manifest = crate::manifest::Manifest::load_repository(repository)?;
        let store = Store::<Self, _, _>::open(repository, ReadOnly)?;
        let mut records = Vec::new();
        for delivery in store.all()? {
            let (_, delivery) = delivery?;
            if manifest
                .validate_member(delivery.member, delivery.author.into())
                .is_ok()
            {
                records.push(delivery);
            }
        }
        Ok(records)
    }

    fn write(
        repository: &Repository,
        signer: &radicle::profile::Signer,
        member: Member,
        record: Record,
        marker: &str,
    ) -> Result<EntryId, crate::agent::PublishError> {
        let mut store = Store::<Self, _, _>::open(repository, WriteAs::new(signer))?;
        let existing = store
            .all()?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|(_, object)| object.author == *signer.public_key() && object.member == member);
        let update = Update { member, record };
        if let Some((id, object)) = existing {
            if object
                .records
                .get(&update.record.id())
                .is_some_and(|saved| {
                    (saved.received, saved.revision)
                        >= (update.record.received, update.record.revision)
                })
            {
                return Ok(object.latest);
            }
            Ok(store
                .update(&TYPENAME, id, marker, (update, Vec::new()), vec![])?
                .head)
        } else {
            Ok(*store.create(marker, (update, Vec::new()), vec![])?.0)
        }
    }
}

impl CobWithType for Deliveries {
    fn type_name() -> &'static TypeName {
        &TYPENAME
    }
}

impl Cob for Deliveries {
    type Action = Update;
    type Error = DecodeError;

    fn from_root<R: ReadRepository>(op: Op<Update>, _repo: &R) -> Result<Self, DecodeError> {
        let mut result = Self {
            author: op.author,
            member: op.actions.head.member,
            records: BTreeMap::new(),
            latest: op.id,
        };
        result.apply(op);
        Ok(result)
    }

    fn op<'a, R: ReadRepository, I: IntoIterator<Item = &'a cob::Entry>>(
        &mut self,
        op: Op<Update>,
        _concurrent: I,
        _repo: &R,
    ) -> Result<(), DecodeError> {
        self.apply(op);
        Ok(())
    }
}

impl<R: ReadRepository> cob::Evaluate<R> for Deliveries {
    type Error = DecodeError;

    fn init(entry: &cob::Entry, repo: &R) -> Result<Self, DecodeError> {
        Self::from_root(Op::try_from(entry)?, repo)
    }

    fn apply<'a, I: Iterator<Item = (&'a EntryId, &'a cob::Entry)>>(
        &mut self,
        entry: &cob::Entry,
        _concurrent: I,
        _repo: &R,
    ) -> Result<(), DecodeError> {
        self.apply(Op::try_from(entry)?);
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Update {
    member: Member,
    record: Record,
}
impl CobAction for Update {}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DecodeError {
    #[error(transparent)]
    Operation(#[from] radicle::cob::op::OpEncodingError),
}

impl Publisher {
    async fn delivery(&self, member: Member, record: Record) -> Result<(), Error> {
        let id = record.id();
        let revision = record.revision;
        let received = record.received;
        self.mutate(
            "delivery_status",
            Some(MutationKey {
                repository: record.activity.repository,
                activity: crate::agent::ActivityId::Receipt(record.activity.delivery_id()),
                invocation: 0,
                request: format!("delivery-status:{received}:{revision}"),
            }),
            move |repo, signer, marker| Deliveries::write(repo, signer, member, record, marker),
            move |repo, author, _operation, _validation| {
                let store = Store::<Deliveries, _, _>::open(repo, ReadOnly)?;
                for delivery in store.all()? {
                    let (_, delivery) = delivery?;
                    if delivery.author == author
                        && delivery.member == member
                        && delivery.records.get(&id).is_some_and(|record| {
                            (record.received, record.revision) >= (received, revision)
                        })
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            },
        )
        .await?;
        Ok(())
    }
}

impl Inbox {
    /// Publishes queue admission without calling or interrupting the model. A failed publication
    /// remains pending; retrying cannot duplicate a reaction or an already-saved state revision.
    pub(crate) async fn publish(
        &self,
        publisher: &Publisher,
        member: Member,
        emoji: char,
    ) -> Result<(), Error> {
        loop {
            let changed = self.changed();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let mut failed = false;
            for (record, mut receipt) in self.pending().await? {
                if !receipt && record.state != State::Ignored {
                    match publisher.receipt(&record.activity, emoji).await {
                        Ok(()) => receipt = true,
                        Err(error) => {
                            tracing::warn!(%error, operation = %record.activity.operation, "queue acknowledgement reaction will retry");
                            failed = true;
                        }
                    }
                }
                let result = async {
                    publisher.delivery(member, record.clone()).await?;
                    self.published(&record, receipt).await
                }
                .await;
                if let Err(error) = result {
                    tracing::warn!(%error, operation = %record.activity.operation, "delivery state publication will retry");
                    failed = true;
                }
            }
            if failed {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            } else {
                tokio::select! { () = changed => {}, () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {} }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use radicle::{
        cob::{Manifest, Op, Timestamp, Version},
        crypto::PublicKey,
    };

    use crate::{
        delivery::{
            Record, State,
            remote::{Deliveries, TYPENAME, Update},
        },
        member::Member,
        watcher::{Activity, Delivery, Kind, Subject, Target},
    };

    #[test]
    fn another_signer_cannot_change_a_members_delivery_state() {
        let owner: PublicKey = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7"
            .parse()
            .expect("owner");
        let stranger: PublicKey = "z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C"
            .parse()
            .expect("stranger");
        let id: radicle::cob::EntryId = "2222222222222222222222222222222222222222"
            .parse()
            .expect("operation");
        let record = Record::new(
            Activity {
                repository: "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc".parse().expect("RID"),
                subject: Subject::Issue(id.into()),
                operation: id,
                action: 0,
                author: owner.into(),
                kind: Kind::Delivered(Delivery::Mentioned(Target::Subject)),
            },
            None,
        );
        let key = record.id();
        let mut deliveries = Deliveries {
            author: owner,
            member: Member::Nico,
            records: [(key.clone(), record.clone())].into(),
            latest: id,
        };
        let mut forged = record;
        forged.state = State::Handled;
        forged.revision = 100;
        let update = Update {
            member: Member::Nico,
            record: forged,
        };
        let op = |author| {
            Op::new(
                id,
                (update.clone(), Vec::new()),
                author,
                Timestamp::from_secs(0),
                None,
                Manifest::new(TYPENAME.clone(), Version::default()),
            )
        };
        deliveries.apply(op(stranger));
        assert_eq!(deliveries.records[&key].state, State::Queued);
        deliveries.apply(op(owner));
        assert_eq!(deliveries.records[&key].state, State::Handled);
        let mut renewed = deliveries.records[&key].clone();
        renewed.received += 1;
        renewed.revision = 1;
        renewed.state = State::Queued;
        deliveries.apply(Op::new(
            id,
            (
                Update {
                    member: Member::Nico,
                    record: renewed,
                },
                Vec::new(),
            ),
            owner,
            Timestamp::from_secs(0),
            None,
            Manifest::new(TYPENAME.clone(), Version::default()),
        ));
        assert_eq!(
            deliveries.records[&key].state,
            State::Queued,
            "reset local state must not keep an old remote completion"
        );
    }
}
