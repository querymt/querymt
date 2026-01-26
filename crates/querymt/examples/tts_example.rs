use anyhow::Result;
use querymt::{
    builder::LLMBuilder,
    plugin::{extism_impl::host::ExtismLoader, host::PluginRegistry},
    tts::TtsRequest,
};
use std::{env, path::PathBuf};

fn usage() -> String {
    let bin = env::args()
        .next()
        .unwrap_or_else(|| "tts_example".to_string());
    format!(
        "Usage: {bin} <providers.toml> <text> <out_file> [provider] [model] [voice] [format]\n\n\
Example:\n  {bin} ./providers.toml \"hello world\" ./out.mp3 openai gpt-4o-mini alloy mp3\n\n\
Env:\n  OPENAI_API_KEY, OPENAI_BASE_URL\n"
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
    if argv.len() < 4 {
        eprintln!("{}", usage());
        std::process::exit(2);
    }

    let provider_config = PathBuf::from(&argv[1]);
    let text = argv[2].clone();
    let out_file = PathBuf::from(&argv[3]);

    let provider = argv.get(4).cloned().unwrap_or_else(|| "openai".to_string());
    let model = argv.get(5).cloned();
    let voice = argv.get(6).cloned();
    let format = argv.get(7).cloned();

    let registry = build_registry(provider_config)?;

    let mut builder = LLMBuilder::new().provider(provider);

    if let Ok(k) = env::var("OPENAI_API_KEY") {
        builder = builder.api_key(k);
    }
    if let Ok(u) = env::var("OPENAI_BASE_URL") {
        builder = builder.base_url(u);
    }

    // OpenAI config requires model; pick a reasonable default.
    builder = builder.model(model.clone().unwrap_or_else(|| "gpt-4o-mini".to_string()));

    let llm = builder.build(&registry).await?;

    let mut req = TtsRequest::new().text(text);
    if let Some(m) = model {
        req = req.model(m);
    }
    if let Some(v) = voice {
        req = req.voice(v);
    }
    if let Some(f) = format {
        req = req.format(f);
    }

    let resp = llm.speech(&req).await?;
    std::fs::write(&out_file, &resp.audio)?;

    if let Some(mt) = resp.mime_type {
        eprintln!("wrote {} ({})", out_file.display(), mt);
    } else {
        eprintln!("wrote {}", out_file.display());
    }

    Ok(())
}
