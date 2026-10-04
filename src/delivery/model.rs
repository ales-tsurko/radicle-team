//! Delivery identity and lifecycle, independent of model execution.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    status::Snapshot,
    watcher::{Activity, Change, Delivery},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) activity: Activity,
    pub(crate) state: State,
    pub(crate) task: Option<i64>,
    pub(crate) received: u64,
    pub(crate) updated: u64,
    pub(crate) revision: i64,
    pub(crate) error: Option<String>,
}

impl Record {
    pub(crate) fn new(activity: Activity, task: Option<i64>) -> Self {
        let now = Snapshot::now();
        Self {
            activity,
            state: State::Queued,
            task,
            received: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            updated: now,
            revision: 1,
            error: None,
        }
    }

    pub(crate) fn id(&self) -> String {
        format!(
            "{}:{}:{}",
            self.activity.repository,
            self.activity.subject,
            self.activity.delivery_id()
        )
    }

    pub(crate) fn priority(&self) -> i64 {
        match self.activity.kind {
            crate::watcher::Kind::Delivered(Delivery::Changed(
                Change::ReviewSubmitted { .. }
                | Change::ReviewEdited(_)
                | Change::ReviewFollowUp { .. },
            ))
            | crate::watcher::Kind::Delivered(Delivery::Mentioned(_) | Delivery::Replied { .. }) => {
                0
            }
            _ => 1,
        }
    }

    pub(crate) fn matches(&self, id: &str) -> bool {
        self.activity.operation.to_string() == id
            || self.id() == id
            || self.activity.subject.id().to_string() == id
            || serde_json::to_value(self.activity.kind).is_ok_and(|value| {
                // IDs inside typed review/revision/comment targets are searchable too.
                value.to_string().contains(&format!("\"{id}\""))
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Queued,
    Processing,
    Handled,
    Ignored,
    Failed,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Handled => "handled",
            Self::Ignored => "ignored",
            Self::Failed => "failed",
        })
    }
}
