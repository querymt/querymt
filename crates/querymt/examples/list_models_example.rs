use anyhow::{Context, Result};
use clap::Parser;
use querymt::{dynamic::PluginRegistryDynamicExt, plugin::host::PluginRegistry};
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
    let registry = PluginRegistry::from_path(cfg_file)
        .with_context(|| format!("failed to load provider config '{}'", cfg_file.display()))?
        .with_dynamic_loaders();

    #[cfg(not(any(feature = "extism_host", feature = "native")))]
    anyhow::bail!(
        "this example was built without plugin loaders. Rebuild with `--features extism_host` and/or `--features native`."
    );

    Ok(registry)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let registry = build_registry(&args.provider_config)?;

    let provider_names: Vec<String> = match &args.provider {
        Some(name) => vec![name.clone()],
        None => registry
            .list_provider_names()
            .into_iter()
            .map(str::to_string)
            .collect(),
    };

    for name in &provider_names {
        match registry.list_models(name).await {
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
