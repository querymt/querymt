//! Cost of one model step of a run over a long session history.
//!
//! Motivation (openspec change `restructure-agent-turn-state`, design D3
//! "Measurement"): each model request copies the run's history several times
//! today, and the change replaces those copies with one projection of the
//! stored messages. This bench records the cost of a run of `TOOL_STEPS` tool
//! steps (so `TOOL_STEPS + 1` model requests) over a seeded history, so the
//! step time before and after the change can be compared.
//!
//! Histories
//! - `messages_400` and `messages_1600`: alternating user and assistant text
//!   messages of about 2,000 bytes.
//! - `messages_400_images`: 400 messages of the same size in which every tenth
//!   user message also carries a 200 kB image attachment. Projecting a stored
//!   message validates its attachments, so this is the history on which a
//!   projection costs most.
//!
//! The provider is the mock of `querymt_agent::test_utils`; its tool calls go
//! to `call_tool`, so no file system work is timed. The history is seeded
//! outside the timed section, in a new session per iteration, so every
//! iteration starts from the same history.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use querymt::LLMParams;
use querymt::chat::{ChatMessage, ChatRole, Tool};
use querymt_agent::acp::protocol::{
    ContentBlock, ImageContent, NewSessionRequest, PromptRequest, TextContent,
};
use querymt_agent::agent::{AgentConfigBuilder, LocalAgentHandle, ToolPolicy};
use querymt_agent::model::{AgentMessage, MessagePart};
use querymt_agent::send_agent::SendAgent;
use querymt_agent::session::SqliteStorage;
use querymt_agent::session::store::SessionStore;
use querymt_agent_runtime::test_utils::{
    MockChatResponse, MockLlmProvider, SharedLlmProvider, TestProviderFactory,
    mock_plugin_registry, mock_querymt_tool_call,
};
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

/// Tool steps of the measured run; it makes `TOOL_STEPS + 1` model requests.
const TOOL_STEPS: usize = 5;
/// Size of each seeded message.
const MESSAGE_BYTES: usize = 2_000;
/// Size of an image attachment.
const IMAGE_BYTES: usize = 200_000;
/// Prefix of the prompt that starts the measured run.
const RUN_PROMPT: &str = "bench run";

#[derive(Clone, Copy)]
struct History {
    label: &'static str,
    messages: usize,
    /// Every tenth user message carries an image.
    images: bool,
}

const HISTORIES: [History; 3] = [
    History {
        label: "messages_400",
        messages: 400,
        images: false,
    },
    History {
        label: "messages_1600",
        messages: 1_600,
        images: false,
    },
    History {
        label: "messages_400_images",
        messages: 400,
        images: true,
    },
];

fn lookup_tool() -> Tool {
    Tool {
        tool_type: "function".into(),
        function: querymt::chat::FunctionTool {
            name: "lookup".into(),
            description: "Looks something up".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            strict: None,
        },
    }
}

/// A provider that calls `lookup` until the run holds `TOOL_STEPS` results,
/// then answers with text.
fn provider() -> SharedLlmProvider {
    let respond = Arc::new(|messages: &[ChatMessage]| {
        let run_start = messages
            .iter()
            .rposition(|m| m.text().starts_with(RUN_PROMPT))
            .unwrap_or(0);
        let step = messages[run_start..]
            .iter()
            .filter(|m| m.portable_input_parts().iter().any(|p| p.is_tool_result()))
            .count();
        if step < TOOL_STEPS {
            MockChatResponse::with_tools(
                "",
                vec![mock_querymt_tool_call(
                    &format!("call-{step}-{}", messages.len()),
                    "lookup",
                    "{}",
                )],
            )
        } else {
            MockChatResponse::text_only("done")
        }
    });
    let mut mock = MockLlmProvider::new();
    let with_tools = respond.clone();
    mock.expect_chat_with_tools()
        .returning(move |messages, _| Ok(with_tools(messages).into()));
    mock.expect_chat()
        .returning(move |messages| Ok(respond(messages).into()));
    mock.expect_call_tool().returning(|_, _| {
        Ok(vec![querymt::chat::ToolResultPart::Text {
            text: "found".into(),
        }])
    });
    SharedLlmProvider::new(mock, vec![lookup_tool()])
}

struct Bench {
    handle: LocalAgentHandle,
    store: Arc<dyn SessionStore>,
    cwd: tempfile::TempDir,
    _registry_dir: tempfile::TempDir,
}

async fn bench_agent() -> Bench {
    let (registry, registry_dir) =
        mock_plugin_registry(Arc::new(TestProviderFactory::new(provider())))
            .expect("plugin registry");
    let storage = Arc::new(
        SqliteStorage::connect(":memory:".into())
            .await
            .expect("storage"),
    );
    let config = Arc::new(
        AgentConfigBuilder::new(
            Arc::new(registry),
            storage,
            LLMParams::new().provider("mock").model("mock-model"),
        )
        .with_tool_policy(ToolPolicy::ProviderOnly)
        .build(),
    );
    let store = config.provider.history_store();
    Bench {
        handle: LocalAgentHandle::from_config(config),
        store,
        cwd: tempfile::tempdir().expect("cwd"),
        _registry_dir: registry_dir,
    }
}

fn text_of(seed: usize, bytes: usize) -> String {
    let header = format!("message {seed}: ");
    let mut text = String::with_capacity(bytes);
    text.push_str(&header);
    text.extend(std::iter::repeat_n('x', bytes - header.len()));
    text
}

/// A new session holding `history`.
async fn seeded_session(bench: &Bench, history: History) -> String {
    let session_id = SendAgent::new_session(
        &bench.handle,
        NewSessionRequest::new(bench.cwd.path().to_path_buf()),
    )
    .await
    .expect("new session")
    .session_id
    .to_string();
    let image = "A".repeat(IMAGE_BYTES);
    for index in 0..history.messages {
        let user = index % 2 == 0;
        let parts = if user && history.images && (index / 2) % 10 == 0 {
            vec![MessagePart::Prompt {
                blocks: vec![
                    ContentBlock::Text(TextContent::new(text_of(index, MESSAGE_BYTES))),
                    ContentBlock::Image(ImageContent::new(image.clone(), "image/png")),
                ],
            }]
        } else {
            vec![MessagePart::Text {
                content: text_of(index, MESSAGE_BYTES),
            }]
        };
        bench
            .store
            .add_message(
                &session_id,
                AgentMessage {
                    id: format!("{session_id}-seed-{index:05}"),
                    session_id: session_id.clone(),
                    role: if user {
                        ChatRole::User
                    } else {
                        ChatRole::Assistant
                    },
                    parts,
                    created_at: 1_700_000_000 + index as i64,
                    parent_message_id: None,
                    source_provider: None,
                    source_model: None,
                },
            )
            .await
            .expect("seed message");
    }
    session_id
}

fn bench_turn_step(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let bench = rt.block_on(bench_agent());

    let mut group = c.benchmark_group("turn_step");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));
    // Elements are model requests, so the report is per step.
    group.throughput(Throughput::Elements(TOOL_STEPS as u64 + 1));

    for history in HISTORIES {
        group.bench_with_input(
            BenchmarkId::new("run_with_tool_steps", history.label),
            &history,
            |b, history| {
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iterations {
                            let session_id = seeded_session(&bench, *history).await;
                            let request = PromptRequest::new(
                                session_id,
                                vec![ContentBlock::Text(TextContent::new(RUN_PROMPT))],
                            );
                            let started = Instant::now();
                            let response = SendAgent::prompt(&bench.handle, request)
                                .await
                                .expect("run completes");
                            elapsed += started.elapsed();
                            black_box(response);
                        }
                        elapsed
                    })
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(2));
    targets = bench_turn_step
);
criterion_main!(benches);
