# Using the Plugin Registry

Once plugins are configured, QueryMT's `ExtismProviderRegistry` is responsible for loading them and making them available.

## Initialization

The registry is typically initialized at application startup, pointing to the plugin configuration file:

```rust
use querymt::plugin::extism_impl::ExtismProviderRegistry;
use querymt::plugin::ProviderRegistry; // Trait for common methods

# async fn example() -> anyhow::Result<()> {
let registry = ExtismProviderRegistry::new("extism_plugins.toml").await?;
# Ok(())
# }
```

This will:
1. Parse the configuration file (`extism_plugins.toml` in this example).
2. For each configured provider:
    - Download the Wasm module if the path is HTTP or OCI. For OCI, it uses a local cache.
    - Load the Wasm module into an Extism `Plugin` instance.
    - Call the plugin's `name()` function to verify it.
    - Store a factory for this plugin in the registry.

## Accessing Providers

You can list all available providers or get a specific provider factory by its configured name:

```rust
# use querymt::plugin::extism_impl::ExtismProviderRegistry;
# use querymt::plugin::{ProviderRegistry, LLMProviderFactory};
# use querymt::LLMProvider;
# use serde_json::json;
# use std::sync::Arc;
#
# async fn example() -> anyhow::Result<()> {
# let registry = ExtismProviderRegistry::new("extism_plugins.toml").await?;
// List all loaded provider factories
let all_factories: Vec<Arc<dyn LLMProviderFactory>> = registry.list();
for factory in all_factories {
    println!("Loaded provider factory: {}", factory.name());
}

// Get a specific provider factory by name (from config)
if let Some(factory) = registry.get("my_openai_plugin") {
    println!("Found factory for: {}", factory.name());

    // Get the config schema for this provider
    let schema = factory.config_schema();
    println!("Config schema: {}", serde_json::to_string_pretty(&schema)?);

    // Create an instance of the provider using its specific configuration
    // This configuration would typically come from user input or another config source,
    // matching the schema provided by `factory.config_schema()`.
    let provider_config = json!({
        "api_key_env": "MY_OPENAI_API_KEY_FROM_ENV", // Assuming plugin handles env var itself
        "model": "gpt-3.5-turbo"
    });

    match factory.from_config(&provider_config) {
        Ok(provider_instance) => {
            // Now you have an LLMProvider instance (Box<dyn LLMProvider>)
            // You can use its methods (chat, embed, complete)
            println!("Successfully created provider instance for {}", factory.name());
            // e.g., provider_instance.chat(&messages).await?
        }
        Err(e) => {
            eprintln!("Failed to create provider instance: {}", e);
        }
    }
} else {
    println!("Provider 'my_openai_plugin' not found.");
}
# Ok(())
# }
```

## Provider Instance

The `factory.from_config(&config_value)` method returns a `Result<Box<dyn LLMProvider>, LLMError>`.
The `Box<dyn LLMProvider>` instance can then be used to perform LLM operations like chat, embedding, and completion. This instance holds the specific configuration it was created with.

The `LLMProvider` trait combines `BasicChatProvider`, `ToolChatProvider`, `EmbeddingProvider`, and `CompletionProvider`.

```rust
use querymt::{
    chat::{ChatMessage, ChatResponse, Tool},
    completion::CompletionRequest,
    LLMProvider, // This is the combined trait
};

# async fn use_provider(provider: Box<dyn LLMProvider>) -> anyhow::Result<()> {
// Example: Chat
let messages = vec![
    ChatMessage::system("You are a helpful assistant."),
    ChatMessage::user("Hello!"),
];
let response: Box<dyn ChatResponse> = provider.chat(&messages).await?;
if let Some(text) = response.text() {
    println!("Assistant: {}", text);
}

// Example: Embedding (if the provider supports it)
let embeddings = provider.embed(vec!["querymt is cool".to_string()]).await?;
println!("Embeddings: {:?}", embeddings);
# Ok(())
# }
```
This design allows QueryMT to work with multiple LLM providers dynamically, configured at runtime.

