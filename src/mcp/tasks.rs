//! Task admission retains the runtime's request and the kind of work it authorized.

use crate::{
    agent::{ActivityId, Source},
    mcp::{protocol::AddTaskRequest, server::ActivityAccess, tools::ToolService},
    queue::{ArtifactRole, MessageReference, TaskKind, TaskRequest},
    watcher::{Change, Comment, Delivery, Kind, Subject},
};

impl ToolService {
    pub(super) async fn task_submission(
        &self,
        active: &ActivityAccess,
        request: AddTaskRequest,
    ) -> Result<TaskRequest, String> {
        let kind = self.task_kind(active, request.kind).await?;
        self.validate_context(&request.context).await?;
        if kind == TaskKind::Review {
            self.review_task(active, &request).await?;
        }
        let mut submission = TaskRequest::new(
            request.brief,
            active.context().source.clone(),
            request.position,
        );
        submission.context.kind = kind;
        submission.context.activity = active.context().delivery.clone();
        let mut key = active.context().activity.request();
        if request.distinct {
            key.push_str(
                &crate::mcp::protocol::mutation_identity("add_task", &submission.brief)
                    .map_err(|error| error.to_string())?,
            );
        }
        submission.request = Some(key);
        submission.distinct = request.distinct;
        submission
            .context
            .update(request.context)
            .map_err(|error| error.to_string())?;
        if let (Source::Subject(subject), ActivityId::Delivery(delivery)) =
            (&active.context().source, active.context().activity)
        {
            submission.context.messages.push(MessageReference {
                subject: subject.to_string(),
                entry: delivery.operation.to_string(),
                revision: None,
                purpose: "Request accepting this work".to_owned(),
            });
        }
        Ok(submission)
    }

    async fn review_task(
        &self,
        active: &ActivityAccess,
        request: &AddTaskRequest,
    ) -> Result<(), String> {
        let target = request
            .context
            .artifacts
            .iter()
            .find(|artifact| artifact.role == ArtifactRole::Review)
            .map(|artifact| artifact.target().map_err(|error| error.to_string()))
            .transpose()?
            .or_else(|| match active.context().source {
                Source::Subject(subject @ Subject::Patch(_)) => Some(subject),
                _ => None,
            })
            .ok_or_else(|| "record the requested patch as a review artifact".to_owned())?;
        let own_review = match active
            .context()
            .delivery
            .as_ref()
            .map(|activity| activity.kind)
        {
            Some(Kind::Delivered(
                Delivery::Changed(Change::ReviewFollowUp { review, .. })
                | Delivery::Replied {
                    comment: Comment::Review { review, .. },
                    ..
                },
            )) => Some(review.to_string()),
            _ => None,
        };
        self.review_invitation(active, target, own_review.as_deref(), None)
            .await
    }
}
