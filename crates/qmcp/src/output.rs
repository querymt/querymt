use std::collections::VecDeque;

use qmacp::error::CliError;
use serde_json::{Map, Value, json};

const MAX_EVENTS: usize = 256;
const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Bounded event history included in each tool result, never printed to stdout.
#[derive(Default)]
pub(crate) struct EventLog {
    events: VecDeque<(Value, usize)>,
    bytes: usize,
    dropped: usize,
}

impl EventLog {
    pub fn push(&mut self, event: Value) {
        let size = event.to_string().len();
        if size > MAX_EVENT_BYTES {
            self.dropped += 1;
            return;
        }
        while self.events.len() >= MAX_EVENTS || self.bytes + size > MAX_EVENT_BYTES {
            if let Some((_, size)) = self.events.pop_front() {
                self.bytes -= size;
                self.dropped += 1;
            }
        }
        self.bytes += size;
        self.events.push_back((event, size));
    }

    pub fn attach(self, mut result: Value) -> Value {
        let object = result.as_object_mut().expect("tool results are objects");
        object.insert(
            "events".into(),
            self.events.into_iter().map(|(event, _)| event).collect(),
        );
        object.insert("droppedEvents".into(), json!(self.dropped));
        result
    }
}

pub(crate) fn failed(value: &Value) -> bool {
    value.get("ok") == Some(&Value::Bool(false))
        || value.get("error").is_some()
        || value.get("stopReason").and_then(Value::as_str) == Some("cancelled")
}

pub(crate) fn normalize(mut value: Value) -> Value {
    let ok = !failed(&value);
    value["ok"] = json!(ok);
    value
}

pub(crate) fn failure(error: CliError) -> Value {
    json!({ "ok": false, "error": error.to_string(), "code": error.exit_code().as_i32() })
}

/// Recovery checkpoints outlive the command future and cannot be evicted by events.
#[derive(Default)]
pub(crate) struct OperationState {
    pub recovery: Map<String, Value>,
    pub batch: Option<BatchState>,
}

#[derive(Default)]
pub(crate) struct BatchState {
    pub results: Vec<Value>,
    pub active_call: Option<String>,
    pub text_truncated: bool,
}

impl OperationState {
    pub fn session(&mut self, session_id: &str, cwd: &std::path::Path, created: bool) {
        self.recovery.insert("sessionId".into(), json!(session_id));
        self.recovery.insert("cwd".into(), json!(cwd));
        self.recovery
            .insert("sessionCreated".into(), json!(created));
    }

    pub fn submission_started(&mut self, session_id: &str, input_id: &str, delivery: &str) {
        self.recovery.insert("sessionId".into(), json!(session_id));
        self.recovery
            .insert("deliveryState".into(), json!("unknown"));
        self.recovery.insert(
            "submission".into(),
            json!({
                "clientInputId": input_id, "delivery": delivery, "state": "unconfirmed",
            }),
        );
    }

    pub fn submission_acknowledged(&mut self, result: &Value) {
        self.recovery
            .insert("deliveryState".into(), json!("accepted"));
        let submission = self
            .recovery
            .get_mut("submission")
            .expect("submission checkpoint exists");
        submission["state"] = json!("acknowledged");
        submission["acknowledgement"] = result.clone();
    }

    pub fn attach_recovery(&self, mut result: Value) -> Value {
        if failed(&result) && !self.recovery.is_empty() {
            result["recovery"] = json!(self.recovery);
        }
        result
    }

    pub fn finish(self, result: Value) -> Value {
        let mut result = self.attach_recovery(result);
        if let Some(batch) = self.batch {
            if result.get("results").is_none() {
                result["results"] = json!(batch.results);
                result["failedCall"] = json!(batch.active_call);
            }
            let truncated = batch.text_truncated || result["textTruncated"] == true;
            result["textTruncated"] = json!(truncated);
        }
        normalize(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_history_is_bounded_and_reports_dropped_events() {
        let mut log = EventLog::default();
        for i in 0..MAX_EVENTS + 5 {
            log.push(json!({"i": i}));
        }
        let result = log.attach(json!({}));
        assert_eq!(result["events"].as_array().unwrap().len(), MAX_EVENTS);
        assert_eq!(result["events"][0]["i"], 5);
        assert_eq!(result["droppedEvents"], 5);
    }
}
