use anyhow::{Context, Result};
use clap::Parser;
#[cfg(feature = "extism_host")]
use querymt::plugin::extism_impl::host::ExtismLoader;
#[cfg(feature = "native")]
use querymt::plugin::host::native::NativeLoader;
use querymt::{
    builder::LLMBuilder,
    plugin::host::{PluginRegistry, ProviderConfig},
    tts::TtsRequest,
};
use std::{env, path::Path, path::PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "tts_example",
    about = "Text-to-speech using QueryMT provider plugins",
    after_help = "Examples:\n  tts_example --text \"hello world\" --out ./out.wav\n  tts_example --provider izwi --model Kokoro-82M --text \"hello\" --out ./out.wav --format wav\n  tts_example --provider openai --text \"hello\" --out ./out.mp3 --format mp3 --api-key $OPENAI_API_KEY"
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

    /// Text to synthesize
    #[arg(short, long)]
    text: String,

    /// Output audio file
    #[arg(short, long = "out", value_name = "FILE")]
    out_file: PathBuf,

    /// Voice hint (provider-specific)
    #[arg(long)]
    voice: Option<String>,

    /// Output format (provider-specific, e.g. wav/mp3/pcm)
    #[arg(long)]
    format: Option<String>,

    /// Speech speed multiplier (provider-specific)
    #[arg(long)]
    speed: Option<f32>,

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

    let model = resolved_model(args.model.clone());

    let mut builder = apply_provider_config(
        LLMBuilder::new().provider(args.provider.clone()),
        &registry,
        &args.provider,
    )?;
    if let Some(model) = model.clone() {
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

    let mut req = TtsRequest::new().text(args.text);
    if let Some(model) = model {
        req = req.model(model);
    }
    if let Some(voice) = args.voice {
        req = req.voice(voice);
    }
    if let Some(format) = args.format {
        req = req.format(format);
    }
    if let Some(speed) = args.speed {
        req = req.speed(speed);
    }

    let resp = llm.speech(&req).await.context("speech synthesis failed")?;
    std::fs::write(&args.out_file, &resp.audio).with_context(|| {
        format!(
            "failed to write synthesized audio to '{}'",
            args.out_file.display()
        )
    })?;

    if let Some(mime_type) = resp.mime_type {
        eprintln!("wrote {} ({})", args.out_file.display(), mime_type);
    } else {
        eprintln!("wrote {}", args.out_file.display());
    }

    Ok(())
}
