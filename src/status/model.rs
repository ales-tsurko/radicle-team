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
    pub(crate) fetched: Option<u64>,
    pub(crate) sync_error: Option<String>,
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
            fetched: None,
            sync_error: None,
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
    #[serde(default)]
    pub(crate) observation: Option<Observation>,
}

impl RoleStatus {
    pub(crate) fn state(&self) -> &'static str {
        if self
            .observation
            .as_ref()
            .is_some_and(|observation| observation.state == AgentState::Retrying)
        {
            return "retrying";
        }
        if self
            .progress
            .is_some_and(|at| Snapshot::now().saturating_sub(at) >= 120)
        {
            return "no recent output";
        }
        let Some(observation) = &self.observation else {
            return "active";
        };
        match observation.state {
            AgentState::AwaitingModel => "waiting for model",
            AgentState::Thinking => "model output",
            AgentState::UsingTool => "tool running",
            AgentState::ToolFailed => "tool failed",
            AgentState::Retrying => "retrying",
        }
    }
}

/// Observed agent activity, without claiming its output is useful or correct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Observation {
    pub(crate) state: AgentState,
    pub(crate) at: u64,
    pub(crate) tool: Option<String>,
    pub(crate) completed: Option<u64>,
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentState {
    AwaitingModel,
    Thinking,
    UsingTool,
    ToolFailed,
    Retrying,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Activity {
    pub(crate) task: Option<i64>,
    pub(crate) source: String,
}

#[cfg(test)]
mod tests {
    use super::{AgentState, Observation, RoleStatus, Snapshot};

    #[test]
    fn distinguishes_observed_activity_from_quiet_and_retrying_runs() {
        let now = Snapshot::now();
        let mut status = RoleStatus {
            progress: Some(now - 121),
            ..RoleStatus::default()
        };
        assert_eq!(status.state(), "no recent output");
        status.observation = Some(Observation {
            state: AgentState::Retrying,
            at: now,
            tool: None,
            completed: None,
            detail: Some("provider stream failed".into()),
        });
        assert_eq!(status.state(), "retrying");
        status.progress = Some(now);
        let observation = status.observation.as_mut().expect("observation");
        observation.state = AgentState::AwaitingModel;
        assert_eq!(status.state(), "waiting for model");
        status.observation.as_mut().expect("observation").state = AgentState::UsingTool;
        assert_eq!(status.state(), "tool running");
    }
}
