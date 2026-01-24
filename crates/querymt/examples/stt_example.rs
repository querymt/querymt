use anyhow::Result;
use querymt::{
    builder::LLMBuilder,
    plugin::{extism_impl::host::ExtismLoader, host::PluginRegistry},
    stt::SttRequest,
};
use std::env;
use std::path::PathBuf;

fn usage() -> String {
    let bin = env::args().next().unwrap_or_else(|| "stt_example".to_string());
    format!(
        "Usage: {bin} <providers.toml> <audio_file> [provider] [model]\n\nExample:\n  {bin} ./providers.toml ./audio.wav openai whisper-1\n\nEnv overrides:\n  OPENAI_API_KEY, OPENAI_BASE_URL, OPENAI_MODEL\n"
    )
}

fn build_registry(cfg_file: PathBuf) -> Result<PluginRegistry> {
    let mut registry = PluginRegistry::from_path(cfg_file)?;
    registry.register_loader(Box::new(ExtismLoader));
    Ok(registry)
}

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = env::args().collect();
    if argv.len() < 3 {
        eprintln!("{}", usage());
        std::process::exit(2);
    }

    let provider_config = PathBuf::from(&argv[1]);
    let audio_path = PathBuf::from(&argv[2]);
    let provider = argv.get(3).cloned().unwrap_or_else(|| "openai".to_string());
    let model = argv
        .get(4)
        .cloned()
        .or_else(|| env::var("OPENAI_MODEL").ok())
        .unwrap_or_else(|| "whisper-1".to_string());

    let registry = build_registry(provider_config)?;
    let audio_bytes = std::fs::read(&audio_path)?;

    let mut builder = LLMBuilder::new().provider(provider).model(model);

    if let Ok(k) = env::var("OPENAI_API_KEY") {
        builder = builder.api_key(k);
    }

    if let Ok(u) = env::var("OPENAI_BASE_URL") {
        builder = builder.base_url(u);
    }

    let llm = builder.build(&registry).await?;

    let mut req = SttRequest::new().audio(audio_bytes);
    if let Some(filename) = audio_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
    {
        req = req.filename(filename);
    }

    let resp = llm.transcribe(&req).await?;
    println!("{}", resp.text);
    Ok(())
}
