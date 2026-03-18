use anyhow::{Context, Result};
use clap::Parser;
#[cfg(feature = "extism_host")]
use querymt::plugin::extism_impl::host::ExtismLoader;
use querymt::plugin::host::PluginRegistry;
#[cfg(feature = "native")]
use querymt::plugin::host::native::NativeLoader;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "list_models_example",
    about = "List available models from QueryMT provider plugins",
    after_help = "Examples:\n  list_models_example\n  list_models_example --provider izwi\n  list_models_example --provider openai --provider-config ./providers.toml"
)]
struct Args {
    /// Path to provider config file
    #[arg(
        short = 'c',
        long = "provider-config",
        default_value = "providers.toml",
        value_name = "FILE"
    )]
    provider_config: PathBuf,

    /// Provider name to list models for (lists all providers if omitted)
    #[arg(short, long)]
    provider: Option<String>,
}

fn build_registry(cfg_file: &Path) -> Result<PluginRegistry> {
    let mut registry = PluginRegistry::from_path(cfg_file)
        .with_context(|| format!("failed to load provider config '{}'", cfg_file.display()))?;

    #[cfg(feature = "extism_host")]
    registry.register_loader(Box::new(ExtismLoader));
    #[cfg(feature = "native")]
    registry.register_loader(Box::new(NativeLoader));

    #[cfg(not(any(feature = "extism_host", feature = "native")))]
    anyhow::bail!(
        "this example was built without plugin loaders. Rebuild with `--features extism_host` and/or `--features native`."
    );

    Ok(registry)
}

/// Serialize a provider's config section to JSON for the factory `list_models` call.
fn provider_config_json(registry: &PluginRegistry, provider: &str) -> String {
    let cfg = registry
        .config
        .providers
        .iter()
        .find(|c| c.name == provider)
        .and_then(|c| c.config.as_ref());

    match cfg {
        Some(map) => serde_json::to_string(map).unwrap_or_else(|_| "{}".to_string()),
        None => "{}".to_string(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let registry = build_registry(&args.provider_config)?;

    let provider_names: Vec<String> = match &args.provider {
        Some(name) => vec![name.clone()],
        None => registry
            .config
            .providers
            .iter()
            .map(|c| c.name.clone())
            .collect(),
    };

    for name in &provider_names {
        let factory = registry
            .get(name)
            .await
            .with_context(|| format!("failed to load provider '{name}'"))?;

        let cfg_json = provider_config_json(&registry, name);
        match factory.list_models(&cfg_json).await {
            Ok(models) => {
                println!("{name} ({} models):", models.len());
                for model in &models {
                    println!("  {model}");
                }
            }
            Err(e) => {
                eprintln!("{name}: failed to list models: {e}");
            }
        }
    }

    Ok(())
}
