//! Task relationships verified independently of member-written working notes.

use std::{future::Future, str::FromStr as _};

use radicle::{
    identity::Did,
    storage::{ReadRepository as _, ReadStorage as _},
};
use serde_json::Value;
use rmcp::Json;

use crate::{
    agent::{ActivityId, Source},
    manifest::Manifest,
    mcp::{
        protocol::{
            EntryRequest, PatchEntity, PatchEntityRequest, SubjectRequest, TaskAction, TaskSummary,
            TimelineRequest,
        },
        server::ActivityAccess,
        tools::ToolService,
    },
    queue::{Artifact, ArtifactRole, ContextUpdate, Task},
    watcher::{Delivery, Kind, Subject},
};

impl ToolService {
    pub(super) async fn mentioned_patch(
        &self,
        access: &ActivityAccess,
        target: Subject,
    ) -> Result<bool, String> {
        let Some(activity) = access.context().delivery.as_ref().filter(|activity| {
            activity.subject != target
                && matches!(activity.kind, Kind::Delivered(Delivery::Mentioned(_)))
        }) else {
            return Ok(false);
        };
        let request = EntryRequest {
            subject: SubjectRequest::from(activity.subject),
            entry: activity.operation.to_string(),
        };
        self.blocking(move |service| service.entry(request))
            .await
            .map(|entry| entry.to_string().contains(&target.id().to_string()))
            .map_err(|error| error.to_string())
    }

    pub(super) async fn implementation_subject(
        &self,
        access: &ActivityAccess,
        subject: &SubjectRequest,
    ) -> Result<(), String> {
        let target = subject.target()?;
        let owned = self
            .blocking(move |service| {
                let repository = service.storage.repository(service.repository)?;
                let patch = radicle::cob::patch::Patches::open(
                    &repository,
                    radicle::cob::store::access::ReadOnly,
                )?
                .get(&target.id())?
                .ok_or_else(|| crate::mcp::server::ServiceError::NotFound(target.to_string()))?;
                Ok(patch.author().public_key() == &service.publisher.author())
            })
            .await
            .map_err(|error| error.to_string())?;
        let directly_addressed = access
            .context()
            .delivery
            .as_ref()
            .is_some_and(|activity| activity.subject == target && activity.direct());
        if owned
            || directly_addressed
            || matches!(access.context().source, Source::Chat(_))
            || self.mentioned_patch(access, target).await?
        {
            return Ok(());
        }
        Err(
            "this patch belongs to another author; a review or saved reference does not authorize \
             changing it. An explicit implementation request is required"
                .to_owned(),
        )
    }

    pub(super) async fn require_implementation(
        &self,
        access: &ActivityAccess,
    ) -> Result<(), String> {
        if self.role != crate::agent::Role::Coder {
            return Err(
                "this role investigates locally; use add_task for durable changes and publication"
                    .to_owned(),
            );
        }
        if let ActivityId::Task(id) = access.context().activity {
            let task = self
                .queue
                .get(id)
                .await
                .map_err(|error| error.to_string())?;
            if task.kind() == crate::queue::TaskKind::Review {
                return Err(
                    "this task was assigned for review, not implementation; report your own \
                     findings"
                        .to_owned(),
                );
            }
        }
        self.building_may_write(access).await
    }

    pub(super) async fn comment_boundary(
        &self,
        access: &ActivityAccess,
        request: &crate::mcp::protocol::CommentWriteRequest,
    ) -> Result<(), String> {
        if matches!(
            request.target,
            crate::mcp::protocol::CommentTargetKind::Review
        ) && let Some(review) = &request.entity
        {
            self.verify_review_thread(access, &request.subject, review)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn active(&self) -> Result<ActivityAccess, String> {
        let access = self.scope.access_current().await?;
        if access.context().repository != self.repository {
            return Err("the active task belongs to a different repository".to_owned());
        }
        Ok(access)
    }

    /// Verifies write access without treating saved references as permission grants.
    pub(super) async fn access_subject(
        &self,
        request: &SubjectRequest,
    ) -> Result<ActivityAccess, String> {
        let access = self.active().await?;
        self.verify_subject(&access, request).await?;
        Ok(access)
    }

    pub(super) async fn verify_subject(
        &self,
        access: &ActivityAccess,
        request: &SubjectRequest,
    ) -> Result<(), String> {
        self.building_may_write(access).await?;
        let denial = match access.require_subject(request).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let subject = request.target()?;
        let created = self.created_here(access, subject).await?;
        if !created && !self.related(&access.context().source, subject).await? {
            return Err(denial);
        }
        self.record_relation(access, subject, ArtifactRole::Supporting)
            .await?;
        Ok(())
    }

    async fn created_here(
        &self,
        access: &ActivityAccess,
        subject: Subject,
    ) -> Result<bool, String> {
        if let ActivityId::Task(task) = access.context().activity {
            let task = self
                .queue
                .get(task)
                .await
                .map_err(|error| error.to_string())?;
            if task.context.artifacts.iter().any(|artifact| {
                artifact.role == ArtifactRole::Produced && artifact.subject == subject.to_string()
            }) {
                return Ok(true);
            }
        }
        let method = match subject {
            Subject::Issue(_) => "create_issue",
            Subject::Patch(_) => "create_patch",
        };
        self.publisher
            .created(access.context().activity, method)
            .await
            .map(|ids| ids.contains(&subject.id().to_string()))
            .map_err(|error| error.to_string())
    }

    /// A member saving a reference cannot invent a relationship or enlarge its repository.
    async fn related(&self, source: &Source, target: Subject) -> Result<bool, String> {
        let Source::Subject(origin) = source else {
            return Ok(true);
        };
        if *origin == target {
            return Ok(true);
        }
        let origin = *origin;
        self.blocking(move |service| {
            let repository = service.storage.repository(service.repository)?;
            let delegates = repository.delegates()?.into_iter().collect::<Vec<_>>();
            let manifest = Manifest::load_optional_repository(&repository)?;
            let origin_state = service.subject(SubjectRequest::from(origin))?;
            let target_state = service.subject(SubjectRequest::from(target))?;
            if service.trusted_reference(&origin_state, target, manifest.as_ref(), &delegates)
                || service.trusted_reference(&target_state, origin, manifest.as_ref(), &delegates)
            {
                return Ok(true);
            }
            Ok(
                service.discussion_references(origin, target, manifest.as_ref(), &delegates)?
                    || service.discussion_references(
                        target,
                        origin,
                        manifest.as_ref(),
                        &delegates,
                    )?,
            )
        })
        .await
        .map_err(|error| error.to_string())
    }

    fn discussion_references(
        &self,
        origin: Subject,
        target: Subject,
        manifest: Option<&Manifest>,
        delegates: &[Did],
    ) -> Result<bool, crate::mcp::server::ServiceError> {
        let mut cursor = None;
        loop {
            let page = self.timeline(TimelineRequest {
                subject: SubjectRequest::from(origin),
                cursor,
                limit: Some(200),
            })?;
            if page
                .entries
                .iter()
                .any(|entry| self.trusted_reference(entry, target, manifest, delegates))
            {
                return Ok(true);
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(false),
            }
        }
    }

    fn trusted_reference(
        &self,
        value: &Value,
        target: Subject,
        manifest: Option<&Manifest>,
        delegates: &[Did],
    ) -> bool {
        let Some(author) = value.get("author").and_then(Value::as_str) else {
            return false;
        };
        let author = Did::from_str(author)
            .or_else(|_| radicle::crypto::PublicKey::from_str(author).map(Did::from));
        let Ok(author) = author else {
            return false;
        };
        let allowed = author == Did::from(self.publisher.author())
            || manifest.map_or_else(
                || delegates.contains(&author),
                |manifest| manifest.allows_request(author, delegates),
            );
        allowed && value.to_string().contains(&target.id().to_string())
    }

    pub(super) async fn task_access(
        &self,
        access: &ActivityAccess,
        task: i64,
    ) -> Result<Task, String> {
        let task = self
            .queue
            .get(task)
            .await
            .map_err(|error| error.to_string())?;
        let matches = match access.context().activity {
            ActivityId::Task(active) => active == task.id,
            _ => {
                matches!(access.context().source, Source::Chat(_))
                    || access.context().source == task.source
                    || access.context().source == task.target()
                    || task.context.artifacts.iter().any(|artifact| {
                        artifact.role == ArtifactRole::Produced
                            && artifact.subject == access.context().source.to_string()
                    })
            }
        };
        if !matches {
            return Err(format!(
                "task {} belongs to {}, not this activity from {}; use task_status to select its \
                 related task",
                task.id,
                task.source,
                access.context().source
            ));
        }
        Ok(task)
    }

    pub(super) async fn building_may_write(&self, access: &ActivityAccess) -> Result<(), String> {
        if let ActivityId::Task(task) = access.context().activity {
            let task = self
                .queue
                .get(task)
                .await
                .map_err(|error| error.to_string())?;
            if task.context.cancelled {
                return Err("this task was cancelled; preserve local work and stop".to_owned());
            }
        }
        Ok(())
    }

    pub(super) async fn task_action(
        &self,
        task: i64,
        action: impl Future<Output = Result<Task, crate::queue::Error>>,
    ) -> Result<Json<TaskAction>, String> {
        let access = self.active().await?;
        self.task_access(&access, task).await?;
        let task = action.await.map_err(|error| error.to_string())?;
        Ok(Json(TaskAction {
            task: TaskSummary::from(task),
        }))
    }

    pub(super) async fn validate_context(&self, update: &ContextUpdate) -> Result<(), String> {
        for artifact in &update.artifacts {
            let subject = artifact.target().map_err(|error| error.to_string())?;
            self.blocking(move |service| service.subject(SubjectRequest::from(subject)))
                .await
                .map_err(|error| error.to_string())?;
        }
        for message in &update.messages {
            let subject = Subject::from_str(&message.subject).map_err(|error| error.to_string())?;
            let request = EntryRequest {
                subject: SubjectRequest::from(subject),
                entry: message.entry.clone(),
            };
            self.blocking(move |service| service.entry(request))
                .await
                .map_err(|error| error.to_string())?;
            if let Some(revision) = message.revision.as_ref() {
                let request = PatchEntityRequest {
                    subject: SubjectRequest::from(subject),
                    entity: revision.clone(),
                    cursor: None,
                    limit: Some(1),
                };
                self.blocking(move |service| service.patch_entity(request, PatchEntity::Revision))
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    pub(super) async fn record_relation(
        &self,
        access: &ActivityAccess,
        subject: Subject,
        role: ArtifactRole,
    ) -> Result<(), String> {
        if let ActivityId::Task(task) = access.context().activity {
            let recorded = if role == ArtifactRole::Produced {
                self.queue.produced(task, subject).await
            } else {
                self.queue
                    .update_context(
                        task,
                        ContextUpdate {
                            artifacts: vec![Artifact {
                                subject: subject.to_string(),
                                role,
                                purpose: "Referenced in the originating discussion".to_owned(),
                            }],
                            ..ContextUpdate::default()
                        },
                    )
                    .await
            };
            recorded.map_err(|error| error.to_string())?;
        }
        access.relate(subject).await;
        Ok(())
    }
}
