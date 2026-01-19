use std::sync::Arc;
use std::time::{Duration, Instant};

use actix::{
    Actor, ActorFutureExt, AsyncContext, Context, Handler, Message, SpawnHandle, WrapFuture,
};
use anyhow::{Context as AnyhowContext, Result, anyhow};
use clap::Parser;
use futures_util::StreamExt;
use querymt::builder::LLMBuilder;
use querymt::chat::{ChatMessage, StreamChunk};
use querymt::error::LLMError;
use querymt::plugin::host::PluginRegistry;
use tracing::{info, warn};

// NOTE ON CANCELLATION VS RESPONSIVENESS
// You can avoid Actix arbiter starvation by moving the LLM call onto a different thread
// (e.g. `tokio::spawn_blocking`, a dedicated Arbiter, or another runtime).
// That keeps the system responsive and lets "cancel" messages be processed on time.
//
// However, this does NOT mean the underlying QueryMT provider request is truly cancelled.
// If the provider implementation performs blocking work (sync HTTP, plugin call, long compute) inside
// an async API, then cancelling/aborting the wrapper future only stops *waiting for the result*.
// The provider work may continue in the background until completion because it is not cancellation-aware.
//
// Hard-cancel with streaming/non-streaming chat generally requires a killable boundary (subprocess) or would require cancellation-aware provider.

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
enum Mode {
    Nonstream,
    Stream,
    NonstreamBlocking,
}

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Path to QueryMT providers config (toml/json/yaml)
    #[arg(short = 'p', long = "providers", value_name = "PATH")]
    providers: std::path::PathBuf,

    /// Provider id from the providers config
    #[arg(long = "provider", default_value = "openai")]
    provider: String,

    /// Model name for the provider
    #[arg(long = "model", default_value = "gpt-4o-mini")]
    model: String,

    /// Prompt to send
    #[arg(
        long = "prompt",
        default_value = "Write a long, detailed essay about the history of the Roman Empire. Include dates and key figures. Then continue with an analysis of administrative reforms, military logistics, and trade routes across multiple centuries."
    )]
    prompt: String,

    /// Request mode: nonstream uses llm.chat(); stream uses llm.chat_stream() (strict)
    #[arg(long = "mode", value_enum, default_value_t = Mode::Nonstream)]
    mode: Mode,

    /// When to attempt cancellation after starting (ms)
    #[arg(long = "cancel-after-ms", default_value_t = 2000)]
    cancel_after_ms: u64,

    /// Optional API key override (can be empty)
    #[arg(long = "api-key")]
    api_key: Option<String>,

    /// Optional base URL override
    #[arg(long = "base-url")]
    base_url: Option<String>,
}

#[derive(Message)]
#[rtype(result = "()")]
struct StartRequest;

#[derive(Message)]
#[rtype(result = "()")]
struct CancelRequest;

struct LlmRequestActor {
    llm: Arc<dyn querymt::LLMProvider>,
    provider: String,
    model: String,
    prompt: String,
    mode: Mode,

    request_id: u64,
    active: Option<SpawnHandle>,
}

impl Actor for LlmRequestActor {
    type Context = Context<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {
        info!(target: "example", "LlmRequestActor started");
    }
}

impl Handler<StartRequest> for LlmRequestActor {
    type Result = ();

    fn handle(&mut self, _msg: StartRequest, ctx: &mut Context<Self>) {
        self.request_id = self.request_id.saturating_add(1);
        let request_id = self.request_id;

        if let Some(handle) = self.active.take() {
            ctx.cancel_future(handle);
        }

        let llm = self.llm.clone();
        let provider = self.provider.clone();
        let model = self.model.clone();
        let prompt = self.prompt.clone();
        let mode = self.mode;

        info!(
            target: "example",
            request_id,
            provider = provider,
            model = model,
            mode = ?mode,
            prompt_len = prompt.len(),
            "StartRequest received; spawning LLM request"
        );

        let started_at = Instant::now();

        let fut = async move {
            let messages = vec![ChatMessage::user().content(prompt).build()];

            match mode {
                Mode::Nonstream => {
                    info!(
                        target: "example",
                        request_id,
                        "LLM future entered; calling llm.chat() NOW"
                    );

                    // IMPORTANT (Actix arbiter starvation / cancellation gotcha):
                    // This `llm.chat()` call may block the *entire* Actix arbiter thread (e.g. some
                    // QueryMT provider implementations do synchronous work inside an `async fn`, or
                    // hold a mutex while doing a blocking plugin/HTTP call).
                    //
                    // When that happens:
                    // - Other actors on the same arbiter (including `CancelActor`) do not get polled.
                    // - `CancelActor`'s `run_later` timer does not fire, so it can't even send `CancelRequest`.
                    // - Even if `CancelRequest` were sent from another thread, `ctx.cancel_future(handle)` only
                    //   cancels the Actix future wrapper; it cannot preempt a currently-running blocking call.
                    // Result: the request runs to completion and "cancel" appears ignored.
                    let resp = llm
                        .chat(&messages)
                        .await
                        .map_err(|e| anyhow!("querymt error: {e}"))?;

                    Ok::<usize, anyhow::Error>(resp.text().unwrap_or_default().len())
                }
                Mode::Stream => {
                    info!(
                        target: "example",
                        request_id,
                        "LLM future entered; calling llm.chat_stream() NOW"
                    );

                    let mut stream = llm.chat_stream(&messages).await.map_err(|e| match e {
                        LLMError::NotImplemented(msg) => {
                            anyhow!("streaming is strict but not implemented: {msg}")
                        }
                        other => anyhow!("querymt error: {other}"),
                    })?;

                    // NOTE (streaming != hard-cancel):
                    // Streaming keeps the Actix arbiter responsive (ticks continue and CancelRequest is processed),
                    // but it does NOT necessarily stop the underlying provider work.
                    // In particular, some QueryMT plugin/Extism implementations drive streaming from a background
                    // thread that continues running the plugin call even if we stop consuming / drop the stream.
                    // Result: "cancel" becomes "stop waiting + ignore late output" rather than a true hard-cancel.
                    let mut text = String::new();
                    let mut saw_first_chunk = false;
                    let mut stop_reason: Option<String> = None;

                    while let Some(chunk_res) = stream.next().await {
                        let chunk = chunk_res.map_err(|e| anyhow!("stream chunk error: {e}"))?;
                        if !saw_first_chunk {
                            saw_first_chunk = true;
                            info!(target: "example", request_id, "First stream chunk received");
                        }

                        match chunk {
                            StreamChunk::Text(delta) => text.push_str(&delta),
                            StreamChunk::Done {
                                stop_reason: reason,
                            } => {
                                stop_reason = Some(reason);
                                break;
                            }
                            _ => {}
                        }
                    }

                    info!(
                        target: "example",
                        request_id,
                        stop_reason = stop_reason.as_deref().unwrap_or("<none>"),
                        "Stream ended"
                    );

                    Ok::<usize, anyhow::Error>(text.len())
                }
                Mode::NonstreamBlocking => {
                    info!(
                        target: "example",
                        request_id,
                        "LLM future entered; calling llm.chat() via spawn_blocking"
                    );

                    // This mode is a mitigation for arbiter starvation: it moves the non-streaming
                    // `llm.chat()` work off the Actix arbiter thread.
                    //
                    // Important limitation: cancelling the Actix future does NOT stop the blocking
                    // worker thread once started. This mode is meant to demonstrate "arbiter stays
                    // responsive" vs "hard cancel".
                    //
                    // Observable behavior: you will receive `CancelRequest` promptly and ticks keep running,
                    // but the underlying provider call may continue in the background and complete later anyway.
                    let join = tokio::task::spawn_blocking(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|e| anyhow!("spawn_blocking runtime build failed: {e}"))?;

                        rt.block_on(async move {
                            let resp = llm
                                .chat(&messages)
                                .await
                                .map_err(|e| anyhow!("querymt error: {e}"))?;
                            Ok::<usize, anyhow::Error>(resp.text().unwrap_or_default().len())
                        })
                    });

                    let text_len = join
                        .await
                        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))??;
                    Ok::<usize, anyhow::Error>(text_len)
                }
            }
        }
        .into_actor(self)
        .map(move |result, actor, _ctx| {
            actor.active = None;
            let elapsed_ms = started_at.elapsed().as_millis();
            match result {
                Ok(text_len) => {
                    info!(
                        target: "example",
                        request_id,
                        elapsed_ms,
                        text_len,
                        "llm.chat() returned OK"
                    );
                }
                Err(err) => {
                    warn!(
                        target: "example",
                        request_id,
                        elapsed_ms,
                        error = %err,
                        "llm.chat() returned ERR"
                    );
                }
            }
        });

        let handle = ctx.spawn(fut);
        self.active = Some(handle);
        info!(target: "example", request_id, "Spawned request future");
    }
}

impl Handler<CancelRequest> for LlmRequestActor {
    type Result = ();

    fn handle(&mut self, _msg: CancelRequest, ctx: &mut Context<Self>) {
        info!(
            target: "example",
            request_id = self.request_id,
            mode = ?self.mode,
            has_active = self.active.is_some(),
            "CancelRequest received"
        );

        if let Some(handle) = self.active.take() {
            ctx.cancel_future(handle);
            info!(
                target: "example",
                request_id = self.request_id,
                "ctx.cancel_future() called"
            );
        }
    }
}

struct CancelActor {
    llm_addr: actix::Addr<LlmRequestActor>,
    cancel_after: Duration,
    tick_every: Duration,
    started_at: Instant,
    ticks: u64,
}

impl Actor for CancelActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        info!(target: "example", "CancelActor started");

        info!(target: "example", "Sending StartRequest");
        self.llm_addr.do_send(StartRequest);

        let llm_addr = self.llm_addr.clone();
        let cancel_after = self.cancel_after;
        info!(
            target: "example",
            cancel_after_ms = cancel_after.as_millis() as u64,
            "Scheduling CancelRequest"
        );

        // NOTE: This timer runs on the same Actix arbiter as the LLM request future.
        // If the LLM request path blocks the arbiter thread (the core issue this example demonstrates),
        // this `run_later` callback will not fire until the blocking call returns. That makes "cancel"
        // look ignored, but the reality is the CancelActor never got CPU time to send the message.
        ctx.run_later(cancel_after, move |_actor, _ctx| {
            info!(target: "example", "Sending CancelRequest NOW");
            llm_addr.do_send(CancelRequest);
        });

        let tick_every = self.tick_every;
        ctx.run_interval(tick_every, |actor, _ctx| {
            actor.ticks += 1;
            let elapsed_ms = actor.started_at.elapsed().as_millis();
            info!(
                target: "example",
                tick = actor.ticks,
                elapsed_ms,
                "CancelActor tick"
            );
        });
    }
}

#[actix_rt::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(true)
        .try_init();

    let args = Args::parse();

    let api_key = match args.api_key {
        Some(v) => v,
        None => std::env::var("OPENAI_API_KEY").unwrap_or_default(),
    };

    let base_url = match args.base_url {
        Some(v) => v,
        None => std::env::var("OPENAI_BASE_URL").unwrap_or_default(),
    };

    info!(
        target: "example",
        providers_path = %args.providers.display(),
        provider = %args.provider,
        model = %args.model,
        base_url_set = !base_url.trim().is_empty(),
        api_key_len = api_key.len(),
        cancel_after_ms = args.cancel_after_ms,
        "Starting example"
    );

    let mut registry = PluginRegistry::from_path(&args.providers)
        .map_err(|e| anyhow!(e))
        .with_context(|| {
            format!(
                "failed to load QueryMT providers config from {}",
                args.providers.display()
            )
        })?;

    registry.register_loader(Box::new(querymt::plugin::extism_impl::host::ExtismLoader));

    info!(target: "example", "Loading QueryMT plugins...");
    registry.load_all_plugins().await;
    info!(target: "example", "Finished loading QueryMT plugins");

    if registry.get(&args.provider).await.is_none() {
        return Err(anyhow!(
            "Provider '{}' not available/loaded from {}",
            args.provider,
            args.providers.display()
        ));
    }

    let mut builder = LLMBuilder::new()
        .provider(args.provider.clone())
        .api_key(api_key)
        .model(args.model.clone())
        .stream(true);

    if !base_url.trim().is_empty() {
        builder = builder.base_url(base_url);
    }

    info!(target: "example", "Building LLM client...");
    let llm = builder
        .build(&registry)
        .await
        .map_err(|e| anyhow!("failed to build LLM: {e}"))?;
    info!(target: "example", "LLM client built");

    let llm_actor = LlmRequestActor {
        llm: Arc::from(llm),
        provider: args.provider,
        model: args.model,
        prompt: args.prompt,
        mode: args.mode,
        request_id: 0,
        active: None,
    }
    .start();

    CancelActor {
        llm_addr: llm_actor,
        cancel_after: Duration::from_millis(args.cancel_after_ms),
        tick_every: Duration::from_millis(250),
        started_at: Instant::now(),
        ticks: 0,
    }
    .start();

    // Keep the system running.
    tokio::signal::ctrl_c().await.ok();
    info!(target: "example", "Ctrl-C received, shutting down");
    Ok(())
}

