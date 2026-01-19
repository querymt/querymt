// NOTE: run to reproduce
// $ RUST_LOG=info,querymt=debug OPENAI_BASE_URL="http://localhost:8080/v1" cargo run --manifest-path crates/querymt/examples/extism_stream_cancel/Cargo.toml -- --provider-config providers.toml --provider openai --model "none"

use anyhow::{anyhow, Result};
use clap::Parser;
use futures::StreamExt;
use log::{debug, info, warn};
use querymt::{
    builder::LLMBuilder,
    chat::{ChatMessage, StreamChunk},
    plugin::{extism_impl::host::ExtismLoader, host::PluginRegistry},
};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "extism_stream_cancel")]
struct Args {
    /// Providers config file path (e.g. providers.toml)
    #[arg(long)]
    provider_config: PathBuf,

    /// Provider name from the config (default: openai)
    #[arg(long, default_value = "openai")]
    provider: String,

    /// Model name to use (provider-specific)
    #[arg(long)]
    model: Option<String>,

    /// API key to use (otherwise reads OPENAI_API_KEY)
    #[arg(long)]
    api_key: Option<String>,

    /// Base URL for OpenAI-compatible providers (otherwise reads OPENAI_BASE_URL)
    #[arg(long)]
    base_url: Option<String>,
}

fn resolve_api_key(args: &Args) -> Option<String> {
    if let Some(k) = &args.api_key {
        return Some(k.clone());
    }

    // For this repro we default to OpenAI.
    std::env::var("OPENAI_API_KEY").ok()
}

fn resolve_base_url(args: &Args) -> Option<String> {
    if let Some(u) = &args.base_url {
        return Some(u.clone());
    }

    std::env::var("OPENAI_BASE_URL").ok()
}

fn build_registry(cfg_file: PathBuf) -> Result<PluginRegistry> {
    let mut registry = PluginRegistry::from_path(cfg_file)?;
    registry.register_loader(Box::new(ExtismLoader));
    Ok(registry)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Enable host/plugin logs via RUST_LOG (e.g. RUST_LOG=querymt=debug).
    // Safe to call multiple times; ignore if already initialized.
    let _ = env_logger::builder().format_timestamp_millis().try_init();

    let args = Args::parse();
    debug!(
        "starting extism_stream_cancel: provider={}, model={:?}",
        args.provider, args.model
    );

    let registry = build_registry(args.provider_config.clone())?;

    let mut builder = LLMBuilder::new()
        .provider(args.provider.clone())
        .stream(true);

    if let Some(model) = &args.model {
        builder = builder.model(model.clone());
    }

    if let Some(api_key) = resolve_api_key(&args) {
        builder = builder.api_key(api_key);
    }

    if let Some(base_url) = resolve_base_url(&args) {
        builder = builder.base_url(base_url);
    }

    let llm = builder.build(&registry).await?;

    let messages = vec![ChatMessage::user()
        .content("Write a detailed essay about the Roman Empire. Cover its rise, key institutions, and the reasons for its decline. Make it long.")
        .build()];

    let mut stream = llm.chat_stream_with_tools(&messages, llm.tools()).await?;

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();

    let consumer = tokio::spawn(async move {
        let mut started_tx = Some(started_tx);
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamChunk::Text(delta)) => {
                    if let Some(tx) = started_tx.take() {
                        let _ = tx.send(());
                    }
                    // Keep raw text output on stdout; use logs for control-plane events.
                    print!("{}", delta);
                }
                Ok(StreamChunk::Done { stop_reason }) => {
                    info!("stream done: {stop_reason}");
                    break;
                }
                Ok(_) => {
                    // Ignore non-text stream events for this repro.
                }
                Err(e) => {
                    warn!("stream error: {e}");
                    break;
                }
            }
        }
    });

    // Wait until generation actually starts (first token). This avoids cancelling before any yield.
    match tokio::time::timeout(std::time::Duration::from_secs(60), started_rx).await {
        Err(_) => {
            consumer.abort();
            return Err(anyhow!("Timed out waiting for first stream chunk"));
        }
        Ok(Ok(())) => {
            info!("stream started; scheduling cancellation in 2s");
        }
        Ok(Err(_)) => {
            consumer.abort();
            return Err(anyhow!(
                "stream ended before first chunk (plugin error or early exit)"
            ));
        }
    }

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    info!("cancelling now: aborting consumer task (drops stream receiver)");
    // This is the "cancellation": stop reading the stream.
    consumer.abort();
    // NOTE: Without implemented patch when trying to do abort, plugin will error and don't really
    // cancel, it will just continue to wait end of stream from remote provider.
    //
    // [2026-01-19T20:47:51.255Z ERROR extism::plugin] call to chat_stream encountered an error: error while executing at wasm backtrace:
    //         0: 0x161858 - qmt_openai.wasm!querymt_extism_macros::qmt_yield_chunk::h8d8af79a866968e3
    //         1: 0x161707 - qmt_openai.wasm!querymt_extism_macros::qmt_yield_chunk_wrapper::hdba269443464398b
    //         2:  0xe466a - qmt_openai.wasm!qmt_openai::extism_exports::chat_stream::inner::hc8d4a1d713a9b46e
    //         3:  0xe31f1 - qmt_openai.wasm!chat_stream
    //         4: 0x3178bf - qmt_openai.wasm!chat_stream.command_export
    //     note: using the `WASMTIME_BACKTRACE_DETAILS=1` environment variable may show more debugging information
    //
    //     Caused by:
    //         Failed to yield chunk: channel closed plugin="2c130744-2d21-4fe6-84b4-634d51e6952c"
    // [2026-01-19T20:47:51.255Z ERROR querymt::plugin::extism_impl::host] chat_stream plugin call failed: error while executing at wasm backtrace:
    //         0: 0x161858 - qmt_openai.wasm!querymt_extism_macros::qmt_yield_chunk::h8d8af79a866968e3
    //         1: 0x161707 - qmt_openai.wasm!querymt_extism_macros::qmt_yield_chunk_wrapper::hdba269443464398b
    //         2:  0xe466a - qmt_openai.wasm!qmt_openai::extism_exports::chat_stream::inner::hc8d4a1d713a9b46e
    //         3:  0xe31f1 - qmt_openai.wasm!chat_stream
    //         4: 0x3178bf - qmt_openai.wasm!chat_stream.command_export
    //     note: using the `WASMTIME_BACKTRACE_DETAILS=1` environment variable may show more debugging information
    //
    //     Caused by:
    //         Failed to yield chunk: channel closed
    //

    info!(
        "cancel request sent; wait for host logs indicating plugin thread exit; press Enter to exit"
    );
    info!(
        "expected host logs: 'chat_stream stopped due to cancellation' then 'chat_stream thread finished'"
    );
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);

    Ok(())
}
