use anyhow::{Context, Result};
use clap::Parser;
#[cfg(feature = "extism_host")]
use querymt::plugin::extism_impl::host::ExtismLoader;
#[cfg(feature = "native")]
use querymt::plugin::host::native::NativeLoader;
use querymt::{
    builder::LLMBuilder,
    plugin::host::{PluginRegistry, ProviderConfig},
    stt::SttRequest,
};
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "stt_example",
    about = "Speech-to-text using QueryMT provider plugins",
    after_help = "Examples:\n  stt_example --audio ./sample.wav\n  stt_example --provider izwi --audio ./sample.wav --model Qwen3-ASR-0.6B\n  stt_example --provider openai --audio ./sample.wav --api-key $OPENAI_API_KEY"
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

    /// Provider name from the config file
    #[arg(short, long, default_value = "openai")]
    provider: String,

    /// Provider model override
    #[arg(short, long)]
    model: Option<String>,

    /// Audio file to transcribe
    #[arg(short, long, value_name = "FILE")]
    audio: PathBuf,

    /// Language hint (provider-specific)
    #[arg(long)]
    language: Option<String>,

    /// API base URL override
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,

    /// API key override
    #[arg(long, value_name = "KEY")]
    api_key: Option<String>,
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

fn ensure_loader_support(registry: &PluginRegistry, provider: &str) -> Result<()> {
    let Some(cfg) = registry
        .config
        .providers
        .iter()
        .find(|cfg| cfg.name == provider)
    else {
        return Ok(());
    };

    ensure_native_loader(cfg, provider)?;
    ensure_wasm_loader(cfg, provider)?;

    Ok(())
}

fn apply_provider_config(
    mut builder: LLMBuilder,
    registry: &PluginRegistry,
    provider: &str,
) -> Result<LLMBuilder> {
    let provider_cfg = registry
        .config
        .providers
        .iter()
        .find(|cfg| cfg.name == provider);
    let Some(provider_cfg) = provider_cfg else {
        return Ok(builder);
    };

    let Some(config) = &provider_cfg.config else {
        return Ok(builder);
    };

    for (key, value) in config {
        let json = serde_json::to_value(value)
            .with_context(|| format!("failed to serialize provider config key '{key}'"))?;
        builder = builder.parameter(key.clone(), json);
    }

    Ok(builder)
}

#[cfg(feature = "native")]
fn ensure_native_loader(_cfg: &ProviderConfig, _provider: &str) -> Result<()> {
    Ok(())
}

#[cfg(not(feature = "native"))]
fn ensure_native_loader(cfg: &ProviderConfig, provider: &str) -> Result<()> {
    if cfg.path.ends_with(std::env::consts::DLL_EXTENSION) {
        anyhow::bail!(
            "provider '{provider}' points to a native plugin ('{}'), but this binary was built without `native`. Re-run with `--features native`.",
            cfg.path
        );
    }
    Ok(())
}

#[cfg(feature = "extism_host")]
fn ensure_wasm_loader(_cfg: &ProviderConfig, _provider: &str) -> Result<()> {
    Ok(())
}

#[cfg(not(feature = "extism_host"))]
fn ensure_wasm_loader(cfg: &ProviderConfig, provider: &str) -> Result<()> {
    if cfg.path.ends_with(".wasm") {
        anyhow::bail!(
            "provider '{provider}' points to a wasm plugin ('{}'), but this binary was built without `extism_host`. Re-run with `--features extism_host`.",
            cfg.path
        );
    }
    Ok(())
}

fn resolved_model(cli_model: Option<String>) -> Option<String> {
    cli_model.or_else(|| env::var("OPENAI_MODEL").ok())
}

fn resolved_api_key(cli_api_key: Option<String>) -> Option<String> {
    cli_api_key.or_else(|| env::var("OPENAI_API_KEY").ok())
}

fn resolved_base_url(cli_base_url: Option<String>) -> Option<String> {
    cli_base_url.or_else(|| env::var("OPENAI_BASE_URL").ok())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let registry = build_registry(&args.provider_config)?;
    ensure_loader_support(&registry, &args.provider)?;

    let audio_bytes = std::fs::read(&args.audio)
        .with_context(|| format!("failed to read audio file '{}'", args.audio.display()))?;

    let mut builder = apply_provider_config(
        LLMBuilder::new().provider(args.provider.clone()),
        &registry,
        &args.provider,
    )?;

    if let Some(model) = resolved_model(args.model.clone()) {
        builder = builder.model(model);
    }
    if let Some(api_key) = resolved_api_key(args.api_key.clone()) {
        builder = builder.api_key(api_key);
    }
    if let Some(base_url) = resolved_base_url(args.base_url.clone()) {
        builder = builder.base_url(base_url);
    }

    let llm = builder
        .build(&registry)
        .await
        .context("failed to initialize provider")?;

    let mut req = SttRequest::new().audio(audio_bytes);
    if let Some(filename) = args
        .audio
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
    {
        req = req.filename(filename);
    }
    if let Some(model) = args.model {
        req = req.model(model);
    }
    if let Some(language) = args.language {
        req = req.language(language);
    }

    let resp = llm.transcribe(&req).await.context("transcription failed")?;
    println!("{}", resp.text);

    Ok(())
}
