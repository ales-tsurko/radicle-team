//! Publication reuse for a task and ordinary issue discovery.

use std::str::FromStr as _;

use radicle::{
    cob::{
        ObjectId,
        issue::{Issues, State},
        store::access::ReadOnly,
    },
    storage::ReadStorage as _,
};

use crate::{
    agent::{ActivityId, Source},
    mcp::{server::ActivityAccess, tools::ToolService},
    watcher::Subject,
};

impl ToolService {
    pub(super) async fn existing_patch(
        &self,
        access: &ActivityAccess,
    ) -> Result<Option<ObjectId>, String> {
        if let ActivityId::Task(task) = access.context().activity {
            let task = self
                .queue
                .get(task)
                .await
                .map_err(|error| error.to_string())?;
            if task.context.cancelled {
                return Err("this task was cancelled; preserve local work and stop".to_owned());
            }
            if let Source::Subject(Subject::Patch(patch)) = task.target() {
                self.verify_subject(
                    access,
                    &crate::mcp::protocol::SubjectRequest::from(Subject::Patch(patch)),
                )
                .await?;
                return Ok(Some(patch));
            }
        }
        self.publisher
            .created(access.context().activity, "create_patch")
            .await
            .map_err(|error| error.to_string())?
            .first()
            .map(|id| ObjectId::from_str(id).map_err(|error| error.to_string()))
            .transpose()
    }

    pub(super) async fn existing_issue(&self, title: &str) -> Result<Option<String>, String> {
        let title = title.trim().to_lowercase();
        self.blocking(move |service| {
            let repository = service.storage.repository(service.repository)?;
            let issues = Issues::open(&repository, ReadOnly)?;
            for issue in issues.all()? {
                let (id, issue) = issue?;
                if matches!(issue.state(), State::Open)
                    && issue.author().id().as_key() == &service.publisher.author()
                    && issue.title().trim().to_lowercase() == title
                {
                    return Ok(Some(id.to_string()));
                }
            }
            Ok(None)
        })
        .await
        .map_err(|error| error.to_string())
    }
}
