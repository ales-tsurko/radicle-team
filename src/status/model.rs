//! The last observed state of one runtime and its concurrent roles.

use std::{
    collections::BTreeMap,
    fmt,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::agent::Role;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub(crate) session: String,
    pub(crate) revision: i64,
    pub(crate) phase: Phase,
    pub(crate) started: u64,
    pub(crate) updated: u64,
    pub(crate) roles: BTreeMap<Role, RoleStatus>,
    pub(crate) failure: Option<String>,
}

impl Snapshot {
    pub(crate) fn new(session: String) -> Self {
        let now = Self::now();
        Self {
            session,
            revision: 0,
            phase: Phase::Starting,
            started: now,
            updated: now,
            roles: Role::ALL
                .into_iter()
                .map(|role| (role, RoleStatus::default()))
                .collect(),
            failure: None,
        }
    }

    pub(crate) fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub(crate) fn observed(at: Instant) -> u64 {
        Self::now().saturating_sub(at.elapsed().as_secs())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Starting,
    Synchronizing,
    AwaitingApproval,
    Initializing,
    Running,
    Stopping,
    Stopped,
    Failed,
    Interrupted,
}

impl Phase {
    pub(crate) const fn active(self) -> bool {
        !matches!(self, Self::Stopped | Self::Failed | Self::Interrupted)
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Starting => "starting",
            Self::Synchronizing => "synchronizing",
            Self::AwaitingApproval => "waiting for manifest approval",
            Self::Initializing => "initializing agents",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct RoleStatus {
    pub(crate) enabled: Option<bool>,
    pub(crate) activity: Option<Activity>,
    pub(crate) started: Option<u64>,
    pub(crate) progress: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Activity {
    pub(crate) task: Option<i64>,
    pub(crate) source: String,
}
