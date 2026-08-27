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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressReview {
    pub reason: String,
    pub step: usize,
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
    pub run_id: String,
    pub revision: ObjectiveRevision,
    pub run_instruction: ObjectiveDirective,
    pub amendments: Vec<ObjectiveAmendment>,
    pub current_task: Option<TaskCommitment>,
    pub execution_origin: String,
    /// Recovery metadata only. It is deliberately excluded from rendering.
    pub historical_intent: Option<IntentSnapshot>,
    pub progress_review: Option<ProgressReview>,
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
        run_id: String,
        instruction: String,
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
            progress_review: None,
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
        self.current_task = task.map(TaskCommitment::from);
        self.completion_disposition = Self::disposition_for_task(task);
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

    pub fn set_progress_review(&mut self, review: Option<ProgressReview>) {
        self.progress_review = review;
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
        if self.progress_review.is_some() {
            sections.push(
                "Progress review (does not alter scope or precedence):\nReview progress against the latest instruction and applicable acceptance criteria. If incomplete, take the next coherent action. If blocked, report the blocker precisely. Completed investigation is not completion of requested implementation. Do not narrow scope or limit work to one fact or one tool call."
                    .to_string(),
            );
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
            "run-1".into(),
            "Inspect only".into(),
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
    fn progress_review_does_not_narrow_scope() {
        let mut objective = RunObjective::new(
            "run-1".into(),
            "Implement the fix".into(),
            "interactive".into(),
            None,
            None,
        );
        objective.set_progress_review(Some(ProgressReview {
            reason: "periodic_progress_review".into(),
            step: 7,
        }));
        let rendered = objective.render();
        assert!(rendered.contains("does not alter scope or precedence"));
        assert!(!rendered.contains("exactly one"));
    }
}
