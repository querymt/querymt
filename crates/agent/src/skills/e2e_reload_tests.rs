//! End-to-end hot-reload scenario for the `skill` tool (openspec change
//! `hot-reload-skills`, task 6.4).
//!
//! Drives a real agent session against a scripted mock provider and performs
//! the manual check from the change:
//!
//! 1. start a session (no skills advertised),
//! 2. add a temporary skill whose explicit protocol `id` differs from its
//!    display name and confirm the next model request advertises and loads it
//!    by ID,
//! 3. edit it and confirm the following request sees the update,
//! 4. remove it and confirm it disappears from the schema and a stale
//!    invocation returns the deterministic availability error.
//!
//! The temporary skill lives in a `TempDir`, so cleanup is automatic.

use crate::api::{Agent, AgentInfra};
use crate::config::SkillsConfig;
use crate::session::backend::StorageBackend;
use crate::session::sqlite_storage::SqliteStorage;
use crate::test_utils::{
    MockChatResponse, MockLlmProvider, SharedLlmProvider, TestProviderFactory,
    mock_plugin_registry, mock_querymt_tool_call,
};
use querymt::plugin::host::PluginRegistry;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// What the scripted provider should do on the next model request.
#[derive(Debug, Clone)]
enum Action {
    /// Reply with text only (no tool call).
    Text,
    /// Invoke the `skill` tool for the given callable ID.
    LoadSkill(String),
}

#[derive(Default, Clone)]
struct RecordedRequest {
    /// Callable IDs advertised by the `skill` tool schema, if the tool was
    /// present in the request.
    skill_enum: Option<Vec<String>>,
    /// The `skill` tool schema description, if advertised.
    skill_description: Option<String>,
    /// Flattened conversation text sent with the request.
    messages: String,
}

type Recorder = Arc<Mutex<Vec<RecordedRequest>>>;
type Script = Arc<Mutex<VecDeque<Action>>>;

fn skill_tool_call(id: &str) -> querymt::ToolCall {
    mock_querymt_tool_call("call-skill", "skill", &format!("{{\"name\":\"{}\"}}", id))
}

fn write_skill(dir: &Path, name: &str, id: &str, description: &str, body: &str) {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            "---\nname: {}\nid: {}\ndescription: {}\n---\n{}\n",
            name, id, description, body
        ),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hot_reload_end_to_end_tracks_the_filesystem_across_model_requests() {
    let skills_dir = TempDir::new().unwrap();

    // Script + recorder shared with the provider's chat_with_tools mock.
    let script: Script = Arc::new(Mutex::new(VecDeque::new()));
    let recorded: Recorder = Arc::new(Mutex::new(Vec::new()));

    let provider = Arc::new(tokio::sync::Mutex::new(MockLlmProvider::new()));
    {
        let script = Arc::clone(&script);
        let recorded = Arc::clone(&recorded);
        let mut mock = provider.try_lock().expect("provider unlocked during setup");
        mock.expect_chat_with_tools().returning(
            move |messages: &[querymt::chat::ChatMessage],
                  tools: Option<&[querymt::chat::Tool]>| {
                let mut entry = RecordedRequest {
                    skill_enum: None,
                    skill_description: None,
                    messages: messages
                        .iter()
                        .map(|message| {
                            message
                                .input_parts()
                                .iter()
                                .map(|part| match part {
                                    querymt::chat::ChatInputPart::Text { text } => text.clone(),
                                    other => format!("{other:?}"),
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .collect::<Vec<_>>()
                        .join("\n---\n"),
                };
                if let Some(tools) = tools
                    && let Some(skill_tool) =
                        tools.iter().find(|tool| tool.function.name == "skill")
                {
                    entry.skill_description = Some(skill_tool.function.description.clone());
                    entry.skill_enum = Some(
                        skill_tool.function.parameters["properties"]["name"]["enum"]
                            .as_array()
                            .map(|values| {
                                values
                                    .iter()
                                    .map(|value| value.as_str().unwrap().to_string())
                                    .collect()
                            })
                            .unwrap_or_default(),
                    );
                }
                recorded.lock().unwrap().push(entry);

                let action = script.lock().unwrap().pop_front().unwrap_or(Action::Text);
                match action {
                    Action::Text => Ok(MockChatResponse::text_only("done").into()),
                    Action::LoadSkill(id) => Ok(MockChatResponse::with_tools(
                        "Loading skill",
                        vec![skill_tool_call(&id)],
                    )
                    .into()),
                }
            },
        );
    }

    // Register the scripted provider under the "mock" provider name.
    let (registry, _registry_temp): (PluginRegistry, TempDir) =
        mock_plugin_registry(Arc::new(TestProviderFactory::new(SharedLlmProvider {
            inner: Arc::clone(&provider),
            tools: vec![].into_boxed_slice(),
        })))
        .unwrap();

    let storage = Arc::new(
        SqliteStorage::connect(":memory:".into())
            .await
            .expect("sqlite storage"),
    );

    let agent = Agent::single()
        .provider("mock", "mock-model")
        .cwd(skills_dir.path())
        .skills(SkillsConfig {
            enabled: true,
            // Only the configured temp source: deterministic regardless of the
            // developer's global skill directories.
            include_external: false,
            paths: vec![skills_dir.path().to_path_buf()],
            ..Default::default()
        })
        .infra(AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage as Arc<dyn StorageBackend>),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .build()
        .await
        .expect("build agent");

    // --- Turn 1: no skills on disk -----------------------------------------
    script.lock().unwrap().push_back(Action::Text);
    agent.chat("Anything new?").await.expect("turn 1");

    {
        let recorded = recorded.lock().unwrap();
        let first = recorded.first().expect("request recorded");
        assert_eq!(first.skill_enum, Some(vec![]), "no skills advertised yet");
    }

    // --- Turn 2: add a skill whose explicit ID differs from its name -------
    write_skill(
        skills_dir.path(),
        "Temp Review Body",
        "temp-review",
        "Initial review description",
        "ORIGINAL-REVIEW-BODY",
    );

    script
        .lock()
        .unwrap()
        .push_back(Action::LoadSkill("temp-review".to_string()));
    script.lock().unwrap().push_back(Action::Text);
    agent.chat("Use the review skill").await.expect("turn 2");

    {
        let recorded = recorded.lock().unwrap();
        let advertise = &recorded[1];
        assert_eq!(
            advertise.skill_enum,
            Some(vec!["temp-review".to_string()]),
            "new skill advertised by stable ID on the next model request"
        );
        let description = advertise.skill_description.as_deref().unwrap();
        assert!(description.contains("Initial review description"));
        // The human-readable name stays display metadata only.
        assert!(description.contains("display name: Temp Review Body"));

        let after_load = &recorded[2];
        assert!(
            after_load.messages.contains("ORIGINAL-REVIEW-BODY"),
            "skill loads by ID and its content reaches the next request"
        );
        assert!(after_load.messages.contains("Initial review description"));
    }

    // --- Turn 3: edit the skill definition ---------------------------------
    write_skill(
        skills_dir.path(),
        "Temp Review Body",
        "temp-review",
        "Edited review description",
        "EDITED-REVIEW-BODY",
    );

    script
        .lock()
        .unwrap()
        .push_back(Action::LoadSkill("temp-review".to_string()));
    script.lock().unwrap().push_back(Action::Text);
    agent.chat("Use it again").await.expect("turn 3");

    {
        let recorded = recorded.lock().unwrap();
        let advertise = &recorded[3];
        assert_eq!(
            advertise.skill_enum,
            Some(vec!["temp-review".to_string()]),
            "still advertised by stable ID after the edit"
        );
        assert!(
            advertise
                .skill_description
                .as_deref()
                .unwrap()
                .contains("Edited review description"),
            "the next model request sees the edited metadata"
        );

        let after_load = &recorded[4];
        assert!(
            after_load.messages.contains("EDITED-REVIEW-BODY"),
            "the edited content is served after the schema refresh"
        );
        // Note: the pre-edit tool result legitimately remains in the
        // conversation history; only the freshly loaded content changes.
    }

    // --- Turn 4: remove the skill ------------------------------------------
    fs::remove_dir_all(skills_dir.path().join("Temp Review Body")).unwrap();

    script
        .lock()
        .unwrap()
        .push_back(Action::LoadSkill("temp-review".to_string()));
    script.lock().unwrap().push_back(Action::Text);
    agent.chat("Once more?").await.expect("turn 4");

    {
        let recorded = recorded.lock().unwrap();
        let advertise = &recorded[5];
        assert_eq!(
            advertise.skill_enum,
            Some(vec![]),
            "removed skill no longer advertised"
        );

        let stale = &recorded[6];
        // The refreshed schema no longer contains the removed ID, so the
        // stale invocation is rejected deterministically at the tool-argument
        // validation layer (naming the requested ID and the empty allowed
        // set) instead of reaching the tool. The tool-level not-found path is
        // covered by the focused tests in skills::tool.
        assert!(
            stale.messages.contains("is not one of") && stale.messages.contains("temp-review"),
            "stale invocation fails deterministically: {:?}",
            stale.messages
        );
        assert!(
            stale.messages.contains("is_error: true"),
            "stale invocation is an error result: {:?}",
            stale.messages
        );
    }
}
