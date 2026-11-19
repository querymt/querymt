//! Tolerant two-step decoding.
//!
//! Step one checks the envelope; step two decodes the payload into the typed
//! wire value. An unknown `type` or `sessionUpdate` is reported as
//! [`Decoded::Unknown`]; any other failure is [`Decoded::Error`]. Decoding never
//! panics and never touches the view.

use querymt_agent_protocol_types::events::AgentEventKind;
use querymt_agent_protocol_types::protocol::SessionUpdate;
use serde::Deserialize;
use serde_json::Value;

/// Outcome of decoding one input or one event.
#[derive(Debug)]
pub(crate) enum Decoded<T> {
    Value(T),
    /// A well-formed input of a kind this reducer does not know.
    Unknown,
    Error(String),
}

/// True when serde rejected a tag value that no variant carries.
fn is_unknown_variant(error: &serde_json::Error) -> bool {
    error.to_string().starts_with("unknown variant")
}

/// A decoded `session/update` notification.
#[derive(Debug)]
pub(crate) struct Notification {
    pub session_id: String,
    pub update: SessionUpdate,
}

/// Decode a notification, either the full JSON-RPC message or its `params`.
pub(crate) fn decode_notification(value: &Value) -> Decoded<Notification> {
    let params = value.get("params").unwrap_or(value);
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        return Decoded::Error("notification has no string `sessionId`".into());
    };
    let Some(update) = params.get("update") else {
        return Decoded::Error("notification has no `update`".into());
    };
    if !update.is_object() {
        return Decoded::Error("`update` is not an object".into());
    }
    if update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .is_none()
    {
        return Decoded::Error("`update` has no string `sessionUpdate`".into());
    }
    match serde_json::from_value::<SessionUpdate>(update.clone()) {
        Ok(update) => Decoded::Value(Notification {
            session_id: session_id.to_owned(),
            update,
        }),
        Err(error) if is_unknown_variant(&error) => Decoded::Unknown,
        Err(error) => Decoded::Error(error.to_string()),
    }
}

/// An event after both decoding steps.
#[derive(Debug)]
pub(crate) struct Event {
    pub seq: i64,
    pub kind: AgentEventKind,
}

#[derive(Deserialize)]
struct Envelope {
    seq: i64,
    kind: Value,
}

/// Decode one snapshot or page event. The error carries the event's `seq`
/// whenever the envelope was readable.
pub(crate) fn decode_event(value: &Value) -> (Option<i64>, Decoded<Event>) {
    let envelope = match Envelope::deserialize(value) {
        Ok(envelope) => envelope,
        Err(error) => return (None, Decoded::Error(error.to_string())),
    };
    let seq = Some(envelope.seq);
    let decoded = match serde_json::from_value::<AgentEventKind>(envelope.kind) {
        Ok(kind) => Decoded::Value(Event {
            seq: envelope.seq,
            kind,
        }),
        Err(error) if is_unknown_variant(&error) => Decoded::Unknown,
        Err(error) => Decoded::Error(error.to_string()),
    };
    (seq, decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn note(update: Value) -> Value {
        json!({ "sessionId": "s1", "update": update })
    }

    #[test]
    fn unknown_update_kind_decodes_as_unknown() {
        let decoded = decode_notification(&note(json!({ "sessionUpdate": "zzz_new" })));
        assert!(matches!(decoded, Decoded::Unknown));
    }

    #[test]
    fn non_object_update_is_an_error() {
        let decoded = decode_notification(&note(json!("text")));
        assert!(matches!(decoded, Decoded::Error(_)));
    }

    #[test]
    fn known_kind_with_bad_fields_is_an_error_not_unknown() {
        let decoded = decode_notification(&note(json!({ "sessionUpdate": "agent_message_chunk" })));
        assert!(matches!(decoded, Decoded::Error(_)));
    }

    #[test]
    fn jsonrpc_wrapper_is_accepted() {
        let value = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": note(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "hi" }
            })),
        });
        assert!(matches!(decode_notification(&value), Decoded::Value(_)));
    }

    #[test]
    fn event_with_unknown_type_is_unknown_and_keeps_seq() {
        let (seq, decoded) = decode_event(&json!({
            "seq": 7, "timestamp": 1, "kind": { "type": "brand_new_event", "data": {} }
        }));
        assert_eq!(seq, Some(7));
        assert!(matches!(decoded, Decoded::Unknown));
    }

    #[test]
    fn event_with_bad_fields_is_an_error_and_keeps_seq() {
        let (seq, decoded) = decode_event(&json!({
            "seq": 9, "timestamp": 1, "kind": { "type": "tool_call_start", "data": { "tool_call_id": 5 } }
        }));
        assert_eq!(seq, Some(9));
        assert!(matches!(decoded, Decoded::Error(_)));
    }
}
