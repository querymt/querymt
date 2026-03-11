//! Actor message types for `SchedulerActor`.
//!
//! Each message struct corresponds to a single actor interaction.
//! All public-facing identifiers use `schedule_public_id` (never row IDs).

/// Timer fired for an interval schedule.
pub(crate) struct DeadlineReached {
    pub schedule_public_id: String,
}

/// Matching event received from EventFanout.
pub(crate) struct EventReceived {
    pub schedule_public_id: String,
    pub event_kind: String,
}

/// Periodic safety pass — re-reads due armed schedules and stale running
/// schedules from storage, repairs in-memory queues.
pub(crate) struct Reconcile;

/// User/API: fire a schedule immediately regardless of deadline/threshold.
pub struct TriggerNow {
    pub schedule_public_id: String,
}

/// User/API: register a new schedule.
pub struct AddSchedule {
    pub schedule: crate::session::domain_schedule::Schedule,
}

/// User/API: remove a schedule.
pub struct RemoveSchedule {
    pub schedule_public_id: String,
}

/// User/API: pause a schedule.
pub struct PauseSchedule {
    pub schedule_public_id: String,
}

/// User/API: resume a paused schedule.
pub struct ResumeSchedule {
    pub schedule_public_id: String,
}

/// Received when a scheduled execution cycle completes successfully.
///
/// Sent by SchedulerActor's EventFanout subscriber when it observes a
/// `ScheduledExecutionCompleted` event.
pub(crate) struct CycleCompleted {
    pub schedule_public_id: String,
    pub turn_id: String,
}

/// Received when a scheduled execution cycle fails.
pub(crate) struct CycleFailed {
    pub schedule_public_id: String,
    pub turn_id: Option<String>,
    pub error: String,
}

/// Query: list schedules, optionally filtered by session.
pub struct ListSchedules {
    pub session_public_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::domain_schedule::*;
    use time::OffsetDateTime;

    #[test]
    fn deadline_reached_construction() {
        let msg = DeadlineReached {
            schedule_public_id: "sched-1".to_string(),
        };
        assert_eq!(msg.schedule_public_id, "sched-1");
    }

    #[test]
    fn event_received_construction() {
        let msg = EventReceived {
            schedule_public_id: "sched-2".to_string(),
            event_kind: "knowledge_ingested".to_string(),
        };
        assert_eq!(msg.schedule_public_id, "sched-2");
        assert_eq!(msg.event_kind, "knowledge_ingested");
    }

    #[test]
    fn trigger_now_construction() {
        let msg = TriggerNow {
            schedule_public_id: "sched-3".to_string(),
        };
        assert_eq!(msg.schedule_public_id, "sched-3");
    }

    #[test]
    fn add_schedule_construction() {
        let schedule = Schedule {
            id: 0,
            public_id: "sched-add".to_string(),
            task_public_id: "task-1".to_string(),
            session_public_id: "sess-1".to_string(),
            task_id: 0,
            session_id: 0,
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
        let msg = AddSchedule { schedule };
        assert_eq!(msg.schedule.public_id, "sched-add");
    }

    #[test]
    fn remove_schedule_construction() {
        let msg = RemoveSchedule {
            schedule_public_id: "sched-del".to_string(),
        };
        assert_eq!(msg.schedule_public_id, "sched-del");
    }

    #[test]
    fn pause_resume_construction() {
        let pause = PauseSchedule {
            schedule_public_id: "sched-p".to_string(),
        };
        let resume = ResumeSchedule {
            schedule_public_id: "sched-p".to_string(),
        };
        assert_eq!(pause.schedule_public_id, resume.schedule_public_id);
    }

    #[test]
    fn cycle_completed_construction() {
        let msg = CycleCompleted {
            schedule_public_id: "sched-cc".to_string(),
            turn_id: "turn-1".to_string(),
        };
        assert_eq!(msg.schedule_public_id, "sched-cc");
        assert_eq!(msg.turn_id, "turn-1");
    }

    #[test]
    fn cycle_failed_construction() {
        let msg = CycleFailed {
            schedule_public_id: "sched-cf".to_string(),
            turn_id: Some("turn-2".to_string()),
            error: "timeout".to_string(),
        };
        assert_eq!(msg.schedule_public_id, "sched-cf");
        assert_eq!(msg.error, "timeout");
    }

    #[test]
    fn list_schedules_construction() {
        let msg = ListSchedules {
            session_public_id: Some("sess-1".to_string()),
        };
        assert_eq!(msg.session_public_id.as_deref(), Some("sess-1"));

        let all = ListSchedules {
            session_public_id: None,
        };
        assert!(all.session_public_id.is_none());
    }
}
