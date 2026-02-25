//! Vision chat example for llama.cpp provider.
//!
//! Demonstrates multimodal (vision) inference. If the model is loaded from a
//! Hugging Face repo and no --mmproj is specified, the provider will try to
//! auto-discover the mmproj file from the same repo.
//!
//! # Usage
//!
//! Non-streaming (mmproj auto-discovered from HF repo):
//! ```bash
//! cargo run --example vision_chat --features metal -- \
//!   --model "unsloth/Qwen3-VL-8B-Instruct-GGUF:UD-Q6_K_XL" \
//!   --image path/to/image.jpg
//! ```
//!
//! With explicit mmproj:
//! ```bash
//! cargo run --example vision_chat --features metal -- \
//!   --model "unsloth/Qwen3-VL-8B-Instruct-GGUF:UD-Q6_K_XL" \
//!   --mmproj "hf:unsloth/Qwen3-VL-8B-Instruct-GGUF:mmproj-F16.gguf" \
//!   --image path/to/image.jpg \
//!   --stream
//! ```

use clap::Parser;
use futures::StreamExt;
use qmt_llama_cpp::{LlamaCppConfig, create_provider};
use querymt::chat::{ChatMessage, ChatRole, ImageMime, MessageType};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(about = "Vision chat with llama.cpp multimodal models")]
struct Args {
    /// Model (local path, <repo>:<quant>, or hf:<repo>:<file>)
    #[arg(short, long)]
    model: String,

    /// Multimodal projection file (optional — auto-discovered from HF repo if omitted)
    #[arg(long)]
    mmproj: Option<String>,

    /// Image file to analyze
    #[arg(short, long)]
    image: PathBuf,

    /// Question or prompt about the image
    #[arg(long, default_value = "What's in this image?")]
    prompt: String,

    /// Stream the response token-by-token
    #[arg(short, long)]
    stream: bool,

    /// Context window size
    #[arg(long, default_value = "8192")]
    n_ctx: u32,

    /// GPU layers to offload (0 = CPU only)
    #[arg(long, default_value = "99")]
    n_gpu_layers: u32,

    /// Maximum tokens to generate
    #[arg(long, default_value = "512")]
    max_tokens: u32,

    /// Media marker override (e.g. "<start_of_image>" for Gemma 3)
    #[arg(long)]
    media_marker: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    if !args.image.exists() {
        eprintln!("Error: image file not found: {}", args.image.display());
        std::process::exit(1);
    }

    let image_data = std::fs::read(&args.image)?;
    let mime = detect_mime(&args.image)?;

    let config = LlamaCppConfig {
        model: args.model.clone(),
        mmproj_path: args.mmproj.clone(),
        media_marker: args.media_marker,
        n_ctx: Some(args.n_ctx),
        n_gpu_layers: Some(args.n_gpu_layers),
        max_tokens: Some(args.max_tokens),
        temperature: None,
        top_p: None,
        top_k: None,
        system: vec![],
        n_batch: None,
        n_threads: None,
        n_threads_batch: None,
        seed: None,
        chat_template: None,
        use_chat_template: None,
        add_bos: None,
        log: None,
        fast_download: None,
        enable_thinking: None,
        flash_attention: None,
        kv_cache_type_k: None,
        kv_cache_type_v: None,
        mmproj_threads: None,
        mmproj_use_gpu: None,
        n_ubatch: None,
    };

    println!("Loading model: {}", args.model);
    if let Some(ref p) = args.mmproj {
        println!("Projection: {}", p);
    } else {
        println!("Projection: (auto-discover from HF repo)");
    }

    let provider = create_provider(config)?;
    println!("Model loaded.\n");

    let messages = vec![ChatMessage {
        role: ChatRole::User,
        message_type: MessageType::Image((mime, image_data)),
        content: args.prompt.clone(),
        thinking: None,
        cache: None,
    }];

    println!("Prompt: {}\n", args.prompt);

    if args.stream {
        println!("--- streaming ---");
        let mut stream = provider.chat_stream(&messages).await?;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(querymt::chat::StreamChunk::Text(t)) => {
                    print!("{}", t);
                    std::io::Write::flush(&mut std::io::stdout())?;
                }
                Ok(querymt::chat::StreamChunk::Usage(u)) => {
                    println!("\n---\ninput={} output={}", u.input_tokens, u.output_tokens);
                }
                Ok(querymt::chat::StreamChunk::Done { stop_reason }) => {
                    println!("stop: {}", stop_reason);
                }
                Err(e) => return Err(e.into()),
                _ => {}
            }
        }
    } else {
        println!("--- response ---");
        let response = provider.chat(&messages).await?;
        println!("{}", response.text().unwrap_or_default());
        if let Some(u) = response.usage() {
            println!("---\ninput={} output={}", u.input_tokens, u.output_tokens);
        }
    }

    Ok(())
}

fn detect_mime(path: &PathBuf) -> Result<ImageMime, Box<dyn std::error::Error>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .ok_or("no file extension")?
        .to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Ok(ImageMime::JPEG),
        "png" => Ok(ImageMime::PNG),
        "gif" => Ok(ImageMime::GIF),
        "webp" => Ok(ImageMime::WEBP),
        other => Err(format!("unsupported image format: {}", other).into()),
    }
}
