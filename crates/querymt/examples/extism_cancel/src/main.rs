// Example (stream cancel):
//   RUST_LOG=info,querymt=debug \
//   OPENAI_BASE_URL="http://localhost:8080/v1" \
//   cargo run --manifest-path crates/querymt/examples/extism_cancel/Cargo.toml -- \
//     stream \
//     --provider-config providers.toml \
//     --provider openai \
//     --model "none" \
//     --cancel-after-secs 2

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use log::{debug, info, warn};
use querymt::{
    builder::LLMBuilder,
    chat::{ChatMessage, StreamChunk},
    plugin::{extism_impl::host::ExtismLoader, host::PluginRegistry},
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Subcommand, Debug, Clone, Copy)]
enum Cmd {
    /// Cancel a streaming request by dropping the stream receiver.
    Stream,
    /// Cancel a non-streaming request by timing out the future.
    Chat,
}

#[derive(Parser, Debug)]
#[command(name = "extism_cancel")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,

    /// Providers config file path (e.g. providers.toml)
    #[arg(long, default_value = "providers.toml", global = true)]
    provider_config: PathBuf,

    /// Provider name from the config (default: openai)
    #[arg(long, default_value = "openai", global = true)]
    provider: String,

    /// Model name to use (provider-specific)
    #[arg(long, global = true)]
    model: Option<String>,

    /// API key to use (otherwise reads OPENAI_API_KEY)
    #[arg(long, global = true)]
    api_key: Option<String>,

    /// Base URL for OpenAI-compatible providers (otherwise reads OPENAI_BASE_URL)
    #[arg(long, global = true)]
    base_url: Option<String>,

    /// Prompt to send (default: long essay request)
    #[arg(long, global = true)]
    prompt: Option<String>,

    /// Cancel the request after this many seconds from start.
    ///
    /// For streaming mode, this cancellation fires even if the provider hasn't produced the first
    /// chunk yet.
    #[arg(long, default_value_t = 2, global = true)]
    cancel_after_secs: u64,

    /// After cancelling, keep the process alive for this many seconds to observe host logs.
    #[arg(long, default_value_t = 5, global = true)]
    post_cancel_wait_secs: u64,

    /// Wait for Enter after cancelling instead of sleeping.
    #[arg(long, global = true)]
    wait_for_enter: bool,
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

fn resolve_prompt(args: &Args) -> String {
    args.prompt.clone().unwrap_or_else(|| {
        "Write a detailed essay about the Roman Empire. Cover its rise, key institutions, and the reasons for its decline. Make it long.".to_string()
    })
}

async fn post_cancel_wait(args: &Args) {
    if args.wait_for_enter {
        info!("cancel request sent; press Enter to exit");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    } else {
        info!(
            "cancel request sent; sleeping {}s to observe host logs",
            args.post_cancel_wait_secs
        );
        tokio::time::sleep(std::time::Duration::from_secs(args.post_cancel_wait_secs)).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Enable host/plugin logs via RUST_LOG (e.g. RUST_LOG=querymt=debug).
    // Safe to call multiple times; ignore if already initialized.
    let _ = env_logger::builder().format_timestamp_millis().try_init();

    let args = Args::parse();
    debug!("starting extism_cancel: provider={}, model={:?}", args.provider, args.model);

    let registry = build_registry(args.provider_config.clone())?;

    let mut builder = LLMBuilder::new().provider(args.provider.clone());

    if let Some(model) = &args.model {
        builder = builder.model(model.clone());
    }
    if let Some(api_key) = resolve_api_key(&args) {
        builder = builder.api_key(api_key);
    }
    if let Some(base_url) = resolve_base_url(&args) {
        builder = builder.base_url(base_url);
    }

    let prompt = resolve_prompt(&args);
    let messages = vec![ChatMessage::user().content(prompt).build()];

    match args.cmd {
        Cmd::Stream => {
            builder = builder.stream(true);
            let llm = builder.build(&registry).await?;

            let mut stream = llm.chat_stream_with_tools(&messages, llm.tools()).await?;

            let started = Arc::new(AtomicBool::new(false));
            let started_consumer = started.clone();

            let consumer = tokio::spawn(async move {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(StreamChunk::Text(delta)) => {
                            if !started_consumer.swap(true, Ordering::SeqCst) {
                                info!("stream started (first text chunk received)");
                            }
                            print!("{}", delta);
                        }
                        Ok(StreamChunk::Done { stop_reason }) => {
                            info!("stream done: {stop_reason}");
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn!("stream error: {e}");
                            break;
                        }
                    }
                }
            });

            // Cancellation mode: cancel after a fixed delay from start.
            //
            // This is useful for validating cancellation even when the provider is slow to
            // produce the first chunk (e.g. stuck in connect / first-byte / long prefill). The
            // cancellation mechanism is to drop the stream receiver by aborting the consumer task.
            tokio::time::sleep(std::time::Duration::from_secs(args.cancel_after_secs)).await;

            let stream_started = started.load(Ordering::SeqCst);
            info!(
                "cancelling now: aborting consumer task (drops stream receiver); stream_started={}",
                stream_started
            );
            consumer.abort();
            let _ = consumer.await;
        }
        Cmd::Chat => {
            builder = builder.stream(false);
            let llm = builder.build(&registry).await?;

            info!("starting non-streaming chat request (will cancel via timeout)");
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(args.cancel_after_secs),
                llm.chat_with_tools(&messages, llm.tools()),
            )
            .await;

            match res {
                Ok(Ok(resp)) => {
                    info!("unexpected success; text={:?}", resp.text());
                }
                Ok(Err(e)) => {
                    warn!("chat returned error: {e}");
                }
                Err(_) => {
                    info!("chat timed out (cancelled)");
                }
            }
        }
    }

    post_cancel_wait(&args).await;
    Ok(())
}
