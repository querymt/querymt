//! Schedule domain types for autonomous scheduled work.
//!
//! A `Schedule` is a first-class entity that references a `Task`. It is separate
//! from the task because scheduling is an operational concern, not a domain concern.
//! A `Task` describes *what* to do. A `Schedule` describes *when* to do it.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Describes when a schedule should fire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleTrigger {
    /// Fire every N seconds.
    Interval { seconds: u64 },
    /// Fire when accumulated events meet a threshold.
    EventDriven {
        event_filter: EventTriggerFilter,
        debounce_seconds: u64,
    },
    /// Fire exactly once at the specified instant, then exhaust.
    OnceAt {
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
}

/// Filter for event-driven schedule triggers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventTriggerFilter {
    /// Which AgentEventKind variants trigger this schedule.
    pub event_kinds: Vec<String>,
    /// Minimum accumulated count before firing.
    pub threshold: u32,
    /// Optional session scope (None = any session).
    pub session_public_id: Option<String>,
}

/// Schedule lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleState {
    /// Waiting for next_run_at or event threshold.
    Armed,
    /// Cycle in progress.
    Running,
    /// User paused.
    Paused,
    /// max_runs reached.
    Exhausted,
    /// Consecutive failures exceeded threshold.
    Failed,
}

impl std::fmt::Display for ScheduleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScheduleState::Armed => write!(f, "armed"),
            ScheduleState::Running => write!(f, "running"),
            ScheduleState::Paused => write!(f, "paused"),
            ScheduleState::Exhausted => write!(f, "exhausted"),
            ScheduleState::Failed => write!(f, "failed"),
        }
    }
}

impl std::str::FromStr for ScheduleState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "armed" => Ok(ScheduleState::Armed),
            "running" => Ok(ScheduleState::Running),
            "paused" => Ok(ScheduleState::Paused),
            "exhausted" => Ok(ScheduleState::Exhausted),
            "failed" => Ok(ScheduleState::Failed),
            _ => Err(format!("Unknown schedule state: {}", s)),
        }
    }
}

/// Execution limits applied to scheduled cycles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleExecutionLimits {
    pub max_steps: Option<u32>,
    pub max_cost_usd: Option<f64>,
}

/// Operational configuration for a schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleConfig {
    /// Maximum number of runs before exhaustion (None = unlimited).
    pub max_runs: Option<u32>,
    /// Maximum consecutive failures before transitioning to Failed.
    #[serde(default = "default_max_consecutive_failures")]
    pub max_consecutive_failures: u32,
    /// Maximum runtime in seconds per cycle.
    #[serde(default = "default_max_runtime_seconds")]
    pub max_runtime_seconds: u64,
    /// Jitter percentage applied to next_run_at (0-100).
    #[serde(default = "default_jitter_percent")]
    pub jitter_percent: u8,
    /// Base backoff duration in seconds for failure recovery.
    #[serde(default = "default_backoff_base_seconds")]
    pub backoff_base_seconds: u64,
    /// Middleware-style execution limits for scheduled cycles.
    pub execution_limits: Option<ScheduleExecutionLimits>,
}

fn default_max_consecutive_failures() -> u32 {
    3
}
fn default_max_runtime_seconds() -> u64 {
    120
}
fn default_jitter_percent() -> u8 {
    10
}
fn default_backoff_base_seconds() -> u64 {
    60
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            max_runs: None,
            max_consecutive_failures: default_max_consecutive_failures(),
            max_runtime_seconds: default_max_runtime_seconds(),
            jitter_percent: default_jitter_percent(),
            backoff_base_seconds: default_backoff_base_seconds(),
            execution_limits: None,
        }
    }
}

/// A schedule entity linking a task to its execution timing.
///
/// ## Identity Contract
/// - `id: i64` is internal only (never leaves repository/API boundary).
/// - `public_id` is the stable external identity used by actor messages, events, and APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    /// Internal DB identity (never leaves repository/API boundary).
    #[serde(skip)]
    pub id: i64,
    /// Stable external identity used by actor messages/events/APIs.
    pub public_id: String,

    /// Public ID of the associated task (domain boundary).
    pub task_public_id: String,
    /// Public ID of the associated session (domain boundary).
    pub session_public_id: String,

    /// Internal task row ID (repository-internal only).
    #[serde(skip)]
    pub task_id: i64,
    /// Internal session row ID (repository-internal only).
    #[serde(skip)]
    pub session_id: i64,

    /// When this schedule should fire.
    pub trigger: ScheduleTrigger,
    /// Current lifecycle state.
    pub state: ScheduleState,

    /// When the last cycle ran (if ever).
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_run_at: Option<OffsetDateTime>,
    /// When the next cycle is scheduled (if armed).
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_run_at: Option<OffsetDateTime>,

    /// Total number of completed runs.
    pub run_count: u32,
    /// Number of consecutive failures (resets on success).
    pub consecutive_failures: u32,

    /// Operational configuration.
    pub config: ScheduleConfig,

    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl Schedule {
    /// Create a new armed schedule with default configuration.
    ///
    /// The `id`, `task_id`, and `session_id` (internal row IDs) are set to 0
    /// and will be populated by the repository on insert.
    pub fn new(
        task_public_id: String,
        session_public_id: String,
        trigger: ScheduleTrigger,
    ) -> Self {
        let now = OffsetDateTime::now_utc();
        let is_once = matches!(&trigger, ScheduleTrigger::OnceAt { .. });
        let next_run_at = match &trigger {
            ScheduleTrigger::Interval { seconds } => {
                Some(now + time::Duration::seconds(*seconds as i64))
            }
            ScheduleTrigger::EventDriven { .. } => None,
            ScheduleTrigger::OnceAt { at } => Some(*at),
        };
        let mut config = ScheduleConfig::default();
        if is_once {
            config.max_runs = Some(1);
        }
        Self {
            id: 0,
            public_id: uuid::Uuid::now_v7().to_string(),
            task_public_id,
            session_public_id,
            task_id: 0,
            session_id: 0,
            trigger,
            state: ScheduleState::Armed,
            last_run_at: None,
            next_run_at,
            run_count: 0,
            consecutive_failures: 0,
            config,
            created_at: now,
            updated_at: now,
        }
    }

    /// Check if a state transition is valid per the schedule state machine.
    pub fn is_valid_transition(from: ScheduleState, to: ScheduleState) -> bool {
        matches!(
            (from, to),
            // Normal firing
            (ScheduleState::Armed, ScheduleState::Running)
            // Cycle completed (below max_runs)
            | (ScheduleState::Running, ScheduleState::Armed)
            // Cycle completed (max_runs reached)
            | (ScheduleState::Running, ScheduleState::Exhausted)
            // Failure threshold reached
            | (ScheduleState::Running, ScheduleState::Failed)
            // User pause from armed
            | (ScheduleState::Armed, ScheduleState::Paused)
            // User pause from running (pause requested)
            | (ScheduleState::Running, ScheduleState::Paused)
            // User resume
            | (ScheduleState::Paused, ScheduleState::Armed)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ScheduleState ────────────────────────────────────────────────────

    #[test]
    fn schedule_state_display_and_parse() {
        let states = [
            (ScheduleState::Armed, "armed"),
            (ScheduleState::Running, "running"),
            (ScheduleState::Paused, "paused"),
            (ScheduleState::Exhausted, "exhausted"),
            (ScheduleState::Failed, "failed"),
        ];
        for (state, expected) in &states {
            assert_eq!(state.to_string(), *expected);
            let parsed: ScheduleState = expected.parse().unwrap();
            assert_eq!(*state, parsed);
        }
    }

    #[test]
    fn schedule_state_parse_unknown_errors() {
        let result: Result<ScheduleState, _> = "unknown".parse();
        assert!(result.is_err());
    }

    #[test]
    fn schedule_state_serde_roundtrip() {
        for state in [
            ScheduleState::Armed,
            ScheduleState::Running,
            ScheduleState::Paused,
            ScheduleState::Exhausted,
            ScheduleState::Failed,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let rt: ScheduleState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, rt);
        }
    }

    // ── ScheduleTrigger ──────────────────────────────────────────────────

    #[test]
    fn schedule_trigger_interval_serde_roundtrip() {
        let trigger = ScheduleTrigger::Interval { seconds: 1800 };
        let json = serde_json::to_string(&trigger).unwrap();
        let rt: ScheduleTrigger = serde_json::from_str(&json).unwrap();
        if let ScheduleTrigger::Interval { seconds } = rt {
            assert_eq!(seconds, 1800);
        } else {
            panic!("Expected Interval variant");
        }
    }

    #[test]
    fn schedule_trigger_once_at_serde_roundtrip() {
        let at = OffsetDateTime::now_utc() + time::Duration::hours(2);
        let trigger = ScheduleTrigger::OnceAt { at };
        let json = serde_json::to_string(&trigger).unwrap();
        assert!(json.contains("\"once_at\""), "tag should be once_at");
        let rt: ScheduleTrigger = serde_json::from_str(&json).unwrap();
        if let ScheduleTrigger::OnceAt { at: rt_at } = rt {
            // Compare to second precision (rfc3339 may truncate sub-second)
            assert_eq!(rt_at.unix_timestamp(), at.unix_timestamp());
        } else {
            panic!("Expected OnceAt variant");
        }
    }

    #[test]
    fn schedule_new_once_at_forces_max_runs_one() {
        let at = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let schedule = Schedule::new(
            "task-1".to_string(),
            "sess-1".to_string(),
            ScheduleTrigger::OnceAt { at },
        );
        assert_eq!(schedule.config.max_runs, Some(1));
        assert_eq!(
            schedule.next_run_at.map(|t| t.unix_timestamp()),
            Some(at.unix_timestamp())
        );
        assert_eq!(schedule.state, ScheduleState::Armed);
    }

    #[test]
    fn schedule_trigger_event_driven_serde_roundtrip() {
        let trigger = ScheduleTrigger::EventDriven {
            event_filter: EventTriggerFilter {
                event_kinds: vec!["knowledge_ingested".to_string()],
                threshold: 5,
                session_public_id: None,
            },
            debounce_seconds: 60,
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let rt: ScheduleTrigger = serde_json::from_str(&json).unwrap();
        if let ScheduleTrigger::EventDriven {
            event_filter,
            debounce_seconds,
        } = rt
        {
            assert_eq!(event_filter.threshold, 5);
            assert_eq!(debounce_seconds, 60);
            assert_eq!(event_filter.event_kinds, vec!["knowledge_ingested"]);
        } else {
            panic!("Expected EventDriven variant");
        }
    }

    // ── ScheduleConfig ───────────────────────────────────────────────────

    #[test]
    fn schedule_config_defaults() {
        let config = ScheduleConfig::default();
        assert_eq!(config.max_runs, None);
        assert_eq!(config.max_consecutive_failures, 3);
        assert_eq!(config.max_runtime_seconds, 120);
        assert_eq!(config.jitter_percent, 10);
        assert_eq!(config.backoff_base_seconds, 60);
        assert!(config.execution_limits.is_none());
    }

    #[test]
    fn schedule_config_serde_roundtrip() {
        let config = ScheduleConfig {
            max_runs: Some(10),
            max_consecutive_failures: 5,
            max_runtime_seconds: 300,
            jitter_percent: 20,
            backoff_base_seconds: 120,
            execution_limits: Some(ScheduleExecutionLimits {
                max_steps: Some(50),
                max_cost_usd: Some(1.5),
            }),
        };
        let json = serde_json::to_string(&config).unwrap();
        let rt: ScheduleConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.max_runs, Some(10));
        assert_eq!(rt.max_consecutive_failures, 5);
        assert_eq!(rt.max_runtime_seconds, 300);
        assert!(rt.execution_limits.is_some());
    }

    // ── State machine transitions ────────────────────────────────────────

    #[test]
    fn valid_transitions() {
        assert!(Schedule::is_valid_transition(
            ScheduleState::Armed,
            ScheduleState::Running
        ));
        assert!(Schedule::is_valid_transition(
            ScheduleState::Running,
            ScheduleState::Armed
        ));
        assert!(Schedule::is_valid_transition(
            ScheduleState::Running,
            ScheduleState::Exhausted
        ));
        assert!(Schedule::is_valid_transition(
            ScheduleState::Running,
            ScheduleState::Failed
        ));
        assert!(Schedule::is_valid_transition(
            ScheduleState::Armed,
            ScheduleState::Paused
        ));
        assert!(Schedule::is_valid_transition(
            ScheduleState::Running,
            ScheduleState::Paused
        ));
        assert!(Schedule::is_valid_transition(
            ScheduleState::Paused,
            ScheduleState::Armed
        ));
    }

    #[test]
    fn invalid_transitions() {
        // Cannot go from Failed to Running directly
        assert!(!Schedule::is_valid_transition(
            ScheduleState::Failed,
            ScheduleState::Running
        ));
        // Cannot go from Exhausted to Running
        assert!(!Schedule::is_valid_transition(
            ScheduleState::Exhausted,
            ScheduleState::Running
        ));
        // Cannot go from Paused to Running (must go through Armed)
        assert!(!Schedule::is_valid_transition(
            ScheduleState::Paused,
            ScheduleState::Running
        ));
        // Cannot go from Armed to Failed
        assert!(!Schedule::is_valid_transition(
            ScheduleState::Armed,
            ScheduleState::Failed
        ));
    }

    // ── Schedule construction ────────────────────────────────────────────

    #[test]
    fn schedule_public_id_serialized_internal_id_skipped() {
        let schedule = Schedule {
            id: 42,
            public_id: "sched-abc".to_string(),
            task_public_id: "task-xyz".to_string(),
            session_public_id: "sess-123".to_string(),
            task_id: 10,
            session_id: 20,
            trigger: ScheduleTrigger::Interval { seconds: 3600 },
            state: ScheduleState::Armed,
            last_run_at: None,
            next_run_at: None,
            run_count: 0,
            consecutive_failures: 0,
            config: ScheduleConfig::default(),
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };

        let json = serde_json::to_string(&schedule).unwrap();
        // Internal IDs should NOT appear in serialized output
        assert!(!json.contains("\"id\""));
        assert!(!json.contains("\"task_id\""));
        assert!(!json.contains("\"session_id\""));
        // Public IDs should appear
        assert!(json.contains("sched-abc"));
        assert!(json.contains("task-xyz"));
        assert!(json.contains("sess-123"));
    }
}
