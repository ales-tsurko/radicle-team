//! A transfer is complete only when the receiving member has acknowledged queued implementation.

use std::str::FromStr as _;

use radicle::{cob::EntryId, identity::Did, storage::ReadStorage as _};
use rmcp::Json;

use crate::{
    delivery::{State, remote::Deliveries},
    mcp::{
        protocol::{HandoffRequest, TaskAction, TaskSummary},
        server::ServiceError,
        tools::ToolService,
    },
    member::Member,
    queue::TaskKind,
    watcher::{Delivery, Kind},
};

impl ToolService {
    pub(super) async fn handoff(
        &self,
        request: HandoffRequest,
    ) -> Result<Json<TaskAction>, String> {
        let active = self.active().await?;
        let task = self.task_access(&active, request.task).await?;
        let member = Member::from_str(&request.member).map_err(|error| error.to_string())?;
        if member == self.queue.member() || task.kind() != TaskKind::Implementation {
            return Err("handoff requires another member and an implementation task".to_owned());
        }
        let operation = EntryId::from_str(&request.operation).map_err(|error| error.to_string())?;
        let source = task.source.clone();
        let target = task.target();
        self.blocking(move |service| {
            let repository = service.storage.repository(service.repository)?;
            let accepted = Deliveries::read(&repository)
                .map_err(|error| ServiceError::Request(error.to_string()))?
                .into_iter()
                .filter(|delivery| delivery.member == member)
                .flat_map(|delivery| delivery.records.into_values())
                .any(|record| {
                    record.activity.operation == operation
                        && record.activity.author == Did::from(service.publisher.author())
                        && matches!(
                            record.activity.kind,
                            Kind::Delivered(Delivery::Mentioned(_))
                        )
                        && (crate::agent::Source::from(record.activity.subject) == source
                            || crate::agent::Source::from(record.activity.subject) == target)
                        && record.task.is_some()
                        && record.task_kind == Some(TaskKind::Implementation)
                        && matches!(
                            record.state,
                            State::Queued | State::Processing | State::Handled
                        )
                });
            if accepted {
                Ok(())
            } else {
                Err(ServiceError::Request(format!(
                    "{member} has not acknowledged queued implementation of handoff {operation}; \
                     keep your task until its signed receipt arrives. A reaction or review \
                     assignment does not confirm this transfer"
                )))
            }
        })
        .await
        .map_err(|error| error.to_string())?;
        let task = self
            .queue
            .handoff(request.task, request.remaining)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Json(TaskAction {
            task: TaskSummary::from(task),
        }))
    }
}
