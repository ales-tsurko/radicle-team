//! Automatic acknowledgement of durable admission, using the member's configured reaction.

use radicle::{
    cob::{EntryId, Reaction, issue, patch},
    crypto::{PublicKey, Signer as _},
};

use crate::{
    agent::{MutationKey, PublishError, Publisher},
    watcher::{Activity, Change, Comment, Delivery, Kind, Subject, Target},
};

#[derive(Clone, Copy)]
enum Receipt {
    Issue(EntryId),
    Revision(radicle::cob::patch::RevisionId, Option<EntryId>),
    Review(radicle::cob::patch::ReviewId, Option<EntryId>),
    Latest,
}

impl Receipt {
    fn for_activity(activity: &Activity) -> Self {
        let comment = match activity.kind {
            Kind::Delivered(
                Delivery::Mentioned(Target::Comment(comment))
                | Delivery::Replied { comment, .. }
                | Delivery::Changed(
                    Change::CommentCreated(comment)
                    | Change::CommentEdited(comment)
                    | Change::CommentResolution { comment, .. },
                ),
            ) => Some(comment),
            _ => None,
        };
        if let Some(comment) = comment {
            return match comment {
                Comment::Issue(id) => Self::Issue(id),
                Comment::Revision { revision, comment } => Self::Revision(revision, Some(comment)),
                Comment::Review { review, comment } => Self::Review(review, Some(comment)),
            };
        }
        match activity.kind {
            Kind::Delivered(
                Delivery::Mentioned(Target::Review(review))
                | Delivery::Changed(
                    Change::ReviewSubmitted { review, .. } | Change::ReviewEdited(review),
                ),
            ) => Self::Review(review, None),
            Kind::Delivered(
                Delivery::Mentioned(Target::Revision(revision))
                | Delivery::Changed(
                    Change::RevisionCreated(revision)
                    | Change::RevisionEdited(revision)
                    | Change::ReviewFollowUp { revision, .. },
                ),
            ) => Self::Revision(revision, None),
            _ => match activity.subject {
                Subject::Issue(id) => Self::Issue(*id),
                Subject::Patch(_) => Self::Latest,
            },
        }
    }

    fn apply(
        self,
        repo: &radicle::storage::git::Repository,
        signer: &radicle::profile::Signer,
        subject: Subject,
        reaction: Reaction,
        marker: &str,
    ) -> Result<EntryId, PublishError> {
        if self.applied(repo, *signer.public_key(), subject, reaction)? {
            return Ok(*subject.id());
        }
        match (subject, self) {
            (Subject::Issue(id), Self::Issue(comment)) => {
                let mut store = issue::Cache::no_cache(repo, signer)?;
                let mut issue = store.get_mut(&id)?;
                Ok(issue.transaction(marker, |tx| tx.react(comment, reaction, true))?)
            }
            (Subject::Patch(id), target) => {
                let mut store = patch::Cache::no_cache(repo, signer)?;
                let mut patch = store.get_mut(&id)?;
                let latest = patch.latest().0;
                Ok(patch.transaction(marker, |tx| match target {
                    Self::Review(review, Some(comment)) => {
                        tx.react_review_comment(review, comment, reaction, true)
                    }
                    Self::Review(review, None) => tx.review_react(review, reaction, true),
                    Self::Revision(revision, Some(comment)) => {
                        tx.comment_react(revision, comment, reaction, true)
                    }
                    Self::Revision(revision, None) => tx.react(revision, reaction, None, true),
                    Self::Latest => tx.react(latest, reaction, None, true),
                    Self::Issue(id) => Err(radicle::cob::store::Error::NotFound(
                        issue::TYPENAME.clone(),
                        id.into(),
                    )),
                })?)
            }
            _ => Err(PublishError::Rejected("invalid receipt target".into())),
        }
    }

    fn applied(
        self,
        repo: &radicle::storage::git::Repository,
        author: PublicKey,
        subject: Subject,
        reaction: Reaction,
    ) -> Result<bool, PublishError> {
        use radicle::cob::store::access::ReadOnly;
        let present = (author, reaction);
        match (subject, self) {
            (Subject::Issue(id), Self::Issue(comment)) => {
                let store = radicle::cob::issue::Issues::open(repo, ReadOnly)?;
                Ok(store
                    .get(&id)?
                    .and_then(|issue| {
                        issue.thread().comment(&comment).map(|comment| {
                            comment
                                .reactions()
                                .get(&reaction)
                                .is_some_and(|authors| authors.contains(&&author))
                        })
                    })
                    .unwrap_or(false))
            }
            (Subject::Patch(id), target) => {
                let store = radicle::cob::patch::Patches::open(repo, ReadOnly)?;
                let Some(patch) = store.get(&id)? else {
                    return Ok(false);
                };
                match target {
                    Self::Review(id, comment) => {
                        let review = patch
                            .revisions()
                            .flat_map(|(_, revision)| revision.reviews().map(|(_, review)| review))
                            .find(|review| review.id() == id);
                        Ok(review.is_some_and(|review| match comment {
                            None => review.reactions().contains(&present),
                            Some(id) => review
                                .comments()
                                .find(|(entry, _)| **entry == id)
                                .is_some_and(|(_, comment)| {
                                    comment
                                        .reactions()
                                        .get(&reaction)
                                        .is_some_and(|authors| authors.contains(&&author))
                                }),
                        }))
                    }
                    Self::Revision(id, comment) => {
                        Ok(patch.revision(&id).is_some_and(|revision| match comment {
                            None => revision
                                .reactions()
                                .get(&None)
                                .is_some_and(|reactions| reactions.contains(&present)),
                            Some(id) => revision.discussion().comment(&id).is_some_and(|comment| {
                                comment
                                    .reactions()
                                    .get(&reaction)
                                    .is_some_and(|authors| authors.contains(&&author))
                            }),
                        }))
                    }
                    Self::Latest => Ok(patch
                        .latest()
                        .1
                        .reactions()
                        .get(&None)
                        .is_some_and(|reactions| reactions.contains(&present))),
                    Self::Issue(_) => Ok(false),
                }
            }
            _ => Ok(false),
        }
    }
}

impl Publisher {
    pub(crate) async fn receipt(
        &self,
        activity: &Activity,
        emoji: char,
    ) -> Result<(), PublishError> {
        // Disabling reactions also disables runtime reactions, while signed status stays available.
        if let Err(PublishError::Authorization(_)) = self.authorize("set_reaction").await {
            return Ok(());
        }
        let reaction =
            Reaction::new(emoji).map_err(|error| PublishError::Rejected(error.to_string()))?;
        let target = Receipt::for_activity(activity);
        let subject = activity.subject;
        self.mutate(
            "set_reaction",
            Some(MutationKey {
                repository: activity.repository,
                activity: crate::agent::ActivityId::Receipt(activity.delivery_id()),
                invocation: 0,
                request: format!("queued-receipt:{emoji}"),
            }),
            move |repo, signer, marker| target.apply(repo, signer, subject, reaction, marker),
            move |repo, author, _, _| target.applied(repo, author, subject, reaction),
        )
        .await?;
        Ok(())
    }
}
