//! Run-based turn boundaries, shared by remote chunk transfer and windowed
//! session loads.
//!
//! A turn is one agent run. It starts at the run's `RunStarted` event and
//! continues, including any queued-input, prompt-block and steering
//! `PromptReceived` events, until the next `RunStarted`. History written
//! before run events existed (no `RunStarted` seen yet) falls back to prompt
//! events: a turn starts at a `UserPromptBlock` / `PromptReceived` that does
//! not directly follow another prompt event.

use crate::events::AgentEventKind;

/// Indices (into the iterated sequence) at which a turn starts.
///
/// Events before the first start belong to an implicit leading turn; callers
/// treat position 0 as a boundary for that reason.
pub fn turn_start_indices<'a, I>(kinds: I) -> Vec<usize>
where
    I: IntoIterator<Item = &'a AgentEventKind>,
{
    let mut starts = Vec::new();
    let mut in_runs = false;
    let mut previous_was_prompt = false;
    for (index, kind) in kinds.into_iter().enumerate() {
        match kind {
            AgentEventKind::RunStarted { .. } => {
                starts.push(index);
                in_runs = true;
                previous_was_prompt = false;
            }
            AgentEventKind::UserPromptBlock { .. } | AgentEventKind::PromptReceived { .. }
                if !in_runs =>
            {
                if !previous_was_prompt {
                    starts.push(index);
                }
                previous_was_prompt = true;
            }
            // Stored after the prompt it belongs to; never a boundary itself.
            AgentEventKind::UserMessageStored { .. } => {}
            _ => previous_was_prompt = false,
        }
    }
    starts
}

/// Index of the first turn start in the sequence, if any.
pub fn first_turn_start<'a, I>(kinds: I) -> Option<usize>
where
    I: IntoIterator<Item = &'a AgentEventKind>,
{
    turn_start_indices(kinds).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str) -> AgentEventKind {
        AgentEventKind::RunStarted {
            run_id: id.into(),
            origin: "interactive".into(),
        }
    }
    fn prompt() -> AgentEventKind {
        AgentEventKind::PromptReceived {
            content: "p".into(),
            message_id: None,
        }
    }
    fn block() -> AgentEventKind {
        AgentEventKind::UserPromptBlock {
            message_id: "m".into(),
            client_prompt_id: None,
            block: serde_json::from_value(serde_json::json!({"type":"text","text":"x"})).unwrap(),
        }
    }
    fn other() -> AgentEventKind {
        AgentEventKind::Cancelled
    }

    #[test]
    fn steering_inside_a_run_is_one_turn() {
        let kinds = [run("r1"), block(), prompt(), other(), prompt(), other()];
        assert_eq!(turn_start_indices(&kinds), vec![0]);
    }

    #[test]
    fn each_run_is_a_turn_and_queued_input_stays_with_its_run() {
        let kinds = [
            run("r1"),
            prompt(),
            AgentEventKind::QueuedInputStarted {
                input_id: "i".into(),
                run_id: "r1".into(),
            },
            run("r2"),
            prompt(),
        ];
        assert_eq!(turn_start_indices(&kinds), vec![0, 3]);
    }

    #[test]
    fn prompt_blocks_before_prompt_received_do_not_split_a_run() {
        let kinds = [other(), run("r1"), block(), block(), prompt(), other()];
        assert_eq!(turn_start_indices(&kinds), vec![1]);
        assert_eq!(first_turn_start(&kinds), Some(1));
    }

    #[test]
    fn legacy_history_without_runs_starts_at_prompts() {
        let kinds = [
            block(),
            block(),
            prompt(),
            other(),
            block(),
            prompt(),
            other(),
        ];
        assert_eq!(turn_start_indices(&kinds), vec![0, 4]);
    }

    #[test]
    fn no_boundary_when_sequence_has_no_prompt_or_run() {
        assert_eq!(turn_start_indices(&[other(), other()]), Vec::<usize>::new());
        assert_eq!(first_turn_start(&[other()]), None);
    }
}
