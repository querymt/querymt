//! Runtime operating mode of an agent session.

use serde::{Deserialize, Serialize};

/// Runtime operating mode for the agent.
/// Modes control what the agent is allowed to do and what system reminders are injected.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum AgentMode {
    /// Full read/write mode - all tools available, no restrictions
    #[default]
    Build = 0,
    /// Planning mode - read-only, agent observes and plans without making changes
    Plan = 1,
    /// Review mode - read-only, agent reviews code and provides feedback
    Review = 2,
}

impl AgentMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => AgentMode::Plan,
            2 => AgentMode::Review,
            _ => AgentMode::Build,
        }
    }

    /// Cycle to next mode: Build -> Plan -> Review -> Build
    pub fn next(self) -> Self {
        match self {
            AgentMode::Build => AgentMode::Plan,
            AgentMode::Plan => AgentMode::Review,
            AgentMode::Review => AgentMode::Build,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AgentMode::Build => "build",
            AgentMode::Plan => "plan",
            AgentMode::Review => "review",
        }
    }

    /// Whether this mode is read-only (agent should not make file changes)
    pub fn is_read_only(&self) -> bool {
        matches!(self, AgentMode::Plan | AgentMode::Review)
    }
}

impl std::fmt::Display for AgentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for AgentMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "build" => Ok(AgentMode::Build),
            "plan" => Ok(AgentMode::Plan),
            "review" => Ok(AgentMode::Review),
            _ => Err(format!(
                "unknown agent mode: '{}'. Valid modes: build, plan, review",
                s
            )),
        }
    }
}
