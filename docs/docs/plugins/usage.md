# Using Plugins with the Host

Once plugins are developed, QueryMT's `querymt::plugin::host::PluginRegistry` is responsible for loading them from a configuration file and making them available to the application.

## Initialization

The registry is typically initialized at application startup by pointing it to a configuration file. For the common dynamic-plugin setup, use the feature-gated `dynamic` extension layer instead of wiring `PluginLoader` implementations manually.

```rust
use querymt::{PluginRegistry, dynamic::PluginRegistryDynamicExt, error::LLMError};

# async fn example() -> Result<(), LLMError> {
// 1. Create a registry from a config file path and attach the built-in loaders.
let registry = PluginRegistry::from_path("providers.toml")?
    .with_dynamic_loaders();

// 2. Load all plugins defined in the config file.
registry.load_all_plugins().await;
# Ok(())
# }
```

This process will:
1. Parse the configuration file (e.g., `plugins.toml`).
2. For each configured provider:
    - Determine its location (local path, OCI image).
    - If it's an OCI image, download and cache it.
    - Determine its type (`Native` or `Wasm`).
    - Use the corresponding registered `PluginLoader` to load the plugin into memory.
    - Create a factory (`Arc<dyn LLMProviderFactory>`) for the plugin and store it in the registry, keyed by its configured name.

## Accessing Provider Factories

Once loaded, you can use `registry.builder(...)` to create provider instances. The builder will use the registry to find the correct factory and automatically merge defaults from `[providers.config]` before pruning the final config by schema.

```rust
use querymt::LLMProvider;
use querymt::PluginRegistry;
use serde_json::json;
# async fn example(registry: PluginRegistry) -> anyhow::Result<()> {

// Use the builder to configure a provider by its name from the config file.
let provider: Box<dyn LLMProvider> = registry
    .builder("my_openai_plugin") // This name must match a name in providers.toml
    .model("gpt-4-turbo")
    .parameter("custom_param", json!(true))
    .build()
    .await?;

// Now you have an LLMProvider instance and can use it.
println!("Successfully created provider instance for 'my_openai_plugin'");
// e.g., provider.chat(&messages).await?

# Ok(())
# }
```

You can also interact with the registry directly to list available factories or get their configuration schemas.

```rust
# use querymt::plugin::host::PluginRegistry;
# use querymt::plugin::LLMProviderFactory;
# use std::sync::Arc;
# fn example(registry: PluginRegistry) -> anyhow::Result<()> {
// List all loaded provider factories
let all_factories: Vec<Arc<dyn LLMProviderFactory>> = registry.list();
for factory in all_factories {
    println!("Loaded provider factory: {}", factory.name());
}

// Get a specific provider factory by name
if let Some(factory) = registry.get("my_openai_plugin") {
    // Get the config schema for this provider
    let schema = factory.config_schema();
    println!("Config schema: {}", serde_json::to_string_pretty(&schema)?);
}
# Ok(())
# }
```

This design allows QueryMT to dynamically load and work with multiple LLM providers configured at runtime, without needing to compile them into the main application.
