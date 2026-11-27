use crate::session::domain::{IntentSnapshot, Task, TaskStatus};
use serde::{Deserialize, Serialize};

pub type ObjectiveRevision = u64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveSource {
    RunPrompt,
    Steering,
    ClarificationAnswer,
    TaskCreated,
    TaskUpdated,
    TaskCompleted,
    TaskPaused,
    TaskCancelled,
}

impl ObjectiveSource {
    pub fn is_user_originated(&self) -> bool {
        matches!(
            self,
            Self::RunPrompt | Self::Steering | Self::ClarificationAnswer
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectiveDirective {
    pub text: String,
    pub source: ObjectiveSource,
    pub source_ref: Option<String>,
    pub accepted_at_ms: Option<u64>,
    pub application_boundary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectiveAmendment {
    pub revision: ObjectiveRevision,
    pub directive: ObjectiveDirective,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCommitment {
    pub public_id: String,
    pub revision: u64,
    pub status: TaskStatus,
    pub expected_deliverable: Option<String>,
    pub acceptance_criteria: Option<String>,
}

impl From<&Task> for TaskCommitment {
    fn from(task: &Task) -> Self {
        Self {
            public_id: task.public_id.clone(),
            revision: task.revision,
            status: task.status,
            expected_deliverable: task.expected_deliverable.clone(),
            acceptance_criteria: task.acceptance_criteria.clone(),
        }
    }
}

/// Stored as a `progress_review` notice at step 7 and every fourth step after.
/// Includes the completion sentence, so it does not depend on the run objective
/// being sent.
pub const PROGRESS_REVIEW_NOTICE: &str = "Progress review (does not alter scope or precedence):\nReview progress against the latest instruction and applicable acceptance criteria. If incomplete, take the next coherent action. If blocked, report the blocker precisely. Completed investigation is not completion of requested implementation. Do not narrow scope or limit work to one fact or one tool call.\n\nCompletion means satisfying the latest instruction and applicable acceptance criteria, or reporting a real blocker.";

/// Whether a progress review is due before the request at `steps` completed
/// model steps: at step 7, then every fourth step after the last review.
pub fn progress_review_due(steps: usize, last_review_step: usize) -> bool {
    (steps >= 7 && last_review_step == 0)
        || (last_review_step > 0 && steps.saturating_sub(last_review_step) >= 4)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionDisposition {
    Untracked,
    Active,
    Completed,
    Paused,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunObjective {
    pub run_id: querymt_agent_core::model::TurnId,
    pub revision: ObjectiveRevision,
    pub run_instruction: ObjectiveDirective,
    pub amendments: Vec<ObjectiveAmendment>,
    pub current_task: Option<TaskCommitment>,
    pub execution_origin: String,
    /// Recovery metadata only. It is deliberately excluded from rendering.
    pub historical_intent: Option<IntentSnapshot>,
    pub completion_disposition: CompletionDisposition,
}

impl RunObjective {
    fn disposition_for_task(task: Option<&Task>) -> CompletionDisposition {
        match task.map(|task| task.status) {
            Some(TaskStatus::Active) => CompletionDisposition::Active,
            Some(TaskStatus::Paused) => CompletionDisposition::Paused,
            Some(TaskStatus::Done) => CompletionDisposition::Completed,
            Some(TaskStatus::Cancelled) => CompletionDisposition::Cancelled,
            None => CompletionDisposition::Untracked,
        }
    }

    pub fn new(
        run_id: querymt_agent_core::model::TurnId,
        instruction: &str,
        execution_origin: String,
        current_task: Option<&Task>,
        historical_intent: Option<IntentSnapshot>,
    ) -> Self {
        let completion_disposition = Self::disposition_for_task(current_task);
        let current_task = current_task.map(TaskCommitment::from);
        Self {
            run_id,
            revision: 1,
            run_instruction: ObjectiveDirective {
                text: instruction.trim().to_string(),
                source: ObjectiveSource::RunPrompt,
                source_ref: None,
                accepted_at_ms: None,
                application_boundary: Some("run_admission".to_string()),
            },
            amendments: Vec::new(),
            current_task,
            execution_origin,
            historical_intent,
            completion_disposition,
        }
    }

    pub fn amend(&mut self, directive: ObjectiveDirective) -> ObjectiveRevision {
        self.revision = self.revision.saturating_add(1);
        self.amendments.push(ObjectiveAmendment {
            revision: self.revision,
            directive,
        });
        self.revision
    }

    pub fn set_task(&mut self, task: Option<&Task>, source: ObjectiveSource) {
        let next_task = task.map(TaskCommitment::from);
        let next_disposition = Self::disposition_for_task(task);
        if self.current_task == next_task && self.completion_disposition == next_disposition {
            return;
        }
        self.current_task = next_task;
        self.completion_disposition = next_disposition;
        self.revision = self.revision.saturating_add(1);
        self.amendments.push(ObjectiveAmendment {
            revision: self.revision,
            directive: ObjectiveDirective {
                text: "Task commitment state changed.".to_string(),
                source,
                source_ref: task.map(|task| task.public_id.clone()),
                accepted_at_ms: None,
                application_boundary: Some("tool_result".to_string()),
            },
        });
    }

    pub fn render(&self) -> String {
        let mut sections = vec![format!(
            "[Run objective revision {}]\nLatest authoritative instruction:\n{}",
            self.revision, self.run_instruction.text
        )];
        let user_updates: Vec<_> = self
            .amendments
            .iter()
            .filter(|amendment| amendment.directive.source.is_user_originated())
            .map(|amendment| format!("- {}", amendment.directive.text))
            .collect();
        if !user_updates.is_empty() {
            sections.push(format!(
                "Later user updates (chronological; later updates override earlier instructions on conflict):\n{}",
                user_updates.join("\n")
            ));
        }
        if let Some(task) = &self.current_task {
            let mut lines = vec![format!(
                "Current task commitment: {} (revision {}, status {:?})",
                task.public_id, task.revision, task.status
            )];
            if let Some(deliverable) = &task.expected_deliverable {
                lines.push(format!("Deliverable: {deliverable}"));
            }
            if let Some(criteria) = &task.acceptance_criteria {
                lines.push(format!("Acceptance criteria: {criteria}"));
            }
            lines.push(
                "This durable commitment supplies context and criteria; it does not restrict newer user instructions."
                    .to_string(),
            );
            sections.push(lines.join("\n"));
        }
        sections.push(
            "Completion means satisfying the latest instruction and applicable acceptance criteria, or reporting a real blocker."
                .to_string(),
        );
        sections.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_user_updates_render_in_order_without_historical_intent() {
        let mut objective = RunObjective::new(
            querymt_agent_core::model::TurnId::new(),
            "Inspect only",
            "interactive".into(),
            None,
            None,
        );
        for text in ["Implement the fix", "Also add integration tests"] {
            objective.amend(ObjectiveDirective {
                text: text.into(),
                source: ObjectiveSource::Steering,
                source_ref: None,
                accepted_at_ms: None,
                application_boundary: None,
            });
        }
        let rendered = objective.render();
        assert!(
            rendered.find("Implement the fix").unwrap()
                < rendered.find("Also add integration tests").unwrap()
        );
        assert!(!rendered.contains("Original requested outcome"));
        assert!(!rendered.contains("Current intent"));
    }

    #[test]
    fn progress_review_notice_does_not_narrow_scope() {
        assert!(PROGRESS_REVIEW_NOTICE.contains("does not alter scope or precedence"));
        assert!(PROGRESS_REVIEW_NOTICE.contains("Completion means satisfying"));
        assert!(!PROGRESS_REVIEW_NOTICE.contains("exactly one"));
    }

    #[test]
    fn rendered_objective_has_no_progress_review_text() {
        let objective = RunObjective::new(
            querymt_agent_core::model::TurnId::new(),
            "Implement the fix",
            "interactive".into(),
            None,
            None,
        );
        let rendered = objective.render();
        assert!(!rendered.contains("Progress review"));
        assert!(rendered.contains("Completion means satisfying"));
    }

    #[test]
    fn progress_review_is_due_at_step_seven_then_every_fourth_step() {
        assert!(!progress_review_due(6, 0));
        assert!(progress_review_due(7, 0));
        assert!(!progress_review_due(10, 7));
        assert!(progress_review_due(11, 7));
        assert!(!progress_review_due(12, 11));
    }

    #[test]
    fn unchanged_task_does_not_create_an_objective_revision() {
        let now = time::OffsetDateTime::now_utc();
        let task = Task {
            id: 1,
            public_id: "task-1".into(),
            session_id: 1,
            kind: crate::session::domain::TaskKind::Finite,
            status: TaskStatus::Active,
            expected_deliverable: Some("Implement the fix".into()),
            acceptance_criteria: Some("Tests pass".into()),
            revision: 1,
            creation_key: Some("call-1".into()),
            completion_evidence: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
        };
        let mut objective = RunObjective::new(
            querymt_agent_core::model::TurnId::new(),
            "Implement the fix",
            "interactive".into(),
            Some(&task),
            None,
        );

        objective.set_task(Some(&task), ObjectiveSource::TaskUpdated);

        assert_eq!(objective.revision, 1);
        assert!(objective.amendments.is_empty());
    }
}
