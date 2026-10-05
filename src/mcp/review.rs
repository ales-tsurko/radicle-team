//! Review invitations and thread boundaries, independent of readable task relationships.

use std::str::FromStr as _;

use radicle::{
    cob::{
        EntryId,
        patch::{self, Patches},
        store::access::ReadOnly,
    },
    identity::Did,
    storage::ReadStorage as _,
};

use crate::{
    agent::{ActivityId, Source},
    mcp::{
        protocol::{ReviewWriteRequest, SubjectRequest},
        recovery::find_review,
        server::{ActivityAccess, ServiceError},
        tools::ToolService,
    },
    queue::TaskKind,
    watcher::{Activity, Change, Comment, Delivery, Kind, Subject},
};

impl ToolService {
    pub(super) async fn task_kind(
        &self,
        access: &ActivityAccess,
        requested: Option<TaskKind>,
    ) -> Result<TaskKind, String> {
        let Source::Subject(Subject::Patch(id)) = access.context().source else {
            return Ok(requested.unwrap_or_default());
        };
        let owned = self
            .blocking(move |service| {
                let repository = service.storage.repository(service.repository)?;
                let patch = Patches::open(&repository, ReadOnly)?
                    .get(&id)?
                    .ok_or_else(|| ServiceError::NotFound(id.to_string()))?;
                Ok(patch.author().public_key() == &service.publisher.author())
            })
            .await
            .map_err(|error| error.to_string())?;
        let kind = requested.unwrap_or(if owned {
            TaskKind::Implementation
        } else {
            TaskKind::Review
        });
        if kind == TaskKind::Implementation
            && !owned
            && !access
                .context()
                .delivery
                .as_ref()
                .is_some_and(Activity::direct)
        {
            return Err(
                "a review assignment does not authorize implementing another author's feedback; \
                 an explicit handoff or mention is required"
                    .to_owned(),
            );
        }
        Ok(kind)
    }

    pub(super) async fn verify_review(
        &self,
        access: &ActivityAccess,
        request: &ReviewWriteRequest,
    ) -> Result<(), String> {
        let target = request.subject.target()?;
        if request.review.is_none()
            && access.context().delivery.as_ref().is_some_and(|activity| {
                matches!(
                    activity.kind,
                    Kind::Delivered(Delivery::Changed(Change::ReviewCommentsResolved))
                )
            })
        {
            return Err("pass your own resolved review ID when checking these findings".to_owned());
        }
        if let ActivityId::Task(id) = access.context().activity {
            let task = self
                .queue
                .get(id)
                .await
                .map_err(|error| error.to_string())?;
            if task.kind() != TaskKind::Review {
                return Err("this is an implementation task, not an invited review".to_owned());
            }
        }
        self.review_invitation(
            access,
            target,
            request.review.as_deref(),
            request.revision.as_deref(),
        )
        .await
    }

    pub(super) async fn review_invitation(
        &self,
        access: &ActivityAccess,
        target: Subject,
        requested_review: Option<&str>,
        requested_revision: Option<&str>,
    ) -> Result<(), String> {
        let Subject::Patch(id) = target else {
            return Err("reviews require a patch".to_owned());
        };
        let activity = access.context().delivery.clone();
        let chat = matches!(access.context().source, Source::Chat(_));
        let review = requested_review.map(str::to_owned);
        let revision = requested_revision.map(str::to_owned);
        let related_invitation = self.mentioned_patch(access, target).await?;
        self.blocking(move |service| {
            let repository = service.storage.repository(service.repository)?;
            let patch = Patches::open(&repository, radicle::cob::store::access::ReadOnly)?
                .get(&id)?
                .ok_or_else(|| ServiceError::NotFound(id.to_string()))?;
            if patch.author().public_key() == &service.publisher.author() {
                return Err(ServiceError::Request(
                    "you cannot review your own patch".to_owned(),
                ));
            }
            let scope = ReviewInvitation::from_activity(activity.as_ref(), target);
            let scope = if chat || related_invitation {
                ReviewInvitation::Requested
            } else {
                scope
            };
            scope.verify(
                &patch,
                &repository,
                service.publisher.author(),
                activity.as_ref(),
                review.as_deref(),
                revision.as_deref(),
            )
        })
        .await
        .map_err(|error| error.to_string())
    }

    pub(super) async fn verify_review_thread(
        &self,
        access: &ActivityAccess,
        subject: &SubjectRequest,
        review: &str,
    ) -> Result<(), String> {
        let target = subject.target()?;
        let review_id =
            patch::ReviewId::from(EntryId::from_str(review).map_err(|error| error.to_string())?);
        let activity = access.context().delivery.clone();
        self.blocking(move |service| {
            let repository = service.storage.repository(service.repository)?;
            let patch = Patches::open(&repository, radicle::cob::store::access::ReadOnly)?
                .get(&target.id())?
                .ok_or_else(|| ServiceError::NotFound(target.to_string()))?;
            let review = find_review(&patch, review_id)
                .ok_or_else(|| ServiceError::NotFound(review_id.to_string()))?;
            let author = &service.publisher.author();
            if patch.author().public_key() == author
                || review.author().public_key() == author
                || activity.as_ref().is_some_and(|activity| {
                    activity.subject == target && activity.addresses_review(review_id)
                })
            {
                return Ok(());
            }
            Err(ServiceError::Request(format!(
                "review {review_id} belongs to another author; respond only when addressed in \
                 that review"
            )))
        })
        .await
        .map_err(|error| error.to_string())
    }
}

#[derive(Debug)]
enum ReviewInvitation {
    Requested,
    Assigned,
    FollowUp {
        review: patch::ReviewId,
        revision: Option<patch::RevisionId>,
    },
    Resolved(patch::RevisionId),
    Absent,
}

impl ReviewInvitation {
    fn from_activity(activity: Option<&Activity>, target: Subject) -> Self {
        let Some(activity) = activity.filter(|activity| activity.subject == target) else {
            return Self::Absent;
        };
        match activity.kind {
            Kind::Assigned => Self::Assigned,
            Kind::Delivered(Delivery::Mentioned(_)) => Self::Requested,
            Kind::Delivered(Delivery::Replied {
                comment: Comment::Review { review, .. },
                ..
            }) => Self::FollowUp {
                review,
                revision: None,
            },
            Kind::Delivered(Delivery::Replied { .. }) => Self::Requested,
            Kind::Delivered(Delivery::Changed(Change::ReviewFollowUp { review, revision })) => {
                Self::FollowUp {
                    review,
                    revision: Some(revision),
                }
            }
            Kind::Delivered(Delivery::Changed(Change::ReviewCommentsResolved)) => {
                Self::Resolved(activity.operation.into())
            }
            Kind::Delivered(Delivery::Changed(Change::PatchReady | Change::RevisionCreated(_))) => {
                Self::Assigned
            }
            _ => Self::Absent,
        }
    }

    fn verify(
        self,
        patch: &patch::Patch,
        repository: &radicle::storage::git::Repository,
        author: radicle::crypto::PublicKey,
        activity: Option<&Activity>,
        requested_review: Option<&str>,
        requested_revision: Option<&str>,
    ) -> Result<(), ServiceError> {
        match self {
            Self::Requested => {}
            Self::Assigned if patch.assignees().any(|did| did == Did::from(author)) => {
                if let Some(review) = patch
                    .revisions()
                    .flat_map(|(_, revision)| revision.reviews())
                    .rfind(|(_, review)| review.author().public_key() == &author)
                    .map(|(_, review)| review)
                    && !activity.is_some_and(|activity| {
                        matches!(activity.kind, Kind::Assigned)
                            && repository
                                .backend
                                .graph_descendant_of(
                                    activity.operation.into(),
                                    (*review.id()).into(),
                                )
                                .unwrap_or(false)
                    })
                {
                    return Err(ServiceError::Request(
                        "this assignment already has your review; follow only your rejected \
                         findings or a new explicit request"
                            .to_owned(),
                    ));
                }
            }
            Self::FollowUp { review, revision } => {
                let own = find_review(patch, review)
                    .filter(|review| review.author().public_key() == &author)
                    .ok_or_else(|| {
                        ServiceError::Request("follow-up must reference your own review".to_owned())
                    })?;
                if revision.is_some() && own.verdict() != Some(patch::Verdict::Reject) {
                    return Err(ServiceError::Request(
                        "an accepted review does not authorize reviewing another author's fixes"
                            .to_owned(),
                    ));
                }
                if requested_review != Some(own.id().to_string().as_str()) {
                    return Err(ServiceError::Request(format!(
                        "this delivery follows your review {}; pass that ID as review and check \
                         only its findings",
                        own.id()
                    )));
                }
                if let Some(revision) = revision
                    && requested_revision.is_some_and(|requested| requested != revision.to_string())
                {
                    return Err(ServiceError::Request(format!(
                        "this follow-up concerns revision {revision}"
                    )));
                }
            }
            Self::Resolved(revision) => {
                let related = patch.revision(&revision).is_some_and(|revision| {
                    revision.resolves().iter().any(|(id, _)| {
                        find_review(patch, (*id).into()).is_some_and(|review| {
                            review.author().public_key() == &author
                                && requested_review
                                    .is_none_or(|requested| requested == id.to_string())
                        })
                    })
                });
                if !related {
                    return Err(ServiceError::Request(
                        "this revision does not resolve findings from the specified own review"
                            .to_owned(),
                    ));
                }
            }
            _ => {
                return Err(ServiceError::Request(
                    "reading a linked patch does not invite a review; wait for an assignment or \
                     explicit mention"
                        .to_owned(),
                ));
            }
        }
        if let Some(revision) = requested_revision {
            let expected = activity
                .and_then(Activity::review_revision)
                .unwrap_or_else(|| patch.latest().0);
            if revision != expected.to_string() {
                return Err(ServiceError::Request(format!(
                    "this review invitation concerns revision {expected}"
                )));
            }
        }
        Ok(())
    }
}
