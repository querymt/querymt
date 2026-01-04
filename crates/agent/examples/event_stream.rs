use agent_client_protocol::{Agent, ContentBlock, NewSessionRequest, PromptRequest, TextContent};
use querymt::builder::LLMBuilder;
use querymt::plugin::{
    extism_impl::host::ExtismLoader, host::PluginRegistry, host::native::NativeLoader,
};
use querymt_agent::agent::QueryMTAgent;
use querymt_agent::events::AgentEventKind;
use querymt_agent::session::sqlite::SqliteSessionStore;
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = match PluginRegistry::from_default_path() {
        Ok(registry) => registry,
        Err(err) => {
            eprintln!(
                "Failed to load providers config from default path: {}",
                err
            );
            return Ok(());
        }
    };
    registry.register_loader(Box::new(ExtismLoader));
    registry.register_loader(Box::new(NativeLoader));
    registry.load_all_plugins().await;

    let provider = LLMBuilder::new()
        .provider("openai".to_string())
        .api_key(std::env::var("OPENAI_API_KEY").unwrap_or("sk-OPENAI".into()))
        .model("gpt-4o-mini")
        .build(&registry)?;
    let provider_arc: Arc<dyn querymt::LLMProvider> = Arc::from(provider);

    let store = SqliteSessionStore::connect(std::path::PathBuf::from(":memory:")).await?;
    let store_arc = Arc::new(store);

    let agent = QueryMTAgent::new(provider_arc.clone(), store_arc.clone());
    let mut events = agent.subscribe_events();

    let session = agent
        .new_session(NewSessionRequest::new(PathBuf::from(".")))
        .await?;

    let prompt = PromptRequest::new(
        session.session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(
            "Write a short story about life.",
        ))],
    );

    let _ = agent.prompt(prompt).await?;

    let mut seen_assistant = false;
    let mut seen_llm_end = false;
    for _ in 0..50 {
        if let Ok(Ok(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await
        {
            println!("{:?}", event);
            match event.kind {
                AgentEventKind::AssistantMessageStored { .. } => seen_assistant = true,
                AgentEventKind::LlmRequestEnd { .. } => seen_llm_end = true,
                _ => {}
            }
            if seen_assistant && seen_llm_end {
                break;
            }
        } else {
            break;
        }
    }
    Ok(())
}
