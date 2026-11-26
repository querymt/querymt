//! The fake agent of a fixture world (design D3): it answers the app's
//! requests the way the quick-panel test rig does, and builds the
//! notifications a scene's agent steps send.

use serde_json::{Value, json};

use super::scene::{NotifySpec, ToolCall};

/// The run id every scripted notification carries (`_meta.querymt.runId`).
const RUN: &str = "r1";

/// A message for the app: a notification, or the response to a request.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Out {
    Notify { method: String, params: Value },
    Reply { id: Value, result: Value },
}

/// A `session/prompt` that has not been answered yet.
#[derive(Debug, Clone, PartialEq)]
struct Turn {
    id: Value,
    session: String,
    message_id: String,
}

pub(super) struct FakeAgent {
    /// What `session/new` answers: the world's session, or `q1`.
    new_session: String,
    /// Where notifications go without an open turn.
    default_session: String,
    turn: Option<Turn>,
    turns: u32,
    /// Every request seen: `(method, params)`, in order.
    pub log: Vec<(String, Value)>,
}

impl FakeAgent {
    pub(super) fn new(world_session: Option<&str>) -> Self {
        let session = world_session.unwrap_or("q1").to_owned();
        FakeAgent {
            new_session: session.clone(),
            default_session: session,
            turn: None,
            turns: 0,
            log: Vec::new(),
        }
    }

    /// What the agent sends back for `request`. A `session/prompt` is held
    /// open until `end_turn`; like a real agent, it is acknowledged by
    /// echoing the prompt as `user_message_chunk`, which turns the app's
    /// pending "Sending prompt…" row into the user's message.
    pub(super) fn answer(&mut self, request: &Value) -> Vec<Out> {
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        let id = request["id"].clone();
        self.log.push((method.clone(), request["params"].clone()));
        let result = match method.as_str() {
            "session/new" => json!({ "sessionId": self.new_session }),
            "session/prompt" => {
                self.turns += 1;
                self.turn = Some(Turn {
                    id,
                    session: request["params"]["sessionId"]
                        .as_str()
                        .unwrap_or(&self.default_session)
                        .to_owned(),
                    message_id: format!("a{}", self.turns),
                });
                // The id of the app's pending row rides in the update's own
                // `_meta`; the run id in the notification's.
                let client_id = request
                    .pointer("/params/_meta/querymt/client_prompt_id")
                    .cloned();
                let blocks = request["params"]["prompt"].as_array();
                return blocks
                    .into_iter()
                    .flatten()
                    .map(|block| {
                        let mut update = json!({
                            "sessionUpdate": "user_message_chunk",
                            "messageId": format!("u{}", self.turns),
                            "content": block,
                        });
                        if let Some(id) = &client_id {
                            update["_meta"] = json!({ "querymt": { "client_prompt_id": id } });
                        }
                        Out::Notify {
                            method: "session/update".into(),
                            params: json!({
                                "sessionId": self.session(),
                                "update": update,
                                "_meta": { "querymt": { "runId": RUN } },
                            }),
                        }
                    })
                    .collect();
            }
            // `session/load`, `session/list` and the rest: the sessions come
            // from the world, not from the agent.
            _ => json!({}),
        };
        vec![Out::Reply { id, result }]
    }

    fn session(&self) -> &str {
        self.turn
            .as_ref()
            .map_or(&self.default_session, |t| &t.session)
    }

    fn message_id(&self) -> String {
        self.turn
            .as_ref()
            .map_or_else(|| "a0".to_owned(), |t| t.message_id.clone())
    }

    fn update(&self, update: Value) -> Out {
        Out::Notify {
            method: "session/update".into(),
            params: json!({
                "sessionId": self.session(),
                "update": update,
                "_meta": { "querymt": { "runId": RUN } },
            }),
        }
    }

    /// A piece of the reply.
    pub(super) fn stream(&self, text: &str) -> Out {
        self.update(json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": self.message_id(),
            "content": { "type": "text", "text": text },
        }))
    }

    pub(super) fn tool_call(&self, call: &ToolCall) -> Out {
        let mut update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": call.id,
            "title": call.title,
            "kind": call.kind,
            "status": call.status,
        });
        if let Some(text) = &call.content {
            update["content"] = json!([
                { "type": "content", "content": { "type": "text", "text": text } }
            ]);
        }
        self.update(update)
    }

    /// `running` (the spinner shows) or `idle`.
    pub(super) fn state(&self, state: &str) -> Out {
        Out::Notify {
            method: "_querymt/session/stateUpdate".into(),
            params: json!({
                "sessionId": self.session(),
                "state": state,
                "runId": RUN,
                "stopReason": "end_turn",
            }),
        }
    }

    /// A raw notification, as written.
    pub(super) fn notify(&self, spec: &NotifySpec) -> Out {
        Out::Notify {
            method: spec.method.clone(),
            params: spec.params.clone(),
        }
    }

    /// Answer the open prompt with `stop_reason`, then go idle. Without an
    /// open turn only the state update is sent.
    pub(super) fn end_turn(&mut self, stop_reason: &str) -> Vec<Out> {
        let mut out = Vec::new();
        let idle = self.state("idle");
        if let Some(turn) = self.turn.take() {
            out.push(Out::Reply {
                id: turn.id,
                result: json!({ "stopReason": stop_reason }),
            });
        }
        out.push(idle);
        out
    }

    pub(super) fn has_open_turn(&self) -> bool {
        self.turn.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[test]
    fn session_new_gets_the_world_session_or_q1() {
        let mut agent = FakeAgent::new(Some("film"));
        assert_eq!(
            agent.answer(&request(1, "session/new", json!({ "cwd": "/w" }))),
            vec![Out::Reply {
                id: json!(1),
                result: json!({ "sessionId": "film" })
            }]
        );
        let mut agent = FakeAgent::new(None);
        let out = agent.answer(&request(2, "session/new", json!({})));
        let [Out::Reply { result, .. }] = out.as_slice() else {
            panic!("session/new is answered");
        };
        assert_eq!(result, &json!({ "sessionId": "q1" }));
    }

    #[test]
    fn everything_else_gets_an_empty_result() {
        let mut agent = FakeAgent::new(None);
        for method in ["session/load", "session/list", "_querymt/models", "x/y"] {
            assert_eq!(
                agent.answer(&request(7, method, json!({}))),
                vec![Out::Reply {
                    id: json!(7),
                    result: json!({})
                }],
                "{method}"
            );
        }
        assert_eq!(agent.log.len(), 4);
        assert!(!agent.has_open_turn());
    }

    #[test]
    fn a_prompt_is_held_until_end_turn() {
        let mut agent = FakeAgent::new(Some("film"));
        let sent = agent.answer(&request(
            5,
            "session/prompt",
            json!({ "sessionId": "s-9",
                    "prompt": [{ "type": "text", "text": "Hi there" }],
                    "_meta": { "querymt": { "client_prompt_id": "c1" } } }),
        ));
        // Held: no reply, only the echo of the prompt.
        assert!(sent.iter().all(|o| matches!(o, Out::Notify { .. })));
        let [Out::Notify { method, params }] = sent.as_slice() else {
            panic!("one echo");
        };
        assert_eq!(method, "session/update");
        assert_eq!(params["sessionId"], "s-9");
        assert_eq!(params["update"]["sessionUpdate"], "user_message_chunk");
        assert_eq!(params["update"]["messageId"], "u1");
        assert_eq!(params["update"]["content"]["text"], "Hi there");
        assert_eq!(
            params["update"]["_meta"]["querymt"]["client_prompt_id"],
            "c1"
        );
        assert!(agent.has_open_turn());
        assert_eq!(agent.log[0].0, "session/prompt");

        // Notifications target the open turn's session, with a message id
        // that is fixed for the turn.
        let Out::Notify { method, params } = agent.stream("Hello ") else {
            panic!("a notification");
        };
        assert_eq!(method, "session/update");
        assert_eq!(params["sessionId"], "s-9");
        assert_eq!(params["update"]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(params["update"]["messageId"], "a1");
        assert_eq!(params["update"]["content"]["text"], "Hello ");
        assert_eq!(params["_meta"]["querymt"]["runId"], "r1");

        let out = agent.end_turn("end_turn");
        assert_eq!(
            out[0],
            Out::Reply {
                id: json!(5),
                result: json!({ "stopReason": "end_turn" })
            }
        );
        let Out::Notify { method, params } = &out[1] else {
            panic!("a state update");
        };
        assert_eq!(method, "_querymt/session/stateUpdate");
        assert_eq!(params["state"], "idle");
        assert_eq!(params["sessionId"], "s-9");
        assert!(!agent.has_open_turn());

        // The next turn gets its own message id.
        agent.answer(&request(
            6,
            "session/prompt",
            json!({ "sessionId": "s-9", "prompt": [] }),
        ));
        let Out::Notify { params, .. } = agent.stream("x") else {
            panic!("a notification");
        };
        assert_eq!(params["update"]["messageId"], "a2");
    }

    #[test]
    fn without_a_turn_notifications_target_the_world_session() {
        let mut agent = FakeAgent::new(Some("film"));
        let Out::Notify { params, .. } = agent.stream("hi") else {
            panic!("a notification");
        };
        assert_eq!(params["sessionId"], "film");
        assert_eq!(params["update"]["messageId"], "a0");
        // Ending a turn that is not open only goes idle.
        let out = agent.end_turn("end_turn");
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Out::Notify { params, .. } if params["state"] == "idle"));
    }

    #[test]
    fn tool_calls_and_raw_notifications_have_their_wire_shape() {
        let agent = FakeAgent::new(Some("film"));
        let call = ToolCall {
            id: "t1".into(),
            title: "Read src/lib.rs".into(),
            kind: "read".into(),
            status: "completed".into(),
            content: Some("42 lines".into()),
        };
        let Out::Notify { params, .. } = agent.tool_call(&call) else {
            panic!("a notification");
        };
        let u = &params["update"];
        assert_eq!(u["sessionUpdate"], "tool_call");
        assert_eq!(u["toolCallId"], "t1");
        assert_eq!(u["title"], "Read src/lib.rs");
        assert_eq!(u["kind"], "read");
        assert_eq!(u["status"], "completed");
        assert_eq!(u["content"][0]["content"]["text"], "42 lines");
        let bare = ToolCall {
            content: None,
            ..call
        };
        let Out::Notify { params, .. } = agent.tool_call(&bare) else {
            panic!("a notification");
        };
        assert!(params["update"].get("content").is_none());

        let Out::Notify { method, params } = agent.state("running") else {
            panic!("a notification");
        };
        assert_eq!(method, "_querymt/session/stateUpdate");
        assert_eq!(params["state"], "running");

        let raw = NotifySpec {
            method: "x/y".into(),
            params: json!({ "a": 1 }),
        };
        assert_eq!(
            agent.notify(&raw),
            Out::Notify {
                method: "x/y".into(),
                params: json!({ "a": 1 })
            }
        );
    }
}
